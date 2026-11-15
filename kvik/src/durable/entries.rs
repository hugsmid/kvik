//! Calls that append an entry that is not a fact: definitions, aliases, retirements, parameter and zone changes.

use super::*;

/// What a decision under the decision lock gives a call that may append an entry: nothing to append, or the entry.
pub(super) enum Decided<T> {
    Done(T),
    Entry(Entry, T),
}

impl Writer {
    /// Decides one entry under the decision lock, queues it and waits for its acknowledgement with the lock held,
    /// so no later decision sees an entry that is not yet durable; applies it to the registry once acknowledged.
    /// While the ingest queue is full it waits for room with no lock held and decides again.
    pub(super) fn commit_entry<T>(
        &self,
        mut decide: impl FnMut(&mut Decider) -> Result<Decided<T>, Error>,
    ) -> Result<T, Error> {
        loop {
            self.writable()?;
            let mut d = lock(&self.decider);
            let (entry, out) = match decide(&mut d)? {
                Decided::Done(out) => return Ok(out),
                Decided::Entry(entry, out) => (entry, out),
            };
            let r = {
                let log = self.log();
                let log = log.as_ref().ok_or(Error::Closed)?;
                log.offer(vec![Item::Entry(entry.clone())])
            };
            let pending = match r {
                Ok(p) => p,
                Err(SubmitError::Busy(LogWait::QueueFull)) => {
                    drop(d);
                    self.wait_room(1).map_err(|e| self.submit_error(e))?;
                    continue;
                }
                Err(e) => return Err(self.submit_error(e)),
            };
            self.entry_acknowledged(&mut d, &entry, pending)?;
            return Ok(out);
        }
    }

    /// As [`Self::commit_entry`] for a caller that already holds the decision lock; a full ingest queue is waited
    /// for with the lock held.
    pub(super) fn commit_entry_held(&self, d: &mut Decider, entry: Entry) -> Result<LogAck, Error> {
        self.writable()?;
        let pending = {
            let log = self.log();
            let log = log.as_ref().ok_or(Error::Closed)?;
            log.submit(vec![Item::Entry(entry.clone())])
                .map_err(|e| self.submit_error(e))?
        };
        self.entry_acknowledged(d, &entry, pending)
    }

    fn entry_acknowledged(
        &self,
        d: &mut Decider,
        entry: &Entry,
        pending: kvik_log::writer::Pending,
    ) -> Result<LogAck, Error> {
        let ack = pending.wait().map_err(|e| match e {
            CommitError::ShutDown => Error::Closed,
            CommitError::ReadOnly => Error::ReadOnly("the store is read-only".into()),
            CommitError::OutcomeUnknown => Error::SyncFailed,
        })?;
        d.registry.apply(entry);
        // Routing sees the entry now, at its position; consumers waiting are woken once the decision lock is
        // released, by the next call or the `kvik-deliver` thread.
        if let Some(h) = &self.hub {
            h.run_quiet(|dl, c| dl.pump(c));
        }
        Ok(ack)
    }

    /// Waits for room for `len` entries in the ingest queue, holding neither a lock nor the log.
    pub(super) fn wait_room(&self, len: usize) -> Result<(), SubmitError> {
        let room = self
            .log()
            .as_ref()
            .map(Log::room)
            .ok_or(SubmitError::ShutDown)?;
        room.wait(len)
    }

    pub(super) fn submit_error(&self, e: SubmitError) -> Error {
        match e {
            SubmitError::ReadOnly => Error::ReadOnly("the store is read-only".into()),
            SubmitError::ShutDown => Error::Closed,
            SubmitError::Busy(w) => Error::Internal(format!("busy: {}", wait_out(w))),
            SubmitError::Call(c) => Error::Invalid {
                field: "entry",
                reason: c.to_string(),
            },
        }
    }

    pub(crate) fn define(&self, spec: &SeriesSpec) -> Result<SeriesId, Error> {
        let lateness = self.limits.default_lateness_bound;
        // Validate before an id is taken.
        registry::definition_of(spec, 1, lateness)?;
        self.commit_entry(|d| {
            if let Some(s) = d.registry.resolve(&spec.name) {
                let new = registry::definition_of(spec, s.def.id, lateness)?;
                return match registry::difference(&new, &s.def) {
                    None => Ok(Decided::Done(SeriesId(s.def.id))),
                    Some(field) => Err(Error::Conflict(field)),
                };
            }
            let ceiling = self.limits.series_ceiling as usize;
            if d.registry.len() >= ceiling {
                return Err(Error::Ceiling(ceiling));
            }
            let floor = self.log().as_ref().map_or(0, |l| l.stats().max_series_id);
            let id = d
                .registry
                .next_id(floor)
                .ok_or(Error::Ceiling(u32::MAX as usize))?;
            let def = registry::definition_of(spec, id, lateness)?;
            Ok(Decided::Entry(
                Entry::Definition(Box::new(def)),
                SeriesId(id),
            ))
        })
    }

