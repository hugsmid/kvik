//! A start in a claimed generation (roadmap 8.3c, `spec.md` §8 Generations, Restore step 1): a directory holding the
//! sealed segments of generation 0, as a restore leaves it, started in generation 1.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::Item;
use kvik_log::entries::{
    Definition, Entry, IntervalLength, Kind, Retention, RouteOp, Routing, Selection, Timing,
    Versioning,
};
use kvik_log::facts::{FactFlags, FactRow, Quality, Value, ValueType};
use kvik_log::fs::{Fs, MemFs, OpenMode};
use kvik_log::manifest::{LossCause, LostRange, RECORD_LOST_RANGE, encode_record, read_manifest};
use kvik_log::mark::{MarkSlot, ResetStamp, read_mark};
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::setaside::{RestoredEnd, SetAside};
use kvik_log::store::{NewStore, StoreDir, StoreError};
use kvik_log::writer::{
    Call, CallSource as _, GenerationStart, IngestQueue, LogWriter, LossKind, MarkSync, OpenError,
    Position, StartOptions, StartReport, StartRule, WriterConfig, create_segment, init_generation,
    seal_segment,
};

const STORE_ID: [u8; 16] = [6; 16];
const T0: i64 = 1_767_225_600_000_000_000;
const MS: Duration = Duration::from_millis(1);
const HALF_HOUR: i64 = 1_800_000_000_000;

fn dir() -> PathBuf {
    PathBuf::from("store")
}

fn log_path(name: &str) -> PathBuf {
    dir().join("log").join(name)
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
        segment_size: 16 << 10,
        max_batch_bytes: 4 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        mark_sync: MarkSync::EveryCommit,
        ..WriterConfig::default()
    }
}

fn fact(series: u32, t: i64) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t: T0 + t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(t as u64),
        changed_at: None,
    })
}

fn interval_series(id: u32) -> Item {
    Item::Entry(Entry::Definition(Box::new(Definition {
        id,
        name: format!("a/b/energy{id}"),
        kind: Kind::Interval,
        value_type: ValueType::F64,
        scale: None,
        unit: "kWh".into(),
        expected_interval: Some(HALF_HOUR),
        staleness: Some(3 * HALF_HOUR),
        record_on_change: false,
        counter: None,
        lateness_bound: 300_000_000_000,
        state_names: Vec::new(),
        versioning: Versioning::None,
        timing: Timing::Observed,
        interval: Some(IntervalLength::Fixed {
            length: HALF_HOUR,
            phase: 0,
        }),
        retention: Retention::LongTerm,
        excess_precision: None,
        staleness_set: false,
    })))
}

fn routing(consumer: &str) -> Item {
    Item::Entry(Entry::Routing(Box::new(Routing {
        consumer: consumer.into(),
        op: RouteOp::Emits,
        funnel: None,
        selection: None,
        operator: None,
        emits: Selection::None,
        condition: None,
    })))
}

fn stamp() -> ResetStamp {
    ResetStamp {
        sequence: 1,
        nonce: 77,
    }
}

/// A writer on one file system, started with `start`, and every acknowledged position.
struct Rig {
    fs: MemFs,
    boot: u8,
    cfg: WriterConfig,
    start: Option<GenerationStart>,
    w: Option<LogWriter>,
    q: IngestQueue,
    calls: BTreeMap<u64, Vec<Item>>,
    now: Duration,
    wall: i64,
    acked: Vec<Position>,
}

impl Rig {
    fn on(fs: MemFs, cfg: WriterConfig, start: Option<GenerationStart>) -> Self {
        Self {
            fs,
            boot: 1,
            q: IngestQueue::new(cfg.ingest_queue),
            cfg,
            start,
            w: None,
            calls: BTreeMap::new(),
            now: Duration::ZERO,
            wall: T0,
            acked: Vec::new(),
        }
    }

    fn try_open(&mut self) -> Result<StartReport, OpenError> {
        self.w = None;
        let store = StoreDir::open(Arc::new(self.fs.clone()), &dir(), Some(&new_store()))?;
        let opts = StartOptions {
            generation: self.start.clone(),
            ..StartOptions::default()
        };
        let w = LogWriter::open_with(
            store,
            self.cfg.clone(),
            Some([self.boot; 16]),
            self.now,
            self.wall,
            opts,
        )?;
        let report = w.start_report().clone();
        assert!(w.fault().is_none(), "{:?}", w.fault());
        self.w = Some(w);
        self.q = IngestQueue::new(self.cfg.ingest_queue);
        self.calls.clear();
        self.maint();
        Ok(report)
    }

