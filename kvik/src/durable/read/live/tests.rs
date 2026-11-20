use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{LogConfig, ReaderConfig, StoreConfig};
use crate::durable::tests::block_on;
use crate::env::{Env, FixedBootId, ManualClock, MemFs};
use crate::health::{LossKind, LossReport};
use crate::read::{
    Answer, Appended, Live, LiveEvent, LiveId, LiveOptions, Part, Query, Selection, UpdateBatch,
    Updated,
};
use crate::series::{Kind, SeriesSpec, ValueType};
use crate::time::Timestamp;
use crate::types::{Fact, Position, Reading, SeriesId, Value};
use crate::{LiveSession, Reader, Select, Store};

pub(super) const T0: i64 = 1_767_225_600_000_000_000;
/// A step of the clock past both the least batch interval and the poll interval.
pub(super) const TICK: Duration = Duration::from_millis(200);

pub(super) struct World {
    pub(super) clock: ManualClock,
    pub(super) env: Env,
    pub(super) fs: MemFs,
    pub(super) boot: FixedBootId,
    pub(super) dir: PathBuf,
    pub(super) log: LogConfig,
}

impl World {
    pub(super) fn new(name: &str) -> Self {
        let clock = ManualClock::new(Timestamp(T0));
        let mut env = Env::test(clock.clone());
        let fs = MemFs::new();
        env.fs = Arc::new(fs.clone());
        let boot = FixedBootId::new(Some([1; 16]));
        env.boot = Arc::new(boot.clone());
        World {
            clock,
            env,
            fs,
            boot,
            dir: PathBuf::from(format!("live/{name}")),
            log: LogConfig::default(),
        }
    }

    pub(super) fn open(&self) -> Store {
        block_on(Store::open(
            StoreConfig::new(&self.dir, "Etc/UTC")
                .create_if_missing(true)
                .env(self.env.clone())
                .log(self.log.clone()),
        ))
        .unwrap()
    }

    /// The bytes of the log's segment files, with their paths.
    pub(super) fn segments(&self) -> Vec<(PathBuf, Vec<u8>)> {
        self.fs
            .paths()
            .into_iter()
            .filter(|p| {
                p.starts_with(self.dir.join("log"))
                    && p.extension().is_some_and(|e| e == "open" || e == "kvl")
            })
            .filter_map(|p| Some((p.clone(), self.fs.read(&p)?)))
            .collect()
    }

    /// Damages `text` in every segment file that holds it.
    pub(super) fn damage(&self, text: &[u8]) {
        assert!(self.damage_any(text));
    }

    /// Damages `text` in every segment file that holds it; whether one did.
    pub(super) fn damage_any(&self, text: &[u8]) -> bool {
        let mut found = false;
        for (p, b) in self.segments() {
            if let Some(at) = b.windows(text.len()).position(|w| w == text) {
                self.fs.poke(&p, at, b"X");
                found = true;
            }
        }
        found
    }

    /// Every fact the store holds in position order, as a reader opened now reads them.
    pub(super) fn history(&self) -> Vec<Fact> {
        let r = self.reader();
        let mut all: Vec<Fact> = Vec::new();
        for id in 1.. {
            let Ok(f) = r.facts(
                SeriesId(id),
                Timestamp(i64::MIN)..Timestamp(i64::MAX),
                crate::read::View::History,
            ) else {
                break;
            };
            all.extend(f.map(Result::unwrap));
        }
        all.sort_by_key(|f| f.position);
        all
    }

    pub(super) fn reader(&self) -> Reader {
        Reader::open(ReaderConfig::new(&self.dir).env(self.env.clone())).unwrap()
    }
}

