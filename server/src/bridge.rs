//! Client for simbridge (bridge/simbridge.m): one long-lived process per simulator for
//! input, accessibility and framebuffer capture.

use crate::error::{ApiError, ApiResult};
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, oneshot};

static EXE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/simbridge"));

/// Writes the embedded simbridge binary under `dir` (once per build) and returns its path.
pub fn install(dir: &Path) -> Result<PathBuf> {
    let hash = EXE.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3));
    let path = dir.join(format!("simbridge-{hash:016x}"));
    if !path.exists() {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".simbridge-{}", std::process::id()));
        std::fs::write(&tmp, EXE)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

pub struct Bridge {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    alive: AtomicBool,
    child: Mutex<Option<Child>>,
    frames: broadcast::Sender<Bytes>,
    viewers: Mutex<Viewers>,
    /// Serializes stream_start/stream_stop decisions so a leaving and a joining viewer can't cross.
    stream_ctl: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Viewers {
    count: usize,
    streaming: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StreamSettings {
    pub fps: f64,
    pub quality: f64,
    pub scale: f64,
}

impl Bridge {
    pub async fn start(exe: &Path, developer_dir: &str, udid: &str) -> Result<Arc<Bridge>> {
        let mut child = Command::new(exe)
            .args([developer_dir, udid])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting simbridge")?;
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (frames, _) = broadcast::channel(4);
        let bridge = Arc::new(Bridge {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            child: Mutex::new(Some(child)),
            frames,
            viewers: Mutex::new(Viewers::default()),
            stream_ctl: tokio::sync::Mutex::new(()),
        });

        let (ready_tx, ready_rx) = oneshot::channel();
        bridge.pending.lock().unwrap().insert(0, ready_tx);
        let udid_log = udid.to_string();
        tokio::spawn(async move {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf).await;
            if !buf.trim().is_empty() {
                tracing::warn!(udid = %udid_log, "simbridge: {}", buf.trim());
            }
        });
        let weak = Arc::downgrade(&bridge);
        tokio::spawn(async move {
            let res: Result<()> = async {
                loop {
                    let mut hdr = [0u8; 5];
                    stdout.read_exact(&mut hdr).await?;
                    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
                    let mut payload = vec![0u8; len];
                    stdout.read_exact(&mut payload).await?;
                    let Some(b) = weak.upgrade() else { return Ok(()) };
                    match hdr[0] {
                        b'R' => {
                            let v: Value = serde_json::from_slice(&payload)?;
                            let id = v["id"].as_u64().unwrap_or(u64::MAX);
                            if let Some(tx) = b.pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(v);
                            }
                        }
                        b'F' => {
                            let _ = b.frames.send(Bytes::from(payload));
                        }
                        k => bail!("unknown frame kind {k}"),
                    }
                }
            }
            .await;
            if let Some(b) = weak.upgrade() {
                b.alive.store(false, Ordering::SeqCst);
                b.pending.lock().unwrap().clear(); // wakes every waiter with an error
            }
            if let Err(e) = res {
                tracing::debug!("simbridge reader ended: {e:#}");
            }
        });

        let ready = tokio::time::timeout(Duration::from_secs(30), ready_rx)
            .await
            .map_err(|_| anyhow!("simbridge didn't start within 30s"))?
            .map_err(|_| anyhow!("simbridge exited at startup (see server log)"))?;
        if ready["ok"] != json!(true) {
            bail!("simbridge: {}", ready["error"]);
        }
        Ok(bridge)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Sends one command and waits for its reply. `cmd` must be an object with "op".
    pub async fn call(&self, mut cmd: Value, timeout: Duration) -> ApiResult<Value> {
        if !self.is_alive() {
            return Err(ApiError::unavailable("simbridge isn't running"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        cmd["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_vec(&cmd).unwrap();
        line.push(b'\n');
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(e) = stdin.write_all(&line).await {
                self.pending.lock().unwrap().remove(&id);
                return Err(ApiError::unavailable(format!("simbridge: {e}")));
            }
        }
        let reply = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => return Err(ApiError::unavailable("simbridge exited")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                return Err(ApiError::timeout(format!("simbridge {} timed out after {}s", cmd["op"], timeout.as_secs())));
            }
        };
        if reply["ok"] == json!(true) {
            Ok(reply)
        } else {
            Err(ApiError::bad_request(reply["error"].as_str().unwrap_or("simbridge error").to_string()))
        }
    }

    /// Subscribes to JPEG frames. The stream runs while at least one subscription lives;
    /// the first subscriber's settings hold until everyone leaves.
    pub async fn subscribe(self: &Arc<Self>, want: StreamSettings) -> ApiResult<FrameSub> {
        let _ctl = self.stream_ctl.lock().await;
        self.viewers.lock().unwrap().count += 1;
        let sub = FrameSub { rx: self.frames.subscribe(), bridge: self.clone() };
        if !self.viewers.lock().unwrap().streaming {
            self.call(json!({"op": "stream_start", "fps": want.fps, "quality": want.quality, "scale": want.scale}), Duration::from_secs(10)).await?;
            self.viewers.lock().unwrap().streaming = true;
        }
        Ok(sub)
    }

    pub fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.start_kill();
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct FrameSub {
    pub rx: broadcast::Receiver<Bytes>,
    bridge: Arc<Bridge>,
}

impl FrameSub {
    /// Next frame; skips frames this viewer was too slow to take. None once the bridge is gone.
    pub async fn next(&mut self) -> Option<Bytes> {
        loop {
            match self.rx.recv().await {
                Ok(f) => return Some(f),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

impl Drop for FrameSub {
    fn drop(&mut self) {
        let last = {
            let mut v = self.bridge.viewers.lock().unwrap();
            v.count -= 1;
            v.count == 0
        };
        if last && self.bridge.is_alive() {
            let b = self.bridge.clone();
            tokio::spawn(async move {
                let _ctl = b.stream_ctl.lock().await;
                let stop = {
                    let mut v = b.viewers.lock().unwrap();
                    let stop = v.count == 0 && v.streaming;
                    if stop {
                        v.streaming = false;
                    }
                    stop
                };
                if stop {
                    let _ = b.call(json!({"op": "stream_stop"}), Duration::from_secs(10)).await;
                }
            });
        }
    }
}
