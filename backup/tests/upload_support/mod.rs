//! A store on `MemFs` with its manifest `Appender`, a recording sink over `MemSink` with injected faults, and the
//! invariants every upload run must leave.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kvik_backup::LocalStore;
use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{MemSink, TestEnvelope, block_on};
use kvik_backup::keys::{Keys, ObjectName};
use kvik_backup::port::{ManifestPort, PortError};
use kvik_backup::upload::{BackupDriver, Step, UploadConfig, Uploader};
use kvik_files::MANIFEST;
use kvik_files::appender::{Appender, Opened, Settled};
use kvik_files::catalogue::{Catalogue, Tier};
use kvik_files::desc::{FileClass, FileDescription, FileRef, kind};
use kvik_files::image;
use kvik_files::rebuild::{Finality, LocalFile, LocalState, Place, Verifier, rebuild_manifest};
use kvik_files::record::{Body, Record};
use kvik_files::tier::CopyVerified;
use kvik_log::fs::{Fs, MemFs, OpenMode};
use kvik_log::manifest::encode_initial;
use kvik_log::name::SegmentName;
use kvik_sink_api::{BackupSink, ListPage, SinkError, SinkFuture, Stored};
use sha2::{Digest, Sha256};

pub const ID: [u8; 16] = [7; 16];
pub const KEY: [u8; 32] = [9; 32];
pub const T0: i64 = 1_000;
pub const STORE_BYTES: &[u8] = b"a store descriptor, uploaded as it is";

pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn cfg() -> UploadConfig {
    UploadConfig {
        max_object: 1 << 30,
        retry_first: Duration::from_secs(1),
        retry_max: Duration::from_secs(300),
    }
}

/// What the run did, in order, across restarts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A `put` that stored or matched an object.
    Put(String),
    /// A record appended to the manifest.
    Append(u64, Body),
}

#[derive(Debug, Default)]
pub struct Log {
    pub events: Vec<Event>,
    /// Broken rules seen while the run went on.
    pub violations: Vec<String>,
}

/// A fault the recording sink injects at one of its operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Fails before reaching the bucket.
    Fail(SinkError),
    /// Applies the call, then loses the reply.
    LoseReply,
    /// A `stat` reports nothing, a `get` finds nothing.
    Absent,
}

#[derive(Debug, Default)]
pub struct Faults {
    pub ops: u64,
    pub at: BTreeMap<u64, Fault>,
    /// Every call from this operation on fails as transient.
    pub kill_from: Option<u64>,
    /// What a `stat` of a key reports instead of the object.
    pub stat_of: BTreeMap<String, StatFault>,
    /// Every `stat` of a key not in `stat_of` fails as transient.
    pub others_fail: bool,
}

/// A `stat` of one key that does not report the object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatFault {
    /// Reports nothing, this many more times.
    Absent(u32),
    /// Reports other content.
    Other,
}

type Slot = Arc<Mutex<Option<Arc<Mutex<Appender>>>>>;

/// A sink over a shared `MemSink` that logs every `put`, injects faults by operation number and checks, at each
/// record `put`, that the record has a synced successor and that no rebuild holds uploads.
#[derive(Clone, Debug)]
pub struct RecSink {
    pub mem: Arc<MemSink>,
    pub log: Arc<Mutex<Log>>,
    pub faults: Arc<Mutex<Faults>>,
    slot: Slot,
}

impl RecSink {
    fn fault(&self) -> Option<Fault> {
        let mut f = lock(&self.faults);
        let op = f.ops;
        f.ops += 1;
        if f.kill_from.is_some_and(|k| op >= k) {
            return Some(Fault::Fail(SinkError::Transient {
                reason: format!("killed at {op}"),
            }));
        }
        f.at.remove(&op)
    }

    /// A fault for a call other than `put`: a hash mismatch is a `put`'s alone.
    fn read_fault(&self) -> Option<Fault> {
        self.fault()
            .filter(|f| !matches!(f, Fault::Fail(SinkError::HashMismatch { .. })))
    }

    fn check_put(&self, key: &str) {
        let Some(app) = lock(&self.slot).clone() else {
            lock(&self.log)
                .violations
                .push(format!("put {key} with no manifest open"));
            return;
        };
        let app = lock(&app);
        let mut log = lock(&self.log);
        if app.holds() {
            log.violations
                .push(format!("put {key} while a rebuild holds uploads"));
        }
        if let Some(ObjectName::Record { epoch, .. }) = Keys::new(ID).parse(key)
            && app.settled(epoch).is_none()
        {
            log.violations
                .push(format!("record {epoch} put without a synced successor"));
        }
    }
}

