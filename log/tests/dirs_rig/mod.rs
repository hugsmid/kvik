//! The rig of the derived-file tests (`format.md` A.18): a writer, its maintenance work and the derived files,
//! driven step by step on the in-memory file system with a manual clock.

#![allow(dead_code, unused_imports)]

pub use std::collections::BTreeMap;
pub use std::path::PathBuf;
pub use std::sync::{Arc, Mutex};
pub use std::time::Duration;

pub use kvik_log::batch::SegmentIdentity;
pub use kvik_log::content::{BatchContent, Item};
pub use kvik_log::directory::cadence::{READER_ALLOWANCE, SERIES_BYTES, SERIES_INTERVAL};
pub use kvik_log::directory::logdir::{
    self, Catalogue, Group, Listed, LogDirectory, Root, SegmentRecord,
};
pub use kvik_log::directory::series::{Candidate, CandidateValue, SeriesDirectory, SeriesEntry};
pub use kvik_log::directory::status::{StatusFile, counter, health};
pub use kvik_log::directory::{self, DERIVED_DIR, Family, Frame, read_latest};
pub use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
pub use kvik_log::fs::{Fs, MemFs};
pub use kvik_log::manifest::{RECORD_EMPTY, encode_initial, encode_record, read_manifest};
pub use kvik_log::mark::read_mark;
pub use kvik_log::name::SegmentName;
pub use kvik_log::scan::{ScanLimit, scan};
pub use kvik_log::store::{NewStore, StoreDir};
pub use kvik_log::writer::{
    Call, CallSource as _, Clock, Directories, DirectoryOptions, DirectoryStart, IngestQueue,
    LogWriter, MarkSync, Position, SealHook, SeriesEntries, StatusContent, WriterConfig,
    create_segment, directory_record, kept_series, seal_segment_described, writer_catalogue,
};
pub use proptest::prelude::*;

pub const STORE_ID: [u8; 16] = [9; 16];
pub const T0: i64 = 1_767_225_600_000_000_000;
pub const MS: Duration = Duration::from_millis(1);

pub fn dir() -> PathBuf {
    PathBuf::from("store")
}

pub fn cfg() -> WriterConfig {
    WriterConfig {
        segment_size: 4 << 10,
        max_batch_bytes: 2 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        mark_sync: MarkSync::Interval(Duration::from_millis(30)),
        ..WriterConfig::default()
    }
}

#[derive(Debug, Default)]
pub struct ManualClock(pub Mutex<Duration>);

impl ManualClock {
    pub fn advance(&self, d: Duration) {
        *self.0.lock().unwrap() += d;
    }
}

impl Clock for ManualClock {
    fn monotonic(&self) -> Duration {
        *self.0.lock().unwrap()
    }
    fn wall(&self) -> i64 {
        T0 + self.monotonic().as_nanos() as i64
    }
}

/// Complete batches with their first and last sequence.
pub type Batches = Vec<(u64, u64, BatchContent)>;

/// Every complete batch of every segment, with its first and last position, in position order.
pub fn batches(fs: &MemFs) -> Batches {
    let mut out = Vec::new();
    for p in fs.paths() {
        let Some(n) = p
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(SegmentName::parse)
        else {
            continue;
        };
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id: n.file_id,
            generation: n.generation,
        };
        let bytes = fs.read(&p).unwrap();
        let Ok(r) = scan(&bytes, &id, ScanLimit::End) else {
            continue;
        };
        for b in &r.batches {
            out.push((
                b.commit.first_sequence,
                b.commit.last_sequence,
                BatchContent::decode(b).unwrap(),
            ));
        }
    }
    out.sort_by_key(|b| b.0);
    out
}

pub fn facts_of(c: &BatchContent) -> impl Iterator<Item = (u64, &FactRow)> {
    let mut seq = c.first_sequence;
    c.items.iter().filter_map(move |i| {
        let s = seq;
        seq += i.sequences();
        match i {
            Item::Fact(r) => Some((s, r)),
            Item::Entry(_) => None,
        }
    })
}

