//! Selective fan-out (Q-1118) against the path it replaced and the model backend: random sessions with random
//! selections, each polled only when its bell rang, a `Latest` fell due or its last call gave a batch, receive exactly
//! what a session rung by every round receives polled every time, and at the end what the model's session holds; a
//! session not owed a look has no batch to give. A round may end inside a session's turn, between its view and its
//! advance.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use super::tests::{Held, TICK, World, apply};
use crate::config::{Backend, StoreConfig};
use crate::durable::read::fanout;
use crate::durable::reader::DurableReader;
use crate::durable::tests::block_on;
use crate::env::Env;
use crate::read::{LiveId, LiveOptions, Query, Selection};
use crate::time::Timestamp;
use crate::types::{Fact, Reading, SeriesId, Value};
use crate::{LiveSession, Select, Store};

const S: i64 = 1_000_000_000;
const NAMES: [&str; 6] = [
    "h/a/power",
    "h/b/power",
    "h/a/energy",
    "x/c/power",
    "x/d/temp",
    "h/e/temp",
];

#[derive(Clone, Debug)]
enum Step {
    /// Readings: (series drawn, seconds from now, value).
    Emit(Vec<(u8, i64, u8)>),
    /// The next series of `NAMES` defined, if any is left.
    Define,
    Advance(u64),
    /// Another read opened in subscriber `k`: (subscriber, kind, selection).
    Open(u8, u8, u8),
    Poll,
    /// A poll in which subscriber `k`'s fan-out session's turn has readings emitted and a round ended between its
    /// view and its advance.
    Race(u8, Vec<(u8, i64, u8)>),
}

/// A subscriber: (reads: 0 `Latest`, 1 `Tail`, 2 both; selection; a `Tail` with small limits).
type Spec = (u8, u8, bool);

fn histories() -> impl Strategy<Value = (Vec<Spec>, Vec<Step>)> {
    let spec = (0u8..3, any::<u8>(), prop::bool::weighted(0.3));
    let reading = (
        any::<u8>(),
        prop_oneof![Just(0i64), -30i64..30],
        any::<u8>(),
    );
    let step = prop_oneof![
        8 => prop::collection::vec(reading.clone(), 1..6).prop_map(Step::Emit),
        1 => Just(Step::Define),
        2 => prop_oneof![Just(0u64), 1u64..20, Just(400)].prop_map(Step::Advance),
        1 => (any::<u8>(), 0u8..2, any::<u8>()).prop_map(|(k, q, s)| Step::Open(k, q, s)),
        6 => Just(Step::Poll),
        2 => (any::<u8>(), prop::collection::vec(reading.clone(), 1..4))
            .prop_map(|(k, rs)| Step::Race(k, rs)),
    ];
    (
        prop::collection::vec(spec, 1..7),
        prop::collection::vec(step, 1..30),
    )
}

fn selection(k: u8) -> Selection {
    match k % 7 {
        0 => Selection::all(),
        1 => Select::Names("h/*/*".into()).into(),
        2 => Select::Names("*/*/power".into()).into(),
        3 => Select::Ids(vec![SeriesId(2)]).into(),
        4 => Select::Ids(vec![SeriesId(1), SeriesId(4), SeriesId(6)]).into(),
        5 => Select::Names("x/*/*".into()).into(),
        _ => Select::None.into(),
    }
}

fn query(kind: u8, sel: u8) -> Query {
    if kind == 0 {
        Query::Latest(selection(sel))
    } else {
        Query::Tail {
            sel: selection(sel),
        }
    }
}

fn small() -> LiveOptions {
    LiveOptions::default().max_rows(2).max_backlog(6, 1 << 20)
}

fn next_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

/// One subscriber's three sessions and what each consumer holds.
struct Sub {
    fan: LiveSession,
    every: LiveSession,
    model: LiveSession,
    small: bool,
    held: [BTreeMap<LiveId, Held>; 3],
    /// The fan-out session's bell before its last call, and whether that call gave a batch or a read was opened since.
    seen: Option<u64>,
    owed: bool,
}

impl Sub {
    fn open(&mut self, kind: u8, sel: u8) {
        let o = if kind == 1 && self.small {
            small()
        } else {
            LiveOptions::default()
        };
        let a = self.fan.open(query(kind, sel), o).unwrap();
        let b = self.every.open(query(kind, sel), o).unwrap();
        let c = self.model.open(query(kind, sel), o).unwrap();
        assert!(a == b && b == c);
        self.owed = true;
    }

