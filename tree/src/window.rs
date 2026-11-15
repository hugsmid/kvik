//! What a window adds to the merge of the nodes inside it (spec §6): edge stretches, and the pro rata credit of an
//! interval series.

use crate::exact::{exact, exact_ns};
use crate::node::{Body, Node, Params, Value};

/// Adds the edge stretches (spec §6 Edge stretches) to `core`, the merge over `[core.start, until)`, `until` being
/// the window end already clipped to the credit limit; `prior` is the last reading before the start. With no
/// reading inside but a valid held value, `last` becomes that value.
pub fn with_stretches(
    p: &Params,
    mut core: Node,
    prior: Option<&(i64, Value)>,
    until: i64,
) -> Node {
    let start = core.start;
    if let Some(pr) = prior {
        let lead_to = core.first.as_ref().map_or(until, |(t, _)| *t);
        let to = p.valid_until(pr.0, lead_to);
        core.hold(&pr.1, i128::from(to) - i128::from(start));
        if let Some(first) = core.first.clone() {
            core.step(p, pr, &first);
        } else if p.valid_until(pr.0, i64::MAX) > start {
            core.last = Some(pr.clone());
        }
    }
    if core.count > 0
        && let Some(last) = core.last.clone()
    {
        let to = p.valid_until(last.0, until);
        core.hold(&last.1, i128::from(to) - i128::from(last.0));
    }
    core
}

/// Credits the readings of an interval series to the window `[start, end)` held by `node` (spec §6 Aggregates):
/// each reading's amount pro rata to the part of its interval inside, exactly; coverage is the length of those
/// parts, and a part shorter than d sets `apportioned`. Count, first, last, min and max are over the readings whose
/// `t` lies in the window. The caller leaves out readings not yet ready.
///
/// # Panics
/// Panics when `p` is not an interval series or `node` is not an interval node.
pub fn credit_intervals<'a, I>(p: &Params, node: &mut Node, readings: I, start: i64, end: i64)
where
    I: IntoIterator<Item = &'a (i64, Value)>,
{
    let d = p.interval.expect("interval series");
    for (t, v) in readings {
        let overlap = i128::from(t.saturating_add(d).min(end)) - i128::from((*t).max(start));
        let inside = start <= *t && *t < end;
        if inside {
            node.note(*t, v);
        }
        if overlap <= 0 {
            continue;
        }
        let Body::Interval {
            sum, apportioned, ..
        } = &mut node.body
        else {
            panic!("interval node");
        };
        let x = v.as_f64().expect("interval value");
        *sum += exact(x) * exact_ns(overlap) / exact_ns(i128::from(d));
        *apportioned |= overlap < i128::from(d);
        node.coverage += overlap;
    }
}
