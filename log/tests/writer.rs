//! The commit path (roadmap 4.4) driven deterministically: the synchronous writer core with explicit times, the
//! in-memory file system with injected faults, and the maintenance work run in place.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{
    Definition, Entry, IntervalLength, Kind, MarkState, Recovery, Retention, Timing, Versioning,
};
use kvik_log::facts::{FactFlags, FactRow, Quality, Value, ValueType};
use kvik_log::fs::{FaultOp as MemOp, Fs, MemFs, MemOp as OpRecord, OpenMode};
use kvik_log::header::FileHeader;
use kvik_log::mark::{MarkSlot, read_mark};
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, SealState, scan};
use kvik_log::store::{NewStore, StoreDir, StoreError};
use kvik_log::writer::{
    Ack, Call, CallSource, CommitError, EveryCommitReason, FaultOp, IngestQueue, LogWriter,
    MarkSync, MarkSyncMode, Position, Reason, SYNC_BURST, SealJob, StartRule, Wait, WriterConfig,
    create_segment, seal_segment_described,
};
use proptest::prelude::*;

const STORE_ID: [u8; 16] = [7; 16];
const BOOT: [u8; 16] = [9; 16];
const T0: i64 = 1_767_225_600_000_000_000;
const MS: Duration = Duration::from_millis(1);
const US: Duration = Duration::from_micros(1);

fn dir() -> PathBuf {
    PathBuf::from("store")
}

fn new_store() -> NewStore {
    NewStore {
        store_id: STORE_ID,
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    }
}

fn cfg() -> WriterConfig {
    WriterConfig {
        segment_size: 64 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        ..WriterConfig::default()
    }
}

fn fact(series: u32, t: i64, text: usize) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: if text > 0 {
            Value::Text("x".repeat(text))
        } else {
            Value::F64(t as u64)
        },
        changed_at: None,
    })
}

fn alias(id: u32) -> Item {
    Item::from(Entry::Alias {
        id,
        name: format!("a{id}"),
    })
}

fn definition(id: u32, interval: Option<IntervalLength>) -> Item {
    Item::from(Entry::Definition(Box::new(Definition {
        id,
        name: format!("s{id}"),
        kind: if interval.is_some() {
            Kind::Interval
        } else {
            Kind::Gauge
        },
        value_type: ValueType::F64,
        scale: None,
        unit: "u".into(),
        expected_interval: None,
        staleness: None,
        record_on_change: false,
        counter: None,
        lateness_bound: 1_000_000_000,
        state_names: vec![],
        versioning: Versioning::None,
        timing: Timing::Observed,
        interval,
        retention: Retention::LongTerm,
        excess_precision: None,
        staleness_set: false,
    })))
}

/// A queue the sync hook can push into while the writer is inside a sync.
#[derive(Clone)]
struct SharedQueue(Arc<Mutex<IngestQueue>>);

impl CallSource for SharedQueue {
    fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }
    fn take(&mut self, max: u64) -> Vec<Call> {
        self.0.lock().unwrap().take(max)
    }
    fn put_back(&mut self, calls: Vec<Call>) {
        self.0.lock().unwrap().put_back(calls)
    }
}

struct H {
    fs: MemFs,
    w: LogWriter,
    q: SharedQueue,
    now: Duration,
    wall: i64,
    calls: BTreeMap<u64, Vec<Item>>,
    done: BTreeMap<u64, Result<Ack, CommitError>>,
    /// Run the maintenance work in place after each step.
    maint: bool,
    /// Calls acknowledged so far, shared with sync hooks.
    acked_n: Arc<Mutex<usize>>,
    /// Seal jobs taken but not yet run.
    inflight: Vec<SealJob>,
}

impl H {
    fn new(cfg: WriterConfig) -> Self {
        Self::with(cfg, Some(BOOT), MemFs::new())
    }

    fn with(cfg: WriterConfig, boot: Option<[u8; 16]>, fs: MemFs) -> Self {
        let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap();
        let w = LogWriter::open(store, cfg.clone(), boot, Duration::ZERO, T0).unwrap();
        let mut h = Self {
            fs,
            w,
            q: SharedQueue(Arc::new(Mutex::new(IngestQueue::new(cfg.ingest_queue)))),
            now: Duration::ZERO,
            wall: T0,
            calls: BTreeMap::new(),
            done: BTreeMap::new(),
            maint: true,
            acked_n: Arc::new(Mutex::new(0)),
            inflight: Vec::new(),
        };
        h.run_maint();
        h
    }

    fn submit(&mut self, items: Vec<Item>) -> u64 {
        let call = Call::new(items.clone(), self.w.config().call_limit()).unwrap();
        let id = self.q.0.lock().unwrap().push(call).unwrap();
        self.calls.insert(id, items);
        id
    }

    /// Creates the segment the writer asks for, if any.
    fn create(&mut self) {
        if let Some(req) = self.w.segment_request(self.wall) {
            let r = create_segment(&self.fs, &req, &self.w.halt());
            self.w.segment_created(r);
        }
    }

    /// Takes the seal jobs that are ready, without running them.
    fn seal_take(&mut self) {
        let jobs = self.w.take_seal_jobs(self.now);
        self.inflight.extend(jobs);
    }

    /// Runs the seal jobs taken.
    fn seal_run(&mut self) {
        for job in std::mem::take(&mut self.inflight) {
            let r = seal_segment_described(&self.fs, &job, &self.w.halt());
            self.w.seal_done(job, r, self.now);
        }
    }

    fn run_maint(&mut self) {
        self.create();
        self.seal_take();
        self.seal_run();
        // A creation asked for after a seal or a roll.
        self.create();
    }

    fn step(&mut self) -> Option<Duration> {
        let mut q = self.q.clone();
        let s = self.w.step(self.now, self.wall, &mut q);
        for (id, r) in s.done {
            assert!(
                self.done.insert(id, r).is_none(),
                "call {id} finished twice"
            );
        }
        *self.acked_n.lock().unwrap() = self.done.values().filter(|r| r.is_ok()).count();
        if self.maint {
            self.run_maint();
        }
        s.next
    }

    fn advance(&mut self, d: Duration) {
        self.now += d;
        self.wall += d.as_nanos() as i64;
    }

    /// Steps until the queue is empty, moving the clock to each deadline.
    fn drain(&mut self) {
        for _ in 0..10_000 {
            let next = self.step();
            if self.q.is_empty() || self.w.fault().is_some() {
                return;
            }
            let at = next.unwrap_or(self.now + MS).max(self.now + MS);
            self.advance(at - self.now);
        }
        panic!("the queue does not drain");
    }

