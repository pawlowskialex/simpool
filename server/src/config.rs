use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// simpool.toml
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Also serve on this Unix socket (mode 0600), e.g. to forward it with `ssh -R`.
    pub listen_unix: Option<PathBuf>,
    /// Bearer tokens accepted by the API. Empty means no auth (only sensible on localhost).
    #[serde(default)]
    pub tokens: Vec<String>,
    /// Simulators this Mac runs at once, across all profiles.
    #[serde(default = "default_max_devices")]
    pub max_devices: usize,
    /// A lease expires after this long without any request or renew.
    #[serde(default = "default_idle_ttl")]
    pub lease_idle_secs: u64,
    /// Hard cap on a lease's lifetime, renewals included. 0 means no cap.
    #[serde(default = "default_max_lease")]
    pub lease_max_secs: u64,
    /// Disk for app files kept between installs. Least recently used files go first.
    #[serde(default = "default_blob_cache_gb")]
    pub blob_cache_gb: u64,
    /// How long an acquire waits in the queue by default.
    #[serde(default = "default_wait")]
    pub acquire_wait_secs: u64,
    /// Let clients send their own simfast config instead of naming a profile.
    #[serde(default = "yes")]
    pub allow_client_configs: bool,
    /// Allow /spawn (run any binary inside the simulator, which runs as this user on this Mac).
    #[serde(default)]
    pub allow_spawn: bool,
    /// Allow /tunnel and /reverse to arbitrary loopback ports on this Mac.
    #[serde(default)]
    pub allow_tunnels: bool,
    /// simfast binary.
    #[serde(default = "default_simfast")]
    pub simfast: PathBuf,
    /// Where uploads, recordings and state go. Default ~/Library/Application Support/simpool.
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Path to a simfast config, relative to simpool.toml.
    pub config: Option<PathBuf>,
    /// Or the simfast config itself.
    pub inline: Option<String>,
    /// Most simulators of this profile at once (default: max_devices).
    pub max: Option<usize>,
    #[serde(default)]
    pub description: String,
    /// Resolved simfast config text.
    #[serde(skip)]
    pub simfast_toml: String,
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:7700".parse().unwrap()
}
fn default_max_devices() -> usize {
    4
}
fn default_idle_ttl() -> u64 {
    600
}
fn default_max_lease() -> u64 {
    24 * 3600
}
fn default_blob_cache_gb() -> u64 {
    20
}
fn default_wait() -> u64 {
    120
}
fn default_simfast() -> PathBuf {
    "simfast".into()
}
fn yes() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        cfg.listen_unix = cfg.listen_unix.map(|p| base.join(p));
        for (name, p) in &mut cfg.profiles {
            p.simfast_toml = match (&p.config, &p.inline) {
                (Some(f), None) => {
                    let f = base.join(f);
                    std::fs::read_to_string(&f).with_context(|| format!("profile {name}: reading {}", f.display()))?
                }
                (None, Some(s)) => s.clone(),
                _ => bail!("profile {name}: set exactly one of `config` or `inline`"),
            };
            toml::from_str::<toml::Value>(&p.simfast_toml).with_context(|| format!("profile {name}: simfast config isn't valid TOML"))?;
        }
        if cfg.max_devices == 0 {
            bail!("max_devices must be at least 1");
        }
        Ok(cfg)
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join("Library/Application Support/simpool")
        })
    }
}
