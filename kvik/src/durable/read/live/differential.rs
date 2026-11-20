use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use super::tests::{Held, TICK, World, apply, losses_named, tail_is_owed_but};
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
    /// A live read cancelled in every session, by its place among those open.
    Cancel(u8),
    /// A note with a text of its own.
    Note,
    /// The bytes of a note but the newest damaged; an even code also has a static read meet them at once.
    Damage(u8),
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
        1 => any::<u8>().prop_map(Step::Cancel),
        2 => Just(Step::Note),
        1 => any::<u8>().prop_map(Step::Damage),
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

/// Events the durable sessions sent, by kind: joined, left, updated, appended, gap, lagged.
static SEEN: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

fn next() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

/// A session with what its consumer holds.
struct Watched {
    session: LiveSession,
    /// Its live reads still open, in the order opened.
    ids: Vec<LiveId>,
    held: BTreeMap<LiveId, Held>,
    /// The cut of the last batch and the cut each `Tail` opened at.
    cut: Position,
    opened: BTreeMap<LiveId, Position>,
}

impl Watched {
    fn new(reader: &Reader, sels: (u8, u8)) -> Self {
        Self::with(reader, sels, LiveOptions::default())
    }

    fn with(reader: &Reader, (l, t): (u8, u8), tail: LiveOptions) -> Self {
        let mut session = reader.session();
        let ids = vec![
            session
                .open(Query::Latest(selection(l)), LiveOptions::default())
                .unwrap(),
            session
                .open(Query::Tail { sel: selection(t) }, tail)
                .unwrap(),
        ];
        Watched {
            session,
            ids,
            held: BTreeMap::new(),
            cut: Position::ORIGIN,
            opened: BTreeMap::new(),
        }
    }

    fn open(&mut self, k: u8) {
        let id = self
            .session
            .open(Query::Latest(selection(k)), LiveOptions::default())
            .unwrap();
        self.ids.push(id);
    }

    fn cancel(&mut self, k: u8) {
        if self.ids.is_empty() {
            return;
        }
        let id = self.ids.remove(usize::from(k) % self.ids.len());
        assert!(self.session.cancel(id));
        self.held.remove(&id);
        self.opened.remove(&id);
    }

    fn poll(&mut self) -> Result<bool, crate::QueryError> {
        let Some(b) = self.session.next(Duration::ZERO)? else {
            return Ok(false);
        };
        assert!(
            b.durable >= self.cut
                || b.events
                    .iter()
                    .any(|(_, e)| matches!(e, crate::LiveEvent::Reset { .. }))
        );
        for (id, e) in &b.events {
            // The `Opened` after a `Reset` keeps the opening cut.
            if matches!(e, crate::LiveEvent::Opened(crate::Answer::Tail)) {
                self.opened.entry(*id).or_insert(b.durable);
            }
        }
        for (_, e) in &b.events {
            let k = match e {
                crate::LiveEvent::Joined { .. } => 0,
                crate::LiveEvent::Left { .. } => 1,
                crate::LiveEvent::Updated(_) => 2,
                crate::LiveEvent::Appended(_) => 3,
                crate::LiveEvent::Gap { .. } => 4,
                crate::LiveEvent::Lagged { .. } => 5,
                _ => continue,
            };
            SEEN[k].fetch_add(1, Ordering::Relaxed);
        }
        apply(&mut self.held, &b);
        self.cut = b.durable;
        Ok(true)
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
    /// A second session on the far reader whose `Tail` of every series has small row and backlog limits: checked
    /// against the history and the losses reported, and its `Latest` against a reader opened afterwards.
    lim: Watched,
    note: SeriesId,
    /// Each note's text and position.
    notes: Vec<(String, Position)>,
    /// The newest position written and the far reader's position at the last poll.
    newest: Position,
    read: Position,
    /// Each series retired, with the newest position when it was.
    retired: Vec<(SeriesId, Position)>,
    /// The far reader built its fact index, which reads a batch once: damage it read before is then never met.
    indexed: bool,
    /// Bytes were damaged: the model, which has no faults, is no longer the oracle.
    faulted: bool,
    /// A session failed as the static read does: nothing more is compared.
    ended: bool,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.own = None;
        if let Some(s) = self.store.take() {
            let _ = block_on(s.shutdown());
        }
    }
}

fn small() -> LiveOptions {
    LiveOptions::default().max_rows(3).max_backlog(8, 1 << 20)
}

