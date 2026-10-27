//! Apps (install, launch, list), their output, and files in the simulator.

use super::{save_body, scratch, AppState, L};
use crate::error::{ApiError, ApiResult};
use crate::pool::Lease;
use crate::proc::{self, plist_to_json, simctl, simctl_with};
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path as FsPath, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/apps", get(list).post(install))
        .route("/apps/{bundle}", get(app_info).delete(uninstall))
        .route("/apps/{bundle}/launch", post(launch))
        .route("/apps/{bundle}/terminate", post(terminate))
        .route("/apps/{bundle}/output", get(output))
        .route("/files", get(read_files).put(write_file).delete(delete_files))
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    user: bool,
}

/// "file:///path/" -> "/path"
fn file_path(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_str) {
        Some(s) => json!(s.strip_prefix("file://").unwrap_or(s).trim_end_matches('/')),
        None => Value::Null,
    }
}

async fn installed(lease: &Lease) -> ApiResult<BTreeMap<String, Value>> {
    let out = simctl(&["listapps", &lease.udid]).await?;
    let all = plist_to_json(&out.stdout).await?;
    let mut apps = BTreeMap::new();
    for (bundle, a) in all.as_object().into_iter().flatten() {
        apps.insert(
            bundle.clone(),
            json!({
                "bundle_id": bundle,
                "name": a.get("CFBundleDisplayName").or(a.get("CFBundleName")),
                "version": a.get("CFBundleShortVersionString"),
                "build": a.get("CFBundleVersion"),
                "type": a.get("ApplicationType"),
                "path": a.get("Path"),
                "data_container": file_path(a.get("DataContainer")),
                "group_containers": a.get("GroupContainers").and_then(Value::as_object).map(|g| g.iter().map(|(k, v)| (k.clone(), file_path(Some(v)))).collect::<serde_json::Map<_, _>>()),
            }),
        );
    }
    Ok(apps)
}

async fn list(L(lease): L, Query(q): Query<ListQuery>) -> ApiResult<Json<Value>> {
    let apps: Vec<Value> = installed(&lease).await?.into_values().filter(|a| !q.user || a["type"] == "User").collect();
    Ok(Json(json!({ "apps": apps })))
}

async fn app_info(L(lease): L, Path(p): Path<HashMap<String, String>>) -> ApiResult<Json<Value>> {
    let bundle = &p["bundle"];
    installed(&lease).await?.remove(bundle).map(Json).ok_or_else(|| ApiError::not_found(format!("{bundle} isn't installed")))
}

/// Body: a zipped .app (`ditto -c -k --keepParent X.app x.zip`), an .ipa, or a tar(.gz) of a .app.
async fn install(L(lease): L, body: Body) -> ApiResult<Json<Value>> {
    let upload = scratch(&lease, "upload");
    save_body(body, &upload).await?;
    let dest = scratch(&lease, "app");
    tokio::fs::create_dir_all(&dest).await?;
    let mut magic = [0u8; 512];
    let n = tokio::fs::File::open(&upload).await?.read(&mut magic).await?;
    let res = async {
        if magic[..n].starts_with(b"PK\x03\x04") {
            proc::run("ditto", ["-x".as_ref(), "-k".as_ref(), upload.as_os_str(), dest.as_os_str()], None, &[], Duration::from_secs(300)).await?;
        } else if magic[..n].starts_with(&[0x1f, 0x8b]) || (n > 262 && &magic[257..262] == b"ustar") {
            proc::run("tar", ["-xf".as_ref(), upload.as_os_str(), "-C".as_ref(), dest.as_os_str()], None, &[], Duration::from_secs(300)).await?;
        } else {
            return Err(ApiError::bad_request("upload a zip of the .app, an .ipa, or a tar(.gz) containing a .app"));
        }
        let app = find_app(&dest, 3).ok_or_else(|| ApiError::bad_request("no .app bundle in the upload"))?;
        let info = plist_to_json(&tokio::fs::read(app.join("Info.plist")).await.map_err(|_| ApiError::bad_request("the .app has no Info.plist"))?).await?;
        let bundle = info["CFBundleIdentifier"].as_str().ok_or_else(|| ApiError::bad_request("Info.plist has no CFBundleIdentifier"))?.to_string();
        simctl(&["install", &lease.udid, &app.to_string_lossy()]).await?;
        Ok(Json(json!({
            "bundle_id": bundle,
            "name": info.get("CFBundleDisplayName").or(info.get("CFBundleName")),
            "version": info.get("CFBundleShortVersionString"),
            "build": info.get("CFBundleVersion"),
        })))
    }
    .await;
    let _ = tokio::fs::remove_file(&upload).await;
    let _ = tokio::fs::remove_dir_all(&dest).await;
    res
}

