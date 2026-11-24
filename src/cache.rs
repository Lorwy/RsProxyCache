use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use crate::hls;

mod published;
use anyhow::{Context, Result, ensure};
use published::{covering_entries, publish_complete, publish_playlist};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{fs::File as AsyncFile, io::AsyncWriteExt};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Metadata {
    pub target: String,
    pub size: u64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
}

const INTERNAL: &str = ".rpc";

pub struct CacheVersion {
    source: String,
    key: String,
    metadata: Metadata,
    id: u64,
}

impl CacheVersion {
    pub fn id(&self) -> u64 {
        self.id
    }
}

#[derive(Deserialize, Serialize)]
struct Record {
    source: String,
    key: String,
    published: Option<PathBuf>,
    modified: Option<SystemTime>,
    playlist: Option<PublishedPlaylist>,
    span: Range<u64>,
    metadata: Metadata,
}

#[derive(Clone, Deserialize, Serialize)]
struct PublishedPlaylist {
    canonical: String,
    original: String,
    dependencies: Vec<PublishedDependency>,
}

#[derive(Clone, Deserialize, Serialize, Hash, Eq, PartialEq)]
struct PublishedDependency {
    key: String,
    path: PathBuf,
}

pub struct Entry {
    record: Record,
    path: PathBuf,
    sidecar: PathBuf,
    used: Mutex<SystemTime>,
    retired: AtomicBool,
}

impl Entry {
    fn matches_published_file(&self) -> bool {
        let Some(relative) = &self.record.published else {
            return false;
        };
        if !self
            .path
            .ancestors()
            .skip(1)
            .take(relative.components().count() - 1)
            .all(|path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()))
        {
            return false;
        }
        fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.len() == self.size()
                && metadata.modified().ok() == self.record.modified
        })
    }

    fn size(&self) -> u64 {
        self.record.span.end - self.record.span.start
    }

    fn open(&self, span: &Range<u64>) -> Result<(File, u64)> {
        if self.record.published.is_some() {
            ensure!(
                self.matches_published_file(),
                "published cache file changed"
            );
        }
        let file = File::open(&self.path)?;
        if !self.retired.load(Ordering::Acquire) {
            fs::metadata(&self.sidecar)?;
        }
        ensure!(
            file.metadata()?.len() == self.size()
                && self.record.modified.is_none_or(|time| file
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok()
                    == Some(time)),
            "invalid cache file length"
        );
        Ok((file, span.start - self.record.span.start))
    }

    pub fn touch(&self, file: &File) {
        let now = SystemTime::now();
        *self.used.lock().unwrap() = now;
        let result = if self.record.published.is_some() {
            File::open(&self.sidecar).and_then(|file| file.set_modified(now))
        } else {
            file.set_modified(now)
        };
        if let Err(error) = result {
            cache_warning("update_interval_access_time", &error);
        }
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if *self.retired.get_mut() && self.record.published.is_none() {
            remove(&self.path);
        }
    }
}

pub enum Piece {
    Cached(Arc<Entry>, Range<u64>),
    Missing(Range<u64>),
}

pub struct Cache {
    dir: PathBuf,
    capacity: u64,
    index: Mutex<Index>,
}

#[derive(Default)]
struct Index {
    entries: HashMap<String, Vec<Arc<Entry>>>,
    size: u64,
    reserved: u64,
    versions: HashMap<String, Weak<CacheVersion>>,
    next_version: u64,
    retired: Vec<(Weak<Entry>, u64)>,
    lease: Option<DirectoryLease>,
    generation: u64,
    playlists: HashMap<String, (Instant, String)>,
    pins: HashMap<String, usize>,
    publishing: HashSet<u64>,
}

struct Reservation {
    cache: Arc<Cache>,
    size: u64,
    generation: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut index = self.cache.index.lock().unwrap();
        if self.generation == index.generation {
            index.reserved -= self.size;
        }
    }
}

impl Cache {
    pub fn open(dir: PathBuf, capacity: u64) -> Result<Arc<Self>> {
        let index = Self::load_index(&dir, capacity)?;
        Ok(Self::with_index(dir, capacity, index))
    }

