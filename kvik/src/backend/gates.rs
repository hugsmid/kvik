//! Gates on the model backend (spec §6 Gates): watches are model routing entries, and what a consumer receives
//! is what [`km::gate::judge`] computes from the log, interleaved with the model's deliveries by position.

use std::collections::BTreeMap;

use kvik_model as km;

use super::convert::{self, dur_nanos, msid, pos, sid, ts, value_out};
use super::{Error, Result, Slot};
use crate::consume::{Delivery, DeliveryFlags, DeliveryId, DeliveryKind};
use crate::gate::{
    Gate, GateError, GateId, GateInstanceState, GateKind, GateState, MAX_GATES_PER_CONSUMER,
    MAX_GATES_PER_STORE, TimerKind, Transition, TransitionCause, TransitionId, check_name,
};
use crate::series::{Kind, SeriesKey, StateValues};
use crate::types::Value;

/// One consumer's gate delivery state, kept across store restarts in this process.
#[derive(Debug, Default)]
pub(super) struct GateFeed {
    /// Every gate the consumer ever watched, by id, with its name and the spec as watched (series by id).
    specs: BTreeMap<u32, (String, Gate)>,
    /// The last transition handed out in this attachment.
    handed: Option<km::gate::TransitionId>,
    /// The last transition ever handed out: one at or before it comes again flagged `replayed`.
    seen: Option<km::gate::TransitionId>,
    /// The gate cursor: every transition at or before it is confirmed.
    confirmed: Option<km::gate::TransitionId>,
    /// A model delivery taken to compare its position with the next transition's.
    pub(super) peeked: Option<km::engine::Envelope>,
}

impl GateFeed {
    /// A new attachment: unconfirmed transitions come again.
    pub(super) fn reattach(&mut self) {
        self.handed = self.confirmed;
        self.peeked = None;
    }

    pub(super) fn any(&self) -> bool {
        !self.specs.is_empty()
    }
}

/// The interface's id of a model transition id.
pub(super) fn id_out(id: &km::gate::TransitionId) -> TransitionId {
    TransitionId {
        at: pos(id.at),
        due: id.due.map(ts),
        condition: GateId(id.condition.0),
        series: sid(id.series),
    }
}

fn id_in(id: &TransitionId) -> km::gate::TransitionId {
    km::gate::TransitionId {
        at: convert::mpos(id.at),
        due: id.due.map(convert::nanos),
        condition: km::gate::ConditionId(id.condition.0),
        series: msid(id.series),
    }
}

impl Slot {
    fn resolve(&self, key: &SeriesKey) -> Result<crate::types::SeriesId> {
        let registry = self.model.registry().map_err(convert::error)?;
        let found = match key {
            SeriesKey::Id(id) => registry.get(msid(*id)).map(|d| d.id),
            SeriesKey::Name(n) => registry
                .live()
                .find(|d| d.names.iter().any(|x| x == n))
                .map(|d| d.id),
        };
        found
            .map(sid)
            .ok_or_else(|| Error::UnknownSeries(format!("{key:?}")))
    }

    /// The gate as the model judges it, and as the interface shows it (series by id).
    fn model_gate(&self, gate: &Gate) -> Result<(km::gate::Gate, Gate)> {
        let mut shown = gate.clone();
        let series = match &gate.input.series {
            Some(k) => {
                let id = self.resolve(k)?;
                shown.input.series = Some(SeriesKey::Id(id));
                Some(id)
            }
            None => None,
        };
        let spec = series.and_then(|s| self.specs.get(&s));
        let selector = match (&gate.each, series) {
            (Some(p), _) => km::gate::Selector::Each(p.clone()),
            (None, Some(s)) => km::gate::Selector::Series(msid(s)),
            (None, None) => return Err(Error::Gate(GateError::Selector)),
        };
        let input = match &gate.input.window {
            None => km::gate::Input::Value,
            Some(w) => km::gate::Input::Window(km::gate::Window {
                width: dur_nanos(w.width)?,
                aggregate: convert::aggregate(&w.aggregate, spec)?,
                min_coverage: w.min_coverage,
                cadence: dur_nanos(w.cadence)?,
            }),
        };
        let kind = match &gate.kind {
            GateKind::Levels(ls) => km::gate::Kind::Levels(
                ls.iter()
                    .map(|l| {
                        Ok(km::gate::Level {
                            at: l.at,
                            clear: l.clear,
                            hold_up: dur_nanos(l.hold_up)?,
                            hold_down: dur_nanos(l.hold_down)?,
                        })
                    })
                    .collect::<Result<_>>()?,
            ),
            GateKind::Set {
                members,
                hold_on,
                hold_off,
            } => km::gate::Kind::Set {
                members: members
                    .iter()
                    .map(|m| member(m, spec))
                    .collect::<Result<_>>()?,
                hold_on: dur_nanos(*hold_on)?,
                hold_off: dur_nanos(*hold_off)?,
            },
            GateKind::Change { deadband } => km::gate::Kind::Change {
                deadband: *deadband,
            },
        };
        let stale_after = gate.stale_after.map(dur_nanos).transpose()?;
        Ok((
            km::gate::Gate {
                selector,
                input,
                kind,
                stale_after,
            },
            shown,
        ))
    }

