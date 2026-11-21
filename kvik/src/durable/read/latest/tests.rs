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

pub(in crate::durable::read) const T0: i64 = 1_767_225_600_000_000_000;
pub(in crate::durable::read) const S: i64 = 1_000_000_000;

pub(in crate::durable::read) fn specs() -> Vec<SeriesSpec> {
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
pub(in crate::durable::read) fn value(series: usize, code: u8) -> Value {
    match (series, code % 3) {
        (_, 2) => Value::Absent,
        (2, k) => Value::Text(["one", "two"][usize::from(k)].into()),
        (5, 0) => Value::State(0),
        (5, _) => Value::State(5),
        (6, k) => Value::Bool(k == 0),
        (_, k) => Value::F64(f64::from(k) + 1.0),
    }
}

pub(in crate::durable::read) fn quality(code: u8) -> Quality {
    [Quality::Good, Quality::Good, Quality::Suspect, Quality::Bad][usize::from(code % 4)]
}

/// One step of a generated history.
#[derive(Clone, Debug)]
pub(in crate::durable::read) enum Step {
    /// Readings (series, seconds from now, value code, quality code, `changed_at`), with revise intent or not.
    Emit(Vec<(usize, i64, u8, u8, u64)>, bool),
    /// The clock moves on by this many seconds.
    Advance(u64),
    /// The store is shut down and opened again.
    Restart,
    /// The power goes and the store is opened again after a reboot.
    PowerLoss,
    /// The power goes, the batch holding a fact chosen by this number is damaged, and the store is opened again:
    /// the start finds the damage. Readers are then compared with a fresh reader's fact index, not the model; with
    /// the flag, also before the start, while no record names the damage (Q-1071).
    Damage(u16, bool),
    /// With the store shut down, a series directory as of a batch boundary chosen by this number, and a reader
    /// that continues it.
    Directory(u16),
    /// Every reader is compared with the model.
    Check,
}

pub(in crate::durable::read) fn steps() -> impl Strategy<Value = Vec<Step>> {
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
        1 => Just(Step::PowerLoss),
        2 => (any::<u16>(), any::<bool>()).prop_map(|(k, unmet)| Step::Damage(k, unmet)),
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
pub(in crate::durable::read) struct Rig {
    pub(in crate::durable::read) clock: ManualClock,
    pub(in crate::durable::read) env: Env,
    pub(in crate::durable::read) dir: PathBuf,
    pub(in crate::durable::read) fs: MemFs,
    pub(in crate::durable::read) store: Option<Store>,
    pub(in crate::durable::read) model: Store,
    pub(in crate::durable::read) ids: Vec<SeriesId>,
    /// A reader opened with the store, kept across its restarts.
    pub(in crate::durable::read) long: Reader,
    /// Readers that continue a series directory written for them.
    pub(in crate::durable::read) continuing: Vec<Reader>,
    /// Readers that started from a directory.
    pub(in crate::durable::read) seeded: u64,
    pub(in crate::durable::read) searches: u64,
    log: LogConfig,
    /// The last boot identity.
    boot: u8,
    /// A batch was damaged: the model no longer describes the store.
    pub(in crate::durable::read) faulted: bool,
    /// Checks made after damage.
    pub(in crate::durable::read) after_damage: u64,
    /// Loss reports compared with the commit time before set aside (Q-1071).
    pub(super) set_aside: u64,
    /// The `through` of the last series directory the rig wrote; kept when a run removes the directories, where it
    /// only narrows the choice of batch to damage.
    directory: Option<kvik_log::writer::Position>,
}

impl Rig {
    pub(in crate::durable::read) fn new(name: &str) -> Self {
        Rig::with_log(name, LogConfig::default())
    }

    /// A rig whose durable store has this log configuration.
    pub(in crate::durable::read) fn with_log(name: &str, log: LogConfig) -> Self {
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
            boot: 1,
            faulted: false,
            after_damage: 0,
            set_aside: 0,
            directory: None,
        }
    }

    pub(in crate::durable::read) fn reopen(&mut self) {
        let config = StoreConfig::new(&self.dir, "Europe/London")
            .env(self.env.clone())
            .log(self.log.clone());
        self.store = Some(block_on(Store::open(config)).unwrap());
    }

    pub(in crate::durable::read) fn shut(&mut self) {
        if let Some(s) = self.store.take() {
            block_on(s.shutdown()).unwrap();
        }
    }

    pub(in crate::durable::read) fn emit(
        &mut self,
        rs: &[(usize, i64, u8, u8, u64)],
        revise: bool,
    ) {
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
    pub(in crate::durable::read) fn emit_readings(&mut self, readings: &[Reading], intent: Intent) {
        let m = block_on(self.model.emit_with(readings, intent)).map(|a| a.outcomes.len());
        let store = self.store.as_ref().expect("an open store");
        let d = block_on(store.emit_with(readings, intent)).map(|a| a.outcomes.len());
        assert_eq!(m.is_ok(), d.is_ok(), "{m:?} {d:?}");
    }

    pub(in crate::durable::read) fn step(&mut self, s: &Step) -> Result<(), TestCaseError> {
        match s {
            Step::Emit(rs, revise) => self.emit(rs, *revise),
            Step::Advance(secs) => self.clock.advance(Duration::from_secs(*secs)),
            Step::Restart => {
                self.shut();
                self.reopen();
            }
            Step::PowerLoss => {
                self.power_loss();
                self.reopen();
            }
            Step::Damage(k, false) => {
                self.power_loss();
                if self.damage_batch(usize::from(*k), None).is_some() {
                    self.faulted = true;
                }
                self.reopen();
            }
            Step::Damage(k, true) => {
                // A clean stop keeps the series directory a reader continues (Q-1003); the power loss after the
                // check makes the start find the damage.
                self.shut();
                if self.damage_batch(usize::from(*k), self.directory).is_some() {
                    self.faulted = true;
                    self.check_unmet_damage()?;
                }
                self.power_loss();
                self.reopen();
            }
            Step::Directory(k) => {
                self.shut();
                if let Some(through) = write_directory(&self.env, &self.dir, usize::from(*k)) {
                    self.directory = Some(through);
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

    /// The process stops, the power goes and the machine boots again with a new boot identity; the store is not
    /// opened again.
    pub(in crate::durable::read) fn power_loss(&mut self) {
        self.fs.crash_after(0);
        drop(self.store.take());
        self.fs.power_loss();
        self.boot = self.boot.wrapping_add(1).max(1);
        self.env.boot = Arc::new(crate::env::FixedBootId::new(Some([self.boot; 16])));
    }

    /// Damages the batch holding a fact chosen by `k`, in a cycle, among those above `above` when given: for an
    /// even `k` the first of them; its first position, `None` with none.
    pub(in crate::durable::read) fn damage_batch(
        &self,
        k: usize,
        above: Option<kvik_log::writer::Position>,
    ) -> Option<Position> {
        let clock = Arc::new(kvik_log::writer::SystemClock::default());
        let mut log = LogReader::open(LogReaderConfig::new(
            &self.dir,
            self.env.fs.clone(),
            clock,
            self.env.boot.current(),
            [0xac; 16],
        ))
        .unwrap();
        let u = log.refresh().unwrap();
        let _ = log.close();
        let held: Vec<_> = u
            .events
            .iter()
            .filter_map(|e| match e {
                ReadEvent::Batch(b) => Some(b),
                _ => None,
            })
            .filter(|b| {
                b.content
                    .items
                    .iter()
                    .any(|i| matches!(i, kvik_log::content::Item::Fact(_)))
            })
            .filter(|b| above.is_none_or(|p| b.first > p))
            .collect();
        let at = if above.is_some() && k.is_multiple_of(2) {
            0
        } else {
            k % held.len().max(1)
        };
        let b = held.get(at)?;
        let segment = self.fs.paths().into_iter().find(|p| {
            p.starts_with(self.dir.join("log"))
                && p.to_string_lossy().contains(&format!("{:016x}", b.file_id))
        })?;
        self.fs.poke(&segment, b.offset as usize + 40, b"XXXXXXXX");
        Some(crate::durable::index::pos(b.first))
    }

    /// A reader opened now.
    pub(in crate::durable::read) fn reader(&self) -> Reader {
        Reader::open(ReaderConfig::new(&self.dir).env(self.env.clone())).unwrap()
    }

    /// A reader opened now that reads the whole log: the series directories are out of its way while it seeds.
    fn whole_reader(&self) -> Reader {
        let fs = &*self.env.fs;
        let moved: Vec<(PathBuf, PathBuf)> =
            kvik_log::directory::generations(fs, &self.dir, Family::Series)
                .into_iter()
                .map(|(_, p)| (Path::new("hidden").join(&p), p))
                .collect();
        for (to, from) in &moved {
            fs.rename(from, to).unwrap();
        }
        let r = self.reader();
        // The reader seeds at its first call, while the directories are away.
        r.live(self.ids[0]).unwrap();
        for (from, to) in &moved {
            fs.rename(from, to).unwrap();
        }
        r
    }

    /// Before any start meets the damage: a reader that continues a series directory, which lies below the damage,
    /// reports the losses a reader of the whole log reports, the commit time before set aside as Q-1071 rules.
    fn check_unmet_damage(&mut self) -> Result<(), TestCaseError> {
        let whole = self.whole_reader();
        prop_assert_eq!(seed(&whole), None);
        let fresh = self.reader();
        let snap = fresh.snapshot().unwrap();
        let at = seed(&fresh);
        let want = (whole.losses(), None);
        prop_assert!(!want.0.is_empty(), "the damage is reported");
        for got in [fresh.losses(), snap.losses().unwrap()] {
            let (alike, wanted) = known_alike((got.clone(), at), want.clone());
            self.set_aside += u64::from(alike != got || wanted != want.0);
            prop_assert_eq!(alike, wanted);
        }
        Ok(())
    }

    /// Every durable reader's live state, series and losses equal the model's: the facts with positions set aside
    /// (the model writes no recovery or tzdata entry). Staleness follows from the fact by the rule of roadmap 5.4,
    /// whose defaults differ between the backends (Q-16), so it is not compared.
    pub(in crate::durable::read) fn check(&mut self) -> Result<(), TestCaseError> {
        if self.faulted {
            return self.check_after_damage();
        }
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

impl Rig {
    /// After damage, every reader's live state, series and losses, a snapshot's among them, equal what a fresh
    /// reader's fact index gives (Q-1006): the model holds no loss.
    fn check_after_damage(&mut self) -> Result<(), TestCaseError> {
        let reference = self.reader();
        let d = reference.durable().unwrap().unwrap();
        let fresh = self.reader();
        let snap = fresh.snapshot().unwrap();
        let own = self.store.as_ref().map(Store::reader);
        let mut readers: Vec<&Reader> = vec![&self.long, &fresh];
        readers.extend(self.continuing.iter());
        readers.extend(own.as_ref());
        let info = |i: crate::SeriesInfo| (i.id, i.latest_t, i.lateness_reference);
        for id in &self.ids {
            let (want, wanted) = d
                .with(|i, _| {
                    let live = crate::durable::windows::live_in(i, d.clocks(), *id, d.now())?;
                    Ok((live, i.series(&crate::SeriesKey::Id(*id))?))
                })
                .unwrap();
            for r in &readers {
                prop_assert_eq!(plain(r.live(*id).unwrap()), plain(want.clone()), "{:?}", id);
                prop_assert_eq!(info(r.series(*id).unwrap()), info(wanted.clone()));
            }
            prop_assert_eq!(plain(snap.live(*id).unwrap()), plain(want));
        }
        let want = (reference.losses(), seed(&reference));
        prop_assert!(!want.0.is_empty(), "the damage is reported");
        let mut set_aside = 0;
        let mut compare = |got: Losses, seed: Option<Position>| {
            let (alike, wanted) = known_alike((got.clone(), seed), want.clone());
            set_aside += u64::from(alike != got || wanted != want.0);
            prop_assert_eq!(alike, wanted);
            Ok(())
        };
        for r in &readers {
            compare(r.losses(), seed(r))?;
        }
        compare(snap.losses().unwrap(), seed(&fresh))?;
        self.set_aside += set_aside;
        self.after_damage += 1;
        Ok(())
    }
}

/// The `through` of the series directory `r` continues, if it continues one.
fn seed(r: &Reader) -> Option<Position> {
    r.durable()?.ok()?.seeded_through()
}

type Losses = Vec<crate::health::LossReport>;

/// Two readers' loss reports, each with the directory its reader continues, with the commit time before an
/// unrecorded loss set aside only where a side that continues a directory lacks it for a loss just above that
/// directory: such a reader reads only above it (spec §11 Health "what is known"; Q-1071).
fn known_alike(a: (Losses, Option<Position>), b: (Losses, Option<Position>)) -> (Losses, Losses) {
    let unknown = |r: &crate::health::LossReport, seed: Option<Position>| {
        r.commit_time_before.is_none()
            && seed.is_some_and(|t| {
                r.first
                    == Some(Position {
                        generation: t.generation,
                        sequence: t.sequence + 1,
                    })
            })
    };
    let ((mut a, sa), (mut b, sb)) = (a, b);
    for (x, y) in a.iter_mut().zip(b.iter_mut()) {
        if !x.recorded && x.identity == y.identity && (unknown(x, sa) || unknown(y, sb)) {
            x.commit_time_before = None;
            y.commit_time_before = None;
        }
    }
    (a, b)
}

/// A live state as the backends agree on it: the fact's position and the staleness set aside.
pub(in crate::durable::read) fn plain(l: Live) -> Live {
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
pub(in crate::durable::read) fn write_directory(
    env: &Env,
    dir: &Path,
    k: usize,
) -> Option<kvik_log::writer::Position> {
    write_directory_under(env, dir, k, None)
}

/// [`write_directory`], recording `stamp` in place of the mark's reset stamp when given.
pub(in crate::durable::read) fn write_directory_under(
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

/// The newest series directory's bytes.
pub(in crate::durable::read) fn newest_directory(fs: &MemFs, dir: &Path) -> Vec<u8> {
    let (_, path) = kvik_log::directory::generations(fs, dir, Family::Series)
        .into_iter()
        .next()
        .expect("a series directory");
    fs.read(&path).unwrap()
}

/// Writes the series directory `bytes` again as of the mark, under its reset stamp and through its position, claiming
/// `lost` lost entries below: a directory the reader's checks accept whatever the log lost since it was folded.
pub(in crate::durable::read) fn restamp_directory(env: &Env, dir: &Path, bytes: &[u8], lost: u32) {
    let fs = &*env.fs;
    let store_id = kvik_log::store::read_descriptor(fs, dir).unwrap().store_id;
    let (frame, d) = SeriesDirectory::decode(bytes, &store_id).unwrap();
    let clock = Arc::new(kvik_log::writer::SystemClock::default());
    let mut log = LogReader::open(LogReaderConfig::new(
        dir,
        env.fs.clone(),
        clock,
        env.boot.current(),
        [0xae; 16],
    ))
    .unwrap();
    let st = log.refresh().unwrap().state;
    let _ = log.close();
    let d = SeriesDirectory {
        through: st.durable,
        stamp: Some(st.stamp),
        lost,
        ..d
    };
    let id = kvik_log::directory::generations(fs, dir, Family::Series)
        .first()
        .map_or(frame.file_id, |(id, _)| *id)
        + 1;
    let frame = Frame {
        file_id: id,
        ..frame
    };
    let bytes = d.encode(frame, store_id).unwrap();
    kvik_log::directory::write_file(fs, dir, Family::Series, id, &bytes).unwrap();
}

/// Runs a history and checks at its end.
pub(in crate::durable::read) fn run(history: &[Step]) -> Result<[u64; 4], TestCaseError> {
    let mut rig = Rig::new("history");
    for s in history {
        rig.step(s)?;
    }
    rig.check()?;
    rig.shut();
    Ok([rig.seeded, rig.searches, rig.after_damage, rig.set_aside])
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
    let counts: [AtomicU64; 4] = Default::default();
    runner
        .run(&steps(), |history| {
            for (c, n) in counts.iter().zip(run(&history)?) {
                c.fetch_add(n, Ordering::Relaxed);
            }
            Ok(())
        })
        .unwrap();
    // The set-aside of Q-1071 is reached by about three histories in 256, too few to assert here: the history
    // below pins it.
    let [seeded, searches, damaged, _] = counts.map(AtomicU64::into_inner);
    assert!(seeded > 0, "some reader continues a series directory");
    assert!(searches > 0, "some live value is found in the log");
    assert!(damaged > 0, "some history compares losses after damage");
}

#[test]
fn a_history_damaging_the_batch_just_above_a_directory_sets_aside_the_commit_time_before() {
    let emit = |t| Step::Emit(vec![(0, t, 1, 0, 0), (1, t, 2, 0, 0)], false);
    // The directory is through the last definition and the first emit's batch lies just above it; an even draw
    // aims there, where the cycle would take the third batch above.
    let history = [
        emit(-30),
        Step::Directory(6),
        emit(-4),
        emit(-2),
        Step::Damage(2, true),
    ];
    let [seeded, _, damaged, set_aside] = run(&history).unwrap();
    assert!(seeded > 0 && damaged > 0);
    assert!(
        set_aside > 0,
        "the continuing reader's loss just above its directory is reached"
    );
}

#[test]
fn a_reader_seeded_from_a_directory_knows_no_commit_time_before_a_loss_just_above_it() {
    let mut rig = Rig::new("seeded-loss");
    let id = rig.ids[2];
    let note = |rig: &Rig, secs: i64, text: &str| {
        Reading::new(
            rig.ids[2],
            Timestamp(rig.clock.wall().0 + secs * S),
            Value::Text(text.into()),
        )
    };
    rig.emit_readings(&[note(&rig, -30, "NOTE-AAA")], Intent::Normal);
    rig.long.live(id).unwrap();
    rig.shut();
    write_directory(&rig.env, &rig.dir, usize::MAX).expect("a batch");
    rig.reopen();
    rig.clock.advance(Duration::from_secs(5));
    rig.emit_readings(&[note(&rig, -4, "NOTE-BBB")], Intent::Normal);
    rig.emit(&[(0, -2, 0, 0, 0)], false);
    rig.shut();
    for p in rig.fs.paths() {
        if p.starts_with(rig.dir.join("log"))
            && let Some(bytes) = rig.fs.read(&p)
            && let Some(at) = bytes.windows(8).position(|w| w == b"NOTE-BBB")
        {
            rig.fs.poke(&p, at, b"X");
        }
    }
    let seeded = rig.reader();
    seeded.live(id).unwrap();
    assert!(seeded.durable().unwrap().unwrap().seeded());
    let whole = &rig.long;
    assert!(!whole.durable().unwrap().unwrap().seeded());
    let strip = |mut l: Vec<crate::health::LossReport>| {
        for r in &mut l {
            r.commit_time_before = None;
        }
        l
    };
    let (known, unknown) = (whole.losses(), seeded.losses());
    assert_eq!(known.len(), 1);
    assert!(!known[0].recorded);
    assert_eq!(known[0].commit_time_before, Some(Timestamp(T0)));
    assert_eq!(unknown[0].commit_time_before, None);
    assert_eq!(
        strip(unknown.clone()),
        strip(known.clone()),
        "the same loss otherwise"
    );
    let at = seed(&seeded);
    assert!(at.is_some());
    let (a, b) = known_alike((unknown.clone(), at), (known.clone(), None));
    assert_eq!(a, b);
    let recorded = |mut l: Losses| {
        for r in &mut l {
            r.recorded = true;
        }
        l
    };
    let (a, b) = known_alike(
        (recorded(unknown.clone()), at),
        (recorded(known.clone()), None),
    );
    assert_ne!(a, b, "a recorded report keeps the commit time compared");
    let below = at.map(|t| Position {
        sequence: t.sequence - 1,
        ..t
    });
    let (a, b) = known_alike((unknown.clone(), below), (known.clone(), None));
    assert_ne!(
        a, b,
        "a loss not just above the directory keeps the commit time compared"
    );
    let (a, b) = known_alike((unknown, None), (known, None));
    assert_ne!(
        a, b,
        "a reader that read the whole log keeps the commit time compared"
    );
}