    pub(crate) fn open_recoverable(dir: PathBuf, capacity: u64) -> Arc<Self> {
        let index = Self::load_index(&dir, capacity).unwrap_or_else(|error| {
            cache_warning("open_directory", &format_args!("{error:#}"));
            Index::default()
        });
        Self::with_index(dir, capacity, index)
    }

    fn with_index(dir: PathBuf, capacity: u64, index: Index) -> Arc<Self> {
        tracing::debug!(
            intervals = index.entries.values().map(Vec::len).sum::<usize>(),
            bytes = index.size,
            "cache opened"
        );
        Arc::new(Self {
            dir,
            capacity,
            index: Mutex::new(index),
        })
    }

    fn load_index(dir: &Path, capacity: u64) -> Result<Index> {
        let lease = DirectoryLease::acquire(dir)?;
        let mut index = Index {
            lease: Some(lease),
            ..Index::default()
        };
        let internal = dir.join(INTERNAL);
        ensure_directory(&internal)?;
        for item in fs::read_dir(&internal)? {
            let path = item?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                match load(dir, &path) {
                    Ok(entry) => {
                        if let Some(playlist) = &entry.record.playlist
                            && hls::is_master(&playlist.original)
                        {
                            index
                                .remember_playlist(&entry.record.source, playlist.original.clone());
                        }
                        index.insert(entry);
                    }
                    Err(_) => {
                        cache_warning("load_interval", &"invalid or unreadable cache record");
                        remove(&path);
                        remove(&path.with_extension("part"));
                    }
                }
            }
        }
        for item in fs::read_dir(&internal)? {
            let path = item?.path();
            if (path.extension().is_some_and(|ext| ext == "part")
                && !path.with_extension("json").is_file())
                || path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".rpc-stage-"))
            {
                remove(&path);
            }
        }
        let invalid: Vec<_> = index
            .entries
            .values()
            .flatten()
            .filter(|entry| {
                entry.record.playlist.is_some()
                    && !index.valid_published_graph(entry, &mut HashSet::new())
            })
            .map(|entry| entry.record.key.clone())
            .collect();
        for key in invalid {
            index.clear(&key);
        }
        index.evict(capacity);
        Ok(index)
    }

    pub(crate) fn capacity(&self) -> u64 {
        self.capacity
    }

    pub(crate) fn pin(&self, key: String) {
        let mut index = self.index.lock().unwrap();
        self.refresh_index(&mut index);
        *index.pins.entry(key).or_default() += 1;
    }

    pub(crate) fn unpin(&self, key: &str) {
        let mut index = self.index.lock().unwrap();
        if let Some(count) = index.pins.get_mut(key) {
            *count -= 1;
            if *count == 0 {
                index.pins.remove(key);
            }
        }
    }

    pub(crate) fn contains(&self, source: &str, span: Option<Range<u64>>) -> bool {
        let key = key(source);
        let index = self.index.lock().unwrap();
        match span {
            Some(span) => index.entries.get(&key).is_some_and(|entries| {
                entries
                    .last()
                    .and_then(|entry| covering_entries(entries, &entry.record.metadata, &span))
                    .is_some_and(|covered| {
                        covered.iter().all(|(entry, span)| entry.open(span).is_ok())
                    })
            }),
            None => index
                .published(&key)
                .is_some_and(|entry| entry.matches_published_file()),
        }
    }

    pub(crate) fn observe_playlist(&self, source: &str, canonical: String) {
        let mut index = self.index.lock().unwrap();
        if self.refresh_index(&mut index) {
            index.remember_playlist(source, canonical);
            index.publish_playlists(&self.dir, self.capacity);
        }
    }

    pub(crate) fn published_playlist(&self, source: &str) -> Option<String> {
        self.playlist_snapshot(source, false)
    }

    pub(crate) fn original_playlist(&self, source: &str) -> Option<String> {
        self.playlist_snapshot(source, true)
    }

    fn playlist_snapshot(&self, source: &str, original: bool) -> Option<String> {
        let mut index = self.index.lock().unwrap();
        if !self.refresh_index(&mut index) {
            return None;
        }
        let key = key(source);
        let mut visited = HashSet::new();
        let entry = index.published(&key)?;
        let playlist = entry.record.playlist.as_ref()?;
        if !index.valid_published_graph(entry, &mut visited) {
            index.clear(&key);
            return None;
        }
        for key in visited {
            if let Some(entry) = index.published_at(&key.key, &key.path)
                && let Ok(file) = File::open(&entry.path)
            {
                entry.touch(&file);
            }
        }
        Some(
            if original {
                &playlist.original
            } else {
                &playlist.canonical
            }
            .clone(),
        )
    }

    pub fn refresh(&self) -> u64 {
        let mut index = self.index.lock().unwrap();
        self.refresh_index(&mut index);
        index.generation
    }

    fn refresh_index(&self, index: &mut Index) -> bool {
        if index
            .lease
            .as_ref()
            .is_some_and(|lease| lease.is_current(&self.dir))
        {
            return true;
        }
        if index.lease.is_some() {
            tracing::warn!("cache directory ownership lost");
            index.reset();
        }
        match Self::load_index(&self.dir, self.capacity) {
            Ok(mut recovered) => {
                recovered.next_version = index.next_version;
                recovered.pins = std::mem::take(&mut index.pins);
                recovered.generation = index.generation + 1;
                *index = recovered;
                tracing::info!("cache directory recovered");
                true
            }
            Err(error) => {
                cache_warning("recover_directory", &format_args!("{error:#}"));
                false
            }
        }
    }

    pub fn open_entry(&self, entry: &Arc<Entry>, span: &Range<u64>) -> Result<(File, u64)> {
        entry.open(span).inspect_err(|error| {
            cache_warning("read_interval", error);
            let mut index = self.index.lock().unwrap();
            if !self.refresh_index(&mut index) {
                return;
            }
            let key = &entry.record.key;
            if let Some(entries) = index.entries.get_mut(key)
                && let Some(position) = entries
                    .iter()
                    .position(|indexed| Arc::ptr_eq(indexed, entry))
            {
                let entry = entries.swap_remove(position);
                if entries.is_empty() {
                    index.entries.remove(key);
                }
                entry.retired.store(true, Ordering::Release);
                remove(&entry.sidecar);
                index.retired.push((Arc::downgrade(&entry), entry.size()));
                index.clear_dependents(key);
            }
        })
    }

    pub fn metadata(&self, key: &str) -> Option<Metadata> {
        loop {
            let entry = self.index.lock().unwrap().entries.get(key)?.last()?.clone();
            if self.open_entry(&entry, &entry.record.span).is_ok() {
                return Some(entry.record.metadata.clone());
            }
        }
    }

    pub fn plan(&self, key: &str, span: Range<u64>, metadata: &Metadata) -> Vec<Piece> {
        let index = self.index.lock().unwrap();
        let Some(entries) = index.entries.get(key) else {
            return vec![Piece::Missing(span)];
        };
        if metadata.etag.is_none() {
            return entries
                .iter()
                .find(|entry| {
                    entry.record.metadata == *metadata
                        && entry.record.span.start <= span.start
                        && entry.record.span.end >= span.end
                })
                .map_or_else(
                    || vec![Piece::Missing(span.clone())],
                    |entry| vec![Piece::Cached(entry.clone(), span.clone())],
                );
        }
        let mut intervals = entries
            .iter()
            .filter(|entry| entry.record.metadata == *metadata)
            .collect::<Vec<_>>();
        intervals.sort_unstable_by_key(|entry| entry.record.span.start);
        let mut intervals = intervals.into_iter().peekable();
        let mut pieces = Vec::new();
        let mut cursor = span.start;
        while cursor < span.end {
            let mut cached = None;
            while intervals
                .peek()
                .is_some_and(|entry| entry.record.span.start <= cursor)
            {
                let entry = intervals.next().unwrap();
                if entry.record.span.end > cursor
                    && cached.is_none_or(|best: &Arc<Entry>| {
                        best.record.span.end < entry.record.span.end
                    })
                {
                    cached = Some(entry);
                }
            }
            if let Some(entry) = cached {
                let next = entry.record.span.end.min(span.end);
                pieces.push(Piece::Cached(entry.clone(), cursor..next));
                cursor = next;
            } else {
                let next = intervals
                    .peek()
                    .map_or(span.end, |entry| entry.record.span.start.min(span.end));
                pieces.push(Piece::Missing(cursor..next));
                cursor = next;
            }
        }
        pieces
    }

    fn reserve(self: &Arc<Self>, size: u64, version: &CacheVersion) -> Option<Reservation> {
        if size == 0 || size > self.capacity {
            return None;
        }
        let mut index = self.index.lock().unwrap();
        if !self.refresh_index(&mut index) || !index.is_current(version) {
            return None;
        }
        index.reserve(self.capacity, size)?;
        Some(Reservation {
            cache: self.clone(),
            size,
            generation: index.generation,
        })
    }

    pub(crate) fn whole_writer(
        self: &Arc<Self>,
        source: String,
        metadata: Metadata,
        length: Option<u64>,
        generation: u64,
    ) -> Option<WholeWriter> {
        if self.capacity == 0 || length.is_some_and(|length| length == 0 || length > self.capacity)
        {
            return None;
        }
        let mut index = self.index.lock().unwrap();
        if !self.refresh_index(&mut index) || index.generation != generation {
            return None;
        }
        let stage = self
            .stage_file()
            .inspect_err(|error| cache_warning("stage_media", error))
            .ok()?;
        drop(index);
        let reservation = Reservation {
            cache: self.clone(),
            size: 0,
            generation,
        };
        Some(WholeWriter {
            source,
            metadata,
            data: StagedInterval::open(reservation, stage, 0..0).ok()?,
        })
    }

    pub fn writer(
        self: &Arc<Self>,
        version: Arc<CacheVersion>,
        span: Range<u64>,
    ) -> Option<IntervalWriter> {
        let chunk_size = if version.metadata.etag.is_some() {
            self.capacity.min(1024 * 1024)
        } else {
            span.end - span.start
        };
        (chunk_size > 0).then(|| IntervalWriter {
            cache: self.clone(),
            version,
            chunk_size,
            end: span.end,
            next: span.start,
            chunk_end: span.start,
            chunk: None,
        })
    }

    pub fn is_current(&self, version: &Arc<CacheVersion>) -> bool {
        self.index.lock().unwrap().is_current(version)
    }

    pub fn invalidate(&self, version: &Arc<CacheVersion>) {
        let mut index = self.index.lock().unwrap();
        if index.is_current(version) {
            index.clear(&version.key);
        }
    }

    fn stage(&self, version: &CacheVersion) -> Result<Option<tempfile::NamedTempFile>> {
        let mut index = self.index.lock().unwrap();
        if !self.refresh_index(&mut index) || !index.is_current(version) {
            return Ok(None);
        }
        self.stage_file().map(Some)
    }

    fn stage_file(&self) -> Result<tempfile::NamedTempFile> {
        Ok(tempfile::Builder::new()
            .prefix(".rpc-stage-")
            .tempfile_in(self.dir.join(INTERNAL))?)
    }

    pub fn adopt(
        &self,
        source: &str,
        metadata: Metadata,
        generation: u64,
    ) -> Option<Arc<CacheVersion>> {
        let key = key(source);
        let mut index = self.index.lock().unwrap();
        if !self.refresh_index(&mut index) || index.generation != generation {
            return None;
        }
        if let Some(active) = index.active(&key)
            && active.metadata == metadata
        {
            return Some(active);
        }
        if index
            .entries
            .get(&key)
            .and_then(|entries| entries.last())
            .is_some_and(|entry| entry.record.metadata != metadata)
        {
            index.clear(&key);
        }
        Some(index.set_version(source, metadata))
    }

    pub fn version_for_patch(&self, key: &str, metadata: &Metadata) -> Option<Arc<CacheVersion>> {
        let mut index = self.index.lock().unwrap();
        if let Some(active) = index.active(key) {
            return (active.metadata == *metadata).then_some(active);
        }
        let last = index.entries.get(key)?.last()?;
        let source = last.record.source.clone();
        (last.record.metadata == *metadata).then(|| index.set_version(&source, metadata.clone()))
    }

    pub fn clear(&self, key: &str) {
        self.index.lock().unwrap().clear_group(key);
        tracing::info!(cache_key = key, "cache cleared");
    }
}