fn find_app(dir: &FsPath, depth: u32) -> Option<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    if let Some(app) = entries.iter().find(|p| p.extension().is_some_and(|e| e == "app") && p.is_dir()) {
        return Some(app.clone());
    }
    if depth == 0 {
        return None;
    }
    entries.iter().filter(|p| p.is_dir() && p.file_name().is_none_or(|n| n != "__MACOSX")).find_map(|p| find_app(p, depth - 1))
}

async fn uninstall(L(lease): L, Path(p): Path<HashMap<String, String>>) -> ApiResult<StatusCode> {
    simctl(&["uninstall", &lease.udid, &p["bundle"]]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub struct LaunchReq {
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Start suspended until a debugger attaches (see POST /debug).
    #[serde(default)]
    pub wait_for_debugger: bool,
    /// Kill a running instance first (default true).
    pub terminate_existing: Option<bool>,
    /// Send the app's stdout/stderr to a log readable at GET /apps/{bundle}/output (default true).
    pub capture_output: Option<bool>,
}

pub fn output_path(lease: &Lease, bundle: &str) -> PathBuf {
    lease.dir.join(format!("output-{}.log", bundle.replace('/', "_")))
}

pub async fn launch_app(lease: &Lease, bundle: &str, req: &LaunchReq) -> ApiResult<u32> {
    let mut args = vec!["launch".to_string()];
    if req.terminate_existing.unwrap_or(true) {
        args.push("--terminate-running-process".into());
    }
    if req.wait_for_debugger {
        args.push("--wait-for-debugger".into());
    }
    if req.capture_output.unwrap_or(true) {
        let out = output_path(lease, bundle);
        tokio::fs::write(&out, b"").await?;
        args.push(format!("--stdout={}", out.display()));
        args.push(format!("--stderr={}", out.display()));
    }
    args.push(lease.udid.clone());
    args.push(bundle.to_string());
    args.extend(req.args.iter().cloned());
    let env: Vec<(String, String)> = req.env.iter().map(|(k, v)| (format!("SIMCTL_CHILD_{k}"), v.clone())).collect();
    let out = simctl_with(&args, None, &env).await?;
    // "com.example.App: 12345"
    out.text()
        .rsplit(':')
        .next()
        .and_then(|p| p.trim().parse().ok())
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("couldn't read the pid from simctl launch: {}", out.text())))
}

async fn launch(L(lease): L, Path(p): Path<HashMap<String, String>>, body: Option<Json<LaunchReq>>) -> ApiResult<Json<Value>> {
    let req = body.map(|b| b.0).unwrap_or_default();
    let pid = launch_app(&lease, &p["bundle"], &req).await?;
    Ok(Json(json!({ "bundle_id": p["bundle"], "pid": pid })))
}

async fn terminate(L(lease): L, Path(p): Path<HashMap<String, String>>) -> ApiResult<StatusCode> {
    simctl(&["terminate", &lease.udid, &p["bundle"]]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct OutputQuery {
    #[serde(default)]
    follow: bool,
}

/// The app's captured stdout/stderr since its last launch; `?follow=true` keeps streaming.
async fn output(L(lease): L, Path(p): Path<HashMap<String, String>>, Query(q): Query<OutputQuery>) -> ApiResult<Response> {
    let path = output_path(&lease, &p["bundle"]);
    let file = tokio::fs::File::open(&path).await.map_err(|_| ApiError::not_found("no captured output; launch the app through this API first"))?;
    if !q.follow {
        return Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], Body::from_stream(ReaderStream::new(file))).into_response());
    }
    let cancel = lease.hold();
    let stream = futures::stream::unfold((file, cancel, path), |(mut f, cancel, path)| async move {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match f.read(&mut buf).await {
                Ok(0) => {
                    // A relaunch truncates the file; start over from the top.
                    let pos = f.stream_position().await.ok()?;
                    if tokio::fs::metadata(&path).await.ok()?.len() < pos {
                        f.seek(std::io::SeekFrom::Start(0)).await.ok()?;
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return None,
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                    }
                }
                Ok(n) => return Some((Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n])), (f, cancel, path))),
                Err(e) => return Some((Err(e), (f, cancel, path))),
            }
        }
    });
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], Body::from_stream(stream)).into_response())
}

