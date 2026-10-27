mod apps;
mod device;
mod input;
mod logs;
mod net;
mod screen;

use crate::error::{ApiError, ApiResult};
use crate::pool::{AcquireRequest, Lease, Pool};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub type AppState = Arc<Pool>;

pub fn router(pool: AppState) -> Router {
    let lease = Router::new()
        .route("/", get(lease_info).delete(release))
        .route("/renew", post(renew))
        .merge(apps::routes())
        .merge(device::routes())
        .merge(input::routes())
        .merge(screen::routes())
        .merge(logs::routes())
        .merge(net::routes());
    let api = Router::new()
        .route("/pool", get(pool_status))
        .route("/leases", get(list_leases).post(acquire))
        .nest("/leases/{lease}", lease)
        .layer(middleware::from_fn_with_state(pool.clone(), auth));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/v1", api)
        .layer(DefaultBodyLimit::max(8 << 30))
        .with_state(pool)
}

/// Bearer token in `Authorization`, or `?token=` for browsers and WebSockets.
async fn auth(State(pool): State<AppState>, req: Request, next: Next) -> Response {
    if pool.cfg.tokens.is_empty() {
        return next.run(req).await;
    }
    let header = req.headers().get("authorization").and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer ")).map(str::to_string);
    let query = req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")).map(str::to_string));
    let ok = header.or(query).is_some_and(|t| pool.cfg.tokens.iter().any(|k| constant_time_eq(k.as_bytes(), t.as_bytes())));
    if !ok {
        return ApiError(StatusCode::UNAUTHORIZED, "missing or wrong token (Authorization: Bearer … or ?token=)".into()).into_response();
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Extracts the lease named in the path (and counts the request as activity on it).
pub struct L(pub Arc<Lease>);

impl FromRequestParts<AppState> for L {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, pool: &AppState) -> Result<Self, ApiError> {
        let Path(p): Path<HashMap<String, String>> = Path::from_request_parts(parts, pool).await.map_err(|e| ApiError::bad_request(e.body_text()))?;
        let id = p.get("lease").ok_or_else(|| ApiError::bad_request("no lease in path"))?;
        pool.get(id).map(L)
    }
}

async fn pool_status(State(pool): State<AppState>) -> Json<Value> {
    Json(pool.status())
}

async fn list_leases(State(pool): State<AppState>) -> Json<Value> {
    Json(json!({ "leases": pool.list().iter().map(|l| l.json()).collect::<Vec<_>>() }))
}

async fn acquire(State(pool): State<AppState>, Json(req): Json<AcquireRequest>) -> ApiResult<impl IntoResponse> {
    let lease = pool.acquire(req).await?;
    let mut v = lease.json();
    // Fill in the screen size now; clients need it for coordinates and it warms the bridge up.
    if let Ok(b) = lease.bridge(&pool).await {
        if let Ok(info) = b.call(json!({"op": "info"}), Duration::from_secs(10)).await {
            v["device"] = device_info(info);
        }
    }
    Ok((StatusCode::CREATED, Json(v)))
}

pub fn device_info(mut info: Value) -> Value {
    if let Some(o) = info.as_object_mut() {
        o.remove("id");
        o.remove("ok");
    }
    info
}

async fn lease_info(State(pool): State<AppState>, L(lease): L) -> ApiResult<Json<Value>> {
    let mut v = lease.json();
    let b = lease.bridge(&pool).await?;
    v["device"] = device_info(b.call(json!({"op": "info"}), Duration::from_secs(10)).await?);
    lease.prune();
    let children: Vec<Value> = lease.children.lock().unwrap().iter().map(|(id, t)| json!({"id": id, "kind": t.kind, "info": t.info})).collect();
    v["sessions"] = json!(children);
    Ok(Json(v))
}

async fn release(State(pool): State<AppState>, L(lease): L) -> ApiResult<StatusCode> {
    pool.release(&lease.id, "released by client").await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct RenewReq {
    ttl_secs: Option<u64>,
}

async fn renew(L(lease): L, body: Option<Json<RenewReq>>) -> Json<Value> {
    lease.touch(body.and_then(|b| b.0.ttl_secs).map(Duration::from_secs));
    Json(lease.json())
}

/// Streams a request body to a file without holding it in memory.
pub async fn save_body(body: Body, path: &FsPath) -> ApiResult<u64> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut f = tokio::fs::File::create(path).await?;
    let mut stream = body.into_data_stream();
    let mut n = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::bad_request(format!("reading upload: {e}")))?;
        n += chunk.len() as u64;
        f.write_all(&chunk).await?;
    }
    f.flush().await?;
    if n == 0 {
        return Err(ApiError::bad_request("empty request body"));
    }
    Ok(n)
}

pub fn scratch(lease: &Lease, name: &str) -> std::path::PathBuf {
    lease.dir.join(format!("{}-{name}", uuid::Uuid::new_v4().simple()))
}