impl BackupSink for RecSink {
    fn put<'a>(&'a self, key: &'a str, bytes: Arc<[u8]>, sha: [u8; 32]) -> SinkFuture<'a, Stored> {
        let fault = self.fault();
        Box::pin(async move {
            match fault {
                Some(Fault::Fail(e)) => return Err(e),
                Some(Fault::Absent) | None => {}
                Some(Fault::LoseReply) => {
                    self.check_put(key);
                    let r = self.mem.put(key, bytes, sha).await;
                    if r.is_ok() {
                        lock(&self.log).events.push(Event::Put(key.into()));
                    }
                    return Err(SinkError::Transient {
                        reason: "reply lost".into(),
                    });
                }
            }
            self.check_put(key);
            let r = self.mem.put(key, bytes, sha).await;
            if r.is_ok() {
                lock(&self.log).events.push(Event::Put(key.into()));
            }
            r
        })
    }

    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
        let fault = self.read_fault();
        let special = {
            let mut f = lock(&self.faults);
            let others_fail = f.others_fail;
            match f.stat_of.get_mut(key) {
                Some(StatFault::Absent(n)) if *n > 0 => {
                    *n -= 1;
                    Some(Ok(None))
                }
                Some(StatFault::Absent(_)) => None,
                Some(StatFault::Other) => Some(Ok(Some(Stored::new(1, [0; 32])))),
                None if others_fail => Some(Err(SinkError::Transient {
                    reason: "unreachable".into(),
                })),
                None => None,
            }
        };
        Box::pin(async move {
            if let Some(r) = special {
                return r;
            }
            match fault {
                Some(Fault::Fail(e)) => Err(e),
                Some(Fault::Absent) => Ok(None),
                Some(Fault::LoseReply) => {
                    let _ = self.mem.stat(key).await;
                    Err(SinkError::Transient {
                        reason: "reply lost".into(),
                    })
                }
                None => self.mem.stat(key).await,
            }
        })
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        let fault = self.read_fault();
        Box::pin(async move {
            match fault {
                Some(Fault::Fail(e)) => Err(e),
                Some(Fault::Absent) => Err(SinkError::NotFound { key: key.into() }),
                Some(Fault::LoseReply) => Err(SinkError::Transient {
                    reason: "reply lost".into(),
                }),
                None => self.mem.get(key, range).await,
            }
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        start_after: Option<&'a str>,
    ) -> SinkFuture<'a, ListPage> {
        let fault = self.read_fault();
        Box::pin(async move {
            match fault {
                Some(Fault::Fail(e)) => Err(e),
                Some(Fault::Absent) => Ok(ListPage::new(Vec::new(), false)),
                Some(Fault::LoseReply) => Err(SinkError::Transient {
                    reason: "reply lost".into(),
                }),
                None => self.mem.list(prefix, start_after).await,
            }
        })
    }
}

/// A manifest port straight over the store's `Appender`, logging appends.
#[derive(Debug)]
pub struct Port {
    /// A record `records_after` shows above the synced ones, as one whose sync failed.
    pub phantom: Option<Record>,
    app: Arc<Mutex<Appender>>,
    fs: MemFs,
    dir: PathBuf,
    log: Arc<Mutex<Log>>,
}

impl ManifestPort for Port {
    fn append(&mut self, body: Body) -> Result<u64, PortError> {
        let e = lock(&self.app).append(body.clone(), T0)?;
        lock(&self.log).events.push(Event::Append(e, body));
        Ok(e)
    }

    fn settle(&mut self, epoch: u64) -> Result<Settled, PortError> {
        let mut app = lock(&self.app);
        let before = app.next_epoch();
        let s = app.settle(epoch, T0)?;
        if app.next_epoch() != before {
            lock(&self.log)
                .events
                .push(Event::Append(before, Body::Empty));
        }
        Ok(s)
    }

    fn holds(&self) -> bool {
        lock(&self.app).holds()
    }

    fn snapshot(&self) -> Arc<Catalogue> {
        Arc::new(lock(&self.app).catalogue().clone())
    }

