use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{ReaderConfig, StoreConfig};
use crate::durable::tests::block_on;
use crate::env::{Env, ManualClock, MemFs};
use crate::read::{
    Answer, Appended, Live, LiveEvent, LiveOptions, Query, Selection, UpdateBatch, Updated,
};
use crate::series::{Kind, SeriesSpec, ValueType};
use crate::time::Timestamp;
use crate::types::{Reading, SeriesId, Value};
use crate::{Reader, Select, Store};

const T0: i64 = 1_767_225_600_000_000_000;
const TICK: Duration = Duration::from_millis(200);

struct World {
    clock: ManualClock,
    env: Env,
    dir: PathBuf,
}

impl World {
    fn new(name: &str) -> Self {
        let clock = ManualClock::new(Timestamp(T0));
        let mut env = Env::test(clock.clone());
        env.fs = Arc::new(MemFs::new());
        World {
            clock,
            env,
            dir: PathBuf::from(format!("live/{name}")),
        }
    }

    fn open(&self) -> Store {
        block_on(Store::open(
            StoreConfig::new(&self.dir, "Europe/London")
                .create_if_missing(true)
                .env(self.env.clone()),
        ))
        .unwrap()
    }

    fn reader(&self) -> Reader {
        Reader::open(ReaderConfig::new(&self.dir).env(self.env.clone())).unwrap()
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
}

fn emit(w: &World, s: &Store, id: SeriesId, v: f64) {
    block_on(s.emit(&[Reading::new(id, w.clock.wall(), Value::F64(v))])).unwrap();
}

fn kinds(b: &UpdateBatch) -> Vec<&'static str> {
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
