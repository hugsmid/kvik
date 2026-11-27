//! The derived files of a durable store (`format.md` A.18): after seals the series directory agrees with the store's
//! own reads as of its position, the log directory lists every sealed segment, and the status file holds the
//! writer's counters; a reopened store continues the series directory from the one it finds.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::block_on;
use common::world::{World, gauge, num};
use kvik::{Live, Quality, Reading, SeriesId, Store, Timestamp, View};
use kvik_log::directory::logdir;
use kvik_log::directory::series::SeriesDirectory;
use kvik_log::directory::status::{StatusFile, counter, health};
use kvik_log::directory::{Family, read_latest};
use kvik_log::fs::Fs as _;
use kvik_log::writer::writer_catalogue;

fn open(w: &World) -> Store {
    let mut c = w.config();
    c.log.seal_interval = Duration::from_millis(300);
    block_on(Store::open(c)).unwrap()
}

fn store_id(w: &World) -> [u8; 16] {
    let b = w.fs.read(&Path::new(&w.dir).join("STORE")).unwrap();
    kvik_log::header::FileHeader::decode(&b).unwrap().store_id
}

fn series_dir(w: &World) -> Option<SeriesDirectory> {
    let id = store_id(w);
    read_latest(&*w.fs, Path::new(&w.dir), Family::Series, |b| {
        SeriesDirectory::decode(b, &id)
    })
    .unwrap()
    .map(|(_, d)| d)
}

