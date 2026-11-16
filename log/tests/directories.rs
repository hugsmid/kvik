//! The derived files (`format.md` A.18) on a writer driven step by step on the in-memory file system: the log
//! directory lists each sealed segment once the mark is synced past it and never above; a rebuild equals the live
//! files; a crash at every write recovers from the previous generation; a replaced segment, a restore and a manifest
//! rebuild invalidate what they touch; a lookup located through the directories equals a full scan; the derived
//! files are a source of F.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::{BatchContent, Item};
use kvik_log::directory::cadence::{READER_ALLOWANCE, SERIES_BYTES, SERIES_INTERVAL};
use kvik_log::directory::logdir::{
    self, Catalogue, Group, Listed, LogDirectory, Root, SegmentRecord,
};
use kvik_log::directory::series::{Candidate, CandidateValue, SeriesDirectory, SeriesEntry};
use kvik_log::directory::status::{StatusFile, counter, health};
use kvik_log::directory::{self, DERIVED_DIR, Family, Frame, read_latest};
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{Fs, MemFs};
use kvik_log::manifest::{RECORD_EMPTY, encode_initial, encode_record, read_manifest};
use kvik_log::mark::read_mark;
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource as _, Clock, Directories, DirectoryOptions, IngestQueue, LogWriter, MarkSync,
    Position, SealHook, StatusContent, WriterConfig, create_segment, seal_described,
    writer_catalogue,
};
use proptest::prelude::*;

const STORE_ID: [u8; 16] = [9; 16];
const T0: i64 = 1_767_225_600_000_000_000;
const MS: Duration = Duration::from_millis(1);

fn dir() -> PathBuf {
    PathBuf::from("store")
}