    pub(crate) fn watch(
        &mut self,
        epoch: u64,
        name: &str,
        gate_name: &str,
        gate: Gate,
    ) -> Result<GateId> {
        let cid = self.bound(epoch, name)?;
        check_name(gate_name).map_err(Error::Gate)?;
        gate.check().map_err(Error::Gate)?;
        let (model, shown) = self.model_gate(&gate)?;
        let in_force = self.model.gates(cid).map_err(convert::error)?;
        // An equal spec under the name is the gate in force; nothing is written (Q-91 (c)).
        if let Some((id, (_, g))) = in_force.iter().find(|(_, (n, _))| n == gate_name) {
            return if *g == model {
                Ok(GateId(id.0))
            } else {
                Err(Error::Gate(GateError::NameInUse))
            };
        }
        if in_force.len() >= MAX_GATES_PER_CONSUMER {
            return Err(Error::Gate(GateError::TooMany));
        }
        let mut total = 0;
        for c in self.consumers.values() {
            total += self.model.gates(c.id).map_err(convert::error)?.len();
        }
        if total >= MAX_GATES_PER_STORE {
            return Err(Error::Gate(GateError::TooMany));
        }
        if self.model.is_read_only() {
            return Err(Error::ReadOnly("routing changes are log entries".into()));
        }
        let c = self.consumers.get_mut(name).ok_or(Error::Closed)?;
        let id = c.gates.specs.keys().next_back().map_or(1, |m| m + 1);
        self.model
            .watch_gate(cid, km::gate::ConditionId(id), gate_name.to_string(), model)
            .map_err(convert::error)?;
        let c = self.consumers.get_mut(name).ok_or(Error::Closed)?;
        c.gates.specs.insert(id, (gate_name.to_string(), shown));
        self.wake_all();
        Ok(GateId(id))
    }

    pub(crate) fn unwatch(&mut self, epoch: u64, name: &str, gate_name: &str) -> Result<()> {
        let cid = self.bound(epoch, name)?;
        let in_force = self.model.gates(cid).map_err(convert::error)?;
        let Some((id, _)) = in_force.iter().find(|(_, (n, _))| n == gate_name) else {
            return Err(Error::Gate(GateError::NoSuchGate));
        };
        if self.model.is_read_only() {
            return Err(Error::ReadOnly("routing changes are log entries".into()));
        }
        self.model.unwatch_gate(cid, *id).map_err(convert::error)?;
        self.wake_all();
        Ok(())
    }

    pub(crate) fn gates(&mut self, epoch: u64, name: &str) -> Result<Vec<(GateId, String, Gate)>> {
        let cid = self.bound(epoch, name)?;
        let in_force = self.model.gates(cid).map_err(convert::error)?;
        let specs = &self.consumers[name].gates.specs;
        Ok(in_force
            .keys()
            .filter_map(|id| {
                specs
                    .get(&id.0)
                    .map(|(n, g)| (GateId(id.0), n.clone(), g.clone()))
            })
            .collect())
    }

    /// `judge` over the consumer's log up to now; `None` when it never watched a gate.
    fn judgement(&self, name: &str) -> Result<Option<km::gate::Judgement>> {
        let c = &self.consumers[name];
        if !c.gates.any() {
            return Ok(None);
        }
        let history = self.model.gate_history(c.id).map_err(convert::error)?;
        km::gate::judge(&history, self.model.now)
            .map(Some)
            .map_err(|e| Error::Internal(format!("gate judgement: {e}")))
    }

    /// Each gate instance's state now, for a resync.
    pub(super) fn gate_states(&self, name: &str) -> Result<Vec<GateInstanceState>> {
        let Some(j) = self.judgement(name)? else {
            return Ok(Vec::new());
        };
        Ok(j.states
            .iter()
            .map(|((g, s), (state, set_by))| GateInstanceState {
                condition: GateId(g.0),
                series: sid(*s),
                state: self.state_out(state, *s),
                set_by: Some(id_out(set_by)),
            })
            .collect())
    }

    /// The next transition not yet handed out in this attachment.
    pub(super) fn next_transition(&self, name: &str) -> Result<Option<km::gate::Transition>> {
        let Some(j) = self.judgement(name)? else {
            return Ok(None);
        };
        let handed = self.consumers[name].gates.handed;
        Ok(j.transitions
            .into_iter()
            .find(|t| handed.is_none_or(|h| t.id > h)))
    }