    /// One call of each session's `next`, the fan-out session's last with `fan_last`. Returns whether any gave a
    /// batch.
    fn poll(&mut self, fan_last: bool) -> Result<bool, TestCaseError> {
        let others = fan_last.then(|| {
            (
                self.every.next(std::time::Duration::ZERO).unwrap(),
                self.model.next(std::time::Duration::ZERO).unwrap(),
            )
        });
        let owed = {
            let s = self.fan.durable_mut();
            let rung = s.as_ref().and_then(|s| s.rung());
            self.owed || rung.is_none() || rung != self.seen || s.is_some_and(|s| s.due_now())
        };
        self.seen = self.fan.durable_mut().and_then(|s| s.rung());
        let mut any = false;
        let fan = self.fan.next(std::time::Duration::ZERO).unwrap();
        prop_assert!(
            fan.is_none() || owed,
            "a session whose bell did not ring had a batch: {:?}",
            fan
        );
        self.owed = fan.is_some();
        let (every, model) = match others {
            Some(o) => o,
            None => (
                self.every.next(std::time::Duration::ZERO).unwrap(),
                self.model.next(std::time::Duration::ZERO).unwrap(),
            ),
        };
        for (k, b) in [fan, every, model].into_iter().enumerate() {
            if let Some(b) = b {
                apply(&mut self.held[k], &b);
                any = true;
            }
        }
        Ok(any)
    }
}

struct Rig {
    w: World,
    store: Store,
    model: Store,
    reader: std::sync::Arc<DurableReader>,
    ids: Vec<SeriesId>,
    subs: Vec<Sub>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.subs.clear();
        let _ = block_on(self.store.shutdown());
    }
}

impl Rig {
    fn new(specs: &[Spec]) -> Self {
        let w = World::new(&format!("fanout-{}", next_id()));
        let store = w.open();
        let mdir = PathBuf::from(format!("live-model/fanout-{}", next_id()));
        let model = block_on(Store::open(
            StoreConfig::new(&mdir, "Etc/UTC")
                .create_if_missing(true)
                .backend(Backend::Model)
                .env(Env::test(w.clock.clone())),
        ))
        .unwrap();
        let own = store.reader();
        let reader = own.durable().unwrap().unwrap();
        reader.fanout.store(true, Ordering::SeqCst);
        let mut rig = Rig {
            reader,
            ids: Vec::new(),
            subs: Vec::new(),
            w,
            store,
            model,
        };
        for _ in 0..3 {
            rig.define();
        }
        for &(kind, sel, small) in specs {
            let mut every = own.session();
            every.every_round();
            let mut s = Sub {
                fan: own.session(),
                every,
                model: rig.model.reader().session(),
                small,
                held: Default::default(),
                seen: None,
                owed: true,
            };
            if kind != 1 {
                s.open(0, sel);
            }
            if kind != 0 {
                s.open(1, sel.wrapping_add(kind));
            }
            rig.subs.push(s);
        }
        rig
    }

    fn define(&mut self) {
        let Some(n) = NAMES.get(self.ids.len()) else {
            return;
        };
        let spec = super::tests::gauge(n);
        let d = block_on(self.store.define_series(spec.clone())).unwrap();
        assert_eq!(block_on(self.model.define_series(spec)).unwrap(), d);
        self.ids.push(d);
    }

    fn readings(&self, rs: &[(u8, i64, u8)]) -> Vec<Reading> {
        let now = self.w.clock.wall().0;
        rs.iter()
            .map(|&(i, secs, v)| {
                let id = self.ids[usize::from(i) % self.ids.len()];
                Reading::new(id, Timestamp(now + secs * S), Value::F64(f64::from(v % 9)))
            })
            .collect()
    }