    fn acked(&self) -> Vec<(u64, Ack)> {
        self.done
            .iter()
            .filter_map(|(&id, r)| r.as_ref().ok().map(|a| (id, *a)))
            .collect()
    }
}

fn mark_path() -> PathBuf {
    dir().join("log").join("MARK")
}

fn durable_mark(fs: &MemFs) -> MarkSlot {
    read_mark(&fs.read_durable(&mark_path()).unwrap(), &STORE_ID)
        .unwrap()
        .unwrap()
        .1
}

/// Segment files under `log/`, by file id: name and contents (durable image or current).
fn segments(fs: &MemFs, durable: bool) -> BTreeMap<u64, (SegmentName, Vec<u8>)> {
    let log = dir().join("log");
    fs.paths()
        .into_iter()
        .filter(|p| p.parent() == Some(log.as_path()))
        .filter_map(|p| {
            let name = SegmentName::parse(p.file_name()?.to_str()?)?;
            let bytes = if durable {
                fs.read_durable(&p)
            } else {
                fs.read(&p)
            }?;
            Some((name.file_id, (name, bytes)))
        })
        .collect()
}

fn ident(n: &SegmentName) -> SegmentIdentity {
    SegmentIdentity {
        store_id: STORE_ID,
        file_id: n.file_id,
        generation: n.generation,
    }
}

/// Every complete batch of every segment, in order: (file id, end offset, content).
fn batches(fs: &MemFs, durable: bool) -> Vec<(u64, u64, BatchContent)> {
    let mut out = Vec::new();
    for (id, (name, bytes)) in segments(fs, durable) {
        let r = scan(&bytes, &ident(&name), ScanLimit::End).unwrap();
        for b in &r.batches {
            out.push((id, b.end(), BatchContent::decode(b).unwrap()));
        }
    }
    out
}

/// A crash image: every segment's and the mark's bytes as a power loss at one moment leaves them.
struct Image {
    segments: BTreeMap<u64, (SegmentName, Vec<u8>)>,
    mark: Vec<u8>,
}

fn image(fs: &MemFs) -> Image {
    Image {
        segments: segments(fs, true),
        mark: fs.read_durable(&mark_path()).unwrap(),
    }
}

fn image_batches(img: &Image) -> Vec<(u64, u64, BatchContent)> {
    let mut out = Vec::new();
    for (id, (name, bytes)) in &img.segments {
        let r = scan(bytes, &ident(name), ScanLimit::End).unwrap();
        for b in &r.batches {
            out.push((*id, b.end(), BatchContent::decode(b).unwrap()));
        }
    }
    out
}

/// Checks a crash image against the first `acked` acknowledged calls: their entries are the image's first entries,
/// in order; at most `extra` batches lie beyond them; the mark names the end of a batch in the image, or nothing.
fn check_image(img: &Image, calls: &BTreeMap<u64, Vec<Item>>, acked: usize, extra: usize) {
    let durable = image_batches(img);
    let mut items: Vec<Item> = Vec::new();
    let mut expect_first = 1;
    for (_, _, c) in &durable {
        assert_eq!(c.first_sequence, expect_first, "dense sequences on disk");
        expect_first = c.last_sequence() + 1;
        items.extend(c.items.iter().cloned());
    }
    let acked_items: Vec<Item> = (1..=acked as u64)
        .flat_map(|id| calls[&id].iter().cloned())
        .collect();
    assert!(
        items.len() >= acked_items.len(),
        "an acknowledged call is not on stable storage"
    );
    assert_eq!(&items[..acked_items.len()], &acked_items[..]);
    let next = acked_items.len() as u64 + 1;
    let beyond = durable
        .iter()
        .filter(|(_, _, c)| c.last_sequence() >= next)
        .count();
    assert!(
        beyond <= extra,
        "{beyond} synced batches are not acknowledged"
    );
    let m = read_mark(&img.mark, &STORE_ID).unwrap().unwrap().1;
    if m.last_sequence == 0 {
        assert_eq!((m.file_id, m.offset), (0, 0));
    } else {
        let b = durable
            .iter()
            .find(|(_, _, c)| c.last_sequence() == m.last_sequence)
            .expect("the mark names a synced batch");
        assert_eq!((b.0, b.1), (m.file_id, m.offset));
    }
    assert!(m.last_sequence < expect_first);
}

/// The checks every history must pass: acknowledged calls hold dense, increasing positions in call order and are
/// exactly the synced batches (apart from at most one batch synced before a fault, never acknowledged); the synced
/// mark is never ahead of synced data.
fn check(h: &H) {
    let acked = h.acked();
    let mut next = 1;
    for (i, (id, ack)) in acked.iter().enumerate() {
        assert_eq!(*id, 1 + i as u64, "acknowledged calls are a prefix");
        assert_eq!(ack.first.sequence, next);
        assert_eq!(ack.first.generation, 0);
        let n = h.calls[id].len() as u64;
        assert_eq!(ack.last.sequence, next + n - 1);
        next += n;
    }
    for (id, r) in &h.done {
        if r.is_err() {
            assert!(acked.iter().all(|(a, _)| a < id));
        }
    }
    let extra = usize::from(h.w.fault().is_some() || h.w.halt().is_set());
    check_image(&image(&h.fs), &h.calls, acked.len(), extra);
}

/// Every created file (but the empty lock file) is synced and then its directory synced; every rename is followed
/// by a directory sync (`format.md` A.4, A.12, A.13).
fn assert_names_durable(ops: &[OpRecord]) {
    for (i, o) in ops.iter().enumerate() {
        let parent = o.path.parent().unwrap();
        let later = |op: MemOp, path: &Path, from: usize| {
            ops[from..]
                .iter()
                .position(|x| x.op == op && x.ok && x.path == path)
                .map(|p| p + from)
        };
        match o.op {
            MemOp::Create if o.ok && !o.path.ends_with("LOCK") => {
                let synced = later(MemOp::Sync, &o.path, i)
                    .unwrap_or_else(|| panic!("{:?} never synced", o.path));
                assert!(
                    later(MemOp::SyncDir, parent, synced).is_some(),
                    "no directory sync after creating {:?}",
                    o.path
                );
            }
            MemOp::Rename if o.ok => assert!(
                later(MemOp::SyncDir, parent, i).is_some(),
                "no directory sync after renaming to {:?}",
                o.path
            ),
            _ => {}
        }
    }
}

