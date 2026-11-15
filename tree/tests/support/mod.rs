//! Generated readings and parameters, and the conversions to the reference model's types.

#![allow(dead_code)]

use kvik_model as km;
use kvik_tree::{Body, CounterRules, Kind, Node, Params, StateKey, Value};
use proptest::prelude::*;

pub const SECOND: i64 = 1_000_000_000;

/// Readings of one series in strictly increasing `t` inside `[start, end)`.
#[derive(Clone, Debug)]
pub struct Case {
    pub p: Params,
    pub readings: Vec<(i64, Value)>,
    pub start: i64,
    pub end: i64,
}

impl Case {
    /// The readings with `t` in `[a, b)`.
    pub fn within(&self, a: i64, b: i64) -> &[(i64, Value)] {
        let i = self.readings.partition_point(|(t, _)| *t < a);
        let j = self.readings.partition_point(|(t, _)| *t < b).max(i);
        &self.readings[i..j]
    }

    /// Cut points inside the span from fractions in `0..=1000`, sorted, with the span edges around them.
    pub fn edges(&self, cuts: &[u16]) -> Vec<i64> {
        let w = i128::from(self.end - self.start);
        let mut e: Vec<i64> = cuts
            .iter()
            .map(|c| self.start + (w * i128::from(*c) / 1000) as i64)
            .collect();
        e.push(self.start);
        e.push(self.end);
        e.sort_unstable();
        e
    }
}

const GAUGE: [f64; 14] = [
    0.0,
    -0.0,
    5e-324,
    -5e-324,
    f64::MIN_POSITIVE,
    0.1,
    1.5,
    -2.25,
    21.37,
    1e300,
    -1e300,
    f64::MAX,
    -f64::MAX,
    1e-300,
];
const COUNTER: [f64; 10] = [
    0.0,
    5e-324,
    0.1,
    1.0,
    10.0,
    99.5,
    100.0,
    1e300,
    f64::MAX,
    3.0,
];
const TEXT: [&str; 4] = ["", "on", "über", "a much longer text value"];

fn value(kind: Kind, pick: u8, x: i16) -> Value {
    let i = usize::from(pick);
    match kind {
        Kind::Gauge | Kind::Interval if i % 3 == 0 => Value::F64(f64::from(x) / 8.0),
        Kind::Gauge | Kind::Interval => Value::F64(GAUGE[i % GAUGE.len()]),
        Kind::Counter if i % 3 == 0 => Value::F64(f64::from(x.unsigned_abs()) / 4.0),
        Kind::Counter => Value::F64(COUNTER[i % COUNTER.len()]),
        Kind::State if x < 0 => Value::Bool(i % 2 == 0),
        Kind::State => Value::State((i % 4) as u16),
        Kind::Text => Value::Text(TEXT[i % TEXT.len()].to_owned()),
    }
}

/// Parameters of every kind, with staleness limits shorter and longer than the gaps and every counter rule.
pub fn params() -> impl Strategy<Value = Params> {
    (
        0usize..5,
        prop_oneof![Just(None), (1i64..40).prop_map(|s| Some(s * SECOND / 2))],
        prop_oneof![Just(None), Just(Some(100.0)), Just(Some(f64::MAX))],
        prop_oneof![Just(0.1), Just(0.5)],
        prop_oneof![Just(SECOND), Just(4 * SECOND), Just(10 * SECOND)],
    )
        .prop_map(|(k, staleness, wrap_at, reset_ceiling, d)| {
            let kind = [
                Kind::Gauge,
                Kind::Counter,
                Kind::State,
                Kind::Text,
                Kind::Interval,
            ][k];
            if kind == Kind::Interval {
                return Params::interval(d);
            }
            Params {
                counter: CounterRules {
                    wrap_at,
                    reset_ceiling,
                },
                ..Params::new(kind, staleness)
            }
        })
}