pub(super) fn gauge(name: &str) -> SeriesSpec {
    SeriesSpec::new(
        name,
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
}

pub(super) fn emit(w: &World, s: &Store, id: SeriesId, v: f64) {
    block_on(s.emit(&[Reading::new(id, w.clock.wall(), Value::F64(v))])).unwrap();
}

pub(super) const S: i64 = 1_000_000_000;

pub(super) fn note(w: &World, s: &Store, id: SeriesId, secs: i64, text: &str) -> Position {
    let r = Reading::new(
        id,
        Timestamp(w.clock.wall().0 + secs * S),
        Value::Text(text.into()),
    );
    block_on(s.emit(&[r])).unwrap().last().unwrap()
}

pub(super) fn tail_all() -> Query {
    Query::Tail {
        sel: Selection::all(),
    }
}

/// A session with a `Latest` and a `Tail` of every series, opened and polled once.
pub(super) fn watched(
    r: &Reader,
    tail: LiveOptions,
) -> (LiveSession, LiveId, LiveId, BTreeMap<LiveId, Held>) {
    let mut s = r.session();
    let l = s
        .open(Query::Latest(Selection::all()), LiveOptions::default())
        .unwrap();
    let t = s.open(tail_all(), tail).unwrap();
    let mut held = BTreeMap::new();
    apply(&mut held, &s.next(Duration::ZERO).unwrap().unwrap());
    (s, l, t, held)
}

pub(super) fn poll(
    w: &World,
    s: &mut LiveSession,
    held: &mut BTreeMap<LiveId, Held>,
) -> Option<UpdateBatch> {
    w.clock.advance(TICK);
    let b = s.next(Duration::ZERO).unwrap()?;
    apply(held, &b);
    Some(b)
}

/// Asserts that a `Tail` opened at `open` holds at `cut` what it owes: the facts a reader opened now reads there,
/// less the spans it was told of, and [`losses_named`] for the losses the writer and a fresh reader report.
pub(super) fn tail_is_owed(w: &World, s: &Store, open: Position, cut: Position, h: &Held) {
    tail_is_owed_but(w, s, (open, cut), h, &|_| false);
}

/// [`tail_is_owed`] for a `Tail` that does not select the facts `unselected` names.
pub(super) fn tail_is_owed_but(
    w: &World,
    s: &Store,
    (open, cut): (Position, Position),
    h: &Held,
    unselected: &dyn Fn(&Fact) -> bool,
) {
    let skipped = |p: Position| {
        h.lagged
            .iter()
            .chain(&h.gaps)
            .any(|(a, b)| *a <= p && p <= *b)
    };
    let owed: Vec<Fact> = w
        .history()
        .into_iter()
        .filter(|f| f.position > open && f.position <= cut && !skipped(f.position))
        .filter(|f| !unselected(f))
        .collect();
    assert_eq!(h.tail, owed);
    let reports: Vec<LossReport> = s.losses().into_iter().chain(w.reader().losses()).collect();
    losses_named(&reports, open, cut, h);
}

/// Asserts that every loss of `reports` between `open` and `cut` was named to a `Tail`, and that it holds no fact
/// inside one.
pub(super) fn losses_named(reports: &[LossReport], open: Position, cut: Position, h: &Held) {
    for l in reports.iter().filter(|l| l.kind == LossKind::DataLost) {
        let (Some(first), Some(last)) = (l.first, l.last) else {
            continue;
        };
        if last <= open || first > cut {
            continue;
        }
        assert!(
            h.gaps
                .iter()
                .chain(&h.lagged)
                .any(|(a, b)| *a <= last && first <= *b),
            "{l:?} not named: {h:?}"
        );
        assert!(
            h.tail
                .iter()
                .all(|f| f.position < first || f.position > last),
            "{l:?}"
        );
    }
}

/// Asserts that a `Latest` of every series holds what a reader opened now reads.
pub(super) fn latest_is_static(w: &World, h: &Held) {
    let snap = w.reader().snapshot().unwrap();
    let want: Vec<(SeriesId, Live)> = snap
        .select(&Selection::all())
        .unwrap()
        .into_iter()
        .map(|id| (id, snap.live(id).unwrap()))
        .collect();
    assert_eq!(h.latest.as_ref().unwrap(), &want);
}

/// What a consumer holds of one live read: what applying every event it was sent gives.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Held {
    pub(super) latest: Option<Vec<(SeriesId, Live)>>,
    pub(super) tail: Vec<Fact>,
    pub(super) lagged: Vec<(Position, Position)>,
    pub(super) gaps: Vec<(Position, Position)>,
    pub(super) resets: Vec<Position>,
    pub(super) closed: bool,
}