    fn step(&mut self, s: &Step) -> Result<(), TestCaseError> {
        match s {
            Step::Emit(rs) => {
                let readings = self.readings(rs);
                let d = block_on(self.store.emit(&readings));
                let m = block_on(self.model.emit(&readings));
                prop_assert_eq!(d.is_ok(), m.is_ok(), "{:?} {:?}", d, m);
            }
            Step::Race(k, rs) => {
                let readings = self.readings(rs);
                let (store, model, reader) =
                    (self.store.clone(), self.model.clone(), self.reader.clone());
                let n = self.subs.len();
                let fan = self.subs[usize::from(*k) % n].fan.durable_mut();
                fan.expect("a durable session").before_advance.0 = Some(Box::new(move || {
                    let d = block_on(store.emit(&readings));
                    let m = block_on(model.emit(&readings));
                    assert_eq!(d.is_ok(), m.is_ok(), "{d:?} {m:?}");
                    fanout::pass(&reader);
                }));
                self.poll_racing(Some(usize::from(*k) % n))?;
                // A turn that computed no batch leaves the hook unfired; nothing was emitted then.
                if let Some(fan) = self.subs[usize::from(*k) % n].fan.durable_mut() {
                    fan.before_advance.0 = None;
                }
            }
            Step::Define => self.define(),
            Step::Advance(secs) => self.w.clock.advance(std::time::Duration::from_secs(*secs)),
            Step::Open(k, kind, sel) => {
                let n = self.subs.len();
                self.subs[usize::from(*k) % n].open(*kind, *sel);
            }
            Step::Poll => {
                self.poll()?;
            }
        }
        Ok(())
    }

    /// A tick of the clock, the fan-out thread's pass, then one call of every session; the fan-out session holds what
    /// the session rung by every round holds.
    fn poll(&mut self) -> Result<bool, TestCaseError> {
        self.poll_racing(None)
    }

    /// As [`Rig::poll`], subscriber `racing`'s fan-out session called after its other two.
    fn poll_racing(&mut self, racing: Option<usize>) -> Result<bool, TestCaseError> {
        self.w.clock.advance(TICK);
        fanout::pass(&self.reader);
        let mut any = false;
        for (k, s) in self.subs.iter_mut().enumerate() {
            any |= s.poll(racing == Some(k))?;
            if s.small {
                let same_cut = !stopped(&mut s.fan) && !stopped(&mut s.every);
                passed_over(&s.held[0], &s.held[1], same_cut)?;
            } else {
                prop_assert_eq!(&s.held[0], &s.held[1]);
            }
        }
        Ok(any)
    }

    /// Polled until no session has anything more: the fan-out session then holds what the model's holds.
    fn settle(&mut self) -> Result<(), TestCaseError> {
        for _ in 0..64 {
            if !self.poll()? {
                break;
            }
        }
        for s in &mut self.subs {
            if s.small {
                prop_assert!(!stopped(&mut s.fan) && !stopped(&mut s.every));
                passed_over(&s.held[0], &s.held[1], true)?;
            } else {
                prop_assert_eq!(plain(&s.held[0]), plain(&s.held[2]));
            }
        }
        Ok(())
    }
}

/// Whether the session's last batch stopped at a row limit inside its rounds (Q-1014): its `Latest`s then stand at that
/// cut.
fn stopped(s: &mut LiveSession) -> bool {
    s.durable_mut().is_some_and(|d| d.rest)
}

/// A small backlog counted every row before Q-1118, so the old path also lagged over rounds holding nothing the
/// session selects: the fan-out session holds facts the old one lacks only inside a range the old one was told it
/// lagged over. A session that does not lag may stop at a row limit where the old one lagged past, and lag later over
/// facts the old one then delivers, so once neither has stopped short of the view the fan-out session holds every fact
/// the old one does or was told it lagged over it, and its `Latest`s are the same.
fn passed_over(
    fan: &BTreeMap<LiveId, Held>,
    old: &BTreeMap<LiveId, Held>,
    same_cut: bool,
) -> Result<(), TestCaseError> {
    prop_assert_eq!(
        fan.keys().collect::<Vec<_>>(),
        old.keys().collect::<Vec<_>>()
    );
    for (id, f) in fan {
        let o = &old[id];
        let within = |h: &Held, x: &Fact| {
            h.lagged
                .iter()
                .any(|(a, b)| *a <= x.position && x.position <= *b)
        };
        if same_cut {
            prop_assert_eq!(&f.latest, &o.latest);
            for x in o.tail.iter().filter(|x| !f.tail.contains(x)) {
                prop_assert!(within(f, x), "{:?} {:?}", f, o);
            }
        }
        for x in f.tail.iter().filter(|x| !o.tail.contains(x)) {
            prop_assert!(within(o, x), "{:?} {:?}", f, o);
        }
        prop_assert!(f.lagged.len() <= o.lagged.len(), "{:?} {:?}", f, o);
    }
    Ok(())
}