    pub(super) fn live(&self, d: &Decider, id: SeriesId) -> Result<(), Error> {
        let s = d
            .registry
            .get(id.0)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
        if s.retired() {
            return Err(Error::Retired(id));
        }
        Ok(())
    }

    pub(crate) fn alias(&self, id: SeriesId, name: &str) -> Result<(), Error> {
        self.commit_entry(|d| {
            self.live(d, id)?;
            if name.is_empty() || name.len() > 255 {
                return Err(Error::Invalid {
                    field: "name",
                    reason: "1 to 255 bytes".into(),
                });
            }
            // A name the series already carries is given again: it becomes the display name, as in the model.
            if d.registry.resolve(name).is_some_and(|s| s.def.id != id.0) {
                return Err(Error::NameInUse(name.into()));
            }
            // An alias that would close a routing cycle is refused (`spec.md` §4 Routing).
            if let Some(h) = &self.hub
                && let Some(c) = h.run(|dl, c| {
                    dl.pump(c);
                    dl.view().cycle_with(None, Some((id.0, name)))
                })
            {
                return Err(Error::RouteCycle(c.join(" -> ")));
            }
            Ok(Decided::Entry(
                Entry::Alias {
                    id: id.0,
                    name: name.into(),
                },
                (),
            ))
        })
    }

    pub(crate) fn retire(&self, id: SeriesId, t_r: Timestamp) -> Result<(), Error> {
        self.commit_entry(|d| {
            let s = d
                .registry
                .get(id.0)
                .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
            // Retiring a retired series changes nothing, as in the model.
            if s.retired() {
                return Ok(Decided::Done(()));
            }
            Ok(Decided::Entry(
                Entry::Retirement {
                    id: id.0,
                    t_r: t_r.0,
                },
                (),
            ))
        })
    }

    pub(crate) fn set_parameters(
        &self,
        id: SeriesId,
        change: &ParameterChange,
    ) -> Result<(), Error> {
        self.commit_entry(|d| {
            d.registry
                .get(id.0)
                .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
            if change.expected_interval.is_some()
                || change.staleness.is_some()
                || change.lateness_bound.is_some()
                || change.counter.is_some()
            {
                return Err(Error::NotSupportedYet(
                    "parameter changes other than the retention class",
                ));
            }
            let Some(r) = change.retention else {
                return Ok(Decided::Done(()));
            };
            let (class, period) = match convert::retention(r)? {
                kvik_model::registry::RetentionClass::LongTerm => (0, None),
                kvik_model::registry::RetentionClass::Diagnostic { period } => (1, Some(period)),
            };
            Ok(Decided::Entry(
                Entry::ParameterChange(LogChange {
                    id: id.0,
                    retention_class: Some(class),
                    retention_period: period,
                    ..LogChange::default()
                }),
                (),
            ))
        })
    }

    /// Decimal series are not supported yet: a raise or a policy change names an existing series or fails.
    pub(crate) fn decimal_only(&self, id: SeriesId) -> Result<(), Error> {
        self.writable()?;
        let d = lock(&self.decider);
        d.registry
            .get(id.0)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
        Err(Error::NotSupportedYet(
            "decimal series, their scale raises and excess-precision policy",
        ))
    }

    pub(crate) fn set_site_zone(&self, zone: &ZoneName, effective: Timestamp) -> Result<(), Error> {
        self.writable()?;
        let z = zone.as_str();
        jiff::tz::TimeZone::get(z).map_err(|_| Error::UnknownZone(z.into()))?;
        if z.is_empty() || z.len() > 63 {
            return Err(Error::UnknownZone(z.into()));
        }
        self.commit_entry(|_| {
            Ok(Decided::Entry(
                Entry::SiteZone {
                    zone: z.into(),
                    effective: effective.0,
                },
                (),
            ))
        })
    }
}
