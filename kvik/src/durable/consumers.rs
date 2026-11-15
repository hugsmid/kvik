//! The consumer calls of a durable store (`docs/api.md` Consuming): registering a consumer, routing changes as log
//! entries, deliveries, confirmations and a consumer's own emits. The work is in [`super::deliver`] and
//! [`super::routing`].

use std::sync::Arc;
use std::task::Waker;

use kvik_log::entries::{Entry, RouteOp, Routing, Selection, consumer_name_ok};

use super::deliver::Hub;
use super::routing::{add_change, emits_in, funnel_out};
use super::{Emitted, Queued, Writer, lock};
use crate::backend::Error;
use crate::consume::{Delivery, DeliveryId, Funnel, FunnelId, Select, Start};
use crate::error::EmitError;
use crate::error::Refusal;
use crate::health::ConsumerStats;
use crate::types::{Ack, Intent, Outcome, Reading};

impl Writer {
    fn hub(&self) -> Result<&Arc<Hub>, Error> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        self.hub
            .as_ref()
            .ok_or_else(|| Error::ReadOnly("the start could not read the log".into()))
    }

    /// Takes the batches acknowledged since the last call into the consumers' queues.
    pub(crate) fn deliver(&self) {
        if let Some(h) = &self.hub {
            h.run(|d, c| d.pump(c));
            h.wait_wakes();
        }
    }

    /// Runs the timers due (`spec.md` §4 Timers), after taking the batches acknowledged, and writes the cursor
    /// files that are due.
    pub(crate) fn tick(&self) {
        if let Some(h) = &self.hub {
            h.ticked();
            let r = h.run(|d, c| {
                d.pump(c);
                d.tick(c);
                d.flush_due(c)
            });
            if let Err(e) = r {
                h.failed(&e);
            }
        }
    }

    /// Runs the timers that are due, before a call is decided, as the model backend runs them; the batches and the
    /// cursor files are left to the acknowledgement and the `kvik-deliver` thread.
    pub(crate) fn run_timers(&self) {
        if let Some(h) = &self.hub {
            h.ticked();
            h.run(|d, c| d.tick(c));
        }
    }

    /// Writes the registry file again after a routing change (Q-130), so that a start knows every consumer from the
    /// registry file and the unsealed segments: up to the last synced mark, which the change's acknowledgement
    /// moved past it.
    fn write_registry_file(&self) -> Result<(), Error> {
        let Some(st) = self.log().as_ref().map(kvik_log::writer::Log::stats) else {
            return Err(Error::Closed);
        };
        let file = self
            .reader()
            .map_err(|e| Error::Internal(e.to_string()))?
            .with(|i, _| Ok(i.registry_file(st.mark_synced, st.max_series_id)))
            .map_err(|e| Error::Internal(e.to_string()))?;
        let wall = self.env.clock.wall().0;
        kvik_log::registry::write(&*self.env.fs, &self.dir, self.store_id, wall, &file).map_err(
            |e| {
                let f = super::registry_fault(&self.dir, &e);
                *lock(&self.own_fault) = Some(f);
                Error::ReadOnly(e.to_string())
            },
        )?;
        Ok(())
    }

    /// Commits a routing change and writes the registry file after it.
    fn commit_routing(&self, d: &mut super::ingest::Decider, change: Routing) -> Result<(), Error> {
        self.commit_entry(d, Entry::Routing(Box::new(change)))?;
        self.write_registry_file()
    }

    /// Writes the cursor files that are due; a failure turns the store read-only.
    fn flush(&self, r: std::io::Result<()>) -> Result<(), Error> {
        if let (Err(e), Some(h)) = (r, &self.hub) {
            h.failed(&e);
            return Err(Error::ReadOnly(e.to_string()));
        }
        Ok(())
    }

    /// A consumer by name (`docs/api.md` `Store::consumer`). A change of `emits` is a routing change, written first
    /// and refused if it closes a cycle.
    pub(crate) fn take_consumer(
        &self,
        name: &str,
        start: Start,
        emits: &Select,
    ) -> Result<(), Error> {
        let hub = self.hub()?.clone();
        if !consumer_name_ok(name) {
            return Err(Error::Route(
                "a consumer name is [a-z0-9][a-z0-9._-]{0,63} (format.md A.12)".into(),
            ));
        }
        let emits = emits_in(emits);
        let mut d = lock(&self.decider);
        let change = hub.run(|dl, c| -> Result<Option<Routing>, Error> {
            dl.pump(c);
            if dl.is_bound(name) {
                return Err(Error::ConsumerInUse(name.to_string()));
            }
            let view = dl.view();
            let known = view.consumers.contains_key(name);
            if view.emits(name) == emits || (!known && emits == Selection::None) {
                return Ok(None);
            }
            let change = Routing {
                consumer: name.to_string(),
                op: RouteOp::Emits,
                funnel: None,
                selection: None,
                operator: None,
                emits: emits.clone(),
                condition: None,
            };
            if let Some(cycle) = view.cycle_with(Some(&change), None) {
                return Err(Error::RouteCycle(cycle.join(" -> ")));
            }
            Ok(Some(change))
        })?;
        if let Some(change) = change {
            self.writable()?;
            self.commit_routing(&mut d, change)?;
        }
        drop(d);
        let r = hub.run(|dl, c| {
            dl.pump(c);
            dl.take(name, start, c)
        })?;
        self.flush(r)
    }

    /// Adds a funnel: a routing change that takes effect at its position.
    pub(crate) fn route(&self, name: &str, funnel: &Funnel) -> Result<FunnelId, Error> {
        let hub = self.hub()?.clone();
        self.writable()?;
        let mut d = lock(&self.decider);
        let change = hub.run(|dl, c| -> Result<Result<Routing, FunnelId>, Error> {
            dl.pump(c);
            if !dl.is_bound(name) {
                return Err(Error::Closed);
            }
            let view = dl.view();
            // The same definition again (a consumer that routes at every start) is the funnel in force, with its
            // latch (Q-91 (c)).
            if let Some((id, _)) = view
                .funnels(name)
                .find(|(_, f)| funnel_out(f, &view.registry) == *funnel)
            {
                return Ok(Err(FunnelId(id)));
            }
            let id = view.consumers.get(name).map_or(0, |r| r.max_funnel) + 1;
            let change = add_change(&view.registry, name, id, funnel, view.emits(name))?;
            if let Some(cycle) = view.cycle_with(Some(&change), None) {
                return Err(Error::RouteCycle(cycle.join(" -> ")));
            }
            Ok(Ok(change))
        })?;
        let change = match change {
            Ok(c) => c,
            Err(existing) => return Ok(existing),
        };
        let id = change.funnel.unwrap_or(0);
        self.commit_routing(&mut d, change)?;
        drop(d);
        hub.run(|_, _| ());
        Ok(FunnelId(id))
    }

    /// Removes a funnel: a routing change; its latch is dropped.
    pub(crate) fn unroute(&self, name: &str, funnel: FunnelId) -> Result<(), Error> {
        let hub = self.hub()?.clone();
        self.writable()?;
        let mut d = lock(&self.decider);
        let change = hub.run(|dl, c| -> Result<Routing, Error> {
            dl.pump(c);
            if !dl.is_bound(name) {
                return Err(Error::Closed);
            }
            let view = dl.view();
            if !view.funnels(name).any(|(id, _)| id == funnel.0) {
                return Err(Error::Route("no such funnel".into()));
            }
            Ok(Routing {
                consumer: name.to_string(),
                op: RouteOp::Remove,
                funnel: Some(funnel.0),
                selection: None,
                operator: None,
                emits: view.emits(name),
                condition: None,
            })
        })?;
        self.commit_routing(&mut d, change)?;
        drop(d);
        hub.run(|dl, _| dl.forget_latch(name, funnel.0));
        Ok(())
    }

    /// The funnels in force.
    pub(crate) fn routes(&self, name: &str) -> Result<Vec<(FunnelId, Funnel)>, Error> {
        self.hub()?.run(|dl, c| {
            dl.pump(c);
            dl.routes(name)
        })
    }

    /// The next delivery if one is ready; registers `waker` to wake when one may be.
    pub(crate) fn poll_delivery(
        &self,
        name: &str,
        waker: Option<&Waker>,
    ) -> Result<Option<Delivery>, Error> {
        let hub = self.hub()?;
        hub.ticked();
        let r = hub.run(|dl, c| {
            dl.pump(c);
            let r = dl.poll(name, c);
            if let (Ok(None), Some(w)) = (&r, waker) {
                dl.register(w);
            }
            r
        });
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        r
    }

    /// Confirms a delivery and those handed out before it.
    pub(crate) fn confirm(&self, name: &str, id: DeliveryId) -> Result<(), Error> {
        let r = self.hub()?.run(|dl, c| dl.confirm(name, id, c))?;
        self.flush(r)
    }

    /// The handle was dropped.
    pub(crate) fn release(&self, name: &str) {
        if let Some(h) = &self.hub {
            h.run(|dl, _| dl.release(name));
        }
    }

    /// Per-consumer statistics.
    pub(crate) fn consumer_stats(&self, name: &str) -> Option<ConsumerStats> {
        self.hub.as_ref()?.run(|dl, _| dl.stats(name))
    }

    /// Readings a consumer emits through its own handle (`spec.md` §4 Routing): a reading on a series outside its
    /// `emits` is refused [`Refusal::NotDeclared`] and counted; the others are emitted without waiting.
    pub(crate) fn consumer_emit(
        self: &Arc<Self>,
        name: &str,
        readings: &[Reading],
    ) -> Result<Emitted, EmitError> {
        let hub = self.hub().map_err(|e| e.emit(&self.dir))?.clone();
        let declared: Vec<bool> = hub
            .run(|dl, c| -> Result<Vec<bool>, Error> {
                dl.pump(c);
                if !dl.is_bound(name) {
                    return Err(Error::Closed);
                }
                Ok(readings
                    .iter()
                    .map(|r| dl.view().may_emit(name, r.series.0))
                    .collect())
            })
            .map_err(|e| e.emit(&self.dir))?;
        {
            let mut c = lock(&self.counters);
            for (r, ok) in readings.iter().zip(&declared) {
                if !ok {
                    *c.series
                        .entry(r.series.0)
                        .or_default()
                        .refused
                        .entry(Refusal::NotDeclared)
                        .or_insert(0) += 1;
                }
            }
        }
        let subset: Vec<Reading> = readings
            .iter()
            .zip(&declared)
            .filter(|(_, ok)| **ok)
            .map(|(r, _)| r.clone())
            .collect();
        let splice = move |ack: Ack| {
            let mut got = ack.outcomes.into_iter();
            Ack::new(
                declared
                    .iter()
                    .map(|ok| {
                        if *ok {
                            got.next().unwrap_or(Outcome::Refused(Refusal::NotDeclared))
                        } else {
                            Outcome::Refused(Refusal::NotDeclared)
                        }
                    })
                    .collect(),
            )
        };
        if subset.is_empty() {
            return Ok(Emitted::Ready(splice(Ack::new(Vec::new()))));
        }
        Ok(match self.emit(&subset, Intent::Normal, false)? {
            Emitted::Ready(ack) => Emitted::Ready(splice(ack)),
            Emitted::Queued(q) => {
                let Queued { completion, finish } = q;
                Emitted::Queued(Queued {
                    completion,
                    finish: Box::new(move |r| finish(r).map(splice)),
                })
            }
        })
    }
}