/// Waits until the series directory covers `seq`: seals run on the log's own monotonic clock (`LogClock`), which
/// the test's manual clock does not drive, so this polls with a generous deadline and asserts nothing about timing.
fn wait_for(w: &World, seq: u64) -> SeriesDirectory {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(d) = series_dir(w).filter(|d| d.through.sequence >= seq) {
            return d;
        }
        assert!(
            Instant::now() < deadline,
            "no series directory through {seq}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until at least `n` segments are sealed.
fn wait_sealed(w: &World, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let sealed = || {
        w.fs.paths()
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "kvl"))
            .count()
    };
    while sealed() < n {
        assert!(Instant::now() < deadline, "fewer than {n} sealed segments");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn emit(s: &Store, readings: &[Reading]) -> u64 {
    block_on(s.emit(readings))
        .unwrap()
        .last()
        .map_or(0, |p| p.sequence)
}

/// The store's own reads agree with an entry: every fact up to `through`, the latest `t` and the live fact.
fn check(s: &Store, d: &SeriesDirectory, ids: &[SeriesId]) {
    let all = Timestamp(0)..Timestamp(i64::MAX);
    let r = s.reader();
    for id in ids {
        let e = d.entries.iter().find(|e| e.series_id == id.0).unwrap();
        let facts: Vec<_> = r
            .facts(*id, all.clone(), View::History)
            .unwrap()
            .map(Result::unwrap)
            .filter(|f| f.position.sequence <= d.through.sequence)
            .collect();
        assert_eq!(e.fact_count, facts.len() as u64, "series {id:?}");
        assert_eq!(e.orphan_count, 0);
        assert_eq!(
            e.latest_t,
            facts.iter().map(|f| f.t.0).max(),
            "series {id:?}"
        );
        if !e.candidate_unsure
            && let Live::Value(v) = r.live(*id).unwrap()
            && v.fact.position.sequence <= d.through.sequence
        {
            let c = e.candidate.unwrap();
            assert_eq!(
                (c.t, c.position.sequence),
                (v.fact.t.0, v.fact.position.sequence)
            );
        }
    }
}

#[test]
fn the_series_directory_agrees_with_the_store_and_continues_after_a_reopen() {
    let w = World::new("derived-agree");
    let s = open(&w);
    let ids: Vec<SeriesId> = (0..3)
        .map(|k| block_on(s.define_series(gauge(&format!("site/dev/m{k}")))).unwrap())
        .collect();
    for round in 0..3 {
        for k in 0..20 {
            let mut readings: Vec<Reading> = ids
                .iter()
                .map(|id| num(*id, round * 100 + k, (round * 100 + k) as f64))
                .collect();
            if k % 6 == 0 {
                readings[1].quality = Quality::Bad;
            }
            emit(&s, &readings);
        }
        // A seal by time; the first is followed by the series directory, later small ones wait for the cadence
        // (A.18): whichever file there is agrees with the store as of its position.
        wait_sealed(&w, round as usize + 1);
        let d = wait_for(&w, 1);
        check(&s, &d, &ids);
    }
    block_on(s.shutdown()).unwrap();
    let id = store_id(&w);
    let dir = Path::new(&w.dir);
    // The log directory lists every sealed segment the catalogue holds.
    let cat = writer_catalogue(&*w.fs, dir, &id, 0).unwrap();
    let ld = logdir::load(&*w.fs, dir, &id, &cat).unwrap().unwrap();
    assert!(ld.groups.iter().all(Option::is_some));
    assert_eq!(ld.segments().count(), cat.segments.len());
    assert!(cat.segments.len() >= 3);
    let (_, st) = read_latest(&*w.fs, dir, Family::Status, |b| StatusFile::decode(b, &id))
        .unwrap()
        .unwrap();
    assert_eq!(st.flags, kvik_log::directory::status::flag::PARTIAL);
    assert!(st.counter(counter::BATCHES).is_some());
    // A reopened store continues from the file it finds and agrees again after more seals.
    let before = series_dir(&w).unwrap();
    let s = open(&w);
    for k in 0..10 {
        emit(&s, &[num(ids[2], 1_000 + k, 1.0)]);
    }
    // The reopened writer has not written one yet, so the next seal is due.
    let d = wait_for(&w, before.through.sequence + 1);
    check(&s, &d, &ids);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn an_idle_store_gets_a_status_file() {
    for k in 0..3 {
        let w = World::new(&format!("derived-idle-{k}"));
        let s = open(&w);
        let id = store_id(&w);
        let deadline = Instant::now() + Duration::from_secs(60);
        let st = loop {
            let st = read_latest(&*w.fs, Path::new(&w.dir), Family::Status, |b| {
                StatusFile::decode(b, &id)
            })
            .unwrap();
            if let Some(st) = st {
                break st.1;
            }
            assert!(Instant::now() < deadline, "no status file on an idle store");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(st.health, health::OK);
        assert_eq!(st.flags, kvik_log::directory::status::flag::PARTIAL);
        block_on(s.shutdown()).unwrap();
    }
}

#[test]
fn a_store_read_only_by_a_fault_of_its_own_writes_no_status_file() {
    let w = World::new("derived-own-fault");
    let s = open(&w);
    block_on(s.define_series(gauge("site/dev/m0"))).unwrap();
    block_on(s.shutdown()).unwrap();
    let id = store_id(&w);
    let dir = Path::new(&w.dir);
    for (_, _, p) in kvik_log::directory::list(&*w.fs, dir).unwrap() {
        w.fs.remove_file(&p).unwrap();
    }
    // The registry file must be written again at the start, and that write fails: the store opens read-only while
    // its log runs.
    let reg = dir.join(kvik_log::registry::REGISTRY_DIR);
    for e in w.fs.read_dir(&reg).unwrap() {
        w.fs.remove_file(&reg.join(&e.name)).unwrap();
    }
    w.fs.fail_nth(
        kvik_log::fs::FaultOp::Create,
        kvik_log::registry::REGISTRY_DIR,
        0,
        std::io::ErrorKind::Other,
    );
    let s = open(&w);
    assert!(block_on(s.define_series(gauge("site/dev/m1"))).is_err());
    // The start's work ends with the root; a store that may report writes its status file in that same run.
    let deadline = Instant::now() + Duration::from_secs(60);
    while read_latest(&*w.fs, dir, Family::Root, |b| logdir::Root::decode(b, &id))
        .unwrap()
        .is_none()
    {
        assert!(Instant::now() < deadline, "no root");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_secs(2));
    let st = read_latest(&*w.fs, dir, Family::Status, |b| StatusFile::decode(b, &id)).unwrap();
    assert!(st.is_none(), "a read-only store reported {st:?}");
    let _ = block_on(s.shutdown());
}

fn data_lost(s: &Store) -> usize {
    s.losses()
        .iter()
        .filter(|l| l.kind == kvik::LossKind::DataLost)
        .count()
}

/// Damages the eight bytes of `v` in every log segment that holds them.
fn damage(w: &World, v: f64) {
    let bytes = v.to_bits().to_le_bytes();
    let held: Vec<_> =
        w.fs.paths()
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "open" || e == "kvl"))
            .filter_map(|p| w.fs.read(&p).map(|b| (p, b)))
            .filter(|(_, b)| b.windows(8).any(|x| x == bytes))
            .collect();
    assert!(!held.is_empty());
    for (p, b) in held {
        let at = b.windows(8).position(|x| x == bytes).unwrap();
        w.fs.poke(&p, at, &[b[at] ^ 0xFF]);
    }
}

/// Waits until every log segment holding the eight bytes of `v` is sealed: a seal writes the segment from the
/// writer's own bytes, so damage to an open one would not last.
fn wait_sealed_with(w: &World, v: f64) {
    let bytes = v.to_bits().to_le_bytes();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let held: Vec<_> =
            w.fs.paths()
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "open" || e == "kvl"))
                .filter(|p| {
                    w.fs.read(p)
                        .is_some_and(|b| b.windows(8).any(|x| x == bytes))
                })
                .collect();
        if !held.is_empty()
            && held
                .iter()
                .all(|p| p.extension().is_some_and(|e| e == "kvl"))
        {
            return;
        }
        assert!(Instant::now() < deadline, "the value was not sealed");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A series directory written before a fact it counts is found lost is removed in the run, with no batch after the
/// find: the store then idles and seals by time, and no file on disk counts a fact it reports lost (A.18).
#[test]
fn a_series_directory_counting_a_fact_found_lost_is_removed_while_the_store_idles() {
    let w = World::new("derived-lost-left");
    let s = open(&w);
    let g = block_on(s.define_series(gauge("site/dev/power"))).unwrap();
    emit(&s, &[num(g, 0, 1_111.5)]);
    let lost = emit(&s, &[num(g, 1, 2_222.5)]);
    wait_sealed_with(&w, 2_222.5);
    let d = wait_for(&w, lost);
    check(&s, &d, &[g]);
    damage(&w, 2_222.5);
    assert_eq!(data_lost(&s), 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(d) = series_dir(&w) {
        assert!(
            Instant::now() < deadline,
            "a series directory through {} is left",
            d.through.sequence
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    block_on(s.shutdown()).unwrap();
}

/// After a restart over a region in a sealed segment, which no lost range records, a query that meets the region
/// again hands on no second loss, and the run writes no series directory (Q-1002), whichever came first.
#[test]
fn a_query_meeting_a_region_the_start_met_adds_no_loss_and_the_run_writes_no_series_directory() {
    let w = World::new("derived-query-first");
    let s = open(&w);
    let g = block_on(s.define_series(gauge("site/dev/power"))).unwrap();
    emit(&s, &[num(g, 0, 1_111.5)]);
    let lost = emit(&s, &[num(g, 1, 2_222.5)]);
    wait_sealed_with(&w, 2_222.5);
    wait_for(&w, lost);
    emit(&s, &[num(g, 2, 3_333.5)]);
    damage(&w, 2_222.5);
    block_on(s.shutdown()).unwrap();
    let s = open(&w);
    let all = Timestamp(0)..Timestamp(i64::MAX);
    assert_eq!(s.reader().facts(g, all, View::History).unwrap().count(), 2);
    assert_eq!(data_lost(&s), 1);
    emit(&s, &[num(g, 3, 4_444.5)]);
    no_series_directory_after_a_seal(&w);
    assert_eq!(data_lost(&s), 1);
    block_on(s.shutdown()).unwrap();
}

/// Waits for a seal after the call, then for the directories' work after it: none of the run's series directories
/// is on disk.
fn no_series_directory_after_a_seal(w: &World) {
    let sealed =
        w.fs.paths()
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "kvl"))
            .count();
    wait_sealed(w, sealed + 1);
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        series_dir(w).is_none(),
        "{:?}",
        series_dir(w).map(|d| d.through)
    );
}

/// A fact the process finds lost after the deliverer folded it: no series directory written afterwards counts it, so
/// none is written in the run (A.18: a file is a function of the kept log only while nothing at or below `through` is
/// lost since).
#[test]
fn a_fact_found_lost_while_open_is_never_counted_by_a_series_directory_of_the_run() {
    let w = World::new("derived-lost-open");
    let mut c = w.config();
    c.log.seal_interval = Duration::from_secs(2);
    let s = block_on(Store::open(c)).unwrap();
    let g = block_on(s.define_series(gauge("site/dev/power"))).unwrap();
    // With a consumer each emit's batch is taken, and folded, before the emit returns.
    let consumer = block_on(s.consumer("c", kvik::Start::Now, kvik::Select::None)).unwrap();
    for (k, v) in [(0, 1_111.5), (1, 2_222.5), (2, 3_333.5)] {
        emit(&s, &[num(g, k, v)]);
    }
    damage(&w, 2_222.5);
    let all = Timestamp(0)..Timestamp(i64::MAX);
    assert_eq!(s.reader().facts(g, all, View::History).unwrap().count(), 2);
    assert_eq!(data_lost(&s), 1);
    emit(&s, &[num(g, 1, 2_222.5)]);
    wait_sealed(&w, 1);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(d) = series_dir(&w) {
            check(&s, &d, &[g]);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(consumer);
    block_on(s.shutdown()).unwrap();
}

/// A series directory through a position the next start finds lost, in a sealed segment no lost range records, is
/// not continued and is removed at the open: no file on disk counts the lost fact, and the reopened store's first
/// file agrees with what it shows.
/// A region the start meets in a sealed segment, which no lost range records, lies below every series directory the
/// run could write: none is continued or written, so a reader process reads the log whole and reports the loss
/// (Q-952, Q-1002).
#[test]
fn a_run_over_a_region_the_start_meets_and_no_lost_range_records_writes_no_series_directory() {
    let w = World::new("derived-lost-start");
    let s = open(&w);
    let g = block_on(s.define_series(gauge("site/dev/power"))).unwrap();
    emit(&s, &[num(g, 0, 1_111.5)]);
    let lost = emit(&s, &[num(g, 1, 2_222.5)]);
    wait_sealed_with(&w, 2_222.5);
    wait_for(&w, lost);
    emit(&s, &[num(g, 2, 3_333.5)]);
    // The deliverer takes the newest batch before the damage: its last batch read is not the damaged one, which the
    // next refresh would check again and find (Q-1490).
    s.tick().unwrap();
    damage(&w, 2_222.5);
    block_on(s.shutdown()).unwrap();
    assert!(series_dir(&w).is_some());
    let s = open(&w);
    assert_eq!(data_lost(&s), 1, "the start meets the region");
    emit(&s, &[num(g, 3, 4_444.5)]);
    no_series_directory_after_a_seal(&w);
    let r = kvik::Reader::open(kvik::ReaderConfig::new(&w.dir).env(w.config().env)).unwrap();
    let lost: Vec<_> = r
        .losses()
        .into_iter()
        .filter(|l| l.kind == kvik::LossKind::DataLost)
        .collect();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert_eq!(r.live(g).unwrap(), s.reader().live(g).unwrap());
    block_on(s.shutdown()).unwrap();
}

/// Q-1001: a series defined during the run is in a registry file before any series directory covers its
/// definition, so that a reader takes the registry from the file and the log above the directory's `through`.
#[test]
fn a_series_directory_never_covers_an_entry_the_registry_file_lacks() {
    let w = World::new("derived-registry-cover");
    let s = open(&w);
    let g = block_on(s.define_series(gauge("site/dev/m0"))).unwrap();
    emit(&s, &[num(g, 0, 1.0)]);
    block_on(s.shutdown()).unwrap();
    let s = open(&w);
    let late = block_on(s.define_series(gauge("site/dev/m1"))).unwrap();
    let last = emit(&s, &[num(late, 1, 2.0)]);
    wait_sealed(&w, 1);
    let d = wait_for(&w, last);
    let (_, file) = kvik_log::registry::read_latest(&*w.fs, Path::new(&w.dir), &store_id(&w))
        .unwrap()
        .unwrap();
    let defined = file.entries.iter().find_map(|(seq, _, e)| match e {
        kvik_log::entries::Entry::Definition(def) if def.id == late.0 => Some(*seq),
        _ => None,
    });
    let defined = defined.expect("the late definition is in the registry file");
    assert!(defined <= d.through.sequence && defined <= file.through);
    block_on(s.shutdown()).unwrap();
}
