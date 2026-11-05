//! Leases: who holds which simulator, for how long, and the queue for capacity.

use crate::bridge::Bridge;
use crate::config::Config;
use crate::error::{ApiError, ApiResult};
use crate::fsutil::remove_tree;
use crate::proc;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Child;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// The longest idle timeout a client can ask for. Beyond some size, adding it to a clock panics.
const MAX_TTL: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

pub struct Pool {
    pub cfg: Config,
    pub developer_dir: String,
    pub bridge_exe: PathBuf,
    global: Arc<Semaphore>,
    profiles: HashMap<String, Arc<Semaphore>>,
    leases: Mutex<HashMap<String, Arc<Lease>>>,
    waiting: AtomicUsize,
    provisioning: AtomicUsize,
}

/// One simulator handed to one client.
pub struct Lease {
    pub id: String,
    pub udid: String,
    pub profile: Option<String>,
    pub label: Option<String>,
    pub created: SystemTime,
    started: Instant,
    idle: Duration,
    /// From lease_max_secs; None when the server sets no cap.
    hard_deadline: Option<Instant>,
    deadline: Mutex<Instant>,
    /// Scratch space for uploads, recordings and captured app output; removed on release.
    pub dir: PathBuf,
    /// Cancelled on release; long-running streams watch it.
    pub cancel: CancellationToken,
    bridge: tokio::sync::Mutex<Option<Arc<Bridge>>>,
    /// Processes that live as long as the lease (debugservers, recorders); killed on release.
    pub children: Mutex<HashMap<String, Tracked>>,
    /// debugserver connections waiting for their client, by session id.
    pub debug_streams: Mutex<HashMap<String, tokio::net::TcpStream>>,
    /// Open streams (live view, logs, tunnels…); while any are open the lease isn't idle.
    streams: AtomicUsize,
    _permits: Vec<OwnedSemaphorePermit>,
}

pub struct Hold {
    lease: Arc<Lease>,
}

impl Hold {
    /// Resolves when the lease is released.
    pub fn cancelled(&self) -> tokio_util::sync::WaitForCancellationFuture<'_> {
        self.lease.cancel.cancelled()
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.lease.streams.fetch_sub(1, Ordering::SeqCst);
        self.lease.touch(None);
    }
}

pub struct Tracked {
    pub kind: &'static str,
    pub child: Child,
    pub info: Value,
}

#[derive(Deserialize, Default)]
pub struct AcquireRequest {
    /// A profile from simpool.toml.
    pub profile: Option<String>,
    /// Or a whole simfast config (if the server allows it).
    pub simfast_config: Option<String>,
    /// Idle timeout for this lease, instead of the server's lease_idle_secs. lease_max_secs still applies.
    pub ttl_secs: Option<u64>,
    /// How long to wait in the queue for capacity. 0 = fail immediately when full.
    pub wait_secs: Option<u64>,
    /// Free text shown in listings, e.g. a CI job URL.
    pub label: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    id: String,
    udid: String,
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Pool {
    pub fn new(cfg: Config, developer_dir: String, bridge_exe: PathBuf) -> Arc<Pool> {
        let profiles = cfg.profiles.iter().map(|(n, p)| (n.clone(), Arc::new(Semaphore::new(p.max.unwrap_or(cfg.max_devices))))).collect();
        Arc::new(Pool {
            global: Arc::new(Semaphore::new(cfg.max_devices)),
            profiles,
            cfg,
            developer_dir,
            bridge_exe,
            leases: Mutex::new(HashMap::new()),
            waiting: AtomicUsize::new(0),
            provisioning: AtomicUsize::new(0),
        })
    }

    fn state_file(&self) -> PathBuf {
        self.cfg.data_dir().join("leases.json")
    }

    /// App files kept between installs, shared by every lease. See api/bundles.rs.
    pub fn blob_store(&self) -> PathBuf {
        self.cfg.data_dir().join("blobs")
    }

    fn persist(&self) {
        let list: Vec<Persisted> = self.leases.lock().unwrap().values().map(|l| Persisted { id: l.id.clone(), udid: l.udid.clone() }).collect();
        let tmp = self.state_file().with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, serde_json::to_vec_pretty(&list).unwrap()).and_then(|_| std::fs::rename(&tmp, self.state_file())) {
            tracing::warn!("saving lease state: {e}");
        }
    }

