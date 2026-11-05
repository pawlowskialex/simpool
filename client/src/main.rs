mod bundle;
mod tunnel;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use futures::StreamExt;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Client for simpoold: lease remote iOS simulators and drive them.
///
/// Set SIMPOOL_URL and SIMPOOL_TOKEN, then `export SIMPOOL_LEASE=$(simpool lease)`.
#[derive(Parser)]
#[command(version, max_term_width = 100)]
struct Cli {
    /// http(s)://host:port, or unix:///path/to.sock (e.g. one forwarded with `ssh -R`)
    #[arg(long, env = "SIMPOOL_URL", default_value = "http://127.0.0.1:7700", global = true)]
    server: String,
    #[arg(long, env = "SIMPOOL_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    /// Lease to act on
    #[arg(short, long, env = "SIMPOOL_LEASE", global = true)]
    lease: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pool capacity, profiles and queue
    Status,
    /// Lease a simulator; prints only the lease id on stdout
    Lease {
        #[arg(short, long)]
        profile: Option<String>,
        /// Send this simfast config instead of naming a profile ("-" for stdin)
        #[arg(short = 'c', long)]
        config: Option<String>,
        /// Idle timeout in seconds
        #[arg(long)]
        ttl: Option<u64>,
        /// Seconds to wait in the queue when the pool is full
        #[arg(long)]
        wait: Option<u64>,
        #[arg(long)]
        label: Option<String>,
        /// Print the whole lease as JSON
        #[arg(long)]
        json: bool,
    },
    /// List leases on the server
    Leases,
    /// Lease and device details
    Info,
    /// Give the simulator back
    Release,
    /// Keep the lease alive
    Renew {
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Renew the lease until it's released or this is stopped; rides out network drops
    Keepalive,
    /// Install a .app (directory), .ipa, zip or tarball. A directory sends only what changed.
    Install {
        path: PathBuf,
        /// A dSYM to send for debugging, repeatable. Those next to the app are found on their own.
        #[arg(long)]
        dsym: Vec<PathBuf>,
        /// Don't look for dSYMs next to the app
        #[arg(long)]
        no_dsym: bool,
    },
    Uninstall { bundle_id: String },
    /// Installed apps
    Apps {
        /// Only apps you installed
        #[arg(long)]
        user: bool,
    },
    /// Launch an app; prints its pid
    Launch {
        bundle_id: String,
        #[arg(long)]
        wait_for_debugger: bool,
        /// KEY=VALUE, repeatable
        #[arg(short, long)]
        env: Vec<String>,
        #[arg(last = true)]
        args: Vec<String>,
    },
    Terminate { bundle_id: String },
    /// The app's stdout/stderr since it was launched through simpool
    Output {
        bundle_id: String,
        #[arg(short, long)]
        follow: bool,
    },
    /// Tap a point (in points) or an element
    Tap {
        x: Option<f64>,
        y: Option<f64>,
        #[command(flatten)]
        el: Element,
        /// Hold for this many seconds (long press)
        #[arg(long)]
        hold: Option<f64>,
        #[arg(long)]
        double: bool,
    },
    Swipe {
        x: f64,
        y: f64,
        to_x: f64,
        to_y: f64,
        #[arg(long, default_value_t = 0.3)]
        duration: f64,
    },
    /// Scroll content: up, down, left or right
    /// Scroll the content in a direction. Momentum-free (stops where it lands) unless --momentum.
    Scroll {
        /// up, down, left or right
        direction: String,
        /// Points to move the content (default ~40% of the screen)
        #[arg(long)]
        distance: Option<f64>,
        /// Gesture duration in seconds
        #[arg(long)]
        duration: Option<f64>,
        /// Inertial flick that carries on past the drag, instead of stopping
        #[arg(long)]
        momentum: bool,
    },
    /// Type text (non-ASCII goes through the pasteboard)
    Type { text: String },
    /// Press a key: enter, escape, tab, backspace, up, a, 4 … with optional modifiers
    Key {
        key: String,
        /// cmd, ctrl, alt, shift (comma separated)
        #[arg(short, long, value_delimiter = ',')]
        modifiers: Vec<String>,
    },
    /// Hardware button: home, lock, side, siri, apple_pay
    Button { name: String },
    /// Run raw input actions: a JSON array, or "-" to read it from stdin
    Input { actions: String },
    /// Accessibility tree of the frontmost app
    Ui {
        /// text, tree or flat
        #[arg(short, long, default_value = "text")]
        format: String,
    },
    /// Find elements
    Find {
        #[command(flatten)]
        el: Element,
    },
    /// Wait for an element to appear (or disappear)
    Wait {
        #[command(flatten)]
        el: Element,
        #[arg(long, default_value_t = 10.0)]
        timeout: f64,
        #[arg(long)]
        gone: bool,
    },
    /// Scroll until an element is on screen
    ScrollTo {
        #[command(flatten)]
        el: Element,
        #[arg(long, default_value = "down")]
        direction: String,
    },
    /// Save a screenshot (PNG, or JPEG if the name ends in .jpg)
    Screenshot {
        #[arg(default_value = "screenshot.png")]
        out: PathBuf,
    },
    /// Screen recording: `record start`, then `record stop ID [OUT]`
    Record {
        #[command(subcommand)]
        cmd: RecordCmd,
    },
    OpenUrl { url: String },
    /// Simulated location
    Location {
        latitude: Option<f64>,
        longitude: Option<f64>,
        #[arg(long)]
        clear: bool,
    },
    /// light or dark
    Appearance { mode: String },
    /// Send a push notification: payload is JSON or @file
    Push { bundle_id: String, payload: String },
    /// grant|revoke|reset SERVICE [BUNDLE]
    Privacy {
        action: String,
        service: String,
        bundle_id: Option<String>,
    },
    /// Print the pasteboard, or set it
    Pasteboard {
        #[arg(long)]
        set: Option<String>,
    },
    /// Add photos, videos or contacts
    Media { files: Vec<PathBuf> },
    /// Face ID / Touch ID: enroll, unenroll, match, fail
    Biometrics {
        action: String,
        #[arg(long, default_value = "face")]
        kind: String,
    },
    /// Stream logs (NDJSON)
    Logs {
        #[arg(long)]
        predicate: Option<String>,
        #[arg(long)]
        process: Option<String>,
        #[arg(long)]
        level: Option<String>,
        /// Past logs instead of streaming, e.g. 5m
        #[arg(long)]
        last: Option<String>,
    },
    /// Crash reports since the lease began; give a name to print one
    Crashes { name: Option<String> },
    /// Files in app containers or the simulator's home
    Files {
        #[command(subcommand)]
        cmd: FilesCmd,
    },
    /// Run a command inside the simulator (if the server allows it)
    Spawn {
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Start a debug session and serve it on a local port for lldb or your editor
    Debug {
        /// App to debug (launched suspended unless --attach)
        bundle_id: Option<String>,
        #[arg(long)]
        pid: Option<u32>,
        /// Attach to the running app instead of relaunching it
        #[arg(long)]
        attach: bool,
        /// debugserver (gdb-remote) for a local lldb, instead of lldb-dap
        #[arg(long)]
        gdb: bool,
        /// Local port (default: any free port)
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Forward a local port to a port on the simulator's Mac (e.g. a server in the app)
    Forward { local_port: u16, remote_port: u16 },
    /// Make a port on the simulator's Mac reach a local address (e.g. your dev backend)
    Reverse {
        remote_port: u16,
        /// host:port to connect to locally
        local: String,
    },
    /// Print the live view's URL, and open it when this is a Mac you're sitting at
    View,
    /// Call any API path: `api GET /ui` (lease-relative) or `api GET /v1/pool`
    Api {
        method: String,
        path: String,
        /// JSON body, or @file
        body: Option<String>,
    },
}

#[derive(Subcommand)]
enum RecordCmd {
    Start {
        #[arg(long)]
        hevc: bool,
    },
    Stop {
        id: String,
        #[arg(default_value = "recording.mp4")]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum FilesCmd {
    Ls {
        #[command(flatten)]
        loc: Loc,
        #[arg(default_value = "")]
        path: String,
    },
    Get {
        #[command(flatten)]
        loc: Loc,
        path: String,
        /// Output file ("-" for stdout); directories come as .tar.gz
        out: Option<PathBuf>,
    },
    Put {
        #[command(flatten)]
        loc: Loc,
        local: PathBuf,
        path: String,
    },
    Rm {
        #[command(flatten)]
        loc: Loc,
        path: String,
        #[arg(short, long)]
        recursive: bool,
    },
}

#[derive(Args)]
struct Loc {
    /// App whose container to use (omit for the simulator's home)
    #[arg(short, long)]
    bundle: Option<String>,
    /// data, app, groups or a group id
    #[arg(short, long, default_value = "data")]
    container: String,
}

#[derive(Args, Default)]
struct Element {
    #[arg(long)]
    id: Option<String>,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    label_contains: Option<String>,
    /// Substring of label, value, title, identifier or placeholder (case-insensitive)
    #[arg(long)]
    text: Option<String>,
    #[arg(long)]
    role: Option<String>,
    #[arg(long)]
    value: Option<String>,
    #[arg(long, default_value_t = 0)]
    index: usize,
}

impl Element {
    fn json(&self) -> Value {
        let mut v = json!({"index": self.index});
        for (k, f) in [("id", &self.id), ("label", &self.label), ("label_contains", &self.label_contains), ("text", &self.text), ("role", &self.role), ("value", &self.value)] {
            if let Some(f) = f {
                v[k] = json!(f);
            }
        }
        v
    }
    fn given(&self) -> bool {
        self.json().as_object().unwrap().len() > 1
    }
}

pub struct Client {
    http: reqwest::Client,
    /// What the user gave, for messages.
    server: String,
    /// Where URLs point. Over a Unix socket the host is only a formality.
    base: String,
    /// Connect through this socket instead of over TCP.
    pub unix: Option<PathBuf>,
    pub token: Option<String>,
    lease: Option<String>,
}

impl Client {
    fn new(server: String, token: Option<String>, lease: Option<String>) -> Result<Client> {
        let unix = server.strip_prefix("unix:").map(|p| PathBuf::from(p.trim_start_matches("//")));
        if let Some(p) = &unix {
            if !p.is_absolute() {
                bail!("{server}: a socket needs an absolute path, like unix:///tmp/simpool.sock");
            }
        }
        let base = if unix.is_some() { "http://localhost".to_string() } else { server.trim_end_matches('/').to_string() };
        Ok(Client { http: http_client(unix.as_deref())?, server, base, unix, token, lease })
    }

    fn lease_path(&self, p: &str) -> Result<String> {
        let id = self.lease.as_deref().ok_or_else(|| anyhow!("no lease: pass --lease or export SIMPOOL_LEASE=$(simpool lease)"))?;
        Ok(format!("/v1/leases/{id}{p}"))
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, self.url(path));
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    async fn send(&self, r: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = r.send().await.with_context(|| format!("can't reach {}", self.server))?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(text);
        bail!("{msg} ({status})")
    }

    async fn json(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut r = self.req(method, path);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = self.send(r).await?;
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        let bytes = resp.bytes().await?;
        Ok(serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())))
    }

    async fn on(&self, method: Method, p: &str, body: Option<Value>) -> Result<Value> {
        self.json(method, &self.lease_path(p)?, body).await
    }

    async fn upload(&self, p: &str, path: &Path) -> Result<Value> {
        let f = tokio::fs::File::open(path).await.with_context(|| format!("opening {}", path.display()))?;
        let len = f.metadata().await?.len();
        let body = reqwest::Body::wrap_stream(tokio_util_stream(f));
        let resp = self.send(self.req(Method::POST, &self.lease_path(p)?).header("content-length", len).body(body)).await?;
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        Ok(resp.json().await.unwrap_or(Value::Null))
    }

    /// Posts a manifest to `path`, sending whatever file contents the server asks for, until it
    /// has them all and acts on the manifest. Normally that's at most two rounds; more only if
    /// the server's store evicts files in between.
    async fn send_bundle(&self, path: &str, scan: &bundle::Scan) -> Result<Value> {
        for _ in 0..3 {
            let reply = self.on(Method::POST, path, Some(scan.manifest.clone())).await?;
            let Some(missing) = reply["missing"].as_array() else { return Ok(reply) };
            let mut items = Vec::with_capacity(missing.len());
            for hash in missing {
                let hash = hash.as_str().unwrap_or_default();
                let (file, size) = scan.blobs.get(hash).ok_or_else(|| anyhow!("the server asked for {hash}, which isn't in this bundle"))?;
                items.push((hash.to_string(), file.clone(), *size));
            }
            let sending: u64 = items.iter().map(|i| i.2).sum();
            let total: u64 = scan.blobs.values().map(|b| b.1).sum();
            eprintln!("simpool: sending {} of {} files ({} of {})", items.len(), scan.blobs.len(), size_text(sending), size_text(total));
            let body = reqwest::Body::wrap_stream(blob_frames(items));
            self.send(self.req(Method::POST, &self.lease_path("/blobs")?).body(body)).await?;
        }
        bail!("the server keeps losing uploaded files; is blob_cache_gb too small for this app?")
    }

    async fn download(&self, path: &str, out: &Path) -> Result<u64> {
        let resp = self.send(self.req(Method::GET, path)).await?;
        let mut stream = resp.bytes_stream();
        let mut n = 0;
        if out == Path::new("-") {
            let mut so = std::io::stdout().lock();
            while let Some(c) = stream.next().await {
                let c = c?;
                so.write_all(&c)?;
                n += c.len() as u64;
            }
            return Ok(n);
        }
        let mut f = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
        while let Some(c) = stream.next().await {
            let c = c?;
            f.write_all(&c)?;
            n += c.len() as u64;
        }
        Ok(n)
    }

    /// Copies a streaming response to stdout as it arrives.
    async fn stream_out(&self, path: &str) -> Result<()> {
        let resp = self.send(self.req(Method::GET, path)).await?;
        let mut stream = resp.bytes_stream();
        let mut so = std::io::stdout();
        while let Some(c) = stream.next().await {
            so.write_all(&c?)?;
            so.flush()?;
        }
        Ok(())
    }
}

fn tokio_util_stream(f: tokio::fs::File) -> impl futures::Stream<Item = std::io::Result<Vec<u8>>> {
    futures::stream::unfold(f, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 256 << 10];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(buf), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    })
}

