//! Shared fixtures for the restore tests: a hand-built source store in a bucket, a hand-written plan (8.3a's stands
//! in later), a sink wrapper that loses, damages or delays chosen objects and logs every call, and a step loop.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kvik_backup::LocalStore;
use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{MemSink, TestEnvelope, block_on};
use kvik_backup::keys::Keys;
use kvik_backup::marker::{Marker, Summary};
use kvik_backup::plan::{PlanError, PlanInput, PlanMode, RestorePlan};
use kvik_backup::restore::{
    Prepared, RestoreConfig, RestoreDriver, RestoreError, RestoreStep, Restorer,
};
use kvik_files::MANIFEST;
use kvik_files::catalogue::Tier;
use kvik_files::desc::{FileClass, FileDescription, FileRef, kind};
use kvik_files::image;
use kvik_files::record::{Body, Record, record_type};
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_files::tier::DeepCopy;
use kvik_log::fs::MemFs;
use kvik_log::manifest::LostRange;
use kvik_log::mark::ResetStamp;
use kvik_log::store::StoreDescriptor;
use kvik_sink_api::{BackupSink, ListPage, SinkError, SinkFuture, Stored};
use sha2::{Digest, Sha256};

pub const KEY: [u8; 32] = [7; 32];
pub const OTHER_KEY: [u8; 32] = [8; 32];
pub const ID: [u8; 16] = [0x5a; 16];
pub const DIR: &str = "restored";
pub const MIN: Duration = Duration::from_secs(60);

pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// A source store: its descriptor and sealed log segments of generation 0, each with its plaintext.
#[derive(Clone, Debug)]
pub struct Source {
    pub id: [u8; 16],
    pub store: Vec<u8>,
    pub segments: Vec<(FileDescription, Vec<u8>)>,
    pub copies: BTreeMap<FileRef, DeepCopy>,
}

/// `n` segments of ten positions each; `big` makes the first span several envelope chunks.
pub fn source(id: [u8; 16], n: u64, big: bool) -> Source {
    let store = StoreDescriptor {
        store_id: id,
        created_at: 1,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: None,
    }
    .encode()
    .unwrap();
    let segments = (1..=n)
        .map(|i| {
            let len = if big && i == 1 {
                150_000
            } else {
                300 + i as usize * 17
            };
            let plain: Vec<u8> = (0..len).map(|b| (b as u64 * 31 + i) as u8).collect();
            let d = FileDescription {
                kind: kind::LOG_SEGMENT,
                class: FileClass::None,
                generation: 0,
                file_id: i,
                range: Some(((0, (i - 1) * 10 + 1), (0, i * 10))),
                hash: sha256(&plain),
                inputs: Vec::new(),
            };
            (d, plain)
        })
        .collect();
    Source {
        id,
        store,
        segments,
        copies: BTreeMap::new(),
    }
}

fn put(sink: &dyn BackupSink, key: &str, bytes: Vec<u8>) -> Stored {
    let sha = sha256(&bytes);
    block_on(sink.put(key, bytes.into(), sha)).unwrap()
}

/// Uploads the source as a backup would: `STORE`, each segment, then its records (an empty record, then one sealed
/// record per segment). Fills `copies` with what the bucket reports.
pub fn upload(sink: &dyn BackupSink, src: &mut Source) {
    upload_files(sink, src);
    let mut previous = 0;
    let records = records_of(src);
    for (i, r) in records.iter().enumerate() {
        put_record(sink, src.id, previous, r);
        put_marker(sink, src.id, 0, i as u64 + 1, Summary::of(&records[..=i]));
        previous = r.epoch;
    }
}

/// The records [`upload`] puts: an empty record at epoch 1, then one sealed record per segment.
pub fn records_of(src: &Source) -> Vec<Record> {
    let mut records = vec![Record::new(1, 0, 10, Body::Empty)];
    for (i, (d, _)) in src.segments.iter().enumerate() {
        records.push(Record::new(
            i as u64 + 2,
            0,
            11,
            Body::Sealed(vec![d.clone()]),
        ));
    }
    records
}

