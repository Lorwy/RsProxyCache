use std::{
    collections::HashMap,
    convert::Infallible,
    io,
    ops::Range,
    sync::{Arc, Mutex, Weak},
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use futures_util::stream;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Frame, Incoming};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::{OwnedMutexGuard, mpsc},
    time::timeout,
};

use crate::{
    cache::{self, Cache, Entry, Metadata, Piece},
    origin::{FinalTarget, Origin, TIMEOUT},
    range::{self, Requested},
};

type Body = UnsyncBoxBody<Bytes, io::Error>;
type Sender = mpsc::Sender<io::Result<Frame<Bytes>>>;

pub struct Playback {
    pub cache: Arc<Cache>,
    pub origin: Origin,
    pub locks: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

enum Prepared {
    Parts(Vec<Part>),
    Failure(Response<Body>),
    Retry,
}

enum Part {
    Local {
        span: Range<u64>,
        entry: Arc<Entry>,
    },
    Remote {
        body: Incoming,
        span: Option<Range<u64>>,
        length: Option<u64>,
    },
}

impl Playback {
    pub async fn request(
        self: Arc<Self>,
        request: Request<Incoming>,
    ) -> Result<Response<Body>, Infallible> {
        let response = match self.handle(request).await {
            Ok(response) => response,
            Err(_) => plain(StatusCode::BAD_GATEWAY, "origin or cache request failed"),
        };
        Ok(response)
    }

    async fn handle(self: &Arc<Self>, request: Request<Incoming>) -> Result<Response<Body>> {
        let Some(source) = request
            .uri()
            .path_and_query()
            .and_then(|value| value.as_str().strip_prefix("/v1/"))
        else {
            return Ok(plain(StatusCode::NOT_FOUND, "unknown playback path"));
        };
        if self.origin.validate(source).is_err() {
            return Ok(plain(
                StatusCode::BAD_REQUEST,
                "invalid or disallowed source URL",
            ));
        }
        let head = request.method() == Method::HEAD;
        if !head && request.method() != Method::GET {
            let mut response = plain(StatusCode::METHOD_NOT_ALLOWED, "use GET or HEAD");
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return Ok(response);
        }
        let requested = if head {
            Requested::Whole
        } else {
            let mut ranges = request.headers().get_all(header::RANGE).iter();
            match ranges.next() {
                None => Requested::Whole,
                Some(value) => match value.to_str().ok().and_then(Requested::parse) {
                    Some(range) if ranges.next().is_none() => range,
                    _ => return Ok(plain(StatusCode::BAD_REQUEST, "expected one bytes range")),
                },
            }
        };
        let key = cache::key(source);
        let guard = if head {
            None
        } else {
            Some(self.lock(&format!("{key}:{}", requested.upstream())).await)
        };
        let previous = self.cache.metadata(&key);
        if head {
            if let Some(metadata) = previous {
                return media_response(requested, 0..metadata.size, &metadata, empty());
            }
            let origin = self.origin.fetch(source, Method::HEAD, None).await?;
            return relay_headers(origin.status(), origin.headers(), empty());
        }

        if let Some(metadata) = &previous {
            if let Some(span) = requested.resolve(metadata.size) {
                match self.prepare(source, &key, span.clone(), metadata).await? {
                    Prepared::Parts(parts) => {
                        if parts.iter().all(|part| matches!(part, Part::Local { .. })) {
                            let body = self.stream(key, None, parts, None);
                            return media_response(requested, span, metadata, body);
                        }
                        if let Some(version) = self.cache.patch_version(&key, metadata) {
                            let body = self.stream(key, Some(version), parts, guard);
                            return media_response(requested, span, metadata, body);
                        }
                    }
                    Prepared::Failure(response) => return Ok(response),
                    Prepared::Retry => {}
                }
            } else if matches!(requested, Requested::Suffix(0)) {
                return unsatisfiable(metadata.size);
            }
        }

        let mut origin = self
            .origin
            .fetch(source, Method::GET, Some(&requested.upstream()))
            .await?;
        if matches!(requested, Requested::Whole)
            && origin.status() == StatusCode::RANGE_NOT_SATISFIABLE
        {
            origin = self.origin.fetch(source, Method::GET, None).await?;
        }

        match origin.status() {
            StatusCode::PARTIAL_CONTENT => {
                let (span, metadata) = partial_metadata(&origin)?;
                ensure!(
                    requested.resolve(metadata.size).as_ref() == Some(&span),
                    "unexpected origin byte range"
                );
                let version = self.cache.adopt(&key, metadata.clone());
                let part = Part::Remote {
                    body: origin.into_body(),
                    length: Some(span.end - span.start),
                    span: Some(span.clone()),
                };
                let body = self.stream(key, Some(version), vec![part], guard);
                media_response(requested, span, &metadata, body)
            }
            StatusCode::OK if matches!(requested, Requested::Whole) => {
                identity(origin.headers())?;
                let status = origin.status();
                let headers = origin.headers().clone();
                let part = Part::Remote {
                    body: origin.into_body(),
                    span: None,
                    length: content_length(&headers)?,
                };
                let body = self.stream(key, None, vec![part], guard);
                relay_headers(status, &headers, body)
            }
            StatusCode::OK => Ok(plain(StatusCode::BAD_GATEWAY, "origin ignored Range")),
            status if status.is_client_error() || status.is_server_error() => {
                relay_headers(status, origin.headers(), empty())
            }
            _ => Ok(plain(StatusCode::BAD_GATEWAY, "unexpected origin response")),
        }
    }

