//! Device state: URLs, location, appearance, permissions, push, pasteboard, media, status bar…

use super::{device_info, save_body, scratch, AppState, L};
use crate::error::{ApiError, ApiResult};
use crate::proc::{simctl, simctl_with};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::time::Duration;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/device", get(info))
        .route("/open-url", post(open_url))
        .route("/location", post(location).delete(clear_location))
        .route("/appearance", post(appearance))
        .route("/content-size", post(content_size))
        .route("/increase-contrast", post(increase_contrast))
        .route("/status-bar", post(status_bar).delete(clear_status_bar))
        .route("/privacy", post(privacy))
        .route("/push", post(push))
        .route("/pasteboard", get(get_pasteboard))
        .route("/pasteboard", put(set_pasteboard))
        .route("/media", post(add_media))
        .route("/keychain/root-cert", post(add_root_cert))
        .route("/keychain/reset", post(reset_keychain))
        .route("/biometrics", post(biometrics))
        .route("/notify", post(notify))
        .route("/memory-warning", post(memory_warning))
        .route("/locale", post(locale))
        .route("/spawn", post(spawn))
}

async fn info(State(pool): State<AppState>, L(lease): L) -> ApiResult<Json<Value>> {
    let b = lease.bridge(&pool).await?;
    Ok(Json(device_info(b.call(json!({"op": "info"}), Duration::from_secs(10)).await?)))
}

#[derive(Deserialize)]
struct UrlReq {
    url: String,
}

