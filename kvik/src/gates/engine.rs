//! The core's state and its calls (spec §6 Gates, Judgement).

use std::collections::{BTreeMap, BTreeSet};

use kvik_model::gate::{
    Cause, ConditionId, Gate, GateState, Input, Selector, TimerKind, Transition, TransitionId,
};
use kvik_model::types::{Nanos, Position, SeriesId, Value};

use super::rules::{self, Inst};
use super::{Body, CoreError, FactItem, Held, History, Item, SeriesState, seed};

/// Last position applied and commit-clock instant reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frontier {
    /// Last position of the log applied, whatever its entry.
    pub at: Position,
    /// Commit-clock instant reached.
    pub now: Nanos,
}

/// One consumer's gates, judged incrementally.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Core {
    pub(super) defs: BTreeMap<ConditionId, Gate>,
    /// Only series an instance reads.
    pub(super) series: BTreeMap<SeriesId, SeriesState>,
    pub(super) readers: BTreeMap<SeriesId, BTreeSet<ConditionId>>,
    pub(super) instances: BTreeMap<(ConditionId, SeriesId), Inst>,
    /// One entry per instance with a deadline, in firing order.
    pub(super) timers: BTreeMap<(Nanos, ConditionId, SeriesId), TimerKind>,
    pub(super) scheduled: BTreeMap<(ConditionId, SeriesId), Nanos>,
    pub(super) frontier: Frontier,
    /// Starts seed from history; off inside a seed (Q-1390).
    pub(super) seeding: bool,
}

impl Default for Core {
    fn default() -> Self {
        Core::new()
    }
}

/// Brings a series up to `now`: facts ahead with `t ≤ now` take effect (spec §4 Future facts).
pub(super) fn bring_up(s: &mut SeriesState, now: Nanos) {
    while let Some(e) = s.ahead.first_entry() {
        if *e.key() > now {
            break;
        }
        let h = e.remove();
        if h.usable {
            s.last_arrival = s.last_arrival.max(Some(h.t));
            s.live = Some(h);
        }
    }
}

fn input(s: &SeriesState) -> (Option<Value>, Option<Nanos>) {
    match &s.live {
        Some(l) => (Some(l.value.clone()), Some(l.t)),
        None => (None, None),
    }
}

impl Core {
    /// An empty core.
    pub fn new() -> Self {
        Core {
            defs: BTreeMap::new(),
            series: BTreeMap::new(),
            readers: BTreeMap::new(),
            instances: BTreeMap::new(),
            timers: BTreeMap::new(),
            scheduled: BTreeMap::new(),
            frontier: Frontier {
                at: Position::ORIGIN,
                now: Nanos::MIN,
            },
            seeding: true,
        }
    }

    pub(super) fn unseeded() -> Self {
        Core {
            seeding: false,
            ..Core::new()
        }
    }

    /// Last position applied and instant reached.
    pub fn frontier(&self) -> Frontier {
        self.frontier
    }

    /// Each instance's state and the transition that set it.
    pub fn states(&self) -> BTreeMap<(ConditionId, SeriesId), (GateState, TransitionId)> {
        self.instances
            .iter()
            .map(|(k, i)| (*k, (i.state.clone(), i.set_by)))
            .collect()
    }

    /// Fires every timer due at or before `now` in `(due, gate, series)` order, each with the frontier's
    /// position as its `at` (the last position of the log before `due`), and raises the frontier's instant to `now`.
    pub fn advance(&mut self, now: Nanos) -> Vec<Transition> {
        let mut out = Vec::new();
        while let Some((&(due, id, s), &kind)) = self.timers.first_key_value() {
            if due > now {
                break;
            }
            let at = self.frontier.at;
            out.extend(self.evaluate(id, s, due, at, Some(due), Cause::Timer { due, kind }));
        }
        self.frontier.now = self.frontier.now.max(now);
        out
    }