impl Reservation {
    fn commit(
        mut self,
        stage: tempfile::NamedTempFile,
        span: Range<u64>,
        version: Arc<CacheVersion>,
    ) -> Result<()> {
        let name = stage
            .path()
            .file_name()
            .and_then(|value| value.to_str())
            .context("invalid staging name")?;
        let id = name
            .strip_prefix(".rpc-stage-")
            .context("invalid staging prefix")?;
        let sidecar_path = self.cache.dir.join(INTERNAL).join(format!("{id}.json"));
        let path = sidecar_path.with_extension("part");
        let record = Record {
            source: version.source.clone(),
            published: None,
            modified: None,
            playlist: None,
            key: version.key.clone(),
            span,
            metadata: version.metadata.clone(),
        };
        ensure!(
            record.span.end - record.span.start == stage.as_file().metadata()?.len(),
            "staged length mismatch"
        );
        stage.as_file().sync_data()?;
        let Some(mut sidecar) = self.cache.stage(&version)? else {
            return Ok(());
        };
        serde_json::to_writer(&mut sidecar, &record)?;
        sidecar.flush()?;
        sidecar.as_file().sync_data()?;
        let mut index = self.cache.index.lock().unwrap();
        if !self.cache.refresh_index(&mut index) || !index.is_current(&version) {
            return Ok(());
        }
        stage.persist_noclobber(&path)?;
        if let Err(error) = sidecar.persist_noclobber(&sidecar_path) {
            remove(&path);
            return Err(error.into());
        }
        index.reserved -= self.size;
        self.size = 0;
        tracing::debug!(
            start = record.span.start,
            end_exclusive = record.span.end,
            "cache interval published"
        );
        index.insert(Entry {
            record,
            path,
            sidecar: sidecar_path,
            used: Mutex::new(SystemTime::now()),
            retired: AtomicBool::new(false),
        });
        drop(index);
        if let Err(error) = publish_complete(&self.cache, &version) {
            cache_warning("publish_media", &format_args!("{error:#}"));
        }
        let mut index = self.cache.index.lock().unwrap();
        if self.cache.refresh_index(&mut index) {
            index.publish_playlists(&self.cache.dir, self.cache.capacity);
        }
        Ok(())
    }
}

