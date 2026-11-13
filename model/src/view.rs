//! The three views over a set of facts (§2.3), computed directly by scanning.
//!
//! These functions are the oracles: they take any collection of facts, in any
//! order, possibly containing the same fact more than once (for example when a
//! compacted file and its input segment are both readable), and compute the view
//! from first principles.

use std::collections::BTreeMap;

use crate::types::{Fact, Nanos, Position, SeriesId};

/// Current view: for each `(series, t)`, the fact with the highest position.
pub fn current<'a, I>(facts: I) -> BTreeMap<(SeriesId, Nanos), Fact>
where
    I: IntoIterator<Item = &'a Fact>,
{
    let mut out: BTreeMap<(SeriesId, Nanos), Fact> = BTreeMap::new();
    for f in facts {
        let key = (f.series, f.t);
        match out.get(&key) {
            Some(existing) if existing.position >= f.position => {}
            _ => {
                out.insert(key, f.clone());
            }
        }
    }
    out
}

/// As-of view by position: the current view over facts with position ≤ `p`.
pub fn as_of_position<'a, I>(facts: I, p: Position) -> BTreeMap<(SeriesId, Nanos), Fact>
where
    I: IntoIterator<Item = &'a Fact>,
{
    current(facts.into_iter().filter(|f| f.position <= p))
}

/// As-of view by arrival time: the current view over facts with
/// `received_at ≤ at`. The specification defines as-of time through a
/// monotonic per-batch commit time instead; this filter differs from it when
/// the host clock went backwards.
pub fn as_of_received<'a, I>(facts: I, at: Nanos) -> BTreeMap<(SeriesId, Nanos), Fact>
where
    I: IntoIterator<Item = &'a Fact>,
{
    current(facts.into_iter().filter(|f| f.received_at <= at))
}

/// History: all distinct facts for `(series, t)` in position order.
pub fn history<'a, I>(facts: I, series: SeriesId, t: Nanos) -> Vec<Fact>
where
    I: IntoIterator<Item = &'a Fact>,
{
    let mut by_pos: BTreeMap<Position, Fact> = BTreeMap::new();
    for f in facts {
        if f.series == series && f.t == t {
            by_pos.insert(f.position, f.clone());
        }
    }
    by_pos.into_values().collect()
}

/// Revisions: facts that are not current.
pub fn revisions<'a, I>(facts: I) -> Vec<Fact>
where
    I: IntoIterator<Item = &'a Fact> + Clone,
{
    let cur = current(facts.clone());
    let mut seen = std::collections::BTreeSet::new();
    facts
        .into_iter()
        .filter(|f| cur[&(f.series, f.t)].position != f.position)
        .filter(|f| seen.insert(f.position))
        .cloned()
        .collect()
}

/// The current, summarised (non-bad) facts of one series, in event-time order.
pub fn summarised_series(
    current: &BTreeMap<(SeriesId, Nanos), Fact>,
    series: SeriesId,
) -> Vec<Fact> {
    current
        .range((series, Nanos::MIN)..=(series, Nanos::MAX))
        .map(|(_, f)| f)
        .filter(|f| f.summarised())
        .cloned()
        .collect()
}