    /// Judges one batch committed at `commit`: timers due at or before it first (Q-1144), then each item
    /// in position order. The frontier ends at the batch's last position, whatever its entry.
    pub fn apply_batch(
        &mut self,
        commit: Nanos,
        items: &[Item],
        history: &dyn History,
    ) -> Result<Vec<Transition>, CoreError> {
        debug_assert!(
            commit >= self.frontier.now,
            "a batch committed below the core's now"
        );
        let mut out = self.advance(commit);
        for item in items {
            debug_assert!(
                item.position > self.frontier.at || item.position == Position::ORIGIN,
                "items out of position order"
            );
            self.frontier.at = item.position;
            let at = item.position;
            match &item.body {
                Body::Fact { fact, outranked } => {
                    self.fact(fact, *outranked, at, commit, history, &mut out)?;
                }
                Body::Watch { id, gate } => self.watch(*id, gate, at, commit, history, &mut out)?,
                Body::Unwatch { id } => self.unwatch(*id)?,
                Body::Other => {}
            }
        }
        Ok(out)
    }

    fn fact(
        &mut self,
        fact: &FactItem,
        outranked: bool,
        at: Position,
        now: Nanos,
        history: &dyn History,
        out: &mut Vec<Transition>,
    ) -> Result<(), CoreError> {
        let Some(st) = self.series.get_mut(&fact.series) else {
            return Ok(());
        };
        bring_up(st, now);
        if !outranked {
            let usable = fact.usable();
            let held = Held {
                t: fact.t,
                position: at,
                value: fact.value.clone(),
                usable,
                changed_at: fact.changed_at,
            };
            if fact.t > now {
                st.ahead.insert(fact.t, held);
            } else {
                if usable {
                    st.last_arrival = st.last_arrival.max(Some(now));
                }
                let live_t = st.live.as_ref().map(|l| l.t);
                if usable && live_t.is_none_or(|t| fact.t >= t) {
                    st.live = Some(held);
                } else if !usable && live_t == Some(fact.t) {
                    // The live fact itself was replaced by one without a usable value: the live state
                    // falls back to an older fact the core does not hold.
                    st.live = history.series(fact.series, now, at)?.live;
                }
            }
        }
        let cause = Cause::Fact {
            position: at,
            t: fact.t,
        };
        let ids: Vec<ConditionId> = self.readers[&fact.series].iter().copied().collect();
        for id in ids {
            out.extend(self.evaluate(id, fact.series, now, at, None, cause.clone()));
        }
        Ok(())
    }

    fn watch(
        &mut self,
        id: ConditionId,
        gate: &Gate,
        at: Position,
        now: Nanos,
        history: &dyn History,
        out: &mut Vec<Transition>,
    ) -> Result<(), CoreError> {
        let Selector::Series(s) = gate.selector else {
            return Err(CoreError::NotSupported(id));
        };
        if !matches!(gate.input, Input::Value) {
            return Err(CoreError::NotSupported(id));
        }
        gate.validate().map_err(|e| CoreError::Invalid(id, e))?;
        if self.defs.contains_key(&id) {
            return Err(CoreError::Id(id));
        }
        // Every read before any change, so a failed read adds nothing of the gate (Q-1881).
        let state = if self.series.contains_key(&s) {
            None
        } else {
            Some(history.series(s, now, at)?)
        };
        let seeded = if self.seeding {
            seed::seed(gate, id, s, now, at, history)?
        } else {
            None
        };
        self.defs.insert(id, gate.clone());
        if let Some(st) = state {
            self.series.insert(s, st);
        }
        self.readers.entry(s).or_default().insert(id);
        let start = TransitionId {
            at,
            due: None,
            condition: id,
            series: s,
        };
        let inst = match seeded {
            Some(i) => Inst { set_by: start, ..i },
            None => Inst::fresh(start, now),
        };
        self.instances.insert((id, s), inst);
        let cause = Cause::Entry { position: at };
        match self.evaluate(id, s, now, at, None, cause.clone()) {
            Some(mut t) => {
                t.from = None;
                out.push(t);
            }
            None => {
                let (value, event_time) = input(&self.series[&s]);
                let i = &self.instances[&(id, s)];
                out.push(Transition {
                    id: i.set_by,
                    from: None,
                    to: Some(i.state.clone()),
                    cause,
                    value,
                    event_time,
                    now,
                });
            }
        }
        Ok(())
    }