/// `STORE` and each segment, with no record. Fills `copies` with what the bucket reports.
pub fn upload_files(sink: &dyn BackupSink, src: &mut Source) {
    let keys = Keys::new(src.id);
    let env = TestEnvelope::new(KEY);
    put(
        sink,
        &keys.store(),
        env.seal(&keys.store(), &src.store).unwrap(),
    );
    for (d, plain) in &src.segments {
        let k = keys.file(d);
        let s = put(sink, &k, env.seal(&k, plain).unwrap());
        src.copies.insert(
            d.file_ref(),
            DeepCopy {
                size: s.size,
                sha256: s.sha256,
            },
        );
    }
}

/// A record's uploaded form, built from `format.md` A.17: the predecessor's epoch, then the record's block.
pub fn uploaded(previous: u64, r: &Record) -> Vec<u8> {
    [previous.to_le_bytes().as_slice(), &r.encode().unwrap()].concat()
}

/// Seals and puts store `id`'s record `r` naming `previous`, as the upload driver would.
pub fn put_record(sink: &dyn BackupSink, id: [u8; 16], previous: u64, r: &Record) {
    let k = Keys::new(id).record(r.generation, r.epoch);
    put(
        sink,
        &k,
        TestEnvelope::new(KEY)
            .seal(&k, &uploaded(previous, r))
            .unwrap(),
    );
}

/// Seals and puts store `id`'s last-uploaded marker `sequence` of `generation`, as the upload driver would (Q-614).
pub fn put_marker(
    sink: &dyn BackupSink,
    id: [u8; 16],
    generation: u32,
    sequence: u64,
    summary: Summary,
) {
    let k = Keys::new(id).marker(generation, sequence);
    let m = Marker {
        generation,
        sequence,
        summary,
    };
    put(
        sink,
        &k,
        TestEnvelope::new(KEY).seal(&k, &m.encode()).unwrap(),
    );
}

thread_local! {
    static COPIES: RefCell<BTreeMap<FileRef, DeepCopy>> = const { RefCell::new(BTreeMap::new()) };
}

/// Gives [`hand_plan`] the copies of `src`, which `upload` writes no copy-verified records for.
pub fn use_copies(src: &Source) {
    COPIES.with(|c| *c.borrow_mut() = src.copies.clone());
}

/// A hand-written plan: every live log segment, restored from its copy, the restored end its last position.
pub fn hand_plan(input: &PlanInput<'_>) -> Result<RestorePlan, PlanError> {
    let mut files = Vec::new();
    for e in input.source.live() {
        let f = e.description.file_ref();
        let copy = COPIES
            .with(|c| c.borrow().get(&f).cloned())
            .ok_or(PlanError::NotCopied { file: f })?;
        files.push(RestoredFile {
            description: e.description.clone(),
            tier: Tier::Deep,
            copy,
        });
    }
    if files.is_empty() {
        return Err(PlanError::NoRecords);
    }
    files.sort_by_key(|f| {
        (
            f.description.kind,
            f.description.generation,
            f.description.file_id,
        )
    });
    let restored_end = files
        .iter()
        .filter_map(|f| f.description.range.map(|r| r.1))
        .max()
        .unwrap_or((input.source_generation, 0));
    let mut fetch = files.clone();
    fetch.sort_by_key(|f| f.description.range.map(|r| r.0));
    Ok(RestorePlan {
        record: RestoreRecord {
            generation: input.generation,
            stamp: ResetStamp {
                sequence: 1,
                nonce: input.nonce,
            },
            source_generation: input.source_generation,
            restored_end,
            carried_ends: Vec::new(),
            losses: Vec::new(),
            acknowledgements: Vec::new(),
            entry_losses: Vec::new(),
            files,
            drill: matches!(input.mode, PlanMode::Drill),
        },
        fetch,
    })
}

static NONCE: AtomicU64 = AtomicU64::new(1_000);

pub fn cfg(mode: PlanMode) -> RestoreConfig {
    RestoreConfig {
        mode,
        lost_tries: 3,
        lost_over: 10 * MIN,
        retry_first: Duration::from_secs(1),
        retry_max: MIN,
        accept_unmarked: false,
        accept_earlier_state: false,
    }
}