#[derive(Deserialize)]
struct FilesQuery {
    /// "device" (the simulator's home), or with bundle: "data", "app", "groups" or a group id.
    #[serde(default = "data")]
    container: String,
    bundle: Option<String>,
    #[serde(default)]
    path: String,
    /// GET on a directory: send a .tar.gz of it instead of a listing.
    #[serde(default)]
    archive: bool,
    /// DELETE a directory and everything in it.
    #[serde(default)]
    recursive: bool,
}

fn data() -> String {
    "data".into()
}

async fn resolve(lease: &Lease, q: &FilesQuery) -> ApiResult<(PathBuf, PathBuf)> {
    let root = match (q.container.as_str(), &q.bundle) {
        ("device", _) => simctl(&["getenv", &lease.udid, "HOME"]).await?.text(),
        (kind, Some(bundle)) => simctl(&["get_app_container", &lease.udid, bundle, kind]).await.map_err(|e| ApiError::not_found(format!("{e:#}")))?.text(),
        (_, None) => return Err(ApiError::bad_request("pass bundle=<bundle id>, or container=device")),
    };
    let rel = FsPath::new(q.path.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(ApiError::bad_request("path must be relative to the container, without .."));
    }
    let root = PathBuf::from(root);
    Ok((root.join(rel), root))
}

async fn read_files(L(lease): L, Query(q): Query<FilesQuery>) -> ApiResult<Response> {
    let (path, root) = resolve(&lease, &q).await?;
    let meta = tokio::fs::metadata(&path).await.map_err(|_| ApiError::not_found(format!("{} doesn't exist", q.path)))?;
    if meta.is_file() {
        let f = tokio::fs::File::open(&path).await?;
        return Ok(([(header::CONTENT_TYPE, "application/octet-stream".to_string()), (header::CONTENT_LENGTH, meta.len().to_string())], Body::from_stream(ReaderStream::new(f))).into_response());
    }
    if q.archive {
        let mut child = tokio::process::Command::new("tar").arg("-czf").arg("-").arg("-C").arg(&path).arg(".").stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let out = child.stdout.take().unwrap();
        tokio::spawn(async move { child.wait().await });
        return Ok(([(header::CONTENT_TYPE, "application/gzip")], Body::from_stream(ReaderStream::new(out))).into_response());
    }
    let mut entries = Vec::new();
    let mut rd = tokio::fs::read_dir(&path).await?;
    while let Some(e) = rd.next_entry().await? {
        let Ok(m) = e.metadata().await else { continue };
        let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
        entries.push(json!({
            "name": e.file_name().to_string_lossy(),
            "type": if m.is_dir() { "dir" } else if m.is_symlink() { "link" } else { "file" },
            "size": m.len(),
            "modified": modified,
        }));
    }
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(json!({ "root": root, "path": q.path, "entries": entries })).into_response())
}

async fn write_file(L(lease): L, Query(q): Query<FilesQuery>, body: Body) -> ApiResult<Json<Value>> {
    let (path, _) = resolve(&lease, &q).await?;
    if q.path.trim_matches('/').is_empty() {
        return Err(ApiError::bad_request("path must name a file"));
    }
    let n = save_body(body, &path).await?;
    Ok(Json(json!({ "path": q.path, "bytes": n })))
}

async fn delete_files(L(lease): L, Query(q): Query<FilesQuery>) -> ApiResult<StatusCode> {
    let (path, _) = resolve(&lease, &q).await?;
    if q.path.trim_matches('/').is_empty() {
        return Err(ApiError::bad_request("refusing to delete the container itself"));
    }
    let meta = tokio::fs::symlink_metadata(&path).await.map_err(|_| ApiError::not_found(format!("{} doesn't exist", q.path)))?;
    if meta.is_dir() {
        if !q.recursive {
            return Err(ApiError::bad_request("that's a directory; pass recursive=true"));
        }
        tokio::fs::remove_dir_all(&path).await?;
    } else {
        tokio::fs::remove_file(&path).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