#[test]
fn store_creation_layout_and_open_rules() {
    let fs = MemFs::new();
    let afs: Arc<dyn Fs> = Arc::new(fs.clone());
    let store = StoreDir::open(afs.clone(), &dir(), Some(&new_store())).unwrap();
    assert_eq!(store.descriptor().site_zone, "Etc/UTC");
    let names: Vec<_> = fs.paths();
    assert_eq!(
        names,
        vec![
            dir().join("LOCK"),
            dir().join("MANIFEST"),
            dir().join("STORE")
        ]
    );
    let manifest = fs.read_durable(&dir().join("MANIFEST")).unwrap();
    assert_eq!(FileHeader::decode(&manifest).unwrap().kind, 3);
    assert!(matches!(
        StoreDir::open(afs.clone(), &dir(), None),
        Err(StoreError::Locked)
    ));
    let w = LogWriter::open(store, cfg(), Some(BOOT), Duration::ZERO, T0).unwrap();
    assert_eq!(w.start_report().rule, StartRule::FirstStart);
    let m = durable_mark(&fs);
    assert_eq!(
        (m.counter, m.file_id, m.offset, m.last_sequence),
        (1, 0, 0, 0)
    );
    assert_names_durable(&fs.ops());
    assert_eq!(m.boot_id, BOOT);
    drop(w);
    // Opening it again on the same boot keeps everything up to the mark and writes no entry.
    let store = StoreDir::open(afs.clone(), &dir(), None).unwrap();
    let w = LogWriter::open(store, cfg(), Some(BOOT), Duration::ZERO, T0).unwrap();
    assert_eq!(w.start_report().rule, StartRule::SameBoot);
    assert!(w.start_report().entry.is_none());
    drop(w);

    // A creation that did not finish is created again; anything else is not a store.
    let fs = MemFs::new();
    let afs: Arc<dyn Fs> = Arc::new(fs.clone());
    for n in ["LOCK", "STORE.tmp"] {
        fs.open(&dir().join(n), OpenMode::CreateNew).unwrap();
    }
    assert!(matches!(
        StoreDir::open(afs.clone(), &dir(), None),
        Err(StoreError::NotAStore)
    ));
    StoreDir::open(afs.clone(), &dir(), Some(&new_store())).unwrap();
    let fs = MemFs::new();
    let afs: Arc<dyn Fs> = Arc::new(fs.clone());
    fs.open(&dir().join("log").join("x"), OpenMode::CreateNew)
        .unwrap();
    assert!(matches!(
        StoreDir::open(afs.clone(), &dir(), Some(&new_store())),
        Err(StoreError::NotAStore)
    ));
    // A foreign sub-directory, even an empty-looking one, makes it not a store.
    let fs = MemFs::new();
    let afs: Arc<dyn Fs> = Arc::new(fs.clone());
    fs.open(&dir().join("photos").join("a"), OpenMode::CreateNew)
        .unwrap();
    assert!(matches!(
        StoreDir::open(afs.clone(), &dir(), Some(&new_store())),
        Err(StoreError::NotAStore)
    ));
    // A damaged descriptor is corrupt.
    let fs = MemFs::new();
    let afs: Arc<dyn Fs> = Arc::new(fs.clone());
    drop(StoreDir::open(afs.clone(), &dir(), Some(&new_store())).unwrap());
    let f = fs.open(&dir().join("STORE"), OpenMode::ReadWrite).unwrap();
    f.write_at(&[0xff], 70).unwrap();
    drop(f);
    assert!(matches!(
        StoreDir::open(afs, &dir(), None),
        Err(StoreError::Corrupt(_))
    ));
}

/// format A.12 Open: a creation whose last directory sync failed leaves `STORE` renamed in the page cache only; the
/// next open syncs the directory, so a later power loss never leaves the store's files without `STORE`.
#[test]
fn opening_a_store_makes_a_creations_rename_durable() {
    for seed in 0..64 {
        let fs = MemFs::new();
        let afs: Arc<dyn Fs> = Arc::new(fs.clone());
        // The fifth directory sync of a creation is the one after the rename.
        fs.fail_nth(MemOp::SyncDir, "store", 4, io::ErrorKind::Other);
        assert!(StoreDir::open(afs.clone(), &dir(), Some(&new_store())).is_err());
        fs.restart();
        let store = StoreDir::open(afs.clone(), &dir(), Some(&new_store())).unwrap();
        let mut w = LogWriter::open(store, cfg(), Some(BOOT), Duration::ZERO, T0).unwrap();
        w.shutdown(Duration::ZERO).unwrap();
        drop(w);
        fs.power_loss_partial(seed);
        fs.restart();
        assert!(
            StoreDir::open(afs, &dir(), Some(&new_store())).is_ok(),
            "seed {seed}"
        );
    }
}

