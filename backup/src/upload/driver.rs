//! The [`Uploader`]: the step function behind [`BackupDriver`].

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use kvik_files::catalogue::{Catalogue, FileEntry, Tier};
use kvik_files::desc::{FileDescription, FileRef, kind};
use kvik_files::record::{Body, Record};
use kvik_files::tier::{CopyVerified, DeepCopy};
use kvik_log::fs::OpenMode;
use kvik_log::name::SegmentName;
use kvik_log::store::{LOG_DIR, STORE};
use kvik_sink_api::{BackupSink, SinkError, Stored, Verified, VerifyError};
use sha2::{Digest, Sha256};

use super::{BackupDriver, Progress, Refused, Step, UploadConfig, UploadError};
use crate::envelope::{Envelope, EnvelopeError};
use crate::keys::{Keys, ObjectName};
use crate::port::ManifestPort;
use crate::{DriverFuture, LocalStore};

/// Owner's ruling (Q-82, Q-284): an object is lost after this many failed lookups while other calls answer...
const LOST_LOOKUPS: u32 = 3;
/// ...spread over at least this long.
const LOST_OVER: Duration = Duration::from_secs(600);

/// The upload driver over a sink, an envelope and the manifest port.
#[derive(Debug)]
pub struct Uploader<S, E, M> {
    pub(super) sink: S,
    pub(super) envelope: E,
    pub(super) keys: Keys,
    local: LocalStore,
    manifest: M,
    cfg: UploadConfig,
    store_done: bool,
    cursor: Option<Cursor>,
    sizes: BTreeMap<FileRef, u64>,
    attempt: u32,
    awaiting: Option<Awaiting>,
    progress: Progress,
}

/// Where the record uploads stand: read from the bucket at the first step, then kept (Q-282).
#[derive(Debug)]
struct Cursor {
    /// The highest epoch uploaded; every record at or below it is in the bucket.
    uploaded: u64,
    /// The highest epoch read from the manifest.
    scanned: u64,
    /// Synced records above `uploaded`, in epoch order.
    pending: VecDeque<Record>,
    /// The epoch of each file's copy-verified record written since the last rebuild record.
    copied_at: BTreeMap<FileRef, u64>,
}

/// An object `put` but not yet reported by the bucket (Q-284).
#[derive(Debug)]
struct Awaiting {
    key: String,
    expected: Stored,
    lookups: u32,
    since: Duration,
}

/// Why a step stopped short.
pub(super) enum Stop {
    Retry(SinkError),
    Fail(UploadError),
}

type Out<T> = Result<T, Stop>;

pub(super) fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

fn sink_stop(e: SinkError) -> Stop {
    if e.is_transient() {
        Stop::Retry(e)
    } else {
        Stop::Fail(UploadError::Verify(VerifyError::Sink(e)))
    }
}

/// A `put` that may have been damaged in transit is repeated; other content under the key is not (Q-284).
fn put_stop(e: SinkError) -> Stop {
    match e {
        SinkError::HashMismatch { .. } => Stop::Retry(e),
        e => sink_stop(e),
    }
}

fn verify_stop(e: VerifyError) -> Stop {
    match e {
        VerifyError::Sink(e) => sink_stop(e),
        e => Stop::Fail(UploadError::Verify(e)),
    }
}

fn port(e: crate::port::PortError) -> Stop {
    Stop::Fail(UploadError::Port(e))
}

fn io_stop(e: io::Error) -> Stop {
    Stop::Fail(UploadError::Io(e))
}

/// The files a record names that must be copied before it goes up (Q-282).
fn names(r: &Record) -> Vec<FileRef> {
    match &r.body {
        Body::Publish(p) => p.published.iter().map(FileDescription::file_ref).collect(),
        Body::Sealed(s) => s.iter().map(FileDescription::file_ref).collect(),
        _ => Vec::new(),
    }
}

fn copied(e: &FileEntry) -> bool {
    e.verified.contains(&Tier::Deep)
}

