//! [`History`] over facts in memory, by the model's rules (spec §2.2, §2.3; Q-1200), for the tests.

use std::collections::BTreeMap;

use kvik_model::types::{Nanos, Position, SeriesId};

use super::{FactItem, Held, History, HistoryError, SeriesState};

/// Facts in position order with their batches' commit times and whether each is outranked.
#[derive(Clone, Debug, Default)]
pub(crate) struct Replay {
    facts: Vec<(Nanos, Position, FactItem, bool)>,
    ranks: BTreeMap<(SeriesId, Nanos), (u64, Position)>,
    /// Each series' names from each definition entry on, in position order.
    names: BTreeMap<SeriesId, Vec<(Position, Vec<String>)>>,
}

fn held(position: Position, f: &FactItem) -> Held {
    Held {
        t: f.t,
        position,
        value: f.value.clone(),
        usable: f.usable(),
        changed_at: f.changed_at,
    }
}

fn live_and_ahead(
    current: &BTreeMap<Nanos, Held>,
    now: Nanos,
) -> (Option<Held>, BTreeMap<Nanos, Held>) {
    let live = current
        .range(..=now)
        .rev()
        .map(|(_, h)| h)
        .find(|h| h.usable)
        .cloned();
    let ahead = match now.checked_add(1) {
        Some(next) => current
            .range(next..)
            .map(|(t, h)| (*t, h.clone()))
            .collect(),
        None => BTreeMap::new(),
    };
    (live, ahead)
}

impl Replay {
    /// Appends a fact and says whether a fact already appended at its `(series, t)` outranks it.
    pub fn push(&mut self, commit: Nanos, position: Position, fact: FactItem) -> bool {
        let r = (fact.changed_at.unwrap_or(0), position);
        let key = (fact.series, fact.t);
        let outranked = self.ranks.get(&key).is_some_and(|c| *c > r);
        if !outranked {
            self.ranks.insert(key, r);
        }
        self.facts.push((commit, position, fact, outranked));
        outranked
    }

    /// Records a definition entry giving `series` the names `names` (none: retired).
    #[cfg(test)]
    pub fn define(&mut self, position: Position, series: SeriesId, names: Vec<String>) {
        self.names
            .entry(series)
            .or_default()
            .push((position, names));
    }

    fn of(
        &self,
        series: SeriesId,
        through: Position,
    ) -> impl Iterator<Item = &(Nanos, Position, FactItem, bool)> {
        self.facts
            .iter()
            .take_while(move |(_, p, _, _)| *p <= through)
            .filter(move |(_, _, f, _)| f.series == series)
    }
}

impl History for Replay {
    fn series(
        &self,
        series: SeriesId,
        now: Nanos,
        through: Position,
    ) -> Result<SeriesState, HistoryError> {
        let mut current: BTreeMap<Nanos, Held> = BTreeMap::new();
        let mut arrivals: Vec<Nanos> = Vec::new();
        for (commit, position, f, outranked) in self.of(series, through) {
            if *outranked {
                continue;
            }
            if f.t > *commit {
                arrivals.retain(|a| *a != f.t);
            }
            if f.usable() {
                arrivals.push((*commit).max(f.t));
            }
            current.insert(f.t, held(*position, f));
        }
        let (live, ahead) = live_and_ahead(&current, now);
        Ok(SeriesState {
            live,
            last_arrival: arrivals.into_iter().filter(|a| *a <= now).max(),
            ahead,
        })
    }

    fn at_look_back(
        &self,
        series: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Result<SeriesState, HistoryError> {
        let mut current: BTreeMap<Nanos, Held> = BTreeMap::new();
        let mut arrived = false;
        for (_, position, f, _) in self
            .of(series, through)
            .filter(|(c, _, _, o)| *c < from && !*o)
        {
            arrived |= f.usable() && f.t <= from;
            current.insert(f.t, held(*position, f));
        }
        let (live, ahead) = live_and_ahead(&current, from);
        Ok(SeriesState {
            live,
            last_arrival: arrived.then_some(from),
            ahead,
        })
    }

    fn facts_since(
        &self,
        series: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Result<Vec<(Nanos, Position, FactItem, bool)>, HistoryError> {
        Ok(self
            .of(series, through)
            .filter(|(c, _, _, _)| *c >= from)
            .cloned()
            .collect())
    }

    fn matching(&self, pattern: &str, through: Position) -> Result<Vec<SeriesId>, HistoryError> {
        Ok(self
            .names
            .iter()
            .filter(|(_, defs)| {
                defs.iter()
                    .take_while(|(p, _)| *p <= through)
                    .last()
                    .is_some_and(|(_, ns)| {
                        ns.iter()
                            .any(|n| kvik_model::gate::pattern_matches(pattern, n))
                    })
            })
            .map(|(s, _)| *s)
            .collect())
    }
}
