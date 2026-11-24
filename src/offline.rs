use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail, ensure};
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use serde::Serialize;
use tokio::sync::watch;

use crate::{
    cache::{Cache, key},
    hls::{self, Resource},
    playback::{Body, Playback, full},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Running,
    Completed,
    Cancelled,
    Unsupported,
    InsufficientCapacity,
    Failed,
}

#[derive(Clone, Serialize)]
pub(crate) struct Progress {
    state: State,
    processed_bytes: u64,
    bandwidth: Option<u64>,
}

struct Task {
    requested: Option<u64>,
    progress: Mutex<Progress>,
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
pub(crate) struct Downloads {
    tasks: Mutex<HashMap<String, Arc<Task>>>,
    variant_parents: Mutex<HashMap<String, String>>,
}

impl Downloads {
    pub(crate) fn note_playlist(&self, source: &str, canonical: &str) {
        if !hls::is_master(canonical) {
            return;
        }
        let mut recent = self.variant_parents.lock().unwrap();
        if recent.len() >= 128 {
            recent.clear();
        }
        for variant in hls::variants(canonical) {
            recent
                .entry(variant.source.to_owned())
                .or_insert_with(|| source.to_owned());
        }
    }

    pub(crate) fn note_variant_request(&self, source: &str) -> Option<String> {
        self.variant_parents.lock().unwrap().get(source).cloned()
    }
}

impl Playback {
    pub(crate) fn start_download(
        self: &Arc<Self>,
        source: &str,
        bandwidth: Option<u64>,
    ) -> Result<(StatusCode, Progress)> {
        self.origin.validate(source)?;
        ensure!(bandwidth != Some(0), "invalid bandwidth");
        let key = key(source);
        let mut tasks = self.downloads.tasks.lock().unwrap();
        if let Some(task) = tasks.get(&key) {
            let progress = task.progress.lock().unwrap().clone();
            if progress.state == State::Running {
                let same = task.requested == bandwidth
                    || bandwidth.is_some() && progress.bandwidth == bandwidth;
                return Ok((
                    if same {
                        StatusCode::ACCEPTED
                    } else {
                        StatusCode::CONFLICT
                    },
                    progress,
                ));
            }
        }
        if tasks.len() >= 256 {
            tasks.retain(|_, task| task.progress.lock().unwrap().state == State::Running);
        }
        let initial = Progress {
            state: State::Running,
            processed_bytes: 0,
            bandwidth,
        };
        let (cancel, mut cancelled) = watch::channel(false);
        let task = Arc::new(Task {
            requested: bandwidth,
            progress: Mutex::new(initial.clone()),
            cancel,
        });
        tasks.insert(key, task.clone());
        let playback = self.clone();
        let source = source.to_owned();
        tokio::spawn(async move {
            let mut pins = TaskPins::new(playback.cache.clone());
            pins.pin(&source);
            let result = tokio::select! {
                biased;
                _ = cancelled.wait_for(|value| *value) => Ok(State::Cancelled),
                result = playback.run_download(&source, &task, &mut pins) => result,
            };
            let state = match result {
                Ok(state) => state,
                Err(error) => {
                    tracing::debug!(error = %error, "offline download failed");
                    State::Failed
                }
            };
            task.progress.lock().unwrap().state = state;
        });
        Ok((StatusCode::ACCEPTED, initial))
    }

    pub(crate) fn download_progress(&self, source: &str, cancel: bool) -> Option<Progress> {
        let task = self
            .downloads
            .tasks
            .lock()
            .unwrap()
            .get(&key(source))
            .cloned()?;
        let progress = task.progress.lock().unwrap().clone();
        if cancel && progress.state == State::Running {
            let _ = task.cancel.send(true);
        }
        Some(progress)
    }