pub fn local(fs: &MemFs) -> LocalStore {
    LocalStore {
        fs: Arc::new(fs.clone()),
        dir: PathBuf::from(DIR),
    }
}

pub fn restorer<S: BackupSink>(
    sink: S,
    fs: &MemFs,
    mode: PlanMode,
    key: [u8; 32],
) -> Restorer<S, TestEnvelope> {
    Restorer::new(
        sink,
        Box::new(move |_| TestEnvelope::new(key)),
        local(fs),
        hand_plan,
        Box::new(|| NONCE.fetch_add(1, Ordering::Relaxed)),
        cfg(mode),
    )
}

/// What a run of steps did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub installed: Vec<FileRef>,
    pub lost: Vec<(FileRef, LostRange, Duration)>,
    pub retries: u64,
    pub now: Duration,
}

/// Steps to `Done`, advancing time by each `Retry`, until `limit` of virtual time.
pub fn steps<D: RestoreDriver>(
    d: &mut D,
    start: Duration,
    limit: Duration,
) -> Result<Outcome, RestoreError> {
    let mut o = Outcome {
        now: start,
        ..Outcome::default()
    };
    for _ in 0..100_000 {
        match block_on(d.step(o.now)) {
            RestoreStep::Installed { file } => o.installed.push(file),
            RestoreStep::Lost { file, loss } => o.lost.push((file, loss, o.now)),
            RestoreStep::Retry { after, .. } => {
                o.retries += 1;
                o.now += after;
                if o.now > limit {
                    return Ok(o);
                }
            }
            RestoreStep::Done => return Ok(o),
            RestoreStep::Failed(e) => return Err(e),
            _ => unreachable!(),
        }
    }
    panic!("no end after 100,000 steps");
}

/// `prepare`, then steps to `Done` within two hours of virtual time.
pub fn restore<D: RestoreDriver>(d: &mut D) -> Result<(Prepared, Outcome), RestoreError> {
    let p = block_on(d.prepare(100))?;
    let o = steps(d, Duration::ZERO, 120 * MIN)?;
    Ok((p, o))
}

/// The restore records and lost ranges in the restored manifest.
pub fn manifest(fs: &MemFs) -> (Vec<Record>, Vec<LostRange>) {
    let bytes = fs.read(&Path::new(DIR).join(MANIFEST)).expect("a manifest");
    let id = kvik_log::header::FileHeader::decode(&bytes)
        .unwrap()
        .store_id;
    let img = image::read(&bytes, &id).unwrap();
    let restores = img
        .records()
        .filter(|r| r.body.record_type() == record_type::RESTORE)
        .cloned()
        .collect();
    let lost = img
        .records()
        .filter_map(|r| match &r.body {
            Body::LostRange(l) => Some(l.clone()),
            _ => None,
        })
        .collect();
    (restores, lost)
}

pub fn segment_path(d: &FileDescription) -> PathBuf {
    Path::new(DIR)
        .join("log")
        .join(format!("{:08x}-{:016x}.kvl", d.generation, d.file_id))
}

/// Every source segment not in `missing` is installed with its plaintext; those in `missing` are not.
pub fn assert_installed(fs: &MemFs, src: &Source, missing: &[FileRef]) {
    for (d, plain) in &src.segments {
        let got = fs.read(&segment_path(d));
        if missing.contains(&d.file_ref()) {
            assert_eq!(got, None, "{:?} should be missing", d.file_ref());
        } else {
            assert_eq!(got.as_deref(), Some(plain.as_slice()), "{:?}", d.file_ref());
        }
    }
}

/// Claim markers in the bucket.
pub fn claims(objects: &BTreeMap<String, Vec<u8>>, id: [u8; 16]) -> Vec<String> {
    let prefix = format!("{}claims/", Keys::new(id).store().trim_end_matches("STORE"));
    objects
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect()
}