/// What POST /blobs reads: for each file, its hash and length, then its contents.
fn blob_frames(items: Vec<(String, PathBuf, u64)>) -> impl futures::Stream<Item = std::io::Result<Vec<u8>>> {
    use tokio::io::AsyncReadExt;
    let current: Option<(tokio::fs::File, u64)> = None;
    futures::stream::unfold((items.into_iter(), current), |(mut items, mut current)| async move {
        loop {
            if let Some((file, left)) = current.as_mut() {
                if *left == 0 {
                    current = None;
                    continue;
                }
                let mut buf = vec![0u8; (*left).min(256 << 10) as usize];
                return match file.read(&mut buf).await {
                    Ok(0) => Some((Err(std::io::Error::other("a file got shorter during the upload")), (items, None))),
                    Ok(n) => {
                        buf.truncate(n);
                        *left -= n as u64;
                        Some((Ok(buf), (items, current)))
                    }
                    Err(e) => Some((Err(e), (items, None))),
                };
            }
            let (hash, path, size) = items.next()?;
            let file = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) => return Some((Err(std::io::Error::other(format!("opening {}: {e}", path.display()))), (items, None))),
            };
            let mut header = Vec::with_capacity(40);
            for i in (0..64).step_by(2) {
                header.push(u8::from_str_radix(&hash[i..i + 2], 16).unwrap_or(0));
            }
            header.extend_from_slice(&size.to_be_bytes());
            return Some((Ok(header), (items, Some((file, size)))));
        }
    })
}