impl Index {
    fn reset(&mut self) {
        *self = Self {
            next_version: self.next_version,
            pins: std::mem::take(&mut self.pins),
            generation: self.generation + 1,
            ..Self::default()
        };
    }

    fn active(&self, key: &str) -> Option<Arc<CacheVersion>> {
        self.versions.get(key)?.upgrade()
    }

    fn is_current(&self, version: &CacheVersion) -> bool {
        self.active(&version.key)
            .is_some_and(|active| std::ptr::eq(active.as_ref(), version))
    }

    fn set_version(&mut self, source: &str, metadata: Metadata) -> Arc<CacheVersion> {
        self.versions
            .retain(|_, version| version.strong_count() > 0);
        let key = key(source);
        let version = Arc::new(CacheVersion {
            source: source.to_owned(),
            key: key.clone(),
            metadata,
            id: self.next_version,
        });
        self.next_version += 1;
        self.versions.insert(key, Arc::downgrade(&version));
        version
    }

    fn clear_group(&mut self, key: &str) {
        let dependencies = self
            .published(key)
            .and_then(|entry| entry.record.playlist.as_ref())
            .map(|playlist| playlist.dependencies.clone())
            .unwrap_or_default();
        self.clear(key);
        for dependency in dependencies {
            let shared = self
                .entries
                .values()
                .flatten()
                .filter_map(|entry| entry.record.playlist.as_ref())
                .any(|playlist| {
                    playlist
                        .dependencies
                        .iter()
                        .any(|item| item.key == dependency.key)
                });
            if !shared && !self.pins.contains_key(&dependency.key) {
                self.clear_group(&dependency.key);
            }
        }
    }