    async fn lock(&self, key: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().unwrap();
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key.to_owned(), Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }

    async fn prepare(
        &self,
        source: &str,
        key: &str,
        span: Range<u64>,
        metadata: &Metadata,
    ) -> Result<Prepared> {
        let plan = self.cache.plan(key, span, metadata);
        if !plan.iter().any(|piece| matches!(piece, Piece::Cached(..))) {
            return Ok(Prepared::Retry);
        }
        let mut parts = Vec::with_capacity(plan.len());
        for piece in plan {
            match piece {
                Piece::Cached(entry, span) => {
                    parts.push(Part::Local { span, entry });
                }
                Piece::Missing(span) => {
                    let response = self
                        .origin
                        .fetch(source, Method::GET, Some(&range::upstream(&span)))
                        .await?;
                    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                        return Ok(Prepared::Retry);
                    }
                    if response.status().is_client_error() || response.status().is_server_error() {
                        return Ok(Prepared::Failure(relay_headers(
                            response.status(),
                            response.headers(),
                            empty(),
                        )?));
                    }
                    ensure!(
                        response.status() == StatusCode::PARTIAL_CONTENT,
                        "origin ignored Range"
                    );
                    let (actual_span, actual) = partial_metadata(&response)?;
                    if actual != *metadata {
                        return Ok(Prepared::Retry);
                    }
                    ensure!(actual_span == span, "unexpected origin byte range");
                    parts.push(Part::Remote {
                        body: response.into_body(),
                        length: Some(span.end - span.start),
                        span: Some(span),
                    });
                }
            }
        }
        Ok(Prepared::Parts(parts))
    }

    fn stream(
        self: &Arc<Self>,
        key: String,
        metadata: Option<Arc<Metadata>>,
        parts: Vec<Part>,
        guard: Option<OwnedMutexGuard<()>>,
    ) -> Body {
        let (sender, receiver) = mpsc::channel(4);
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let _guard = guard;
            for part in parts {
                let result = match part {
                    Part::Local { span, entry } => send_local(&sender, &entry, span).await,
                    Part::Remote { body, span, length } => {
                        send_remote(&sender, &cache, &key, metadata.as_ref(), body, span, length)
                            .await
                    }
                };
                if let Err(error) = result {
                    let _ = sender.send(Err(io::Error::other(error))).await;
                    break;
                }
            }
        });
        StreamBody::new(stream::unfold(receiver, |mut receiver| async {
            receiver.recv().await.map(|frame| (frame, receiver))
        }))
        .boxed_unsync()
    }
}