    fn records_after(&self, after: u64) -> Result<Vec<Record>, PortError> {
        let bytes = self
            .fs
            .read(&self.dir.join(MANIFEST))
            .ok_or_else(|| PortError::Io(std::io::ErrorKind::NotFound.into()))?;
        let img = image::read(&bytes, &ID).map_err(std::io::Error::other)?;
        // A record that cannot be read stops the upload of those after it (spec §8 Generations).
        if img.corrupt.iter().any(|c| c.before_epoch > after) {
            return Err(PortError::Io(std::io::Error::other("a corrupt record")));
        }
        let mut v: Vec<Record> = img.records().filter(|r| r.epoch > after).cloned().collect();
        v.extend(self.phantom.clone().filter(|r| r.epoch > after));
        Ok(v)
    }
}

pub type Driver = Uploader<RecSink, TestEnvelope, Port>;

/// A store, its bucket and the run's log.
pub struct Harness {
    pub fs: MemFs,
    pub dir: PathBuf,
    pub sink: RecSink,
    pub segments: Vec<(FileDescription, Vec<u8>)>,
    next_seq: u64,
}

fn write_synced(fs: &MemFs, path: &Path, bytes: &[u8]) {
    let f = fs.open(path, OpenMode::CreateNew).unwrap();
    f.write_at(bytes, 0).unwrap();
    f.sync_all().unwrap();
    drop(f);
    fs.sync_dir(path.parent().unwrap()).unwrap();
}

