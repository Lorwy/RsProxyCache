pub mod cache;
pub mod origin;
pub mod playback;
mod range;

mod mobile;

use std::sync::Arc;

use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

pub async fn serve(
    listener: TcpListener,
    playback: Arc<playback::Playback>,
) -> std::io::Result<()> {
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
