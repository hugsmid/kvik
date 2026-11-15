//! The durable backend: what reaches the log survives crashes and power losses, a reader in another handle sees
//! what is committed, the series registry is recovered at every start, ids are never assigned twice, and the
//! durable backend decides every outcome as the model backend does.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common::{at, block_on, start};
use kvik::env::{Env, FixedBootId, ManualClock, MemFs};
use kvik::{
    Backend, ChangedAtKind, DefineError, Fact, Intent, IntervalLength, Kind, LogConfig, LossKind,
    Outcome, Position, Quality, Reader, ReaderConfig, Reading, Refusal, RetentionClass, SeriesId,
    SeriesSpec, StateValues, Store, StoreConfig, Timestamp, Value, ValueType, View, ZoneName,
};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

/// A durable store's world: its in-memory file system, boot identity and clock.
struct World {
    dir: String,
    fs: Arc<MemFs>,
    boot: Arc<FixedBootId>,
    clock: ManualClock,
    boot_count: u8,
}

impl World {
    fn new(name: &str) -> Self {
        World {
            dir: format!("durable/{name}"),
            fs: Arc::new(MemFs::new()),
            boot: Arc::new(FixedBootId::new(Some([1; 16]))),
            clock: ManualClock::new(start()),
            boot_count: 1,
        }
    }

    fn env(&self) -> Env {
        let mut env = Env::test(self.clock.clone());
        env.fs = self.fs.clone();
        env.boot = self.boot.clone();
        env
    }

    fn config(&self) -> StoreConfig {
        let mut log = LogConfig::default();
        log.segment_size = 16 << 20;
        StoreConfig::new(&self.dir, "Europe/London")
            .create_if_missing(true)
            .env(self.env())
            .log(log)
    }

    fn open(&self) -> Store {
        block_on(Store::open(self.config())).unwrap()
    }

    fn reader(&self) -> Reader {
        Reader::open(ReaderConfig::new(&self.dir).env(self.env())).unwrap()
    }

    /// The process stops (every later file operation fails), the store's handles are dropped, then the power
    /// goes: every file returns to what was synced. The machine boots again with a new boot identity.
    fn power_loss(&mut self, store: Store) {
        self.fs.crash_after(0);
        drop(store);
        self.fs.power_loss();
        self.boot_count += 1;
        self.boot.set(Some([self.boot_count; 16]));
    }

    /// The bytes of the open segments, with their paths.
    fn segments(&self) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        self.fs
            .paths()
            .into_iter()
            .filter(|p| {
                p.starts_with(Path::new(&self.dir).join("log"))
                    && p.extension().is_some_and(|e| e == "open" || e == "kvl")
            })
            .map(|p| {
                let b = self.fs.read(&p).unwrap();
                (p, b)
            })
            .collect()
    }
}