/// A series' readings: up to 40 at whole or odd nanosecond times (interval readings at multiples of d), in a span
/// that extends past them on both sides.
pub fn case() -> impl Strategy<Value = Case> {
    (
        params(),
        prop::collection::btree_set(0i64..400, 0..40),
        prop::collection::vec((any::<u8>(), any::<i16>()), 40),
        0i64..1_000,
    )
        .prop_map(|(p, ticks, seeds, jitter)| {
            let unit = p.interval.unwrap_or(SECOND / 2);
            let readings = ticks
                .iter()
                .zip(&seeds)
                .map(|(k, (pick, x))| {
                    let t = k * unit + if p.interval.is_some() { 0 } else { jitter };
                    (t, value(p.kind, *pick, *x))
                })
                .collect();
            Case {
                p,
                readings,
                start: -3 * unit,
                end: 403 * unit,
            }
        })
}

/// The model's parameters for `p`.
pub fn model_params(p: &Params) -> km::summary::Params {
    let kind = match p.kind {
        Kind::Gauge => km::types::Kind::Gauge,
        Kind::Counter => km::types::Kind::Counter,
        Kind::State => km::types::Kind::State,
        Kind::Text => km::types::Kind::Text,
        Kind::Interval => km::types::Kind::Interval,
    };
    if let Some(d) = p.interval {
        return km::summary::Params::interval(d, 0);
    }
    km::summary::Params {
        counter: km::summary::CounterRules {
            wrap_at: p.counter.wrap_at,
            reset_ceiling: p.counter.reset_ceiling,
        },
        ..km::summary::Params::new(kind, p.staleness)
    }
}

fn name(i: u16) -> String {
    format!("s{i}")
}

pub fn model_value(v: &Value) -> km::types::Value {
    match v {
        Value::F64(x) => km::types::Value::Num(*x),
        Value::Bool(b) => km::types::Value::Bool(*b),
        Value::State(i) => km::types::Value::Str(name(*i)),
        Value::Text(s) => km::types::Value::Str(s.clone()),
    }
}

pub fn model_readings(r: &[(i64, Value)]) -> Vec<(i64, km::types::Value)> {
    r.iter().map(|(t, v)| (*t, model_value(v))).collect()
}

/// `n` as the model holds it, a state index named `s<index>`.
pub fn model_node(n: &Node) -> km::summary::Node {
    use km::summary::Body as B;
    let body = match &n.body {
        Body::Gauge {
            sum,
            min,
            max,
            integral,
        } => B::Gauge {
            sum: sum.to_ratio(),
            min: *min,
            max: *max,
            integral: integral.to_ratio(),
        },
        Body::Counter {
            increase,
            resets,
            regressions,
            steps,
            long_steps,
            elapsed,
        } => B::Counter {
            increase: increase.to_ratio(),
            resets: *resets,
            regressions: *regressions,
            steps: *steps,
            long_steps: *long_steps,
            elapsed: *elapsed,
        },
        Body::State { time, transitions } => B::State {
            time: time
                .iter()
                .map(|(k, ns)| {
                    let k = match k {
                        StateKey::Bool(b) => km::types::StateKey::Bool(*b),
                        StateKey::State(i) => km::types::StateKey::Name(name(*i)),
                    };
                    (k, *ns)
                })
                .collect(),
            transitions: *transitions,
        },
        Body::Text => B::Text,
        Body::Interval {
            sum,
            min,
            max,
            apportioned,
        } => B::Interval {
            sum: sum.to_ratio(),
            min: *min,
            max: *max,
            apportioned: *apportioned,
        },
    };
    km::summary::Node {
        start: n.start,
        end: n.end,
        count: n.count,
        first: n.first.as_ref().map(|(t, v)| (*t, model_value(v))),
        last: n.last.as_ref().map(|(t, v)| (*t, model_value(v))),
        coverage: n.coverage,
        body,
    }
}
