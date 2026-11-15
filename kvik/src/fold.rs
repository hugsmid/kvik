//! Window evaluation over summarised readings (spec §5.2, §6), independent of where the readings come from. The
//! node fold, merge and edge credit are `kvik_tree`'s, the ones the stored tree uses, in exact arithmetic (Q-180), so
//! results equal the model's; the window rules (credit limit, readiness, reach) are the model's. A window is a core
//! up to its credit limit, folded from the readings inside (stored nodes take the place of the readings they cover
//! from roadmap 7.1b), then the edge stretches; an interval series is credited pro rata instead. Readings are
//! current, not bad quality, in strictly increasing `t`.

use kvik_model as km;
use kvik_model::registry::Timing;
use kvik_model::summary::Params;
use kvik_model::types::Nanos;
use kvik_tree as kt;

/// A summarised reading: its event time and value, a named state by its index.
pub(crate) type Reading = (Nanos, kt::Value);

/// The parameters a node depends on, from the model's.
pub(crate) fn node_params(p: &Params) -> kt::Params {
    kt::Params {
        kind: match p.kind {
            km::types::Kind::Gauge => kt::Kind::Gauge,
            km::types::Kind::Counter => kt::Kind::Counter,
            km::types::Kind::State => kt::Kind::State,
            km::types::Kind::Text => kt::Kind::Text,
            km::types::Kind::Interval => kt::Kind::Interval,
        },
        staleness: p.staleness,
        counter: kt::CounterRules {
            wrap_at: p.counter.wrap_at,
            reset_ceiling: p.counter.reset_ceiling,
        },
        interval: p.interval.map(|iv| iv.length),
    }
}

/// The earliest `t` a window starting at `start` reads besides the reading before it: for an interval series the
/// start of the earliest interval that can reach into the window, else `start`.
pub(crate) fn reach(p: &Params, start: Nanos) -> Nanos {
    match p.interval {
        Some(iv) => start.saturating_sub(iv.length - 1),
        None => start,
    }
}

/// A window `[start, end)` evaluated at `now` from raw readings: `prior` is the last reading before `start`, and
/// `readings` hold at least those with `t` in `[start, end)` (others are ignored). An interval series takes
/// [`interval_window`] instead.
pub(crate) fn window(
    p: &Params,
    prior: Option<&Reading>,
    readings: &[Reading],
    start: Nanos,
    end: Nanos,
    now: Nanos,
) -> kt::Node {
    if p.interval.is_some() {
        return interval_window(p, readings, start, end, now);
    }
    let np = node_params(p);
    // A window wholly after `now` on an observed series is empty, with zero coverage.
    if p.wholly_future(start, now) {
        return kt::Node::empty(np.kind, start, end);
    }
    let until = p.credit_limit(end, now).max(start);
    let core = kt::fold(&np, start, until, within(readings, start, until));
    let mut n = kt::with_stretches(&np, core, prior, until);
    n.end = end;
    n
}

/// A window `[start, end)` over an interval series, evaluated at `now`: every ready reading's amount pro rata to
/// the part of its interval inside the window (spec §6 Aggregates); a reading of an observed series is ready once
/// `t + d ≤ now`. `readings` hold at least those with `t` in `[reach(start), end)`.
pub(crate) fn interval_window(
    p: &Params,
    readings: &[Reading],
    start: Nanos,
    end: Nanos,
    now: Nanos,
) -> kt::Node {
    let np = node_params(p);
    let end = end.max(start);
    let observed = p.timing == Timing::Observed;
    let ready = within(readings, reach(p, start), end)
        .iter()
        .filter(|(t, _)| !observed || p.ready_at(*t) <= now);
    let mut n = kt::Node::empty(np.kind, start, end);
    kt::credit_intervals(&np, &mut n, ready, start, end);
    n
}

/// The readings with `t` in `[from, to)`, from readings in `t` order.
pub(crate) fn within(readings: &[Reading], from: Nanos, to: Nanos) -> &[Reading] {
    let a = readings.partition_point(|(t, _)| *t < from);
    let b = readings.partition_point(|(t, _)| *t < to).max(a);
    &readings[a..b]
}

/// The last reading before `t`, from readings in `t` order.
pub(crate) fn before(readings: &[Reading], t: Nanos) -> Option<&Reading> {
    readings[..readings.partition_point(|(r, _)| *r < t)].last()
}

