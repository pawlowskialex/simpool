//! Touch, keyboard and hardware buttons, and the accessibility tree for finding things on screen.

use super::{AppState, L};
use crate::bridge::Bridge;
use crate::error::{ApiError, ApiResult};
use crate::pool::Lease;
use crate::proc::simctl_with;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/input", post(input))
        .route("/ui", get(ui_tree))
        .route("/ui/at", get(ui_at))
        .route("/ui/find", post(ui_find))
        .route("/ui/tap", post(ui_tap))
        .route("/ui/wait", post(ui_wait))
        .route("/ui/scroll-to", post(ui_scroll_to))
}

#[derive(Deserialize)]
struct InputReq {
    actions: Vec<Value>,
}

/// Runs actions in order and stops at the first failure.
/// {"actions": [{"type": "tap", "x": 100, "y": 200}, {"type": "text", "text": "hello"}, {"type": "key", "key": "enter"}]}
async fn input(State(pool): State<AppState>, L(lease): L, Json(r): Json<InputReq>) -> ApiResult<Json<Value>> {
    let bridge = lease.bridge(&pool).await?;
    let mut results = Vec::new();
    for (i, a) in r.actions.into_iter().enumerate() {
        let res = run_action(&bridge, &lease, a).await.map_err(|e| ApiError(e.0, format!("action {i}: {}", e.1)))?;
        results.push(res);
        lease.touch(None);
    }
    Ok(Json(json!({ "results": results })))
}

fn secs(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0).max(0.0)
}

/// One input action. Shared by POST /input and the live-view WebSocket.
pub async fn run_action(bridge: &Arc<Bridge>, lease: &Lease, mut a: Value) -> ApiResult<Value> {
    let ty = a["type"].as_str().ok_or_else(|| ApiError::bad_request("each action needs a \"type\""))?.to_string();
    let budget = Duration::from_secs_f64(15.0 + 2.0 * (secs(&a["duration"]) + secs(&a["hold"])));
    match ty.as_str() {
        "wait" => {
            tokio::time::sleep(Duration::from_secs_f64(secs(&a["seconds"]).min(60.0))).await;
            Ok(json!({}))
        }
        "tap" | "double_tap" | "long_press" => {
            let mut target = Value::Null;
            if a.get("element").is_some() {
                let q: UiQuery = serde_json::from_value(a["element"].clone()).map_err(|e| ApiError::bad_request(format!("element: {e}")))?;
                let el = resolve(bridge, &q, a["timeout"].as_f64().unwrap_or(5.0)).await?;
                let (x, y) = center(&el);
                a["x"] = json!(x);
                a["y"] = json!(y);
                target = el;
            }
            let mut cmd = json!({"op": "tap", "x": a["x"], "y": a["y"]});
            match ty.as_str() {
                "double_tap" => cmd["count"] = json!(2),
                "long_press" => cmd["duration"] = json!(a["duration"].as_f64().unwrap_or(1.0)),
                _ => {
                    if let Some(d) = a["duration"].as_f64() {
                        cmd["duration"] = json!(d);
                    }
                    if let Some(c) = a["count"].as_u64() {
                        cmd["count"] = json!(c);
                    }
                }
            }
            bridge.call(cmd, budget).await?;
            Ok(if target.is_null() { json!({}) } else { json!({ "element": strip_children(target) }) })
        }
        "text" => {
            let text = a["text"].as_str().ok_or_else(|| ApiError::bad_request("text needs \"text\""))?;
            let budget = Duration::from_secs_f64(15.0 + text.len() as f64 * 0.1);
            let typeable = text.chars().all(|c| c.is_ascii_graphic() || matches!(c, ' ' | '\n' | '\t'));
            if typeable && !a["paste"].as_bool().unwrap_or(false) {
                bridge.call(json!({"op": "text", "text": text, "interval": a["interval"]}), budget).await?;
                Ok(json!({ "method": "keyboard" }))
            } else {
                // Emoji, accents, CJK: put it on the pasteboard and paste with ⌘V.
                simctl_with(&["pbcopy", &lease.udid], Some(text.as_bytes()), &[]).await?;
                bridge.call(json!({"op": "key", "key": "v", "modifiers": ["cmd"]}), budget).await?;
                Ok(json!({ "method": "paste" }))
            }
        }
        "scroll" => {
            let info = bridge.call(json!({"op": "info"}), budget).await?;
            let (w, h) = (info["points"]["width"].as_f64().unwrap_or(400.0), info["points"]["height"].as_f64().unwrap_or(800.0));
            let dir = a["direction"].as_str().unwrap_or("down");
            let (x, y) = (a["x"].as_f64().unwrap_or(w / 2.0), a["y"].as_f64().unwrap_or(h / 2.0));
            // Momentum-free by default: the content stops at the dragged distance
            // instead of flinging on. Pass "momentum": true for an inertial flick.
            let momentum = a["momentum"].as_bool().unwrap_or(false);
            let dur = a["duration"].as_f64().unwrap_or(0.3);
            bridge.call(scroll_swipe(dir, x, y, a["distance"].as_f64().unwrap_or(h * 0.4), dur, momentum)?, budget).await?;
            Ok(json!({}))
        }
        "swipe" | "drag" | "pinch" | "pan" | "touch" | "key" | "button" => {
            a["op"] = json!(ty);
            a.as_object_mut().unwrap().remove("type");
            bridge.call(a, budget).await?;
            Ok(json!({}))
        }
        other => Err(ApiError::bad_request(format!(
            "unknown action type {other:?}; have tap, double_tap, long_press, swipe, scroll, drag, pinch, pan, touch, text, key, button, wait"
        ))),
    }
}

