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
use kvik_log::fs::{FaultOp as MemOp, Fs, MemFs, OpenMode};
use kvik_log::header::FileHeader;
use kvik_log::mark::{MarkSlot, read_mark};
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, SealState, scan};
use kvik_log::store::{NewStore, StoreDir, StoreError};
use kvik_log::writer::{
    Ack, Call, CallSource, CommitError, EveryCommitReason, FaultOp, IngestQueue, LogWriter,
    MarkSync, MarkSyncMode, OpenError, Position, Reason, Wait, WriterConfig, create_segment,
    seal_segment,
};
use proptest::prelude::*;

const STORE_ID: [u8; 16] = [7; 16];
const BOOT: [u8; 16] = [9; 16];
const T0: i64 = 1_767_225_600_000_000_000;
const MS: Duration = Duration::from_millis(1);

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
    Item::Entry(Entry::Alias {
        id,
        name: format!("a{id}"),
    })
}

fn definition(id: u32, interval: Option<IntervalLength>) -> Item {
    Item::Entry(Entry::Definition(Box::new(Definition {
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
}

impl H {
    fn new(cfg: WriterConfig) -> Self {
        Self::with(cfg, Some(BOOT), MemFs::new())
    }

    fn with(cfg: WriterConfig, boot: Option<[u8; 16]>, fs: MemFs) -> Self {
        let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap();
        let w = LogWriter::open(store, cfg.clone(), boot, Duration::ZERO).unwrap();
        let mut h = Self {
            fs,
            w,
            q: SharedQueue(Arc::new(Mutex::new(IngestQueue::new(cfg.ingest_queue)))),
            now: Duration::ZERO,
            wall: T0,
            calls: BTreeMap::new(),
            done: BTreeMap::new(),
            maint: true,
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

    fn run_maint(&mut self) {
        let fs = self.fs.clone();
        if let Some(req) = self.w.segment_request(self.wall) {
            let r = create_segment(&fs, &req);
            self.w.segment_created(r);
        }
        for job in self.w.take_seal_jobs(self.now) {
            let r = seal_segment(&fs, &job);
            self.w.seal_done(job, r, self.now);
        }
        // A creation asked for after a seal or a roll.
        if let Some(req) = self.w.segment_request(self.wall) {
            let r = create_segment(&fs, &req);
            self.w.segment_created(r);
        }
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
    // The durable batches, as a power loss would leave them.
    let durable = batches(&h.fs, true);
    let mut items: Vec<Item> = Vec::new();
    let mut expect_first = 1;
    for (_, _, c) in &durable {
        assert_eq!(c.first_sequence, expect_first, "dense sequences on disk");
        expect_first = c.last_sequence() + 1;
        items.extend(c.items.iter().cloned());
    }
    let acked_items: Vec<Item> = acked
        .iter()
        .flat_map(|(id, _)| h.calls[id].iter().cloned())
        .collect();
    assert!(
        items.len() >= acked_items.len(),
        "an acknowledged call is not synced"
    );
    assert_eq!(&items[..acked_items.len()], &acked_items[..]);
    if h.w.fault().is_none() {
        assert_eq!(
            items.len(),
            acked_items.len(),
            "a synced batch is not acknowledged"
        );
    } else {
        let extra = durable
            .iter()
            .filter(|(_, _, c)| c.last_sequence() >= next)
            .count();
        assert!(
            extra <= 1,
            "more than one batch beyond the acknowledgements"
        );
        for (id, r) in &h.done {
            if r.is_err() {
                assert!(acked.iter().all(|(a, _)| a < id));
            }
        }
    }
    // The synced mark names the end of a synced batch, or nothing.
    let m = durable_mark(&h.fs);
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
    let w = LogWriter::open(store, cfg(), Some(BOOT), Duration::ZERO).unwrap();
    let m = durable_mark(&fs);
    assert_eq!(
        (m.counter, m.file_id, m.offset, m.last_sequence),
        (1, 0, 0, 0)
    );
    assert_eq!(m.boot_id, BOOT);
    drop(w);
    // Reopening a log that holds a mark is recovery (roadmap 4.5).
    let store = StoreDir::open(afs.clone(), &dir(), None).unwrap();
    assert!(matches!(
        LogWriter::open(store, cfg(), Some(BOOT), Duration::ZERO),
        Err(OpenError::Store(StoreError::NeedsRecovery(_)))
    ));

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

#[test]
fn group_commit_coalesces_under_load_and_adds_no_delay_after_a_quiet_period() {
    let mut h = H::new(cfg());
    let a = h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert!(
        h.done[&a].is_ok(),
        "the first call after a quiet period is synced at once"
    );
    let mut ids = Vec::new();
    for _ in 0..5 {
        h.advance(MS);
        ids.push(h.submit(vec![fact(1, T0, 0), fact(2, T0, 0)]));
        let next = h.step();
        assert_eq!(next, Some(Duration::from_millis(10)));
    }
    assert_eq!(h.done.len(), 1);
    h.advance(Duration::from_millis(5));
    h.step();
    assert!(ids.iter().all(|i| h.done[i].is_ok()));
    assert_eq!(h.w.stats().batches, 2, "five calls in one batch");
    assert_eq!(h.w.stats().syncs, 2);
    // A quiet period: the next call is synced in the same step it arrives.
    h.advance(Duration::from_secs(1));
    let b = h.submit(vec![fact(3, T0, 0)]);
    h.step();
    assert!(h.done[&b].is_ok());
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
    let a = h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert!(h.done[&a].is_ok());
    assert_eq!(
        durable_mark(&h.fs).last_sequence,
        0,
        "facts wait for the timer"
    );
    assert_eq!(h.w.mark().last_sequence, 1);
    h.advance(Duration::from_millis(20));
    h.fs.clear_ops();
    let b = h.submit(vec![alias(1)]);
    h.step();
    assert!(h.done[&b].is_ok());
    assert_eq!(durable_mark(&h.fs).last_sequence, 2);
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
    assert_eq!(durable_mark(&h.fs).last_sequence, 2);
    assert_eq!(next, Some(Duration::from_millis(1_020)));
    h.advance(next.unwrap() - h.now);
    h.step();
    assert_eq!(durable_mark(&h.fs).last_sequence, 3);
    assert_eq!(h.w.stats().mark_synced.sequence, 3);
    // Clean shutdown syncs the mark.
    h.advance(MS);
    h.submit(vec![fact(1, T0, 0)]);
    h.step();
    assert_eq!(durable_mark(&h.fs).last_sequence, 3);
    h.w.shutdown(h.now).unwrap();
    assert_eq!(durable_mark(&h.fs).last_sequence, 4);
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
    let next = h.step();
    assert_eq!(next, Some(Duration::from_secs(1)));
    let before = durable_mark(&h.fs);
    h.advance(Duration::from_secs(1));
    h.step();
    let segs = segments(&h.fs, true);
    let (n1, _) = &segs[&1];
    assert!(n1.sealed);
    let (n2, b2) = &segs[&2];
    assert!(!n2.sealed);
    assert_eq!(FileHeader::decode(b2).unwrap().base_sequence, 2);
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

#[test]
fn a_blocked_seal_is_retried() {
    let mut h = H::new(WriterConfig {
        seal_interval: Duration::from_secs(1),
        ..cfg()
    });
    h.fs.fail_nth(MemOp::Rename, ".kvl", 0, io::ErrorKind::PermissionDenied);
    h.submit(vec![fact(1, T0, 0)]);
    h.step();
    h.advance(Duration::from_secs(1));
    h.step();
    assert!(h.w.fault().is_none());
    assert_eq!(h.w.reasons(), vec![Reason::SealBlocked { file_id: 1 }]);
    assert!(!segments(&h.fs, false)[&1].0.sealed);
    h.advance(Duration::from_secs(10));
    h.step();
    assert!(h.w.reasons().is_empty());
    assert!(segments(&h.fs, false)[&1].0.sealed);
    assert_eq!(h.w.stats().sealed_segments, 1);
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
    Item::Entry(Entry::Recovery(Recovery {
        after: 0,
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
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (1..40usize, 0..60usize, prop::bool::weighted(0.15))
            .prop_map(|(facts, text, entry)| Op::Submit { facts, text, entry }),
        2 => (0..30u64).prop_map(Op::Advance),
        4 => Just(Op::Step),
    ]
}

fn fault() -> impl Strategy<Value = Option<(MemOp, &'static str, usize)>> {
    prop_oneof![
        2 => Just(None),
        1 => (0..40usize).prop_map(|k| Some((MemOp::Sync, ".open", k))),
        1 => (0..400usize).prop_map(|k| Some((MemOp::Write, ".open", k))),
        1 => (0..40usize).prop_map(|k| Some((MemOp::Write, "MARK", k))),
        1 => (0..10usize).prop_map(|k| Some((MemOp::Sync, "MARK", k))),
    ]
}

proptest! {
    /// Generated histories with an optional injected fault: acknowledged batches are exactly the synced ones,
    /// positions are dense and increasing, nothing happens after a fault, and the synced mark never names more
    /// than synced data.
    #[test]
    fn histories(ops in prop::collection::vec(op(), 1..80), fault in fault(), every in any::<bool>()) {
        let fs = MemFs::new();
        let mut h = H::with(
            WriterConfig {
                segment_size: 16 << 10,
                max_batch_bytes: 4 << 10,
                mark_sync: if every { MarkSync::EveryCommit } else { MarkSync::Interval(Duration::from_millis(50)) },
                ..cfg()
            },
            Some(BOOT),
            fs.clone(),
        );
        if let Some((op, path, k)) = fault {
            fs.fail_nth(op, path, k, io::ErrorKind::Other);
        }
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
                Op::Step => { h.step(); }
            }
            if let Some(n) = frozen {
                prop_assert_eq!(fs.ops().len(), n);
            } else if h.w.fault().is_some() {
                frozen = Some(fs.ops().len());
            }
            check(&h);
        }
        h.drain();
        if h.w.fault().is_none() {
            prop_assert!(h.q.is_empty());
            prop_assert_eq!(h.done.len(), h.calls.len());
        }
        let _ = h.w.shutdown(h.now);
        if let Some(n) = frozen {
            prop_assert_eq!(fs.ops().len(), n);
        } else if h.w.fault().is_none() {
            // After a clean shutdown the synced mark covers every acknowledged call.
            let last = h.acked().last().map_or(0, |(_, a)| a.last.sequence);
            prop_assert_eq!(durable_mark(&fs).last_sequence, last);
            prop_assert_eq!(h.w.stats().durable, Position { generation: 0, sequence: last });
        }
        check(&h);
    }
}
