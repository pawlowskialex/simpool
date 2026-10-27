mod api;
mod bridge;
mod config;
mod error;
mod pool;
mod proc;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "simpoold=info".into())).init();
    let args = Args::parse();
    let mut cfg = config::Config::load(&args.config)?;
    if let Some(l) = args.listen {
        cfg.listen = l;
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
    let pool = pool::Pool::new(cfg, developer_dir, bridge_exe);
    pool.recover().await;
    tokio::spawn(pool.clone().reap_forever());

    let app = api::router(pool.clone());
    let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("listening on {listen}"))?;
    tracing::info!("simpoold listening on http://{listen} ({} profiles, max {} simulators)", pool.cfg.profiles.len(), pool.cfg.max_devices);
    let p2 = pool.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            tracing::info!("shutting down: releasing every lease");
            // Releasing cancels the leases' streams and sockets, which lets serve() finish.
            p2.release_all().await;
        })
        .await?;
    Ok(())
}
