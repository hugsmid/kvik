use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use super::tests::{Held, T0, TICK, World, apply};
use crate::config::{Backend, StoreConfig};
use crate::durable::tests::block_on;
use crate::env::Env;
use crate::read::{Live, LiveId, LiveOptions, Query, Selection};
use crate::series::{Kind, SeriesSpec, Timing, ValueType};
use crate::time::Timestamp;
use crate::types::{Fact, Position, Quality, Reading, SeriesId, Value};
use crate::{LiveSession, Reader, Select, Store};

const S: i64 = 1_000_000_000;

#[derive(Clone, Debug)]
enum Step {
    /// Readings (series drawn, seconds from now, value code, quality code).
    Emit(Vec<(u8, i64, u8, u8)>),
    /// A series defined under name `k` of a small set, some of which the selections match.
    Define(u8),
    Alias(u8, u8),
    Retire(u8),
    Advance(u64),
    /// Another `Latest` opened in every session.
    Open(u8),
    Restart,
    Poll,
}

fn steps() -> impl Strategy<Value = (u8, u8, Vec<Step>)> {
    let reading = (
        any::<u8>(),
        prop_oneof![3 => prop::sample::select(vec![-60i64, -5, 0, 5, 40]), 1 => -300i64..120],
        any::<u8>(),
        any::<u8>(),
    );
    let step = prop_oneof![
        8 => prop::collection::vec(reading, 1..5).prop_map(Step::Emit),
        2 => any::<u8>().prop_map(Step::Define),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Step::Alias(a, b)),
        1 => any::<u8>().prop_map(Step::Retire),
        3 => prop_oneof![Just(0u64), 1u64..40, Just(120)].prop_map(Step::Advance),
        1 => any::<u8>().prop_map(Step::Open),
        1 => Just(Step::Restart),
        6 => Just(Step::Poll),
    ];
    (any::<u8>(), any::<u8>(), prop::collection::vec(step, 1..24))
}

fn selection(k: u8) -> Selection {
    match k % 6 {
        0 => Selection::all(),
        1 => Select::Names("h/*/*".into()).into(),
        2 => Select::Names("*/*/power".into()).into(),
        3 => Select::Ids(vec![SeriesId(3), SeriesId(1), SeriesId(5)]).into(),
        4 => Selection::new([
            Select::Names("x/*/*".into()),
            Select::Ids(vec![SeriesId(2)]),
        ]),
        _ => Select::None.into(),
    }
}

fn name(k: u8) -> String {
    let (first, last) = [
        ("h", "power"),
        ("x", "power"),
        ("h", "price"),
        ("x", "note"),
    ][usize::from(k % 4)];
    format!("{first}/d{}/{last}", k / 4 % 3)
}

fn spec(name: &str, k: u8) -> SeriesSpec {
    let f64 = Kind::Gauge {
        value: ValueType::F64,
    };
    match k % 3 {
        0 => SeriesSpec::new(name, f64, "W").staleness(Duration::from_secs(30)),
        1 => SeriesSpec::new(name, f64, "c").timing(Timing::Scheduled),
        _ => SeriesSpec::new(name, f64, "W"),
    }
}

fn quality(code: u8) -> Quality {
    [Quality::Good, Quality::Good, Quality::Suspect, Quality::Bad][usize::from(code % 4)]
}

/// Events the durable sessions sent, by kind: joined, left, updated, appended.
static SEEN: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn next() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

/// A session with what its consumer holds.
struct Watched {
    session: LiveSession,
    held: BTreeMap<LiveId, Held>,
    /// The cut of the last batch and the cut each `Tail` opened at.
    cut: Position,
    opened: BTreeMap<LiveId, Position>,
}

impl Watched {
    fn new(reader: &Reader, (l, t): (u8, u8)) -> Self {
        let mut session = reader.session();
        session
            .open(Query::Latest(selection(l)), LiveOptions::default())
            .unwrap();
        session
            .open(Query::Tail { sel: selection(t) }, LiveOptions::default())
            .unwrap();
        Watched {
            session,
            held: BTreeMap::new(),
            cut: Position::ORIGIN,
            opened: BTreeMap::new(),
        }
    }