/// A swipe that scrolls content in `dir` ("down" reveals what's below, like reading on).
/// Without momentum, the finger decelerates into the lift (`settle`), so content
/// stops at the dragged distance; with momentum it keeps its speed into the lift,
/// so UIKit flings on past it.
fn scroll_swipe(dir: &str, x: f64, y: f64, dist: f64, duration: f64, momentum: bool) -> ApiResult<Value> {
    let (dx, dy) = match dir {
        "down" => (0.0, -dist),
        "up" => (0.0, dist),
        "right" => (-dist, 0.0),
        "left" => (dist, 0.0),
        _ => return Err(ApiError::bad_request("direction is up, down, left or right")),
    };
    let settle = if momentum { 0.0 } else { (duration * 1.4).max(0.3) };
    Ok(json!({"op": "swipe", "x": x - dx / 2.0, "y": y - dy / 2.0, "to_x": x + dx / 2.0, "to_y": y + dy / 2.0, "duration": duration, "settle": settle}))
}

// ---- accessibility ----

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct UiQuery {
    /// accessibilityIdentifier, exact
    pub id: Option<String>,
    /// accessibilityLabel, exact
    pub label: Option<String>,
    pub label_contains: Option<String>,
    pub label_regex: Option<String>,
    pub value: Option<String>,
    pub value_contains: Option<String>,
    /// AXButton, AXStaticText, AXTextField, AXCell, AXSwitch …
    pub role: Option<String>,
    /// Case-insensitive substring of label, value, title, identifier or placeholder.
    pub text: Option<String>,
    pub enabled: Option<bool>,
    /// Which match to use when several match (default 0, in tree order).
    #[serde(default)]
    pub index: usize,
}

impl UiQuery {
    fn describe(&self) -> String {
        let s = serde_json::to_string(&json!({
            "id": self.id, "label": self.label, "label_contains": self.label_contains, "label_regex": self.label_regex,
            "value": self.value, "value_contains": self.value_contains, "role": self.role, "text": self.text, "enabled": self.enabled,
        }))
        .unwrap();
        s.replace(",\"", ", \"").replace(":null", "").to_string()
    }

    fn matches(&self, e: &Value, re: Option<&regex::Regex>) -> bool {
        let field = |k: &str| e[k].as_str().unwrap_or("");
        let eq = |want: &Option<String>, k: &str| want.as_ref().is_none_or(|w| field(k) == w);
        let contains = |want: &Option<String>, k: &str| want.as_ref().is_none_or(|w| field(k).contains(w.as_str()));
        eq(&self.id, "identifier")
            && eq(&self.label, "label")
            && contains(&self.label_contains, "label")
            && re.is_none_or(|r| r.is_match(field("label")))
            && eq(&self.value, "value")
            && contains(&self.value_contains, "value")
            && eq(&self.role, "role")
            && self.enabled.is_none_or(|en| e["enabled"].as_bool() == Some(en))
            && self.text.as_ref().is_none_or(|t| {
                let t = t.to_lowercase();
                ["label", "value", "title", "identifier", "placeholder"].iter().any(|k| field(k).to_lowercase().contains(&t))
            })
    }

    fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.label.is_none()
            && self.label_contains.is_none()
            && self.label_regex.is_none()
            && self.value.is_none()
            && self.value_contains.is_none()
            && self.role.is_none()
            && self.text.is_none()
    }
}

fn flatten(e: &Value, depth: usize, out: &mut Vec<Value>) {
    let mut me = strip_children(e.clone());
    me["depth"] = json!(depth);
    out.push(me);
    for c in e["children"].as_array().into_iter().flatten() {
        flatten(c, depth + 1, out);
    }
}

fn strip_children(mut e: Value) -> Value {
    if let Some(o) = e.as_object_mut() {
        o.remove("children");
    }
    e
}

fn center(e: &Value) -> (f64, f64) {
    let f = &e["frame"];
    (f["x"].as_f64().unwrap_or(0.0) + f["width"].as_f64().unwrap_or(0.0) / 2.0, f["y"].as_f64().unwrap_or(0.0) + f["height"].as_f64().unwrap_or(0.0) / 2.0)
}

async fn tree(bridge: &Bridge, max_depth: Option<u64>) -> ApiResult<Value> {
    let r = bridge.call(json!({"op": "ax_tree", "max_depth": max_depth.unwrap_or(64)}), Duration::from_secs(30)).await?;
    Ok(r["tree"].clone())
}

async fn find(bridge: &Bridge, q: &UiQuery) -> ApiResult<Vec<Value>> {
    if q.is_empty() {
        return Err(ApiError::bad_request("the query matches everything; give id, label, text, role …"));
    }
    let re = q.label_regex.as_deref().map(regex::Regex::new).transpose().map_err(|e| ApiError::bad_request(format!("label_regex: {e}")))?;
    let mut flat = Vec::new();
    flatten(&tree(bridge, None).await?, 0, &mut flat);
    Ok(flat.into_iter().filter(|e| q.matches(e, re.as_ref())).collect())
}