/// Whether every live file `r` names is verified in deep.
fn ready(r: &Record, snap: &Catalogue) -> bool {
    names(r)
        .into_iter()
        .all(|f| snap.file(f).is_none_or(|e| !e.is_live() || copied(e)))
}

/// The upload queue: live files not verified in deep, in ascending position (Q-280).
fn queue(snap: &Catalogue) -> Vec<&FileEntry> {
    let mut q: Vec<&FileEntry> = snap.live().filter(|e| !copied(e)).collect();
    q.sort_by_key(|e| (e.description.range.map(|r| r.0), e.description.file_ref()));
    q
}

/// A fresh copy-verified record: one written since the last rebuild record, so it carries no field 100.
fn fresh_copy(r: &Record) -> Option<FileRef> {
    match CopyVerified::from_body(&r.body) {
        Some(Ok(c)) if r.old_epoch.is_none() && c.tier == Tier::Deep => Some(c.file.file_ref()),
        _ => None,
    }
}

impl<S: BackupSink, E: Envelope, M: ManifestPort> Uploader<S, E, M> {
    /// A driver uploading the store in `local` through `manifest`.
    pub fn new(
        sink: S,
        envelope: E,
        keys: Keys,
        local: LocalStore,
        manifest: M,
        cfg: UploadConfig,
    ) -> Self {
        Self {
            sink,
            envelope,
            keys,
            local,
            manifest,
            cfg,
            store_done: false,
            cursor: None,
            sizes: BTreeMap::new(),
            attempt: 0,
            awaiting: None,
            progress: Progress::default(),
        }
    }

    async fn run(&mut self, now: Duration) -> Step {
        if self.manifest.holds() {
            return Step::Held;
        }
        match self.advance(now).await {
            Ok(moved) => {
                self.attempt = 0;
                self.progress.failing_since = None;
                if moved { Step::Progress } else { Step::Idle }
            }
            Err(Stop::Retry(error)) => {
                let after = self
                    .cfg
                    .retry_first
                    .saturating_mul(1u32.checked_shl(self.attempt).unwrap_or(u32::MAX))
                    .min(self.cfg.retry_max);
                self.attempt = self.attempt.saturating_add(1);
                self.progress.retries = self.progress.retries.saturating_add(1);
                self.progress.failing_since.get_or_insert(now);
                self.progress.last_error = Some(error.to_string());
                Step::Retry { after, error }
            }
            Err(Stop::Fail(e)) => {
                self.progress.failing_since.get_or_insert(now);
                self.progress.last_error = Some(e.to_string());
                Step::Failed(e)
            }
        }
    }

    /// One object at most; `Ok(false)` when nothing is left to do now.
    async fn advance(&mut self, now: Duration) -> Out<bool> {
        if !self.store_done && self.upload_store(now).await? {
            let snap = self.manifest.snapshot();
            self.update_progress(&snap);
            return Ok(true);
        }
        let snap = self.manifest.snapshot();
        if self.cursor.is_none() {
            let c = self.open_cursor(&snap).await?;
            self.cursor = Some(c);
        }
        self.refresh(&snap)?;
        let moved = self.next_object(&snap, now).await?;
        let snap = self.manifest.snapshot();
        self.refresh(&snap)?;
        self.update_progress(&snap);
        Ok(moved)
    }

    async fn next_object(&mut self, snap: &Catalogue, now: Duration) -> Out<bool> {
        let front = self.cursor.as_ref().and_then(|c| c.pending.front());
        if let Some(r) = front
            && ready(r, snap)
            // The last record is settled once: an empty last record waits for a successor (spec §7 Manifest).
            && !(r.epoch == snap.last_epoch() && r.body == Body::Empty)
        {
            let r = r.clone();
            self.upload_record(&r, now).await?;
            return Ok(true);
        }
        let Some(e) = queue(snap).first().map(|e| (*e).clone()) else {
            return Ok(false);
        };
        self.upload_file(&e, now).await?;
        Ok(true)
    }