    fn open(&mut self) -> StartReport {
        self.try_open().expect("the start finishes")
    }

    fn submit(&mut self, items: Vec<Item>) {
        let call = Call::new(items.clone(), self.cfg.call_limit()).unwrap();
        let id = self.q.push(call).unwrap();
        self.calls.insert(id, items);
    }

    fn maint(&mut self) {
        let Some(w) = self.w.as_mut() else { return };
        let fs = self.fs.clone();
        for _ in 0..2 {
            if let Some(req) = w.segment_request(self.wall) {
                let r = create_segment(&fs, &req, &w.halt());
                w.segment_created(r);
            }
            for job in w.take_seal_jobs(self.now) {
                let r = seal_segment(&fs, &job, &w.halt());
                w.seal_done(job, r, self.now);
            }
        }
    }

    fn step(&mut self) {
        let Some(w) = self.w.as_mut() else { return };
        let s = w.step(self.now, self.wall, &mut self.q);
        for (_, r) in s.done {
            if let Ok(a) = r {
                self.acked.push(a.first);
            }
        }
    }

    fn advance(&mut self, d: Duration) {
        self.now += d;
        self.wall += d.as_nanos() as i64;
    }

    fn drain(&mut self) {
        for _ in 0..1000 {
            if self.q.is_empty() {
                return;
            }
            self.step();
            self.maint();
            self.advance(10 * MS);
        }
    }

    /// The segment in use is sealed by time (`spec.md` §3 Segments).
    fn timed_seal(&mut self) {
        self.advance(Duration::from_secs(301));
        for _ in 0..10 {
            self.step();
            self.maint();
            self.advance(10 * MS);
        }
    }

    fn kill(&mut self) {
        self.w = None;
        self.fs.restart();
    }

    fn mark(&self) -> MarkSlot {
        let b = self.fs.read(&log_path("MARK")).unwrap();
        read_mark(&b, &STORE_ID).unwrap().unwrap().1
    }

    fn segments(&self) -> Vec<(SegmentName, PathBuf)> {
        let mut v: Vec<(SegmentName, PathBuf)> = self
            .fs
            .paths()
            .into_iter()
            .filter_map(|p| Some((SegmentName::parse(p.file_name()?.to_str()?)?, p.clone())))
            .collect();
        v.sort_by_key(|s| s.0.file_id);
        v
    }

    fn lost_ranges(&self) -> Vec<(u32, LostRange)> {
        read_manifest(&self.fs.read(&dir().join("MANIFEST")).unwrap(), &STORE_ID)
            .unwrap()
            .records
            .into_iter()
            .filter_map(|r| r.lost.map(|l| (r.generation, l)))
            .collect()
    }

    /// The reach (`max_t - min_t`) of `series` in the segment indexes of sealed segments of `generation`.
    fn reach(&self, generation: u32, series: u32) -> Option<i64> {
        let mut reach = None;
        for (n, p) in self.segments() {
            if !n.sealed || n.generation != generation {
                continue;
            }
            let id = SegmentIdentity {
                store_id: STORE_ID,
                file_id: n.file_id,
                generation: n.generation,
            };
            let bytes = self.fs.read(&p).unwrap();
            let rep = scan(&bytes, &id, ScanLimit::End).unwrap();
            if let Some(index) = rep.seal.and_then(|s| s.index.flatten()) {
                for rec in index.records.iter().filter(|r| r.series_id == series) {
                    reach = Some(rec.max_t - rec.min_t);
                }
            }
        }
        reach
    }
}

/// Generation 0's log: a sealed segment defining interval series 5 and routing to `kept`, then one defining series 9
/// and routing to `aside`. Returns the sealed files and the first one's last position, where a restore ends.
fn source() -> (Vec<(SegmentName, Vec<u8>)>, u64) {
    let mut r = Rig::on(MemFs::new(), cfg(), None);
    r.open();
    r.submit(vec![interval_series(5), routing("kept")]);
    r.drain();
    r.timed_seal();
    let end = r.acked.last().unwrap().sequence + 1;
    r.submit(vec![interval_series(9), routing("aside"), fact(9, 0)]);
    r.drain();
    r.timed_seal();
    let files: Vec<(SegmentName, Vec<u8>)> = r
        .segments()
        .into_iter()
        .filter(|(n, _)| n.sealed)
        .map(|(n, p)| (n, r.fs.read(&p).unwrap()))
        .collect();
    assert_eq!(files.len(), 2);
    (files, end)
}