/// Polls until the element appears (or, with `gone`, disappears). Ok(None) means it timed out.
async fn wait_for(bridge: &Bridge, q: &UiQuery, timeout: f64, gone: bool) -> ApiResult<Option<Value>> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.clamp(0.0, 300.0));
    loop {
        let found = find(bridge, q).await?.into_iter().nth(q.index);
        match (&found, gone) {
            (Some(_), false) => return Ok(found),
            (None, true) => return Ok(Some(json!({}))),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn resolve(bridge: &Bridge, q: &UiQuery, timeout: f64) -> ApiResult<Value> {
    wait_for(bridge, q, timeout, false).await?.ok_or_else(|| ApiError::not_found(format!("no element matching {} within {timeout}s", q.describe())))
}

#[derive(Deserialize)]
struct TreeQuery {
    /// tree (nested JSON, default), flat (list with depth), or text (indented lines, compact for LLMs)
    #[serde(default)]
    format: Option<String>,
    max_depth: Option<u64>,
}

async fn ui_tree(State(pool): State<AppState>, L(lease): L, Query(q): Query<TreeQuery>) -> ApiResult<Response> {
    let bridge = lease.bridge(&pool).await?;
    let t = tree(&bridge, q.max_depth).await?;
    match q.format.as_deref().unwrap_or("tree") {
        "tree" => Ok(Json(t).into_response()),
        "flat" => {
            let mut flat = Vec::new();
            flatten(&t, 0, &mut flat);
            Ok(Json(json!({ "elements": flat })).into_response())
        }
        "text" => {
            let mut flat = Vec::new();
            flatten(&t, 0, &mut flat);
            let mut s = String::new();
            for e in flat {
                let f = &e["frame"];
                s += &format!("{}{}", "  ".repeat(e["depth"].as_u64().unwrap_or(0) as usize), e["role"].as_str().unwrap_or("?").trim_start_matches("AX"));
                for k in ["label", "value", "identifier", "placeholder"] {
                    if let Some(v) = e[k].as_str() {
                        s += &format!(" {k}={v:?}");
                    }
                }
                if e["enabled"] == json!(false) {
                    s += " disabled";
                }
                s += &format!(
                    " @({:.0},{:.0} {:.0}x{:.0})\n",
                    f["x"].as_f64().unwrap_or(0.0),
                    f["y"].as_f64().unwrap_or(0.0),
                    f["width"].as_f64().unwrap_or(0.0),
                    f["height"].as_f64().unwrap_or(0.0)
                );
            }
            Ok(s.into_response())
        }
        other => Err(ApiError::bad_request(format!("format {other:?}: use tree, flat or text"))),
    }
}

#[derive(Deserialize)]
struct PointQuery {
    x: f64,
    y: f64,
}

async fn ui_at(State(pool): State<AppState>, L(lease): L, Query(p): Query<PointQuery>) -> ApiResult<Json<Value>> {
    let bridge = lease.bridge(&pool).await?;
    let r = bridge.call(json!({"op": "ax_at", "x": p.x, "y": p.y, "max_depth": 0}), Duration::from_secs(30)).await?;
    Ok(Json(r["tree"].clone()))
}

async fn ui_find(State(pool): State<AppState>, L(lease): L, Json(q): Json<UiQuery>) -> ApiResult<Json<Value>> {
    let bridge = lease.bridge(&pool).await?;
    Ok(Json(json!({ "elements": find(&bridge, &q).await? })))
}

#[derive(Deserialize)]
struct WaitReq {
    #[serde(flatten)]
    query: UiQuery,
    #[serde(default = "five")]
    timeout: f64,
    /// Wait for it to disappear instead.
    #[serde(default)]
    gone: bool,
}

fn five() -> f64 {
    5.0
}

async fn ui_wait(State(pool): State<AppState>, L(lease): L, Json(r): Json<WaitReq>) -> ApiResult<Json<Value>> {
    let bridge = lease.bridge(&pool).await?;
    match wait_for(&bridge, &r.query, r.timeout, r.gone).await? {
        Some(e) if !r.gone => Ok(Json(json!({ "element": e }))),
        Some(_) => Ok(Json(json!({ "gone": true }))),
        None => Err(ApiError::timeout(format!("{} still {} after {}s", r.query.describe(), if r.gone { "there" } else { "missing" }, r.timeout))),
    }
}

async fn ui_tap(State(pool): State<AppState>, L(lease): L, Json(r): Json<WaitReq>) -> ApiResult<Json<Value>> {
    let bridge = lease.bridge(&pool).await?;
    let el = resolve(&bridge, &r.query, r.timeout).await?;
    let (x, y) = center(&el);
    bridge.call(json!({"op": "tap", "x": x, "y": y}), Duration::from_secs(15)).await?;
    Ok(Json(json!({ "element": el })))
}

#[derive(Deserialize)]
struct ScrollToReq {
    #[serde(flatten)]
    query: UiQuery,
    /// Direction to scroll the content (default down).
    #[serde(default)]
    direction: Option<String>,
    #[serde(default = "ten")]
    max_swipes: u32,
    /// Where to swipe (default: screen center).
    x: Option<f64>,
    y: Option<f64>,
}

fn ten() -> u32 {
    10
}

/// Swipes until the element is fully on screen.
async fn ui_scroll_to(State(pool): State<AppState>, L(lease): L, Json(r): Json<ScrollToReq>) -> ApiResult<Json<Value>> {
    let bridge = lease.bridge(&pool).await?;
    let info = bridge.call(json!({"op": "info"}), Duration::from_secs(10)).await?;
    let (w, h) = (info["points"]["width"].as_f64().unwrap_or(400.0), info["points"]["height"].as_f64().unwrap_or(800.0));
    let dir = r.direction.as_deref().unwrap_or("down");
    for swipes in 0..=r.max_swipes {
        if let Some(e) = find(&bridge, &r.query).await?.into_iter().nth(r.query.index) {
            let f = &e["frame"];
            let (x, y, fw, fh) = (f["x"].as_f64().unwrap_or(-1.0), f["y"].as_f64().unwrap_or(-1.0), f["width"].as_f64().unwrap_or(0.0), f["height"].as_f64().unwrap_or(0.0));
            // Leave room for the status bar and home indicator.
            if x >= 0.0 && y >= 50.0 && x + fw <= w && y + fh <= h - 34.0 {
                return Ok(Json(json!({ "element": e, "swipes": swipes })));
            }
        }
        if swipes < r.max_swipes {
            bridge.call(scroll_swipe(dir, r.x.unwrap_or(w / 2.0), r.y.unwrap_or(h / 2.0), h * 0.35, 0.35, false)?, Duration::from_secs(15)).await?;
            tokio::time::sleep(Duration::from_millis(400)).await; // let scrolling settle
        }
    }
    Err(ApiError::not_found(format!("{} not fully visible after {} swipes {dir}", r.query.describe(), r.max_swipes)))
}