fn size_text(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / (1u64 << 10) as f64),
        b => format!("{b} bytes"),
    }
}

#[cfg(unix)]
fn http_client(unix: Option<&Path>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder();
    if let Some(sock) = unix {
        b = b.unix_socket(sock);
    }
    Ok(b.build()?)
}

#[cfg(not(unix))]
fn http_client(unix: Option<&Path>) -> Result<reqwest::Client> {
    if unix.is_some() {
        bail!("Unix sockets aren't available on this platform");
    }
    Ok(reqwest::Client::new())
}

/// Scans a bundle off the async runtime; hashing a big app takes a moment.
async fn scan_bundle(path: &Path) -> Result<bundle::Scan> {
    let name = bundle::bundle_name(path)?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || bundle::scan(&path, &name)).await?
}

async fn install(c: &Client, path: &Path, dsym: Vec<PathBuf>, no_dsym: bool) -> Result<()> {
    let installed = if path.is_dir() {
        let scan = scan_bundle(path).await?;
        c.send_bundle("/apps/manifest", &scan).await?
    } else {
        c.upload("/apps", path).await?
    };
    print(&installed);
    let bundle_id = installed["bundle_id"].as_str().ok_or_else(|| anyhow!("the server didn't say which app it installed"))?;
    let mut dsyms = dsym;
    if !no_dsym {
        for found in bundle::find_dsyms(path) {
            if !dsyms.contains(&found) {
                dsyms.push(found);
            }
        }
        if dsyms.is_empty() {
            eprintln!("simpool: no dSYM next to {}, so the debugger on the server has no symbols for it. Build one (Xcode: dwarf-with-dsym; Bazel: --apple_generate_dsym) or pass --dsym.", path.display());
        }
    }
    // The app is in already, so a dSYM that can't be placed only costs debugging. Say so and go on.
    let mut failed = None;
    for d in dsyms {
        let sent = async {
            let scan = scan_bundle(&d).await?;
            c.send_bundle(&format!("/apps/{bundle_id}/dsyms"), &scan).await
        };
        match sent.await {
            Ok(_) => eprintln!("simpool: added {}", d.display()),
            Err(e) => {
                eprintln!("simpool: couldn't add {}: {e:#}", d.display());
                failed = Some(d);
            }
        }
    }
    match failed {
        Some(d) => bail!("{} was installed, but without {}", path.display(), d.display()),
        None => Ok(()),
    }
}

