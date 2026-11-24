//! The judgement of gates over a history on the commit clock (spec §6 Gates, Judgement).

use std::collections::BTreeMap;

use super::{
    Cause, ConditionId, Entry, Gate, GateError, GateState, Input, Kind, MAX_INSTANCES, Selector,
    TimerKind, Transition, TransitionId, pattern_matches,
};
use crate::summary::{AggValue, Params};
use crate::types::{Fact, Kind as SeriesKind, Nanos, Position, Quality, SeriesId, Value};

/// Every transition of a history, and each instance's state at its end.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Judgement {
    /// Transitions in delivery order ([`TransitionId`] order).
    pub transitions: Vec<Transition>,
    /// Each instance in force at the end: its state and the id of the transition that set it.
    pub states: BTreeMap<(ConditionId, SeriesId), (GateState, TransitionId)>,
    /// Series an `Each` could not take beyond [`MAX_INSTANCES`], counted once each time one starts to
    /// match while the gate is full (Q-1143).
    pub instances_refused: u64,
}

/// Judges every gate the history watches, firing timers due at or before `until` after its last entry.
/// Timers due at or before an entry's commit time fire before it (Q-1144).
pub fn judge(history: &[Entry], until: Nanos) -> Result<Judgement, GateError> {
    let mut j = Judge::default();
    let mut last: Option<(Position, Nanos)> = None;
    for e in history {
        if let Some((p, c)) = last
            && (e.position() <= p || e.commit_time() < c)
        {
            return Err(GateError::Order(e.position()));
        }
        if let Some((p, _)) = last {
            j.run_timers(e.commit_time(), p);
        }
        j.apply(e)?;
        last = Some((e.position(), e.commit_time()));
    }
    if let Some((p, c)) = last {
        j.run_timers(until.max(c), p);
    }
    j.out.transitions.sort_by_key(|t| t.id);
    for w in j.gates.values() {
        for (s, i) in &w.instances {
            j.out
                .states
                .insert((w.id, *s), (i.reported.clone(), i.set_by));
        }
    }
    Ok(j.out)
}

struct SeriesData {
    current: BTreeMap<Nanos, Fact>,
    arrivals: Vec<Nanos>,
    names: Vec<String>,
    params: Params,
}

impl Default for SeriesData {
    fn default() -> Self {
        SeriesData {
            current: BTreeMap::new(),
            arrivals: Vec::new(),
            names: Vec::new(),
            params: Params::new(SeriesKind::Gauge, None),
        }
    }
}

impl SeriesData {
    fn last_arrival(&self, now: Nanos) -> Option<Nanos> {
        self.arrivals.iter().copied().filter(|a| *a <= now).max()
    }

    /// Live state (spec §2.3): the current fact with the greatest `t ≤ now` among those not bad.
    fn live(&self, now: Nanos) -> Option<&Fact> {
        self.current
            .range(..=now)
            .rev()
            .map(|(_, f)| f)
            .find(|f| usable(f))
    }
}

/// A fact that is neither bad quality nor without a value.
fn usable(f: &Fact) -> bool {
    f.quality != Quality::Bad && !matches!(f.value, Value::Absent)
}

struct Instance {
    on: Vec<bool>,
    pending: Vec<Option<Nanos>>,
    reference: Option<Value>,
    reported: GateState,
    set_by: TransitionId,
    last_eval: Nanos,
}

struct Watched {
    id: ConditionId,
    gate: Gate,
    instances: BTreeMap<SeriesId, Instance>,
    refused: std::collections::BTreeSet<SeriesId>,
}

#[derive(Default)]
struct Judge {
    series: BTreeMap<SeriesId, SeriesData>,
    gates: BTreeMap<ConditionId, Watched>,
    out: Judgement,
}