fn generation_start(end: u64) -> GenerationStart {
    GenerationStart {
        generation: 1,
        stamp: stamp(),
        set_aside: SetAside::new(vec![RestoredEnd {
            generation: 1,
            end: (0, end),
        }]),
    }
}

/// A new directory holding generation 0's sealed segments and its first mark of generation 1, as a restore leaves it.
fn restored(cfg: WriterConfig) -> (Rig, u64) {
    let (files, end) = source();
    let fs = MemFs::new();
    let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap();
    for (n, bytes) in &files {
        let f = fs
            .open(&log_path(&n.to_string()), OpenMode::CreateNew)
            .unwrap();
        f.write_at(bytes, 0).unwrap();
        f.sync_all().unwrap();
    }
    let start = generation_start(end);
    init_generation(&store, &start, T0).unwrap();
    drop(store);
    (Rig::on(fs, cfg, Some(start)), end)
}

#[test]
fn init_generation_writes_the_first_mark_once_and_refuses_another_generations() {
    let (r, end) = restored(cfg());
    let m = r.mark();
    assert_eq!(
        (m.generation, m.stamp, m.file_id, m.offset, m.last_sequence),
        (1, stamp(), 0, 0, 0)
    );
    let store = StoreDir::open(Arc::new(r.fs.clone()), &dir(), Some(&new_store())).unwrap();
    let before = r.fs.read(&log_path("MARK"));
    init_generation(&store, &generation_start(end), T0).unwrap();
    assert_eq!(r.fs.read(&log_path("MARK")), before);
    let other = GenerationStart {
        generation: 2,
        ..generation_start(end)
    };
    assert!(matches!(
        init_generation(&store, &other, T0),
        Err(StoreError::Generation { found: 1, start: 2 })
    ));
}

#[test]
fn a_restored_generation_takes_ingest_at_its_first_position_with_the_restores_stamp() {
    let (mut r, _) = restored(cfg());
    let rep = r.open();
    assert_eq!(rep.rule, StartRule::FirstStart);
    assert!(rep.entry.is_none());
    assert!(rep.losses.is_empty());
    r.submit(vec![fact(5, 0)]);
    r.drain();
    assert_eq!(
        r.acked,
        vec![Position {
            generation: 1,
            sequence: 1
        }]
    );
    let m = r.mark();
    assert_eq!((m.generation, m.stamp, m.boot_id), (1, stamp(), [1; 16]));
    assert!(r.lost_ranges().is_empty());
}

#[test]
fn earlier_generations_sealed_segments_take_no_part_in_the_chain() {
    let (mut r, _) = restored(cfg());
    r.open();
    r.submit(vec![fact(5, 0)]);
    r.drain();
    r.kill();
    let rep = r.open();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert!(rep.losses.is_empty() && rep.entry.is_none());
    r.submit(vec![fact(5, 1)]);
    r.drain();
    assert_eq!(r.acked.last().unwrap().sequence, 2);
    r.kill();
    r.boot += 1;
    let rep = r.open();
    let e = rep.entry.expect("rule 3 writes an entry");
    assert_eq!((e.entry.after, e.sequence), (2, 3));
    assert!(
        rep.losses.iter().all(|l| l.kind == LossKind::PossibleLoss),
        "{:?}",
        rep.losses
    );
    let m = r.mark();
    assert_eq!((m.generation, m.last_sequence), (1, 3));
}

#[test]
fn set_aside_batches_reserve_their_ids_but_name_no_consumer_and_give_no_interval() {
    let (mut r, _) = restored(cfg());
    r.open();
    assert!(r.w.as_ref().unwrap().stats().max_series_id >= 9);
    r.submit(vec![fact(5, 0), fact(9, 0)]);
    r.drain();
    r.timed_seal();
    assert_eq!(r.reach(1, 5), Some(HALF_HOUR));
    assert_eq!(r.reach(1, 9), Some(0));
    r.kill();
    r.boot += 1;
    let rep = r.open();
    let e = rep.entry.unwrap();
    assert_eq!(e.entry.consumers, vec!["kept".to_string()]);
    assert!(e.entry.id_floor >= 9);
}