/// A fault on one object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The bucket no longer holds it.
    Lose,
    /// Its bytes are damaged; the reported hash is unchanged.
    Damage,
    /// Downloads fail as `HashMismatch`.
    Mismatch,
    /// The next `n` calls on it fail as transient.
    Delay(u32),
    /// The next `n` downloads of it fail as transient.
    SlowGet(u32),
    /// The next `n` uploads to it fail as `HashMismatch` and store nothing.
    RejectPut(u32),
    /// Every upload to it is stored, then fails as `HashMismatch`.
    StoreThenMismatch,
    /// An upload replaces an existing object and succeeds: no conditional writes.
    Overwrite,
    /// An upload replaces an existing object, then fails as `Exists`.
    OverwriteQuietly,
    /// An upload to an existing object succeeds and changes nothing.
    Pretend,
    /// Listings leave it out; every other call finds it.
    Unlisted,
    /// Listings show it twice.
    ListedTwice,
}

/// A sink over a [`MemSink`] that applies per-key faults and logs every call as (operation, key).
#[derive(Debug, Default)]
pub struct FaultSink {
    pub inner: MemSink,
    faults: Mutex<BTreeMap<String, Fault>>,
    log: Mutex<Vec<(&'static str, String)>>,
    hashes: Mutex<Hashes>,
}

/// How a [`FaultSink`] treats an upload whose hash is not its bytes'.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hashes {
    /// Rejected as `HashMismatch`, nothing stored.
    #[default]
    Checked,
    /// Stored, then rejected as `HashMismatch`.
    StoreRejected,
    /// Stored, and it succeeds reporting the hash sent.
    Trusted,
    /// Not stored, yet it succeeds.
    Swallowed,
}

impl FaultSink {
    pub fn new(inner: MemSink) -> Self {
        Self {
            inner,
            ..Self::default()
        }
    }

    pub fn set(&self, key: &str, f: Option<Fault>) {
        let mut m = self.faults.lock().unwrap();
        match f {
            Some(f) => m.insert(key.into(), f),
            None => m.remove(key),
        };
    }

    pub fn set_hashes(&self, h: Hashes) {
        *self.hashes.lock().unwrap() = h;
    }

    pub fn log(&self) -> Vec<(&'static str, String)> {
        self.log.lock().unwrap().clone()
    }

    pub fn count(&self, op: &str, key: &str) -> usize {
        self.log()
            .iter()
            .filter(|(o, k)| *o == op && k == key)
            .count()
    }

    /// Logs the call; returns the fault to apply, spending one delayed call.
    fn enter(&self, op: &'static str, key: &str) -> Option<Fault> {
        self.log.lock().unwrap().push((op, key.into()));
        let mut m = self.faults.lock().unwrap();
        match m.get_mut(key) {
            Some(Fault::Delay(0)) => None,
            Some(Fault::Delay(n)) => {
                *n -= 1;
                Some(Fault::Delay(*n + 1))
            }
            Some(Fault::SlowGet(0)) => None,
            Some(Fault::SlowGet(n)) if op == "get" => {
                *n -= 1;
                Some(Fault::Delay(*n + 1))
            }
            Some(Fault::SlowGet(_)) => None,
            Some(Fault::RejectPut(0)) => None,
            Some(Fault::RejectPut(n)) if op == "put" => {
                *n -= 1;
                Some(Fault::RejectPut(*n + 1))
            }
            Some(
                Fault::RejectPut(_)
                | Fault::StoreThenMismatch
                | Fault::Overwrite
                | Fault::OverwriteQuietly
                | Fault::Pretend,
            ) if op != "put" => None,
            Some(Fault::Unlisted | Fault::ListedTwice) => None,
            other => other.copied(),
        }
    }
}

fn transient(key: &str) -> SinkError {
    SinkError::Transient {
        reason: format!("delayed: {key}"),
    }
}