impl Judge {
    fn apply(&mut self, e: &Entry) -> Result<(), GateError> {
        let (at, now) = (e.position(), e.commit_time());
        match e {
            Entry::Fact { fact, .. } => {
                let sd = self.series.entry(fact.series).or_default();
                let outranked = sd
                    .current
                    .get(&fact.t)
                    .is_some_and(|c| c.rank() > fact.rank());
                if !outranked {
                    sd.current.insert(fact.t, fact.clone());
                    // A bad or valueless fact refreshes nothing (spec §2.5, §5.4); a future one counts
                    // once it takes effect (spec §4 Future facts).
                    if usable(fact) {
                        sd.arrivals.push(now.max(fact.t));
                    }
                }
                let cause = Cause::Fact {
                    position: at,
                    t: fact.t,
                };
                let ids: Vec<ConditionId> = self.gates.keys().copied().collect();
                for id in ids {
                    self.evaluate(id, fact.series, now, at, None, cause.clone());
                }
            }
            Entry::Define {
                series,
                names,
                params,
                ..
            } => {
                let sd = self.series.entry(*series).or_default();
                sd.names = names.clone();
                sd.params = *params;
                let ids: Vec<ConditionId> = self.gates.keys().copied().collect();
                for id in ids {
                    self.reconcile(id, now, at);
                }
            }
            Entry::Watch { id, gate, .. } => {
                gate.validate()?;
                if self.gates.contains_key(id) {
                    return Err(GateError::Id(*id));
                }
                self.gates.insert(
                    *id,
                    Watched {
                        id: *id,
                        gate: gate.clone(),
                        instances: BTreeMap::new(),
                        refused: Default::default(),
                    },
                );
                self.reconcile(*id, now, at);
            }
            Entry::Unwatch { id, .. } => {
                self.gates.remove(id).ok_or(GateError::Id(*id))?;
            }
        }
        Ok(())
    }

    /// Brings a gate's instances in line with the series its selector names: joins start at the entry,
    /// leaves end there.
    fn reconcile(&mut self, id: ConditionId, now: Nanos, at: Position) {
        if let Selector::Series(s) = &self.gates[&id].gate.selector {
            self.series.entry(*s).or_default();
        }
        let w = &self.gates[&id];
        let wanted: Vec<SeriesId> = match &w.gate.selector {
            Selector::Series(s) => vec![*s],
            Selector::Each(p) => self
                .series
                .iter()
                .filter(|(_, sd)| sd.names.iter().any(|n| pattern_matches(p, n)))
                .map(|(s, _)| *s)
                .collect(),
        };
        let leaving: Vec<SeriesId> = w
            .instances
            .keys()
            .filter(|s| !wanted.contains(s))
            .copied()
            .collect();
        let cause = Cause::Entry { position: at };
        self.gates
            .get_mut(&id)
            .expect("watched")
            .refused
            .retain(|s| wanted.contains(s));
        for s in leaving {
            let w = self.gates.get_mut(&id).expect("watched");
            let i = w.instances.remove(&s).expect("instance");
            self.out.transitions.push(Transition {
                id: TransitionId {
                    at,
                    due: None,
                    condition: id,
                    series: s,
                },
                from: Some(i.reported),
                to: None,
                cause: cause.clone(),
                value: None,
                event_time: None,
                now,
            });
        }
        for s in wanted {
            let w = self.gates.get_mut(&id).expect("watched");
            if w.instances.contains_key(&s) {
                continue;
            }
            if w.instances.len() >= MAX_INSTANCES {
                if w.refused.insert(s) {
                    self.out.instances_refused += 1;
                }
                continue;
            }
            w.refused.remove(&s);
            let n = match &w.gate.kind {
                Kind::Levels(ls) => ls.len(),
                Kind::Set { .. } => 1,
                Kind::Change { .. } => 0,
            };
            w.instances.insert(
                s,
                Instance {
                    on: vec![false; n],
                    pending: vec![None; n],
                    reference: None,
                    reported: GateState::Unknown,
                    set_by: TransitionId {
                        at,
                        due: None,
                        condition: id,
                        series: s,
                    },
                    last_eval: now,
                },
            );
            self.evaluate_start(id, s, now, at, cause.clone());
        }
    }