#[test]
fn group_commit_syncs_at_once_within_the_budget_then_batches_until_its_refill() {
    let mut h = H::new(cfg());
    h.maint = false;
    let q = h.q.clone();
    let limit = h.w.config().call_limit();
    let fs = h.fs.clone();
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let seen = arrivals.clone();
    fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        if p.to_string_lossy().ends_with(".open") && seen.lock().unwrap().is_empty() {
            let items = vec![fact(9, T0, 0)];
            let call = Call::new(items.clone(), limit).unwrap();
            let id = q.0.lock().unwrap().push(call).unwrap();
            seen.lock().unwrap().push((id, items));
        }
    })));
    let a = h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert!(
        h.done[&a].is_ok(),
        "the first call after a quiet period is synced at once"
    );
    fs.set_sync_hook(None);
    let during = arrivals.lock().unwrap()[0].0;
    assert!(
        !h.done.contains_key(&during),
        "not covered by the sync it arrived during"
    );
    h.advance(100 * US);
    h.step();
    assert!(h.done[&during].is_ok());
    for _ in 2..SYNC_BURST {
        h.advance(100 * US);
        let c = h.submit(vec![fact(2, T0, 0)]);
        h.step();
        assert!(h.done[&c].is_ok(), "inside the budget's burst");
    }
    // The budget is spent: calls wait for its refill, one sync per interval (the ceiling: no clock), and form one batch.
    let mut ids = Vec::new();
    for _ in 0..5 {
        h.advance(100 * US);
        ids.push(h.submit(vec![fact(1, T0, 0), fact(2, T0, 0)]));
        assert_eq!(h.step(), Some(Duration::from_millis(10)));
    }
    assert!(ids.iter().all(|i| !h.done.contains_key(i)));
    h.advance(Duration::from_millis(10) - h.now);
    h.step();
    assert!(ids.iter().all(|i| h.done[i].is_ok()));
    assert_eq!(
        h.w.stats().batches,
        SYNC_BURST + 1,
        "five calls in one batch"
    );
    // A quiet period refills the budget: the next call is synced in the step it arrives.
    h.advance(Duration::from_secs(1));
    let b = h.submit(vec![fact(3, T0, 0)]);
    h.step();
    assert!(h.done[&b].is_ok());
    for (id, items) in arrivals.lock().unwrap().iter() {
        h.calls.insert(*id, items.clone());
    }
    check(&h);

    // With the bound disabled, every step may sync.
    let mut h = H::new(WriterConfig {
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    for _ in 0..3 {
        let c = h.submit(vec![fact(1, T0, 0)]);
        h.step();
        assert!(h.done[&c].is_ok());
    }
    check(&h);
}

#[test]
fn calls_arriving_during_a_sync_form_the_next_batch() {
    let mut h = H::new(WriterConfig {
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    // Segment 1 exists; no further creation runs during this test's first steps.
    h.maint = false;
    let q = h.q.clone();
    let limit = h.w.config().call_limit();
    let fs = h.fs.clone();
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let seen = arrivals.clone();
    fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        if p.to_string_lossy().ends_with(".open") && seen.lock().unwrap().len() < 3 {
            let items = vec![fact(9, T0, 0)];
            let call = Call::new(items.clone(), limit).unwrap();
            let id = q.0.lock().unwrap().push(call).unwrap();
            seen.lock().unwrap().push((id, items));
        }
    })));
    let a = h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert!(h.done[&a].is_ok());
    let first: Vec<u64> = arrivals.lock().unwrap().iter().map(|x| x.0).collect();
    assert_eq!(first.len(), 1);
    assert!(
        !h.done.contains_key(&first[0]),
        "not covered by the sync it arrived during"
    );
    h.step();
    assert!(h.done[&first[0]].is_ok());
    fs.set_sync_hook(None);
    for (id, items) in arrivals.lock().unwrap().iter() {
        h.calls.insert(*id, items.clone());
    }
    h.drain();
    check(&h);
}

#[test]
fn a_batch_with_an_entry_that_is_not_a_fact_forces_the_mark_sync() {
    let mut h = H::new(cfg());
    let first = h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert!(h.done[&first].is_ok());
    assert_eq!(
        durable_mark(&h.fs).last_sequence,
        1,
        "the segment's first batch forces it (Q-630)"
    );
    h.advance(Duration::from_millis(20));
    let a = h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert!(h.done[&a].is_ok());
    assert_eq!(
        durable_mark(&h.fs).last_sequence,
        1,
        "facts wait for the timer"
    );
    assert_eq!(h.w.mark().last_sequence, 2);
    h.advance(Duration::from_millis(20));
    h.fs.clear_ops();
    let b = h.submit(vec![alias(1)]);
    h.step();
    assert!(h.done[&b].is_ok());
    assert_eq!(durable_mark(&h.fs).last_sequence, 3);
    let ops: Vec<_> =
        h.fs.ops()
            .into_iter()
            .filter(|o| o.op == MemOp::Write || o.op == MemOp::Sync)
            .map(|o| (o.op, o.path.ends_with("MARK")))
            .collect();
    assert_eq!(
        &ops[..4],
        &[
            (MemOp::Write, false),
            (MemOp::Sync, false),
            (MemOp::Write, true),
            (MemOp::Sync, true)
        ]
    );
    // The timer: a facts batch, then the mark is synced once `mark_sync_interval` has passed.
    h.advance(Duration::from_millis(20));
    let c = h.submit(vec![fact(1, T0, 0)]);
    let next = h.step();
    assert!(h.done[&c].is_ok());
    assert_eq!(durable_mark(&h.fs).last_sequence, 3);
    assert_eq!(next, Some(Duration::from_millis(1_040)));
    h.advance(next.unwrap() - h.now);
    h.step();
    assert_eq!(durable_mark(&h.fs).last_sequence, 4);
    assert_eq!(h.w.stats().mark_synced.sequence, 4);
    // Clean shutdown syncs the mark.
    h.advance(MS);
    h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert_eq!(durable_mark(&h.fs).last_sequence, 4);
    h.w.shutdown(h.now).unwrap();
    assert_eq!(durable_mark(&h.fs).last_sequence, 5);
    check(&h);
}

#[test]
fn every_commit_mode_and_no_boot_id() {
    let mut h = H::with(cfg(), None, MemFs::new());
    assert_eq!(
        h.w.stats().mark_sync,
        MarkSyncMode::EveryCommit {
            reason: EveryCommitReason::NoBootId
        }
    );
    h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert_eq!(durable_mark(&h.fs).last_sequence, 1);
    assert_eq!(durable_mark(&h.fs).boot_id, [0; 16]);
    let h = H::new(WriterConfig {
        mark_sync: MarkSync::EveryCommit,
        ..cfg()
    });
    assert_eq!(
        h.w.stats().mark_sync,
        MarkSyncMode::EveryCommit {
            reason: EveryCommitReason::Configured
        }
    );
}

