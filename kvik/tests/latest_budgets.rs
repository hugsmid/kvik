//! The latest-value budgets of the query design (roadmap 9.0c), timed: S1, 400 latest values from one snapshot in
//! the writer's process (target 0.2 ms on the desktop class, 2 ms on the minimal class), and S2, the same from a
//! reader of another handle that stays open (2 ms desktop, 10 ms minimal). Both figures of a row are targets, one per
//! hardware class (the design's section 1). The store is on the real file system under the temporary directory, with
//! a seal after every round of readings. Run in a release build, alone:
//! `cargo test --release -p kvik --test latest_budgets -- --ignored --nocapture`.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::block_on;
use kvik::{Kind, Live, Reader, ReaderConfig, Reading, SeriesId, SeriesSpec, Store, StoreConfig};
use kvik::{Timestamp, Value, ValueType};

const SERIES: usize = 400;
const ROUNDS: usize = 1_000;
const RUNS: usize = 5;

fn wall() -> Timestamp {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Timestamp(i64::try_from(ns).unwrap())
}

fn round(store: &Store, ids: &[SeriesId], k: usize) {
    let t = wall();
    let readings: Vec<Reading> = ids
        .iter()
        .map(|id| Reading::new(*id, t, Value::F64(k as f64)))
        .collect();
    block_on(store.emit(&readings)).unwrap();
}

fn sealed(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("log"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "kvl"))
        })
        .count()
}

/// Times a snapshot and the live state of every series from it, after a round of new readings or none: the best and
/// the spread of `RUNS`.
fn timed(what: &str, store: &Store, ids: &[SeriesId], reader: &Reader, new: bool) {
    let mut runs = Vec::new();
    let mut parts = Vec::new();
    for k in 0..RUNS {
        if new {
            round(store, ids, ROUNDS + k);
        }
        std::thread::sleep(Duration::from_millis(50));
        let began = Instant::now();
        let snap = reader.snapshot().unwrap();
        let taken = began.elapsed();
        let live = ids
            .iter()
            .filter(|id| matches!(snap.live(**id), Ok(Live::Value(_))))
            .count();
        let total = began.elapsed();
        runs.push(total);
        parts.push((taken, total - taken));
        assert_eq!(live, ids.len());
    }
    eprintln!("{what}: (snapshot, live calls) {parts:?}");
    let best = runs.iter().min().unwrap();
    let worst = runs.iter().max().unwrap();
    eprintln!(
        "{what}: best {:.3} ms, spread {:.3} ms, runs {:?}",
        best.as_secs_f64() * 1e3,
        (*worst - *best).as_secs_f64() * 1e3,
        runs
    );
}

#[test]
#[ignore = "a measurement"]
fn latest_values_of_400_series_timed() {
    let dir: PathBuf = std::env::temp_dir().join(format!("kvik-latest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = StoreConfig::new(&dir, "Etc/UTC").create_if_missing(true);
    config.log.segment_size = 16 << 20;
    config.log.seal_interval = Duration::from_millis(20);
    let store = block_on(Store::open(config)).unwrap();
    let ids: Vec<SeriesId> = (0..SERIES)
        .map(|k| {
            let spec = SeriesSpec::new(
                format!("site/d{}/m{}", k / 20, k % 20),
                Kind::Gauge {
                    value: ValueType::F64,
                },
                "W",
            );
            block_on(store.define_series(spec)).unwrap()
        })
        .collect();
    let began = Instant::now();
    for k in 0..ROUNDS {
        round(&store, &ids, k);
        std::thread::sleep(Duration::from_millis(25));
    }
    eprintln!(
        "{ROUNDS} rounds of {SERIES} readings in {:.1} s, {} sealed segments",
        began.elapsed().as_secs_f64(),
        sealed(&dir)
    );

    let own = store.reader();
    let began = Instant::now();
    own.snapshot().unwrap();
    eprintln!(
        "S1, first snapshot: {:.3} ms",
        began.elapsed().as_secs_f64() * 1e3
    );
    timed(
        "S1, the writer's process, a round since",
        &store,
        &ids,
        &own,
        true,
    );
    timed(
        "S1, the writer's process, nothing since",
        &store,
        &ids,
        &own,
        false,
    );

    let began = Instant::now();
    let other = Reader::open(ReaderConfig::new(&dir)).unwrap();
    let snap = other.snapshot().unwrap();
    for id in &ids {
        snap.live(*id).unwrap();
    }
    drop(snap);
    eprintln!(
        "S2, first snapshot of a reader just opened: {:.3} ms",
        began.elapsed().as_secs_f64() * 1e3
    );
    timed(
        "S2, another handle, a round since",
        &store,
        &ids,
        &other,
        true,
    );
    timed(
        "S2, another handle, nothing since",
        &store,
        &ids,
        &other,
        false,
    );

    drop(other);
    block_on(store.shutdown()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