fn gauge(name: &str) -> SeriesSpec {
    SeriesSpec::new(
        name,
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
    .expected_interval(Duration::from_secs(1))
}

fn num(id: SeriesId, secs: i64, v: f64) -> Reading {
    Reading::new(id, at(secs), Value::F64(v))
}

fn history(r: &Reader, id: SeriesId) -> Vec<Fact> {
    r.facts(id, at(-1_000_000)..at(1_000_000), View::History)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn acknowledged_readings_survive_a_power_loss() {
    let mut w = World::new("power-loss");
    let s = w.open();
    let g = block_on(s.define_series(gauge("site/meter/power"))).unwrap();
    let mut acked: Vec<(Position, f64)> = Vec::new();
    for i in 0..50 {
        let ack = block_on(s.emit(&[num(g, i, i as f64), num(g, i, i as f64)])).unwrap();
        assert_eq!(ack.duplicates_dropped(), 1);
        acked.push((ack.first().unwrap(), i as f64));
    }
    // A call that waits on no one: queued, never awaited, so never acknowledged.
    drop(s.try_emit(&[num(g, 1_000, 1.0)]).unwrap());
    w.power_loss(s);

    let s = w.open();
    let r = s.reader();
    let kept = history(&r, g);
    for (p, v) in &acked {
        assert!(
            kept.iter()
                .any(|f| f.position == *p && f.number() == Some(*v)),
            "acknowledged reading at {p} lost"
        );
    }
    // The registry file the start after the power loss writes holds the definition and the start's recovery entry,
    // up to the mark it synced.
    let f = registry_file(&w);
    let kinds: Vec<_> = f
        .entries
        .iter()
        .map(|(_, _, e)| matches!(e, kvik_log::entries::Entry::Recovery(_)))
        .collect();
    assert_eq!(kinds, vec![false, true]);
    assert_eq!(f.max_series_id, 1);
    // The registry was recovered: the series is defined, under its name, and defining it again is idempotent.
    assert_eq!(s.series("site/meter/power").unwrap().id, g);
    assert_eq!(
        block_on(s.define_series(gauge("site/meter/power"))).unwrap(),
        g
    );
    // A replay of what was acknowledged is a duplicate.
    let replay: Vec<_> = acked.iter().map(|(_, v)| num(g, *v as i64, *v)).collect();
    let ack = block_on(s.emit(&replay)).unwrap();
    assert_eq!(ack.recorded(), 0, "{:?}", ack.outcomes);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_reader_process_sees_committed_facts() {
    let w = World::new("reader-process");
    let s = w.open();
    // Another handle on the same files, as another process would open it.
    let r = w.reader();
    let g = block_on(s.define_series(gauge("site/room/temperature"))).unwrap();
    assert_eq!(r.series("site/room/temperature").unwrap().id, g);
    let first = block_on(s.emit(&[num(g, 0, 20.5)])).unwrap();
    let state = r.refresh().unwrap();
    assert!(state.durable >= first.last().unwrap());
    assert_eq!(history(&r, g).len(), 1);
    block_on(s.emit(&[num(g, 1, 21.0), num(g, 0, 20.0)])).unwrap();
    block_on(s.alias(g, "site/room/temp")).unwrap();
    // Every read call reads what was appended since the last.
    let cur: Vec<_> = r
        .facts(g, at(0)..at(2), View::Current)
        .unwrap()
        .map(|f| f.unwrap().number().unwrap())
        .collect();
    assert_eq!(cur, vec![20.0, 21.0]);
    let then: Vec<_> = r
        .facts(g, at(0)..at(2), View::AsOf(first.last().unwrap()))
        .unwrap()
        .map(|f| f.unwrap().number().unwrap())
        .collect();
    assert_eq!(then, vec![20.5]);
    let info = r.series("site/room/temp").unwrap();
    assert_eq!(info.names, vec!["site/room/temperature", "site/room/temp"]);
    assert_eq!(info.latest_t, Some(at(1)));
    // The reader keeps reading after the writer has gone.
    block_on(s.shutdown()).unwrap();
    assert_eq!(history(&r, g).len(), 3);
    // Not answered by a reader yet.
    assert!(matches!(
        r.live(g),
        Err(kvik::QueryError::NotAvailable { .. })
    ));
}

#[test]
fn the_registry_is_recovered_after_crashes() {
    let mut w = World::new("registry-recovery");
    let s = w.open();
    let a = block_on(s.define_series(gauge("site/a/power"))).unwrap();
    let mode = block_on(s.define_series(SeriesSpec::new(
        "site/a/mode",
        Kind::State {
            values: StateValues::Named(vec!["off".into(), "on".into()]),
        },
        "",
    )))
    .unwrap();
    let price =
        block_on(s.define_series(gauge("grid/area/price").versioned(ChangedAtKind::Version)))
            .unwrap();
    let energy = block_on(s.define_series(SeriesSpec::new(
        "site/a/energy",
        Kind::Interval {
            value: ValueType::F64,
            length: IntervalLength::Fixed {
                d: Duration::from_secs(1_800),
                phase: Duration::ZERO,
            },
        },
        "kWh",
    )))
    .unwrap();
    block_on(s.alias(a, "site/a/p")).unwrap();
    block_on(s.retire(mode, at(100))).unwrap();
    block_on(s.set_parameters(
        a,
        at(0),
        kvik::ParameterChange::default().retention(RetentionClass::Diagnostic {
            period: Duration::from_secs(3_600),
        }),
    ))
    .unwrap();
    block_on(s.set_site_zone(&ZoneName::new("Europe/Oslo"), at(86_400))).unwrap();
    let before: Vec<_> = [a, mode, price, energy]
        .iter()
        .map(|id| s.series(*id).unwrap())
        .collect();
    w.power_loss(s);

    // The zone in force is the one the entry named.
    assert!(matches!(
        block_on(Store::open(w.config())),
        Err(kvik::OpenError::SiteZoneMismatch { .. })
    ));
    let mut oslo = w.config();
    oslo.time_zone = "Europe/Oslo".into();
    let s = block_on(Store::open(oslo)).unwrap();
    let after: Vec<_> = [a, mode, price, energy]
        .iter()
        .map(|id| s.series(*id).unwrap())
        .collect();
    assert_eq!(after, before);
    assert_eq!(s.series("site/a/p").unwrap().id, a);
    assert!(matches!(
        s.series("site/a/mode"),
        Err(kvik::QueryError::UnknownSeries)
    ));
    // Rules decided by the recovered registry.
    let ack = block_on(s.emit(&[
        Reading::new(mode, at(100), Value::State(1)),
        Reading::new(mode, at(99), Value::State(1)),
        num(price, 0, 1.0),
        Reading::new(energy, at(900), Value::F64(1.0)),
        Reading::new(energy, at(1_800), Value::F64(1.0)),
    ]))
    .unwrap();
    assert_eq!(
        ack.refused().collect::<Vec<_>>(),
        vec![
            (0, Refusal::Retired),
            (2, Refusal::ChangedAtMissing),
            (3, Refusal::OffPhase)
        ]
    );
    assert!(matches!(
        block_on(s.define_series(gauge("site/a/power").expected_interval(Duration::from_secs(5)))),
        Err(DefineError::Conflict {
            field: "expected_interval"
        })
    ));
    // A retired name can be defined anew: a new series with a new id.
    let mode2 = block_on(s.define_series(SeriesSpec::new("site/a/mode", Kind::Text, ""))).unwrap();
    assert!(mode2.0 > energy.0);
}

#[test]
fn ids_start_at_one_and_never_reuse_a_floor() {
    let w = World::new("id-floor");
    let s = w.open();
    let a = block_on(s.define_series(gauge("x/y/a"))).unwrap();
    assert_eq!(a, SeriesId(1), "ids start at 1 (Q-121)");
    let b = block_on(s.define_series(gauge("x/y/lost"))).unwrap();
    assert_eq!(b, SeriesId(2));
    // Facts after the definition, so that its batch lies below the mark, between complete batches.
    for i in 0..3 {
        block_on(s.emit(&[num(a, i, 1.0)])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    // Damage the batch that holds the second definition: synced bytes lost on the disk.
    let (path, bytes) = w
        .segments()
        .into_iter()
        .find(|(_, b)| b.windows(8).any(|x| x == b"x/y/lost"))
        .expect("a segment holding the definition");
    let at_name = bytes.windows(8).position(|x| x == b"x/y/lost").unwrap();
    w.fs.poke(&path, at_name, b"X");

    let s = w.open();
    let losses = s.losses();
    assert!(
        losses.iter().any(|l| l.kind == LossKind::DataLost),
        "{losses:?}"
    );
    assert!(matches!(s.series(b), Err(kvik::QueryError::UnknownSeries)));
    // The lost definition's id stays assigned: defined again, the series gets a new id.
    let again = block_on(s.define_series(gauge("x/y/lost"))).unwrap();
    assert_eq!(again, SeriesId(3));
    block_on(s.shutdown()).unwrap();
}

/// The store's registry file, read as a start reads it.
fn registry_file(w: &World) -> kvik_log::registry::RegistryFile {
    let store_id = kvik_log::store::read_descriptor(&*w.fs, Path::new(&w.dir))
        .unwrap()
        .store_id;
    kvik_log::registry::read_latest(&*w.fs, Path::new(&w.dir), &store_id)
        .unwrap()
        .expect("a registry file")
        .1
}

#[test]
fn the_registry_file_follows_the_kept_log() {
    let w = World::new("registry-file");
    let s = w.open();
    // Written by the first start, empty.
    assert!(registry_file(&w).entries.is_empty());
    let a = block_on(s.define_series(gauge("x/y/a"))).unwrap();
    block_on(s.alias(a, "x/y/b")).unwrap();
    block_on(s.emit(&[num(a, 0, 1.0)])).unwrap();
    block_on(s.shutdown()).unwrap();
    drop(s);
    // Rewritten at the clean shutdown, up to the mark it synced: the definition and the alias with their positions.
    let f = registry_file(&w);
    assert_eq!(f.entries.len(), 2);
    assert_eq!(f.max_series_id, 1);
    assert!(f.through >= 3);

    // A file that is not a function of the kept log (here it claims a definition at a position that holds a fact,
    // as one from a lost range would) is replaced at the next start; the ids it names stay assigned.
    let mut planted = f.clone();
    let mut def = match &f.entries[0].2 {
        kvik_log::entries::Entry::Definition(d) => (**d).clone(),
        other => panic!("{other:?}"),
    };
    def.id = 50;
    def.name = "x/y/ghost".into();
    planted.entries.push((
        f.through,
        0,
        kvik_log::entries::Entry::Definition(Box::new(def)),
    ));
    planted.max_series_id = 50;
    let store_id = kvik_log::store::read_descriptor(&*w.fs, Path::new(&w.dir))
        .unwrap()
        .store_id;
    kvik_log::registry::write(&*w.fs, Path::new(&w.dir), store_id, 0, &planted).unwrap();
    let s = w.open();
    let rebuilt = registry_file(&w);
    assert_eq!(rebuilt.entries, f.entries, "rebuilt from the kept log");
    assert_eq!(rebuilt.max_series_id, 50, "the floor never goes down");
    assert!(matches!(
        s.series("x/y/ghost"),
        Err(kvik::QueryError::UnknownSeries)
    ));
    assert_eq!(
        block_on(s.define_series(gauge("x/y/c"))).unwrap(),
        SeriesId(51)
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn the_registry_file_carries_the_id_floor_when_the_mark_is_lost() {
    let w = World::new("registry-floor");
    let s = w.open();
    let a = block_on(s.define_series(gauge("x/y/a"))).unwrap();
    block_on(s.define_series(gauge("x/y/lost"))).unwrap();
    for i in 0..3 {
        block_on(s.emit(&[num(a, i, 1.0)])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    // The definition of id 2 is damaged and the mark is gone: only the registry file still knows id 2.
    let (path, bytes) = w
        .segments()
        .into_iter()
        .find(|(_, b)| b.windows(8).any(|x| x == b"x/y/lost"))
        .unwrap();
    let at_name = bytes.windows(8).position(|x| x == b"x/y/lost").unwrap();
    w.fs.poke(&path, at_name, b"X");
    kvik::env::Fs::remove_file(&*w.fs, &Path::new(&w.dir).join("log").join("MARK")).unwrap();
    let s = w.open();
    assert_eq!(
        block_on(s.define_series(gauge("x/y/lost"))).unwrap(),
        SeriesId(3),
        "the id floor of the registry file"
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn orphaned_facts_stay_out_of_views_and_are_exported() {
    let w = World::new("orphaned");
    let s = w.open();
    let a = block_on(s.define_series(gauge("x/y/a"))).unwrap();
    let b = block_on(s.define_series(gauge("x/y/orphan"))).unwrap();
    block_on(s.emit(&[num(b, 0, 7.0), num(b, 1, 8.0)])).unwrap();
    for i in 0..3 {
        block_on(s.emit(&[num(a, i, 1.0)])).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    let (path, bytes) = w
        .segments()
        .into_iter()
        .find(|(_, x)| x.windows(10).any(|n| n == b"x/y/orphan"))
        .unwrap();
    let at_name = bytes.windows(10).position(|n| n == b"x/y/orphan").unwrap();
    w.fs.poke(&path, at_name, b"X");

    let s = w.open();
    // Kept in the log, outside views (spec §2.1): the id is unknown to every call.
    assert!(matches!(s.series(b), Err(kvik::QueryError::UnknownSeries)));
    let r = s.reader();
    assert!(matches!(
        r.facts(b, at(0)..at(2), View::Current),
        Err(kvik::QueryError::UnknownSeries)
    ));
    let ack = block_on(s.emit(&[num(b, 2, 9.0)])).unwrap();
    assert_eq!(
        ack.refused().collect::<Vec<_>>(),
        vec![(0, Refusal::UnknownSeries)]
    );
    // Exported for re-ingest, in position order.
    let orphans = s
        .export_orphaned(Position::ORIGIN..=Position::new(0, u64::MAX))
        .unwrap();
    let values: Vec<_> = orphans.iter().map(|f| (f.series, f.number())).collect();
    assert_eq!(values, vec![(b, Some(7.0)), (b, Some(8.0))]);
    // New ids go above every series id in a kept fact.
    assert_eq!(
        block_on(s.define_series(gauge("x/y/new"))).unwrap(),
        SeriesId(3)
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_store_on_the_system_environment() {
    // The default environment: the operating system's file system, clocks, boot identity and tzdata.
    let dir = std::env::temp_dir().join(format!("kvik-durable-system-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut log = LogConfig::default();
    log.segment_size = 16 << 20;
    let config = || {
        StoreConfig::new(&dir, "Europe/London")
            .create_if_missing(true)
            .log(log.clone())
    };
    let s = block_on(Store::open(config())).unwrap();
    assert!(s.is_durable());
    let g = block_on(s.define_series(gauge("a/b/c"))).unwrap();
    let now = s.now();
    let ack = block_on(s.emit(&[Reading::new(g, now, Value::F64(1.5))])).unwrap();
    assert_eq!(ack.recorded(), 1);
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = block_on(Store::open(config())).unwrap();
    let r = Reader::open(ReaderConfig::new(&dir)).unwrap();
    let fs: Vec<_> = r
        .facts(g, now..Timestamp(now.0 + 1), View::Current)
        .unwrap()
        .map(|f| f.unwrap().number())
        .collect();
    assert_eq!(fs, vec![Some(1.5)]);
    assert_eq!(r.series("a/b/c").unwrap().id, g);
    drop(r);
    block_on(s.shutdown()).unwrap();
    drop(s);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn writers_on_several_threads() {
    let w = World::new("threads");
    let s = w.open();
    let g = block_on(s.define_series(gauge("a/b/c"))).unwrap();
    let writers: Vec<_> = (0..4)
        .map(|k| {
            let s = s.clone();
            thread::spawn(move || {
                (0..25)
                    .map(|i| {
                        block_on(s.emit(&[num(g, k * 100 + i, i as f64)]))
                            .unwrap()
                            .first()
                            .unwrap()
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut positions: Vec<Position> = writers
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    positions.sort();
    positions.dedup();
    assert_eq!(positions.len(), 100);
    assert_eq!(history(&s.reader(), g).len(), 100);
    assert_eq!(s.series_stats(g).unwrap().recorded, 100);
    block_on(s.shutdown()).unwrap();
}

// ----- the same outcomes as the model backend -----

/// One step of a generated history.
#[derive(Clone, Debug)]
enum Step {
    /// A call of readings, each (series index, seconds, value code, quality code, `changed_at`), with revise
    /// intent or not.
    Emit(Vec<(usize, i64, u8, u8, Option<u64>)>, bool),
    Retire(usize, i64),
    Alias(usize),
}

fn specs() -> Vec<SeriesSpec> {
    vec![
        gauge("h/g/power"),
        SeriesSpec::new(
            "h/c/energy",
            Kind::Counter {
                value: ValueType::F64,
            },
            "Wh",
        ),
        SeriesSpec::new(
            "h/s/mode",
            Kind::State {
                values: StateValues::Named(vec!["a".into(), "b".into()]),
            },
            "",
        ),
        SeriesSpec::new("h/t/note", Kind::Text, ""),
        gauge("h/v/price").versioned(ChangedAtKind::Version),
        SeriesSpec::new(
            "h/i/energy",
            Kind::Interval {
                value: ValueType::F64,
                length: IntervalLength::Fixed {
                    d: Duration::from_secs(4),
                    phase: Duration::from_secs(1),
                },
            },
            "kWh",
        ),
        SeriesSpec::new(
            "h/b/door",
            Kind::State {
                values: StateValues::Bool,
            },
            "",
        ),
    ]
}

fn value(code: u8) -> Value {
    match code % 12 {
        0 => Value::F64(1.0),
        1 => Value::F64(2.0),
        2 => Value::F64(-1.0),
        3 => Value::F64(f64::NAN),
        4 => Value::F64(-0.0),
        5 => Value::Absent,
        6 => Value::State(0),
        7 => Value::State(5),
        8 => Value::Text("x".into()),
        9 => Value::Text("é".repeat(200)),
        10 => Value::Bool(true),
        _ => Value::Decimal { units: 1, scale: 1 },
    }
}

fn quality(code: u8) -> Quality {
    [
        Quality::Good,
        Quality::Estimated,
        Quality::Suspect,
        Quality::Bad,
    ][usize::from(code % 4)]
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let reading = (
        0usize..8,
        -400i64..20,
        any::<u8>(),
        any::<u8>(),
        prop_oneof![Just(None), (0u64..3).prop_map(Some)],
    );
    let step = prop_oneof![
        6 => (prop::collection::vec(reading, 1..6), any::<bool>()).prop_map(|(r, rev)| Step::Emit(r, rev)),
        1 => (0usize..7, -100i64..20).prop_map(|(i, t)| Step::Retire(i, t)),
        1 => (0usize..7).prop_map(Step::Alias),
    ];
    prop::collection::vec(step, 1..12)
}

/// Runs a history on a store; returns every result in a comparable form, and the facts of each series.
fn run(s: &Store, history: &[Step]) -> (Vec<String>, Vec<Vec<Fact>>) {
    let mut out = Vec::new();
    let ids: Vec<SeriesId> = specs()
        .into_iter()
        .map(|spec| block_on(s.define_series(spec)).unwrap())
        .collect();
    let id = |i: usize| ids.get(i).copied().unwrap_or(SeriesId(999));
    for (n, step) in history.iter().enumerate() {
        match step {
            Step::Emit(rs, revise) => {
                let readings: Vec<_> = rs
                    .iter()
                    .map(|(i, t, v, q, c)| {
                        let mut r =
                            Reading::new(id(*i), at(*t), value(*v)).with_quality(quality(*q));
                        r.changed_at = *c;
                        r
                    })
                    .collect();
                let intent = if *revise {
                    Intent::Revise
                } else {
                    Intent::Normal
                };
                let ack = block_on(s.emit_with(&readings, intent)).unwrap();
                let outcomes: Vec<String> = ack
                    .outcomes
                    .iter()
                    .map(|o| match o {
                        Outcome::Recorded { outranked, .. } => {
                            format!("recorded outranked={outranked}")
                        }
                        other => format!("{other:?}"),
                    })
                    .collect();
                out.push(format!("emit {outcomes:?}"));
            }
            Step::Retire(i, t) => out.push(format!(
                "retire {:?}",
                block_on(s.retire(id(*i), at(*t))).map_err(|e| e.to_string())
            )),
            Step::Alias(i) => out.push(format!(
                "alias {:?}",
                block_on(s.alias(id(*i), &format!("h/alias/{n}"))).map_err(|e| e.to_string())
            )),
        }
    }
    let r = s.reader();
    let facts = ids
        .iter()
        .map(|id| {
            history_facts(&r, *id)
                .into_iter()
                .map(|mut f| {
                    // Positions differ only by what each backend counts besides facts; compare everything else.
                    f.position = Position::ORIGIN;
                    f
                })
                .collect()
        })
        .collect();
    (out, facts)
}

fn history_facts(r: &Reader, id: SeriesId) -> Vec<Fact> {
    let mut fs = history(r, id);
    for f in &mut fs {
        // NaN is never stored, so facts compare with `==`.
        assert!(!matches!(f.value, Value::F64(x) if x.is_nan()));
    }
    fs
}

#[test]
fn the_durable_backend_decides_as_the_model_does() {
    // Each case opens a durable store; a sixteenth of the configured cases keeps the deep run in bounds.
    let cases = (ProptestConfig::default().cases / 16).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner
        .run(&steps(), |history| {
            let clock = ManualClock::new(start());
            let model = block_on(Store::open(
                StoreConfig::new(
                    format!("kvik-test/differential/{}", next()),
                    "Europe/London",
                )
                .create_if_missing(true)
                .backend(Backend::Model)
                .env(Env::test(clock.clone())),
            ))
            .unwrap();
            let w = World {
                clock,
                ..World::new("differential")
            };
            let mut config = w.config();
            config.log.min_sync_interval = Duration::ZERO;
            let durable = block_on(Store::open(config)).unwrap();
            let (m_out, m_facts) = run(&model, &history);
            let (d_out, d_facts) = run(&durable, &history);
            prop_assert_eq!(m_out, d_out);
            prop_assert_eq!(m_facts, d_facts);
            block_on(durable.shutdown()).unwrap();
            block_on(model.shutdown()).unwrap();
            Ok(())
        })
        .unwrap();
}

fn next() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}
