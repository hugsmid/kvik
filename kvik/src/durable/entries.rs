//! Appending entries that are not facts: definitions, aliases, retirements, parameter changes and site-zone
//! changes (`docs/api.md` Series, `spec.md` §2.1).

use super::*;

impl Writer {
    /// Queues one entry and waits for its acknowledgement, holding the decision lock throughout, so no later
    /// decision sees an entry that is not yet durable. Applies it to the registry once acknowledged.
    pub(super) fn commit_entry(&self, d: &mut Decider, entry: Entry) -> Result<LogAck, Error> {
        self.writable()?;
        let pending = {
            let log = self.log();
            let log = log.as_ref().ok_or(Error::Closed)?;
            log.submit(vec![Item::from(entry.clone())])
                .map_err(|e| self.submit_error(e))?
        };
        let ack = pending.wait().map_err(|e| match e {
            CommitError::ShutDown => Error::Closed,
            CommitError::ReadOnly => Error::ReadOnly("the store is read-only".into()),
            CommitError::OutcomeUnknown => Error::SyncFailed,
        })?;
        d.registry.apply(&entry);
        // Routing sees the entry now, at its position; consumers waiting are woken once the decision lock is
        // released, by the next call or the `kvik-deliver` thread.
        if let Some(h) = &self.hub {
            h.run_quiet(|dl, c| dl.pump(c));
        }
        Ok(ack)
    }

    fn submit_error(&self, e: SubmitError) -> Error {
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

    // ----- series -----

    pub(crate) fn define(&self, spec: &SeriesSpec) -> Result<SeriesId, Error> {
        self.writable()?;
        let lateness = self.limits.default_lateness_bound;
        let mut d = lock(&self.decider);
        if let Some(s) = d.registry.resolve(&spec.name) {
            let new = registry::definition_of(spec, s.def.id, lateness)?;
            return match registry::difference(&new, &s.def) {
                None => Ok(SeriesId(s.def.id)),
                Some(field) => Err(Error::Conflict(field)),
            };
        }
        // Validate before an id is taken.
        registry::definition_of(spec, 1, lateness)?;
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
        self.commit_entry(&mut d, Entry::Definition(Box::new(def)))?;
        Ok(SeriesId(id))
    }

    fn live(&self, d: &Decider, id: SeriesId) -> Result<(), Error> {
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
        self.writable()?;
        let mut d = lock(&self.decider);
        self.live(&d, id)?;
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
        self.commit_entry(
            &mut d,
            Entry::Alias {
                id: id.0,
                name: name.into(),
            },
        )
        .map(|_| ())
    }

    pub(crate) fn retire(&self, id: SeriesId, t_r: Timestamp) -> Result<(), Error> {
        self.writable()?;
        let mut d = lock(&self.decider);
        let s = d
            .registry
            .get(id.0)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
        // Retiring a retired series changes nothing, as in the model.
        if s.retired() {
            return Ok(());
        }
        self.commit_entry(
            &mut d,
            Entry::Retirement {
                id: id.0,
                t_r: t_r.0,
            },
        )
        .map(|_| ())
    }

    pub(crate) fn set_parameters(
        &self,
        id: SeriesId,
        change: &ParameterChange,
    ) -> Result<(), Error> {
        self.writable()?;
        let mut d = lock(&self.decider);
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
            return Ok(());
        };
        let (class, period) = match convert::retention(r)? {
            kvik_model::registry::RetentionClass::LongTerm => (0, None),
            kvik_model::registry::RetentionClass::Diagnostic { period } => (1, Some(period)),
        };
        self.commit_entry(
            &mut d,
            Entry::ParameterChange(LogChange {
                id: id.0,
                retention_class: Some(class),
                retention_period: period,
                ..LogChange::default()
            }),
        )
        .map(|_| ())
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
        let mut d = lock(&self.decider);
        self.commit_entry(
            &mut d,
            Entry::SiteZone {
                zone: z.into(),
                effective: effective.0,
            },
        )
        .map(|_| ())
    }
}