fn open_appender(fs: &MemFs, dir: &Path) -> Appender {
    match Appender::open(fs, dir, ID, 0).unwrap() {
        Opened::Ready(a) => *a,
        Opened::Corrupt(_) => panic!("corrupt manifest"),
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    /// A new store with no sealed file and an empty bucket.
    pub fn new() -> Self {
        let fs = MemFs::new();
        let dir = PathBuf::from("store");
        write_synced(&fs, &dir.join("MANIFEST"), &encode_initial(ID, T0));
        write_synced(&fs, &dir.join("STORE"), STORE_BYTES);
        let app = Arc::new(Mutex::new(open_appender(&fs, &dir)));
        Self::with(fs, dir, app)
    }

    /// A harness over a store already on `fs`, its manifest open in `app`.
    pub fn with(fs: MemFs, dir: PathBuf, app: Arc<Mutex<Appender>>) -> Self {
        let sink = RecSink {
            mem: Arc::new(MemSink::new()),
            log: Arc::default(),
            faults: Arc::default(),
            slot: Arc::new(Mutex::new(Some(app))),
        };
        Self {
            fs,
            dir,
            sink,
            segments: Vec::new(),
            next_seq: 1,
        }
    }

    pub fn app(&self) -> Arc<Mutex<Appender>> {
        lock(&self.sink.slot).clone().expect("a manifest open")
    }

    /// Closes the manifest; every driver must be dropped first.
    pub fn close(&self) {
        *lock(&self.sink.slot) = None;
    }

    /// Puts `app` in place of the open manifest.
    pub fn set_app(&self, app: Appender) -> Arc<Mutex<Appender>> {
        let a = Arc::new(Mutex::new(app));
        *lock(&self.sink.slot) = Some(a.clone());
        a
    }

    /// Opens the manifest again after the process stopped (the caller dropped the driver first).
    pub fn reopen(&self) {
        self.close();
        self.set_app(open_appender(&self.fs, &self.dir));
    }

    pub fn stat_fault(&self, key: &str, f: Option<StatFault>) {
        let mut faults = lock(&self.sink.faults);
        match f {
            Some(f) => faults.stat_of.insert(key.into(), f),
            None => faults.stat_of.remove(key),
        };
    }

    pub fn others_fail(&self, on: bool) {
        lock(&self.sink.faults).others_fail = on;
    }

    /// Writes and syncs a sealed segment of `len` bytes, then appends its sealed-segment record.
    pub fn seal(&mut self, len: usize) -> FileDescription {
        let d = self.write_segment(len);
        lock(&self.app())
            .append(Body::Sealed(vec![d.clone()]), T0)
            .unwrap();
        d
    }

    /// Writes and syncs a sealed segment, with no record.
    pub fn write_segment(&mut self, len: usize) -> FileDescription {
        let file_id = self.segments.len() as u64 + 1;
        let content: Vec<u8> = (0..len)
            .map(|i| (i as u64 ^ file_id.wrapping_mul(131)) as u8)
            .collect();
        let first = self.next_seq;
        let last = first + 9;
        self.next_seq = last + 1;
        let d = FileDescription {
            kind: kind::LOG_SEGMENT,
            class: FileClass::None,
            generation: 0,
            file_id,
            range: Some(((0, first), (0, last))),
            hash: sha256(&content),
            inputs: Vec::new(),
        };
        let name = SegmentName {
            generation: 0,
            file_id,
            sealed: true,
        };
        write_synced(
            &self.fs,
            &self.dir.join("log").join(name.to_string()),
            &content,
        );
        self.segments.push((d.clone(), content));
        d
    }

    pub fn local(&self) -> LocalStore {
        LocalStore {
            fs: Arc::new(self.fs.clone()),
            dir: self.dir.clone(),
        }
    }

    pub fn port(&self) -> Port {
        Port {
            phantom: None,
            app: self.app(),
            fs: self.fs.clone(),
            dir: self.dir.clone(),
            log: self.sink.log.clone(),
        }
    }

    pub fn driver(&self) -> Driver {
        Uploader::new(
            self.sink.clone(),
            TestEnvelope::new(KEY),
            Keys::new(ID),
            self.local(),
            self.port(),
            cfg(),
        )
    }

    pub fn fault_at(&self, op: u64, f: Fault) {
        lock(&self.sink.faults).at.insert(op, f);
    }

    pub fn ops(&self) -> u64 {
        lock(&self.sink.faults).ops
    }

    pub fn kill_sink_from(&self, op: Option<u64>) {
        lock(&self.sink.faults).kill_from = op;
    }

    pub fn heal(&self) {
        let mut f = lock(&self.sink.faults);
        f.at.clear();
        f.kill_from = None;
        f.stat_of.clear();
        f.others_fail = false;
    }

    pub fn events(&self) -> Vec<Event> {
        lock(&self.sink.log).events.clone()
    }

    pub fn records(&self) -> Vec<Record> {
        let bytes = self.fs.read(&self.dir.join(MANIFEST)).unwrap();
        image::read(&bytes, &ID)
            .unwrap()
            .records()
            .cloned()
            .collect()
    }

    /// Every `put` of `key` that stored or matched an object.
    pub fn puts_of(&self, key: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, Event::Put(k) if k == key))
            .count()
    }

    /// The invariants a run leaves once it is idle with a healed bucket.
    pub fn check_done(&self) {
        let keys = Keys::new(ID);
        let env = TestEnvelope::new(KEY);
        let objects = self.sink.mem.objects();
        let records = self.records();
        let log = lock(&self.sink.log);
        assert_eq!(log.violations, Vec::<String>::new());
        let store = keys.store();
        assert_eq!(
            objects.get(&store),
            Some(&env.seal(&store, STORE_BYTES).unwrap())
        );
        let snap = lock(&self.app()).catalogue().clone();
        for (d, content) in &self.segments {
            let Some(e) = snap.file(d.file_ref()).filter(|e| e.is_live()) else {
                continue;
            };
            let key = keys.file(d);
            assert_eq!(
                objects.get(&key),
                Some(&env.seal(&key, content).unwrap()),
                "{key}"
            );
            assert!(e.verified.contains(&Tier::Deep), "{key} not verified");
            assert_eq!(fresh_copies(&records, d.file_ref()), 1, "{key}");
        }
        let last = records.last().map(|r| r.epoch);
        let mut in_manifest = BTreeSet::new();
        for (i, r) in records.iter().enumerate() {
            let key = keys.record(r.generation, r.epoch);
            in_manifest.insert(key.clone());
            if Some(r.epoch) == last {
                assert_eq!(r.body, Body::Empty, "the last record of an idle run");
                assert!(!objects.contains_key(&key), "the last record went up");
            } else {
                let want = env.seal(&key, &uploaded_form(&records, i)).unwrap();
                assert_eq!(objects.get(&key), Some(&want), "record {}", r.epoch);
            }
        }
        // Records of a manifest since rebuilt stay in the bucket under their old epochs (spec §8 Generations).
        let first = records.first().map_or(0, |r| r.epoch);
        for k in objects.keys() {
            if let Some(ObjectName::Record { epoch, .. }) = keys.parse(k) {
                assert!(
                    epoch < first || in_manifest.contains(k),
                    "{k} is in no local record"
                );
            }
        }
        check_order(&log.events, &records, &|f| {
            snap.file(f).is_some_and(|e| e.is_live())
        });
    }
}

/// The uploaded form of `records[i]`, built from `format.md` A.17: the epoch of the record before it in the manifest,
/// 0 for the first and for a rebuild record that opens a rebuilt manifest, then its block.
pub fn uploaded_form(records: &[Record], i: usize) -> Vec<u8> {
    let r = &records[i];
    let opens = matches!(r.body, Body::Rebuild(_)) && r.old_epoch.is_none();
    let previous = match i.checked_sub(1) {
        Some(j) if !opens => records[j].epoch,
        _ => 0,
    };
    [previous.to_le_bytes().as_slice(), &r.encode().unwrap()].concat()
}