    /// `STORE` before any other object (spec §8 Deep). `Ok(true)` if it was uploaded now.
    async fn upload_store(&mut self, now: Duration) -> Out<bool> {
        let key = self.keys.store();
        if self.looked_up(&key, now).await?.is_some() {
            self.store_done = true;
            return Ok(true);
        }
        if self.sink.stat(&key).await.map_err(sink_stop)?.is_some() {
            self.store_done = true;
            return Ok(false);
        }
        let plain = self.read_local(self.local.dir.join(STORE))?;
        let sealed = self.seal(&key, &plain)?;
        self.put_verified(&key, sealed, now).await?;
        self.store_done = true;
        Ok(true)
    }

    /// The record cursor from a listing of this generation's records (Q-282): a listing that lags only repeats
    /// `put`s of identical bytes.
    async fn open_cursor(&mut self, snap: &Catalogue) -> Out<Cursor> {
        let g = snap.generation();
        let prefix = self.keys.records_prefix(g);
        let mut uploaded = 0u64;
        let mut after: Option<String> = None;
        loop {
            let page = self
                .sink
                .list(&prefix, after.as_deref())
                .await
                .map_err(sink_stop)?;
            for k in &page.keys {
                if let Some(ObjectName::Record { generation, epoch }) = self.keys.parse(k)
                    && generation == g
                {
                    uploaded = uploaded.max(epoch);
                }
            }
            match page.keys.last() {
                Some(last) if page.more => after = Some(last.clone()),
                _ => break,
            }
        }
        let mut c = Cursor {
            uploaded,
            scanned: 0,
            pending: VecDeque::new(),
            copied_at: BTreeMap::new(),
        };
        take(&mut c, self.manifest.records_after(0).map_err(port)?, snap);
        Ok(c)
    }

    fn refresh(&mut self, snap: &Catalogue) -> Out<()> {
        let Some(c) = self.cursor.as_mut() else {
            return Ok(());
        };
        if c.scanned >= snap.last_epoch() {
            return Ok(());
        }
        let records = self.manifest.records_after(c.scanned).map_err(port)?;
        take(c, records, snap);
        Ok(())
    }

    async fn upload_record(&mut self, r: &Record, now: Duration) -> Out<()> {
        self.manifest.settle(r.epoch).map_err(port)?;
        let key = self.keys.record(r.generation, r.epoch);
        if self.looked_up(&key, now).await?.is_none() {
            let plain = r
                .encode()
                .map_err(|e| io_stop(io::Error::new(io::ErrorKind::InvalidData, e)))?;
            let sealed = self.seal(&key, &plain)?;
            self.put_verified(&key, sealed, now).await?;
        }
        if let Some(c) = self.cursor.as_mut() {
            c.pending.pop_front();
            c.uploaded = c.uploaded.max(r.epoch);
        }
        Ok(())
    }

    async fn upload_file(&mut self, e: &FileEntry, now: Duration) -> Out<()> {
        let d = &e.description;
        let key = self.keys.file(d);
        if let Some(v) = self.looked_up(&key, now).await? {
            self.append_copy(d, v.stored())?;
            self.count_upload(v.stored().size);
            return Ok(());
        }
        // A copy recorded before a rebuild is verified again by its reported hash, without a new upload (Q-280).
        if let Some(c) = &e.deep
            && self.awaiting.is_none()
        {
            let want = Stored::new(c.size, c.sha256);
            if self.sink.stat(&key).await.map_err(sink_stop)? == Some(want.clone()) {
                return self.append_copy(d, &want);
            }
        }
        let path = self.local_path(d)?;
        let plain = self.read_local(path)?;
        if sha256(&plain) != d.hash {
            return Err(Stop::Fail(UploadError::LocalHash { file: d.file_ref() }));
        }
        let sealed = self.seal(&key, &plain)?;
        let v = self.put_verified(&key, sealed, now).await?;
        self.append_copy(d, v.stored())?;
        self.count_upload(v.stored().size);
        Ok(())
    }

