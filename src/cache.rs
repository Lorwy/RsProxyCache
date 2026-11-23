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
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};
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

pub struct CacheVersion {
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
    key: String,
    span: Range<u64>,
    metadata: Metadata,
}

pub struct Entry {
    record: Record,
    path: PathBuf,
    used: Mutex<SystemTime>,
    retired: AtomicBool,
}

impl Entry {
    fn size(&self) -> u64 {
        self.record.span.end - self.record.span.start
    }

    pub fn open(&self, span: &Range<u64>) -> Result<(File, u64)> {
        let file = File::open(&self.path)?;
        let now = SystemTime::now();
        *self.used.lock().unwrap() = now;
        let _ = file.set_modified(now);
        Ok((file, span.start - self.record.span.start))
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if *self.retired.get_mut() {
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
    _lock: File,
}

#[derive(Default)]
struct Index {
    entries: HashMap<String, Vec<Arc<Entry>>>,
    size: u64,
    reserved: u64,
    versions: HashMap<String, Weak<CacheVersion>>,
    next_version: u64,
    retired: Vec<(Weak<Entry>, u64)>,
}

struct Reservation {
    cache: Arc<Cache>,
    size: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.cache.index.lock().unwrap().reserved -= self.size;
    }
}

impl Cache {
    pub fn open(dir: PathBuf, capacity: u64) -> Result<Arc<Self>> {
        fs::create_dir_all(&dir)?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(".lock"))?;
        lock.try_lock().context("cache directory already in use")?;
        let mut index = Index::default();
        for item in fs::read_dir(&dir)? {
            let path = item?.path();
            if path.extension().is_some_and(|ext| ext == "json") && owned(&path) {
                if let Ok(entry) = load(&path) {
                    index.insert(entry);
                } else {
                    remove(&path);
                    remove(&path.with_extension("data"));
                }
            }
        }
        for item in fs::read_dir(&dir)? {
            let path = item?.path();
            if (owned(&path)
                && path.extension().is_some_and(|ext| ext == "data")
                && !path.with_extension("json").is_file())
                || path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".rpc-stage-"))
            {
                remove(&path);
            }
        }
        index.evict(capacity);
        Ok(Arc::new(Self {
            dir,
            capacity,
            index: Mutex::new(index),
            _lock: lock,
        }))
    }

    pub fn metadata(&self, key: &str) -> Option<Metadata> {
        self.index
            .lock()
            .unwrap()
            .entries
            .get(key)?
            .last()
            .map(|entry| entry.record.metadata.clone())
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

    fn reserve(self: &Arc<Self>, size: u64) -> Option<Reservation> {
        if size == 0 || size > self.capacity {
            return None;
        }
        let mut index = self.index.lock().unwrap();
        let remaining = self
            .capacity
            .checked_sub(size.checked_add(index.reserved)?)?;
        index.evict(remaining);
        if index.size > remaining {
            return None;
        }
        index.reserved += size;
        Some(Reservation {
            cache: self.clone(),
            size,
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

    fn stage(&self) -> Result<tempfile::NamedTempFile> {
        Ok(tempfile::Builder::new()
            .prefix(".rpc-stage-")
            .tempfile_in(&self.dir)?)
    }

    pub fn adopt(&self, key: &str, metadata: Metadata) -> Arc<CacheVersion> {
        let mut index = self.index.lock().unwrap();
        if let Some(active) = index.active(key)
            && active.metadata == metadata
        {
            return active;
        }
        if index
            .entries
            .get(key)
            .and_then(|entries| entries.last())
            .is_some_and(|entry| entry.record.metadata != metadata)
        {
            index.clear(key);
        }
        index.set_version(key, metadata)
    }

    pub fn version_for_patch(&self, key: &str, metadata: &Metadata) -> Option<Arc<CacheVersion>> {
        let mut index = self.index.lock().unwrap();
        if let Some(active) = index.active(key) {
            return (active.metadata == *metadata).then_some(active);
        }
        let last = index.entries.get(key)?.last()?;
        (last.record.metadata == *metadata).then(|| index.set_version(key, metadata.clone()))
    }

    pub fn clear(&self, key: &str) {
        self.index.lock().unwrap().clear(key);
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
        let path = self.cache.dir.join(format!("rpc-{id}.data"));
        let record = Record {
            key: version.key.clone(),
            span,
            metadata: version.metadata.clone(),
        };
        ensure!(
            record.span.end - record.span.start == stage.as_file().metadata()?.len(),
            "staged length mismatch"
        );
        stage.as_file().sync_data()?;
        let mut sidecar = self.cache.stage()?;
        serde_json::to_writer(&mut sidecar, &record)?;
        sidecar.flush()?;
        sidecar.as_file().sync_data()?;
        let mut index = self.cache.index.lock().unwrap();
        if !index.is_current(&version) {
            return Ok(());
        }
        stage.persist_noclobber(&path)?;
        if let Err(error) = sidecar.persist_noclobber(path.with_extension("json")) {
            remove(&path);
            return Err(error.into());
        }
        index.reserved -= self.size;
        self.size = 0;
        index.insert(Entry {
            record,
            path,
            used: Mutex::new(SystemTime::now()),
            retired: AtomicBool::new(false),
        });
        Ok(())
    }
}

impl Index {
    fn active(&self, key: &str) -> Option<Arc<CacheVersion>> {
        self.versions.get(key)?.upgrade()
    }

    fn is_current(&self, version: &Arc<CacheVersion>) -> bool {
        self.active(&version.key)
            .is_some_and(|active| Arc::ptr_eq(&active, version))
    }

    fn set_version(&mut self, key: &str, metadata: Metadata) -> Arc<CacheVersion> {
        self.versions
            .retain(|_, version| version.strong_count() > 0);
        let version = Arc::new(CacheVersion {
            key: key.to_owned(),
            metadata,
            id: self.next_version,
        });
        self.next_version += 1;
        self.versions
            .insert(key.to_owned(), Arc::downgrade(&version));
        version
    }

    fn clear(&mut self, key: &str) {
        self.versions.remove(key);
        for entry in self.entries.remove(key).unwrap_or_default() {
            remove(&entry.path.with_extension("json"));
            entry.retired.store(true, Ordering::Relaxed);
            self.retired.push((Arc::downgrade(&entry), entry.size()));
        }
    }

    fn insert(&mut self, entry: Entry) {
        self.size += entry.size();
        self.entries
            .entry(entry.record.key.clone())
            .or_default()
            .push(Arc::new(entry));
    }

    fn evict(&mut self, capacity: u64) {
        self.retired.retain(|(entry, size)| {
            if entry.strong_count() > 0 {
                return true;
            }
            self.size -= size;
            false
        });
        if self.size <= capacity {
            return;
        }
        let mut candidates: Vec<_> = self
            .entries
            .values()
            .flatten()
            .filter(|entry| Arc::strong_count(entry) == 1)
            .map(|entry| {
                (
                    *entry.used.lock().unwrap(),
                    entry.path.clone(),
                    entry.size(),
                )
            })
            .collect();
        candidates.sort_unstable();
        let mut paths = HashSet::new();
        for (_, path, size) in candidates {
            if self.size <= capacity {
                break;
            }
            self.size -= size;
            paths.insert(path);
        }
        self.entries.retain(|_, entries| {
            entries.retain(|entry| {
                if paths.contains(&entry.path) {
                    delete(entry);
                    false
                } else {
                    true
                }
            });
            !entries.is_empty()
        });
    }
}

pub fn key(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}

fn load(path: &Path) -> Result<Entry> {
    let record: Record = serde_json::from_reader(File::open(path)?)?;
    let path = path.with_extension("data");
    let metadata = fs::metadata(&path)?;
    ensure!(
        record.span.start < record.span.end && record.span.end <= record.metadata.size,
        "invalid span"
    );
    ensure!(
        metadata.len() == record.span.end - record.span.start,
        "invalid cache file length"
    );
    Ok(Entry {
        record,
        path,
        used: Mutex::new(metadata.modified()?),
        retired: AtomicBool::new(false),
    })
}

fn owned(path: &Path) -> bool {
    path.file_stem()
        .is_some_and(|name| name.to_string_lossy().starts_with("rpc-"))
}

fn delete(entry: &Entry) {
    remove(&entry.path.with_extension("json"));
    remove(&entry.path);
}

fn remove(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("cache cleanup failed: {error}");
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
                self.chunk = StagedInterval::new(&self.cache, self.next..self.chunk_end);
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

struct StagedInterval {
    reservation: Reservation,
    stage: tempfile::NamedTempFile,
    file: AsyncFile,
    span: Range<u64>,
}

impl StagedInterval {
    fn new(cache: &Arc<Cache>, span: Range<u64>) -> Option<Self> {
        let reservation = cache.reserve(span.end - span.start)?;
        let stage = cache.stage().ok()?;
        let file = AsyncFile::from_std(stage.reopen().ok()?);
        Some(Self {
            reservation,
            stage,
            file,
            span,
        })
    }

    async fn commit(mut self, version: Arc<CacheVersion>) {
        if self.file.flush().await.is_err() {
            return;
        }
        drop(self.file.into_std().await);
        let _ = tokio::task::spawn_blocking(move || {
            self.reservation.commit(self.stage, self.span, version)
        })
        .await;
    }
}