/// Renews often enough to survive a couple of failed attempts. Only a lease that's really gone ends it.
async fn keepalive(c: &Client) -> Result<()> {
    let path = c.lease_path("/renew")?;
    let mut wait = std::time::Duration::from_secs(30);
    loop {
        match c.req(Method::POST, &path).send().await {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => bail!("the lease is gone (released or expired)"),
            Ok(resp) if resp.status().is_success() => {
                let lease: Value = resp.json().await.unwrap_or(Value::Null);
                let idle = lease["idle_timeout_secs"].as_u64().unwrap_or(600);
                wait = std::time::Duration::from_secs((idle / 3).clamp(5, 300));
            }
            Ok(resp) => eprintln!("simpool: couldn't renew ({}), will retry", resp.status()),
            Err(e) => eprintln!("simpool: couldn't reach {}, will retry: {e}", c.server),
        }
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}

async fn view(c: &Client) -> Result<()> {
    let mut path = c.lease_path("/view")?;
    if let Some(t) = &c.token {
        path += &format!("?token={}", q(t));
    }
    #[cfg(unix)]
    if let Some(sock) = c.unix.clone() {
        // A browser can't use a Unix socket, so offer it a local port for as long as this runs.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        println!("http://127.0.0.1:{}{path}", listener.local_addr()?.port());
        if let Some(listen) = c.json(Method::GET, "/v1/pool", None).await?["listen"].as_str() {
            let host = listen.replace("0.0.0.0", "127.0.0.1").replace("[::]", "127.0.0.1");
            eprintln!("simpool: that URL works on this machine. On the server's own machine, use http://{host}{path}");
        }
        eprintln!("simpool: Ctrl-C ends the view");
        tokio::select! {
            r = tunnel::serve_socket_proxy(listener, sock) => r?,
            _ = tokio::signal::ctrl_c() => {}
        }
        return Ok(());
    }
    let url = c.url(&path);
    println!("{url}");
    let at_the_mac = cfg!(target_os = "macos") && std::env::var_os("SSH_CONNECTION").is_none();
    if at_the_mac {
        let _ = std::process::Command::new("open").arg(&url).status();
    }
    Ok(())
}

fn print(v: &Value) {
    match v {
        Value::Null => {}
        Value::String(s) => println!("{}", s.trim_end()),
        v => println!("{}", serde_json::to_string_pretty(v).unwrap()),
    }
}

fn json_arg(s: &str) -> Result<Value> {
    let text = if s == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else if let Some(f) = s.strip_prefix('@') {
        std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?
    } else {
        s.to_string()
    };
    serde_json::from_str(&text).context("not valid JSON")
}

fn q(s: &str) -> String {
    // Minimal query-string escaping.
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("simpool: {e:#}");
        std::process::exit(1);
    }
}