    fn evaluate_start(
        &mut self,
        id: ConditionId,
        s: SeriesId,
        now: Nanos,
        at: Position,
        cause: Cause,
    ) {
        let before = self.out.transitions.len();
        let changed = self.evaluate(id, s, now, at, None, cause.clone());
        if !changed {
            let i = &self.gates[&id].instances[&s];
            let (value, event_time) = input_value(&self.gates[&id].gate, self.data(s), now);
            self.out.transitions.push(Transition {
                id: i.set_by,
                from: None,
                to: Some(i.reported.clone()),
                cause,
                value,
                event_time,
                now,
            });
        } else {
            self.out.transitions[before].from = None;
        }
    }

    /// Every instance's series has data: a watch by id creates it, an `Each` selects only defined ones.
    fn data(&self, s: SeriesId) -> &SeriesData {
        &self.series[&s]
    }

    /// Fires every timer due at or before `limit`, in due order, then gate and series.
    fn run_timers(&mut self, limit: Nanos, at: Position) {
        loop {
            let mut next: Option<(Nanos, TimerKind, ConditionId, SeriesId)> = None;
            for w in self.gates.values() {
                for (s, i) in &w.instances {
                    if let Some((due, kind)) = next_due(&w.gate, i, self.data(*s))
                        && due <= limit
                        && next.is_none_or(|(d, _, _, _)| due < d)
                    {
                        next = Some((due, kind, w.id, *s));
                    }
                }
            }
            let Some((due, kind, id, s)) = next else {
                return;
            };
            self.evaluate(id, s, due, at, Some(due), Cause::Timer { due, kind });
        }
    }

    /// Evaluates one instance at `now`, recording a transition when its state changes.
    fn evaluate(
        &mut self,
        id: ConditionId,
        s: SeriesId,
        now: Nanos,
        at: Position,
        due: Option<Nanos>,
        cause: Cause,
    ) -> bool {
        let Some(w) = self.gates.get(&id) else {
            return false;
        };
        if !w.instances.contains_key(&s) {
            return false;
        }
        let sd = self.data(s);
        let stale = w.gate.stale_after.is_some_and(|lim| {
            sd.last_arrival(now)
                .is_some_and(|a| now.saturating_sub(a) >= lim)
        });
        let (value, event_time) = input_value(&w.gate, sd, now);
        let gate = w.gate.clone();
        let i = self
            .gates
            .get_mut(&id)
            .and_then(|w| w.instances.get_mut(&s))
            .expect("instance");
        i.last_eval = now;
        let next = match (&value, stale) {
            (_, true) => {
                i.pending.iter_mut().for_each(|p| *p = None);
                GateState::Stale
            }
            (None, false) => {
                i.pending.iter_mut().for_each(|p| *p = None);
                GateState::Unknown
            }
            (Some(v), false) => step(&gate.kind, i, v, now),
        };
        if next == i.reported {
            return false;
        }
        let tid = TransitionId {
            at,
            due,
            condition: id,
            series: s,
        };
        let from = std::mem::replace(&mut i.reported, next.clone());
        i.set_by = tid;
        self.out.transitions.push(Transition {
            id: tid,
            from: Some(from),
            to: Some(next),
            cause,
            value,
            event_time,
            now,
        });
        true
    }
}

/// Moves the boundaries for value `v` at `now` and returns the state they give.
fn step(kind: &Kind, i: &mut Instance, v: &Value, now: Nanos) -> GateState {
    match kind {
        Kind::Levels(ls) => {
            let Some(x) = v.as_f64() else {
                i.pending.iter_mut().for_each(|p| *p = None);
                return GateState::Unknown;
            };
            for (k, l) in ls.iter().enumerate() {
                let toward = if i.on[k] { x < l.clear } else { x > l.at };
                let h = hold(kind, k, i.on[k]);
                cross(&mut i.on[k], &mut i.pending[k], toward, h, now);
            }
            // The level counts boundaries from the lowest while each is crossed (spec §6 Gates, Levels).
            let level = i.on.iter().take_while(|on| **on).count();
            GateState::Level(u8::try_from(level).expect("at most 16 levels"))
        }
        Kind::Set { members, .. } => {
            let inside = members.iter().any(|m| m.identical(v));
            let toward = inside != i.on[0];
            let h = hold(kind, 0, i.on[0]);
            cross(&mut i.on[0], &mut i.pending[0], toward, h, now);
            GateState::Match(i.on[0])
        }
        Kind::Change { deadband } => {
            let moved = match (&i.reference, v) {
                (None, _) => true,
                (Some(Value::Num(r)), Value::Num(x)) => (x - r).abs() > *deadband,
                (Some(r), x) => !r.identical(x),
            };
            if moved {
                i.reference = Some(v.clone());
            }
            GateState::Value(i.reference.clone().expect("reference set"))
        }
    }
}