    fn clear_dependents(&mut self, key: &str) {
        let dependents: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entries)| {
                entries.iter().any(|entry| {
                    entry.record.playlist.as_ref().is_some_and(|playlist| {
                        playlist.dependencies.iter().any(|item| item.key == key)
                    })
                })
            })
            .map(|(candidate, _)| candidate.clone())
            .collect();
        for dependent in dependents {
            self.clear(&dependent);
        }
    }

    fn clear(&mut self, key: &str) {
        let entries = self.entries.remove(key).unwrap_or_default();
        self.clear_dependents(key);
        self.versions.remove(key);
        for entry in entries {
            if entry.record.playlist.is_some() {
                self.playlists.remove(&entry.record.source);
            }
            entry.retired.store(true, Ordering::Release);
            remove(&entry.sidecar);
            if entry.record.published.is_some() && entry.matches_published_file() {
                remove(&entry.path);
            }
            self.retired.push((Arc::downgrade(&entry), entry.size()));
        }
    }

    fn published(&self, key: &str) -> Option<&Arc<Entry>> {
        self.entries.get(key)?.iter().find(|entry| {
            entry.record.published.is_some() && entry.record.span == (0..entry.record.metadata.size)
        })
    }

    fn published_at(&self, key: &str, path: &Path) -> Option<&Arc<Entry>> {
        self.entries
            .get(key)?
            .iter()
            .find(|entry| entry.record.published.as_deref() == Some(path))
    }

    fn valid_published_graph(
        &self,
        entry: &Entry,
        visited: &mut HashSet<PublishedDependency>,
    ) -> bool {
        let Some(path) = &entry.record.published else {
            return false;
        };
        if !visited.insert(PublishedDependency {
            key: entry.record.key.clone(),
            path: path.clone(),
        }) {
            return true;
        }
        entry.matches_published_file()
            && entry.record.playlist.as_ref().is_none_or(|playlist| {
                playlist.dependencies.iter().all(|dependency| {
                    self.published_at(&dependency.key, &dependency.path)
                        .is_some_and(|entry| self.valid_published_graph(entry, visited))
                })
            })
    }

    fn publish_playlists(&mut self, root: &Path, capacity: u64) {
        let mut changed = true;
        while changed {
            changed = false;
            let sources: Vec<_> = self.playlists.keys().cloned().collect();
            for source in sources {
                if publish_playlist(root, self, capacity, &source).unwrap_or_else(|error| {
                    cache_warning("publish_playlist", &format_args!("{error:#}"));
                    false
                }) {
                    if !self
                        .playlists
                        .get(&source)
                        .is_some_and(|(_, playlist)| hls::is_master(playlist))
                    {
                        self.playlists.remove(&source);
                    }
                    changed = true;
                }
            }
        }
    }

    fn remember_playlist(&mut self, source: &str, canonical: String) {
        if self.playlists.len() >= 128
            && !self.playlists.contains_key(source)
            && let Some(oldest) = self
                .playlists
                .iter()
                .filter(|(source, _)| !self.pins.contains_key(&key(source)))
                .min_by_key(|(_, (seen, _))| *seen)
                .map(|(source, _)| source.clone())
        {
            self.playlists.remove(&oldest);
        }
        self.playlists
            .insert(source.to_owned(), (Instant::now(), canonical));
    }

    fn insert(&mut self, entry: Entry) {
        self.size += entry.size();
        self.entries
            .entry(entry.record.key.clone())
            .or_default()
            .push(Arc::new(entry));
    }

    fn reserve(&mut self, capacity: u64, size: u64) -> Option<()> {
        let remaining = capacity.checked_sub(size.checked_add(self.reserved)?)?;
        self.evict(remaining);
        if self.size > remaining {
            return None;
        }
        self.reserved += size;
        Some(())
    }

    fn reap_retired(&mut self) {
        self.retired.retain(|(entry, size)| {
            if entry.strong_count() > 0 {
                return true;
            }
            self.size -= size;
            false
        });
    }

    fn evict(&mut self, capacity: u64) {
        self.reap_retired();
        if self.size <= capacity {
            return;
        }
        while self.size > capacity {
            let dependencies: HashSet<_> = self
                .entries
                .values()
                .flatten()
                .filter_map(|entry| entry.record.playlist.as_ref())
                .flat_map(|playlist| playlist.dependencies.iter().map(|item| &item.key))
                .collect();
            let Some((key, position)) = self
                .entries
                .iter()
                .flat_map(|(key, entries)| {
                    entries
                        .iter()
                        .enumerate()
                        .map(move |(position, entry)| (key, position, entry))
                })
                .filter(|(key, _, entry)| {
                    Arc::strong_count(entry) == 1
                        && !dependencies.contains(key)
                        && !self.pins.contains_key(*key)
                })
                .min_by_key(|(_, _, entry)| *entry.used.lock().unwrap())
                .map(|(key, position, _)| (key.clone(), position))
            else {
                break;
            };
            if self.entries[&key][position].record.playlist.is_some() {
                self.clear_group(&key);
                self.reap_retired();
                continue;
            }
            let entries = self.entries.get_mut(&key).unwrap();
            let entry = entries.swap_remove(position);
            if entries.is_empty() {
                self.entries.remove(&key);
            }
            self.size -= entry.size();
            delete(&entry);
        }
    }
}