/// Deep links, universal links (if kept by slimming) and web pages.
async fn open_url(L(lease): L, Json(r): Json<UrlReq>) -> ApiResult<StatusCode> {
    simctl(&["openurl", &lease.udid, &r.url]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LocationReq {
    latitude: Option<f64>,
    longitude: Option<f64>,
    /// A route to drive: [[lat, lon], ...]
    waypoints: Option<Vec<[f64; 2]>>,
    /// Route speed in meters per second (default 20).
    speed: Option<f64>,
    /// One of the built-in scenarios (`xcrun simctl location <udid> list`).
    scenario: Option<String>,
}

async fn location(L(lease): L, Json(r): Json<LocationReq>) -> ApiResult<StatusCode> {
    let u = lease.udid.as_str();
    match (r.latitude.zip(r.longitude), &r.waypoints, &r.scenario) {
        (Some((lat, lon)), None, None) => simctl(&["location", u, "set", &format!("{lat},{lon}")]).await?,
        (None, Some(w), None) if w.len() >= 2 => {
            let mut args = vec!["location".to_string(), u.into(), "start".into(), format!("--speed={}", r.speed.unwrap_or(20.0))];
            args.extend(w.iter().map(|[a, b]| format!("{a},{b}")));
            simctl(&args).await?
        }
        (None, None, Some(s)) => simctl(&["location", u, "run", s]).await?,
        _ => return Err(ApiError::bad_request("pass latitude+longitude, waypoints (2 or more [lat, lon]), or scenario")),
    };
    Ok(StatusCode::NO_CONTENT)
}

async fn clear_location(L(lease): L) -> ApiResult<StatusCode> {
    simctl(&["location", &lease.udid, "clear"]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ModeReq {
    mode: String,
}

/// {"mode": "light" | "dark"}
async fn appearance(L(lease): L, Json(r): Json<ModeReq>) -> ApiResult<StatusCode> {
    if !matches!(r.mode.as_str(), "light" | "dark") {
        return Err(ApiError::bad_request("mode is light or dark"));
    }
    simctl(&["ui", &lease.udid, "appearance", &r.mode]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SizeReq {
    size: String,
}

/// Dynamic Type: extra-small … extra-extra-extra-large, accessibility-medium … accessibility-extra-extra-extra-large,
/// or increment / decrement.
async fn content_size(L(lease): L, Json(r): Json<SizeReq>) -> ApiResult<StatusCode> {
    simctl(&["ui", &lease.udid, "content_size", &r.size]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct EnabledReq {
    enabled: bool,
}

async fn increase_contrast(L(lease): L, Json(r): Json<EnabledReq>) -> ApiResult<StatusCode> {
    simctl(&["ui", &lease.udid, "increase_contrast", if r.enabled { "enabled" } else { "disabled" }]).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Any `simctl status_bar override` flag, snake_case or camelCase:
/// {"time": "9:41", "battery_level": 100, "battery_state": "charged", "wifi_bars": 3, "cellular_bars": 4,
///  "data_network": "wifi", "operator_name": ""}
async fn status_bar(L(lease): L, Json(r): Json<Map<String, Value>>) -> ApiResult<StatusCode> {
    let mut args = vec!["status_bar".to_string(), lease.udid.clone(), "override".into()];
    for (k, v) in &r {
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(ApiError::bad_request(format!("bad status bar key {k:?}")));
        }
        let mut flag = String::from("--");
        let mut upper = false;
        for c in k.chars() {
            if c == '_' {
                upper = true;
            } else if upper {
                flag.push(c.to_ascii_uppercase());
                upper = false;
            } else {
                flag.push(c);
            }
        }
        args.push(flag);
        args.push(match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    }
    simctl(&args).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn clear_status_bar(L(lease): L) -> ApiResult<StatusCode> {
    simctl(&["status_bar", &lease.udid, "clear"]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PrivacyReq {
    /// grant | revoke | reset
    action: String,
    /// all, calendar, contacts-limited, contacts, location, location-always, photos-add, photos,
    /// media-library, microphone, motion, reminders, siri, camera (Xcode-dependent)
    service: String,
    bundle_id: Option<String>,
}

async fn privacy(L(lease): L, Json(r): Json<PrivacyReq>) -> ApiResult<StatusCode> {
    if !matches!(r.action.as_str(), "grant" | "revoke" | "reset") {
        return Err(ApiError::bad_request("action is grant, revoke or reset"));
    }
    let mut args = vec!["privacy", &lease.udid, &r.action, &r.service];
    if let Some(b) = &r.bundle_id {
        args.push(b);
    } else if r.action != "reset" {
        return Err(ApiError::bad_request("grant and revoke need bundle_id"));
    }
    simctl(&args).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PushReq {
    bundle_id: String,
    /// The APNs payload, e.g. {"aps": {"alert": "Hi", "badge": 1}}
    payload: Value,
}

/// Needs push kept on in the simfast config (`keep = ["push"]`).
async fn push(L(lease): L, Json(r): Json<PushReq>) -> ApiResult<StatusCode> {
    let payload = serde_json::to_vec(&r.payload).unwrap();
    simctl_with(&["push", &lease.udid, &r.bundle_id, "-"], Some(&payload), &[]).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_pasteboard(L(lease): L) -> ApiResult<String> {
    Ok(String::from_utf8_lossy(&simctl(&["pbpaste", &lease.udid]).await?.stdout).into_owned())
}

async fn set_pasteboard(L(lease): L, text: String) -> ApiResult<StatusCode> {
    simctl_with(&["pbcopy", &lease.udid], Some(text.as_bytes()), &[]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MediaQuery {
    /// File name with extension, which tells Photos what it is (photo.jpg, clip.mov, contact.vcf).
    name: String,
}

async fn add_media(L(lease): L, Query(q): Query<MediaQuery>, body: Body) -> ApiResult<StatusCode> {
    let name = std::path::Path::new(&q.name).file_name().ok_or_else(|| ApiError::bad_request("name must be a file name"))?.to_string_lossy().to_string();
    let path = scratch(&lease, &name);
    save_body(body, &path).await?;
    let res = simctl(&["addmedia", &lease.udid, &path.to_string_lossy()]).await;
    let _ = tokio::fs::remove_file(&path).await;
    res.map_err(|e| {
        let msg = format!("{e:#}");
        if msg.contains("PHPhotosErrorDomain") {
            ApiError(StatusCode::CONFLICT, format!("{msg}\nhint: slim simulators turn Photos off; add \"photos\" to keep in the profile's simfast config"))
        } else {
            ApiError::from(e)
        }
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body: a PEM or DER certificate to trust as a root CA (for TLS-intercepting proxies, staging CAs).
async fn add_root_cert(L(lease): L, body: Body) -> ApiResult<StatusCode> {
    let path = scratch(&lease, "root.pem");
    save_body(body, &path).await?;
    let res = simctl(&["keychain", &lease.udid, "add-root-cert", &path.to_string_lossy()]).await;
    let _ = tokio::fs::remove_file(&path).await;
    res?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reset_keychain(L(lease): L) -> ApiResult<StatusCode> {
    simctl(&["keychain", &lease.udid, "reset"]).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn notify_post(udid: &str, name: &str) -> ApiResult<()> {
    simctl(&["spawn", udid, "notifyutil", "-p", name]).await?;
    Ok(())
}

#[derive(Deserialize)]
struct BiometricsReq {
    /// enroll | unenroll | match | fail
    action: String,
    /// face (default) or touch
    #[serde(default = "face")]
    kind: String,
}

fn face() -> String {
    "face".into()
}

/// Face ID / Touch ID: enrol the device, then answer the next prompt with a match or a failure.
async fn biometrics(L(lease): L, Json(r): Json<BiometricsReq>) -> ApiResult<StatusCode> {
    let u = lease.udid.as_str();
    let sensor = match r.kind.as_str() {
        "face" => "pearl",
        "touch" => "fingerTouch",
        _ => return Err(ApiError::bad_request("kind is face or touch")),
    };
    match r.action.as_str() {
        "enroll" | "unenroll" => {
            let state = if r.action == "enroll" { "1" } else { "0" };
            simctl(&["spawn", u, "notifyutil", "-s", "com.apple.BiometricKit.enrollmentChanged", state]).await?;
            notify_post(u, "com.apple.BiometricKit.enrollmentChanged").await?;
        }
        "match" => notify_post(u, &format!("com.apple.BiometricKit_Sim.{sensor}.match")).await?,
        "fail" => notify_post(u, &format!("com.apple.BiometricKit_Sim.{sensor}.nomatch")).await?,
        _ => return Err(ApiError::bad_request("action is enroll, unenroll, match or fail")),
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NotifyReq {
    name: String,
}

/// Posts a Darwin notification inside the simulator.
async fn notify(L(lease): L, Json(r): Json<NotifyReq>) -> ApiResult<StatusCode> {
    notify_post(&lease.udid, &r.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn memory_warning(L(lease): L) -> ApiResult<StatusCode> {
    notify_post(&lease.udid, "com.apple.UIKit.SimulateMemoryWarning").await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LocaleReq {
    /// Preferred languages, e.g. ["fr-FR", "en"]
    languages: Option<Vec<String>>,
    /// e.g. fr_FR
    locale: Option<String>,
}

/// Sets the system language/region. Apps pick it up when relaunched.
async fn locale(L(lease): L, Json(r): Json<LocaleReq>) -> ApiResult<StatusCode> {
    let u = lease.udid.as_str();
    if let Some(langs) = &r.languages {
        let mut args = vec!["spawn", u, "defaults", "write", ".GlobalPreferences", "AppleLanguages", "-array"];
        args.extend(langs.iter().map(String::as_str));
        simctl(&args).await?;
    }
    if let Some(l) = &r.locale {
        simctl(&["spawn", u, "defaults", "write", ".GlobalPreferences", "AppleLocale", "-string", l]).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SpawnReq {
    /// argv[0] is a path inside the simulator runtime, e.g. /usr/bin/defaults
    argv: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    stdin: Option<String>,
}

/// Runs a process inside the simulator and returns its output. Off unless allow_spawn.
async fn spawn(State(pool): State<AppState>, L(lease): L, Json(r): Json<SpawnReq>) -> ApiResult<Json<Value>> {
    if !pool.cfg.allow_spawn {
        return Err(ApiError::forbidden("spawn is disabled on this server (allow_spawn = false)"));
    }
    if r.argv.is_empty() {
        return Err(ApiError::bad_request("argv is empty"));
    }
    let mut args = vec!["simctl".to_string(), "spawn".into(), lease.udid.clone()];
    args.extend(r.argv.iter().cloned());
    let env: Vec<(String, String)> = r.env.iter().map(|(k, v)| (format!("SIMCTL_CHILD_{k}"), v.clone())).collect();
    let mut cmd = tokio::process::Command::new("xcrun");
    cmd.args(&args).envs(env).kill_on_drop(true).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    cmd.stdin(if r.stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() });
    let mut child = cmd.spawn()?;
    if let Some(input) = r.stdin {
        use tokio::io::AsyncWriteExt;
        let mut si = child.stdin.take().unwrap();
        si.write_all(input.as_bytes()).await?;
    }
    let out = tokio::time::timeout(Duration::from_secs(300), child.wait_with_output()).await.map_err(|_| ApiError::timeout("spawn timed out after 300s"))??;
    Ok(Json(json!({
        "exit_code": out.status.code(),
        "stdout": String::from_utf8_lossy(&out.stdout),
        "stderr": String::from_utf8_lossy(&out.stderr),
    })))
}