async fn input(c: &Client, actions: Value) -> Result<Value> {
    c.on(Method::POST, "/input", Some(json!({ "actions": actions }))).await
}

async fn run(cli: Cli) -> Result<()> {
    let c = Client::new(cli.server, cli.token, cli.lease)?;
    use Method as M;
    match cli.cmd {
        Cmd::Status => print(&c.json(M::GET, "/v1/pool", None).await?),
        Cmd::Lease { profile, config, ttl, wait, label, json } => {
            let simfast_config = match config.as_deref() {
                Some("-") => Some(std::io::read_to_string(std::io::stdin())?),
                Some(f) => Some(std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?),
                None => None,
            };
            let body = json!({"profile": profile, "simfast_config": simfast_config, "ttl_secs": ttl, "wait_secs": wait, "label": label});
            let l = c.json(M::POST, "/v1/leases", Some(body)).await?;
            if json {
                print(&l);
            } else {
                eprintln!("simpool: leased {} ({}, {})", l["udid"].as_str().unwrap_or("?"), l["device"]["device_type"].as_str().unwrap_or("?"), l["device"]["runtime"].as_str().unwrap_or("?"));
                println!("{}", l["id"].as_str().unwrap_or_default());
            }
        }
        Cmd::Leases => print(&c.json(M::GET, "/v1/leases", None).await?),
        Cmd::Info => print(&c.on(M::GET, "", None).await?),
        Cmd::Release => {
            c.on(M::DELETE, "", None).await?;
            eprintln!("simpool: released");
        }
        Cmd::Renew { ttl } => print(&c.on(M::POST, "/renew", Some(json!({"ttl_secs": ttl}))).await?),
        Cmd::Keepalive => keepalive(&c).await?,
        Cmd::Install { path, dsym, no_dsym } => install(&c, &path, dsym, no_dsym).await?,
        Cmd::Uninstall { bundle_id } => c.on(M::DELETE, &format!("/apps/{bundle_id}"), None).await.map(drop)?,
        Cmd::Apps { user } => {
            let v = c.on(M::GET, &format!("/apps?user={user}"), None).await?;
            for a in v["apps"].as_array().into_iter().flatten() {
                println!("{:<45} {:<24} {}", a["bundle_id"].as_str().unwrap_or(""), a["name"].as_str().unwrap_or(""), a["version"].as_str().unwrap_or(""));
            }
        }
        Cmd::Launch { bundle_id, wait_for_debugger, env, args } => {
            let env: serde_json::Map<String, Value> = env
                .iter()
                .map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), json!(v))).ok_or_else(|| anyhow!("--env wants KEY=VALUE, got {kv}")))
                .collect::<Result<_>>()?;
            let v = c.on(M::POST, &format!("/apps/{bundle_id}/launch"), Some(json!({"args": args, "env": env, "wait_for_debugger": wait_for_debugger}))).await?;
            println!("{}", v["pid"]);
        }
        Cmd::Terminate { bundle_id } => c.on(M::POST, &format!("/apps/{bundle_id}/terminate"), None).await.map(drop)?,
        Cmd::Output { bundle_id, follow } => c.stream_out(&c.lease_path(&format!("/apps/{bundle_id}/output?follow={follow}"))?).await?,
        Cmd::Tap { x, y, el, hold, double } => {
            let ty = if double { "double_tap" } else if hold.is_some() { "long_press" } else { "tap" };
            let mut a = json!({"type": ty, "duration": hold});
            match (x, y, el.given()) {
                (Some(x), Some(y), false) => {
                    a["x"] = json!(x);
                    a["y"] = json!(y);
                }
                (None, None, true) => a["element"] = el.json(),
                _ => bail!("give X Y, or an element (--label, --id, --text …)"),
            }
            input(&c, json!([a])).await?;
        }
        Cmd::Swipe { x, y, to_x, to_y, duration } => input(&c, json!([{"type": "swipe", "x": x, "y": y, "to_x": to_x, "to_y": to_y, "duration": duration}])).await.map(drop)?,
        Cmd::Scroll { direction, distance, duration, momentum } => {
            let mut a = json!({"type": "scroll", "direction": direction, "momentum": momentum});
            if let Some(d) = distance { a["distance"] = json!(d); }
            if let Some(d) = duration { a["duration"] = json!(d); }
            input(&c, json!([a])).await.map(drop)?
        }
        Cmd::Type { text } => input(&c, json!([{"type": "text", "text": text}])).await.map(drop)?,
        Cmd::Key { key, modifiers } => input(&c, json!([{"type": "key", "key": key, "modifiers": modifiers}])).await.map(drop)?,
        Cmd::Button { name } => input(&c, json!([{"type": "button", "button": name}])).await.map(drop)?,
        Cmd::Input { actions } => print(&input(&c, json_arg(&actions)?).await?),
        Cmd::Ui { format } => print(&c.on(M::GET, &format!("/ui?format={}", q(&format)), None).await?),
        Cmd::Find { el } => print(&c.on(M::POST, "/ui/find", Some(el.json())).await?),
        Cmd::Wait { el, timeout, gone } => {
            let mut b = el.json();
            b["timeout"] = json!(timeout);
            b["gone"] = json!(gone);
            print(&c.on(M::POST, "/ui/wait", Some(b)).await?)
        }
        Cmd::ScrollTo { el, direction } => {
            let mut b = el.json();
            b["direction"] = json!(direction);
            print(&c.on(M::POST, "/ui/scroll-to", Some(b)).await?)
        }
        Cmd::Screenshot { out } => {
            let jpeg = out.extension().is_some_and(|e| e == "jpg" || e == "jpeg");
            let n = c.download(&c.lease_path(&format!("/screenshot?format={}", if jpeg { "jpeg" } else { "png" }))?, &out).await?;
            eprintln!("simpool: wrote {} ({n} bytes)", out.display());
        }
        Cmd::Record { cmd: RecordCmd::Start { hevc } } => {
            let v = c.on(M::POST, "/recordings", Some(json!({"codec": if hevc { "hevc" } else { "h264" }}))).await?;
            println!("{}", v["id"].as_str().unwrap_or_default());
        }
        Cmd::Record { cmd: RecordCmd::Stop { id, out } } => {
            let resp = c.send(c.req(M::POST, &c.lease_path(&format!("/recordings/{id}/stop"))?)).await?;
            std::fs::write(&out, resp.bytes().await?)?;
            eprintln!("simpool: wrote {}", out.display());
        }
        Cmd::OpenUrl { url } => c.on(M::POST, "/open-url", Some(json!({"url": url}))).await.map(drop)?,
        Cmd::Location { latitude, longitude, clear } => match (latitude, longitude, clear) {
            (_, _, true) => c.on(M::DELETE, "/location", None).await.map(drop)?,
            (Some(a), Some(b), false) => c.on(M::POST, "/location", Some(json!({"latitude": a, "longitude": b}))).await.map(drop)?,
            _ => bail!("give LATITUDE LONGITUDE, or --clear"),
        },
        Cmd::Appearance { mode } => c.on(M::POST, "/appearance", Some(json!({"mode": mode}))).await.map(drop)?,
        Cmd::Push { bundle_id, payload } => c.on(M::POST, "/push", Some(json!({"bundle_id": bundle_id, "payload": json_arg(&payload)?}))).await.map(drop)?,
        Cmd::Privacy { action, service, bundle_id } => c.on(M::POST, "/privacy", Some(json!({"action": action, "service": service, "bundle_id": bundle_id}))).await.map(drop)?,
        Cmd::Pasteboard { set: Some(text) } => {
            c.send(c.req(M::PUT, &c.lease_path("/pasteboard")?).body(text)).await?;
        }
        Cmd::Pasteboard { set: None } => print(&c.on(M::GET, "/pasteboard", None).await?),
        Cmd::Media { files } => {
            for f in files {
                let name = f.file_name().ok_or_else(|| anyhow!("{} isn't a file", f.display()))?.to_string_lossy().to_string();
                c.upload(&format!("/media?name={}", q(&name)), &f).await?;
                eprintln!("simpool: added {name}");
            }
        }
        Cmd::Biometrics { action, kind } => c.on(M::POST, "/biometrics", Some(json!({"action": action, "kind": kind}))).await.map(drop)?,
        Cmd::Logs { predicate, process, level, last } => {
            let mut qs = vec![];
            for (k, v) in [("predicate", predicate), ("process", process), ("level", level), ("last", last)] {
                if let Some(v) = v {
                    qs.push(format!("{k}={}", q(&v)));
                }
            }
            c.stream_out(&c.lease_path(&format!("/logs?{}", qs.join("&")))?).await?
        }
        Cmd::Crashes { name: None } => print(&c.on(M::GET, "/crashes", None).await?),
        Cmd::Crashes { name: Some(n) } => print(&c.on(M::GET, &format!("/crashes/{}", q(&n)), None).await?),
        Cmd::Files { cmd } => files(&c, cmd).await?,
        Cmd::Spawn { argv } => {
            let v = c.on(M::POST, "/spawn", Some(json!({"argv": argv}))).await?;
            print!("{}", v["stdout"].as_str().unwrap_or(""));
            eprint!("{}", v["stderr"].as_str().unwrap_or(""));
            std::process::exit(v["exit_code"].as_i64().unwrap_or(1) as i32);
        }
        Cmd::Debug { bundle_id, pid, attach, gdb, port } => {
            let body = json!({"bundle_id": bundle_id, "pid": pid, "launch": !attach, "protocol": if gdb { "gdb-remote" } else { "dap" }});
            let s = c.on(M::POST, "/debug", Some(body)).await?;
            let sid = s["id"].as_str().unwrap_or_default().to_string();
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
            let local = listener.local_addr()?.port();
            let pid = &s["pid"];
            if gdb {
                eprintln!("simpool: debugserver attached to pid {pid}. In lldb:\n  (lldb) process connect connect://127.0.0.1:{local}");
            } else {
                eprintln!("simpool: lldb-dap is listening on 127.0.0.1:{local}. Point your editor's DAP client there and attach to pid {pid}, e.g. VS Code:\n  {{\"type\": \"lldb-dap\", \"request\": \"attach\", \"name\": \"simpool\", \"pid\": {pid}, \"debugAdapterHostname\": \"127.0.0.1\", \"debugAdapterPort\": {local}}}");
            }
            eprintln!("simpool: Ctrl-C ends the session");
            let ws_path = s["ws_path"].as_str().unwrap_or_default().to_string();
            tokio::select! {
                r = tunnel::serve_forward(&c, listener, ws_path) => r?,
                _ = tokio::signal::ctrl_c() => {}
            }
            let _ = c.on(M::DELETE, &format!("/debug/{sid}"), None).await;
        }
        Cmd::Forward { local_port, remote_port } => {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", local_port)).await?;
            eprintln!("simpool: 127.0.0.1:{} -> simulator Mac 127.0.0.1:{remote_port}", listener.local_addr()?.port());
            tunnel::serve_forward(&c, listener, c.lease_path(&format!("/tunnel?port={remote_port}"))?).await?;
        }
        Cmd::Reverse { remote_port, local } => {
            eprintln!("simpool: simulator Mac 127.0.0.1:{remote_port} -> {local}");
            tunnel::serve_reverse(&c, c.lease_path(&format!("/reverse?port={remote_port}"))?, &local).await?;
        }
        Cmd::View => view(&c).await?,
        Cmd::Api { method, path, body } => {
            let m = Method::from_bytes(method.to_uppercase().as_bytes())?;
            let path = if path.starts_with("/v1") || path == "/healthz" { path } else { c.lease_path(&path)? };
            print(&c.json(m, &path, body.as_deref().map(json_arg).transpose()?).await?);
        }
    }
    Ok(())
}

