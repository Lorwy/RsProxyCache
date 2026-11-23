mod cache;
mod origin;
mod playback;
mod range;

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail, ensure};
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
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

    let playback = Arc::new(playback::Playback {
        origin: origin::Origin::new(hosts, allow_private)?,
        cache: cache::Cache::open(dir, capacity)?,
        locks: Mutex::default(),
    });
    let listener = TcpListener::bind(setting("RPC_LISTEN", "127.0.0.1:8080")).await?;
    eprintln!("listening on {}", listener.local_addr()?);
    loop {
        let (stream, _) = listener.accept().await?;
        let playback = playback.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| playback.clone().request(request));
            let _ = http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}