pub fn key(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}

fn ensure_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(path)?.is_dir(),
                "not a cache directory"
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn load(root: &Path, sidecar: &Path) -> Result<Entry> {
    let record: Record = serde_json::from_reader(File::open(sidecar)?)?;
    ensure!(key(&record.source) == record.key, "invalid cache key");
    let path = if let Some(relative) = &record.published {
        ensure!(relative.components().count() >= 2, "invalid published path");
        let mut path = root.to_path_buf();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                anyhow::bail!("invalid published path");
            };
            path.push(name);
            ensure!(
                !fs::symlink_metadata(&path)?.is_symlink(),
                "symlink in published path"
            );
        }
        path
    } else {
        sidecar.with_extension("part")
    };
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        record.span.start < record.span.end && record.span.end <= record.metadata.size,
        "invalid span"
    );
    ensure!(
        metadata.is_file()
            && metadata.len() == record.span.end - record.span.start
            && record
                .modified
                .is_none_or(|time| metadata.modified().ok() == Some(time)),
        "invalid cache file"
    );
    let used = if record.published.is_some() {
        fs::metadata(sidecar)?.modified()?
    } else {
        metadata.modified()?
    };
    Ok(Entry {
        record,
        path,
        sidecar: sidecar.to_path_buf(),
        used: Mutex::new(used),
        retired: AtomicBool::new(false),
    })
}