/// A plain fold of the facts up to a position: what a series directory says in this crate's tests (its meaning is
/// the embedding crate's).
pub fn fold(batches: &Batches, limit: u64, written_at: i64) -> (Position, Vec<SeriesEntry>) {
    let mut m: BTreeMap<u32, SeriesEntry> = BTreeMap::new();
    let mut through = 0;
    for (_, last, c) in batches.iter().filter(|b| b.1 <= limit) {
        through = through.max(*last);
        for (seq, r) in facts_of(c) {
            let e = m.entry(r.series_id).or_insert_with(|| SeriesEntry {
                series_id: r.series_id,
                ..SeriesEntry::default()
            });
            e.fact_count += 1;
            e.latest_t = Some(e.latest_t.map_or(r.t, |t| t.max(r.t)));
            let Value::F64(bits) = r.value else {
                continue;
            };
            if r.quality != Quality::Bad
                && r.t <= written_at
                && e.candidate.is_none_or(|c| c.t <= r.t)
            {
                e.candidate = Some(Candidate {
                    t: r.t,
                    position: Position {
                        generation: 0,
                        sequence: seq,
                    },
                    quality: r.quality,
                    value: CandidateValue::F64(bits),
                });
            }
        }
    }
    (
        Position {
            generation: 0,
            sequence: through,
        },
        m.into_values().collect(),
    )
}

#[derive(Debug)]
pub struct Hook {
    pub fs: MemFs,
    pub health: Mutex<u8>,
    /// Added to the position the entries are as of: a hook that oversteps the limit it is given.
    pub overshoot: Mutex<u64>,
}

impl SealHook for Hook {
    fn series(&self, limit: Position, written_at: i64) -> Option<SeriesEntries> {
        let (mut through, entries) = fold(&batches(&self.fs), limit.sequence, written_at);
        through.sequence += *self.overshoot.lock().unwrap();
        Some(SeriesEntries {
            through,
            judged_at: written_at,
            entries,
        })
    }
    fn status(&self) -> Option<StatusContent> {
        Some(StatusContent {
            health: *self.health.lock().unwrap(),
            pressure: false,
            reasons: Vec::new(),
            counters: vec![(counter::BATCHES, 1)],
        })
    }
}

pub fn fact(series: u32, t: i64, quality: Quality) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t: T0 + t,
        quality,
        flags: FactFlags::default(),
        value: Value::F64(t as u64),
        changed_at: None,
    })
}

/// A writer, its maintenance work and the derived files, driven step by step.
pub struct Rig {
    pub fs: MemFs,
    pub clock: Arc<ManualClock>,
    pub cfg: WriterConfig,
    pub w: Option<LogWriter>,
    pub q: IngestQueue,
    pub dirs: Option<Directories>,
    pub hook: Arc<Hook>,
    /// The series directory's bound on uncovered bytes: small, so that the small test segments reach it.
    pub series_bytes: u64,
    /// Sealed segments not yet handed to the directories (none while a test holds them back).
    pub sealed_records: Vec<SegmentRecord>,
}

impl Rig {
    pub fn new(cfg: WriterConfig) -> Self {
        let fs = MemFs::new();
        let mut r = Rig {
            hook: Arc::new(Hook {
                fs: fs.clone(),
                health: Mutex::new(health::OK),
                overshoot: Mutex::new(0),
            }),
            fs,
            clock: Arc::new(ManualClock::default()),
            q: IngestQueue::new(cfg.ingest_queue),
            cfg,
            w: None,
            dirs: None,
            series_bytes: 1,
            sealed_records: Vec::new(),
        };
        r.open();
        r
    }