impl BackupSink for FaultSink {
    fn put<'a>(&'a self, key: &'a str, bytes: Arc<[u8]>, sha: [u8; 32]) -> SinkFuture<'a, Stored> {
        let mismatch = || SinkError::HashMismatch { key: key.into() };
        match self.enter("put", key) {
            Some(Fault::Delay(_)) => Box::pin(async move { Err(transient(key)) }),
            Some(Fault::RejectPut(_)) => Box::pin(async move { Err(mismatch()) }),
            Some(Fault::StoreThenMismatch) => {
                let r = block_on(self.inner.put(key, bytes, sha)).and(Err(mismatch()));
                Box::pin(async move { r })
            }
            Some(f @ (Fault::Overwrite | Fault::OverwriteQuietly | Fault::Pretend))
                if self.inner.objects().contains_key(key) && sha256(&bytes) == sha =>
            {
                if f != Fault::Pretend {
                    self.inner.set(key, &bytes);
                }
                let r = match f {
                    Fault::OverwriteQuietly => Err(SinkError::Exists { key: key.into() }),
                    _ => Ok(Stored::new(bytes.len() as u64, sha)),
                };
                Box::pin(async move { r })
            }
            _ if sha256(&bytes) != sha && !self.inner.objects().contains_key(key) => {
                let ok = Ok(Stored::new(bytes.len() as u64, sha));
                let r = match *self.hashes.lock().unwrap() {
                    Hashes::Checked => Err(mismatch()),
                    Hashes::StoreRejected => {
                        self.inner.set(key, &bytes);
                        Err(mismatch())
                    }
                    Hashes::Trusted => {
                        self.inner.set(key, &bytes);
                        ok
                    }
                    Hashes::Swallowed => ok,
                };
                Box::pin(async move { r })
            }
            _ => self.inner.put(key, bytes, sha),
        }
    }

    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
        match self.enter("stat", key) {
            Some(Fault::Delay(_)) => Box::pin(async move { Err(transient(key)) }),
            Some(Fault::Lose) => Box::pin(async { Ok(None) }),
            _ => self.inner.stat(key),
        }
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        let r = match self.enter("get", key) {
            Some(Fault::Delay(_)) => Err(transient(key)),
            Some(Fault::Lose) => Err(SinkError::NotFound { key: key.into() }),
            Some(Fault::Mismatch) => Err(SinkError::HashMismatch { key: key.into() }),
            Some(Fault::Damage) => block_on(self.inner.get(key, None)).and_then(|mut b| {
                let mid = b.len() / 2;
                b[mid] ^= 0x40;
                match range {
                    None => Ok(b),
                    Some(r) => b
                        .get(r.start as usize..r.end as usize)
                        .map(<[u8]>::to_vec)
                        .ok_or(SinkError::InvalidRange { key: key.into() }),
                }
            }),
            None
            | Some(
                Fault::SlowGet(_)
                | Fault::RejectPut(_)
                | Fault::StoreThenMismatch
                | Fault::Overwrite
                | Fault::OverwriteQuietly
                | Fault::Pretend
                | Fault::Unlisted
                | Fault::ListedTwice,
            ) => block_on(self.inner.get(key, range)),
        };
        Box::pin(async move { r })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        start_after: Option<&'a str>,
    ) -> SinkFuture<'a, ListPage> {
        self.log.lock().unwrap().push(("list", prefix.into()));
        let faults = self.faults.lock().unwrap().clone();
        let r = block_on(self.inner.list(prefix, start_after)).map(|p| {
            let mut keys = Vec::new();
            for k in p.keys {
                match faults.get(&k) {
                    Some(Fault::Lose | Fault::Unlisted) => {}
                    Some(Fault::ListedTwice) => keys.extend([k.clone(), k]),
                    _ => keys.push(k),
                }
            }
            ListPage::new(keys, p.more)
        });
        Box::pin(async move { r })
    }
}

/// A [`MemSink`] holding a copy of `objects`.
pub fn mem_with(objects: &BTreeMap<String, Vec<u8>>) -> MemSink {
    let m = MemSink::new();
    for (k, v) in objects {
        m.set(k, v);
    }
    m
}

/// A bucket holding `src`, with `src.copies` filled; returns its objects too.
pub fn bucket(src: &mut Source) -> BTreeMap<String, Vec<u8>> {
    let m = MemSink::new();
    upload(&m, src);
    use_copies(src);
    m.objects()
}
