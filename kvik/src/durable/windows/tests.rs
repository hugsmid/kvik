use kvik_model::oracle;
use kvik_model::registry::Timing;
use kvik_model::summary::Params;
use kvik_model::types::{Kind, SECOND};
use kvik_tree as kt;
use proptest::prelude::*;

use super::stream::{Folder, reading};
use crate::error::QueryError;
use crate::fold::{self, Reading};
use crate::time::Timestamp;
use crate::types::{Fact, FactFlags, Position, Quality, SeriesId, Value};

const NAMES: usize = 3;

/// A fact from its code: values that rise and fall back, no value, an undeclared state, bad quality.
fn fact(kind: Kind, t: i64, code: u8) -> Fact {
    let k = code % 8;
    let value = match (kind, k) {
        (_, 7) => Value::Absent,
        (Kind::State, _) => Value::State(u16::from(k % 4)),
        (_, k) => Value::F64(f64::from(k) * 1.5),
    };
    let quality = if code >= 200 {
        Quality::Bad
    } else {
        Quality::Good
    };
    Fact {
        series: SeriesId(1),
        t: Timestamp(t),
        position: Position::ORIGIN,
        received_at: Timestamp(0),
        value,
        quality,
        supplied_quality: quality,
        flags: FactFlags::default(),
        changed_at: None,
    }
}

fn model_value(v: &kt::Value) -> kvik_model::types::Value {
    match v {
        kt::Value::State(i) => kvik_model::types::Value::Str(format!("s{i}")),
        other => kvik_model::types::Value::Num(other.as_f64().unwrap()),
    }
}

proptest! {
    /// Windows over consecutive spans folded from one stream, the core a chunk at a time, equal the one-shot fold of
    /// every reading (the evaluation before roadmap 9.0h) and the model's direct computation, for every kind and
    /// timing, spans wholly ahead of `now`, and chunks down to one reading.
    #[test]
    fn a_streamed_fold_equals_the_one_shot_fold_and_the_model(
        ts in prop::collection::btree_set(-80i64..80, 0..24),
        codes in prop::collection::vec(any::<u8>(), 24),
        kind in 0usize..4,
        timing in 0usize..3,
        staleness in prop_oneof![Just(None), (1i64..20).prop_map(Some)],
        start in -90i64..90,
        widths in prop::collection::vec(1i64..30, 1..6),
        now in -100i64..100,
        chunk in 1usize..5,
    ) {
        let kind = [Kind::Gauge, Kind::Counter, Kind::State, Kind::Interval][kind];
        let mut p = if kind == Kind::Interval {
            Params::interval(4 * SECOND, SECOND)
        } else {
            Params::new(kind, staleness.map(|s| s * SECOND))
        };
        p.timing = [Timing::Observed, Timing::Scheduled, Timing::Forecast][timing];
        let facts: Vec<Fact> = ts
            .iter()
            .zip(&codes)
            .map(|(t, c)| fact(kind, t * SECOND, *c))
            .filter(|f| p.interval.is_none_or(|iv| iv.on_phase(f.t.0)))
            .collect();
        let readings: Vec<Reading> = facts.iter().filter_map(|f| reading(NAMES, f.clone())).collect();
        let mut spans = Vec::new();
        let mut lo = start * SECOND;
        for w in &widths {
            spans.push((lo, lo + w * SECOND));
            lo += w * SECOND;
        }
        let (from, to) = (spans[0].0, spans[spans.len() - 1].1);
        let now = now * SECOND;
        let reach = fold::reach(&p, from);
        let fed: Vec<Result<Fact, QueryError>> = facts
            .iter()
            .filter(|f| reach <= f.t.0 && f.t.0 < to)
            .cloned()
            .map(Ok)
            .collect();
        let prior = fold::before(&readings, from)
            .cloned()
            .filter(|_| p.interval.is_none() && !p.wholly_future(from, now));
        let mut folder =
            Folder::of(p, now, NAMES, Box::new(fed.into_iter()), prior).chunked(chunk);
        let names: Vec<String> = (0..NAMES).map(|i| format!("s{i}")).collect();
        let model: Vec<_> = readings.iter().map(|(t, v)| (*t, model_value(v))).collect();
        for (lo, hi) in spans {
            let got = fold::model_node(folder.node(lo, hi).unwrap(), &names);
            let before = fold::before(&readings, lo);
            let want = fold::model_node(fold::window(&p, before, &readings, lo, hi, now), &names);
            prop_assert_eq!(&got, &want, "[{}, {})", lo, hi);
            prop_assert_eq!(&got, &oracle::window(&p, &model, lo, hi, now));
        }
    }
}

/// A folder of `n` readings a second apart from `0`, folding `[0, n)` s in spans of `width` seconds.
fn fold_long(p: Params, n: i64, width: i64) -> usize {
    let on_phase = move |t: &i64| p.interval.is_none_or(|iv| iv.on_phase(*t));
    let facts = (0..n)
        .map(|k| k * SECOND)
        .filter(on_phase)
        .map(move |t| Ok(fact(p.kind, t, 1)));
    let mut folder = Folder::of(p, n * SECOND, NAMES, Box::new(facts), None);
    let mut lo = 0;
    while lo < n {
        folder
            .node(lo * SECOND, (lo + width).min(n) * SECOND)
            .unwrap();
        lo += width;
    }
    folder.held_most()
}

#[test]
fn a_folder_holds_one_chunk_whatever_the_range_and_the_bucket() {
    let gauge = Params::new(Kind::Gauge, None);
    assert_eq!(fold_long(gauge, 20_000, 20_000), super::stream::CHUNK);
    assert_eq!(fold_long(gauge, 20_000, 700), super::stream::CHUNK);
    // An interval series keeps only the readings whose 4 s interval reaches into the bucket or past it: one reading
    // at each edge.
    let interval = Params::interval(4 * SECOND, SECOND);
    assert!(fold_long(interval, 20_000, 20_000) <= 1);
    assert!(fold_long(interval, 20_000, 7) <= 2);
}
