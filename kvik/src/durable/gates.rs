//! Gates on the durable backend (spec §6 Gates): `watch` and `unwatch` are routing changes 4 and 5 (`format.md`
//! A.6) and the routing view keeps the gates in force. Nothing judges them yet, so `watch` writes its entry only
//! when [`LIVE`] (or a test) allows it, and refuses `NotSupportedYet` otherwise.

use std::sync::atomic::Ordering;

use kvik_log::entries::{
    GateKind as EntryKind, GateLevel, GateSelector, GateSpec, GateValue, RouteGate, RouteOp,
    Routing,
};

use super::registry::Registry;
use super::routing::View;
use super::{Writer, lock};
use crate::backend::{Error, convert};
use crate::gate::{
    Gate, GateError, GateId, GateKind, Input, Level, MAX_GATES_PER_CONSUMER, MAX_GATES_PER_STORE,
    check_name,
};
use crate::series::SeriesKey;
use crate::types::{SeriesId, Value};

/// Whether durable gates are judged; turned on with the live engine (Q-1583).
pub(crate) const LIVE: bool = false;

/// The gate as its watch entry carries it, its series resolved now (`docs/api.md` Gates, Inputs).
pub(crate) fn entry_spec(registry: &Registry, name: &str, gate: &Gate) -> Result<GateSpec, Error> {
    let selector = match (&gate.each, &gate.input.series) {
        (Some(p), None) => GateSelector::Each(p.clone()),
        (None, Some(SeriesKey::Id(id))) => {
            registry
                .get(id.0)
                .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
            GateSelector::One(id.0)
        }
        (None, Some(SeriesKey::Name(n))) => GateSelector::One(
            registry
                .resolve(n)
                .ok_or_else(|| Error::UnknownSeries(n.clone()))?
                .def
                .id,
        ),
        _ => return Err(Error::Gate(GateError::Selector)),
    };
    if gate.input.window.is_some() {
        return Err(Error::NotSupportedYet(
            "a window input to a gate on the durable backend (no incremental window fold yet)",
        ));
    }
    let ns = convert::dur_nanos;
    let kind = match &gate.kind {
        GateKind::Levels(ls) => EntryKind::Levels(
            ls.iter()
                .map(|l| {
                    Ok(GateLevel {
                        at: l.at,
                        clear: l.clear,
                        hold_up: ns(l.hold_up)?,
                        hold_down: ns(l.hold_down)?,
                    })
                })
                .collect::<Result<_, Error>>()?,
        ),
        GateKind::Set {
            members,
            hold_on,
            hold_off,
        } => EntryKind::Set {
            members: members
                .iter()
                .map(|m| {
                    Ok(match m {
                        Value::F64(x) => GateValue::F64(*x),
                        Value::Decimal { units, scale } => GateValue::Decimal {
                            units: *units,
                            scale: *scale,
                        },
                        Value::State(i) => GateValue::State(u32::from(*i)),
                        Value::Bool(b) => GateValue::Bool(*b),
                        Value::Text(t) => GateValue::Text(t.clone()),
                        Value::Absent => return Err(Error::Gate(GateError::Set)),
                    })
                })
                .collect::<Result<_, Error>>()?,
            hold_on: ns(*hold_on)?,
            hold_off: ns(*hold_off)?,
        },
        GateKind::Change { deadband } => EntryKind::Change {
            deadband: *deadband,
        },
    };
    Ok(GateSpec {
        name: name.to_string(),
        selector,
        input: 1,
        kind,
        stale_after: gate.stale_after.map(ns).transpose()?,
    })
}

/// The gate a watch entry carries, as the interface shows it (series by id).
pub(crate) fn gate_out(spec: &GateSpec) -> Gate {
    let d = |n: i64| convert::dur(i128::from(n));
    let (input, each) = match &spec.selector {
        GateSelector::One(id) => (Input::value(SeriesId(*id)), None),
        GateSelector::Each(p) => (Input::value_each(), Some(p.clone())),
    };
    let kind = match &spec.kind {
        EntryKind::Levels(ls) => GateKind::Levels(
            ls.iter()
                .map(|l| {
                    Level::above(l.at)
                        .clear(l.clear)
                        .hold_up(d(l.hold_up))
                        .hold_down(d(l.hold_down))
                })
                .collect(),
        ),
        EntryKind::Set {
            members,
            hold_on,
            hold_off,
        } => GateKind::Set {
            members: members
                .iter()
                .map(|m| match m {
                    GateValue::F64(x) => Value::F64(*x),
                    GateValue::Decimal { units, scale } => Value::Decimal {
                        units: *units,
                        scale: *scale,
                    },
                    // Field 74 holds an index below 65,536 (Q-1810).
                    GateValue::State(i) => Value::State(*i as u16),
                    GateValue::Bool(b) => Value::Bool(*b),
                    GateValue::Text(t) => Value::Text(t.clone()),
                })
                .collect(),
            hold_on: d(*hold_on),
            hold_off: d(*hold_off),
        },
        EntryKind::Change { deadband } => GateKind::Change {
            deadband: *deadband,
        },
    };
    let mut g = match kind {
        GateKind::Levels(ls) => Gate::levels(input, ls),
        GateKind::Set {
            members,
            hold_on,
            hold_off,
        } => Gate::set(input, members)
            .hold_on(hold_on)
            .hold_off(hold_off),
        GateKind::Change { deadband } => Gate::change(input).deadband(deadband),
    };
    if let Some(n) = spec.stale_after {
        g = g.stale_after(d(n));
    }
    if let Some(p) = each {
        g = g.each(p);
    }
    g
}

