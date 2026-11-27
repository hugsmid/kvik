//! `Each` instances joining and leaving as definitions commit (spec §6 Gates, One stream; Q-1143), and reset points
//! (Q-2170).

use std::collections::BTreeSet;

use kvik_model::gate::{
    Cause, ConditionId, MAX_INSTANCES, Selector, Transition, TransitionId, pattern_matches,
};
use kvik_model::types::{Nanos, Position, SeriesId};

use super::rules::Inst;
use super::{Core, CoreError, History, seed};

impl Core {
    /// A definition entry at `at` giving `series` the names `names`: each `Each` gate whose pattern starts or stops
    /// matching it gains or loses its instance, and a series refused while the gate was full joins once there is room.
    pub(super) fn define(
        &mut self,
        series: SeriesId,
        names: &[String],
        at: Position,
        now: Nanos,
        history: &dyn History,
        out: &mut Vec<Transition>,
    ) -> Result<(), CoreError> {
        let each: Vec<(ConditionId, String)> = self
            .defs
            .iter()
            .filter_map(|(id, g)| match &g.selector {
                Selector::Each(p) => Some((*id, p.clone())),
                Selector::Series(_) => None,
            })
            .collect();
        // One series changes, and each gate gives it at most one transition, in gate order: already in id order.
        for (id, p) in each {
            // Instances and refusals hold every series the pattern matches (the invariant `reconcile` keeps), so only
            // this series' membership can change.
            let mut wanted: BTreeSet<SeriesId> = self.members(id).collect();
            if names.iter().any(|n| pattern_matches(&p, n)) {
                wanted.insert(series);
            } else {
                wanted.remove(&series);
            }
            self.reconcile(id, wanted.into_iter().collect(), at, now, history, out)?;
        }
        Ok(())
    }

    /// The series an `Each` gate has an instance on or refused.
    fn members(&self, id: ConditionId) -> impl Iterator<Item = SeriesId> + '_ {
        self.instances
            .range((id, SeriesId(0))..)
            .take_while(move |(k, _)| k.0 == id)
            .map(|(k, _)| k.1)
            .chain(self.refused.get(&id).into_iter().flatten().copied())
    }

    /// Brings an `Each` gate's instances in line with `wanted` (in id order) at the entry at `at`: leaves end with a
    /// transition to none, joins start with one from none, up to [`MAX_INSTANCES`]; the rest are refused, counted
    /// once each time one starts to match while the gate is full (Q-1143).
    pub(super) fn reconcile(
        &mut self,
        id: ConditionId,
        wanted: Vec<SeriesId>,
        at: Position,
        now: Nanos,
        history: &dyn History,
        out: &mut Vec<Transition>,
    ) -> Result<(), CoreError> {
        let leaving: Vec<SeriesId> = self
            .members(id)
            .filter(|s| self.instances.contains_key(&(id, *s)) && !wanted.contains(s))
            .collect();
        if let Some(r) = self.refused.get_mut(&id) {
            r.retain(|s| wanted.contains(s));
        }
        let cause = Cause::Entry { position: at };
        for s in leaving {
            let i = self.drop_instance((id, s)).expect("an instance");
            out.push(Transition {
                id: TransitionId {
                    at,
                    due: None,
                    condition: id,
                    series: s,
                },
                from: Some(i.state),
                to: None,
                cause: cause.clone(),
                value: None,
                event_time: None,
                now,
            });
        }
        let gate = self.defs[&id].clone();
        for s in wanted {
            if self.instances.contains_key(&(id, s)) {
                continue;
            }
            let count = self
                .instances
                .range((id, SeriesId(0))..)
                .take_while(|(k, _)| k.0 == id)
                .count();
            let refused = self.refused.entry(id).or_default();
            if count >= MAX_INSTANCES {
                if refused.insert(s) {
                    self.instances_refused += 1;
                }
                continue;
            }
            refused.remove(&s);
            let state = if self.series.contains_key(&s) {
                None
            } else {
                Some(history.series(s, now, at)?)
            };
            let seeded = if self.seeding {
                seed::seed(&gate, id, s, now, at, history)?
            } else {
                None
            };
            self.start(id, s, state, seeded, at, now, out);
        }
        Ok(())
    }

    /// A reset point at `at`, `now` (Q-2170): every instance in force seeded again, as if starting there, with no
    /// transition. Each series' state is read again from the log, which no longer holds what the reset point voids.
    pub(super) fn reset(
        &mut self,
        at: Position,
        now: Nanos,
        history: &dyn History,
    ) -> Result<(), CoreError> {
        let series: Vec<SeriesId> = self.series.keys().copied().collect();
        for s in series {
            let st = history.series(s, now, at)?;
            self.series.insert(s, st);
        }
        let keys: Vec<(ConditionId, SeriesId)> = self.instances.keys().copied().collect();
        for (id, s) in keys {
            let start = TransitionId {
                at,
                due: None,
                condition: id,
                series: s,
            };
            let seeded = if self.seeding {
                seed::seed(&self.defs[&id], id, s, now, at, history)?
            } else {
                None
            };
            let inst = match seeded {
                Some(i) => Inst { set_by: start, ..i },
                None => Inst::fresh(start, now),
            };
            self.instances.insert((id, s), inst);
            let _ = self.evaluate(id, s, now, at, None, Cause::Entry { position: at });
        }
        Ok(())
    }

    /// Series refused by full `Each` gates since the last call (Q-1143), for the store's count.
    pub fn take_refused(&mut self) -> u64 {
        std::mem::take(&mut self.instances_refused)
    }
}
