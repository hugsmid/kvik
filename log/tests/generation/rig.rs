//! The rig the generation tests share: a writer on a `MemFs`, and a restored directory as a restore leaves it.

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
use kvik_log::manifest::{LostRange, read_manifest};
use kvik_log::mark::{MarkSlot, ResetStamp, read_mark};
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::setaside::{RestoredEnd, SetAside};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource as _, GenerationStart, IngestQueue, LogWriter, MarkSync, OpenError, Position,
    StartOptions, StartReport, WriterConfig, create_segment, init_generation,
    seal_segment_described,
};

pub const STORE_ID: [u8; 16] = [6; 16];
pub const T0: i64 = 1_767_225_600_000_000_000;
pub const MS: Duration = Duration::from_millis(1);
pub const HALF_HOUR: i64 = 1_800_000_000_000;

pub fn dir() -> PathBuf {
    PathBuf::from("store")
}

pub fn log_path(name: &str) -> PathBuf {
    dir().join("log").join(name)
}

pub fn new_store() -> NewStore {
    NewStore {
        store_id: STORE_ID,
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    }
}

pub fn cfg() -> WriterConfig {
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

pub fn fact(series: u32, t: i64) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t: T0 + t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(t as u64),
        changed_at: None,
    })
}

pub fn interval_series(id: u32) -> Item {
    Item::from(Entry::Definition(Box::new(Definition {
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

pub fn routing(consumer: &str) -> Item {
    Item::from(Entry::Routing(Box::new(Routing {
        consumer: consumer.into(),
        op: RouteOp::Emits,
        funnel: None,
        selection: None,
        operator: None,
        emits: Selection::None,
        condition: None,
        gate: None,
    })))
}

pub fn stamp() -> ResetStamp {
    ResetStamp {
        sequence: 1,
        nonce: 77,
    }
}

/// A writer on one file system, started with `start`, and every acknowledged position.
pub struct Rig {
    pub fs: MemFs,
    pub boot: u8,
    pub cfg: WriterConfig,
    pub start: Option<GenerationStart>,
    pub w: Option<LogWriter>,
    pub q: IngestQueue,
    pub calls: BTreeMap<u64, Vec<Item>>,
    pub now: Duration,
    pub wall: i64,
    pub acked: Vec<Position>,
}

impl Rig {
    pub fn on(fs: MemFs, cfg: WriterConfig, start: Option<GenerationStart>) -> Self {
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

    pub fn try_open(&mut self) -> Result<StartReport, OpenError> {
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

    pub fn open(&mut self) -> StartReport {
        self.try_open().expect("the start finishes")
    }

    pub fn submit(&mut self, items: Vec<Item>) {
        let call = Call::new(items.clone(), self.cfg.call_limit()).unwrap();
        let id = self.q.push(call).unwrap();
        self.calls.insert(id, items);
    }

    pub fn maint(&mut self) {
        let Some(w) = self.w.as_mut() else { return };
        let fs = self.fs.clone();
        for _ in 0..2 {
            if let Some(req) = w.segment_request(self.wall) {
                let r = create_segment(&fs, &req, &w.halt());
                w.segment_created(r);
            }
            for job in w.take_seal_jobs(self.now) {
                let r = seal_segment_described(&fs, &job, &w.halt());
                w.seal_done(job, r, self.now);
            }
        }
    }

    pub fn step(&mut self) {
        let Some(w) = self.w.as_mut() else { return };
        let s = w.step(self.now, self.wall, &mut self.q);
        for (_, r) in s.done {
            if let Ok(a) = r {
                self.acked.push(a.first);
            }
        }
    }

    pub fn advance(&mut self, d: Duration) {
        self.now += d;
        self.wall += d.as_nanos() as i64;
    }

    pub fn drain(&mut self) {
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
    pub fn timed_seal(&mut self) {
        self.advance(Duration::from_secs(301));
        for _ in 0..10 {
            self.step();
            self.maint();
            self.advance(10 * MS);
        }
    }

    pub fn kill(&mut self) {
        self.w = None;
        self.fs.restart();
    }

    pub fn mark(&self) -> MarkSlot {
        let b = self.fs.read(&log_path("MARK")).unwrap();
        read_mark(&b, &STORE_ID).unwrap().unwrap().1
    }

    pub fn segments(&self) -> Vec<(SegmentName, PathBuf)> {
        let mut v: Vec<(SegmentName, PathBuf)> = self
            .fs
            .paths()
            .into_iter()
            .filter_map(|p| Some((SegmentName::parse(p.file_name()?.to_str()?)?, p.clone())))
            .collect();
        v.sort_by_key(|s| s.0.file_id);
        v
    }

    pub fn lost_ranges(&self) -> Vec<(u32, LostRange)> {
        read_manifest(&self.fs.read(&dir().join("MANIFEST")).unwrap(), &STORE_ID)
            .unwrap()
            .records
            .into_iter()
            .filter_map(|r| r.lost.map(|l| (r.generation, l)))
            .collect()
    }

    /// The reach (`max_t - min_t`) of `series` in the segment indexes of sealed segments of `generation`.
    pub fn reach(&self, generation: u32, series: u32) -> Option<i64> {
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

/// Sealed segment files, as a restore installs them.
pub type Files = Vec<(SegmentName, Vec<u8>)>;

/// The sealed segments in `r`'s directory.
pub fn sealed_files(r: &Rig) -> Files {
    r.segments()
        .into_iter()
        .filter(|(n, _)| n.sealed)
        .map(|(n, p)| (n, r.fs.read(&p).unwrap()))
        .collect()
}

/// Generation 0's log: a sealed segment defining interval series 5 and routing to `kept`, then one defining series 9
/// and routing to `aside`. Returns the sealed files and the first one's last position, where a restore ends.
pub fn source() -> (Files, u64) {
    let mut r = Rig::on(MemFs::new(), cfg(), None);
    r.open();
    r.submit(vec![interval_series(5), routing("kept")]);
    r.drain();
    r.timed_seal();
    let end = r.acked.last().unwrap().sequence + 1;
    r.submit(vec![interval_series(9), routing("aside"), fact(9, 0)]);
    r.drain();
    r.timed_seal();
    let files = sealed_files(&r);
    assert_eq!(files.len(), 2);
    (files, end)
}

pub fn generation_start(end: u64) -> GenerationStart {
    GenerationStart {
        generation: 1,
        stamp: stamp(),
        set_aside: SetAside::new(vec![RestoredEnd {
            generation: 1,
            end: (0, end),
        }]),
    }
}

/// A new directory holding `files`, synced, before `init_generation`.
pub fn installed(files: &Files) -> MemFs {
    let fs = MemFs::new();
    drop(StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap());
    for (n, bytes) in files {
        let f = fs
            .open(&log_path(&n.to_string()), OpenMode::CreateNew)
            .unwrap();
        f.write_at(bytes, 0).unwrap();
        f.sync_all().unwrap();
    }
    fs.sync_dir(&dir().join("log")).unwrap();
    fs
}

/// A new directory holding `files` and the first mark of `start`, as a restore leaves it.
pub fn restored_from(files: &Files, start: GenerationStart, cfg: WriterConfig) -> Rig {
    let fs = installed(files);
    let store = StoreDir::open(Arc::new(fs.clone()), &dir(), None).unwrap();
    init_generation(&store, &start, T0).unwrap();
    drop(store);
    Rig::on(fs, cfg, Some(start))
}

/// [`source`]'s files restored in generation 1.
pub fn restored(cfg: WriterConfig) -> (Rig, u64) {
    let (files, end) = source();
    (restored_from(&files, generation_start(end), cfg), end)
}
