//! Debugger sessions and TCP tunnels, all carried over WebSockets so they pass through the
//! same port and auth as the rest of the API.

use super::apps::{launch_app, LaunchReq};
use super::{AppState, L};
use crate::error::{ApiError, ApiResult};
use crate::pool::Tracked;
use crate::proc::simctl;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{delete, get};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use crate::pool::Hold;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/debug", get(list_debug).post(start_debug))
        .route("/debug/{session}", delete(stop_debug))
        .route("/debug/{session}/connect", get(connect_debug))
        .route("/tunnel", get(tunnel))
        .route("/reverse", get(reverse))
}

#[derive(Deserialize)]
struct DebugReq {
    /// Attach to this app: launch it suspended (launch=true, the default) or find its running process.
    bundle_id: Option<String>,
    /// Or attach to this pid.
    pid: Option<u32>,
    #[serde(default = "yes")]
    launch: bool,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// "dap" (lldb-dap, symbols resolved on this Mac; default) or "gdb-remote" (debugserver, for a remote lldb).
    #[serde(default = "dap")]
    protocol: String,
}

fn yes() -> bool {
    true
}
fn dap() -> String {
    "dap".into()
}

async fn running_pid(udid: &str, bundle: &str) -> ApiResult<u32> {
    let out = simctl(&["spawn", udid, "launchctl", "list"]).await?.text();
    out.lines()
        .filter(|l| l.contains(&format!("UIKitApplication:{bundle}[")))
        .find_map(|l| l.split_whitespace().next()?.parse().ok())
        .ok_or_else(|| ApiError::not_found(format!("{bundle} isn't running; pass launch=true")))
}

fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