impl Held {
    pub(super) fn apply(&mut self, e: &LiveEvent) {
        match e {
            LiveEvent::Opened(Answer::Latest(v)) => self.latest = Some(v.clone()),
            LiveEvent::Opened(Answer::Tail) => {}
            LiveEvent::Appended(Appended::Facts(f)) => {
                if let (Some(last), Some(first)) = (self.tail.last(), f.first()) {
                    assert!(last.position < first.position, "position order");
                }
                assert!(f.windows(2).all(|w| w[0].position < w[1].position));
                self.tail.extend(f.iter().cloned());
            }
            LiveEvent::Updated(Updated::Live { series, live }) => {
                let v = self.latest.as_mut().expect("opened");
                let at = v.iter_mut().find(|(k, _)| k == series).expect("shown");
                at.1 = live.clone();
            }
            LiveEvent::Reset { above } => {
                self.resets.push(*above);
                self.tail.retain(|f| f.position <= *above);
            }
            LiveEvent::Gap { first, last } => {
                self.gaps.push((*first, *last));
                self.tail
                    .retain(|f| f.position < *first || f.position > *last);
            }
            LiveEvent::Lagged { from, to } => self.lagged.push((*from, *to)),
            LiveEvent::Joined {
                series,
                index,
                part: Part::Live(live),
                ..
            } => {
                let v = self.latest.as_mut().expect("opened");
                assert!(!v.iter().any(|(k, _)| k == series));
                v.insert(*index, (*series, live.clone()));
            }
            LiveEvent::Left { series } => {
                let v = self.latest.as_mut().expect("opened");
                v.retain(|(k, _)| k != series);
            }
            LiveEvent::Closed(_) => self.closed = true,
        }
    }
}

/// Applies a batch to the consumers of its live reads, checking the order of each read's events.
pub(super) fn apply(held: &mut BTreeMap<LiveId, Held>, b: &UpdateBatch) {
    let mut last: BTreeMap<LiveId, u8> = BTreeMap::new();
    for (id, e) in &b.events {
        let r = crate::read::event_rank(e);
        let was = last.insert(*id, r).unwrap_or(0);
        assert!(was <= r, "event order in {b:?}");
        held.entry(*id).or_default().apply(e);
    }
}

pub(super) fn kinds(b: &UpdateBatch) -> Vec<&'static str> {
    b.events
        .iter()
        .map(|(_, e)| match e {
            LiveEvent::Opened(_) => "opened",
            LiveEvent::Appended(_) => "appended",
            LiveEvent::Updated(_) => "updated",
            LiveEvent::Reset { .. } => "reset",
            LiveEvent::Gap { .. } => "gap",
            LiveEvent::Lagged { .. } => "lagged",
            LiveEvent::Joined { .. } => "joined",
            LiveEvent::Left { .. } => "left",
            LiveEvent::Closed(_) => "closed",
        })
        .collect()
}

#[test]
fn a_session_opens_then_brings_a_new_reading_to_latest_and_tail() {
    let w = World::new("smoke");
    let s = w.open();
    let id = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    emit(&w, &s, id, 1.0);
    for reader in [s.reader(), w.reader()] {
        let mut session = reader.session();
        let l = session
            .open(Query::Latest(Selection::all()), LiveOptions::default())
            .unwrap();
        let t = session
            .open(
                Query::Tail {
                    sel: Select::All.into(),
                },
                LiveOptions::default(),
            )
            .unwrap();
        let b = session.next(Duration::ZERO).unwrap().unwrap();
        assert_eq!(kinds(&b), ["opened", "opened"]);
        assert!(
            matches!(&b.events[0], (x, LiveEvent::Opened(Answer::Latest(v))) if *x == l && v.len() == 1)
        );
        assert!(session.next(Duration::ZERO).unwrap().is_none());
        w.clock.advance(TICK);
        emit(&w, &s, id, 2.0);
        let b = session.next(Duration::ZERO).unwrap().unwrap();
        assert_eq!(kinds(&b), ["updated", "appended"], "{b:?}");
        assert!(
            matches!(&b.events[0].1, LiveEvent::Updated(Updated::Live { live: Live::Value(v), .. }) if v.fact.value == Value::F64(2.0))
        );
        assert!(
            matches!(&b.events[1], (x, LiveEvent::Appended(Appended::Facts(f))) if *x == t && f.len() == 1)
        );
    }
    block_on(s.shutdown()).unwrap();
}