    pub fn open(&mut self) {
        self.dirs = None;
        self.w = None;
        let new = NewStore {
            store_id: STORE_ID,
            created_at: T0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "kvik test".into(),
        };
        let store = StoreDir::open(Arc::new(self.fs.clone()), &dir(), Some(&new)).unwrap();
        let w = LogWriter::open(
            store,
            self.cfg.clone(),
            Some([1; 16]),
            self.clock.monotonic(),
            self.clock.wall(),
        )
        .unwrap();
        let mut start = w.directory_start();
        start.series_bytes = self.series_bytes;
        self.dirs = Some(Directories::new(
            Arc::new(self.fs.clone()),
            &dir(),
            STORE_ID,
            start,
            DirectoryOptions {
                hook: Some(self.hook.clone()),
                status_interval: Some(Duration::from_secs(60)),
            },
        ));
        self.w = Some(w);
        self.q = IngestQueue::new(self.cfg.ingest_queue);
        self.maint();
    }

    pub fn maint(&mut self) {
        let Some(w) = self.w.as_mut() else {
            return;
        };
        let (now, wall) = (self.clock.monotonic(), self.clock.wall());
        for _ in 0..2 {
            if let Some(req) = w.segment_request(wall) {
                let r = create_segment(&self.fs, &req, &w.halt());
                w.segment_created(r);
            }
            for job in w.take_seal_jobs(now) {
                let r = seal_segment_described(&self.fs, &job, &w.halt());
                if let Ok(f) = &r {
                    self.sealed_records.push(directory_record(&job, f, true));
                }
                w.seal_done(job, r, now);
            }
        }
    }

    pub fn run(&mut self) {
        let (Some(w), Some(d)) = (self.w.as_mut(), self.dirs.as_mut()) else {
            return;
        };
        for rec in self.sealed_records.drain(..) {
            d.sealed(rec);
        }
        let synced = w.stats().mark_synced;
        for _ in 0..1_000_000 {
            d.run(synced, self.clock.monotonic(), self.clock.wall());
            if d.started() {
                break;
            }
        }
        // No derived file names a position above the mark on stable storage (`spec.md` §3 Durable mark).
        let mark = self.durable_mark();
        let root = read_latest(&self.fs, &dir(), Family::Root, |b| {
            Root::decode(b, &STORE_ID)
        })
        .unwrap();
        assert!(root.is_none_or(|(_, r)| r.through.sequence <= mark));
        assert!(
            self.series()
                .is_none_or(|(_, s)| s.through.sequence <= mark)
        );
    }

    pub fn step(&mut self) {
        let (now, wall) = (self.clock.monotonic(), self.clock.wall());
        if let Some(w) = self.w.as_mut() {
            w.step(now, wall, &mut self.q);
        }
        self.maint();
        self.run();
    }

    pub fn submit(&mut self, items: Vec<Item>) {
        let call = Call::new(items, self.cfg.call_limit()).unwrap();
        self.q.push(call).unwrap();
    }

    /// Commits everything queued, syncs the mark and lets the directories catch up.
    pub fn settle(&mut self) {
        for _ in 0..2_000 {
            self.step();
            self.clock.advance(10 * MS);
            if self.q.is_empty() && self.w.as_ref().is_none_or(|w| !w.seals_running()) {
                break;
            }
        }
        let now = self.clock.monotonic();
        if let Some(w) = self.w.as_mut() {
            w.sync_mark_now(now);
        }
        self.run();
        self.check_contiguous();
    }

    /// `through` never passes a sealed segment that is not listed (A.18 Contiguity).
    pub fn check_contiguous(&self) {
        if let Some(d) = self.load()
            && d.groups.iter().all(Option::is_some)
        {
            let listed: Vec<_> = d.segments().cloned().collect();
            let below: Vec<_> = self
                .sealed()
                .into_iter()
                .filter(|s| s.last() <= d.root.through)
                .collect();
            assert_eq!(
                listed, below,
                "a sealed segment at or below through is not listed"
            );
        }
    }

    pub fn catalogue(&self) -> Catalogue {
        writer_catalogue(&self.fs, &dir(), &STORE_ID, 0).unwrap()
    }

    pub fn load(&self) -> Option<LogDirectory> {
        logdir::load(&self.fs, &dir(), &STORE_ID, &self.catalogue()).unwrap()
    }