fn cfg() -> WriterConfig {
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
struct ManualClock(Mutex<Duration>);

impl ManualClock {
    fn advance(&self, d: Duration) {
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
type Batches = Vec<(u64, u64, BatchContent)>;

/// Every complete batch of every segment, with its first and last position, in position order.
fn batches(fs: &MemFs) -> Batches {
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

fn facts_of(c: &BatchContent) -> impl Iterator<Item = (u64, &FactRow)> {
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
fn fold(batches: &Batches, limit: u64, written_at: i64) -> (Position, Vec<SeriesEntry>) {
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
struct Hook {
    fs: MemFs,
    health: Mutex<u8>,
}

impl SealHook for Hook {
    fn series(&self, limit: Position, written_at: i64) -> Option<(Position, Vec<SeriesEntry>)> {
        Some(fold(&batches(&self.fs), limit.sequence, written_at))
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

fn fact(series: u32, t: i64, quality: Quality) -> Item {
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
struct Rig {
    fs: MemFs,
    clock: Arc<ManualClock>,
    cfg: WriterConfig,
    w: Option<LogWriter>,
    q: IngestQueue,
    dirs: Option<Directories>,
    hook: Arc<Hook>,
    /// The segment size the cadence assumes, so that the small test segments exercise its byte rule.
    cadence_segment: u64,
    /// Sealed segments not yet handed to the directories (none while a test holds them back).
    sealed_records: Vec<SegmentRecord>,
}

impl Rig {
    fn new(cfg: WriterConfig) -> Self {
        let fs = MemFs::new();
        let mut r = Rig {
            hook: Arc::new(Hook {
                fs: fs.clone(),
                health: Mutex::new(health::OK),
            }),
            fs,
            clock: Arc::new(ManualClock::default()),
            q: IngestQueue::new(cfg.ingest_queue),
            cfg,
            w: None,
            dirs: None,
            cadence_segment: SERIES_BYTES,
            sealed_records: Vec::new(),
        };
        r.open();
        r
    }

    fn open(&mut self) {
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
        start.segment_size = self.cadence_segment;
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

    fn maint(&mut self) {
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
                let r = seal_described(&self.fs, &job, &w.halt());
                if let Ok(rec) = &r {
                    self.sealed_records.push(rec.clone());
                }
                w.seal_done(job, r.map(|_| ()), now);
            }
        }
    }

    fn run(&mut self) {
        let (Some(w), Some(d)) = (self.w.as_mut(), self.dirs.as_mut()) else {
            return;
        };
        for rec in self.sealed_records.drain(..) {
            d.sealed(rec);
        }
        let synced = w.stats().mark_synced;
        d.run(synced, self.clock.monotonic(), self.clock.wall());
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

    fn step(&mut self) {
        let (now, wall) = (self.clock.monotonic(), self.clock.wall());
        if let Some(w) = self.w.as_mut() {
            w.step(now, wall, &mut self.q);
        }
        self.maint();
        self.run();
    }

    fn submit(&mut self, items: Vec<Item>) {
        let call = Call::new(items, self.cfg.call_limit()).unwrap();
        self.q.push(call).unwrap();
    }

    /// Commits everything queued, syncs the mark and lets the directories catch up.
    fn settle(&mut self) {
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
    }

    fn catalogue(&self) -> Catalogue {
        writer_catalogue(&self.fs, &dir(), &STORE_ID, 0).unwrap()
    }

    fn load(&self) -> Option<LogDirectory> {
        logdir::load(&self.fs, &dir(), &STORE_ID, &self.catalogue()).unwrap()
    }

    fn series(&self) -> Option<(Frame, SeriesDirectory)> {
        read_latest(&self.fs, &dir(), Family::Series, |b| {
            SeriesDirectory::decode(b, &STORE_ID)
        })
        .unwrap()
    }

    /// Every sealed segment's record, from its file, in position order.
    fn sealed(&self) -> Vec<SegmentRecord> {
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
    fn durable_mark(&self) -> u64 {
        let b = self
            .fs
            .read_durable(&dir().join("log").join("MARK"))
            .unwrap();
        read_mark(&b, &STORE_ID)
            .unwrap()
            .map_or(0, |m| m.1.last_sequence)
    }

    fn derived(&self) -> Vec<PathBuf> {
        self.fs
            .paths()
            .into_iter()
            .filter(|p| p.starts_with(dir().join(DERIVED_DIR)))
            .collect()
    }
}

/// Facts over five series, `n` calls of one to three facts.
fn ingest(r: &mut Rig, n: usize, base: i64) {
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

fn group_of(d: &LogDirectory) -> Vec<Group> {
    d.groups.iter().map(|g| g.clone().unwrap()).collect()
}

#[test]
fn the_log_directory_lists_each_sealed_segment_once_the_mark_is_synced_past_it() {
    let mut r = Rig::new(cfg());
    let mut checked = 0;
    for round in 0..60 {
        ingest(&mut r, 120, round * 1_000_000);
        if let Some(d) = r.load() {
            assert!(d.groups.iter().all(Option::is_some));
            let mark = r.durable_mark();
            for s in d.segments() {
                assert!(
                    s.last_sequence <= mark,
                    "a listed segment above the synced mark"
                );
            }
            if let Some((_, s)) = r.series() {
                assert!(
                    s.through.sequence <= mark,
                    "the series directory above the synced mark"
                );
            }
            checked += 1;
        }
    }
    r.settle();
    let d = r.load().unwrap();
    let sealed = r.sealed();
    assert!(
        sealed.len() > logdir::GROUP_SEGMENTS,
        "more than one group: {}",
        sealed.len()
    );
    assert_eq!(d.segments().cloned().collect::<Vec<_>>(), sealed);
    assert_eq!(d.root.through, sealed.last().unwrap().last());
    assert_eq!(
        d.groups.len(),
        sealed.len().div_ceil(logdir::GROUP_SEGMENTS)
    );
    assert!(checked > 30);
    // Only the newest two generations of each family are kept.
    for (family, _, _) in directory::list(&r.fs, &dir()).unwrap() {
        assert!(directory::generations(&r.fs, &dir(), family).len() <= 2);
    }
}

#[test]
fn a_reopened_writer_reuses_every_group_and_reads_no_segment_again() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 400, 0);
    r.settle();
    let live = r.load().unwrap();
    r.open();
    r.settle();
    assert_eq!(r.dirs.as_ref().unwrap().stats().described, 0);
    assert_eq!(r.load().unwrap().root.groups, live.root.groups);
}

#[test]
fn a_status_file_is_written_on_its_interval_and_at_once_on_a_change_of_health() {
    let mut r = Rig::new(cfg());
    r.settle();
    let status = |r: &Rig| {
        read_latest(&r.fs, &dir(), Family::Status, |b| {
            StatusFile::decode(b, &STORE_ID)
        })
        .unwrap()
        .unwrap()
    };
    let (first, s) = status(&r);
    assert_eq!(s.health, health::OK);
    assert!(s.mark_synced.sequence <= r.durable_mark());
    r.clock.advance(Duration::from_secs(2));
    r.run();
    assert_eq!(
        status(&r).0.file_id,
        first.file_id,
        "not before its interval"
    );
    *r.hook.health.lock().unwrap() = health::DEGRADED;
    r.clock.advance(Duration::from_secs(2));
    r.run();
    let (second, s) = status(&r);
    assert_eq!(
        (second.file_id, s.health),
        (first.file_id + 1, health::DEGRADED)
    );
    r.clock.advance(Duration::from_secs(61));
    r.run();
    assert_eq!(status(&r).0.file_id, second.file_id + 1, "on its interval");
}

#[test]
fn the_series_directory_follows_the_time_rule_when_segments_are_small() {
    let mut r = Rig::new(cfg());
    r.cadence_segment = 4 << 10;
    r.open();
    ingest(&mut r, 200, 0);
    r.settle();
    let (first, _) = r.series().unwrap();
    ingest(&mut r, 200, 10_000_000);
    r.settle();
    assert_eq!(
        r.series().unwrap().0.file_id,
        first.file_id,
        "small seals within the interval"
    );
    r.clock.advance(SERIES_INTERVAL);
    ingest(&mut r, 200, 20_000_000);
    r.settle();
    assert!(r.series().unwrap().0.file_id > first.file_id);
}

/// The live files' content, without their frames.
fn content(r: &Rig) -> (Root, Vec<Group>, Option<SeriesDirectory>) {
    let d = r.load().unwrap();
    (d.root.clone(), group_of(&d), r.series().map(|s| s.1))
}

/// A property test's configuration with `cases / divisor` cases, run by a runner of our own (`proptest!` would set
/// the cases from `PROPTEST_CASES` again).
fn config(divisor: u32) -> ProptestConfig {
    let d = ProptestConfig::default();
    ProptestConfig {
        cases: (d.cases / divisor).max(1),
        ..d
    }
}

/// Removing every derived file and starting again gives the same root, groups and series directory as the files
/// written seal by seal.
#[test]
fn a_rebuilt_directory_equals_the_live_one() {
    let mut runner = proptest::test_runner::TestRunner::new(config(16));
    runner
        .run(&(20usize..240, 0usize..3), |(calls, restarts)| {
            let mut r = Rig::new(cfg());
            for k in 0..=restarts {
                ingest(&mut r, calls / (restarts + 1), k as i64 * 10_000_000);
                if k < restarts {
                    r.settle();
                    r.open();
                }
            }
            r.settle();
            let live = content(&r);
            let wall = r.series().map(|s| s.0.written_at);
            for p in r.derived() {
                r.fs.remove_file(&p).unwrap();
            }
            r.open();
            r.settle();
            prop_assert_eq!(
                r.dirs.as_ref().unwrap().stats().described as usize,
                r.sealed().len()
            );
            let rebuilt = content(&r);
            prop_assert_eq!(&rebuilt.0, &live.0);
            prop_assert_eq!(&rebuilt.1, &live.1);
            // The candidates depend on the write time: the live entries are those of a fold at theirs.
            let live_entries = live.2.map(|s| (s.through, s.entries));
            let folded = wall.map(|w| {
                fold(
                    &batches(&r.fs),
                    live_entries.as_ref().unwrap().0.sequence,
                    w,
                )
            });
            prop_assert_eq!(live_entries, folded);
            // With no sealed segment there is nothing for a series directory to cover.
            prop_assert_eq!(r.series().is_some(), !r.sealed().is_empty());
            if let Some((f, s)) = r.series() {
                prop_assert_eq!(
                    (s.through, s.entries),
                    fold(&batches(&r.fs), s.through.sequence, f.written_at)
                );
            }
            Ok(())
        })
        .unwrap();
}

/// Seals of any size up to `segment_size` at any rate, through the cadence: after each seal's due write the
/// uncovered bytes are at most `SERIES_BYTES − segment_size` (under half the allowance), and before it at most
/// `max(SERIES_BYTES, segment_size)`, inside the allowance for every `segment_size` up to it.
#[test]
fn uncovered_bytes_stay_inside_the_allowance_at_any_rate() {
    use kvik_log::directory::cadence::{Uncovered, series_due};
    const MIB: u64 = 1 << 20;
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig::default());
    let seals = prop::collection::vec((1u64..=1024, 0u64..=1_200), 1..400);
    runner
        .run(&(16u64..=64, seals), |(segment_mib, seals)| {
            let segment_size = segment_mib * MIB;
            let mut u = Uncovered::default();
            let mut since: Option<Duration> = None;
            for (i, (len_kib, gap_s)) in seals.into_iter().enumerate() {
                let len = (len_kib * segment_size / 1024).max(4096);
                let last = Position {
                    generation: 0,
                    sequence: i as u64 + 1,
                };
                since = since.map(|d| d + Duration::from_secs(gap_s));
                u.sealed(last, len);
                prop_assert!(u.bytes() <= SERIES_BYTES.max(segment_size));
                prop_assert!(u.bytes() <= READER_ALLOWANCE);
                if series_due(u.bytes(), segment_size, since) {
                    u.covered(last);
                    since = Some(Duration::ZERO);
                }
                prop_assert!(u.bytes() <= SERIES_BYTES.saturating_sub(segment_size));
            }
            Ok(())
        })
        .unwrap();
}

/// A store with directories written, then more seals waiting to be listed.
fn waiting() -> Rig {
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

#[test]
fn a_crash_at_any_write_recovers_from_the_previous_generation() {
    let reference = {
        let mut r = waiting();
        r.fs.clear_ops();
        r.run();
        let ops = r.fs.ops().len();
        assert!(
            ops >= 8,
            "a group, the root, the series directory and status: {ops}"
        );
        (ops, content(&r))
    };
    for n in 0..=reference.0 {
        for power in [false, true] {
            let mut r = waiting();
            let before = (r.load().map(|d| d.root), r.series().map(|s| s.1));
            r.fs.crash_after(n);
            r.run();
            if power {
                r.fs.power_loss_partial(n as u64);
            } else {
                r.fs.restart();
            }
            // Whatever a reader finds is true of the segments: after a process crash the old generation or the
            // new one, after a power cut possibly an older one or nothing; never a record that is not the file's.
            let found = (r.load(), r.series().map(|s| s.1));
            let sealed = r.sealed();
            if let Some(d) = &found.0 {
                let listed: Vec<_> = d.segments().cloned().collect();
                assert!(listed.iter().all(|x| sealed.contains(x)), "n {n}");
                if d.groups.iter().all(Option::is_some) {
                    assert!(sealed.starts_with(&listed), "n {n}");
                }
                if !power {
                    assert!(
                        Some(&d.root) == before.0.as_ref() || d.root == reference.1.0,
                        "n {n}"
                    );
                }
            } else {
                assert!(
                    power,
                    "a root of either generation after a process crash, n {n}"
                );
            }
            // A new start rebuilds forward from what it finds and ends where the uninterrupted run did.
            r.open();
            r.settle();
            let after = content(&r);
            assert_eq!(
                after.0.groups, reference.1.0.groups,
                "n {n}, power loss {power}"
            );
            assert_eq!(after.1, reference.1.1, "n {n}, power loss {power}");
            if !power {
                let reused = r.dirs.as_ref().unwrap().stats().described;
                assert!(
                    reused <= 80,
                    "only the segments no valid group lists are read: {reused}"
                );
            }
        }
    }
}

#[test]
fn a_replaced_segment_invalidates_its_group_alone() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 7_000, 0);
    r.settle();
    let live = r.load().unwrap();
    assert!(live.groups.len() >= 2);
    let victim = live.groups[0].as_ref().unwrap().segments[3].clone();
    let mut cat = r.catalogue();
    let l = cat.segments.remove(&victim.file_id).unwrap();
    cat.segments.insert(
        10_000,
        Listed {
            seal: Some((l.seal.unwrap().0, 1)),
            ..l
        },
    );
    let d = logdir::load(&r.fs, &dir(), &STORE_ID, &cat)
        .unwrap()
        .unwrap();
    assert!(
        d.groups[0].is_none(),
        "the group listing the replaced segment"
    );
    assert!(
        d.groups[1..].iter().all(Option::is_some),
        "every other group"
    );
    // A segment whose hash differs, as the manifest describes it, is replaced too.
    let mut cat = r.catalogue();
    cat.segments.get_mut(&victim.file_id).unwrap().sha256 = Some([0; 32]);
    let d = logdir::load(&r.fs, &dir(), &STORE_ID, &cat)
        .unwrap()
        .unwrap();
    assert!(d.groups[0].is_none() && d.groups[1].is_some());
    cat.segments.get_mut(&victim.file_id).unwrap().sha256 = Some(victim.sha256);
    assert!(
        logdir::load(&r.fs, &dir(), &STORE_ID, &cat)
            .unwrap()
            .unwrap()
            .groups[0]
            .is_some()
    );
}

#[test]
fn a_restore_invalidates_every_file_of_the_older_generation() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 100, 0);
    r.settle();
    let mut cat = r.catalogue();
    cat.generation = 1;
    assert_eq!(logdir::load(&r.fs, &dir(), &STORE_ID, &cat).unwrap(), None);
    let (frame, s) = r.series().unwrap();
    let mut start = r.w.as_ref().unwrap().directory_start();
    assert!(start.keeps(frame.generation, s.stamp, s.through));
    start.generation = 1;
    assert!(!start.keeps(frame.generation, s.stamp, s.through));
}

#[test]
fn a_reset_below_the_series_directory_invalidates_it_and_one_above_keeps_it() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 100, 0);
    r.settle();
    let (frame, s) = r.series().unwrap();
    let mut start = r.w.as_ref().unwrap().directory_start();
    start.stamp = Some(kvik_log::mark::ResetStamp {
        sequence: s.through.sequence + 5,
        nonce: 3,
    });
    start.reset_after = Some(s.through);
    assert!(start.keeps(frame.generation, s.stamp, s.through));
    start.reset_after = Some(Position {
        generation: 0,
        sequence: s.through.sequence - 1,
    });
    assert!(!start.keeps(frame.generation, s.stamp, s.through));
}

#[test]
fn a_manifest_rebuild_invalidates_the_root_alone() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 400, 0);
    r.settle();
    let live = r.load().unwrap();
    // A rebuilt manifest renumbers its records: the record the root names is at the same offset with another epoch.
    let path = dir().join("MANIFEST");
    let old = read_manifest(&r.fs.read(&path).unwrap(), &STORE_ID).unwrap();
    let mut bytes = encode_initial(STORE_ID, T0);
    bytes.truncate(old.records[0].offset as usize);
    bytes.extend_from_slice(&encode_record(RECORD_EMPTY, 7, 0, T0, None));
    r.fs.remove_file(&path).unwrap();
    let f = r.fs.open(&path, kvik_log::fs::OpenMode::CreateNew).unwrap();
    f.write_at(&bytes, 0).unwrap();
    drop(f);
    assert_eq!(r.load(), None, "the root no longer names a record");
    r.open();
    r.settle();
    assert_eq!(
        r.dirs.as_ref().unwrap().stats().described,
        0,
        "every group reused"
    );
    let d = r.load().unwrap();
    assert_eq!(d.root.groups, live.root.groups);
    assert_eq!(d.root.epoch, 7);
}

#[test]
fn a_lookup_located_through_the_directories_equals_a_full_scan() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 500, 0);
    r.settle();
    // Seals not yet listed and an open segment: the reader covers both from the log.
    let d = r.dirs.take();
    ingest(&mut r, 40, 90_000_000);
    r.dirs = d;
    let mark = read_mark(
        &r.fs.read(&dir().join("log").join("MARK")).unwrap(),
        &STORE_ID,
    )
    .unwrap()
    .unwrap()
    .1;
    let all = batches(&r.fs);
    let full = |s: u32| -> Vec<(u64, i64)> {
        all.iter()
            .filter(|b| b.1 <= mark.last_sequence)
            .flat_map(|b| {
                facts_of(&b.2)
                    .filter(|f| f.1.series_id == s)
                    .map(|(p, f)| (p, f.t))
            })
            .collect()
    };
    let dirs = r.load().unwrap();
    let series = r.series().unwrap().1;
    let listed: BTreeMap<u64, &SegmentRecord> = dirs.segments().map(|s| (s.file_id, s)).collect();
    let cat = r.catalogue();
    let uncovered: Vec<u64> = cat
        .segments
        .keys()
        .filter(|id| !listed.contains_key(id))
        .copied()
        .collect();
    let uncovered_bytes: u64 = uncovered
        .iter()
        .map(|id| cat.segments[id].seal.unwrap().0)
        .sum();
    assert!(!uncovered.is_empty() && uncovered_bytes <= READER_ALLOWANCE);
    let read = |file_id: u64, sealed: bool| -> (Option<Vec<u32>>, Batches) {
        let name = SegmentName {
            generation: 0,
            file_id,
            sealed,
        };
        let bytes =
            r.fs.read(&dir().join("log").join(name.to_string()))
                .unwrap();
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id,
            generation: 0,
        };
        let limit = if sealed {
            ScanLimit::End
        } else {
            ScanLimit::Mark(mark.offset)
        };
        let rep = scan(&bytes, &id, limit).unwrap();
        let index = rep
            .seal
            .and_then(|s| s.index.flatten())
            .map(|i| i.records.iter().map(|x| x.series_id).collect());
        let b = rep
            .batches
            .iter()
            .map(|b| {
                (
                    b.commit.first_sequence,
                    b.commit.last_sequence,
                    BatchContent::decode(b).unwrap(),
                )
            })
            .collect();
        (index, b)
    };
    for s in 1..=6 {
        let mut found = Vec::new();
        let mut take = |b: Batches| {
            for (_, _, c) in &b {
                found.extend(
                    facts_of(c)
                        .filter(|f| f.1.series_id == s)
                        .map(|(p, f)| (p, f.t)),
                );
            }
        };
        for rec in listed.values() {
            let (index, b) = read(rec.file_id, true);
            // A segment whose index does not list the series is passed over (A.8).
            if index.is_some_and(|i| i.contains(&s)) {
                take(b);
            }
        }
        for id in &uncovered {
            take(read(*id, true).1);
        }
        take(read(mark.file_id, false).1);
        found.sort();
        assert_eq!(found, full(s), "series {s}");
        // The series directory's count and latest `t`, with the facts above its `through`, are the full scan's.
        let e = series.entries.iter().find(|e| e.series_id == s);
        let above: Vec<_> = full(s)
            .into_iter()
            .filter(|(p, _)| *p > series.through.sequence)
            .collect();
        let count = e.map_or(0, |e| e.fact_count) + above.len() as u64;
        assert_eq!(count, full(s).len() as u64, "series {s}");
        let latest = e
            .and_then(|e| e.latest_t)
            .into_iter()
            .chain(above.iter().map(|f| f.1))
            .max();
        assert_eq!(latest, full(s).iter().map(|f| f.1).max(), "series {s}");
    }
}