    fn count_upload(&mut self, size: u64) {
        self.progress.uploaded_files = self.progress.uploaded_files.saturating_add(1);
        self.progress.uploaded_bytes = self.progress.uploaded_bytes.saturating_add(size);
    }

    fn append_copy(&mut self, file: &FileDescription, s: &Stored) -> Out<()> {
        let c = CopyVerified {
            file: file.clone(),
            tier: Tier::Deep,
            copy: DeepCopy {
                size: s.size,
                sha256: s.sha256,
            },
        };
        self.manifest.append(c.to_body()).map_err(port)?;
        Ok(())
    }

    fn seal(&self, key: &str, plain: &[u8]) -> Out<Vec<u8>> {
        let sealed = self
            .envelope
            .seal(key, plain)
            .map_err(|e| Stop::Fail(e.into()))?;
        let size = sealed.len() as u64;
        if size > self.cfg.max_object {
            return Err(Stop::Fail(EnvelopeError::TooLarge { size }.into()));
        }
        Ok(sealed)
    }

    async fn put_verified(&mut self, key: &str, sealed: Vec<u8>, now: Duration) -> Out<Verified> {
        let expected = Stored::new(sealed.len() as u64, sha256(&sealed));
        self.awaiting = None;
        self.sink
            .put(key, sealed.into(), expected.sha256)
            .await
            .map_err(put_stop)?;
        match Verified::check(&self.sink, key, &expected).await {
            Ok(v) => Ok(v),
            Err(VerifyError::Reported { reported: None, .. }) => {
                self.awaiting = Some(Awaiting {
                    key: key.into(),
                    expected,
                    lookups: 0,
                    since: now,
                });
                self.missing(key).await
            }
            Err(e) => Err(verify_stop(e)),
        }
    }

    /// For an object awaiting its report: the proof once the bucket reports it, or `None` once it counts as lost
    /// and is to be uploaded again (Q-82's thresholds, Q-284). `None` too when `key` awaits nothing.
    async fn looked_up(&mut self, key: &str, now: Duration) -> Out<Option<Verified>> {
        let Some(a) = self.awaiting.as_ref().filter(|a| a.key == key) else {
            return Ok(None);
        };
        let expected = a.expected.clone();
        match Verified::check(&self.sink, key, &expected).await {
            Ok(v) => {
                self.awaiting = None;
                Ok(Some(v))
            }
            Err(VerifyError::Reported { reported: None, .. }) => {
                let lost = self.awaiting.as_ref().is_some_and(|a| {
                    a.lookups >= LOST_LOOKUPS && now.saturating_sub(a.since) >= LOST_OVER
                });
                if lost {
                    self.awaiting = None;
                    return Ok(None);
                }
                self.missing(key).await.map(Some)
            }
            Err(e) => Err(verify_stop(e)),
        }
    }

    /// Counts a failed lookup of `key` if the bucket answers another call, and asks to be called again.
    async fn missing(&mut self, key: &str) -> Out<Verified> {
        let other = if key == self.keys.store() {
            self.keys.probe()
        } else {
            self.keys.store()
        };
        let answers = self.sink.stat(&other).await.is_ok();
        if let Some(a) = self.awaiting.as_mut()
            && answers
        {
            a.lookups = a.lookups.saturating_add(1);
        }
        Err(Stop::Retry(SinkError::Transient {
            reason: format!("{key}: the bucket reports no object after its upload"),
        }))
    }