/// Copy-verified records of `f` written since the last rebuild record.
pub fn fresh_copies(records: &[Record], f: FileRef) -> usize {
    records
        .iter()
        .filter(|r| r.old_epoch.is_none())
        .filter(
            |r| matches!(CopyVerified::from_body(&r.body), Some(Ok(c)) if c.file.file_ref() == f),
        )
        .count()
}

/// Per live file: its upload, then its copy-verified record, then the upload of each sealed-segment or publish
/// record naming it; record uploads first reach the bucket in epoch order.
pub fn check_order(events: &[Event], records: &[Record], live: &dyn Fn(FileRef) -> bool) {
    let keys = Keys::new(ID);
    let first_put = |key: &str| {
        events
            .iter()
            .position(|e| matches!(e, Event::Put(k) if k == key))
    };
    let mut seen = Vec::new();
    for e in events {
        if let Event::Put(k) = e
            && let Some(ObjectName::Record { epoch, .. }) = keys.parse(k)
            && !seen.contains(&epoch)
        {
            seen.push(epoch);
        }
    }
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "record uploads out of order: {seen:?}"
    );
    for r in records {
        let s = match &r.body {
            Body::Sealed(s) => s,
            Body::Publish(p) => &p.published,
            _ => continue,
        };
        let Some(rec_put) = first_put(&keys.record(r.generation, r.epoch)) else {
            continue;
        };
        for d in s.iter().filter(|d| live(d.file_ref())) {
            let file_put = first_put(&keys.file(d));
            let copy = events.iter().rposition(|e| {
                matches!(e, Event::Append(_, b)
                    if matches!(CopyVerified::from_body(b), Some(Ok(c)) if c.file == *d))
            });
            let copy = copy.or_else(|| {
                // A copy-verified record whose append reported a failure but reached the disk.
                records
                    .iter()
                    .any(
                        |x| matches!(CopyVerified::from_body(&x.body), Some(Ok(c)) if c.file == *d),
                    )
                    .then_some(0)
            });
            let (Some(fp), Some(cp)) = (file_put, copy) else {
                panic!(
                    "record {} went up before file {} was copied",
                    r.epoch, d.file_id
                );
            };
            assert!(
                fp < rec_put && cp < rec_put,
                "record {} before its file",
                r.epoch
            );
            if cp > 0 {
                assert!(
                    fp < cp,
                    "copy-verified record before the upload of {}",
                    d.file_id
                );
            }
        }
    }
}

/// Steps until idle, held or failed, moving `now` on at each retry. Panics if it does not settle in `max` steps.
pub fn run<D: BackupDriver>(d: &mut D, now: &mut Duration, max: usize) -> Step {
    for _ in 0..max {
        match block_on(d.step(*now)) {
            Step::Progress => {}
            Step::Retry { after, .. } => *now += after,
            s => return s,
        }
    }
    panic!("no idle step after {max} steps");
}

/// Steps until the first retry, failure, hold or idle step.
pub fn run_to_fault(d: &mut Driver, now: Duration) -> Step {
    for _ in 0..10_000 {
        match block_on(d.step(now)) {
            Step::Progress => {}
            s => return s,
        }
    }
    panic!("no fault, hold or idle step");
}

/// Steps until idle with a healthy bucket.
pub fn run_idle(d: &mut Driver, now: &mut Duration) {
    let s = run(d, now, 10_000);
    assert!(matches!(s, Step::Idle), "{s:?}");
}

/// A verifier that takes every local file as matching its description.
pub struct AllVerify;

impl Verifier for AllVerify {
    fn verify(&self, _: &LocalFile, _: &[&LocalFile]) -> bool {
        true
    }
}

/// The harness's segments as a rebuild finds them on hot.
pub fn local_files(h: &Harness) -> Vec<LocalFile> {
    h.segments
        .iter()
        .map(|(d, _)| LocalFile {
            description: d.clone(),
            place: Place::Hot,
            verified: true,
        })
        .collect()
}

/// Rebuilds the closed manifest, final.
pub fn rebuild_final(h: &Harness) -> Appender {
    let files = local_files(h);
    let state = LocalState {
        files: &files,
        losses: &[],
        lease_epoch: 0,
        mark_file_id: 0,
    };
    rebuild_manifest(&h.fs, &h.dir, ID, state, &AllVerify, Finality::Final, T0)
        .unwrap()
        .0
}