async fn send_local(sender: &Sender, entry: &Entry, span: Range<u64>) -> Result<()> {
    let (file, offset) = entry.open(&span)?;
    let mut file = File::from_std(file);
    let mut remaining = span.end - span.start;
    file.seek(SeekFrom::Start(offset)).await?;
    let mut buffer = vec![0; 64 * 1024];
    while remaining > 0 {
        let size = (remaining.min(buffer.len() as u64)) as usize;
        let size = file.read(&mut buffer[..size]).await?;
        ensure!(size != 0, "cache file ended early");
        sender
            .send(Ok(Frame::data(Bytes::copy_from_slice(&buffer[..size]))))
            .await?;
        remaining -= size as u64;
    }
    Ok(())
}

async fn send_remote(
    sender: &Sender,
    cache: &Arc<Cache>,
    key: &str,
    metadata: Option<&Arc<Metadata>>,
    mut body: Incoming,
    span: Option<Range<u64>>,
    length: Option<u64>,
) -> Result<()> {
    let mut writer = match (span, metadata) {
        (Some(span), Some(metadata)) => {
            let chunk_size = if metadata.etag.is_some() {
                cache.max_interval_size()
            } else {
                span.end - span.start
            };
            (chunk_size > 0).then(|| IntervalWriter {
                chunk_size,
                end: span.end,
                next: span.start,
                chunk_end: span.start,
                chunk: None,
            })
        }
        _ => None,
    };
    let mut received = 0_u64;
    let mut last = None;
    loop {
        let frame = tokio::select! {
            _ = sender.closed() => return Ok(()),
            frame = timeout(TIMEOUT, body.frame()) => frame?,
        };
        let Some(frame) = frame else { break };
        let Ok(data) = frame?.into_data() else {
            continue;
        };
        if data.is_empty() {
            continue;
        }
        received = received
            .checked_add(data.len() as u64)
            .context("body too large")?;
        ensure!(
            length.is_none_or(|length| received <= length),
            "origin exceeded declared length"
        );
        if let (Some(writer), Some(metadata)) = (&mut writer, metadata) {
            writer.write(cache, key, metadata, &data).await;
        }
        // Hold the final bytes until length is verified and the completed interval is published.
        if length == Some(received) {
            last = Some(data);
        } else {
            sender.send(Ok(Frame::data(data))).await?;
        }
    }
    ensure!(
        length.is_none_or(|length| received == length),
        "origin ended early"
    );
    if let (Some(writer), Some(metadata)) = (writer, metadata)
        && let Some(chunk) = writer.chunk
    {
        chunk.commit(key, metadata).await;
    }
    if let Some(last) = last {
        sender.send(Ok(Frame::data(last))).await?;
    }
    Ok(())
}

struct IntervalWriter {
    chunk_size: u64,
    end: u64,
    next: u64,
    chunk_end: u64,
    chunk: Option<StagedInterval>,
}

impl IntervalWriter {
    async fn write(
        &mut self,
        cache: &Arc<Cache>,
        key: &str,
        metadata: &Arc<Metadata>,
        mut data: &[u8],
    ) {
        while !data.is_empty() {
            if self.next == self.chunk_end {
                self.chunk_end = self.next.saturating_add(self.chunk_size).min(self.end);
                self.chunk = StagedInterval::new(cache, self.next..self.chunk_end);
            }
            let size = (self.chunk_end - self.next).min(data.len() as u64) as usize;
            if let Some(chunk) = &mut self.chunk
                && chunk.file.write_all(&data[..size]).await.is_err()
            {
                self.chunk = None;
            }
            self.next += size as u64;
            data = &data[size..];
            if self.next == self.chunk_end
                && self.next != self.end
                && let Some(chunk) = self.chunk.take()
            {
                chunk.commit(key, metadata).await;
            }
        }
    }
}