async fn start_debug(State(pool): State<AppState>, L(lease): L, Json(r): Json<DebugReq>) -> ApiResult<(StatusCode, Json<Value>)> {
    let pid = match (r.pid, &r.bundle_id) {
        (Some(pid), None) => pid,
        (None, Some(b)) if r.launch => launch_app(&lease, b, &LaunchReq { args: r.args.clone(), env: r.env.clone(), wait_for_debugger: true, ..Default::default() }).await?,
        (None, Some(b)) => running_pid(&lease.udid, b).await?,
        _ => return Err(ApiError::bad_request("pass bundle_id or pid")),
    };
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let port;
    let child = match r.protocol.as_str() {
        "gdb-remote" => {
            // debugserver takes exactly one connection, so instead of racing it to its port we
            // listen and let it dial us; the connection waits here for the client's WebSocket.
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            port = listener.local_addr()?.port();
            let ds = std::path::Path::new(&pool.developer_dir).join("../SharedFrameworks/LLDB.framework/Resources/debugserver");
            let mut child = tokio::process::Command::new(ds)
                .args(["--reverse-connect", &format!("127.0.0.1:{port}"), &format!("--attach={pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let stream = tokio::select! {
                acc = tokio::time::timeout(Duration::from_secs(20), listener.accept()) => match acc {
                    Ok(Ok((s, _))) => s,
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_) => return Err(anyhow::anyhow!("debugserver didn't attach to pid {pid} within 20s").into()),
                },
                st = child.wait() => {
                    let mut err = String::new();
                    if let Some(mut e) = child.stderr.take() { let _ = e.read_to_string(&mut err).await; }
                    return Err(anyhow::anyhow!("debugserver exited ({}) before attaching: {}", st?, err.trim()).into());
                }
            };
            lease.debug_streams.lock().unwrap().insert(id.clone(), stream);
            if let Some(mut e) = child.stderr.take() {
                // Keep draining so a chatty debugserver never blocks on a full pipe.
                tokio::spawn(async move { tokio::io::copy(&mut e, &mut tokio::io::sink()).await });
            }
            child
        }
        "dap" => {
            port = free_port()?;
            let mut child = tokio::process::Command::new("xcrun")
                .args(["lldb-dap", "--connection", &format!("listen://127.0.0.1:{port}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            // lldb-dap serves any number of connections, so a probe connection is harmless.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                    break;
                }
                if let Ok(Some(st)) = child.try_wait() {
                    return Err(anyhow::anyhow!("lldb-dap exited ({st}) before listening").into());
                }
                if tokio::time::Instant::now() > deadline {
                    return Err(anyhow::anyhow!("lldb-dap didn't start listening within 20s").into());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            child
        }
        other => return Err(ApiError::bad_request(format!("protocol {other:?}: dap or gdb-remote"))),
    };

    let path = format!("/v1/leases/{}/debug/{id}/connect", lease.id);
    let hint = if r.protocol == "dap" {
        format!("DAP server for pid {pid}: send an `attach` request with \"pid\": {pid}. Use `simpool debug` to get a local TCP port for your editor.")
    } else {
        format!("debugserver attached to pid {pid} and accepts one connection: `simpool debug --gdb` gives you a local port for `process connect`")
    };
    let info = json!({"id": id, "protocol": r.protocol, "pid": pid, "bundle_id": r.bundle_id, "ws_path": path, "hint": hint});
    lease.track(format!("dbg-{id}"), Tracked { kind: "debug", child, info: json!({"port": port, "session": info.clone()}) });
    Ok((StatusCode::CREATED, Json(info)))
}

async fn list_debug(L(lease): L) -> Json<Value> {
    lease.prune();
    let list: Vec<Value> = lease.children.lock().unwrap().values().filter(|t| t.kind == "debug").map(|t| t.info["session"].clone()).collect();
    Json(json!({ "sessions": list }))
}

async fn stop_debug(L(lease): L, Path(p): Path<HashMap<String, String>>) -> ApiResult<StatusCode> {
    let t = lease.children.lock().unwrap().remove(&format!("dbg-{}", p["session"]));
    let mut t = t.ok_or_else(|| ApiError::not_found("no such debug session"))?;
    lease.debug_streams.lock().unwrap().remove(&p["session"]);
    let _ = t.child.kill().await;
    Ok(StatusCode::NO_CONTENT)
}

async fn connect_debug(L(lease): L, Path(p): Path<HashMap<String, String>>, ws: WebSocketUpgrade) -> ApiResult<Response> {
    let sid = &p["session"];
    let port = lease.children.lock().unwrap().get(&format!("dbg-{sid}")).map(|t| (t.info["session"]["protocol"] == "gdb-remote", t.info["port"].as_u64()));
    let tcp = match port {
        None => return Err(ApiError::not_found("no such debug session")),
        Some((true, _)) => lease.debug_streams.lock().unwrap().remove(sid).ok_or_else(|| ApiError::conflict("this debugserver session already had its one connection; start a new session"))?,
        Some((false, port)) => TcpStream::connect(("127.0.0.1", port.unwrap_or(0) as u16)).await.map_err(|e| ApiError::unavailable(format!("lldb-dap isn't accepting connections: {e}")))?,
    };
    let cancel = lease.hold();
    Ok(ws.on_upgrade(move |socket| pipe(socket, tcp, cancel)))
}

#[derive(Deserialize)]
struct PortQuery {
    port: u16,
}

/// WebSocket ↔ 127.0.0.1:port on this Mac. Simulator apps share the Mac's network, so this
/// reaches servers that apps (or test harnesses) listen on.
async fn tunnel(State(pool): State<AppState>, L(lease): L, Query(q): Query<PortQuery>, ws: WebSocketUpgrade) -> ApiResult<Response> {
    if !pool.cfg.allow_tunnels {
        return Err(ApiError::forbidden("tunnels are disabled on this server (allow_tunnels = false)"));
    }
    let tcp = TcpStream::connect(("127.0.0.1", q.port)).await.map_err(|e| ApiError::unavailable(format!("nothing accepting on port {}: {e}", q.port)))?;
    let cancel = lease.hold();
    Ok(ws.on_upgrade(move |socket| pipe(socket, tcp, cancel)))
}

/// Copies bytes both ways until either side closes.
async fn pipe(mut ws: WebSocket, tcp: TcpStream, cancel: Hold) {
    let (mut rd, mut wr) = tcp.into_split();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            n = rd.read(&mut buf) => match n {
                Ok(0) | Err(_) => break,
                Ok(n) => if ws.send(Message::Binary(Bytes::copy_from_slice(&buf[..n]))).await.is_err() { break },
            },
            m = ws.recv() => match m {
                Some(Ok(Message::Binary(b))) => if wr.write_all(&b).await.is_err() { break },
                Some(Ok(Message::Text(t))) => if wr.write_all(t.as_bytes()).await.is_err() { break },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
        }
    }
    let _ = wr.shutdown().await;
    let _ = ws.send(Message::Close(None)).await;
}

// Reverse tunnel framing, both directions: [conn id: u32 BE][kind: u8][payload]
const OPEN: u8 = 0;
const DATA: u8 = 1;
const CLOSE: u8 = 2;

fn frame(conn: u32, kind: u8, payload: &[u8]) -> Message {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.extend_from_slice(&conn.to_be_bytes());
    v.push(kind);
    v.extend_from_slice(payload);
    Message::Binary(v.into())
}

/// Listens on 127.0.0.1:port on this Mac and carries each connection to the client over one
/// WebSocket, so an app in the simulator can reach e.g. a dev backend on the client's machine.
async fn reverse(State(pool): State<AppState>, L(lease): L, Query(q): Query<PortQuery>, ws: WebSocketUpgrade) -> ApiResult<Response> {
    if !pool.cfg.allow_tunnels {
        return Err(ApiError::forbidden("tunnels are disabled on this server (allow_tunnels = false)"));
    }
    let listener = TcpListener::bind(("127.0.0.1", q.port)).await.map_err(|e| ApiError::conflict(format!("can't listen on port {} on the simulator's Mac: {e}", q.port)))?;
    let cancel = lease.hold();
    Ok(ws.on_upgrade(move |socket| reverse_session(socket, listener, cancel)))
}

async fn reverse_session(mut ws: WebSocket, listener: TcpListener, cancel: Hold) {
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
    let mut conns: HashMap<u32, mpsc::Sender<Bytes>> = HashMap::new();
    let mut next = 1u32;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            acc = listener.accept() => {
                let Ok((tcp, _)) = acc else { continue };
                let id = next;
                next = next.wrapping_add(1);
                let (mut rd, mut wr) = tcp.into_split();
                let (in_tx, mut in_rx) = mpsc::channel::<Bytes>(64);
                conns.insert(id, in_tx);
                if out_tx.send(frame(id, OPEN, &[])).await.is_err() { break }
                let tx = out_tx.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 32 << 10];
                    loop {
                        match rd.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => if tx.send(frame(id, DATA, &buf[..n])).await.is_err() { return },
                        }
                    }
                    let _ = tx.send(frame(id, CLOSE, &[])).await;
                });
                tokio::spawn(async move {
                    while let Some(b) = in_rx.recv().await {
                        if wr.write_all(&b).await.is_err() { break }
                    }
                    let _ = wr.shutdown().await;
                });
            }
            m = out_rx.recv() => {
                let Some(m) = m else { break };
                if ws.send(m).await.is_err() { break }
            }
            m = ws.recv() => match m {
                Some(Ok(Message::Binary(b))) if b.len() >= 5 => {
                    let id = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                    match b[4] {
                        DATA => if let Some(tx) = conns.get(&id) {
                            if tx.send(b.slice(5..)).await.is_err() { conns.remove(&id); }
                        },
                        CLOSE => { conns.remove(&id); }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
        }
    }
    let _ = ws.send(Message::Close(None)).await;
}
