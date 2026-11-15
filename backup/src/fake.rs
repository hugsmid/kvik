//! Fakes for the seams, behind the `test-support` feature: consumers build against these until each provider lands.
//!
//! - [`MemSink`]: an in-memory sink that can be killed from a given operation on.
//! - [`TestEnvelope`]: a tagged XOR envelope that detects a wrong key and damaged chunks; no secrecy.
//! - [`DirectPort`]: a [`ManifestPort`] straight over an `Appender`.
//! - [`ScriptedDriver`], [`ScriptedRestore`]: drivers that replay a script.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io;
use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kvik_files::MANIFEST;
use kvik_files::appender::{Appender, Settled};
use kvik_files::catalogue::Catalogue;
use kvik_files::image;
use kvik_files::record::{Body, Record};
use kvik_log::fs::OpenMode;
use kvik_sink_api::{BackupSink, ListPage, SinkError, SinkFuture, Stored};
use sha2::{Digest, Sha256};

use crate::envelope::{CHUNK, Envelope, EnvelopeError, KeyCheck};
use crate::port::{ManifestPort, PortError};
use crate::restore::{Prepared, RestoreDriver, RestoreError, RestoreProgress, RestoreStep};
use crate::upload::{BackupDriver, Progress, Refused, Step};
use crate::{DriverFuture, LocalStore};

/// Runs `f` to completion on this thread, parking while it is pending.
pub fn block_on<F: Future>(f: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        match f.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => return v,
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}

fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

#[derive(Debug, Default)]
struct MemState {
    objects: BTreeMap<String, Arc<[u8]>>,
    ops: u64,
    puts: u64,
    kill_at: Option<u64>,
}

/// An in-memory bucket with the sink's rules: no overwrite, a repeat with identical content succeeds. From the
/// operation numbered [`MemSink::kill_at`] on (counting from 0), every call fails as [`SinkError::Transient`] and
/// changes nothing, for kill loops.
#[derive(Debug, Default)]
pub struct MemSink {
    state: Mutex<MemState>,
}

impl MemSink {
    /// An empty bucket.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, MemState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fails every call from operation `op` on; `None` heals the bucket.
    pub fn kill_at(&self, op: Option<u64>) {
        self.state().kill_at = op;
    }

    /// Operations called so far, failed ones included.
    pub fn ops(&self) -> u64 {
        self.state().ops
    }

    /// `put` calls that stored or matched an object.
    pub fn puts(&self) -> u64 {
        self.state().puts
    }

    /// Every object.
    pub fn objects(&self) -> BTreeMap<String, Vec<u8>> {
        let s = self.state();
        s.objects
            .iter()
            .map(|(k, v)| (k.clone(), v.to_vec()))
            .collect()
    }

    /// Replaces or adds an object outside the sink's rules, to damage the bucket.
    pub fn set(&self, key: &str, bytes: &[u8]) {
        self.state().objects.insert(key.into(), bytes.into());
    }

    /// Removes an object outside the sink's rules.
    pub fn remove(&self, key: &str) {
        self.state().objects.remove(key);
    }

    fn call<T>(
        &self,
        f: impl FnOnce(&mut MemState) -> Result<T, SinkError>,
    ) -> Result<T, SinkError> {
        let mut s = self.state();
        let op = s.ops;
        s.ops += 1;
        if s.kill_at.is_some_and(|k| op >= k) {
            return Err(SinkError::Transient {
                reason: format!("killed at operation {op}"),
            });
        }
        f(&mut s)
    }
}

impl BackupSink for MemSink {
    fn put<'a>(&'a self, key: &'a str, bytes: Arc<[u8]>, sha: [u8; 32]) -> SinkFuture<'a, Stored> {
        let r = self.call(|s| {
            if sha256(&bytes) != sha {
                return Err(SinkError::HashMismatch { key: key.into() });
            }
            if let Some(have) = s.objects.get(key) {
                if **have != *bytes {
                    return Err(SinkError::Exists { key: key.into() });
                }
            } else {
                s.objects.insert(key.into(), bytes.clone());
            }
            s.puts += 1;
            Ok(Stored::new(bytes.len() as u64, sha))
        });
        Box::pin(async move { r })
    }

    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
        let r = self.call(|s| {
            Ok(s.objects
                .get(key)
                .map(|b| Stored::new(b.len() as u64, sha256(b))))
        });
        Box::pin(async move { r })
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        let r = self.call(|s| {
            let b = s
                .objects
                .get(key)
                .ok_or_else(|| SinkError::NotFound { key: key.into() })?;
            let Some(r) = range else {
                return Ok(b.to_vec());
            };
            let len = b.len() as u64;
            if r.start > r.end || r.end > len {
                return Err(SinkError::InvalidRange { key: key.into() });
            }
            Ok(b[r.start as usize..r.end as usize].to_vec())
        });
        Box::pin(async move { r })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        start_after: Option<&'a str>,
    ) -> SinkFuture<'a, ListPage> {
        let r = self.call(|s| {
            let keys = s
                .objects
                .keys()
                .filter(|k| k.starts_with(prefix) && start_after.is_none_or(|a| k.as_str() > a))
                .cloned()
                .collect();
            Ok(ListPage::new(keys, false))
        });
        Box::pin(async move { r })
    }
}

