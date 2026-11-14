//! Direct computations used as oracles: windows and calendar days computed in
//! one pass over the current view, without the tree and without `merge`.

use std::collections::BTreeMap;

use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::summary::{Node, Params, direct};
use crate::tree::{TreeError, day_span};
use crate::types::{Fact, Nanos, SeriesId, Value};
use crate::view;

/// Summarised current readings of a series in event-time order.
pub fn readings(current: &BTreeMap<(SeriesId, Nanos), Fact>, s: SeriesId) -> Vec<(Nanos, Value)> {
    view::summarised_series(current, s)
        .into_iter()
        .map(|f| (f.t, f.value))
        .collect()
}

/// A node over `[start, end)` from its own readings only: what a stored node
/// over that span must equal (invariant 5).
pub fn node(p: &Params, all: &[(Nanos, Value)], start: Nanos, end: Nanos) -> Node {
    let inside: Vec<(Nanos, Value)> = all
        .iter()
        .filter(|(t, _)| start <= *t && *t < end)
        .cloned()
        .collect();
    direct(p, start, end, &inside)
}

/// A window `[start, end)` evaluated at `now`, by definition (§6): every
/// reading is held until the next reading, its staleness limit, or the
/// credit limit (`min(end, now)` for an observed series, `end` for a
/// scheduled or forecast one); only the part inside the window is credited.
/// Readings after the credit limit are ignored, and a window wholly after
/// `now` on an observed series is empty. Count, sum, min, max, first and last are over
/// readings inside; counter steps and state transitions are credited to the
/// window holding the later reading. With no reading inside, `last` is the
/// value held at the window start, if valid.
pub fn window(p: &Params, all: &[(Nanos, Value)], start: Nanos, end: Nanos, now: Nanos) -> Node {
    if p.timing == crate::registry::Timing::Observed && start > now {
        return Node::empty(p.kind, start, end);
    }
    let until = match p.timing {
        crate::registry::Timing::Observed => end.min(now),
        _ => end,
    }
    .max(start);
    let prior = all.iter().rev().find(|(t, _)| *t < start);
    let inside: Vec<&(Nanos, Value)> = all
        .iter()
        .filter(|(t, _)| start <= *t && *t < until)
        .collect();
    let mut seq: Vec<(&(Nanos, Value), bool)> = Vec::new();
    if let Some(pr) = prior {
        seq.push((pr, false));
    }
    seq.extend(inside.iter().map(|r| (*r, true)));
    let mut fresh = Node::empty(p.kind, start, end);
    for i in 0..seq.len() {
        let ((t, v), is_inside) = seq[i];
        if is_inside {
            fresh.observe(*t, v);
        }
        let next = seq.get(i + 1).map_or(until, |((nt, _), _)| *nt);
        let from = (*t).max(start);
        let to = p.valid_until(*t, next);
        fresh.hold(v, i128::from(to) - i128::from(from));
        if i > 0 && is_inside {
            fresh.step(p, seq[i - 1].0, seq[i].0);
        }
    }
    if inside.is_empty()
        && let Some(pr) = prior
        && p.valid_until(pr.0, Nanos::MAX) > start
    {
        fresh.last = Some(pr.clone());
    }
    fresh
}

/// A calendar day computed directly: the readings of the day only, as the
/// stored day node is defined (§5.7).
pub fn day(
    p: &Params,
    all: &[(Nanos, Value)],
    zone: &TimeZone,
    date: Date,
) -> Result<Node, TreeError> {
    let (s, e) = day_span(zone, date)?;
    Ok(node(p, all, s, e))
}

/// Rate over a counter window `[start, end)` at `now`, by its definition
/// (§6): the increase credited to the window divided by the elapsed seconds
/// between the readings that bound its steps (the last reading before the
/// start, when a reading lies inside, and the last reading inside), and
/// whether any of those steps spans more than S. `None` when no step falls
/// in the window.
pub fn rate(
    p: &Params,
    all: &[(Nanos, Value)],
    start: Nanos,
    end: Nanos,
    now: Nanos,
) -> Option<(crate::types::Exact, bool)> {
    use crate::summary::Body;
    use crate::types::{SECOND, exact_ns};
    if p.timing == crate::registry::Timing::Observed && start > now {
        return None;
    }
    let until = match p.timing {
        crate::registry::Timing::Observed => end.min(now),
        _ => end,
    }
    .max(start);
    let inside: Vec<Nanos> = all
        .iter()
        .map(|(t, _)| *t)
        .filter(|t| start <= *t && *t < until)
        .collect();
    let prior = all.iter().rev().map(|(t, _)| *t).find(|t| *t < start);
    let mut bounds: Vec<Nanos> = prior.filter(|_| !inside.is_empty()).into_iter().collect();
    bounds.extend(&inside);
    if bounds.len() < 2 {
        return None;
    }
    let elapsed = bounds[bounds.len() - 1] - bounds[0];
    let long = bounds
        .windows(2)
        .any(|w| p.staleness.is_some_and(|s| w[1] - w[0] > s));
    let Body::Counter { increase, .. } = window(p, all, start, end, now).body else {
        return None;
    };
    Some((
        increase * exact_ns(i128::from(SECOND)) / exact_ns(i128::from(elapsed)),
        long,
    ))
}