    pub(crate) fn download_request(
        self: &Arc<Self>,
        request: &Request<impl Sized>,
        source: &str,
    ) -> Result<Response<Body>> {
        self.origin.validate(source)?;
        let (status, payload) = match *request.method() {
            Method::POST => {
                let bandwidth = request
                    .headers()
                    .get("x-rpc-bandwidth")
                    .map(|value| value.to_str()?.parse::<u64>().map_err(anyhow::Error::from))
                    .transpose()?;
                let (status, progress) = self.start_download(source, bandwidth)?;
                (status, serde_json::to_vec(&progress)?)
            }
            Method::GET | Method::DELETE => {
                match self.download_progress(source, request.method() == Method::DELETE) {
                    Some(progress) => (StatusCode::OK, serde_json::to_vec(&progress)?),
                    None => (StatusCode::NOT_FOUND, b"{}".to_vec()),
                }
            }
            _ => (StatusCode::METHOD_NOT_ALLOWED, b"{}".to_vec()),
        };
        Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CACHE_CONTROL, "no-store")
            .body(full(payload.into()))?)
    }

    async fn run_download(
        self: &Arc<Self>,
        source: &str,
        task: &Task,
        pins: &mut TaskPins,
    ) -> Result<State> {
        let uri = self.origin.validate(source)?;
        let is_hls = if hls::playlist_path(uri.path()) {
            true
        } else {
            let response = self.offline_fetch(source, None, Method::HEAD, None).await?;
            hls::playlist_type(response.headers())
        };
        if !is_hls {
            let complete = self.transfer(source, None, None, task).await?;
            return Ok(if complete {
                State::Completed
            } else {
                State::InsufficientCapacity
            });
        }
        let published = self.cache.published_playlist(source);
        let canonical = match self.cache.original_playlist(source) {
            Some(playlist) => playlist,
            None => self.download_manifest(source).await?,
        };
        if hls::uses_encryption(&canonical) {
            return Ok(State::Unsupported);
        }
        let (selected, playlists) = if hls::is_master(&canonical) {
            let recent = self.recent_variant(source);
            let mut variants = hls::variants(&canonical);
            variants.retain(|variant| {
                task.requested
                    .is_none_or(|wanted| variant.bandwidth == wanted)
            });
            variants.sort_by_key(|variant| {
                (recent.as_deref() != Some(variant.source), variant.bandwidth)
            });
            ensure!(!variants.is_empty(), "requested HLS variant is unavailable");
            let mut selected = None;
            let mut unsupported = false;
            let mut last_error = None;
            for variant in variants {
                task.progress.lock().unwrap().bandwidth = Some(variant.bandwidth);
                if published.as_deref().is_some_and(|playlist| {
                    hls::variants(playlist)
                        .iter()
                        .any(|cached| cached.source == variant.source)
                }) {
                    return Ok(State::Completed);
                }
                match self.variant_playlists(&canonical, &variant).await {
                    Ok(Some(playlists)) => {
                        selected = Some((variant.source.to_owned(), playlists));
                        break;
                    }
                    Ok(None) => unsupported = true,
                    Err(error) => last_error = Some(error),
                }
            }
            match selected {
                Some((variant, playlists)) => (Some(variant), playlists),
                None if unsupported => return Ok(State::Unsupported),
                None => return Err(last_error.context("no playable HLS variant")?),
            }
        } else {
            if published.is_some() {
                return Ok(State::Completed);
            }
            (None, vec![(source.to_owned(), canonical.clone())])
        };
        for (playlist_source, playlist) in playlists {
            pins.pin(&playlist_source);
            let mut references = Vec::new();
            hls::media_references(&playlist, |address, range| {
                references.push((address.to_owned(), range));
                Ok((address.to_owned(), false))
            })?;
            self.cache.observe_playlist(&playlist_source, playlist);
            let mut seen = HashSet::new();
            for (address, range) in references {
                if seen.insert((address.clone(), range.clone())) {
                    pins.pin(&address);
                    if !self
                        .transfer(&address, Some(Resource::Media), range, task)
                        .await?
                    {
                        return Ok(State::InsufficientCapacity);
                    }
                }
            }
            if self.cache.published_playlist(&playlist_source).is_none() {
                return Ok(State::InsufficientCapacity);
            }
        }
        if hls::is_master(&canonical) {
            self.cache.observe_playlist(source, canonical);
        }
        Ok(
            if self
                .cache
                .published_playlist(source)
                .is_some_and(|playlist| {
                    selected.as_ref().is_none_or(|wanted| {
                        hls::variants(&playlist)
                            .iter()
                            .any(|variant| variant.source == wanted)
                    })
                })
            {
                State::Completed
            } else {
                State::InsufficientCapacity
            },
        )
    }

    async fn variant_playlists(
        &self,
        canonical: &str,
        variant: &hls::Variant<'_>,
    ) -> Result<Option<Vec<(String, String)>>> {
        let mut playlists = Vec::new();
        for address in
            std::iter::once(variant.source).chain(hls::default_audio(canonical, variant.attributes))
        {
            let playlist = match self.cache.original_playlist(address) {
                Some(playlist) => playlist,
                None => self.download_manifest(address).await?,
            };
            if hls::uses_encryption(&playlist) {
                return Ok(None);
            }
            ensure!(!hls::is_master(&playlist), "nested HLS master playlist");
            playlists.push((address.to_owned(), playlist));
        }
        Ok(Some(playlists))
    }

    async fn transfer(
        self: &Arc<Self>,
        source: &str,
        resource: Option<Resource>,
        span: Option<Range<u64>>,
        task: &Task,
    ) -> Result<bool> {
        let mut response = self
            .offline_fetch(source, resource, Method::GET, span.as_ref())
            .await?;
        if response.status().is_success()
            && response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|length| length > self.cache.capacity())
        {
            return Ok(false);
        }
        ensure!(
            response.status().is_success(),
            "origin returned {}",
            response.status()
        );
        while let Some(frame) = response.body_mut().frame().await {
            let frame = frame?;
            if let Some(data) = frame.data_ref() {
                let mut progress = task.progress.lock().unwrap();
                progress.processed_bytes =
                    progress.processed_bytes.saturating_add(data.len() as u64);
            }
        }
        Ok(self.cache.contains(source, span))
    }

    async fn offline_fetch(
        self: &Arc<Self>,
        source: &str,
        resource: Option<Resource>,
        method: Method,
        span: Option<&Range<u64>>,
    ) -> Result<Response<Body>> {
        let target = match resource {
            Some(resource) => format!("hls/{}/{source}", resource.name()),
            None => source.to_owned(),
        };
        let path = match &self.signing_key {
            Some(key) => crate::mobile::sign(key, &target),
            None => target,
        };
        let mut request = Request::builder().uri(format!("/v1/{path}")).method(method);
        if let Some(span) = span {
            request = request.header(header::RANGE, crate::range::upstream(span));
        }
        self.handle(request.body(())?).await
    }

    async fn download_manifest(&self, source: &str) -> Result<String> {
        let origin = self.origin.fetch(source, Method::GET, None).await?;
        if !origin.status().is_success() {
            bail!("playlist origin returned {}", origin.status());
        }
        self.canonical_playlist(origin).await
    }
}

struct TaskPins {
    cache: Arc<Cache>,
    keys: HashSet<String>,
}

impl TaskPins {
    fn new(cache: Arc<Cache>) -> Self {
        Self {
            cache,
            keys: HashSet::new(),
        }
    }

    fn pin(&mut self, source: &str) {
        let key = key(source);
        if self.keys.insert(key.clone()) {
            self.cache.pin(key);
        }
    }
}

impl Drop for TaskPins {
    fn drop(&mut self) {
        for key in &self.keys {
            self.cache.unpin(key);
        }
    }
}
