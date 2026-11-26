//! [`History`] over facts in memory, by the model's rules (spec §2.2, §2.3; Q-1200): the seed's
//! look-back and the differential's oracle side both read through it.

use std::collections::BTreeMap;

use kvik_model::types::{Nanos, Position, SeriesId};

use super::{FactItem, Held, History, SeriesState};

/// Facts in position order with their batches' commit times.
#[derive(Clone, Debug, Default)]
pub(crate) struct Replay {
    facts: Vec<(Nanos, Position, FactItem)>,
    ranks: BTreeMap<(SeriesId, Nanos), (u64, Position)>,
}

fn rank(f: &FactItem, p: Position) -> (u64, Position) {
    (f.changed_at.unwrap_or(0), p)
}

impl Replay {
    /// Appends a fact and says whether a fact already appended at its `(series, t)` outranks it.
    pub fn push(&mut self, commit: Nanos, position: Position, fact: FactItem) -> bool {
        let r = rank(&fact, position);
        let key = (fact.series, fact.t);
        let outranked = self.ranks.get(&key).is_some_and(|c| *c > r);
        if !outranked {
            self.ranks.insert(key, r);
        }
        self.facts.push((commit, position, fact));
        outranked
    }
}

impl History for Replay {
    fn series(&self, series: SeriesId, now: Nanos, through: Position) -> SeriesState {
        let mut current: BTreeMap<Nanos, Held> = BTreeMap::new();
        let mut arrivals: Vec<Nanos> = Vec::new();
        for (commit, position, f) in &self.facts {
            if *position > through {
                break;
            }
            if f.series != series {
                continue;
            }
            let r = rank(f, *position);
            if current
                .get(&f.t)
                .is_some_and(|c| (c.changed_at.unwrap_or(0), c.position) > r)
            {
                continue;
            }
            if f.t > *commit {
                arrivals.retain(|a| *a != f.t);
            }
            if f.usable() {
                arrivals.push((*commit).max(f.t));
            }
            current.insert(
                f.t,
                Held {
                    t: f.t,
                    position: *position,
                    value: f.value.clone(),
                    usable: f.usable(),
                    changed_at: f.changed_at,
                },
            );
        }
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
        SeriesState {
            live,
            last_arrival: arrivals.into_iter().filter(|a| *a <= now).max(),
            ahead,
        }
    }

    fn facts_since(
        &self,
        series: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Vec<(Nanos, Position, FactItem)> {
        self.facts
            .iter()
            .take_while(|(_, p, _)| *p <= through)
            .filter(|(c, _, f)| f.series == series && *c >= from)
            .cloned()
            .collect()
    }
}