    /// Stops simulators left behind by a previous run that didn't shut down cleanly.
    pub async fn recover(&self) {
        let Ok(text) = std::fs::read(self.state_file()) else { return };
        let old: Vec<Persisted> = serde_json::from_slice(&text).unwrap_or_default();
        for p in old {
            tracing::info!(lease = %p.id, udid = %p.udid, "stopping simulator left over from the last run");
            let _ = self.simfast_stop(&p.udid).await;
            let _ = remove_tree(&self.cfg.data_dir().join("leases").join(&p.id));
        }
        self.persist();
    }

    pub async fn acquire(self: &Arc<Self>, mut req: AcquireRequest) -> ApiResult<Arc<Lease>> {
        if req.profile.is_none() && req.simfast_config.is_none() && self.cfg.profiles.len() == 1 {
            req.profile = self.cfg.profiles.keys().next().cloned();
        }
        let (config_text, profile_sem) = match (&req.profile, &req.simfast_config) {
            (Some(name), None) => {
                let p = self.cfg.profiles.get(name).ok_or_else(|| ApiError::not_found(format!("no profile {name:?} (have: {})", self.profile_names())))?;
                (p.simfast_toml.clone(), Some(self.profiles[name].clone()))
            }
            (None, Some(text)) => {
                if !self.cfg.allow_client_configs {
                    return Err(ApiError::forbidden("this server only hands out its profiles; pass \"profile\""));
                }
                toml::from_str::<toml::Value>(text).map_err(|e| ApiError::bad_request(format!("simfast_config isn't valid TOML: {e}")))?;
                (text.clone(), None)
            }
            _ => return Err(ApiError::bad_request(format!("pass exactly one of \"profile\" ({}) or \"simfast_config\"", self.profile_names()))),
        };

        let wait = Duration::from_secs(req.wait_secs.unwrap_or(self.cfg.acquire_wait_secs));
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let permits = tokio::time::timeout(wait, async {
            let mut permits = Vec::new();
            // Profile first, so a request queued for a full profile doesn't hold global capacity.
            if let Some(s) = profile_sem {
                permits.push(s.acquire_owned().await.unwrap());
            }
            permits.push(self.global.clone().acquire_owned().await.unwrap());
            permits
        })
        .await;
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        let permits = permits.map_err(|_| ApiError::unavailable(format!("no simulator free within {}s; try again or pass a longer wait_secs", wait.as_secs())))?;

        self.provisioning.fetch_add(1, Ordering::SeqCst);
        let started = Instant::now();
        let udid = self.simfast_run(&config_text).await;
        self.provisioning.fetch_sub(1, Ordering::SeqCst);
        let udid = udid?;

        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = self.cfg.data_dir().join("leases").join(&id);
        std::fs::create_dir_all(&dir)?;
        let idle = Duration::from_secs(req.ttl_secs.unwrap_or(self.cfg.lease_idle_secs)).clamp(Duration::from_secs(30), MAX_TTL);
        let now = Instant::now();
        let hard_deadline = match self.cfg.lease_max_secs {
            0 => None,
            secs => now.checked_add(Duration::from_secs(secs)),
        };
        let lease = Arc::new(Lease {
            id: id.clone(),
            udid,
            profile: req.profile,
            label: req.label,
            created: SystemTime::now(),
            started: now,
            idle,
            hard_deadline,
            deadline: Mutex::new(hard_deadline.map_or(now + idle, |h| h.min(now + idle))),
            dir,
            cancel: CancellationToken::new(),
            bridge: tokio::sync::Mutex::new(None),
            children: Mutex::new(HashMap::new()),
            debug_streams: Mutex::new(HashMap::new()),
            streams: AtomicUsize::new(0),
            _permits: permits,
        });
        self.leases.lock().unwrap().insert(id, lease.clone());
        self.persist();
        tracing::info!(lease = %lease.id, udid = %lease.udid, profile = ?lease.profile, "leased in {:.1}s", started.elapsed().as_secs_f64());
        Ok(lease)
    }