async fn files(c: &Client, cmd: FilesCmd) -> Result<()> {
    let loc_q = |loc: &Loc, path: &str| {
        let mut s = match &loc.bundle {
            Some(b) => format!("/files?bundle={}&container={}", q(b), q(&loc.container)),
            None => "/files?container=device".to_string(),
        };
        s += &format!("&path={}", q(path));
        s
    };
    match cmd {
        FilesCmd::Ls { loc, path } => {
            let v = c.on(Method::GET, &loc_q(&loc, &path), None).await?;
            for e in v["entries"].as_array().into_iter().flatten() {
                let kind = if e["type"] == "dir" { "/" } else { "" };
                println!("{:>10}  {}{kind}", e["size"], e["name"].as_str().unwrap_or(""));
            }
        }
        FilesCmd::Get { loc, path, out } => {
            let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "download".into());
            let out = out.unwrap_or_else(|| PathBuf::from(name));
            let n = c.download(&c.lease_path(&(loc_q(&loc, &path) + "&archive=true"))?, &out).await?;
            if out != Path::new("-") {
                eprintln!("simpool: wrote {} ({n} bytes)", out.display());
            }
        }
        FilesCmd::Put { loc, local, path } => {
            let f = tokio::fs::read(&local).await.with_context(|| format!("reading {}", local.display()))?;
            c.send(c.req(Method::PUT, &c.lease_path(&loc_q(&loc, &path))?).body(f)).await?;
        }
        FilesCmd::Rm { loc, path, recursive } => {
            c.on(Method::DELETE, &format!("{}&recursive={recursive}", loc_q(&loc, &path)), None).await?;
        }
    }
    Ok(())
}