    /// Starts `gates` at `at`, `now` by the seed rule without their start transitions (spec §6 Gates, Transitions;
    /// reseed): each instance's `set_by` is the start id it would have had. Every read comes first, so a failed
    /// one changes nothing.
    pub fn reseed(
        &mut self,
        gates: &[(ConditionId, Gate)],
        at: Position,
        now: Nanos,
        history: &dyn History,
    ) -> Result<(), CoreError> {
        let mut next = self.clone();
        next.frontier.at = next.frontier.at.max(at);
        next.frontier.now = next.frontier.now.max(now);
        let mut dropped = Vec::new();
        for (id, gate) in gates {
            next.watch(*id, gate, at, now, history, &mut dropped)?;
        }
        *self = next;
        Ok(())
    }

    /// The earliest timer due.
    pub fn next_due(&self) -> Option<Nanos> {
        self.timers.first_key_value().map(|(k, _)| k.0)
    }

    /// Removes a gate with its instance, timer and, when no other gate reads it, its series.
    fn unwatch(&mut self, id: ConditionId) -> Result<(), CoreError> {
        self.defs.remove(&id).ok_or(CoreError::Id(id))?;
        let keys: Vec<(ConditionId, SeriesId)> = self
            .instances
            .range((id, SeriesId(0))..)
            .take_while(|(k, _)| k.0 == id)
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            self.instances.remove(&k);
            self.unschedule(k);
            let readers = self.readers.get_mut(&k.1).expect("series read");
            readers.remove(&id);
            if readers.is_empty() {
                self.readers.remove(&k.1);
                self.series.remove(&k.1);
            }
        }
        Ok(())
    }

    fn unschedule(&mut self, k: (ConditionId, SeriesId)) {
        if let Some(due) = self.scheduled.remove(&k) {
            self.timers.remove(&(due, k.0, k.1));
        }
    }

    pub(super) fn reschedule(&mut self, k: (ConditionId, SeriesId)) {
        self.unschedule(k);
        let next = rules::next_due(&self.defs[&k.0], &self.instances[&k], &self.series[&k.1]);
        if let Some((due, kind)) = next {
            self.timers.insert((due, k.0, k.1), kind);
            self.scheduled.insert(k, due);
        }
    }

    /// Evaluates one instance at `now`; a transition when its state changes.
    fn evaluate(
        &mut self,
        id: ConditionId,
        s: SeriesId,
        now: Nanos,
        at: Position,
        due: Option<Nanos>,
        cause: Cause,
    ) -> Option<Transition> {
        let gate = self.defs.get(&id)?;
        let st = self.series.get_mut(&s)?;
        let i = self.instances.get_mut(&(id, s))?;
        bring_up(st, now);
        let stale = rules::stale(gate, st, now);
        let (value, event_time) = input(st);
        i.last_eval = now;
        let next = match &value {
            _ if stale => {
                i.pending.clear();
                GateState::Stale
            }
            None => {
                i.pending.clear();
                GateState::Unknown
            }
            Some(v) => rules::step(&gate.kind, i, v, now),
        };
        let out = (next != i.state).then(|| {
            let tid = TransitionId {
                at,
                due,
                condition: id,
                series: s,
            };
            let from = std::mem::replace(&mut i.state, next.clone());
            i.set_by = tid;
            Transition {
                id: tid,
                from: Some(from),
                to: Some(next),
                cause,
                value,
                event_time,
                now,
            }
        });
        self.reschedule((id, s));
        out
    }
}
