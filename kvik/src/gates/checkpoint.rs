//! A snapshot as a cursor file's gate checkpoint holds it (`format.md` A.15, block `0x31`), and back.

use std::collections::BTreeMap;

use kvik_log::cursor::{
    Checkpoint, CheckpointFact, CheckpointInstance, CheckpointSeries, CheckpointState,
    CheckpointValue, GateId,
};
use kvik_log::writer::Position as LogPos;
use kvik_model::gate::{ConditionId, Gate, GateState, Selector, TransitionId};
use kvik_model::types::{Position, SeriesId, Value};

use super::engine::Frontier;
use super::rules::Inst;
use super::{Held, SeriesState, Snapshot};

fn pos_out(p: Position) -> LogPos {
    LogPos {
        generation: p.generation,
        sequence: p.sequence,
    }
}

fn pos_in(p: LogPos) -> Position {
    Position::new(p.generation, p.sequence)
}

/// A transition id as a cursor file holds it.
pub(crate) fn id_out(id: &TransitionId) -> GateId {
    GateId {
        at: pos_out(id.at),
        due: id.due,
        gate: id.condition.0,
        series: id.series.0,
    }
}

/// A transition id from a cursor file.
pub(crate) fn id_in(id: &GateId) -> TransitionId {
    TransitionId {
        at: pos_in(id.at),
        due: id.due,
        condition: ConditionId(id.gate),
        series: SeriesId(id.series),
    }
}

fn value_out(v: &Value) -> CheckpointValue {
    match v {
        Value::Num(x) => CheckpointValue::Num(*x),
        Value::Bool(b) => CheckpointValue::Bool(*b),
        Value::Str(s) => CheckpointValue::Text(s.clone()),
        Value::Absent => CheckpointValue::Absent,
    }
}

fn value_in(v: &CheckpointValue) -> Value {
    match v {
        CheckpointValue::Num(x) => Value::Num(*x),
        CheckpointValue::Bool(b) => Value::Bool(*b),
        CheckpointValue::Text(s) => Value::Str(s.clone()),
        CheckpointValue::Absent => Value::Absent,
    }
}

impl Snapshot {
    /// The checkpoint that holds this snapshot.
    pub fn to_checkpoint(&self) -> Checkpoint {
        Checkpoint {
            through: pos_out(self.frontier.at),
            now: self.frontier.now,
            instances: self
                .instances
                .iter()
                .map(|((g, s), i)| CheckpointInstance {
                    gate: g.0,
                    series: s.0,
                    state: match &i.state {
                        GateState::Unknown => CheckpointState::Unknown,
                        GateState::Stale => CheckpointState::Stale,
                        GateState::Level(n) => CheckpointState::Level(*n),
                        GateState::Match(b) => CheckpointState::Match(*b),
                        GateState::Value(v) => CheckpointState::Value(value_out(v)),
                    },
                    set_by: id_out(&i.set_by),
                    crossed: i.crossed,
                    pending: i.pending.clone(),
                    reference: i.reference.as_ref().map(value_out),
                    last_eval: i.last_eval,
                })
                .collect(),
            series: self
                .series
                .iter()
                .map(|(s, st)| CheckpointSeries {
                    series: s.0,
                    live: st.live.as_ref().map(|h| CheckpointFact {
                        t: h.t,
                        position: pos_out(h.position),
                        value: value_out(&h.value),
                        usable: h.usable,
                        changed_at: h.changed_at,
                    }),
                    last_arrival: st.last_arrival,
                })
                .collect(),
        }
    }

    /// The snapshot a checkpoint holds.
    pub fn from_checkpoint(c: &Checkpoint) -> Snapshot {
        let state_in = |s: &CheckpointState| match s {
            CheckpointState::Unknown => GateState::Unknown,
            CheckpointState::Stale => GateState::Stale,
            CheckpointState::Level(n) => GateState::Level(*n),
            CheckpointState::Match(b) => GateState::Match(*b),
            CheckpointState::Value(v) => GateState::Value(value_in(v)),
        };
        Snapshot {
            series: c
                .series
                .iter()
                .map(|s| {
                    let live = s.live.as_ref().map(|f| Held {
                        t: f.t,
                        position: pos_in(f.position),
                        value: value_in(&f.value),
                        usable: f.usable,
                        changed_at: f.changed_at,
                    });
                    let st = SeriesState {
                        live,
                        last_arrival: s.last_arrival,
                        ahead: BTreeMap::new(),
                    };
                    (SeriesId(s.series), st)
                })
                .collect(),
            instances: c
                .instances
                .iter()
                .map(|i| {
                    let inst = Inst {
                        state: state_in(&i.state),
                        set_by: id_in(&i.set_by),
                        crossed: i.crossed,
                        pending: i.pending.clone(),
                        reference: i.reference.as_ref().map(value_in),
                        last_eval: i.last_eval,
                    };
                    ((ConditionId(i.gate), SeriesId(i.series)), inst)
                })
                .collect(),
            frontier: Frontier {
                at: pos_in(c.through),
                now: c.now,
            },
        }
    }

    /// An empty core's snapshot at a frontier: the resume point of a consumer whose gates all came after it.
    pub fn empty(frontier: Frontier) -> Snapshot {
        Snapshot {
            series: BTreeMap::new(),
            instances: BTreeMap::new(),
            frontier,
        }
    }

    /// Keeps the instances whose gate is in `defs` and reads its own series (any, for `Each`), and the series they read
    /// (G9); the one-series gates of `defs` without an instance are returned for a reseed. Returns the count dropped, with those gates.
    pub fn retain_in_force(
        &mut self,
        defs: &BTreeMap<ConditionId, Gate>,
    ) -> (usize, Vec<ConditionId>) {
        let before = self.instances.len();
        // An `Each` gate's instances are the series it held at the checkpoint; it may hold none.
        self.instances.retain(|(g, s), _| {
            defs.get(g).is_some_and(|gate| match gate.selector {
                Selector::Series(x) => x == *s,
                Selector::Each(_) => true,
            })
        });
        let dropped = before - self.instances.len();
        let read: std::collections::BTreeSet<SeriesId> =
            self.instances.keys().map(|(_, s)| *s).collect();
        self.series.retain(|s, _| read.contains(s));
        let missing = defs
            .iter()
            .filter(|(_, gate)| matches!(gate.selector, Selector::Series(_)))
            .map(|(g, _)| g)
            .filter(|g| !self.instances.keys().any(|(h, _)| h == *g))
            .copied()
            .collect();
        (dropped, missing)
    }
}