/// The hold boundary `k` needs before it flips from `on`.
fn hold(kind: &Kind, k: usize, on: bool) -> Nanos {
    match kind {
        Kind::Levels(ls) if on => ls[k].hold_down,
        Kind::Levels(ls) => ls[k].hold_up,
        Kind::Set { hold_off, .. } if on => *hold_off,
        Kind::Set { hold_on, .. } => *hold_on,
        Kind::Change { .. } => 0,
    }
}

/// One boundary: flips once its condition has held for `hold` since it first held without a break.
fn cross(on: &mut bool, pending: &mut Option<Nanos>, toward: bool, hold: Nanos, now: Nanos) {
    if !toward {
        *pending = None;
        return;
    }
    let since = *pending.get_or_insert(now);
    if now.saturating_sub(since) >= hold {
        *on = !*on;
        *pending = None;
    }
}

/// The input's value at `now`, and the event time of the live-state fact it came from.
fn input_value(gate: &Gate, sd: &SeriesData, now: Nanos) -> (Option<Value>, Option<Nanos>) {
    match &gate.input {
        Input::Value => match sd.live(now) {
            Some(f) => (Some(f.value.clone()), Some(f.t)),
            None => (None, None),
        },
        Input::Window(w) => {
            let readings: Vec<(Nanos, Value)> = sd
                .current
                .values()
                .filter(|f| f.summarised())
                .map(|f| (f.t, f.value.clone()))
                .collect();
            let node =
                crate::oracle::window(&sd.params, &readings, now.saturating_sub(w.width), now, now);
            // Coverage below `min_coverage` gives no value (Q-1141).
            if node.coverage * 1_000 < i128::from(w.min_coverage) * i128::from(w.width) {
                return (None, None);
            }
            let v = node.aggregate(&w.aggregate).and_then(|a| match a {
                AggValue::Exact(_) => a.to_f64().map(Value::Num),
                AggValue::Value(v) => Some(v),
            });
            (v, None)
        }
    }
}

/// The instance's next deadline after its last evaluation: a hold, staleness, a future fact taking
/// effect or a window tick, and which (the first in that order when several fall together). A deadline
/// beyond the clock's range never falls due.
fn next_due(gate: &Gate, i: &Instance, sd: &SeriesData) -> Option<(Nanos, TimerKind)> {
    let after = i.last_eval;
    let mut cands: Vec<(Nanos, TimerKind)> = Vec::new();
    if let Some(lim) = gate.stale_after
        && i.reported != GateState::Stale
        && let Some(a) = sd.last_arrival(after)
    {
        cands.extend(a.checked_add(lim).map(|d| (d, TimerKind::Stale)));
    }
    cands.extend(i.pending.iter().enumerate().filter_map(|(k, p)| {
        let due = p.and_then(|p| p.checked_add(hold(&gate.kind, k, i.on[k])))?;
        Some((due, TimerKind::Hold))
    }));
    // A future fact not outranked is an arrival at its `t`, so this also schedules its taking effect.
    if let Some(a) = sd.arrivals.iter().copied().filter(|a| *a > after).min() {
        cands.push((a, TimerKind::Mature));
    }
    if let Input::Window(w) = &gate.input {
        let next = (after.div_euclid(w.cadence) + 1).checked_mul(w.cadence);
        cands.extend(next.map(|d| (d, TimerKind::Window)));
    }
    cands
        .into_iter()
        .filter(|(d, _)| *d > after)
        .min_by_key(|(d, k)| (*d, *k as u8))
}