    fn profile_names(&self) -> String {
        self.cfg.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
    }

    async fn simfast_run(&self, config: &str) -> ApiResult<String> {
        let out = proc::run(&self.cfg.simfast, ["run", "-"], Some(config.as_bytes()), &[], Duration::from_secs(600))
            .await
            .map_err(|e| ApiError(axum::http::StatusCode::BAD_GATEWAY, format!("simfast couldn't provide a simulator: {e:#}")))?;
        let udid = out.text().lines().last().unwrap_or("").trim().to_string();
        if uuid::Uuid::parse_str(&udid).is_err() {
            return Err(ApiError(axum::http::StatusCode::BAD_GATEWAY, format!("simfast printed {udid:?}, not a UDID; stderr: {}", out.stderr)));
        }
        Ok(udid)
    }

    async fn simfast_stop(&self, udid: &str) -> Result<()> {
        proc::run(&self.cfg.simfast, ["stop", udid], None, &[], Duration::from_secs(120)).await?;
        Ok(())
    }

    /// Looks up a lease and counts the lookup as activity.
    pub fn get(&self, id: &str) -> ApiResult<Arc<Lease>> {
        let l = self.leases.lock().unwrap().get(id).cloned().ok_or_else(|| ApiError::not_found(format!("no lease {id} (released or expired)")))?;
        l.touch(None);
        Ok(l)
    }

    pub fn list(&self) -> Vec<Arc<Lease>> {
        let mut v: Vec<_> = self.leases.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|l| l.created);
        v
    }

    pub async fn release(&self, id: &str, why: &str) -> ApiResult<()> {
        let lease = self.leases.lock().unwrap().remove(id).ok_or_else(|| ApiError::not_found(format!("no lease {id}")))?;
        self.persist();
        lease.cancel.cancel();
        if let Some(b) = lease.bridge.lock().await.take() {
            b.kill();
        }
        // Recorders get SIGINT first so they can finish their file; everything else is killed.
        let children: Vec<Tracked> = lease.children.lock().unwrap().drain().map(|(_, t)| t).collect();
        for mut t in children {
            if t.kind == "recording" {
                if let Some(pid) = t.child.id() {
                    unsafe { libc::kill(pid as i32, libc::SIGINT) };
                }
                let _ = tokio::time::timeout(Duration::from_secs(5), t.child.wait()).await;
            }
            let _ = t.child.kill().await;
        }
        if let Err(e) = self.simfast_stop(&lease.udid).await {
            tracing::warn!(lease = %id, "simfast stop {}: {e:#}", lease.udid);
        }
        if let Err(e) = remove_tree(&lease.dir) {
            tracing::warn!(lease = %id, "removing {}: {e}", lease.dir.display());
        }
        tracing::info!(lease = %id, udid = %lease.udid, "released ({why}) after {}s", lease.started.elapsed().as_secs());
        Ok(())
        // The permits drop with the last Arc<Lease>, which frees the slot.
    }

    pub async fn release_all(&self) {
        let ids: Vec<String> = self.leases.lock().unwrap().keys().cloned().collect();
        for id in ids {
            let _ = self.release(&id, "server shutting down").await;
        }
    }

    /// Expires idle and over-age leases every few seconds.
    pub async fn reap_forever(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let now = Instant::now();
            let expired: Vec<(String, &'static str)> = self
                .leases
                .lock()
                .unwrap()
                .values()
                .filter_map(|l| {
                    if l.hard_deadline.is_some_and(|h| now >= h) {
                        Some((l.id.clone(), "reached lease_max_secs"))
                    } else if now >= *l.deadline.lock().unwrap() && l.streams.load(Ordering::SeqCst) == 0 {
                        Some((l.id.clone(), "idle timeout"))
                    } else {
                        None
                    }
                })
                .collect();
            for (id, why) in expired {
                let _ = self.release(&id, why).await;
            }
        }
    }

    pub fn status(&self) -> Value {
        let leases = self.leases.lock().unwrap();
        let per_profile: serde_json::Map<String, Value> = self
            .cfg
            .profiles
            .iter()
            .map(|(name, p)| {
                let used = leases.values().filter(|l| l.profile.as_deref() == Some(name)).count();
                (name.clone(), json!({"max": p.max.unwrap_or(self.cfg.max_devices), "leased": used, "description": p.description}))
            })
            .collect();
        json!({
            "listen": self.cfg.listen,
            "max_devices": self.cfg.max_devices,
            "leased": leases.len(),
            "available": self.global.available_permits(),
            "waiting": self.waiting.load(Ordering::SeqCst),
            "provisioning": self.provisioning.load(Ordering::SeqCst),
            "profiles": per_profile,
            "allow_client_configs": self.cfg.allow_client_configs,
            "developer_dir": self.developer_dir,
        })
    }
}

