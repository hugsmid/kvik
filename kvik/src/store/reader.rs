//! The [`Reader`] handle (`docs/api.md` Reader processes, Reading).

use super::*;

/// A read-only handle (`docs/api.md` Reader processes, Reading). Calls are
/// blocking and take no lock on the directory; it keeps reading after the
/// writer shuts down. Reads see only committed data: durable, up to the mark,
/// with the durable backend; in memory with the model backend. `Send + Sync`
/// and cheap to clone.
#[derive(Clone, Debug)]
pub struct Reader {
    pub(super) inner: ReaderInner,
}

#[derive(Clone, Debug)]
pub(super) enum ReaderInner {
    Model(Arc<Shared>),
    /// A reader process on the durable log.
    Process(Arc<DurableReader>),
    /// The reader of a durable store's own process, opened on first use.
    Store(Arc<durable::Writer>),
}

impl Reader {
    /// A reader on the store in `config.dir`. With the model backend the
    /// store must have been opened in this process (else
    /// [`OpenError::NotAStore`]); with the durable backend it reads the
    /// directory on `config.env`'s file system, in this process or another.
    pub fn open(config: ReaderConfig) -> Result<Reader, OpenError> {
        let inner = match config.backend {
            Backend::Model => {
                ReaderInner::Model(backend::find(&config.dir).map_err(|e| e.open(&config.dir))?)
            }
            Backend::Durable => ReaderInner::Process(Arc::new(DurableReader::open(
                &config.dir,
                &config.env,
                config.lease_refresh,
            )?)),
        };
        Ok(Reader { inner })
    }

    fn durable(&self) -> Option<Result<Arc<DurableReader>, QueryError>> {
        match &self.inner {
            ReaderInner::Model(_) => None,
            ReaderInner::Process(r) => Some(Ok(r.clone())),
            ReaderInner::Store(w) => Some(w.reader()),
        }
    }

    /// Runs `model` on the model store, or `durable` on the durable reader.
    fn on<R>(
        &self,
        model: impl FnOnce(&Shared) -> Result<R, QueryError>,
        durable: impl FnOnce(&DurableReader) -> Result<R, QueryError>,
    ) -> Result<R, QueryError> {
        match &self.inner {
            ReaderInner::Model(s) => model(s),
            _ => durable(&*self.durable().expect("a durable reader")?),
        }
    }

    /// The store's id (`format.md` A.1), as read at open: no file is read and
    /// the lease is not renewed. The model backend draws one when the
    /// store is created, from the environment's entropy.
    pub fn store_id(&self) -> [u8; 16] {
        match &self.inner {
            ReaderInner::Model(s) => s.store_id(),
            ReaderInner::Process(r) => r.store_id(),
            ReaderInner::Store(w) => w.store_id(),
        }
    }

    /// Re-reads the mark, the manifest tail and the new entries, and renews
    /// the lease.
    pub fn refresh(&self) -> Result<ReaderState, QueryError> {
        self.on(
            |s| s.with(|s| s.refresh()).map_err(|e| e.query()),
            durable::refresh,
        )
    }

    /// Live state: the latest valid value, with staleness (spec §2.3).
    pub fn live(&self, id: SeriesId) -> Result<Live, QueryError> {
        self.on(
            |s| s.with(|s| s.live(id)).map_err(|e| e.query()),
            |r| durable::live(r, id),
        )
    }

    /// A window result (spec §6).
    pub fn window(
        &self,
        id: SeriesId,
        window: Window,
        aggregate: Aggregate,
    ) -> Result<WindowResult, QueryError> {
        self.on(
            |s| {
                s.with(|s| s.window(id, &window, &aggregate))
                    .map_err(|e| e.query())
            },
            |r| durable::window(r, id, &window, &aggregate),
        )
    }

    /// A chart query: raw facts or buckets with count, min, max, mean and
    /// coverage.
    pub fn buckets(
        &self,
        id: SeriesId,
        range: impl Into<TimeRange>,
        resolution: Resolution,
    ) -> Result<ChartData, QueryError> {
        let range = range.into();
        self.on(
            |s| {
                s.with(|s| s.buckets(id, range, resolution))
                    .map_err(|e| e.query())
            },
            |r| durable::buckets(r, id, range, resolution),
        )
    }