    fn poll(&mut self) -> bool {
        let Some(b) = self.session.next(Duration::ZERO).unwrap() else {
            return false;
        };
        assert!(
            b.durable >= self.cut
                || b.events
                    .iter()
                    .any(|(_, e)| matches!(e, crate::LiveEvent::Reset { .. }))
        );
        for (id, e) in &b.events {
            if matches!(e, crate::LiveEvent::Opened(crate::Answer::Tail)) {
                self.opened.insert(*id, b.durable);
            }
        }
        for (_, e) in &b.events {
            let k = match e {
                crate::LiveEvent::Joined { .. } => 0,
                crate::LiveEvent::Left { .. } => 1,
                crate::LiveEvent::Updated(_) => 2,
                crate::LiveEvent::Appended(_) => 3,
                _ => continue,
            };
            SEEN[k].fetch_add(1, Ordering::Relaxed);
        }
        apply(&mut self.held, &b);
        self.cut = b.durable;
        true
    }
}

struct Rig {
    w: World,
    store: Option<Store>,
    model: Store,
    ids: Vec<SeriesId>,
    sels: (u8, u8),
    /// A reader process's session, kept across restarts, against a model session opened with it.
    far: (Reader, Watched, Watched),
    /// The writer's own session, opened again at each start, against a model session opened with it.
    own: Option<(Watched, Watched)>,
}

impl Rig {
    fn new(sels: (u8, u8)) -> Self {
        let w = World::new(&format!("differential-{}", next()));
        let store = w.open();
        let mdir = PathBuf::from(format!("live-model/{}", next()));
        let model = block_on(Store::open(
            StoreConfig::new(&mdir, "Europe/London")
                .create_if_missing(true)
                .backend(Backend::Model)
                .env(Env::test(w.clock.clone())),
        ))
        .unwrap();
        let mut rig = Rig {
            far: {
                let r = w.reader();
                let m = model.reader();
                let a = Watched::new(&r, sels);
                (r, a, Watched::new(&m, sels))
            },
            w,
            store: Some(store),
            model,
            ids: Vec::new(),
            sels,
            own: None,
        };
        for k in 0..3 {
            rig.define(k);
        }
        rig.own = Some(rig.own_pair());
        rig
    }

    fn own_pair(&self) -> (Watched, Watched) {
        let s = self.store.as_ref().expect("open");
        (
            Watched::new(&s.reader(), self.sels),
            Watched::new(&self.model.reader(), self.sels),
        )
    }

    fn store(&self) -> &Store {
        self.store.as_ref().expect("an open store")
    }

    fn define(&mut self, k: u8) {
        let n = name(k);
        let d = block_on(self.store().define_series(spec(&n, k)));
        let m = block_on(self.model.define_series(spec(&n, k)));
        assert_eq!(d.is_ok(), m.is_ok(), "{d:?} {m:?}");
        if let (Ok(d), Ok(m)) = (d, m) {
            assert_eq!(d, m);
            self.ids.push(d);
        }
    }

    fn step(&mut self, s: &Step) -> Result<(), TestCaseError> {
        let pick = |ids: &[SeriesId], k: u8| ids[usize::from(k) % ids.len()];
        match s {
            Step::Emit(rs) => {
                let now = self.w.clock.wall().0;
                let readings: Vec<Reading> = rs
                    .iter()
                    .map(|&(i, secs, v, q)| {
                        let value = if v % 5 == 0 {
                            Value::Absent
                        } else {
                            Value::F64(f64::from(v % 7))
                        };
                        Reading::new(pick(&self.ids, i), Timestamp(now + secs * S), value)
                            .with_quality(quality(q))
                    })
                    .collect();
                let d = block_on(self.store().emit(&readings)).map(|a| a.outcomes.len());
                let m = block_on(self.model.emit(&readings)).map(|a| a.outcomes.len());
                prop_assert_eq!(d.is_ok(), m.is_ok(), "{:?} {:?}", d, m);
            }
            Step::Define(k) => self.define(*k),
            Step::Alias(i, k) => {
                let id = pick(&self.ids, *i);
                let n = format!("{}a", name(*k));
                let d = block_on(self.store().alias(id, &n));
                let m = block_on(self.model.alias(id, &n));
                prop_assert_eq!(d.is_ok(), m.is_ok(), "{:?} {:?}", d, m);
            }
            Step::Retire(i) => {
                let id = pick(&self.ids, *i);
                let at = self.w.clock.wall();
                let d = block_on(self.store().retire(id, at));
                let m = block_on(self.model.retire(id, at));
                prop_assert_eq!(d.is_ok(), m.is_ok(), "{:?} {:?}", d, m);
            }
            Step::Advance(secs) => self.w.clock.advance(Duration::from_secs(*secs)),
            Step::Open(k) => {
                let (_, far, model) = &mut self.far;
                for s in [far, model] {
                    s.session
                        .open(Query::Latest(selection(*k)), LiveOptions::default())
                        .unwrap();
                }
                if let Some((own, model)) = &mut self.own {
                    for s in [own, model] {
                        s.session
                            .open(Query::Latest(selection(*k)), LiveOptions::default())
                            .unwrap();
                    }
                }
            }
            Step::Restart => {
                self.own = None;
                if let Some(s) = self.store.take() {
                    block_on(s.shutdown()).unwrap();
                }
                self.store = Some(self.w.open());
                self.own = Some(self.own_pair());
            }
            Step::Poll => self.poll()?,
        }
        Ok(())
    }

