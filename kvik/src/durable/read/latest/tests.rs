use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kvik_log::directory::series::SeriesDirectory;
use kvik_log::directory::{Family, Frame};
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig as LogReaderConfig};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use crate::config::{Backend, LogConfig, ReaderConfig, StoreConfig};
use crate::durable::derived::SeriesFold;
use crate::durable::tests::block_on;
use crate::env::{Env, ManualClock, MemFs};
use crate::read::Live;
use crate::series::{ChangedAtKind, IntervalLength, Kind, SeriesSpec, StateValues, Timing};
use crate::time::Timestamp;
use crate::types::{Intent, Position, Quality, Reading, SeriesId, Value};
use crate::{Reader, Store, ValueType};

pub(super) const T0: i64 = 1_767_225_600_000_000_000;
pub(super) const S: i64 = 1_000_000_000;

pub(super) fn specs() -> Vec<SeriesSpec> {
    let f64 = || Kind::Gauge {
        value: ValueType::F64,
    };
    vec![
        SeriesSpec::new("h/g/power", f64(), "W").staleness(Duration::from_secs(30)),
        SeriesSpec::new("h/p/price", f64(), "c").timing(Timing::Scheduled),
        SeriesSpec::new("h/t/note", Kind::Text, ""),
        SeriesSpec::new("h/v/price", f64(), "c").versioned(ChangedAtKind::Version),
        SeriesSpec::new(
            "h/i/energy",
            Kind::Interval {
                value: ValueType::F64,
                length: IntervalLength::Fixed {
                    d: Duration::from_secs(4),
                    phase: Duration::ZERO,
                },
            },
            "kWh",
        ),
        SeriesSpec::new(
            "h/s/mode",
            Kind::State {
                values: StateValues::Named(vec!["a".into(), "b".into()]),
            },
            "",
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

/// A value of the series' type, or none: index 5's state 5 is undeclared, so never live.
pub(super) fn value(series: usize, code: u8) -> Value {
    match (series, code % 3) {
        (_, 2) => Value::Absent,
        (2, k) => Value::Text(["one", "two"][usize::from(k)].into()),
        (5, 0) => Value::State(0),
        (5, _) => Value::State(5),
        (6, k) => Value::Bool(k == 0),
        (_, k) => Value::F64(f64::from(k) + 1.0),
    }
}

pub(super) fn quality(code: u8) -> Quality {
    [Quality::Good, Quality::Good, Quality::Suspect, Quality::Bad][usize::from(code % 4)]
}

/// One step of a generated history.
#[derive(Clone, Debug)]
pub(super) enum Step {
    /// Readings (series, seconds from now, value code, quality code, `changed_at`), with revise intent or not.
    Emit(Vec<(usize, i64, u8, u8, u64)>, bool),
    /// The clock moves on by this many seconds.
    Advance(u64),
    /// The store is shut down and opened again.
    Restart,
    /// With the store shut down, a series directory as of a batch boundary chosen by this number, and a reader
    /// that continues it.
    Directory(u16),
    /// Every reader is compared with the model.
    Check,
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let reading = (
        0usize..7,
        prop_oneof![3 => prop::sample::select(vec![-60i64, -30, -4, 0, 4, 30, 90]), 2 => -300i64..120],
        any::<u8>(),
        any::<u8>(),
        0u64..3,
    );
    let step = prop_oneof![
        8 => (prop::collection::vec(reading, 1..6), prop::bool::weighted(0.2))
            .prop_map(|(r, revise)| Step::Emit(r, revise)),
        3 => (0u64..120).prop_map(Step::Advance),
        1 => Just(Step::Restart),
        2 => any::<u16>().prop_map(Step::Directory),
        2 => Just(Step::Check),
    ];
    prop::collection::vec(step, 1..14)
}

fn next() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

/// Both backends on one clock, the durable store's readers, and what they found.
pub(super) struct Rig {
    pub(super) clock: ManualClock,
    pub(super) env: Env,
    pub(super) dir: PathBuf,
    pub(super) fs: MemFs,
    pub(super) store: Option<Store>,
    pub(super) model: Store,
    pub(super) ids: Vec<SeriesId>,
    /// A reader opened with the store, kept across its restarts.
    pub(super) long: Reader,
    /// Readers that continue a series directory written for them.
    pub(super) continuing: Vec<Reader>,
    /// Readers that started from a directory.
    pub(super) seeded: u64,
    pub(super) searches: u64,
    log: LogConfig,
}

impl Rig {
    pub(super) fn new(name: &str) -> Self {
        Rig::with_log(name, LogConfig::default())
    }

    /// A rig whose durable store has this log configuration.
    pub(super) fn with_log(name: &str, log: LogConfig) -> Self {
        let clock = ManualClock::new(Timestamp(T0));
        let mut env = Env::test(clock.clone());
        let fs = MemFs::new();
        env.fs = Arc::new(fs.clone());
        let dir = PathBuf::from(format!("latest/{name}-{}", next()));
        let config = |d: &Path, b: Backend, env: Env| {
            StoreConfig::new(d, "Europe/London")
                .create_if_missing(true)
                .backend(b)
                .env(env)
        };
        let store = block_on(Store::open(
            config(&dir, Backend::Durable, env.clone()).log(log.clone()),
        ))
        .unwrap();
        let mdir = PathBuf::from(format!("latest-model/{name}-{}", next()));
        let model = block_on(Store::open(config(
            &mdir,
            Backend::Model,
            Env::test(clock.clone()),
        )))
        .unwrap();
        let mut ids = Vec::new();
        for spec in specs() {
            let id = block_on(store.define_series(spec.clone())).unwrap();
            assert_eq!(block_on(model.define_series(spec)).unwrap(), id);
            ids.push(id);
        }
        let long = Reader::open(ReaderConfig::new(&dir).env(env.clone())).unwrap();
        Rig {
            clock,
            env,
            dir,
            fs,
            store: Some(store),
            model,
            ids,
            long,
            continuing: Vec::new(),
            seeded: 0,
            searches: 0,
            log,
        }
    }

    pub(super) fn reopen(&mut self) {
        let config = StoreConfig::new(&self.dir, "Europe/London")
            .env(self.env.clone())
            .log(self.log.clone());
        self.store = Some(block_on(Store::open(config)).unwrap());
    }

    pub(super) fn shut(&mut self) {
        if let Some(s) = self.store.take() {
            block_on(s.shutdown()).unwrap();
        }
    }

    pub(super) fn emit(&mut self, rs: &[(usize, i64, u8, u8, u64)], revise: bool) {
        let now = self.clock.wall().0;
        let readings: Vec<Reading> = rs
            .iter()
            .map(|&(i, secs, v, q, c)| {
                let mut r = Reading::new(
                    self.ids[i],
                    Timestamp(now.saturating_add(secs * S)),
                    value(i, v),
                )
                .with_quality(quality(q));
                if i == 3 {
                    r.changed_at = Some(c);
                }
                r
            })
            .collect();
        let intent = if revise {
            Intent::Revise
        } else {
            Intent::Normal
        };
        self.emit_readings(&readings, intent);
    }

    /// Emits `readings` to both backends, which accept or refuse the call alike.
    pub(super) fn emit_readings(&mut self, readings: &[Reading], intent: Intent) {
        let m = block_on(self.model.emit_with(readings, intent)).map(|a| a.outcomes.len());
        let store = self.store.as_ref().expect("an open store");
        let d = block_on(store.emit_with(readings, intent)).map(|a| a.outcomes.len());
        assert_eq!(m.is_ok(), d.is_ok(), "{m:?} {d:?}");
    }

    fn step(&mut self, s: &Step) -> Result<(), TestCaseError> {
        match s {
            Step::Emit(rs, revise) => self.emit(rs, *revise),
            Step::Advance(secs) => self.clock.advance(Duration::from_secs(*secs)),
            Step::Restart => {
                self.shut();
                self.reopen();
            }
            Step::Directory(k) => {
                self.shut();
                if write_directory(&self.env, &self.dir, usize::from(*k)).is_some() {
                    let r =
                        Reader::open(ReaderConfig::new(&self.dir).env(self.env.clone())).unwrap();
                    if self.continuing.len() == 3 {
                        self.continuing.remove(0);
                    }
                    self.continuing.push(r);
                }
                self.check()?;
                self.reopen();
            }
            Step::Check => self.check()?,
        }
        Ok(())
    }

    /// A reader opened now.
    pub(super) fn reader(&self) -> Reader {
        Reader::open(ReaderConfig::new(&self.dir).env(self.env.clone())).unwrap()
    }

    /// Every durable reader's live state, series and losses equal the model's: the facts with positions set aside
    /// (the model writes no recovery or tzdata entry). Staleness follows from the fact by the rule of roadmap 5.4,
    /// whose defaults differ between the backends (Q-16), so it is not compared.
    pub(super) fn check(&mut self) -> Result<(), TestCaseError> {
        let fresh = Reader::open(ReaderConfig::new(&self.dir).env(self.env.clone())).unwrap();
        let snap = fresh.snapshot().unwrap();
        let own = self.store.as_ref().map(Store::reader);
        let mut readers: Vec<&Reader> = vec![&self.long, &fresh];
        readers.extend(self.continuing.iter());
        readers.extend(own.as_ref());
        let m = self.model.reader();
        // The backends fill a spec's unset keys differently: the fields the registry and the facts give.
        let info = |i: crate::SeriesInfo| {
            (
                i.id,
                i.names,
                i.retired_at,
                i.latest_t,
                i.lateness_reference,
                i.spec.timing,
            )
        };
        for id in &self.ids {
            let want = m.live(*id).unwrap();
            let name = m.series(*id).unwrap().name;
            let wanted = info(m.series(*id).unwrap());
            for r in &readers {
                let got = r.live(*id).unwrap();
                prop_assert_eq!(plain(got), plain(want.clone()), "series {:?}", id);
                prop_assert_eq!(info(r.series(*id).unwrap()), wanted.clone());
            }
            prop_assert_eq!(plain(snap.live(*id).unwrap()), plain(want));
            prop_assert_eq!(info(snap.series(name.as_str()).unwrap()), wanted);
        }
        let want = self.model.losses();
        for r in &readers {
            prop_assert_eq!(r.losses(), want.clone());
        }
        prop_assert_eq!(snap.losses().unwrap(), want);
        for r in readers {
            if let Some(d) = r.durable() {
                let d = d.unwrap();
                self.searches += d.searches();
                self.seeded += u64::from(d.seeded());
            }
        }
        Ok(())
    }
}

/// A live state as the backends agree on it: the fact's position and the staleness set aside.
pub(super) fn plain(l: Live) -> Live {
    match l {
        Live::Value(mut v) => {
            v.fact.position = Position::ORIGIN;
            v.stale = false;
            Live::Value(v)
        }
        other => other,
    }
}

/// With the store shut down: a series directory as the writer's fold gives it as of the `k`-th batch boundary (in
/// a cycle; the last for `usize::MAX`), written as the newest generation, under the registry file the clean stop
/// wrote (Q-1001). `None` without a batch.
pub(super) fn write_directory(
    env: &Env,
    dir: &Path,
    k: usize,
) -> Option<kvik_log::writer::Position> {
    write_directory_under(env, dir, k, None)
}

/// [`write_directory`], recording `stamp` in place of the mark's reset stamp when given.
pub(super) fn write_directory_under(
    env: &Env,
    dir: &Path,
    k: usize,
    stamp: Option<kvik_log::mark::ResetStamp>,
) -> Option<kvik_log::writer::Position> {
    let store_id = kvik_log::store::read_descriptor(&*env.fs, dir)
        .unwrap()
        .store_id;
    let clock = Arc::new(kvik_log::writer::SystemClock::default());
    let mut log = LogReader::open(LogReaderConfig::new(
        dir,
        env.fs.clone(),
        clock,
        env.boot.current(),
        [0xab; 16],
    ))
    .unwrap();
    let u = log.refresh().unwrap();
    let _ = log.close();
    let batches: Vec<_> = u
        .events
        .iter()
        .filter_map(|e| match e {
            ReadEvent::Batch(b) => Some(b),
            _ => None,
        })
        .collect();
    let at = if k == usize::MAX {
        batches.len().checked_sub(1)?
    } else {
        k % batches.len().max(1)
    };
    let through = batches.get(at)?.last;
    let fold = SeriesFold::new(through);
    for b in batches.iter().filter(|b| b.last <= through) {
        fold.batch(b);
    }
    let wall = env.clock.wall().0;
    let e = fold.entries(through, wall)?;
    let st = u.state;
    let d = SeriesDirectory {
        through: e.through,
        lost: 0,
        stamp: stamp.or((st.stamp != Default::default()).then_some(st.stamp)),
        judged_at: e.judged_at,
        entries: e.entries,
    };
    let id = kvik_log::directory::generations(&*env.fs, dir, Family::Series)
        .first()
        .map_or(1, |(id, _)| id + 1);
    let frame = Frame {
        generation: st.generation,
        file_id: id,
        written_at: wall,
    };
    let bytes = d.encode(frame, store_id).unwrap();
    kvik_log::directory::write_file(&*env.fs, dir, Family::Series, id, &bytes).unwrap();
    Some(through)
}

/// Runs a history and checks at its end.
pub(super) fn run(history: &[Step]) -> Result<(u64, u64), TestCaseError> {
    let mut rig = Rig::new("history");
    for s in history {
        rig.step(s)?;
    }
    rig.check()?;
    rig.shut();
    Ok((rig.seeded, rig.searches))
}

#[test]
fn latest_values_equal_the_models_at_every_check() {
    // Each case opens two stores and several readers: a quarter of the configured cases.
    let cases = (ProptestConfig::default().cases / 4).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let (seeded, searches) = (AtomicU64::new(0), AtomicU64::new(0));
    runner
        .run(&steps(), |history| {
            let (s, f) = run(&history)?;
            seeded.fetch_add(s, Ordering::Relaxed);
            searches.fetch_add(f, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
    assert!(
        seeded.into_inner() > 0,
        "some reader continues a series directory"
    );
    assert!(
        searches.into_inner() > 0,
        "some live value is found in the log"
    );
}