    /// Hands out a transition.
    pub(super) fn hand_transition(&mut self, name: &str, t: km::gate::Transition) -> Delivery {
        let c = self.consumers.get_mut(name).expect("a bound consumer");
        let replayed = c.gates.seen.is_some_and(|s| t.id <= s);
        c.gates.handed = Some(t.id);
        c.gates.seen = c.gates.seen.max(Some(t.id));
        let transition = self.transition_out(name, &t);
        Delivery {
            id: DeliveryId::Transition(transition.id),
            position: transition.id.at,
            kind: DeliveryKind::Transition(transition),
            flags: DeliveryFlags {
                replayed,
                restoring: self.model.restoring(),
                ..DeliveryFlags::default()
            },
            delivered_at: ts(self.model.now),
        }
    }

    /// A model transition as the interface shows it.
    fn transition_out(&self, name: &str, t: &km::gate::Transition) -> Transition {
        let gate_name = self.consumers[name]
            .gates
            .specs
            .get(&t.id.condition.0)
            .map(|(n, _)| n.clone())
            .unwrap_or_default();
        let id = id_out(&t.id);
        let spec = self.specs.get(&sid(t.id.series));
        let cause = match t.cause.clone() {
            km::gate::Cause::Fact { position, t } => TransitionCause::Fact {
                position: pos(position),
                t: ts(t),
            },
            km::gate::Cause::Timer { due, kind } => TransitionCause::Timer {
                due: ts(due),
                kind: match kind {
                    km::gate::TimerKind::Hold => TimerKind::Hold,
                    km::gate::TimerKind::Stale => TimerKind::Stale,
                    km::gate::TimerKind::Mature => TimerKind::Mature,
                    km::gate::TimerKind::Window => TimerKind::Window,
                },
            },
            km::gate::Cause::Entry { position } => TransitionCause::Entry {
                position: pos(position),
            },
        };
        Transition {
            id,
            name: gate_name,
            series: id.series,
            from: t.from.as_ref().map(|s| self.state_out(s, t.id.series)),
            to: t.to.as_ref().map(|s| self.state_out(s, t.id.series)),
            cause,
            value: t.value.as_ref().map(|v| value_out(v, spec)),
            event_time: t.event_time.map(ts),
            now: ts(t.now),
        }
    }

    /// Confirms transitions through `id` (cumulative); one not handed out in this attachment is refused.
    pub(super) fn confirm_transition(&mut self, name: &str, id: &TransitionId) -> Result<()> {
        let id = id_in(id);
        let c = self.consumers.get_mut(name).ok_or(Error::Closed)?;
        if c.gates.handed.is_none_or(|h| id > h) {
            return Err(Error::BadConfirm);
        }
        c.gates.confirmed = c.gates.confirmed.max(Some(id));
        Ok(())
    }

    fn state_out(&self, s: &km::gate::GateState, series: km::types::SeriesId) -> GateState {
        match s {
            km::gate::GateState::Unknown => GateState::Unknown,
            km::gate::GateState::Stale => GateState::Stale,
            km::gate::GateState::Level(n) => GateState::Level(*n),
            km::gate::GateState::Match(b) => GateState::Match(*b),
            km::gate::GateState::Value(v) => {
                GateState::Value(value_out(v, self.specs.get(&sid(series))))
            }
        }
    }
}

/// Whether transition `t` goes before the model delivery `e`: before a fact at a later position, after the
/// fact at its own (spec §4 Order, §6 Identity); at or before the position of any other delivery.
pub(super) fn before(t: &km::gate::Transition, e: &km::engine::Envelope) -> bool {
    match &e.kind {
        km::engine::DeliveryKind::Fact(f) => t.id.at < f.position,
        _ => t.id.at <= e.position,
    }
}

/// A set member as the model compares it: numbers as `f64`, a named state by its name on a one-series gate.
fn member(v: &Value, spec: Option<&crate::series::SeriesSpec>) -> Result<km::types::Value> {
    use km::types::Value as V;
    Ok(match v {
        Value::F64(x) => V::Num(*x),
        Value::Decimal { units, scale } => V::Num(*units as f64 / 10f64.powi(i32::from(*scale))),
        Value::Bool(b) => V::Bool(*b),
        Value::Text(t) => V::Str(t.clone()),
        Value::State(i) => match spec.map(|s| &s.kind) {
            Some(Kind::State {
                values: StateValues::Named(names),
            }) => V::Str(
                names
                    .get(usize::from(*i))
                    .ok_or(Error::Gate(GateError::Set))?
                    .clone(),
            ),
            _ => {
                return Err(Error::NotSupportedYet(
                    "a named-state member on a gate whose series has no named states (each gates included)",
                ));
            }
        },
        Value::Absent => return Err(Error::Gate(GateError::Set)),
    })
}

#[cfg(test)]
mod tests;