#[test]
fn mark_slots_alternate_and_series_ids_are_tracked() {
    let mut h = H::new(WriterConfig {
        mark_sync: MarkSync::EveryCommit,
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    h.submit(vec![definition(5, None), fact(5, T0, 0)]);
    h.step();
    h.submit(vec![definition(3, None)]);
    h.step();
    let file = h.fs.read_durable(&mark_path()).unwrap();
    let slots = kvik_log::mark::read_slots(&file, &STORE_ID);
    let counters: Vec<u64> = slots
        .iter()
        .map(|s| match s {
            kvik_log::mark::SlotRead::Valid(m) => m.counter,
            _ => 0,
        })
        .collect();
    assert_eq!(counters, vec![3, 2]);
    assert_eq!(durable_mark(&h.fs).max_series_id, 5);
    assert_eq!(h.w.stats().max_series_id, 5);
}

#[test]
fn a_failed_sync_makes_the_store_read_only_and_acknowledges_nothing_it_covered() {
    // Planned after the first segment was created: the first `.open` write is the header rewrite that takes it
    // into use, the first `.open` sync is the batch's.
    for (op, path) in [
        (MemOp::Sync, ".open"),
        (MemOp::Write, ".open"),
        (MemOp::Write, "MARK"),
        (MemOp::Sync, "MARK"),
    ] {
        let fs = MemFs::new();
        let mut h = H::with(
            WriterConfig {
                mark_sync: MarkSync::EveryCommit,
                min_sync_interval: Duration::ZERO,
                ..cfg()
            },
            Some(BOOT),
            fs.clone(),
        );
        fs.fail_nth(op, path, 0, io::ErrorKind::Other);
        let a = h.submit(vec![fact(1, T0, 0)]);
        let b = h.submit(vec![fact(2, T0, 0)]);
        h.step();
        let fault = h.w.fault().cloned().expect("read-only");
        assert_eq!(
            h.done[&a],
            Err(CommitError::OutcomeUnknown),
            "{op:?} {path}"
        );
        assert_eq!(h.done[&b], Err(CommitError::OutcomeUnknown));
        let ops = fs.ops().len();
        let c = h.submit(vec![fact(3, T0, 0)]);
        h.advance(Duration::from_secs(10));
        h.step();
        assert_eq!(h.done[&c], Err(CommitError::ReadOnly));
        assert!(h.w.shutdown(h.now).is_err());
        assert_eq!(fs.ops().len(), ops, "no file operation after a fault");
        assert_eq!(durable_mark(&fs).last_sequence, 0);
        match (op, path) {
            (MemOp::Sync, "MARK") => assert_eq!(fault.op, FaultOp::MarkSync),
            (MemOp::Write, "MARK") => assert_eq!(fault.op, FaultOp::MarkWrite),
            (MemOp::Sync, _) => assert_eq!(fault.op, FaultOp::Sync),
            _ => assert_eq!(fault.op, FaultOp::Write),
        }
        check(&h);
    }
}

#[test]
fn segments_roll_and_seal_into_files_the_scanner_reads_back_completely() {
    let mut h = H::new(WriterConfig {
        segment_size: 8 << 10,
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    h.submit(vec![
        definition(
            1,
            Some(IntervalLength::Fixed {
                length: 60_000_000_000,
                phase: 0,
            }),
        ),
        definition(2, None),
    ]);
    for i in 0..200 {
        h.submit(vec![fact(1 + i % 2, T0 + i as i64, 20)]);
        h.step();
    }
    h.drain();
    h.w.shutdown(h.now).unwrap();
    let segs = segments(&h.fs, false);
    assert!(segs.len() >= 4, "{} segments", segs.len());
    let mut next = 1;
    let mut total = 0;
    let last_id = *segs.keys().max().unwrap();
    for (id, (name, bytes)) in &segs {
        let r = scan(bytes, &ident(name), ScanLimit::End).unwrap();
        let h0 = FileHeader::decode(bytes).unwrap();
        if r.batches.is_empty() {
            assert!(
                !name.sealed && h0.base_sequence == 0,
                "only the segment created ahead is empty"
            );
            assert_eq!(*id, last_id);
            continue;
        }
        assert_eq!(h0.base_sequence, next);
        assert_eq!(r.batches[0].commit.first_sequence, next);
        for b in &r.batches {
            assert_eq!(b.commit.first_sequence, next);
            next = b.commit.last_sequence + 1;
            total += b.entry_count();
        }
        if name.sealed {
            let seal = r.seal.as_ref().expect("a sealed segment has its seal");
            assert_eq!(seal.state, SealState::Sealed);
            assert!(seal.file_crc_matches(bytes));
            assert_eq!(seal.trailer.batch_count, r.batches.len() as u64);
            assert_eq!(seal.trailer.last_sequence, next - 1);
            assert_eq!(seal.end, bytes.len() as u64);
            let index = seal.index.clone().flatten().expect("an index");
            let facts: u32 = index.records.iter().map(|r| r.fact_count).sum();
            let counted = r
                .batches
                .iter()
                .flat_map(|b| BatchContent::decode(b).unwrap().items)
                .filter(|i| matches!(i, Item::Fact(_)))
                .count();
            assert_eq!(facts as usize, counted);
            for rec in &index.records {
                // Series 1 is a fixed interval of 60 s: its reach is t + d (A.8).
                let d = if rec.series_id == 1 {
                    60_000_000_000
                } else {
                    0
                };
                assert!(rec.max_t >= rec.min_t + d);
            }
        } else {
            assert!(r.seal.is_none());
        }
    }
    assert_eq!(total, 202);
    assert_names_durable(&h.fs.ops());
    assert_eq!(
        h.w.stats().sealed_segments as usize,
        segs.values().filter(|s| s.0.sealed).count()
    );
    check(&h);
}

#[test]
fn a_timed_seal_takes_the_next_segment_into_use_without_moving_the_mark() {
    let mut h = H::new(WriterConfig {
        seal_interval: Duration::from_secs(1),
        mark_sync: MarkSync::EveryCommit,
        ..cfg()
    });
    h.submit(vec![fact(1, T0, 0)]);
    h.step();
    // The seal time is armed once the next segment is created ahead.
    let next = h.step();
    assert_eq!(next, Some(Duration::from_secs(1)));
    let before = durable_mark(&h.fs);
    h.advance(Duration::from_secs(1));
    h.step();
    let segs = segments(&h.fs, true);
    let (n1, _) = &segs[&1];
    assert!(n1.sealed);
    let (n2, bytes2) = &segs[&2];
    assert!(!n2.sealed);
    assert_eq!(FileHeader::decode(bytes2).unwrap().base_sequence, 2);
    assert_eq!(
        durable_mark(&h.fs),
        before,
        "the mark keeps naming the sealed segment"
    );
    // An empty segment is never sealed by time.
    h.advance(Duration::from_secs(5));
    h.step();
    assert!(!segments(&h.fs, true)[&2].0.sealed);
    h.submit(vec![fact(1, T0, 0)]);
    h.step();
    let m = durable_mark(&h.fs);
    assert_eq!((m.file_id, m.last_sequence), (2, 2));
    check(&h);
}

/// A writer whose first segment is due for a timed seal at the next step, with `fault` planned first.
fn due_for_a_seal(fault: impl FnOnce(&MemFs)) -> H {
    let mut h = H::new(WriterConfig {
        seal_interval: Duration::from_secs(1),
        ..cfg()
    });
    fault(&h.fs);
    h.submit(vec![fact(1, T0, 0)]);
    h.step();
    h.advance(Duration::from_secs(1));
    h.step();
    h
}

fn assert_blocked_then_sealed(mut h: H) {
    assert!(h.w.fault().is_none());
    assert_eq!(h.w.reasons(), vec![Reason::SealBlocked { file_id: 1 }]);
    assert!(!segments(&h.fs, false)[&1].0.sealed);
    h.advance(Duration::from_secs(10));
    h.step();
    assert!(h.w.reasons().is_empty());
    assert!(segments(&h.fs, false)[&1].0.sealed);
    assert_eq!(h.w.stats().sealed_segments, 1);
    check(&h);
}

#[test]
fn a_seal_blocked_by_a_sharing_error_is_retried() {
    // At the rename and at the length change (Q-115 (m)).
    assert_blocked_then_sealed(due_for_a_seal(|fs| {
        fs.fail_nth(MemOp::Rename, ".kvl", 0, io::ErrorKind::ResourceBusy)
    }));
    assert_blocked_then_sealed(due_for_a_seal(|fs| {
        fs.fail_nth(MemOp::SetLen, "1.open", 0, io::ErrorKind::ResourceBusy)
    }));
}

#[test]
fn no_space_at_a_seal_rename_is_retried() {
    assert_blocked_then_sealed(due_for_a_seal(|fs| {
        fs.fail_nth(MemOp::Rename, ".kvl", 0, io::ErrorKind::StorageFull)
    }));
}

#[test]
fn a_failed_rename_or_length_change_of_a_seal_is_a_write_failure() {
    // Any error that is not a sharing error (lack of space too, at the length change) is never retried
    // (Q-115 (m)).
    for (op, path, kind) in [
        (MemOp::Rename, ".kvl", io::ErrorKind::PermissionDenied),
        (MemOp::Rename, ".kvl", io::ErrorKind::Other),
        (MemOp::SetLen, "1.open", io::ErrorKind::Other),
        (MemOp::SetLen, "1.open", io::ErrorKind::StorageFull),
    ] {
        let mut h = due_for_a_seal(|fs| fs.fail_nth(op, path, 0, kind));
        let f = h.w.fault().cloned().expect("a write failure");
        assert_eq!((f.op, f.kind), (FaultOp::Write, kind), "{op:?} {kind:?}");
        assert!(h.w.reasons().is_empty(), "not a blocked seal");
        assert!(!segments(&h.fs, false)[&1].0.sealed);
        let n = h.fs.ops().len();
        h.advance(Duration::from_secs(10));
        let a = h.submit(vec![fact(1, T0 + 1, 0)]);
        h.step();
        assert_eq!(h.done[&a], Err(CommitError::ReadOnly));
        assert_eq!(h.fs.ops().len(), n, "no retry, nothing after the fault");
    }
}

#[test]
fn a_rename_whose_flush_fails_is_a_failed_sync_not_retried() {
    // `StdFs` on Windows renames and then flushes the file; a failed flush comes back from the rename with the
    // name already changed. Retrying would fail with not-found for ever and mask the failed sync.
    let mut h =
        due_for_a_seal(|fs| fs.fail_nth(MemOp::RenameFlush, ".kvl", 0, io::ErrorKind::Other));
    let f = h.w.fault().cloned().expect("a failed sync");
    assert_eq!(f.op, FaultOp::Sync);
    assert!(f.path.to_string_lossy().ends_with("1.kvl"), "{:?}", f.path);
    assert!(h.w.reasons().is_empty(), "not a blocked seal");
    let n = h.fs.ops().len();
    h.advance(Duration::from_secs(10));
    h.step();
    assert_eq!(h.fs.ops().len(), n, "never retried");
}

/// On Windows, another program's handle that does not share delete access blocks the seal's rename; once it is
/// closed the retried seal finishes (`format.md` A.13, Q-115 (m)).
#[cfg(windows)]
#[test]
fn a_seal_blocked_by_another_handle_finishes_after_its_release() {
    use kvik_log::fs::StdFs;
    use std::os::windows::fs::OpenOptionsExt;

    let root = std::env::temp_dir().join(format!("kvik-log-share-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let fs: Arc<dyn Fs> = Arc::new(StdFs);
    let cfg = WriterConfig {
        seal_interval: Duration::from_secs(1),
        ..cfg()
    };
    let store = StoreDir::open(fs.clone(), &root, Some(&new_store())).unwrap();
    let mut w = LogWriter::open(store, cfg.clone(), Some(BOOT), Duration::ZERO, T0).unwrap();
    let mut q = SharedQueue(Arc::new(Mutex::new(IngestQueue::new(cfg.ingest_queue))));
    let create = |w: &mut LogWriter| {
        if let Some(req) = w.segment_request(T0) {
            let r = create_segment(&*fs, &req, &w.halt());
            w.segment_created(r);
        }
    };
    let seal = |w: &mut LogWriter, now: Duration| {
        for job in w.take_seal_jobs(now) {
            let r = seal_segment_described(&*fs, &job, &w.halt());
            w.seal_done(job, r, now);
        }
    };
    create(&mut w);
    let call = Call::new(vec![fact(1, T0, 0)], w.config().call_limit()).unwrap();
    q.0.lock().unwrap().push(call).unwrap();
    let s = w.step(Duration::ZERO, T0, &mut q);
    assert_eq!(s.done.len(), 1);
    create(&mut w);
    // The timed seal takes segment 2 into use and hands segment 1 over to be sealed.
    let mut now = Duration::from_secs(1);
    w.step(now, T0, &mut q);
    let name = |sealed| {
        SegmentName {
            generation: 0,
            file_id: 1,
            sealed,
        }
        .to_string()
    };
    let open = root.join("log").join(name(false));
    // Shares read and write (the engine's handle writes), not delete.
    let other = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1 | 2)
        .open(&open)
        .unwrap();
    seal(&mut w, now);
    assert!(w.fault().is_none(), "{:?}", w.fault());
    assert_eq!(w.reasons(), vec![Reason::SealBlocked { file_id: 1 }]);
    assert!(open.exists());
    drop(other);
    now += Duration::from_secs(10);
    w.step(now, T0, &mut q);
    seal(&mut w, now);
    assert!(w.fault().is_none(), "{:?}", w.fault());
    assert!(w.reasons().is_empty());
    assert!(root.join("log").join(name(true)).exists());
    assert_eq!(w.stats().sealed_segments, 1);
    drop(w);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_creation_without_space_deletes_the_partial_file_waits_and_retries() {
    let fs = MemFs::new();
    fs.set_free_space(40 << 10);
    let mut h = H::with(cfg(), Some(BOOT), fs.clone());
    assert!(
        segments(&fs, false).is_empty(),
        "the partial file is deleted"
    );
    assert!(h.w.fault().is_none());
    let a = h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert!(!h.done.contains_key(&a));
    assert!(matches!(h.w.waiting(), Some(Wait::DiskCritical { free }) if free < 40 << 10));
    assert!(
        h.w.reasons()
            .iter()
            .any(|r| matches!(r, Reason::DiskCritical { .. }))
    );
    fs.set_free_space(1 << 30);
    h.advance(Duration::from_secs(1));
    h.step();
    h.step();
    assert!(h.done[&a].is_ok());
    assert_eq!(h.w.waiting(), None);
    // The failed creation's id is not reused.
    assert_eq!(
        segments(&fs, false).keys().copied().collect::<Vec<_>>(),
        vec![2, 3]
    );
    check(&h);
}

#[test]
fn with_free_space_unknown_a_failed_creation_is_retried_after_30_seconds() {
    let fs = MemFs::new();
    fs.set_free_space_unknown(true);
    fs.set_free_space(40 << 10);
    let mut h = H::with(cfg(), Some(BOOT), fs.clone());
    assert!(
        segments(&fs, false).is_empty(),
        "the partial file is deleted"
    );
    assert!(h.w.fault().is_none());
    let creations = |fs: &MemFs| {
        fs.ops()
            .iter()
            .filter(|o| o.op == MemOp::Create && o.path.to_string_lossy().ends_with(".open"))
            .count()
    };
    assert_eq!(creations(&fs), 1);
    // The first check after the failure starts the 30 s; each try zero-fills until it fails, so none comes sooner.
    let next = h.step();
    assert_eq!(next, Some(Duration::from_secs(30)));
    fs.set_free_space(1 << 30);
    for s in 1..30 {
        h.advance(Duration::from_secs(1));
        h.step();
        assert_eq!(creations(&fs), 1, "no new request at {s} s");
    }
    h.advance(Duration::from_secs(1));
    h.step();
    assert_eq!(creations(&fs), 2);
    assert_eq!(
        segments(&fs, false).keys().copied().collect::<Vec<_>>(),
        vec![2]
    );
    let a = h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert!(h.done[&a].is_ok());
    check(&h);
}

#[test]
fn free_space_below_the_critical_mark_makes_ingest_wait() {
    let fs = MemFs::new();
    let mut h = H::with(
        WriterConfig {
            disk_critical: 1 << 30,
            disk_low_bytes: 2 << 30,
            ..cfg()
        },
        Some(BOOT),
        fs.clone(),
    );
    fs.set_free_space(100 << 20);
    h.advance(Duration::from_secs(1));
    let a = h.submit(vec![fact(1, T0, 0)]);
    let next = h.step();
    assert!(!h.done.contains_key(&a));
    assert!(matches!(h.w.waiting(), Some(Wait::DiskCritical { .. })));
    assert_eq!(next, Some(Duration::from_secs(2)));
    fs.set_free_space(3 << 30);
    h.advance(Duration::from_secs(1));
    h.step();
    assert!(h.done[&a].is_ok());
    assert!(h.w.reasons().is_empty());
    fs.set_free_space(1536 << 20);
    h.advance(Duration::from_secs(1));
    h.step();
    assert!(matches!(h.w.reasons()[..], [Reason::DiskLow { .. }]));
}

#[test]
fn commit_time_never_decreases_when_the_wall_clock_steps_back() {
    let mut h = H::new(WriterConfig {
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    h.submit(vec![fact(1, T0, 0)]);
    h.step();
    h.wall -= 1_000_000_000;
    h.submit(vec![fact(1, T0, 0)]);
    h.step();
    let b = batches(&h.fs, true);
    assert_eq!(b[0].2.received_at, b[1].2.received_at);
}

fn recovery() -> Item {
    Item::from(Entry::Recovery(Recovery {
        after: 0,
        stamp: None,
        boot_id: BOOT,
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: MarkState::Readable,
        id_floor: 0,
        consumers: vec![],
    }))
}

#[test]
fn a_call_holding_a_recovery_entry_is_alone_in_its_batch() {
    let mut h = H::new(WriterConfig {
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    h.submit(vec![fact(1, T0, 0)]);
    h.submit(vec![recovery()]);
    h.submit(vec![fact(2, T0, 0)]);
    h.drain();
    let b = batches(&h.fs, true);
    assert_eq!(b.len(), 3);
    assert_eq!(b[1].2.items, vec![recovery()]);
    check(&h);
}

/// A source that hands over every call at once, as a source that ignores the batch rules would.
struct Greedy(Vec<Call>);

impl CallSource for Greedy {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    fn take(&mut self, _: u64) -> Vec<Call> {
        std::mem::take(&mut self.0)
    }
    fn put_back(&mut self, mut calls: Vec<Call>) {
        calls.append(&mut self.0);
        self.0 = calls;
    }
}

#[test]
fn calls_that_do_not_encode_together_are_committed_one_by_one() {
    let mut h = H::new(WriterConfig {
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    let limit = h.w.config().call_limit();
    let mut src = Greedy(
        [vec![fact(1, T0, 0)], vec![recovery()], vec![fact(2, T0, 0)]]
            .into_iter()
            .map(|i| Call::new(i, limit).unwrap())
            .collect(),
    );
    let s = h.w.step(h.now, h.wall, &mut src);
    assert_eq!(s.done.len(), 1);
    assert_eq!(src.0.len(), 2);
    for _ in 0..2 {
        h.w.step(h.now, h.wall, &mut src);
    }
    assert!(src.is_empty());
    let b = batches(&h.fs, true);
    assert_eq!(b.len(), 3);
    assert_eq!(b[1].2.items, vec![recovery()]);
}

#[derive(Clone, Debug)]
enum Op {
    Submit {
        facts: usize,
        text: usize,
        entry: bool,
    },
    Advance(u64),
    Step,
    /// kvik-maint creates the segment asked for: late, whenever this runs.
    Create,
    /// kvik-maint takes the seal jobs; they stay in flight until [`Op::SealRun`].
    SealTake,
    /// kvik-maint runs the seal jobs in flight.
    SealRun,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (1..40usize, 0..60usize, prop::bool::weighted(0.15))
            .prop_map(|(facts, text, entry)| Op::Submit { facts, text, entry }),
        2 => (0..60u64).prop_map(Op::Advance),
        4 => Just(Op::Step),
        2 => Just(Op::Create),
        1 => Just(Op::SealTake),
        1 => Just(Op::SealRun),
    ]
}

type Planned = Option<(MemOp, &'static str, usize, io::ErrorKind)>;

fn fault() -> impl Strategy<Value = Planned> {
    let kind = prop_oneof![Just(io::ErrorKind::Other), Just(io::ErrorKind::StorageFull)];
    prop_oneof![
        4 => Just(None),
        2 => (0..40usize, kind.clone()).prop_map(|(k, e)| Some((MemOp::Sync, ".open", k, e))),
        2 => (0..300usize, kind).prop_map(|(k, e)| Some((MemOp::Write, ".open", k, e))),
        1 => (0..40usize).prop_map(|k| Some((MemOp::Write, "MARK", k, io::ErrorKind::Other))),
        1 => (0..10usize).prop_map(|k| Some((MemOp::Sync, "MARK", k, io::ErrorKind::Other))),
        1 => (0..6usize).prop_map(|k| Some((MemOp::Create, ".open", k, io::ErrorKind::Other))),
        // A seal's length change or rename: blocked by a sharing error (and the rename by lack of space), a write
        // failure otherwise; a rename whose flush fails is a failed sync (Q-115 (m)).
        1 => (0..4usize, prop_oneof![Just(io::ErrorKind::Other), Just(io::ErrorKind::ResourceBusy)])
            .prop_map(|(k, e)| Some((MemOp::SetLen, ".open", k, e))),
        1 => (
            0..4usize,
            prop_oneof![
                Just(io::ErrorKind::PermissionDenied),
                Just(io::ErrorKind::ResourceBusy),
                Just(io::ErrorKind::StorageFull),
            ],
        )
            .prop_map(|(k, e)| Some((MemOp::Rename, ".kvl", k, e))),
        1 => (0..4usize).prop_map(|k| Some((MemOp::RenameFlush, ".kvl", k, io::ErrorKind::Other))),
        1 => (0..10usize).prop_map(|k| Some((MemOp::SyncDir, "log", k, io::ErrorKind::Other))),
    ]
}

proptest! {
    /// Generated histories with the maintenance work interleaved (late creations, seals in flight), an optional
    /// injected fault and a crash image taken at one sync: acknowledged batches are exactly the synced ones and
    /// survive a power loss at any sync, positions are dense and increasing, no file operation follows a fault on
    /// either side, the synced mark never names more than synced data, and the writer never asks to run again at
    /// once with nothing to do.
    #[test]
    fn histories(
        ops in prop::collection::vec(op(), 1..100),
        fault in fault(),
        every in any::<bool>(),
        crash_at in 0..80usize,
    ) {
        let fs = MemFs::new();
        let mut h = H::with(
            WriterConfig {
                segment_size: 16 << 10,
                max_batch_bytes: 4 << 10,
                seal_interval: Duration::from_millis(100),
                mark_sync: if every { MarkSync::EveryCommit } else { MarkSync::Interval(Duration::from_millis(50)) },
                ..cfg()
            },
            Some(BOOT),
            fs.clone(),
        );
        h.maint = false;
        if let Some((op, path, k, kind)) = fault {
            fs.fail_nth(op, path, k, kind);
        }
        // The crash image at the `crash_at`-th sync, with the calls acknowledged by then.
        let crash: Arc<Mutex<Option<(usize, Image)>>> = Arc::new(Mutex::new(None));
        let (count, c2, f2, n2) = (Arc::new(Mutex::new(0usize)), crash.clone(), fs.clone(), h.acked_n.clone());
        fs.set_sync_hook(Some(Arc::new(move |_: &Path| {
            let mut n = count.lock().unwrap();
            if *n == crash_at {
                *c2.lock().unwrap() = Some((*n2.lock().unwrap(), image(&f2)));
            }
            *n += 1;
        })));
        let mut series = 0u32;
        let mut frozen: Option<usize> = None;
        for o in ops {
            match o {
                Op::Submit { facts, text, entry } => {
                    let mut items: Vec<Item> = (0..facts).map(|i| fact(i as u32 % 5, T0 + i as i64, text)).collect();
                    if entry {
                        series += 1;
                        items.insert(0, alias(series));
                    }
                    h.submit(items);
                }
                Op::Advance(ms) => h.advance(MS * ms as u32),
                Op::Step => {
                    // A turn that asks to run again at once makes progress: within the budget's burst of batch
                    // syncs and two more turns at the same time, the writer has nothing left to do now.
                    let mut next = h.step();
                    for _ in 0..SYNC_BURST as usize + 2 {
                        if next.is_some_and(|t| t <= h.now) {
                            next = h.step();
                        }
                    }
                    let idle_ok = h.w.fault().is_some()
                        || h.w.reasons().iter().any(|r| matches!(r, Reason::SealBlocked { .. }));
                    prop_assert!(idle_ok || next.is_none_or(|t| t > h.now), "asks to run again at once: {:?}", next);
                }
                Op::Create => h.create(),
                Op::SealTake => h.seal_take(),
                Op::SealRun => h.seal_run(),
            }
            if let Some(n) = frozen {
                prop_assert_eq!(fs.ops().len(), n, "a file operation after a fault");
            } else if h.w.halt().is_set() {
                frozen = Some(fs.ops().len());
            }
            check(&h);
        }
        h.maint = true;
        h.seal_run();
        h.drain();
        if !h.w.halt().is_set() {
            prop_assert!(h.q.is_empty());
            prop_assert_eq!(h.done.len(), h.calls.len());
        }
        let _ = h.w.shutdown(h.now);
        fs.set_sync_hook(None);
        if let Some(n) = frozen {
            prop_assert_eq!(fs.ops().len(), n);
        } else if h.w.fault().is_none() {
            // After a clean shutdown the synced mark covers every acknowledged call.
            let last = h.acked().last().map_or(0, |(_, a)| a.last.sequence);
            prop_assert_eq!(durable_mark(&fs).last_sequence, last);
            prop_assert_eq!(h.w.stats().durable, Position { generation: 0, sequence: last });
            if fault.is_none() {
                assert_names_durable(&fs.ops());
            }
        }
        check(&h);
        if let Some((acked, img)) = crash.lock().unwrap().take() {
            // A sync in flight may cover one batch not yet acknowledged.
            check_image(&img, &h.calls, acked, 1);
        }
        // A power loss now: the files hold every acknowledged call.
        let (calls, acked) = (h.calls.clone(), h.acked().len());
        let extra = usize::from(h.w.halt().is_set());
        drop(h);
        fs.power_loss();
        let after = Image {
            segments: segments(&fs, false),
            mark: fs.read(&mark_path()).unwrap(),
        };
        check_image(&after, &calls, acked, extra);
    }
}
