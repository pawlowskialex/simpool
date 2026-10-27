//! Unified logging and crash reports.

use super::{AppState, L};
use crate::error::{ApiError, ApiResult};
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;

pub fn routes() -> Router<AppState> {
    Router::new().route("/logs", get(logs)).route("/crashes", get(crashes)).route("/crashes/{name}", get(crash))
}

#[derive(Deserialize)]
struct LogQuery {
    /// NSPredicate, e.g. subsystem == "com.example.app" or process == "MyApp"
    predicate: Option<String>,
    /// Shorthand for process == "<name>"
    process: Option<String>,
    /// default, info or debug (default info)
    level: Option<String>,
    /// Past logs instead of a live stream, e.g. 30s, 5m, 1h
    last: Option<String>,
}

/// NDJSON, one log entry per line. Live (until the client disconnects) unless `last` is given.
async fn logs(L(lease): L, Query(q): Query<LogQuery>) -> ApiResult<Response> {
    let level = q.level.as_deref().unwrap_or("info");
    if !matches!(level, "default" | "info" | "debug") {
        return Err(ApiError::bad_request("level is default, info or debug"));
    }
    let mut predicate = q.predicate.clone();
    if let Some(p) = &q.process {
        let clause = format!("process == \"{}\"", p.replace('"', ""));
        predicate = Some(match predicate {
            Some(other) => format!("({other}) AND {clause}"),
            None => clause,
        });
    }
    let mut args: Vec<String> = vec!["simctl".into(), "spawn".into(), lease.udid.clone(), "log".into()];
    match &q.last {
        Some(last) => {
            if !last.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(ApiError::bad_request("last looks like 30s, 5m or 1h"));
            }
            args.extend(["show".into(), "--last".into(), last.clone(), "--style".into(), "ndjson".into()]);
            if level != "default" {
                args.push(format!("--{level}"));
            }
        }
        None => args.extend(["stream".into(), "--style".into(), "ndjson".into(), "--level".into(), level.into()]),
    }
    if let Some(p) = predicate {
        args.extend(["--predicate".into(), p]);
    }
    let mut child = tokio::process::Command::new("xcrun").args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
    let out = child.stdout.take().unwrap();
    let cancel = lease.hold();
    // The child rides along in the stream state, so a client disconnect kills `log`.
    let stream = futures::stream::unfold((out, child, cancel), |(mut out, child, cancel)| async move {
        let mut buf = vec![0u8; 32 << 10];
        let n = tokio::select! {
            _ = cancel.cancelled() => return None,
            n = out.read(&mut buf) => n.ok()?,
        };
        if n == 0 {
            return None;
        }
        buf.truncate(n);
        Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), (out, child, cancel)))
    });
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson"), (header::CACHE_CONTROL, "no-store")], Body::from_stream(stream)).into_response())
}

fn reports_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Logs/DiagnosticReports")
}

/// Crash and hang reports from this simulator since the lease started.
async fn crashes(L(lease): L) -> ApiResult<Json<Value>> {
    let since = lease.created;
    let udid = lease.udid.clone();
    let list = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        for e in std::fs::read_dir(reports_dir()).into_iter().flatten().flatten() {
            let path = e.path();
            if !path.extension().is_some_and(|x| x == "ips" || x == "crash") {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            if m.modified().map(|t| t < since).unwrap_or(true) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if !text.contains(&udid) {
                continue;
            }
            // .ips files start with a one-line JSON header.
            let header: Value = text.lines().next().and_then(|l| serde_json::from_str(l).ok()).unwrap_or(json!({}));
            out.push(json!({
                "name": e.file_name().to_string_lossy(),
                "app_name": header["app_name"],
                "bundle_id": header["bundleID"],
                "timestamp": header["timestamp"],
                "bug_type": header["bug_type"],
                "bytes": m.len(),
            }));
        }
        out.sort_by(|a, b| b["timestamp"].as_str().cmp(&a["timestamp"].as_str()));
        out
    })
    .await
    .map_err(|e| anyhow::anyhow!(e))?;
    Ok(Json(json!({ "crashes": list })))
}

async fn crash(L(lease): L, Path(p): Path<HashMap<String, String>>) -> ApiResult<Response> {
    let name = &p["name"];
    if name.contains('/') || name.starts_with('.') {
        return Err(ApiError::bad_request("bad report name"));
    }
    let text = tokio::fs::read_to_string(reports_dir().join(name)).await.map_err(|_| ApiError::not_found("no such report"))?;
    if !text.contains(&lease.udid) {
        return Err(ApiError::not_found("no such report for this simulator"));
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}