const MAGIC: [u8; 2] = *b"KT";
const VERSION: u8 = 1;
const ALGORITHM: u8 = 0xff;
const HEADER: usize = 12;
const TAG: usize = 8;
const FRAME: u64 = (CHUNK + TAG) as u64;

/// A tagged XOR envelope: a 12-byte header (`KT`, version, algorithm, key check), then per chunk the plaintext XOR
/// the key and an 8-byte tag over (key, object key, chunk index, plaintext). Deterministic, no secrecy. Its
/// [`Envelope::span`] may run past the object's end in the last chunk; clamp it to the object's size.
#[derive(Clone)]
pub struct TestEnvelope {
    key: [u8; 32],
}

impl std::fmt::Debug for TestEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestEnvelope(..)")
    }
}

impl TestEnvelope {
    /// An envelope under `key`.
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    fn tag(&self, object_key: &str, chunk: u64, plain: &[u8]) -> [u8; TAG] {
        let mut h = Sha256::new();
        h.update(self.key);
        h.update((object_key.len() as u64).to_le_bytes());
        h.update(object_key.as_bytes());
        h.update(chunk.to_le_bytes());
        h.update(plain);
        let d: [u8; 32] = h.finalize().into();
        let mut t = [0u8; TAG];
        t.copy_from_slice(&d[..TAG]);
        t
    }

    fn xor(&self, b: &mut [u8]) {
        for (i, x) in b.iter_mut().enumerate() {
            *x ^= self.key[i % 32];
        }
    }

    fn open_frames(
        &self,
        object_key: &str,
        first_chunk: u64,
        mut bytes: &[u8],
    ) -> Result<Vec<u8>, EnvelopeError> {
        let mut out = Vec::with_capacity(bytes.len());
        let mut chunk = first_chunk;
        while !bytes.is_empty() {
            let n = bytes.len().min(CHUNK + TAG);
            if n <= TAG {
                return Err(EnvelopeError::Truncated);
            }
            let (frame, rest) = bytes.split_at(n);
            let (cipher, tag) = frame.split_at(n - TAG);
            let mut plain = cipher.to_vec();
            self.xor(&mut plain);
            if self.tag(object_key, chunk, &plain) != tag {
                return Err(EnvelopeError::Authentication { chunk });
            }
            out.extend_from_slice(&plain);
            bytes = rest;
            chunk += 1;
        }
        Ok(out)
    }
}

impl Envelope for TestEnvelope {
    fn key_check(&self) -> KeyCheck {
        let d = sha256(&[b"key check".as_slice(), &self.key].concat());
        let mut c = [0u8; 8];
        c.copy_from_slice(&d[..8]);
        KeyCheck(c)
    }

    fn seal(&self, object_key: &str, plaintext: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
        let mut out =
            Vec::with_capacity(HEADER + plaintext.len() + plaintext.len() / CHUNK * TAG + TAG);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&[VERSION, ALGORITHM]);
        out.extend_from_slice(&self.key_check().0);
        for (i, plain) in plaintext.chunks(CHUNK).enumerate() {
            let mut c = plain.to_vec();
            self.xor(&mut c);
            out.extend_from_slice(&c);
            out.extend_from_slice(&self.tag(object_key, i as u64, plain));
        }
        Ok(out)
    }

    fn open(&self, object_key: &str, object: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
        let header = object.get(..HEADER).ok_or(EnvelopeError::Truncated)?;
        self.check_header(header)?;
        self.open_frames(object_key, 0, &object[HEADER..])
    }

    fn header_len(&self) -> u64 {
        HEADER as u64
    }

    fn check_header(&self, header: &[u8]) -> Result<(), EnvelopeError> {
        let h = header.get(..HEADER).ok_or(EnvelopeError::Truncated)?;
        if h[..2] != MAGIC || h[2] != VERSION || h[3] != ALGORITHM {
            return Err(EnvelopeError::Unsupported {
                version: h[2],
                algorithm: h[3],
            });
        }
        if h[4..] != self.key_check().0 {
            return Err(EnvelopeError::WrongKey);
        }
        Ok(())
    }

    fn span(&self, plain: Range<u64>) -> Range<u64> {
        let c = CHUNK as u64;
        let first = plain.start / c;
        let end = plain.end.div_ceil(c).max(first);
        let at = |chunk: u64| (HEADER as u64).saturating_add(chunk.saturating_mul(FRAME));
        at(first)..at(end)
    }

    fn open_span(
        &self,
        object_key: &str,
        header: &[u8],
        plain: Range<u64>,
        bytes: &[u8],
    ) -> Result<Vec<u8>, EnvelopeError> {
        self.check_header(header)?;
        let first = plain.start / CHUNK as u64;
        let p = self.open_frames(object_key, first, bytes)?;
        let base = first.saturating_mul(CHUNK as u64);
        let (lo, hi) = (plain.start - base, plain.end.saturating_sub(base));
        let (lo, hi) = (usize::try_from(lo), usize::try_from(hi));
        match (lo, hi) {
            (Ok(lo), Ok(hi)) if lo <= hi && hi <= p.len() => Ok(p[lo..hi].to_vec()),
            _ => Err(EnvelopeError::Truncated),
        }
    }
}

