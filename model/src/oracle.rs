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
/// reading is held until the next reading, its staleness limit, or
/// `min(end, now)`; only the part inside the window is credited. Readings
/// after `now` are ignored. Count, sum, min, max, first and last are over
/// readings inside; counter steps and state transitions are credited to the
/// window holding the later reading. With no reading inside, `last` is the
/// value held at the window start, if valid.
pub fn window(p: &Params, all: &[(Nanos, Value)], start: Nanos, end: Nanos, now: Nanos) -> Node {
    let until = end.min(now).max(start);
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
            fresh.step(p, &seq[i - 1].0.1, v);
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
