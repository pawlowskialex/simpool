mod api;
mod bridge;
mod config;
mod error;
mod fsutil;
mod pool;
mod proc;

use anyhow::{Context, Result};
use clap::Parser;
use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// simpoold: leases simfast simulators to remote clients and drives them over HTTP.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Server config
    #[arg(short, long, default_value = "simpool.toml", env = "SIMPOOL_CONFIG")]
    config: PathBuf,
    /// Override `listen` from the config
    #[arg(long)]
    listen: Option<std::net::SocketAddr>,
    /// Override `listen_unix` from the config
    #[arg(long)]
    listen_unix: Option<PathBuf>,
}

/// Binds the socket, replacing one left by an earlier run. Only the owner can connect.
fn bind_unix(path: &Path) -> Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
        std::fs::remove_file(path).with_context(|| format!("removing old socket {}", path.display()))?;
    }
    let listener = tokio::net::UnixListener::bind(path).with_context(|| format!("listening on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "simpoold=info".into())).init();
    let args = Args::parse();
    let mut cfg = config::Config::load(&args.config)?;
    if let Some(l) = args.listen {
        cfg.listen = l;
    }
    if let Some(p) = args.listen_unix {
        cfg.listen_unix = Some(p);
    }
    let data = cfg.data_dir();
    std::fs::create_dir_all(data.join("leases")).with_context(|| format!("creating {}", data.display()))?;
    let developer_dir = proc::developer_dir()?;
    let bridge_exe = bridge::install(&data.join("bin"))?;
    proc::run(&cfg.simfast, ["--version"], None, &[], std::time::Duration::from_secs(10))
        .await
        .with_context(|| format!("can't run simfast ({}); set `simfast` in the config", cfg.simfast.display()))?;

    if cfg.tokens.is_empty() && !cfg.listen.ip().is_loopback() {
        tracing::warn!("listening on {} with no tokens: anyone who can reach this port can run code on this Mac", cfg.listen);
    }
    let listen = cfg.listen;
    let listen_unix = cfg.listen_unix.clone();
    let pool = pool::Pool::new(cfg, developer_dir, bridge_exe);
    pool.recover().await;
    tokio::spawn(pool.clone().reap_forever());

    let app = api::router(pool.clone());
    let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("listening on {listen}"))?;
    // After the TCP bind, so a second simpoold fails there instead of taking over this socket.
    let unix_listener = listen_unix.as_deref().map(bind_unix).transpose()?;
    tracing::info!("simpoold listening on http://{listen} ({} profiles, max {} simulators)", pool.cfg.profiles.len(), pool.cfg.max_devices);
    if let Some(p) = &listen_unix {
        tracing::info!("also listening on unix://{}", p.display());
    }
    let shutdown = CancellationToken::new();
    let (p2, done) = (pool.clone(), shutdown.clone());
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        tracing::info!("shutting down: releasing every lease");
        // Releasing cancels the leases' streams and sockets, which lets serve() finish.
        p2.release_all().await;
        done.cancel();
    });
    let tcp = axum::serve(listener, app.clone()).with_graceful_shutdown(shutdown.clone().cancelled_owned());
    match unix_listener {
        Some(unix) => {
            let unix = axum::serve(unix, app).with_graceful_shutdown(shutdown.cancelled_owned());
            tokio::try_join!(tcp.into_future(), unix.into_future())?;
            let _ = std::fs::remove_file(listen_unix.unwrap());
        }
        None => tcp.await?,
    }
    Ok(())
}