fn delete(entry: &Entry) {
    remove(&entry.sidecar);
    if entry.record.published.is_none() || entry.matches_published_file() {
        remove(&entry.path);
    }
}

fn remove(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        cache_warning("remove_file", &error);
    }
}

pub struct IntervalWriter {
    cache: Arc<Cache>,
    version: Arc<CacheVersion>,
    chunk_size: u64,
    end: u64,
    next: u64,
    chunk_end: u64,
    chunk: Option<StagedInterval>,
}

impl IntervalWriter {
    pub async fn write(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.next == self.chunk_end {
                self.chunk_end = self.next.saturating_add(self.chunk_size).min(self.end);
                self.chunk =
                    StagedInterval::new(&self.cache, self.next..self.chunk_end, &self.version);
            }
            let size = (self.chunk_end - self.next).min(data.len() as u64) as usize;
            if let Some(chunk) = &mut self.chunk
                && let Err(error) = chunk.file.write_all(&data[..size]).await
            {
                cache_warning("write_interval", &error);
                self.chunk = None;
            }
            self.next += size as u64;
            data = &data[size..];
            if self.next == self.chunk_end
                && self.next != self.end
                && let Some(chunk) = self.chunk.take()
            {
                chunk.commit(self.version.clone()).await;
            }
        }
    }

    pub async fn finish(self) {
        if let Some(chunk) = self.chunk {
            chunk.commit(self.version).await;
        }
    }
}

pub(crate) struct WholeWriter {
    source: String,
    metadata: Metadata,
    data: StagedInterval,
}

