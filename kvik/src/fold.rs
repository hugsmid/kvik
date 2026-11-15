//! Window evaluation as a fold over readings and partial summaries (spec §5.2, §5.3, §6), with no knowledge of
//! where the readings come from.
//!
//! The per-node fold (count, sum, min, max, integral, first and last, the boundary pair, counter steps, state time,
//! coverage) and its associative merge are `kvik_model::summary`'s, in exact arithmetic: one implementation for the
//! reference model and the engine, so that results agree to the bit. This module composes them into window
//! results the way `docs/spec.md` §6 Evaluation describes: a **core** over the window up to its credit limit,
//! assembled from parts (nodes folded from raw readings here; stored nodes of the summary tree, roadmap 7.x, take
//! the place of the parts they cover), then the **edge stretches** from the reading before the window. An interval
//! series is credited pro rata at the edges instead (spec §5.4, §6). Callers pass readings that are summarised
//! (current, not bad quality), in strictly increasing `t`.

use kvik_model::registry::Timing;
use kvik_model::summary::{Node, Params, credit_intervals, direct, merge_all, with_stretches};
use kvik_model::types::{Nanos, Value};

/// A summarised reading: its event time and value.
pub(crate) type Reading = (Nanos, Value);

/// The earliest `t` a window starting at `start` reads besides the reading before it: for an interval series the
/// start of the earliest interval that can reach into the window, else `start`.
pub(crate) fn reach(p: &Params, start: Nanos) -> Nanos {
    match p.interval {
        Some(iv) => start.saturating_sub(iv.length - 1),
        None => start,
    }
}

/// The node of the readings of `[start, end)`, without stretches: a part of a window's core. `readings` are the
/// readings with `t` in the span.
pub(crate) fn part(p: &Params, start: Nanos, end: Nanos, readings: &[Reading]) -> Node {
    direct(p, start, end, readings)
}

/// The core of a window `[start, until)` from adjacent parts in order, `until` being its credit limit.
pub(crate) fn core(p: &Params, start: Nanos, until: Nanos, parts: &[Node]) -> Node {
    let mut core = merge_all(p, start, parts.iter());
    core.start = start;
    core.end = until;
    core
}

/// A window `[start, end)` from its core and the last reading before `start`: the leading stretch credits that
/// reading within its staleness limit, the trailing stretch runs from the last reading to the credit limit (spec §6
/// Edge stretches).
pub(crate) fn finish(
    p: &Params,
    core: Node,
    prior: Option<&Reading>,
    until: Nanos,
    end: Nanos,
) -> Node {
    let mut n = with_stretches(p, core, prior, until);
    n.end = end;
    n
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
) -> Node {
    if p.interval.is_some() {
        return interval_window(p, readings, start, end, now);
    }
    // A window wholly after `now` on an observed series is empty, with zero coverage.
    if p.wholly_future(start, now) {
        return Node::empty(p.kind, start, end);
    }
    let until = p.credit_limit(end, now).max(start);
    let inside = within(readings, start, until);
    let core = core(p, start, until, &[part(p, start, until, inside)]);
    finish(p, core, prior, until, end)
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
) -> Node {
    let end = end.max(start);
    let reaching = within(readings, reach(p, start), end);
    let observed = p.timing == Timing::Observed;
    let ready: Vec<&Reading> = reaching
        .iter()
        .filter(|(t, _)| !observed || p.ready_at(*t) <= now)
        .collect();
    let mut n = Node::empty(p.kind, start, end);
    credit_intervals(p, &mut n, ready, start, end);
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

#[cfg(test)]
mod tests {
    use kvik_model::oracle;
    use kvik_model::summary::Params;
    use kvik_model::types::{Kind, SECOND, Value};
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
            let all: Vec<Reading> = ts
                .iter()
                .zip(&values)
                .map(|(t, v)| {
                    let v = match kind {
                        Kind::State => Value::Str(format!("s{v}")),
                        _ => Value::Num(f64::from(*v) * 1.5),
                    };
                    (t * SECOND, v)
                })
                .filter(|(t, _)| p.interval.is_none_or(|iv| iv.on_phase(*t)))
                .collect();
            let (start, end, now) = (start * SECOND, (start + len) * SECOND, now * SECOND);
            let want = oracle::window(&p, &all, start, end, now);
            prop_assert_eq!(window(&p, before(&all, start), &all, start, end, now), want.clone());
            // Only the part a caller must pass.
            let from = reach(&p, start);
            prop_assert_eq!(window(&p, before(&all, start), within(&all, from, end), start, end, now), want);
        }
    }
}