/// A node as the model holds it, which the result rules shared by both backends take; `names` are the series'
/// declared state names, which every state index of the node must name.
pub(crate) fn model_node(n: kt::Node, names: &[String]) -> km::summary::Node {
    let name = |i: u16| {
        names
            .get(usize::from(i))
            .expect("a summarised state names a declared state")
            .clone()
    };
    let value = |v: kt::Value| match v {
        kt::Value::F64(x) => km::types::Value::Num(x),
        kt::Value::Bool(b) => km::types::Value::Bool(b),
        kt::Value::State(i) => km::types::Value::Str(name(i)),
        kt::Value::Text(s) => km::types::Value::Str(s),
    };
    use km::summary::Body as B;
    let body = match n.body {
        kt::Body::Gauge {
            sum,
            min,
            max,
            integral,
        } => B::Gauge {
            sum: sum.to_ratio(),
            min,
            max,
            integral: integral.to_ratio(),
        },
        kt::Body::Counter {
            increase,
            resets,
            regressions,
            steps,
            long_steps,
            elapsed,
        } => B::Counter {
            increase: increase.to_ratio(),
            resets,
            regressions,
            steps,
            long_steps,
            elapsed,
        },
        kt::Body::State { time, transitions } => {
            let mut by_name = std::collections::BTreeMap::new();
            for (k, ns) in time {
                let key = match k {
                    kt::StateKey::Bool(b) => km::types::StateKey::Bool(b),
                    kt::StateKey::State(i) => km::types::StateKey::Name(name(i)),
                };
                *by_name.entry(key).or_insert(0) += ns;
            }
            B::State {
                time: by_name,
                transitions,
            }
        }
        kt::Body::Text => B::Text,
        kt::Body::Interval {
            sum,
            min,
            max,
            apportioned,
        } => B::Interval {
            sum: sum.to_ratio(),
            min,
            max,
            apportioned,
        },
    };
    km::summary::Node {
        start: n.start,
        end: n.end,
        count: n.count,
        first: n.first.map(|(t, v)| (t, value(v))),
        last: n.last.map(|(t, v)| (t, value(v))),
        coverage: n.coverage,
        body,
    }
}

#[cfg(test)]
mod tests {
    use kvik_model::oracle;
    use kvik_model::types::{Kind, SECOND};
    use proptest::prelude::*;

    use super::*;

    fn params(kind: Kind, staleness: Option<i64>, interval: bool) -> Params {
        if interval {
            Params::interval(4 * SECOND, SECOND)
        } else {
            Params::new(kind, staleness)
        }
    }

    proptest! {
        /// The fold equals the model's direct computation of a window (invariant 7), from the readings a caller
        /// passes, also when it passes more than the window needs.
        #[test]
        fn a_window_equals_the_direct_computation(
            ts in prop::collection::btree_set(-60i64..60, 0..12),
            values in prop::collection::vec(0u8..4, 12),
            kind in 0usize..4,
            timing in 0usize..3,
            staleness in prop_oneof![Just(None), (1i64..20).prop_map(Some)],
            (start, len, now) in (-70i64..70, 1i64..80, -80i64..90),
        ) {
            let kinds = [Kind::Gauge, Kind::Counter, Kind::State, Kind::Interval];
            let kind = kinds[kind];
            let mut p = params(kind, staleness.map(|s| s * SECOND), kind == Kind::Interval);
            p.timing = [Timing::Observed, Timing::Scheduled, Timing::Forecast][timing];
            let names: Vec<String> = (0..4).map(|i| format!("s{i}")).collect();
            let all: Vec<Reading> = ts
                .iter()
                .zip(&values)
                .map(|(t, v)| {
                    let v = match kind {
                        Kind::State => kt::Value::State(u16::from(*v)),
                        _ => kt::Value::F64(f64::from(*v) * 1.5),
                    };
                    (t * SECOND, v)
                })
                .filter(|(t, _)| p.interval.is_none_or(|iv| iv.on_phase(*t)))
                .collect();
            let model: Vec<(Nanos, km::types::Value)> = all
                .iter()
                .map(|(t, v)| {
                    let v = match v {
                        kt::Value::State(i) => km::types::Value::Str(names[usize::from(*i)].clone()),
                        other => km::types::Value::Num(other.as_f64().unwrap()),
                    };
                    (*t, v)
                })
                .collect();
            let (start, end, now) = (start * SECOND, (start + len) * SECOND, now * SECOND);
            let want = oracle::window(&p, &model, start, end, now);
            let got = window(&p, before(&all, start), &all, start, end, now);
            prop_assert_eq!(model_node(got, &names), want.clone());
            // Only the part a caller must pass.
            let from = reach(&p, start);
            let got = window(&p, before(&all, start), within(&all, from, end), start, end, now);
            prop_assert_eq!(model_node(got, &names), want);
        }
    }
}