struct StagedInterval {
    reservation: cache::Reservation,
    stage: tempfile::NamedTempFile,
    file: File,
    span: Range<u64>,
}

impl StagedInterval {
    fn new(cache: &Arc<Cache>, span: Range<u64>) -> Option<Self> {
        let reservation = cache.reserve(span.end - span.start)?;
        let stage = cache.stage().ok()?;
        let file = File::from_std(stage.reopen().ok()?);
        Some(Self {
            reservation,
            stage,
            file,
            span,
        })
    }

    async fn commit(mut self, key: &str, metadata: &Arc<Metadata>) {
        if self.file.flush().await.is_err() {
            return;
        }
        drop(self.file.into_std().await);
        let metadata = metadata.clone();
        let key = key.to_owned();
        let _ = tokio::task::spawn_blocking(move || {
            self.reservation
                .commit(self.stage, key, self.span, metadata)
        })
        .await;
    }
}

fn partial_metadata(response: &Response<Incoming>) -> Result<(Range<u64>, Metadata)> {
    let headers = response.headers();
    let target = response
        .extensions()
        .get::<FinalTarget>()
        .context("missing origin target")?;
    identity(headers)?;
    let (span, size) = headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(range::content_range)
        .context("invalid Content-Range")?;
    ensure!(
        content_length(headers)?.is_none_or(|length| length == span.end - span.start),
        "invalid origin length"
    );
    let etag = headers
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            value.len() >= 2
                && value.starts_with('"')
                && value.ends_with('"')
                && !value[1..value.len() - 1].contains('"')
        })
        .map(str::to_owned);
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    Ok((
        span,
        Metadata {
            target: target.0.clone(),
            size,
            etag,
            content_type,
        },
    ))
}

fn content_length(headers: &HeaderMap) -> Result<Option<u64>> {
    headers
        .get(header::CONTENT_LENGTH)
        .map(|value| Ok(value.to_str()?.parse()?))
        .transpose()
}

fn identity(headers: &HeaderMap) -> Result<()> {
    ensure!(
        headers
            .get(header::CONTENT_ENCODING)
            .is_none_or(|value| { value.as_bytes().eq_ignore_ascii_case(b"identity") }),
        "origin encoded the representation"
    );
    Ok(())
}

fn media_response(
    requested: Requested,
    span: Range<u64>,
    metadata: &Metadata,
    body: Body,
) -> Result<Response<Body>> {
    let mut response = Response::builder()
        .header(header::CONTENT_LENGTH, span.end - span.start)
        .header(header::ACCEPT_RANGES, "bytes");
    if !matches!(requested, Requested::Whole) {
        response = response.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", span.start, span.end - 1, metadata.size),
        );
    }
    if let Some(content_type) = &metadata.content_type {
        response = response.header(header::CONTENT_TYPE, content_type);
    }
    if let Some(etag) = &metadata.etag {
        response = response.header(header::ETAG, etag);
    }
    Ok(response.body(body)?)
}

fn relay_headers(status: StatusCode, headers: &HeaderMap, body: Body) -> Result<Response<Body>> {
    let mut response = Response::builder().status(status);
    if status.is_success() {
        for name in [
            header::CONTENT_LENGTH,
            header::CONTENT_TYPE,
            header::ACCEPT_RANGES,
            header::ETAG,
            header::LAST_MODIFIED,
        ] {
            if let Some(value) = headers.get(&name) {
                response = response.header(name, value);
            }
        }
    } else if status == StatusCode::RANGE_NOT_SATISFIABLE
        && let Some(value) = headers.get(header::CONTENT_RANGE)
    {
        response = response.header(header::CONTENT_RANGE, value);
    }
    Ok(response.body(body)?)
}

fn unsatisfiable(size: u64) -> Result<Response<Body>> {
    Ok(Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
        .body(empty())?)
}

fn plain(status: StatusCode, message: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(Bytes::from_static(message.as_bytes())))
        .unwrap()
}

fn empty() -> Body {
    full(Bytes::new())
}

fn full(bytes: Bytes) -> Body {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}