impl WholeWriter {
    pub(crate) async fn write(&mut self, bytes: &[u8]) -> Option<()> {
        let reservation = &mut self.data.reservation;
        let cache = &reservation.cache;
        {
            let mut index = cache.index.lock().unwrap();
            if !cache.refresh_index(&mut index) || reservation.generation != index.generation {
                return None;
            }
            index.reserve(cache.capacity, bytes.len() as u64)?;
            reservation.size += bytes.len() as u64;
        }
        self.data
            .file
            .write_all(bytes)
            .await
            .inspect_err(|error| cache_warning("write_media", error))
            .ok()?;
        self.data.span.end = reservation.size;
        Some(())
    }

    pub(crate) async fn finish(mut self) {
        let reservation = &self.data.reservation;
        self.metadata.size = reservation.size;
        if self.metadata.size > 0
            && let Some(version) =
                reservation
                    .cache
                    .adopt(&self.source, self.metadata, reservation.generation)
        {
            self.data.commit(version).await;
        }
    }
}

struct StagedInterval {
    reservation: Reservation,
    stage: tempfile::NamedTempFile,
    file: AsyncFile,
    span: Range<u64>,
}

impl StagedInterval {
    fn new(cache: &Arc<Cache>, span: Range<u64>, version: &CacheVersion) -> Option<Self> {
        let reservation = cache.reserve(span.end - span.start, version)?;
        let stage = cache
            .stage(version)
            .inspect_err(|error| cache_warning("stage_interval", error))
            .ok()??;
        Self::open(reservation, stage, span)
            .inspect_err(|error| cache_warning("open_staging_file", error))
            .ok()
    }

    fn open(
        reservation: Reservation,
        stage: tempfile::NamedTempFile,
        span: Range<u64>,
    ) -> Result<Self> {
        let file = AsyncFile::from_std(stage.reopen()?);
        Ok(Self {
            reservation,
            stage,
            file,
            span,
        })
    }

    async fn commit(mut self, version: Arc<CacheVersion>) {
        if let Err(error) = self.file.flush().await {
            cache_warning("flush_interval", &error);
            return;
        }
        drop(self.file.into_std().await);
        let span = tracing::Span::current();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| self.reservation.commit(self.stage, self.span, version))
        })
        .await;
        if let Err(error) = result
            .context("cache commit task")
            .and_then(|result| result)
        {
            cache_warning("commit_interval", &format_args!("{error:#}"));
        }
    }
}

struct DirectoryLease {
    lock: File,
    #[cfg(not(unix))]
    marker: tempfile::NamedTempFile,
}

impl DirectoryLease {
    fn acquire(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).context("create cache directory")?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(".lock"))
            .context("open cache lock")?;
        #[cfg(not(target_os = "android"))]
        lock.try_lock()
            .context("cannot acquire cache directory lock")?;
        #[cfg(target_os = "android")]
        {
            use std::os::fd::AsRawFd;
            // Rust's File::try_lock is unsupported on Android. The descriptor
            // retains this nonblocking flock until DirectoryLease is dropped.
            let status = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if status != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("cannot acquire cache directory lock");
            }
        }
        let lease = Self {
            lock,
            #[cfg(not(unix))]
            marker: tempfile::Builder::new()
                .prefix(".rpc-lease-")
                .tempfile_in(dir)?,
        };
        ensure!(lease.is_current(dir), "cache lock path changed");
        Ok(lease)
    }

    fn is_current(&self, dir: &Path) -> bool {
        let Ok(current) = fs::metadata(dir.join(".lock")) else {
            return false;
        };
        let Ok(held) = self.lock.metadata() else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            (current.dev(), current.ino()) == (held.dev(), held.ino())
        }
        #[cfg(not(unix))]
        {
            self.marker.path().is_file() && current.created().ok() == held.created().ok()
        }
    }
}

fn cache_warning(operation: &'static str, error: &dyn std::fmt::Display) {
    static LAST: std::sync::LazyLock<Mutex<HashMap<&'static str, Instant>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    let mut last = LAST.lock().unwrap();
    if last
        .get(operation)
        .is_none_or(|previous| now.duration_since(*previous) >= Duration::from_secs(10))
    {
        last.insert(operation, now);
        tracing::warn!(operation, %error, "cache operation failed");
    }
}