    /// Only log segments have a local layout yet (`format.md` A.12; Q-285).
    fn local_path(&self, d: &FileDescription) -> Out<PathBuf> {
        if d.kind != kind::LOG_SEGMENT {
            return Err(io_stop(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("no local layout for files of kind {}", d.kind),
            )));
        }
        let name = SegmentName {
            generation: d.generation,
            file_id: d.file_id,
            sealed: true,
        };
        Ok(self.local.dir.join(LOG_DIR).join(name.to_string()))
    }

    fn read_local(&self, path: PathBuf) -> Out<Vec<u8>> {
        let f = self.local.fs.open(&path, OpenMode::Read).map_err(io_stop)?;
        let len = f.len().map_err(io_stop)?;
        if len > self.cfg.max_object {
            return Err(Stop::Fail(EnvelopeError::TooLarge { size: len }.into()));
        }
        let mut b = vec![0u8; usize::try_from(len).map_err(|e| io_stop(io::Error::other(e)))?];
        let mut at = 0usize;
        while at < b.len() {
            match f.read_at(&mut b[at..], at as u64).map_err(io_stop)? {
                0 => {
                    return Err(io_stop(io::Error::from(io::ErrorKind::UnexpectedEof)));
                }
                n => at += n,
            }
        }
        Ok(b)
    }

    fn local_size(&mut self, d: &FileDescription) -> u64 {
        let f = d.file_ref();
        if let Some(n) = self.sizes.get(&f) {
            return *n;
        }
        let n = self
            .local_path(d)
            .ok()
            .and_then(|p| self.local.fs.open(&p, OpenMode::Read).ok())
            .and_then(|h| h.len().ok());
        if let Some(n) = n {
            self.sizes.insert(f, n);
        }
        n.unwrap_or(0)
    }

    fn update_progress(&mut self, snap: &Catalogue) {
        let q: Vec<FileDescription> = queue(snap).iter().map(|e| e.description.clone()).collect();
        self.sizes
            .retain(|f, _| q.iter().any(|d| d.file_ref() == *f));
        let bytes = q
            .iter()
            .fold(0u64, |acc, d| acc.saturating_add(self.local_size(d)));
        self.progress.queued_files = q.len() as u64;
        self.progress.queued_bytes = bytes;
        let Some(c) = self.cursor.as_ref() else {
            return;
        };
        self.progress.queued_records = c.pending.len() as u64;
        self.progress.backed_up_through = backed_up_through(c, snap);
    }
}

/// Takes the synced records of `records` into the cursor.
fn take(c: &mut Cursor, records: Vec<Record>, snap: &Catalogue) {
    for r in records {
        if r.epoch <= c.scanned || r.epoch > snap.last_epoch() {
            continue;
        }
        if matches!(r.body, Body::Rebuild(_)) {
            c.copied_at.clear();
        }
        if let Some(f) = fresh_copy(&r) {
            c.copied_at.insert(f, r.epoch);
        }
        c.scanned = r.epoch;
        if r.epoch > c.uploaded {
            c.pending.push_back(r);
        }
    }
}

/// The last position of the longest run of live log segments, from the first, whose sealed-segment and
/// copy-verified records are both uploaded.
fn backed_up_through(c: &Cursor, snap: &Catalogue) -> Option<(u32, u64)> {
    let mut segs: Vec<&FileEntry> = snap
        .live()
        .filter(|e| e.description.kind == kind::LOG_SEGMENT)
        .collect();
    segs.sort_by_key(|e| (e.description.range.map(|r| r.0), e.description.file_ref()));
    let mut through = None;
    for e in segs {
        let up = |epoch: Option<u64>| epoch.is_some_and(|x| x <= c.uploaded);
        let listed = e.published.is_none_or(|p| p <= c.uploaded);
        if !(listed && up(c.copied_at.get(&e.description.file_ref()).copied())) {
            break;
        }
        if let Some((_, last)) = e.description.range {
            through = Some(last);
        }
    }
    through
}

impl<S: BackupSink, E: Envelope, M: ManifestPort> BackupDriver for Uploader<S, E, M> {
    fn probe<'a>(&'a mut self) -> DriverFuture<'a, Result<(), Refused>> {
        Box::pin(self.probe_bucket())
    }

    fn step<'a>(&'a mut self, now: Duration) -> DriverFuture<'a, Step> {
        Box::pin(self.run(now))
    }

    fn progress(&self) -> Progress {
        self.progress.clone()
    }
}
