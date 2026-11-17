//! The derived files (`format.md` A.18) as the writer keeps them: each sealed segment listed once the mark is synced
//! past it, a rebuild equal to the live files, the cadence, the status file and seals handed over out of order.

mod dirs_rig;

use dirs_rig::*;

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
    r.series_bytes = SERIES_BYTES;
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

/// A hook whose entries are as of the position it is told, up to the limit.
#[derive(Debug, Default)]
struct Behind(Mutex<Position>, std::sync::atomic::AtomicU64);

impl SealHook for Behind {
    fn series(&self, limit: Position, written_at: i64) -> Option<SeriesEntries> {
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(SeriesEntries {
            through: (*self.0.lock().unwrap()).min(limit),
            judged_at: written_at,
            entries: Vec::new(),
        })
    }
}

/// The uncovered sealed bytes of `seals`, as a reader counts them: those above the series directory's `through`.
fn uncovered(seals: &[SegmentRecord], d: &Directories, at_or_below: Position) -> u64 {
    let through = d.series_through().unwrap_or_default();
    seals
        .iter()
        .filter(|s| s.last() > through && s.last() <= at_or_below)
        .map(|s| s.len)
        .sum()
}

fn seal(prev: Position, len: u64) -> SegmentRecord {
    SegmentRecord {
        file_id: prev.sequence + 1,
        generation: 0,
        has_entries: false,
        first_sequence: prev.sequence + 1,
        last_sequence: prev.sequence + 10,
        first_commit_time: T0,
        last_commit_time: T0,
        fact_count: 10,
        len,
        file_crc32c: 0,
        sha256: [0; 32],
    }
}

/// Seals of any size up to `segment_size` at any rate, handed to `Directories` itself, the mark synced past each at
/// once or a seal later: after every run the uncovered bytes of the segments it may cover are at most `SERIES_BYTES`,
/// and all uncovered bytes at most that plus the segments still waiting for the mark sync (Q-773).
#[test]
fn uncovered_bytes_stay_bounded_through_the_directories_at_any_rate() {
    const MIB: u64 = 1 << 20;
    let mut runner = proptest::test_runner::TestRunner::new(config(4));
    let seals = prop::collection::vec((1u64..=1024, 0u64..=1_200, any::<bool>()), 1..120);
    runner
        .run(&(16u64..=64, seals), |(segment_mib, seals)| {
            let segment_size = segment_mib * MIB;
            let fs = MemFs::new();
            let hook = Arc::new(Behind::default());
            let start = DirectoryStart {
                generation: 0,
                stamp: None,
                reset_after: None,
                segment_size,
                disk_critical: 0,
                series_bytes: SERIES_BYTES,
            };
            let opts = DirectoryOptions {
                hook: Some(hook.clone()),
                status_interval: None,
            };
            let mut d = Directories::new(Arc::new(fs.clone()), &dir(), STORE_ID, start, opts);
            let mut all: Vec<SegmentRecord> = Vec::new();
            let mut now = Duration::ZERO;
            let mut synced = Position::default();
            for (len_kib, gap_s, late) in seals {
                let prev = all.last().map_or(Position::default(), SegmentRecord::last);
                let r = seal(prev, (len_kib * segment_size / 1024).max(4096));
                all.push(r.clone());
                d.sealed(r.clone());
                if !late {
                    synced = r.last();
                }
                *hook.0.lock().unwrap() = synced;
                now += Duration::from_secs(gap_s);
                for _ in 0..100 {
                    d.run(synced, now, T0);
                    if d.started() {
                        break;
                    }
                }
                d.run(synced, now, T0);
                let waiting: u64 = all
                    .iter()
                    .filter(|s| s.last() > synced)
                    .map(|s| s.len)
                    .sum();
                prop_assert!(uncovered(&all, &d, synced) <= SERIES_BYTES);
                prop_assert!(uncovered(&all, &d, r.last()) <= SERIES_BYTES + waiting);
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_hook_behind_the_seals_is_asked_again_after_a_growing_wait_and_then_written() {
    let fs = MemFs::new();
    let hook = Arc::new(Behind::default());
    let start = DirectoryStart {
        generation: 0,
        stamp: None,
        reset_after: None,
        segment_size: 64 << 20,
        disk_critical: 0,
        series_bytes: SERIES_BYTES,
    };
    let opts = DirectoryOptions {
        hook: Some(hook.clone()),
        status_interval: None,
    };
    let mut d = Directories::new(Arc::new(fs.clone()), &dir(), STORE_ID, start, opts);
    let r = seal(Position::default(), 40 << 20);
    d.sealed(r.clone());
    // The hook stays at position 0 for 20 s of runs every 100 ms: each try fails, and the waits grow.
    let mut now = Duration::ZERO;
    for _ in 0..200 {
        d.run(r.last(), now, T0);
        now += Duration::from_millis(100);
    }
    assert_eq!(d.series_through(), None);
    let asked = hook.1.load(std::sync::atomic::Ordering::SeqCst);
    assert!((3..=10).contains(&asked), "asked {asked} times");
    *hook.0.lock().unwrap() = r.last();
    now += Duration::from_secs(120);
    d.run(r.last(), now, T0);
    assert_eq!(d.series_through(), Some(r.last()));
}

#[test]
fn entries_a_hook_gives_above_the_synced_mark_are_not_written() {
    let mut r = Rig::new(cfg());
    *r.hook.overshoot.lock().unwrap() = 1;
    ingest(&mut r, 300, 0);
    r.settle();
    assert_eq!(r.series(), None);
    *r.hook.overshoot.lock().unwrap() = 0;
    ingest(&mut r, 100, 10_000_000);
    r.settle();
    assert!(r.series().is_some());
}

#[test]
fn seals_listed_together_across_a_group_boundary_are_all_written() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 6_000, 0);
    r.settle();
    let before = r.sealed().len();
    assert!(before < logdir::GROUP_SEGMENTS, "{before}");
    let d = r.dirs.take();
    ingest(&mut r, 800, 90_000_000);
    r.settle();
    assert!(r.sealed().len() > logdir::GROUP_SEGMENTS);
    r.dirs = d;
    r.settle();
    let d = r.load().unwrap();
    assert!(d.groups.iter().all(Option::is_some));
    assert_eq!(d.segments().cloned().collect::<Vec<_>>(), r.sealed());
}

#[test]
fn a_seal_handed_over_after_the_files_were_read_is_listed_once() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 300, 0);
    r.settle();
    // Seals the new directories hear of although they find them on disk when they first run.
    r.dirs = None;
    ingest(&mut r, 200, 10_000_000);
    r.settle();
    assert!(!r.sealed_records.is_empty());
    r.open();
    r.settle();
    let d = r.load().unwrap();
    assert!(d.groups.iter().all(Option::is_some));
    assert_eq!(d.segments().cloned().collect::<Vec<_>>(), r.sealed());
}
