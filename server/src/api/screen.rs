//! Screenshots, live screen (WebSocket and MJPEG), the browser live view, and recordings.

use super::input::run_action;
use super::{AppState, L};
use crate::bridge::StreamSettings;
use crate::error::{ApiError, ApiResult};
use crate::pool::Tracked;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio_util::io::ReaderStream;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/screenshot", get(screenshot))
        .route("/screen/ws", get(screen_ws))
        .route("/screen/mjpeg", get(mjpeg))
        .route("/view", get(view))
        .route("/recordings", get(list_recordings).post(start_recording))
        .route("/recordings/{rec}", get(download_recording))
        .route("/recordings/{rec}/stop", post(stop_recording))
}

#[derive(Deserialize)]
struct ShotQuery {
    /// png (default) or jpeg
    format: Option<String>,
    /// JPEG quality 0-1 (default 0.8)
    quality: Option<f64>,
    /// 0-1 of native pixels (default 1)
    scale: Option<f64>,
}

async fn screenshot(State(pool): State<AppState>, L(lease): L, Query(q): Query<ShotQuery>) -> ApiResult<Response> {
    let format = match q.format.as_deref().unwrap_or("png") {
        "png" => "png",
        "jpeg" | "jpg" => "jpeg",
        other => return Err(ApiError::bad_request(format!("format {other:?}: png or jpeg"))),
    };
    let bridge = lease.bridge(&pool).await?;
    let r = bridge.call(json!({"op": "screenshot", "format": format, "quality": q.quality.unwrap_or(0.8), "scale": q.scale.unwrap_or(1.0)}), Duration::from_secs(20)).await?;
    let data = base64::engine::general_purpose::STANDARD.decode(r["data"].as_str().unwrap_or("")).map_err(|e| anyhow::anyhow!("bad image from simbridge: {e}"))?;
    Ok(([(header::CONTENT_TYPE, format!("image/{format}"))], data).into_response())
}

#[derive(Deserialize)]
struct StreamQuery {
    fps: Option<f64>,
    quality: Option<f64>,
    scale: Option<f64>,
}

impl StreamQuery {
    fn settings(&self) -> StreamSettings {
        StreamSettings {
            fps: self.fps.unwrap_or(30.0).clamp(1.0, 60.0),
            quality: self.quality.unwrap_or(0.6).clamp(0.1, 1.0),
            scale: self.scale.unwrap_or(0.5).clamp(0.1, 1.0),
        }
    }
}