#[test]
fn a_start_refuses_a_mark_or_segment_of_another_generation() {
    let (r, end) = restored(cfg());
    let fs = r.fs.clone();
    let mut zero = Rig::on(fs.clone(), cfg(), None);
    assert!(matches!(
        zero.try_open(),
        Err(OpenError::Store(StoreError::Generation {
            found: 1,
            start: 0
        }))
    ));
    let mut two = Rig::on(
        fs.clone(),
        cfg(),
        Some(GenerationStart {
            generation: 2,
            ..generation_start(end)
        }),
    );
    assert!(matches!(
        two.try_open(),
        Err(OpenError::Store(StoreError::Generation {
            found: 1,
            start: 2
        }))
    ));
    let later = SegmentName {
        generation: 2,
        file_id: 40,
        sealed: true,
    };
    let (n, _) = r.segments().into_iter().find(|(n, _)| n.sealed).unwrap();
    let bytes = fs.read(&log_path(&n.to_string())).unwrap();
    let f = fs
        .open(&log_path(&later.to_string()), OpenMode::CreateNew)
        .unwrap();
    f.write_at(&bytes, 0).unwrap();
    let mut one = Rig::on(fs, cfg(), Some(generation_start(end)));
    assert!(matches!(
        one.try_open(),
        Err(OpenError::Store(StoreError::Generation {
            found: 2,
            start: 1
        }))
    ));
}

/// Q-264: position 1 as the chain's start stays with generation 0 (Q-321); a restored generation's lowest unsealed
/// segment gone leaves no gap below the next one.
#[test]
fn the_position_one_start_rule_is_limited_to_generation_0() {
    let small = WriterConfig {
        segment_size: 4096,
        max_batch_bytes: 1024,
        ..cfg()
    };
    let (mut r, _) = restored(small);
    r.open();
    // Segments are created but never sealed, so the lowest of generation 1 stays unsealed.
    for k in 0..400 {
        r.submit(vec![fact(5, k)]);
        r.step();
        let (fs, wall) = (r.fs.clone(), r.wall);
        let w = r.w.as_mut().unwrap();
        if let Some(req) = w.segment_request(wall) {
            let res = create_segment(&fs, &req, &w.halt());
            w.segment_created(res);
        }
        r.advance(10 * MS);
        let open: Vec<_> = r
            .segments()
            .into_iter()
            .filter(|(n, _)| !n.sealed)
            .collect();
        if open.len() >= 4 && r.mark().file_id >= open[2].0.file_id {
            break;
        }
    }
    let lowest = r
        .segments()
        .into_iter()
        .find(|(n, _)| n.generation == 1 && !n.sealed)
        .unwrap();
    r.kill();
    r.fs.remove_file(&lowest.1).unwrap();
    r.fs.restart();
    r.open();
    assert!(r.lost_ranges().is_empty(), "{:?}", r.lost_ranges());
}

/// A rule 2 region found in a restored generation is recorded in that generation, also beside a loss of generation 0
/// at the same sequences, which a restore records (Q-261).
#[test]
fn a_loss_in_a_restored_generation_is_recorded_in_it_beside_one_of_generation_0() {
    let (mut r, _) = restored(cfg());
    r.open();
    for k in 0..3 {
        r.submit(vec![fact(5, k)]);
        r.drain();
    }
    let (n, path) = r
        .segments()
        .into_iter()
        .find(|(n, _)| n.generation == 1 && !n.sealed)
        .unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: n.file_id,
        generation: 1,
    };
    let at = scan(&r.fs.read(&path).unwrap(), &id, ScanLimit::End)
        .unwrap()
        .batches[1]
        .offset;
    r.kill();
    let earlier = LostRange {
        identity: (0, 2),
        first: (0, 2),
        last: (0, 2),
        file_id: 1,
        offset: 0,
        length: 0,
        cause: LossCause::LostInBucket,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: Some(stamp()),
    };
    let manifest = dir().join("MANIFEST");
    let f = r.fs.open(&manifest, OpenMode::ReadWrite).unwrap();
    f.write_at(
        &encode_record(RECORD_LOST_RANGE, 2, 1, T0, Some(&earlier)),
        f.len().unwrap(),
    )
    .unwrap();
    r.fs.poke(&path, at as usize + 40, &[0xff; 8]);
    r.fs.restart();
    r.open();
    let lost: Vec<_> = r
        .lost_ranges()
        .into_iter()
        .map(|(g, l)| (g, l.first, l.last))
        .collect();
    assert_eq!(lost, vec![(1, (0, 2), (0, 2)), (1, (1, 2), (1, 2))]);
}