#[test]
fn a_position_a_derived_file_names_raises_f() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 20, 0);
    r.settle();
    let mark = r.durable_mark();
    let d = SeriesDirectory {
        through: Position {
            generation: 0,
            sequence: mark + 40,
        },
        stamp: None,
        entries: Vec::new(),
    };
    let frame = Frame {
        generation: 0,
        file_id: 99,
        written_at: T0,
    };
    directory::write_file(
        &r.fs,
        &dir(),
        Family::Series,
        99,
        &d.encode(frame, STORE_ID).unwrap(),
    )
    .unwrap();
    r.fs.remove_file(&dir().join("log").join("MARK")).unwrap();
    r.dirs = None;
    r.w = None;
    r.open();
    let e = r.w.as_ref().unwrap().start_report().entry.clone().unwrap();
    assert_eq!(e.sequence, mark + 41, "F above the derived file's position");
}

#[test]
fn a_damaged_newest_generation_falls_back_to_the_previous_one() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 400, 0);
    r.settle();
    ingest(&mut r, 400, 20_000_000);
    r.settle();
    for family in [Family::Root, Family::Series] {
        let gens = directory::generations(&r.fs, &dir(), family);
        assert_eq!(gens.len(), 2, "{family:?}");
        // One bit of the newest's body flipped: invalid, so a reader takes the previous generation.
        let mut newest = r.fs.read(&gens[0].1).unwrap();
        newest[90] ^= 1;
        r.fs.remove_file(&gens[0].1).unwrap();
        let f =
            r.fs.open(&gens[0].1, kvik_log::fs::OpenMode::CreateNew)
                .unwrap();
        f.write_at(&newest, 0).unwrap();
        drop(f);
        let found = match family {
            Family::Root => read_latest(&r.fs, &dir(), family, |b| Root::decode(b, &STORE_ID))
                .unwrap()
                .map(|(f, _)| f.file_id),
            _ => r.series().map(|(f, _)| f.file_id),
        };
        assert_eq!(found, Some(gens[1].0), "{family:?}");
    }
    // The writer rebuilds forward from what it finds and ends where it was.
    r.open();
    r.settle();
    let d = r.load().unwrap();
    assert!(d.groups.iter().all(Option::is_some));
    assert_eq!(d.segments().cloned().collect::<Vec<_>>(), r.sealed());
}

#[test]
fn a_group_of_another_generation_than_the_root_names_is_not_used() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 300, 0);
    r.settle();
    let d = r.load().unwrap();
    let last = d.root.groups.last().unwrap().number;
    let gens = directory::generations(&r.fs, &dir(), Family::Group(last));
    assert_eq!(gens.len(), 2, "the open group was written at least twice");
    r.fs.remove_file(&gens[0].1).unwrap();
    // The previous generation still matches the catalogue, but it lists fewer segments than the root says.
    let d = r.load().unwrap();
    assert!(d.groups.last().unwrap().is_none());
}
