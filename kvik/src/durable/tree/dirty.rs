//! The dirty set (design 7.1b section 5 Commit path): the `(series, quarter-hour)` pairs whose nodes a checkpoint
//! must bring up to its K, and the series whose nodes a registry entry changed.
//!
//! A call's pairs are marked before its facts are offered to the log, so every fact at or below a position the
//! checkpoint takes as K after taking the set had its pair marked first. A pair stays while a call that marked it is
//! unsettled or its last acknowledged position is above the K of the checkpoint that cleared it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::writer::Position as LogPos;
use kvik_tree::set::QUARTER;

use super::super::lock;
use super::super::registry::Registry;

/// A dirty `(series, quarter-hour start)`.
pub(crate) type Key = (u32, i64);

/// What is known of the calls that marked a pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mark {
    /// Calls that marked it and are not yet settled.
    pending: u32,
    /// The highest last position of a settled call that marked it; [`UNKNOWN`] once one's outcome is unknown.
    last: LogPos,
}

/// The last position of a call whose outcome is unknown: no checkpoint clears its pairs.
const UNKNOWN: LogPos = LogPos {
    generation: u32::MAX,
    sequence: u64::MAX,
};

#[derive(Debug, Default)]
struct State {
    keys: HashMap<Key, Mark>,
    rebuild: BTreeSet<u32>,
}

/// See the module documentation.
#[derive(Debug, Default)]
pub(crate) struct Dirty(Mutex<State>);

/// The pairs of a call's facts, each once.
pub(crate) fn keys_of(items: &[Item]) -> Vec<Key> {
    let mut v: Vec<Key> = items
        .iter()
        .filter_map(|i| match i {
            Item::Fact(r) => Some((r.series_id, r.t - r.t.rem_euclid(QUARTER))),
            Item::Entry(_) => None,
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The series whose stored nodes `entry` can change (Q-1428): a definition of an id already defined, a parameter
/// change of the expected interval, staleness limit, wrap or reset ceiling, and a retirement. Aliases, routing,
/// zone and tzdata entries, recovery entries (their losses are the global floor's, Q-1429), scale raises (decimal
/// series store no level), and the other parameters (recording, lateness, retention, precision) change no node.
pub(crate) fn node_changing(registry: &Registry, entry: &Entry) -> Option<u32> {
    match entry {
        Entry::Definition(d) => registry.get(d.id).map(|_| d.id),
        Entry::ParameterChange(c) => (c.expected_interval.is_some()
            || c.staleness.is_some()
            || c.wrap_at.is_some()
            || c.reset_ceiling_ppm.is_some())
        .then_some(c.id),
        Entry::Retirement { id, .. } => Some(*id),
        Entry::Alias { .. }
        | Entry::Routing(_)
        | Entry::SiteZone { .. }
        | Entry::Tzdata { .. }
        | Entry::Recovery(_)
        | Entry::ScaleRaise { .. }
        | Entry::Ignored { .. } => None,
    }
}

impl Dirty {
    /// Marks a call's pairs before it is offered.
    pub(crate) fn begin(&self, keys: &[Key]) {
        let mut s = lock(&self.0);
        for k in keys {
            s.keys.entry(*k).or_default().pending += 1;
        }
    }

    /// Settles a call that [`Dirty::begin`] marked: `Some(last)` committed through `last`, `None` not offered after
    /// all, `Err(())` with its outcome unknown.
    pub(crate) fn settle(&self, keys: &[Key], last: Result<Option<LogPos>, ()>) {
        let mut s = lock(&self.0);
        for k in keys {
            let Some(m) = s.keys.get_mut(k) else {
                continue;
            };
            m.pending = m.pending.saturating_sub(1);
            match last {
                Ok(Some(p)) => m.last = m.last.max(p),
                Ok(None) => {}
                Err(()) => m.last = UNKNOWN,
            }
            if m.pending == 0 && m.last == LogPos::default() {
                s.keys.remove(k);
            }
        }
    }

    /// Marks pairs a start found in the log above K, at their positions (`dirty_from_log`).
    pub(crate) fn fill(&self, found: &BTreeMap<Key, LogPos>, series: &BTreeSet<u32>) {
        let mut s = lock(&self.0);
        for (k, p) in found {
            let m = s.keys.entry(*k).or_default();
            m.last = m.last.max(*p);
        }
        s.rebuild.extend(series.iter().copied());
    }

    /// Marks a series whose nodes a registry entry changed, before the entry is offered.
    pub(crate) fn rebuild(&self, series: u32) {
        lock(&self.0).rebuild.insert(series);
    }

    /// The pairs and series marked now, in ascending order.
    pub(crate) fn take(&self) -> (Vec<Key>, BTreeSet<u32>) {
        let s = lock(&self.0);
        let mut keys: Vec<Key> = s.keys.keys().copied().collect();
        keys.sort_unstable();
        (keys, s.rebuild.clone())
    }

    /// After a checkpoint at `k` that brought `keys` up to it and recorded `series` as rebuilding: clears each pair
    /// no unsettled call marked and none settled above `k`.
    pub(crate) fn cleared(&self, keys: &[Key], series: &BTreeSet<u32>, k: LogPos) {
        let mut s = lock(&self.0);
        for key in keys {
            if s.keys
                .get(key)
                .is_some_and(|m| m.pending == 0 && m.last <= k)
            {
                s.keys.remove(key);
            }
        }
        s.rebuild.retain(|x| !series.contains(x));
    }

    /// How many pairs are marked.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        lock(&self.0).keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: u64) -> LogPos {
        LogPos {
            generation: 1,
            sequence: s,
        }
    }

    #[test]
    fn a_pair_marked_by_a_call_not_yet_settled_survives_a_checkpoint_above_it() {
        let d = Dirty::default();
        d.begin(&[(1, 0)]);
        let (keys, _) = d.take();
        d.cleared(&keys, &BTreeSet::new(), p(100));
        assert_eq!(d.len(), 1);
        d.settle(&[(1, 0)], Ok(Some(p(101))));
        d.cleared(&keys, &BTreeSet::new(), p(100));
        assert_eq!(d.len(), 1, "committed above that K");
        d.cleared(&keys, &BTreeSet::new(), p(101));
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn a_call_not_offered_after_all_leaves_nothing_and_an_unknown_outcome_is_never_cleared() {
        let d = Dirty::default();
        d.begin(&[(1, 0), (2, QUARTER)]);
        d.settle(&[(1, 0)], Ok(None));
        d.settle(&[(2, QUARTER)], Err(()));
        let (keys, _) = d.take();
        assert_eq!(keys, [(2, QUARTER)]);
        d.cleared(&keys, &BTreeSet::new(), p(u64::MAX - 1));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn a_pair_marked_again_after_the_set_was_taken_is_kept() {
        let d = Dirty::default();
        d.begin(&[(1, 0)]);
        d.settle(&[(1, 0)], Ok(Some(p(5))));
        let (keys, _) = d.take();
        d.begin(&[(1, 0)]);
        d.cleared(&keys, &BTreeSet::new(), p(5));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn keys_of_a_call_are_its_facts_quarter_hours_once_each() {
        use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
        let row = |s: u32, t: i64| {
            Item::Fact(FactRow {
                series_id: s,
                t,
                quality: Quality::Good,
                flags: FactFlags::default(),
                value: Value::F64(0),
                changed_at: None,
            })
        };
        let items = [
            row(2, 1),
            row(1, QUARTER + 5),
            row(2, QUARTER - 1),
            row(2, -1),
        ];
        assert_eq!(keys_of(&items), [(1, QUARTER), (2, -QUARTER), (2, 0)]);
    }
}