/// The id of one more gate on `consumer`, within the limits (`docs/api.md` Gates, Limits).
fn admit(view: &View, consumer: &str) -> Result<u32, Error> {
    let routes = view.consumers.get(consumer);
    let total: usize = view.consumers.values().map(|r| r.gates.len()).sum();
    if routes.map_or(0, |r| r.gates.len()) >= MAX_GATES_PER_CONSUMER || total >= MAX_GATES_PER_STORE
    {
        return Err(Error::Gate(GateError::TooMany));
    }
    routes
        .map_or(0, |r| r.max_gate)
        .checked_add(1)
        .ok_or(Error::Gate(GateError::TooMany))
}

impl Writer {
    /// Lets `watch` write its entry before gates are judged.
    #[cfg(test)]
    pub(crate) fn allow_gates(&self) {
        self.gates_live.store(true, Ordering::SeqCst);
    }

    /// Watches a gate: routing change 4, at its position. An equal spec under the name is the gate in force and
    /// writes nothing (Q-91 (c)).
    pub(crate) fn watch(&self, consumer: &str, name: &str, gate: &Gate) -> Result<GateId, Error> {
        check_name(name).map_err(Error::Gate)?;
        gate.check().map_err(Error::Gate)?;
        let hub = self.hub()?.clone();
        self.writable()?;
        let mut d = lock(&self.decider);
        let change = hub.run(|dl, c| -> Result<Result<Routing, GateId>, Error> {
            dl.pump(c);
            if !dl.is_bound(consumer) {
                return Err(Error::Closed);
            }
            let view = dl.view();
            let spec = entry_spec(&view.registry, name, gate)?;
            let routes = view.consumers.get(consumer);
            if let Some((id, g)) =
                routes.and_then(|r| r.gates.iter().find(|(_, g)| g.spec.name == name))
            {
                return if g.spec == spec {
                    Ok(Err(GateId(*id)))
                } else {
                    Err(Error::Gate(GateError::NameInUse))
                };
            }
            let id = admit(view, consumer)?;
            Ok(Ok(Routing {
                consumer: consumer.to_string(),
                op: RouteOp::Watch,
                funnel: None,
                selection: None,
                operator: None,
                emits: view.emits(consumer),
                condition: None,
                gate: Some(Box::new(RouteGate {
                    id,
                    spec: Some(spec),
                })),
            }))
        })?;
        let change = match change {
            Ok(c) => c,
            Err(existing) => return Ok(existing),
        };
        if !self.gates_live.load(Ordering::SeqCst) {
            return Err(Error::NotSupportedYet(
                "gates on the durable backend (the engine that judges them is not built yet)",
            ));
        }
        let id = change.gate.as_ref().map_or(0, |g| g.id);
        self.commit_routing(&mut d, change)?;
        drop(d);
        hub.run(|_, _| ());
        Ok(GateId(id))
    }

    /// Unwatches a gate by name: routing change 5, at its position.
    pub(crate) fn unwatch(&self, consumer: &str, name: &str) -> Result<(), Error> {
        let hub = self.hub()?.clone();
        self.writable()?;
        let mut d = lock(&self.decider);
        let change = hub.run(|dl, c| -> Result<Routing, Error> {
            dl.pump(c);
            if !dl.is_bound(consumer) {
                return Err(Error::Closed);
            }
            let view = dl.view();
            let id = view
                .consumers
                .get(consumer)
                .and_then(|r| r.gates.iter().find(|(_, g)| g.spec.name == name))
                .map(|(id, _)| *id)
                .ok_or(Error::Gate(GateError::NoSuchGate))?;
            Ok(Routing {
                consumer: consumer.to_string(),
                op: RouteOp::Unwatch,
                funnel: None,
                selection: None,
                operator: None,
                emits: view.emits(consumer),
                condition: None,
                gate: Some(Box::new(RouteGate { id, spec: None })),
            })
        })?;
        self.commit_routing(&mut d, change)?;
        drop(d);
        hub.run(|_, _| ());
        Ok(())
    }

    /// The gates in force, by id, with their names.
    pub(crate) fn gates(&self, consumer: &str) -> Result<Vec<(GateId, String, Gate)>, Error> {
        self.hub()?.run(|dl, c| {
            dl.pump(c);
            if !dl.is_bound(consumer) {
                return Err(Error::Closed);
            }
            Ok(dl
                .view()
                .consumers
                .get(consumer)
                .map(|r| {
                    r.gates
                        .iter()
                        .map(|(id, g)| (GateId(*id), g.spec.name.clone(), gate_out(&g.spec)))
                        .collect()
                })
                .unwrap_or_default())
        })
    }
}

#[cfg(test)]
mod tests;