/// What both backends agree on: facts without positions, staleness set aside (Q-16).
fn plain(held: &BTreeMap<LiveId, Held>) -> Vec<(LiveId, String)> {
    held.iter()
        .map(|(id, h)| {
            let latest = h.latest.as_ref().map(|v| {
                v.iter()
                    .map(|(k, l)| match l {
                        crate::read::Live::Value(v) => (*k, Some((v.fact.t, v.fact.value.clone()))),
                        _ => (*k, None),
                    })
                    .collect::<Vec<_>>()
            });
            let tail: Vec<_> = h
                .tail
                .iter()
                .map(|f| (f.series, f.t, f.value.clone()))
                .collect();
            (*id, format!("{latest:?} {tail:?}"))
        })
        .collect()
}

fn run((specs, steps): (Vec<Spec>, Vec<Step>)) -> Result<(), TestCaseError> {
    let mut rig = Rig::new(&specs);
    rig.poll()?;
    for s in &steps {
        rig.step(s)?;
    }
    rig.settle()
}

#[test]
fn sessions_polled_only_when_rung_receive_what_sessions_rung_by_every_round_and_the_model_receive()
{
    let cases = (ProptestConfig::default().cases / 4).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner.run(&histories(), run).unwrap();
}

#[test]
fn a_small_tail_selecting_nothing_passes_over_other_series_rows_where_the_old_path_lagged() {
    let r = run((
        vec![(1, 158, true), (0, 0, false)],
        vec![
            Step::Emit(vec![(0, 0, 0), (0, 0, 1), (0, 0, 2), (0, 0, 3)]),
            Step::Emit(vec![(0, -1, 0), (0, 0, 13), (0, 1, 0)]),
        ],
    ));
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn a_round_ending_inside_the_turn_that_opens_a_tail_is_delivered_to_it() {
    let r = run((
        vec![(1, 3, false)],
        vec![
            Step::Emit(vec![(1, 0, 1)]),
            Step::Poll,
            Step::Poll,
            Step::Open(0, 1, 3),
            Step::Race(0, vec![(1, 0, 5)]),
        ],
    ));
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn a_small_session_raced_after_a_late_open_holds_the_old_paths_latest() {
    let r = run((
        vec![
            (0, 0, false),
            (0, 81, true),
            (2, 14, false),
            (0, 0, false),
            (0, 0, false),
            (0, 0, false),
        ],
        vec![
            Step::Emit(vec![(0, 0, 0)]),
            Step::Advance(0),
            Step::Open(253, 1, 46),
            Step::Race(2, vec![(1, 0, 0)]),
            Step::Emit(vec![(0, 0, 16), (0, 0, 14), (20, 0, 0)]),
            Step::Emit(vec![(141, 0, 8), (0, 0, 0), (0, 0, 28)]),
        ],
    ));
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn a_latest_opened_while_a_small_tail_stops_at_its_row_limit_holds_the_old_paths_latest_once_caught_up()
 {
    let r = run((
        vec![(1, 8, true), (0, 0, false)],
        vec![
            Step::Emit(vec![(0, 0, 0)]),
            Step::Race(7, vec![(80, 0, 11), (29, 0, 3)]),
            Step::Emit(vec![(0, 0, 0), (0, 0, 1), (0, 0, 2), (31, 0, 0), (0, 1, 0)]),
            Step::Open(14, 0, 35),
            Step::Define,
        ],
    ));
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn a_small_tail_that_stops_where_the_old_path_lagged_may_lag_later_over_a_fact_the_old_path_delivers()
 {
    let r = run((
        vec![(2, 20, true), (0, 0, false), (0, 0, false), (0, 0, false)],
        vec![
            Step::Define,
            Step::Emit(vec![(0, 0, 0)]),
            Step::Race(1, vec![(179, 0, 0)]),
            Step::Emit(vec![(0, 0, 0), (0, 0, 1), (0, 0, 38)]),
            Step::Emit(vec![(0, 0, 3), (0, 0, 4), (0, 0, 59)]),
            Step::Poll,
            Step::Emit(vec![(97, 0, 253)]),
            Step::Poll,
        ],
    ));
    assert!(r.is_ok(), "{r:?}");
}