/// A [`ManifestPort`] straight over an `Appender`, with a fixed `written_at`.
#[derive(Debug)]
pub struct DirectPort {
    appender: Appender,
    local: LocalStore,
    store_id: [u8; 16],
    written_at: i64,
}

impl DirectPort {
    /// A port over `appender`, the manifest of the store `store_id` in `local`.
    pub fn new(appender: Appender, local: LocalStore, store_id: [u8; 16], written_at: i64) -> Self {
        Self {
            appender,
            local,
            store_id,
            written_at,
        }
    }

    fn read_manifest(&self) -> io::Result<Vec<u8>> {
        let f = self
            .local
            .fs
            .open(&self.local.dir.join(MANIFEST), OpenMode::Read)?;
        let mut b = vec![0u8; usize::try_from(f.len()?).map_err(io::Error::other)?];
        let mut at = 0;
        while at < b.len() {
            match f.read_at(&mut b[at..], at as u64)? {
                0 => b.truncate(at),
                n => at += n,
            }
        }
        Ok(b)
    }
}

impl ManifestPort for DirectPort {
    fn append(&mut self, body: Body) -> Result<u64, PortError> {
        Ok(self.appender.append(body, self.written_at)?)
    }

    fn settle(&mut self, epoch: u64) -> Result<Settled, PortError> {
        Ok(self.appender.settle(epoch, self.written_at)?)
    }

    fn holds(&self) -> bool {
        self.appender.holds()
    }

    fn snapshot(&self) -> Arc<Catalogue> {
        Arc::new(self.appender.catalogue().clone())
    }

    fn records_after(&self, after: u64) -> Result<Vec<Record>, PortError> {
        let bytes = self.read_manifest()?;
        let img = image::read(&bytes, &self.store_id).map_err(io::Error::other)?;
        Ok(img.records().filter(|r| r.epoch > after).cloned().collect())
    }
}

/// A [`BackupDriver`] that replays `steps`, then is [`Step::Idle`].
#[derive(Debug, Default)]
pub struct ScriptedDriver {
    /// What `probe` answers: `None` accepts.
    pub refusal: Option<Refused>,
    /// The steps, in order.
    pub steps: VecDeque<Step>,
    /// What `progress` reports.
    pub progress: Progress,
    /// The `now` of each `step` call.
    pub calls: Vec<Duration>,
}

impl BackupDriver for ScriptedDriver {
    fn probe<'a>(&'a mut self) -> DriverFuture<'a, Result<(), Refused>> {
        let r = self.refusal.clone().map_or(Ok(()), Err);
        Box::pin(async move { r })
    }

    fn step<'a>(&'a mut self, now: Duration) -> DriverFuture<'a, Step> {
        self.calls.push(now);
        let s = self.steps.pop_front().unwrap_or(Step::Idle);
        Box::pin(async move { s })
    }

    fn progress(&self) -> Progress {
        self.progress.clone()
    }
}

/// A [`RestoreDriver`] that replays its scripts: `prepare` answers in order, then [`RestoreError::NoStore`];
/// `step` replays `steps`, then is [`RestoreStep::Done`].
#[derive(Debug, Default)]
pub struct ScriptedRestore {
    /// What each `prepare` answers.
    pub prepared: VecDeque<Result<Prepared, RestoreError>>,
    /// The steps, in order.
    pub steps: VecDeque<RestoreStep>,
    /// What `progress` reports.
    pub progress: RestoreProgress,
}

impl RestoreDriver for ScriptedRestore {
    fn prepare<'a>(&'a mut self, _wall: i64) -> DriverFuture<'a, Result<Prepared, RestoreError>> {
        let r = self
            .prepared
            .pop_front()
            .unwrap_or(Err(RestoreError::NoStore));
        Box::pin(async move { r })
    }

    fn step<'a>(&'a mut self, _now: Duration) -> DriverFuture<'a, RestoreStep> {
        let s = self.steps.pop_front().unwrap_or(RestoreStep::Done);
        Box::pin(async move { s })
    }

    fn progress(&self) -> RestoreProgress {
        self.progress.clone()
    }
}

#[cfg(test)]
mod tests;
