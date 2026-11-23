mod cache;
mod origin;
mod playback;
mod range;

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    let level = setting("RPC_LOG_LEVEL", "info")
        .parse::<tracing::level_filters::LevelFilter>()
        .context("invalid RPC_LOG_LEVEL")?;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(level)
        .with_target(false)
        .init();
    let mut args = std::env::args().skip(1);
    let command = args.next();
    if command
        .as_deref()
        .is_some_and(|command| !matches!(command, "serve" | "clear"))
    {
        bail!("usage: rs-proxy-cache [serve | clear SOURCE_URL]");
    }
    let dir = PathBuf::from(setting("RPC_CACHE_DIR", "./cache"));
    let capacity = setting("RPC_CACHE_BYTES", "10737418240")
        .parse::<u64>()
        .context("invalid RPC_CACHE_BYTES")?;
    if command.as_deref() == Some("clear") {
        let source = args
            .next()
            .context("clear requires a complete source URL")?;
        ensure!(args.next().is_none(), "unexpected argument");
        let cache = cache::Cache::open(dir, u64::MAX)?;
        cache.clear(&cache::key(&source));
        return Ok(());
    }
    ensure!(args.next().is_none(), "unexpected argument");

    let hosts: HashSet<_> = setting("RPC_ORIGIN_HOSTS", "")
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    ensure!(
        !hosts.is_empty(),
        "RPC_ORIGIN_HOSTS must contain at least one host"
    );
    let allow_private = setting("RPC_ALLOW_PRIVATE_ORIGINS", "false")
        .parse::<bool>()
        .context("invalid RPC_ALLOW_PRIVATE_ORIGINS")?;

    let origin_hosts = hosts.len();
    let cache_dir = dir.clone();
    let playback = Arc::new(playback::Playback::new(
        cache::Cache::open(dir, capacity)?,
        origin::Origin::new(hosts, allow_private)?,
    ));
    let listener = TcpListener::bind(setting("RPC_LISTEN", "127.0.0.1:8080")).await?;
    tracing::info!(listen = %listener.local_addr()?, cache_dir = %cache_dir.display(), capacity_bytes = capacity, origin_hosts, allow_private, "listening on configured address");
    loop {
        let (stream, _) = listener.accept().await?;
        let playback = playback.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| playback.clone().request(request));
            if let Err(error) = http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(%error, "client connection ended with an error");
            }
        });
    }
}

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}