impl Lease {
    /// Pushes the idle deadline out (by `ttl` or the lease's own idle timeout).
    pub fn touch(&self, ttl: Option<Duration>) {
        let mut next = Instant::now() + ttl.unwrap_or(self.idle).min(MAX_TTL);
        if let Some(hard) = self.hard_deadline {
            next = next.min(hard);
        }
        let mut d = self.deadline.lock().unwrap();
        if next > *d {
            *d = next;
        }
    }

    pub fn json(&self) -> Value {
        let now = Instant::now();
        let left = self.deadline.lock().unwrap().saturating_duration_since(now);
        json!({
            "id": self.id,
            "udid": self.udid,
            "profile": self.profile,
            "label": self.label,
            "created_at": unix(self.created),
            "expires_at": unix(SystemTime::now() + left),
            "idle_timeout_secs": self.idle.as_secs(),
            "max_expires_at": self.hard_deadline.map(|h| unix(SystemTime::now() + h.saturating_duration_since(now))),
        })
    }

    /// The simbridge for this simulator, started on first use and restarted if it died.
    pub async fn bridge(&self, pool: &Pool) -> ApiResult<Arc<Bridge>> {
        let mut slot = self.bridge.lock().await;
        if let Some(b) = slot.as_ref().filter(|b| b.is_alive()) {
            return Ok(b.clone());
        }
        let b = Bridge::start(&pool.bridge_exe, &pool.developer_dir, &self.udid).await?;
        *slot = Some(b.clone());
        Ok(b)
    }

    /// Marks a long-lived stream as open, which keeps the lease from idling out until it ends.
    pub fn hold(self: &Arc<Self>) -> Hold {
        self.streams.fetch_add(1, Ordering::SeqCst);
        Hold { lease: self.clone() }
    }

    pub fn track(&self, id: String, t: Tracked) {
        self.children.lock().unwrap().insert(id, t);
    }

    /// Forgets debuggers and recorders that have exited on their own.
    pub fn prune(&self) {
        let mut gone = Vec::new();
        self.children.lock().unwrap().retain(|id, t| {
            let alive = matches!(t.child.try_wait(), Ok(None));
            if !alive {
                gone.push(id.trim_start_matches("dbg-").to_string());
            }
            alive
        });
        let mut streams = self.debug_streams.lock().unwrap();
        for id in gone {
            streams.remove(&id);
        }
    }
}
