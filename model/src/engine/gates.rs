//! Gates as routing entries (spec §6 Gates): watch and unwatch, the gates in force, and the log as
//! [`gate::judge`](crate::gate::judge) reads it.

use std::collections::BTreeMap;

use super::{ConsumerId, EntryBody, Error, Model, RouteChange, params_of};
use crate::gate::{self, ConditionId, Gate};
use crate::registry::{Registry, RegistryEvent};

impl Model {
    /// Watches a gate on a consumer: a routing entry that takes effect at its position.
    pub fn watch_gate(
        &mut self,
        consumer: ConsumerId,
        id: ConditionId,
        name: String,
        gate: Gate,
    ) -> Result<crate::types::Position, Error> {
        gate.validate()
            .map_err(|_| Error::BadRoute("gate refused"))?;
        if self.gates(consumer)?.contains_key(&id) {
            return Err(Error::BadRoute("gate already in force"));
        }
        self.commit_route(RouteChange::Watch {
            consumer,
            id,
            name,
            gate,
        })
    }

    /// Unwatches a gate: a routing entry that takes effect at its position.
    pub fn unwatch_gate(
        &mut self,
        consumer: ConsumerId,
        id: ConditionId,
    ) -> Result<crate::types::Position, Error> {
        if !self.gates(consumer)?.contains_key(&id) {
            return Err(Error::BadRoute("gate not in force"));
        }
        self.commit_route(RouteChange::Unwatch { consumer, id })
    }

    /// The gates of a consumer in force now, with their names.
    pub fn gates(
        &self,
        consumer: ConsumerId,
    ) -> Result<BTreeMap<ConditionId, (String, Gate)>, Error> {
        if !self.consumers.iter().any(|c| c.id == consumer) {
            return Err(Error::UnknownConsumer(consumer));
        }
        let mut out = BTreeMap::new();
        for change in self.vol()?.routes.values() {
            match change {
                RouteChange::Watch {
                    consumer: c,
                    id,
                    name,
                    gate,
                } if *c == consumer => {
                    out.insert(*id, (name.clone(), gate.clone()));
                }
                RouteChange::Unwatch { consumer: c, id } if *c == consumer => {
                    out.remove(id);
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// The whole log as one consumer's gates read it: facts, definitions and name changes, that consumer's
    /// watches and unwatches, and every other entry as [`gate::Entry::Other`].
    pub fn gate_history(&self, consumer: ConsumerId) -> Result<Vec<gate::Entry>, Error> {
        let v = self.vol()?;
        let resets: BTreeMap<crate::types::Position, ()> = self
            .reset_points()
            .into_iter()
            .map(|r| (r.id, ()))
            .collect();
        let mut pending = resets.keys().copied().peekable();
        let mut registry = Registry::default();
        let mut out: Vec<gate::Entry> = Vec::with_capacity(v.entries.len());
        let mut last: Option<(crate::types::Position, crate::types::Nanos)> = None;
        for (p, e) in &v.entries {
            let (position, commit_time) = (*p, e.received_at);
            // A reset point with no entry of its own (a lost range's last position) goes before the next entry, at
            // the commit time before it; so does the start of a new generation (Q-2170).
            while let Some(r) = pending.next_if(|r| *r < position) {
                out.push(gate::Entry::Reset {
                    position: r,
                    commit_time: last.map_or(commit_time, |(_, c)| c),
                });
            }
            if let Some((lp, lc)) = last
                && lp.generation != position.generation
                && out
                    .last()
                    .is_none_or(|x| !matches!(x, gate::Entry::Reset { .. }))
            {
                out.push(gate::Entry::Reset {
                    position: crate::types::Position::new(position.generation, 0),
                    commit_time: lc,
                });
            }
            last = Some((position, commit_time));
            if pending.next_if(|r| *r == position).is_some() {
                out.push(gate::Entry::Reset {
                    position,
                    commit_time,
                });
                if let EntryBody::Registry(r) = &e.body {
                    registry.apply(r);
                }
                continue;
            }
            out.push(match &e.body {
                EntryBody::Fact(f) => gate::Entry::Fact {
                    commit_time,
                    fact: f.clone(),
                },
                EntryBody::Registry(r) => {
                    registry.apply(r);
                    match r {
                        RegistryEvent::Define { id, .. }
                        | RegistryEvent::Alias { id, .. }
                        | RegistryEvent::Retire { id, .. } => match registry.get(*id) {
                            Some(d) => gate::Entry::Define {
                                position,
                                commit_time,
                                series: d.id,
                                names: if d.retired {
                                    Vec::new()
                                } else {
                                    d.names.clone()
                                },
                                params: params_of(d),
                            },
                            None => gate::Entry::Other {
                                position,
                                commit_time,
                            },
                        },
                        _ => gate::Entry::Other {
                            position,
                            commit_time,
                        },
                    }
                }
                EntryBody::Route(RouteChange::Watch {
                    consumer: c,
                    id,
                    gate,
                    ..
                }) if *c == consumer => gate::Entry::Watch {
                    position,
                    commit_time,
                    id: *id,
                    gate: gate.clone(),
                },
                EntryBody::Route(RouteChange::Unwatch { consumer: c, id }) if *c == consumer => {
                    gate::Entry::Unwatch {
                        position,
                        commit_time,
                        id: *id,
                    }
                }
                EntryBody::Route(_) | EntryBody::Recovery(_) => gate::Entry::Other {
                    position,
                    commit_time,
                },
            });
        }
        if let Some((_, c)) = last {
            out.extend(pending.map(|r| gate::Entry::Reset {
                position: r,
                commit_time: c,
            }));
        }
        Ok(out)
    }
}