impl Rig {
    fn new(sels: (u8, u8)) -> Self {
        let w = World::new(&format!("differential-{}", next()));
        let store = w.open();
        let mdir = PathBuf::from(format!("live-model/{}", next()));
        let model = block_on(Store::open(
            StoreConfig::new(&mdir, "Etc/UTC")
                .create_if_missing(true)
                .backend(Backend::Model)
                .env(Env::test(w.clock.clone())),
        ))
        .unwrap();
        let r = w.reader();
        let lim = Watched::with(&r, (sels.0, 0), small());
        let mut rig = Rig {
            far: {
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
            lim,
            note: SeriesId(0),
            notes: Vec::new(),
            newest: Position::ORIGIN,
            read: Position::ORIGIN,
            retired: Vec::new(),
            indexed: false,
            faulted: false,
            ended: false,
        };
        for k in 0..3 {
            rig.define(k);
        }
        let spec = SeriesSpec::new("n/d0/note", Kind::Text, "");
        rig.note = block_on(rig.store().define_series(spec.clone())).unwrap();
        assert_eq!(block_on(rig.model.define_series(spec)).unwrap(), rig.note);
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
                let d = block_on(self.store().emit(&readings)).map(|a| {
                    self.newest = self.newest.max(a.last().unwrap_or(Position::ORIGIN));
                    a.outcomes.len()
                });
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
                if d.is_ok() {
                    let p = self.store().reader().snapshot().unwrap().state().durable;
                    self.retired.push((id, p));
                }
            }
            Step::Advance(secs) => self.w.clock.advance(Duration::from_secs(*secs)),
            Step::Open(k) => {
                let (_, far, model) = &mut self.far;
                far.open(*k);
                model.open(*k);
                self.lim.open(*k);
                if let Some((own, model)) = &mut self.own {
                    own.open(*k);
                    model.open(*k);
                }
            }
            Step::Cancel(k) => {
                let (_, far, model) = &mut self.far;
                far.cancel(*k);
                model.cancel(*k);
                self.lim.cancel(*k);
                if let Some((own, model)) = &mut self.own {
                    own.cancel(*k);
                    model.cancel(*k);
                }
            }
            Step::Note => {
                let text = format!("DN-{:03}", self.notes.len());
                let r = [Reading::new(
                    self.note,
                    self.w.clock.wall(),
                    Value::Text(text.clone()),
                )];
                let d = block_on(self.store().emit(&r));
                let m = block_on(self.model.emit(&r));
                prop_assert_eq!(d.is_ok(), m.is_ok(), "{:?} {:?}", d, m);
                if let Some(p) = d.ok().and_then(|a| a.last()) {
                    self.newest = self.newest.max(p);
                    self.notes.push((text, p));
                }
            }
            Step::Damage(k) => {
                // Never the newest batch, which a start may truncate; once the far reader indexed, only what it has
                // not read, so that it meets every damage as a fresh reader does.
                let open: Vec<&(String, Position)> = self
                    .notes
                    .iter()
                    .filter(|(_, p)| *p < self.newest && (!self.indexed || *p > self.read))
                    .collect();
                if open.is_empty() {
                    return Ok(());
                }
                let (text, p) = open[usize::from(*k) % open.len()].clone();
                if self.w.damage_any(text.as_bytes()) {
                    self.faulted = true;
                    if p <= self.read {
                        let all = Timestamp(i64::MIN)..Timestamp(i64::MAX);
                        let read = self.far.0.facts(self.note, all, crate::read::View::History);
                        drop(read.map(Iterator::count));
                        self.indexed = true;
                        // The call's refresh read on to the mark.
                        self.read = self.far.0.snapshot().unwrap().state().durable;
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

    /// Every session polled after a tick, the far and the writer's on threads of their own; each durable consumer
    /// then holds the static read at its cut and, until a fault, the same as its model session with positions and
    /// staleness set aside; after one, the losses reported named to it and none of its facts inside one, and its
    /// `Latest` that of a reader opened afterwards. The limited session is then brought to the mark and checked.
    fn poll(&mut self) -> Result<(), TestCaseError> {
        self.w.clock.advance(TICK);
        let store = self.store.clone().expect("open");
        // What the writer's process knows before its session's poll: the session must name it.
        let own_known = store.losses();
        let (far_reader, far, far_model) = &mut self.far;
        let (own, own_model) = self.own.as_mut().expect("opened");
        let (far_moved, own_moved) = std::thread::scope(|scope| {
            let a = scope.spawn(|| far.poll());
            let b = scope.spawn(|| own.poll());
            (a.join().unwrap(), b.join().unwrap())
        });
        // A segment a `Latest` must search that damage made unreadable fails the session's batch as it fails the
        // static read; the case ends there.
        let (far_moved, own_moved) = match (far_moved, own_moved) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                for (r, reader, s) in [(a, far_reader.clone(), &*far), (b, store.reader(), &*own)] {
                    if let Err(e) = r {
                        prop_assert!(
                            matches!(e, crate::QueryError::NotAvailable { .. }),
                            "{:?}",
                            e
                        );
                        prop_assert!(fails_statically(&reader, s), "{:?}", e);
                    }
                }
                self.ended = true;
                return Ok(());
            }
        };
        // The far reader meets every damage a fresh reader does.
        let far_known: Vec<_> = store
            .losses()
            .into_iter()
            .chain(self.w.reader().losses())
            .collect();
        for (reader, durable, model, moved, known) in [
            (far_reader.clone(), &*far, far_model, far_moved, &far_known),
            (store.reader(), &*own, own_model, own_moved, &own_known),
        ] {
            model.poll().unwrap();
            static_at_cut(&reader, durable, moved)?;
            if !self.faulted {
                prop_assert_eq!(plain(&durable.held), plain(&model.held));
            }
            for (id, at) in &durable.opened {
                for f in &durable.held[id].tail {
                    prop_assert!(f.position > *at && f.position <= durable.cut);
                }
                if self.faulted {
                    losses_named(known, *at, durable.cut, &durable.held[id]);
                }
            }
        }
        if self.faulted && far_moved {
            fresh_at_cut(&self.w, far)?;
        }
        loop {
            match self.lim.poll() {
                Ok(true) => self.w.clock.advance(TICK),
                Ok(false) => break,
                Err(e) => {
                    prop_assert!(
                        matches!(e, crate::QueryError::NotAvailable { .. }),
                        "{:?}",
                        e
                    );
                    prop_assert!(fails_statically(far_reader, &self.lim), "{:?}", e);
                    self.ended = true;
                    return Ok(());
                }
            }
        }
        static_at_cut(far_reader, &self.lim, false)?;
        fresh_at_cut(&self.w, &self.lim)?;
        for (id, at) in &self.lim.opened {
            let h = &self.lim.held[id];
            if h.latest.is_none() {
                // A retired series leaves every selection (Q-1012).
                let retired = |f: &Fact| {
                    self.retired
                        .iter()
                        .any(|(id, p)| f.series == *id && f.position > *p)
                };
                tail_is_owed_but(&self.w, &store, (*at, self.lim.cut), h, &retired);
            }
        }
        self.read = far_reader.snapshot().unwrap().state().durable;
        Ok(())
    }
}

/// Whether the static read of some series a `Latest` of the session would judge fails.
fn fails_statically(reader: &Reader, s: &Watched) -> bool {
    let Ok(snap) = reader.snapshot() else {
        return true;
    };
    let mut ids: Vec<SeriesId> = snap.select(&Selection::all()).unwrap_or_default();
    ids.extend(
        s.held
            .values()
            .flat_map(|h| h.latest.iter().flatten().map(|(id, _)| *id)),
    );
    ids.iter().any(|id| snap.live(*id).is_err())
}

/// Each `Latest` the consumer holds equals that of a reader opened now, when it reads at the session's cut.
fn fresh_at_cut(w: &World, s: &Watched) -> Result<(), TestCaseError> {
    let snap = w.reader().snapshot().unwrap();
    if snap.state().durable != s.cut {
        return Ok(());
    }
    for h in s.held.values() {
        let Some(shown) = &h.latest else { continue };
        for (id, live) in shown {
            if let Ok(want) = snap.live(*id) {
                prop_assert_eq!(live, &want, "{:?}", id);
            }
        }
    }
    Ok(())
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
        // A series whose segment damage made unreadable, which the session does not search again (Q-1016).
        let want: Vec<(SeriesId, Live)> = shown
            .iter()
            .map(|(id, live)| (*id, snap.live(*id).unwrap_or_else(|_| live.clone())))
            .collect();
        prop_assert_eq!(shown, &want);
    }
    Ok(())
}

/// What consumers hold as both backends agree on it: positions and staleness set aside (Q-16).
type Plain = Vec<(LiveId, Option<Vec<(SeriesId, Live)>>, Vec<Fact>)>;

fn plain(held: &BTreeMap<LiveId, Held>) -> Plain {
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
        if rig.ended {
            return Ok(());
        }
        rig.step(s)?;
    }
    if rig.ended {
        return Ok(());
    }
    rig.poll()
}

#[test]
fn every_batch_brings_each_consumer_to_the_static_read_at_its_cut_and_to_the_models() {
    // Each case opens two stores, two readers and five sessions: a quarter of the configured cases.
    let cases = (ProptestConfig::default().cases / 4).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner.run(&steps(), run).unwrap();
    for (k, what) in SEEN
        .iter()
        .zip(["joined", "left", "updated", "appended", "gap", "lagged"])
    {
        assert!(
            k.load(Ordering::Relaxed) > 0,
            "no {what} event was generated"
        );
    }
}