    pub fn series(&self) -> Option<(Frame, SeriesDirectory)> {
        read_latest(&self.fs, &dir(), Family::Series, |b| {
            SeriesDirectory::decode(b, &STORE_ID)
        })
        .unwrap()
    }

    /// Every sealed segment's record, from its file, in position order.
    pub fn sealed(&self) -> Vec<SegmentRecord> {
        let mut v: Vec<SegmentRecord> = self
            .fs
            .paths()
            .into_iter()
            .filter_map(|p| {
                let n = SegmentName::parse(p.file_name()?.to_str()?).filter(|n| n.sealed)?;
                let id = SegmentIdentity {
                    store_id: STORE_ID,
                    file_id: n.file_id,
                    generation: n.generation,
                };
                SegmentRecord::of_segment(&self.fs.read(&p)?, &id)
            })
            .collect();
        v.sort_by_key(SegmentRecord::last);
        v
    }

    /// The last sequence of the mark as it is on stable storage.
    pub fn durable_mark(&self) -> u64 {
        let b = self
            .fs
            .read_durable(&dir().join("log").join("MARK"))
            .unwrap();
        read_mark(&b, &STORE_ID)
            .unwrap()
            .map_or(0, |m| m.1.last_sequence)
    }

    pub fn derived(&self) -> Vec<PathBuf> {
        self.fs
            .paths()
            .into_iter()
            .filter(|p| p.starts_with(dir().join(DERIVED_DIR)))
            .collect()
    }
}

/// Facts over five series, `n` calls of one to three facts.
pub fn ingest(r: &mut Rig, n: usize, base: i64) {
    for k in 0..n {
        let items = (0..1 + k % 3)
            .map(|j| {
                let q = if (k + j) % 7 == 0 {
                    Quality::Bad
                } else {
                    Quality::Good
                };
                fact(
                    ((k + j) % 5) as u32 + 1,
                    base + (k * 3 + j) as i64 * 1_000,
                    q,
                )
            })
            .collect();
        r.submit(items);
        r.step();
        r.clock.advance(7 * MS);
    }
}

pub fn group_of(d: &LogDirectory) -> Vec<Group> {
    d.groups.iter().map(|g| g.clone().unwrap()).collect()
}

/// The live files' content, without their frames.
pub fn content(r: &Rig) -> (Root, Vec<Group>, Option<SeriesDirectory>) {
    let d = r.load().unwrap();
    (d.root.clone(), group_of(&d), r.series().map(|s| s.1))
}

/// A property test's configuration with `cases / divisor` cases, run by a runner of our own (`proptest!` would set
/// the cases from `PROPTEST_CASES` again).
pub fn config(divisor: u32) -> ProptestConfig {
    let d = ProptestConfig::default();
    ProptestConfig {
        cases: (d.cases / divisor).max(1),
        ..d
    }
}

/// A store with directories written, then more seals waiting to be listed.
pub fn waiting() -> Rig {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 300, 0);
    r.settle();
    // Seals whose directories the next run writes.
    let d = r.dirs.take();
    ingest(&mut r, 80, 50_000_000);
    r.settle();
    r.dirs = d;
    r
}

/// Writes `bytes` over the file at `p`.
pub fn rewrite(r: &Rig, p: &std::path::Path, bytes: &[u8]) {
    r.fs.remove_file(p).unwrap();
    let f = r.fs.open(p, kvik_log::fs::OpenMode::CreateNew).unwrap();
    f.write_at(bytes, 0).unwrap();
}

/// The path of a sealed segment.
pub fn sealed_path(file_id: u64) -> PathBuf {
    let name = SegmentName {
        generation: 0,
        file_id,
        sealed: true,
    };
    dir().join("log").join(name.to_string())
}

/// A store with more than one group, directories written and the writer stopped.
pub fn two_groups() -> Rig {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 7_000, 0);
    r.settle();
    assert!(r.load().unwrap().groups.len() >= 2);
    r
}