    /// Every session polled after a tick; each durable consumer then holds the static read at its cut, and the
    /// same as its model session with positions and staleness set aside.
    fn poll(&mut self) -> Result<(), TestCaseError> {
        self.w.clock.advance(TICK);
        let store = self.store.clone().expect("open");
        let (far_reader, far, far_model) = &mut self.far;
        let (own, own_model) = self.own.as_mut().expect("opened");
        for (reader, durable, model) in [
            (far_reader.clone(), far, far_model),
            (store.reader(), own, own_model),
        ] {
            let moved = durable.poll();
            model.poll();
            static_at_cut(&reader, durable, moved)?;
            prop_assert_eq!(plain(&durable.held), plain(&model.held));
            for (id, at) in &durable.opened {
                for f in &durable.held[id].tail {
                    prop_assert!(f.position > *at && f.position <= durable.cut);
                }
            }
        }
        Ok(())
    }
}

/// Each `Latest` the consumer holds equals the static read at the session's cut and `now`, staleness included: the
/// cut of the batch just taken, or the newest when no batch came, since nothing then changed.
fn static_at_cut(reader: &Reader, s: &Watched, moved: bool) -> Result<(), TestCaseError> {
    let snap = reader.snapshot().unwrap();
    if moved {
        prop_assert_eq!(snap.state().durable, s.cut);
    }
    for h in s.held.values() {
        let Some(shown) = &h.latest else { continue };
        let ids: Vec<SeriesId> = shown.iter().map(|(id, _)| *id).collect();
        let want: Vec<(SeriesId, Live)> = ids
            .iter()
            .map(|id| (*id, snap.live(*id).unwrap()))
            .collect();
        prop_assert_eq!(shown, &want);
    }
    Ok(())
}

/// What consumers hold as both backends agree on it: positions and staleness set aside (Q-16).
fn plain(held: &BTreeMap<LiveId, Held>) -> Vec<(LiveId, Option<Vec<(SeriesId, Live)>>, Vec<Fact>)> {
    let fact = |mut f: Fact| {
        f.position = Position::ORIGIN;
        f
    };
    held.iter()
        .map(|(id, h)| {
            let latest = h.latest.as_ref().map(|v| {
                v.iter()
                    .map(|(k, l)| {
                        let l = match l.clone() {
                            Live::Value(mut v) => {
                                v.fact = fact(v.fact);
                                v.stale = false;
                                Live::Value(v)
                            }
                            other => other,
                        };
                        (*k, l)
                    })
                    .collect()
            });
            (*id, latest, h.tail.iter().cloned().map(fact).collect())
        })
        .collect()
}

fn run((l, t, history): (u8, u8, Vec<Step>)) -> Result<(), TestCaseError> {
    let mut rig = Rig::new((l, t));
    rig.poll()?;
    for s in &history {
        rig.step(s)?;
    }
    rig.poll()?;
    rig.own = None;
    if let Some(s) = rig.store.take() {
        block_on(s.shutdown()).unwrap();
    }
    let _ = T0;
    Ok(())
}

#[test]
fn every_batch_brings_each_consumer_to_the_static_read_at_its_cut_and_to_the_models() {
    // Each case opens two stores, two readers and four sessions: a quarter of the configured cases.
    let cases = (ProptestConfig::default().cases / 4).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner.run(&steps(), run).unwrap();
    for (k, what) in SEEN.iter().zip(["joined", "left", "updated", "appended"]) {
        assert!(
            k.load(Ordering::Relaxed) > 0,
            "no {what} event was generated"
        );
    }
}