/// Binary messages: JPEG frames (sent when the screen changes, and at least once a second).
/// Text messages from the client: input actions as in POST /input, e.g.
/// {"type": "touch", "phase": "down", "x": 100, "y": 200}. Errors come back as {"error": "..."}.
async fn screen_ws(State(pool): State<AppState>, L(lease): L, Query(q): Query<StreamQuery>, ws: WebSocketUpgrade) -> ApiResult<Response> {
    let bridge = lease.bridge(&pool).await?;
    let info = bridge.call(json!({"op": "info"}), Duration::from_secs(10)).await?;
    let mut frames = bridge.subscribe(q.settings()).await?;
    let hold = lease.hold();
    Ok(ws.on_upgrade(move |mut socket: WebSocket| async move {
        let hello = json!({"type": "info", "device": super::device_info(info)});
        if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
            return;
        }
        // Actions run in order on their own task so long gestures don't stall the picture.
        let (act_tx, mut act_rx) = tokio::sync::mpsc::channel::<Value>(256);
        let (res_tx, mut res_rx) = tokio::sync::mpsc::channel::<Value>(64);
        let (b2, l2) = (bridge.clone(), lease.clone());
        let worker = tokio::spawn(async move {
            while let Some(a) = act_rx.recv().await {
                l2.touch(None);
                if let Err(e) = run_action(&b2, &l2, a).await {
                    let _ = res_tx.send(json!({"type": "error", "error": e.1})).await;
                }
            }
        });
        loop {
            tokio::select! {
                _ = hold.cancelled() => {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
                f = frames.next() => {
                    let Some(f) = f else { break };
                    if socket.send(Message::Binary(f)).await.is_err() { break }
                }
                r = res_rx.recv() => {
                    if let Some(r) = r {
                        if socket.send(Message::Text(r.to_string().into())).await.is_err() { break }
                    }
                }
                m = socket.recv() => match m {
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<Value>(&t) {
                        Ok(a) => { let _ = act_tx.try_send(a); }
                        Err(e) => { let _ = socket.send(Message::Text(json!({"type": "error", "error": format!("bad JSON: {e}")}).to_string().into())).await; }
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
            }
        }
        worker.abort();
    }))
}

/// multipart/x-mixed-replace JPEG stream; works in an <img> tag and most video tools.
async fn mjpeg(State(pool): State<AppState>, L(lease): L, Query(q): Query<StreamQuery>) -> ApiResult<Response> {
    let bridge = lease.bridge(&pool).await?;
    let frames = bridge.subscribe(q.settings()).await?;
    let cancel = lease.hold();
    let stream = futures::stream::unfold((frames, cancel), |(mut frames, cancel)| async move {
        let f = tokio::select! {
            _ = cancel.cancelled() => None,
            f = frames.next() => f,
        }?;
        let mut part = format!("--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", f.len()).into_bytes();
        part.extend_from_slice(&f);
        part.extend_from_slice(b"\r\n");
        Some((Ok::<_, std::io::Error>(bytes::Bytes::from(part)), (frames, cancel)))
    });
    Ok(([(header::CONTENT_TYPE, "multipart/x-mixed-replace; boundary=frame"), (header::CACHE_CONTROL, "no-store")], Body::from_stream(stream)).into_response())
}

async fn view(L(_lease): L) -> Html<&'static str> {
    Html(include_str!("view.html"))
}

#[derive(Deserialize, Default)]
struct RecordReq {
    /// h264 (default) or hevc
    codec: Option<String>,
}

async fn start_recording(L(lease): L, body: Option<Json<RecordReq>>) -> ApiResult<(StatusCode, Json<Value>)> {
    let codec = body.and_then(|b| b.0.codec).unwrap_or_else(|| "h264".into());
    if !matches!(codec.as_str(), "h264" | "hevc") {
        return Err(ApiError::bad_request("codec is h264 or hevc"));
    }
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let path = lease.dir.join(format!("recording-{id}.mp4"));
    let child = tokio::process::Command::new("xcrun")
        .args(["simctl", "io", &lease.udid, "recordVideo", &format!("--codec={codec}"), "--force"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let info = json!({"id": id, "codec": codec, "started_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()});
    lease.track(format!("rec-{id}"), Tracked { kind: "recording", child, info: info.clone() });
    Ok((StatusCode::CREATED, Json(info)))
}

async fn list_recordings(L(lease): L) -> Json<Value> {
    lease.prune();
    let running: Vec<Value> = lease.children.lock().unwrap().values().filter(|t| t.kind == "recording").map(|t| t.info.clone()).collect();
    let mut done = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&lease.dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_prefix("recording-").and_then(|n| n.strip_suffix(".mp4")) {
                if !running.iter().any(|r| r["id"] == id) {
                    done.push(json!({"id": id, "bytes": e.metadata().map(|m| m.len()).unwrap_or(0)}));
                }
            }
        }
    }
    Json(json!({"recording": running, "finished": done}))
}

fn rec_id(p: &HashMap<String, String>) -> ApiResult<String> {
    let id = p["rec"].clone();
    if !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ApiError::bad_request("bad recording id"));
    }
    Ok(id)
}

/// Stops the recorder (it finalizes the file on SIGINT) and returns the MP4.
async fn stop_recording(L(lease): L, Path(p): Path<HashMap<String, String>>) -> ApiResult<Response> {
    let id = rec_id(&p)?;
    let tracked = lease.children.lock().unwrap().remove(&format!("rec-{id}"));
    let mut t = tracked.ok_or_else(|| ApiError::not_found(format!("no running recording {id}")))?;
    if let Some(pid) = t.child.id() {
        unsafe { libc::kill(pid as i32, libc::SIGINT) };
    }
    if tokio::time::timeout(Duration::from_secs(30), t.child.wait()).await.is_err() {
        let _ = t.child.kill().await;
        return Err(anyhow::anyhow!("recorder didn't finish within 30s").into());
    }
    send_recording(&lease.dir.join(format!("recording-{id}.mp4"))).await
}

async fn download_recording(L(lease): L, Path(p): Path<HashMap<String, String>>) -> ApiResult<Response> {
    let id = rec_id(&p)?;
    if lease.children.lock().unwrap().contains_key(&format!("rec-{id}")) {
        return Err(ApiError::conflict("still recording; POST …/stop first"));
    }
    send_recording(&lease.dir.join(format!("recording-{id}.mp4"))).await
}

async fn send_recording(path: &std::path::Path) -> ApiResult<Response> {
    let f = tokio::fs::File::open(path).await.map_err(|_| ApiError::not_found("no such recording (or it produced no file)"))?;
    let len = f.metadata().await?.len();
    Ok(([(header::CONTENT_TYPE, "video/mp4".to_string()), (header::CONTENT_LENGTH, len.to_string())], Body::from_stream(ReaderStream::new(f))).into_response())
}