    /// Facts with `t` in a range, in a view (spec §2.3).
    pub fn facts(
        &self,
        id: SeriesId,
        range: impl Into<TimeRange>,
        view: View,
    ) -> Result<Facts, QueryError> {
        let range = range.into();
        self.on(
            |s| s.with(|s| s.facts(id, range, view)).map_err(|e| e.query()),
            |r| durable::facts(r, id, range, view),
        )
    }

    /// The latest fact at or before `t` that a window can credit (not bad
    /// quality, not displaced), in a view, ignoring the staleness limit; for
    /// a versioned series the version the view selects.
    pub fn at(
        &self,
        id: SeriesId,
        t: Timestamp,
        view: View,
    ) -> Result<Option<FactRow>, QueryError> {
        self.on(
            |s| s.with(|s| s.at(id, t, view)).map_err(|e| e.query()),
            |r| durable::at(r, id, t, view),
        )
    }

    /// A calendar day or month result in the site zone: a window over its
    /// span (spec §5.7).
    pub fn calendar(
        &self,
        id: SeriesId,
        period: impl Into<CalendarPeriod>,
        aggregate: Aggregate,
    ) -> Result<WindowResult, QueryError> {
        let window = Window::Calendar {
            window: CalendarWindow::Period(period.into()),
            zone: None,
        };
        self.window(id, window, aggregate)
    }

    /// A calendar day or month result in a named IANA zone for the whole
    /// span (spec §5.7).
    pub fn calendar_in(
        &self,
        id: SeriesId,
        period: impl Into<CalendarPeriod>,
        zone: &ZoneName,
        aggregate: Aggregate,
    ) -> Result<WindowResult, QueryError> {
        let window = Window::Calendar {
            window: CalendarWindow::Period(period.into()),
            zone: Some(zone.clone()),
        };
        self.window(id, window, aggregate)
    }

    /// The interval of an interval series that contains `t`: `[start, end)`.
    pub fn interval(
        &self,
        id: SeriesId,
        t: Timestamp,
    ) -> Result<(Timestamp, Timestamp), QueryError> {
        self.on(
            |s| s.with(|s| s.interval(id, t)).map_err(|e| e.query()),
            |r| durable::interval(r, id, t),
        )
    }

    /// What the store knows about a series, by id or by name.
    pub fn series(&self, key: impl Into<SeriesKey>) -> Result<SeriesInfo, QueryError> {
        let key = key.into();
        self.on(
            |s| s.with(|s| s.series(key.clone())).map_err(|e| e.query()),
            |r| r.with(|i, _| i.series(&key)),
        )
    }

    /// What the files report: loss reports, not the writer's counters; loss
    /// reports that cannot be read are a reason too.
    pub fn health(&self) -> Health {
        let (losses, error) = match &self.inner {
            ReaderInner::Process(r) => match r.losses() {
                Ok(v) => (v, None),
                Err(e) => (Vec::new(), Some(e.to_string())),
            },
            ReaderInner::Store(w) => w.loss_reports(),
            ReaderInner::Model(_) => (self.losses(), None),
        };
        let mut reasons: Vec<_> = losses
            .into_iter()
            .filter(|l| !l.acknowledged && l.kind == crate::health::LossKind::DataLost)
            .map(crate::health::Reason::DataLost)
            .collect();
        if let Some(error) = error {
            reasons.push(crate::health::Reason::LossesUnknown { error });
        }
        if reasons.is_empty() {
            Health::Ok
        } else {
            Health::Degraded { reasons }
        }
    }

    /// Every loss report, for ever.
    pub fn losses(&self) -> Vec<LossReport> {
        match &self.inner {
            ReaderInner::Model(s) => s.with(|s| s.losses()),
            ReaderInner::Store(w) => w.losses(),
            ReaderInner::Process(r) => r.losses().unwrap_or_default(),
        }
    }
}
