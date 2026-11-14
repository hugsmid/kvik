//! The model backend: the interface implemented on the reference model,
//! in memory and not durable.
//!
//! Each store directory names one in-memory store, kept in a process-wide
//! table so that a store can be shut down and opened again (a restart) and
//! read by [`Reader::open`](crate::Reader::open). Nothing is written to disk.
//!
//! The model fixes its consumers when it is created, so the backend reserves
//! [`Limits::max_consumers`](crate::Limits::max_consumers) slots, each with a
//! configured funnel that selects nothing, and binds consumer names to slots.

mod convert;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Waker;
use std::time::Instant;

use jiff::Timestamp;
use km::engine::{ConsumerDecl, ConsumerId, EmitError, Model, Selection};
use kvik_model as km;

use crate::config::StoreConfig;
use crate::consume::{
    Condition, Delivery, DeliveryFlags, DeliveryId, DeliveryKind, Funnel, FunnelId, FunnelState,
    Resync, Select, Start, Trigger,
};
use crate::env::Clock;
use crate::error::{BusyReason, Error, Refusal, Result};
use crate::health::{ConsumerStats, Health, HealthReason, SeriesStats, Stats};
use crate::read::{
    Aggregate, Bucket, CalendarPeriod, CalendarWindow, ChartData, Live, LiveValue, Resolution,
    ResultFlags, View, Window, WindowResult,
};
use crate::series::{
    IntervalLength, Kind, ParameterChange, RetentionClass, SeriesInfo, SeriesKey, SeriesSpec,
    StateValues, Timing,
};
use crate::types::{Ack, Fact, Intent, Position, Reading, SeriesId, TimeSource, Value};
use convert::{dur, dur_nanos, fact_out, mpos, mquality, msid, mwait, nanos, pos, sid, ts};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// In-memory stores of this process, by directory.
static STORES: Mutex<BTreeMap<PathBuf, Arc<Shared>>> = Mutex::new(BTreeMap::new());

/// One in-memory store, shared by its writer, readers and consumers.
#[derive(Debug)]
pub(crate) struct Shared {
    dir: PathBuf,
    state: Mutex<Slot>,
}

impl Shared {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Slot> {
        lock(&self.state)
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Finds the in-memory store of a directory, for a reader.
pub(crate) fn find(dir: &Path) -> Result<Arc<Shared>> {
    lock(&STORES)
        .get(dir)
        .cloned()
        .ok_or_else(|| Error::NotFound {
            dir: dir.to_path_buf(),
        })
}

/// Opens (creates, or restarts) the in-memory store of a directory. Returns
/// it with the store session's epoch.
pub(crate) fn open(config: &StoreConfig) -> Result<(Arc<Shared>, u64)> {
    jiff::tz::TimeZone::get(&config.time_zone)
        .map_err(|_| Error::UnknownZone(config.time_zone.clone()))?;
    let mut stores = lock(&STORES);
    if let Some(shared) = stores.get(&config.dir).cloned() {
        drop(stores);
        let epoch = shared.lock().restart(config)?;
        return Ok((shared, epoch));
    }
    let slot = Slot::create(config)?;
    let shared = Arc::new(Shared {
        dir: config.dir.clone(),
        state: Mutex::new(slot),
    });
    stores.insert(config.dir.clone(), shared.clone());
    Ok((shared, 1))
}

/// A consumer name bound to a model consumer slot.
#[derive(Debug)]
struct ConsumerSlot {
    id: ConsumerId,
    /// A handle is live.
    handle: bool,
    /// The store session in which a handle was last taken.
    attached: Option<u64>,
    funnels: BTreeMap<FunnelId, FunnelEntry>,
    /// Model funnel ids to the interface's (they differ once a pattern funnel
    /// has been re-routed).
    to_api: BTreeMap<u32, FunnelId>,
    next_funnel: u32,
}

#[derive(Debug)]
struct FunnelEntry {
    spec: Funnel,
    model: u32,
    /// For a fact funnel with a name pattern: the series it selects now.
    pattern_set: Option<BTreeSet<km::types::SeriesId>>,
}

/// The state behind a store.
#[derive(Debug)]
pub(crate) struct Slot {
    model: Model,
    /// A writer handle is open.
    open: bool,
    /// Store session, raised at every open.
    epoch: u64,
    clock: Arc<dyn Clock>,
    last_mono: Instant,
    specs: BTreeMap<SeriesId, SeriesSpec>,
    consumers: BTreeMap<String, ConsumerSlot>,
    max_consumers: usize,
    wakers: Vec<Waker>,
    /// Model monotonic time of each series' last commit of a fact that counts
    /// for live staleness (spec §5.4).
    committed_mono: BTreeMap<SeriesId, i64>,
    /// Refusals decided before a reading reaches the model.
    refused: BTreeMap<Refusal, u64>,
}

fn model_config(config: &StoreConfig) -> Result<km::engine::Config> {
    Ok(km::engine::Config {
        ingest_capacity: config.limits.ingest_queue,
        series_ceiling: config.limits.series_ceiling,
        low_mark: config.limits.low_mark,
        critical_mark: config.limits.critical_mark,
        seal_interval: dur_nanos(config.log.seal_interval)?,
        site_zone: config.time_zone.clone(),
        ..km::engine::Config::default()
    })
}

/// Whether `name` matches `pattern`: the same number of `/`-separated
/// segments, `*` matching any run of characters within a segment.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    fn segment(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) => (0..=s.len()).any(|i| segment(rest, &s[i..])),
            Some((c, rest)) => s.first() == Some(c) && segment(rest, &s[1..]),
        }
    }
    let (p, n): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), name.split('/').collect());
    p.len() == n.len()
        && p.iter()
            .zip(&n)
            .all(|(p, n)| segment(p.as_bytes(), n.as_bytes()))
}

fn without_name(spec: &SeriesSpec) -> SeriesSpec {
    SeriesSpec {
        name: String::new(),
        ..spec.clone()
    }
}

impl Slot {
    fn create(config: &StoreConfig) -> Result<Slot> {
        let clock = config.env.clock.clone();
        let wall = nanos(clock.wall())?;
        let max = config.limits.max_consumers;
        let slots = (0..max)
            .map(|i| {
                Ok(ConsumerDecl {
                    id: ConsumerId(u32::try_from(i).map_err(|_| Error::TooManyConsumers(max))?),
                    selection: Selection::Series(BTreeSet::new()),
                    capacity: config.limits.consumer_queue,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut model = Model::new(model_config(config)?, slots, wall);
        model.clock_synchronised = clock.synchronised();
        Ok(Slot {
            model,
            open: true,
            epoch: 1,
            last_mono: clock.monotonic(),
            clock,
            specs: BTreeMap::new(),
            consumers: BTreeMap::new(),
            max_consumers: max,
            wakers: Vec::new(),
            committed_mono: BTreeMap::new(),
            refused: BTreeMap::new(),
        })
    }

    fn restart(&mut self, config: &StoreConfig) -> Result<u64> {
        if self.open {
            return Err(Error::Locked {
                dir: config.dir.clone(),
            });
        }
        self.clock = config.env.clock.clone();
        self.last_mono = self.clock.monotonic();
        let ceiling = config.limits.series_ceiling;
        let cfg = &mut self.model.config;
        cfg.ingest_capacity = config.limits.ingest_queue;
        cfg.series_ceiling = ceiling;
        cfg.low_mark = config.limits.low_mark;
        cfg.critical_mark = config.limits.critical_mark;
        cfg.seal_interval = dur_nanos(config.log.seal_interval)?;
        cfg.site_zone = config.time_zone.clone();
        self.sync_clock();
        self.model.restart();
        if !self.model.is_up() {
            return Err(self.model.disk.open_refused.clone().map_or_else(
                || Error::Internal("the store did not start".into()),
                convert::error,
            ));
        }
        self.open = true;
        self.epoch += 1;
        for c in self.consumers.values_mut() {
            c.handle = false;
            c.attached = None;
        }
        Ok(self.epoch)
    }

    /// Stops the store: the model loses its volatile state, as at a crash or
    /// shutdown; everything committed stays.
    pub(crate) fn close(&mut self, epoch: u64) {
        if self.open && self.epoch == epoch {
            self.open = false;
            self.model.crash();
            self.wake_all();
        }
    }

    /// Brings the model's clocks to the injected clock's: the monotonic
    /// delta advances both, the remaining wall difference is a jump.
    fn sync_clock(&mut self) {
        let mono = self.clock.monotonic();
        let d = mono.saturating_duration_since(self.last_mono);
        self.last_mono = mono;
        let dn = i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
        if dn > 0 {
            self.model.advance(dn);
        }
        if let Ok(w) = nanos(self.clock.wall()) {
            let dw = w - self.model.wall;
            if dw != 0 {
                self.model.jump_wall(dw);
            }
        }
        self.model.clock_synchronised = self.clock.synchronised();
    }

    fn up(&mut self) -> Result<()> {
        if !self.open || !self.model.is_up() {
            return Err(Error::Closed);
        }
        self.sync_clock();
        Ok(())
    }

    fn writable(&mut self, epoch: u64) -> Result<()> {
        if self.epoch != epoch {
            return Err(Error::Closed);
        }
        self.up()?;
        if self.model.is_read_only() {
            return Err(Error::ReadOnly {
                cause: format!("{:?}", self.model.health().reasons),
            });
        }
        Ok(())
    }

    pub(crate) fn register(&mut self, w: &Waker) {
        if !self.wakers.iter().any(|x| x.will_wake(w)) {
            self.wakers.push(w.clone());
        }
    }

    fn wake_all(&mut self) {
        for w in self.wakers.drain(..) {
            w.wake();
        }
    }

    /// The model keeps every delivery it hands out as an outside observer;
    /// the backend has no use for that record, so it is dropped.
    fn forget_observations(&mut self) {
        self.model.observed.clear();
        self.model.received.clear();
        self.model.confirmed.clear();
    }

    fn spec(&self, id: SeriesId) -> Result<&SeriesSpec> {
        self.specs
            .get(&id)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))
    }

    // ----- series -----

    pub(crate) fn define(&mut self, epoch: u64, spec: SeriesSpec) -> Result<SeriesId> {
        self.writable(epoch)?;
        let d = convert::definition(&spec)?;
        let id = sid(self
            .model
            .define_with(
                &spec.name,
                d.kind,
                d.value_type,
                &spec.unit,
                d.staleness,
                d.options,
            )
            .map_err(convert::error)?);
        if let Some(existing) = self.specs.get(&id) {
            if without_name(existing) == without_name(&spec) {
                return Ok(id);
            }
            return Err(Error::NameInUse(spec.name));
        }
        if spec.retention != RetentionClass::LongTerm {
            self.model
                .set_retention(msid(id), d.retention)
                .map_err(convert::error)?;
        }
        self.specs.insert(id, spec);
        self.reroute_patterns()?;
        self.wake_all();
        Ok(id)
    }

    pub(crate) fn set_parameters(
        &mut self,
        epoch: u64,
        id: SeriesId,
        _effective_from: Timestamp,
        change: ParameterChange,
    ) -> Result<()> {
        self.writable(epoch)?;
        self.spec(id)?;
        if change.expected_interval.is_some()
            || change.staleness.is_some()
            || change.lateness_bound.is_some()
            || change.counter.is_some()
        {
            return Err(Error::NotSupportedYet {
                what: "parameter changes other than the retention class",
            });
        }
        if let Some(r) = change.retention {
            self.model
                .set_retention(msid(id), convert::retention(r)?)
                .map_err(convert::error)?;
            if let Some(s) = self.specs.get_mut(&id) {
                s.retention = r;
            }
        }
        Ok(())
    }

    pub(crate) fn alias(&mut self, epoch: u64, id: SeriesId, name: &str) -> Result<()> {
        self.writable(epoch)?;
        self.model.rename(msid(id), name).map_err(convert::error)?;
        self.reroute_patterns()
    }

    pub(crate) fn retire(&mut self, epoch: u64, id: SeriesId, t_r: Timestamp) -> Result<()> {
        self.writable(epoch)?;
        self.spec(id)?;
        self.model
            .retire(msid(id), nanos(t_r)?)
            .map_err(convert::error)
    }

    pub(crate) fn series(&mut self, key: SeriesKey) -> Result<SeriesInfo> {
        self.up()?;
        let registry = self.model.registry().map_err(convert::error)?;
        let def = match &key {
            SeriesKey::Id(id) => registry.get(msid(*id)),
            SeriesKey::Name(n) => registry.resolve(n),
        }
        .ok_or_else(|| {
            Error::UnknownSeries(match &key {
                SeriesKey::Id(id) => id.to_string(),
                SeriesKey::Name(n) => n.clone(),
            })
        })?
        .clone();
        let id = sid(def.id);
        let spec = self.spec(id)?.clone();
        let facts = self.model.facts().map_err(convert::error)?;
        let mine = || facts.iter().filter(|f| f.series == def.id);
        let latest_t = mine().map(|f| f.t).max().map(ts);
        let lateness_reference = (spec.timing == Timing::Observed)
            .then(|| {
                mine()
                    .filter(|f| !f.flags.future && !f.flags.unsynced_clock)
                    .map(|f| f.t)
                    .max()
                    .map(ts)
            })
            .flatten();
        Ok(SeriesInfo {
            id,
            name: def.names.last().cloned().unwrap_or_default(),
            names: def.names.clone(),
            spec,
            staleness: def.staleness.map(|s| dur(i128::from(s))),
            retired_at: def.retired_at.map(ts),
            latest_t,
            lateness_reference,
        })
    }

    /// The live series whose name or alias matches a pattern.
    fn resolve_pattern(&self, pattern: &str) -> Result<BTreeSet<km::types::SeriesId>> {
        Ok(self
            .model
            .registry()
            .map_err(convert::error)?
            .live()
            .filter(|d| d.names.iter().any(|n| matches_pattern(pattern, n)))
            .map(|d| d.id)
            .collect())
    }

    /// Re-evaluates the name patterns of fact funnels after a definition or
    /// alias (spec §4 Routing): a funnel whose selection changed is replaced
    /// by one with the new selection, effective at its position.
    fn reroute_patterns(&mut self) -> Result<()> {
        let mut work = Vec::new();
        for (name, c) in &self.consumers {
            for (fid, e) in &c.funnels {
                if let (Some(old), Select::Names(p)) = (&e.pattern_set, &e.spec.series) {
                    let new = self.resolve_pattern(p)?;
                    if &new != old {
                        work.push((name.clone(), *fid, new));
                    }
                }
            }
        }
        for (name, fid, set) in work {
            let c = self.consumers.get_mut(&name).expect("listed above");
            let e = c.funnels.get_mut(&fid).expect("listed above");
            let fresh = c.next_funnel;
            c.next_funnel += 1;
            self.model
                .remove_route(c.id, km::engine::FunnelId(e.model))
                .map_err(convert::error)?;
            self.model
                .add_route(
                    c.id,
                    km::engine::FunnelId(fresh),
                    km::engine::Funnel::facts(Selection::Series(set.clone())),
                )
                .map_err(convert::error)?;
            c.to_api.insert(fresh, fid);
            e.model = fresh;
            e.pattern_set = Some(set);
        }
        Ok(())
    }

    // ----- writing -----

    fn model_reading(&self, r: &Reading) -> Result<km::types::Reading, Refusal> {
        let t = nanos(r.t).map_err(|_| Refusal::TimeOutOfRange)?;
        let spec = self.specs.get(&r.series).ok_or(Refusal::UnknownSeries)?;
        use km::types::Value as V;
        let value = match (&spec.kind, &r.value) {
            (Kind::Gauge { .. } | Kind::Counter { .. } | Kind::Interval { .. }, Value::F64(x)) => {
                V::Num(*x)
            }
            (
                Kind::State {
                    values: StateValues::Bool,
                },
                Value::Bool(b),
            ) => V::Bool(*b),
            (
                Kind::State {
                    values: StateValues::Named(names),
                },
                Value::State(i),
            ) => V::Str(
                names
                    .get(usize::from(*i))
                    .ok_or(Refusal::UnknownState)?
                    .clone(),
            ),
            (Kind::Text, Value::Text(s)) => V::Str(s.clone()),
            _ => return Err(Refusal::WrongType),
        };
        Ok(km::types::Reading {
            series: msid(r.series),
            t,
            value,
            quality: mquality(r.quality),
            changed_at: r.changed_at,
            t_source: match r.source {
                TimeSource::Device => km::types::TSource::Device,
                TimeSource::Host => km::types::TSource::Host,
            },
        })
    }

    /// Emits a batch and commits it. `Ok(Err(_))` is back-pressure: nothing
    /// of the batch was accepted; the wait is counted when `count_wait`.
    pub(crate) fn emit(
        &mut self,
        epoch: u64,
        readings: &[Reading],
        intent: Intent,
        count_wait: bool,
    ) -> Result<Result<Ack, BusyReason>> {
        self.writable(epoch)?;
        let m = &mut self.model;
        // The model queues reading by reading and cannot take one back, so
        // every cause of a wait is checked before anything is queued.
        let busy = if m.free_bytes() < m.config.critical_mark {
            Some(BusyReason::DiskFull)
        } else if !m.clock_synchronised && readings.iter().any(|r| r.source == TimeSource::Host) {
            Some(BusyReason::UnsyncedClock)
        } else if m.queued() + readings.len() > m.config.ingest_capacity {
            Some(BusyReason::QueueFull)
        } else {
            None
        };
        if let Some(b) = busy {
            if count_wait {
                *m.counters.waits.entry(mwait(b)).or_insert(0) += 1;
            }
            return Ok(Err(b));
        }
        m.tick().map_err(convert::error)?;
        let mut refused = Vec::new();
        let mut keys = Vec::new();
        for (i, r) in readings.iter().enumerate() {
            let mr = match self.model_reading(r) {
                Ok(mr) => mr,
                Err(x) => {
                    *self.refused.entry(x).or_insert(0) += 1;
                    refused.push((i, x));
                    continue;
                }
            };
            let key = (mr.series, mr.t);
            let res = match intent {
                Intent::Normal => self.model.emit(mr),
                Intent::Revise => self.model.emit_revise(mr),
            };
            match res {
                Ok(()) => keys.push(key),
                Err(EmitError::Refused(x)) => match convert::refusal(&x) {
                    Some(x) => refused.push((i, x)),
                    None => return Err(Error::Internal(format!("refused mid-batch: {x}"))),
                },
                Err(EmitError::Wait(w)) => {
                    return Err(Error::Internal(format!(
                        "unexpected wait mid-batch: {}",
                        convert::wait(&w)
                    )));
                }
            }
        }
        let report = self.model.commit(None).map_err(convert::error)?;
        let recorded: BTreeSet<_> = report.positions.iter().copied().collect();
        let current = self.model.current().map_err(convert::error)?;
        for (s, t) in keys {
            if let Some(f) = current.get(&(s, t))
                && recorded.contains(&f.position)
                && f.summarised()
                && !f.flags.future
            {
                self.committed_mono.insert(sid(s), self.model.mono);
            }
        }
        self.wake_all();
        Ok(Ok(Ack {
            first: report.positions.first().copied().map(pos),
            last: report.positions.last().copied().map(pos),
            recorded: u32::try_from(report.positions.len()).unwrap_or(u32::MAX),
            duplicates_dropped: u32::try_from(report.duplicates).unwrap_or(u32::MAX),
            refused,
        }))
    }

    /// Runs the timer evaluations due by the clock now (spec §4 Timers).
    pub(crate) fn tick(&mut self, epoch: u64) -> Result<()> {
        if self.epoch != epoch {
            return Err(Error::Closed);
        }
        self.up()?;
        self.model.tick().map_err(convert::error)?;
        self.wake_all();
        Ok(())
    }

    // ----- consumers -----

    pub(crate) fn consumer(&mut self, epoch: u64, name: &str, start: Start) -> Result<()> {
        if self.epoch != epoch {
            return Err(Error::Closed);
        }
        self.up()?;
        if let Some(c) = self.consumers.get_mut(name) {
            if c.handle {
                return Err(Error::ConsumerInUse(name.to_string()));
            }
            // Taken again in the same store session: the consumer restarted;
            // it resumes from its durable cursor (spec §6 Restart and
            // resume). After a store restart the model has begun that resume
            // already.
            if c.attached == Some(self.epoch) {
                self.model.reconnect(c.id).map_err(convert::error)?;
            }
            c.attached = Some(self.epoch);
            c.handle = true;
            return Ok(());
        }
        match start {
            Start::Now => {}
            Start::Position(_) => {
                return Err(Error::NotSupportedYet {
                    what: "a new consumer starting at a position",
                });
            }
            Start::Time(_) => {
                return Err(Error::NotSupportedYet {
                    what: "a new consumer starting at a time",
                });
            }
        }
        let used: BTreeSet<ConsumerId> = self.consumers.values().map(|c| c.id).collect();
        let id = (0..self.max_consumers)
            .filter_map(|i| u32::try_from(i).ok().map(ConsumerId))
            .find(|i| !used.contains(i))
            .ok_or(Error::TooManyConsumers(self.max_consumers))?;
        // A fresh slot: drop whatever it queued while unbound and confirm up
        // to now, so that it starts after the last committed position.
        self.model.reconnect(id).map_err(convert::error)?;
        self.model
            .catch_up(id, usize::MAX)
            .map_err(convert::error)?;
        let got = self.model.receive(id, usize::MAX).map_err(convert::error)?;
        if let Some(last) = got.last() {
            self.model
                .confirm_delivery(id, last.id())
                .map_err(convert::error)?;
        }
        self.forget_observations();
        self.consumers.insert(
            name.to_string(),
            ConsumerSlot {
                id,
                handle: true,
                attached: Some(self.epoch),
                funnels: BTreeMap::new(),
                to_api: BTreeMap::new(),
                next_funnel: 1,
            },
        );
        Ok(())
    }

    fn bound(&mut self, epoch: u64, name: &str) -> Result<ConsumerId> {
        if self.epoch != epoch {
            return Err(Error::Closed);
        }
        self.up()?;
        self.consumers.get(name).map(|c| c.id).ok_or(Error::Closed)
    }

    pub(crate) fn release(&mut self, epoch: u64, name: &str) {
        if self.epoch == epoch
            && let Some(c) = self.consumers.get_mut(name)
        {
            c.handle = false;
        }
    }

    pub(crate) fn route(&mut self, epoch: u64, name: &str, funnel: Funnel) -> Result<FunnelId> {
        let cid = self.bound(epoch, name)?;
        if self.model.is_read_only() {
            return Err(Error::ReadOnly {
                cause: "routing changes are log entries".into(),
            });
        }
        let registry = self.model.registry().map_err(convert::error)?;
        let (selection, pattern_set) = match &funnel.series {
            Select::All => (Selection::All, None),
            Select::Ids(ids) => {
                let mut set = BTreeSet::new();
                for id in ids {
                    if registry.get(msid(*id)).is_none() {
                        return Err(Error::UnknownSeries(id.to_string()));
                    }
                    set.insert(msid(*id));
                }
                (Selection::Series(set), None)
            }
            Select::Names(p) => {
                let set = self.resolve_pattern(p)?;
                (Selection::Series(set.clone()), Some(set))
            }
        };
        let single = match &selection {
            Selection::Series(set) if set.len() == 1 => set.iter().next().copied(),
            _ => None,
        };
        let spec = single.and_then(|s| self.specs.get(&sid(s)));
        let condition = match &funnel.condition {
            None => None,
            Some(c) => {
                let s = single.ok_or_else(|| {
                    Error::Route("a condition funnel selects exactly one series".into())
                })?;
                Some(match c {
                    Condition::Threshold { on, clear } => {
                        if clear > on {
                            return Err(Error::Route(
                                "the clear level is above the activation level".into(),
                            ));
                        }
                        km::condition::Condition::Threshold {
                            on: *on,
                            clear: *clear,
                        }
                    }
                    Condition::OnChange { deadband } => km::condition::Condition::OnChange {
                        deadband: *deadband,
                    },
                    Condition::HeldFor { above, n } => km::condition::Condition::HeldFor {
                        above: *above,
                        n: dur_nanos(*n)?,
                    },
                    Condition::StaleFor { n } => {
                        let staleness =
                            registry.get(s).and_then(|d| d.staleness).ok_or_else(|| {
                                Error::Route("stale-for needs a staleness limit".into())
                            })?;
                        km::condition::Condition::StaleFor {
                            staleness,
                            n: dur_nanos(*n)?,
                        }
                    }
                })
            }
        };
        let operator = match &funnel.operator {
            None => None,
            Some(op) => Some(km::engine::Operator {
                aggregate: convert::aggregate(&op.aggregate, spec)?,
                width: dur_nanos(op.width)?,
            }),
        };
        let c = self.consumers.get_mut(name).ok_or(Error::Closed)?;
        let fid = c.next_funnel;
        let pattern_set = if funnel.condition.is_none() {
            pattern_set
        } else {
            None
        };
        self.model
            .add_route(
                cid,
                km::engine::FunnelId(fid),
                km::engine::Funnel {
                    selection,
                    operator,
                    condition,
                },
            )
            .map_err(convert::error)?;
        c.next_funnel += 1;
        c.to_api.insert(fid, FunnelId(fid));
        c.funnels.insert(
            FunnelId(fid),
            FunnelEntry {
                spec: funnel,
                model: fid,
                pattern_set,
            },
        );
        self.wake_all();
        Ok(FunnelId(fid))
    }

    pub(crate) fn unroute(&mut self, epoch: u64, name: &str, funnel: FunnelId) -> Result<()> {
        let cid = self.bound(epoch, name)?;
        let c = self.consumers.get_mut(name).ok_or(Error::Closed)?;
        let model = c
            .funnels
            .get(&funnel)
            .map(|e| e.model)
            .ok_or_else(|| Error::Route("no such funnel".into()))?;
        self.model
            .remove_route(cid, km::engine::FunnelId(model))
            .map_err(convert::error)?;
        c.funnels.remove(&funnel);
        Ok(())
    }

    pub(crate) fn routes(&mut self, epoch: u64, name: &str) -> Result<Vec<(FunnelId, Funnel)>> {
        self.bound(epoch, name)?;
        Ok(self.consumers[name]
            .funnels
            .iter()
            .map(|(id, e)| (*id, e.spec.clone()))
            .collect())
    }

    /// Takes the next delivery, if one is ready: runs due timers and
    /// catch-up first.
    pub(crate) fn poll_delivery(&mut self, epoch: u64, name: &str) -> Result<Option<Delivery>> {
        let cid = self.bound(epoch, name)?;
        self.model.tick().map_err(convert::error)?;
        self.model
            .catch_up(cid, usize::MAX)
            .map_err(convert::error)?;
        let mut got = self.model.receive(cid, 1).map_err(convert::error)?;
        self.forget_observations();
        Ok(got.pop().map(|e| self.delivery(name, e)))
    }

    fn api_funnel(&self, name: &str, model: km::engine::FunnelId) -> FunnelId {
        self.consumers
            .get(name)
            .and_then(|c| c.to_api.get(&model.0).copied())
            .unwrap_or(FunnelId(model.0))
    }

    fn delivery(&self, name: &str, e: km::engine::Envelope) -> Delivery {
        use km::engine::DeliveryKind as K;
        let id = match e.id() {
            km::engine::DeliveryId::Fact(p) => DeliveryId::Fact(pos(p)),
            km::engine::DeliveryId::Timer {
                generation,
                funnel,
                sequence,
            } => DeliveryId::Timer {
                generation,
                funnel: self.api_funnel(name, funnel),
                sequence,
            },
            km::engine::DeliveryId::GenerationChanged => DeliveryId::GenerationChanged,
            km::engine::DeliveryId::Resync => DeliveryId::Resync,
        };
        let mut flags = DeliveryFlags {
            replayed: e.replayed,
            outranked: e.outranked,
            restoring: self.model.restoring(),
            ..DeliveryFlags::default()
        };
        let kind = match e.kind {
            K::Fact(f) => {
                flags.future = f.flags.future;
                DeliveryKind::Fact(fact_out(&f, self.specs.get(&sid(f.series))))
            }
            K::Trigger(t) => DeliveryKind::Trigger(Trigger {
                funnel: self.api_funnel(name, t.funnel),
                state: convert::condition_state(&t.state),
                value: None,
                coverage: None,
                cause: convert::trigger_cause(t.cause),
            }),
            K::Resync(r) => DeliveryKind::Resync(Resync {
                funnels: r
                    .funnels
                    .iter()
                    .filter(|f| f.funnel != km::engine::FunnelId::CONFIGURED)
                    .map(|f| FunnelState {
                        funnel: self.api_funnel(name, f.funnel),
                        live: f
                            .live
                            .iter()
                            .map(|(s, fact)| (sid(*s), self.live_of(sid(*s), fact.as_ref())))
                            .collect(),
                        condition: f
                            .condition
                            .as_ref()
                            .map(|c| convert::condition_state(&c.state)),
                        transition: f.condition.as_ref().is_some_and(|c| c.transition),
                    })
                    .collect(),
                replaced: r.replaced.map(|(a, b)| (pos(a), pos(b))),
            }),
            K::GenerationChanged {
                cursor,
                restored_end,
            } => DeliveryKind::GenerationChanged {
                cursor: cursor.map(pos),
                restored_end: pos(restored_end),
            },
        };
        Delivery {
            id,
            position: pos(e.position),
            kind,
            flags,
            delivered_at: ts(self.model.now),
        }
    }

    pub(crate) fn confirm(&mut self, epoch: u64, name: &str, id: DeliveryId) -> Result<()> {
        let cid = self.bound(epoch, name)?;
        let mid = match id {
            DeliveryId::Fact(p) => km::engine::DeliveryId::Fact(mpos(p)),
            // Condition funnels are never re-routed: their ids are the
            // model's.
            DeliveryId::Timer {
                generation,
                funnel,
                sequence,
            } => km::engine::DeliveryId::Timer {
                generation,
                funnel: km::engine::FunnelId(funnel.0),
                sequence,
            },
            DeliveryId::GenerationChanged => km::engine::DeliveryId::GenerationChanged,
            DeliveryId::Resync => km::engine::DeliveryId::Resync,
        };
        self.model
            .confirm_delivery(cid, mid)
            .map_err(convert::error)?;
        self.forget_observations();
        Ok(())
    }

    // ----- reading -----

    fn live_of(&self, id: SeriesId, fact: Option<&km::types::Fact>) -> Live {
        let Some(f) = fact else { return Live::None };
        let spec = self.specs.get(&id);
        let def = self
            .model
            .registry()
            .ok()
            .and_then(|r| r.get(msid(id)).cloned());
        let stale = match spec.map(|s| s.timing) {
            Some(Timing::Scheduled) => self
                .model
                .scheduled_stale(msid(id))
                .ok()
                .flatten()
                .unwrap_or(false),
            Some(Timing::Forecast) | None => false,
            Some(Timing::Observed) => match def.and_then(|d| d.staleness) {
                None => false,
                // Live staleness of an observed series is measured on the
                // monotonic clock since its last commit (spec §5.4).
                Some(limit) => match self.committed_mono.get(&id) {
                    Some(m) => self.model.mono - m > limit,
                    None => self.model.now - f.t > limit,
                },
            },
        };
        Live::Value(LiveValue {
            fact: fact_out(f, spec),
            stale,
        })
    }

    pub(crate) fn live(&mut self, id: SeriesId) -> Result<Live> {
        self.up()?;
        self.spec(id)?;
        let f = self.model.latest(msid(id)).map_err(convert::error)?;
        Ok(self.live_of(id, f.as_ref()))
    }

    fn window_result(
        &self,
        id: SeriesId,
        node: &km::summary::Node,
        agg: &km::summary::Aggregate,
        span: (i64, i64),
        restoring: bool,
        summary_stale: bool,
    ) -> WindowResult {
        let (from, to) = span;
        let width = i128::from(to) - i128::from(from);
        WindowResult {
            value: node
                .aggregate(agg)
                .map(|v| convert::aggregated(&v, self.specs.get(&id))),
            count: node.count,
            coverage: dur(node.coverage),
            coverage_fraction: if width > 0 {
                node.coverage as f64 / width as f64
            } else {
                0.0
            },
            from: ts(from),
            to: ts(to),
            flags: ResultFlags {
                restoring,
                apportioned: node.apportioned(),
                summary_stale,
                rate_flagged: matches!(agg, km::summary::Aggregate::Rate) && node.rate_flagged(),
                ..ResultFlags::default()
            },
        }
    }

    fn model_aggregate(&self, id: SeriesId, agg: &Aggregate) -> Result<km::summary::Aggregate> {
        let spec = self.spec(id)?;
        let a = convert::aggregate(agg, Some(spec))?;
        let kind = self
            .model
            .registry()
            .map_err(convert::error)?
            .get(msid(id))
            .map(|d| d.kind)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
        if km::summary::refused(kind, &a) {
            return Err(Error::AggregateNotApplicable);
        }
        Ok(a)
    }

    pub(crate) fn window(
        &mut self,
        id: SeriesId,
        window: &Window,
        agg: &Aggregate,
    ) -> Result<WindowResult> {
        self.up()?;
        let a = self.model_aggregate(id, agg)?;
        let now = self.model.now;
        let span = match window {
            Window::Relative { start, end } => {
                let off = |d: &jiff::SignedDuration| {
                    i64::try_from(d.as_nanos()).map_err(|_| Error::TimeOutOfRange)
                };
                let (a, b) = (off(start)?, off(end)?);
                (
                    now.checked_add(a).ok_or(Error::TimeOutOfRange)?,
                    now.checked_add(b).ok_or(Error::TimeOutOfRange)?,
                )
            }
            Window::Between { from, to } => (nanos(*from)?, nanos(*to)?),
            Window::Calendar { window, zone } => {
                let cw = match window {
                    CalendarWindow::SinceMidnight => km::calendar::CalendarWindow::SinceMidnight,
                    CalendarWindow::ThisMonth => km::calendar::CalendarWindow::ThisMonth,
                    CalendarWindow::Day(k) => km::calendar::CalendarWindow::Day(*k),
                    CalendarWindow::Month(k) => km::calendar::CalendarWindow::Month(*k),
                    CalendarWindow::Period(p) => {
                        km::calendar::CalendarWindow::Period(model_period(*p)?)
                    }
                };
                let ans = self
                    .model
                    .calendar_window(msid(id), cw, zone.as_deref())
                    .map_err(convert::error)?;
                return Ok(self.window_result(
                    id,
                    &ans.node,
                    &a,
                    (ans.from, ans.to),
                    ans.restoring,
                    ans.summary_stale,
                ));
            }
        };
        if span.0 >= span.1 {
            return Err(Error::InvalidQuery(
                "a window's start must be before its end".into(),
            ));
        }
        let ans = self
            .model
            .query_window(msid(id), span.0, span.1)
            .map_err(convert::error)?;
        Ok(self.window_result(id, &ans.node, &a, span, ans.restoring, ans.summary_stale))
    }

    pub(crate) fn buckets(
        &mut self,
        id: SeriesId,
        range: (Timestamp, Timestamp),
        resolution: Resolution,
    ) -> Result<ChartData> {
        self.up()?;
        let spec = self.spec(id)?.clone();
        let (a, b) = (nanos(range.0)?, nanos(range.1)?);
        if a >= b {
            return Err(Error::InvalidQuery(
                "a range's start must be before its end".into(),
            ));
        }
        let s = msid(id);
        const MINUTE: i64 = 60_000_000_000;
        const QUARTER: i64 = 15 * MINUTE;
        let span = b - a;
        let step = match resolution {
            Resolution::Auto { max_points } => {
                let max_points = max_points.max(1);
                let current = self.model.current().map_err(convert::error)?;
                let raw: Vec<Fact> = current
                    .range((s, a)..(s, b))
                    .map(|(_, f)| fact_out(f, Some(&spec)))
                    .collect();
                if raw.len() <= max_points {
                    return Ok(ChartData::Facts(raw));
                }
                let max = i64::try_from(max_points).unwrap_or(i64::MAX);
                let fits = |step: i64| (span + step - 1) / step <= max;
                if fits(MINUTE) {
                    MINUTE
                } else if fits(QUARTER) {
                    QUARTER
                } else {
                    let quarters = (span + QUARTER - 1) / QUARTER;
                    QUARTER * ((quarters + max - 1) / max)
                }
            }
            Resolution::Step(d) => {
                let step = dur_nanos(d)?;
                if step <= 0 || span / step > 1_000_000 {
                    return Err(Error::InvalidQuery(
                        "a step must be positive and give at most 1,000,000 buckets".into(),
                    ));
                }
                step
            }
        };
        let mut buckets = Vec::new();
        let mut t = a.div_euclid(step) * step;
        while t < b {
            let (lo, hi) = (t.max(a), (t + step).min(b));
            let node = self.model.window(s, lo, hi).map_err(convert::error)?;
            let num = |agg: km::summary::Aggregate| node.aggregate(&agg).and_then(|v| v.to_f64());
            let numeric = !matches!(spec.kind, Kind::State { .. } | Kind::Text);
            buckets.push(Bucket {
                from: ts(lo),
                to: ts(hi),
                count: node.count,
                min: numeric.then(|| num(km::summary::Aggregate::Min)).flatten(),
                max: numeric.then(|| num(km::summary::Aggregate::Max)).flatten(),
                avg: matches!(spec.kind, Kind::Gauge { .. })
                    .then(|| num(km::summary::Aggregate::Avg))
                    .flatten(),
                coverage: dur(node.coverage),
            });
            t += step;
        }
        Ok(ChartData::Buckets {
            step: dur(i128::from(step)),
            buckets,
        })
    }

    pub(crate) fn facts(
        &mut self,
        id: SeriesId,
        range: (Timestamp, Timestamp),
        view: View,
    ) -> Result<Vec<Fact>> {
        self.up()?;
        let spec = self.spec(id)?.clone();
        let (a, b) = (nanos(range.0)?, nanos(range.1)?);
        let s = msid(id);
        let out = |f: &km::types::Fact| fact_out(f, Some(&spec));
        Ok(match view {
            View::Current => self
                .model
                .current()
                .map_err(convert::error)?
                .range((s, a)..(s, b.max(a)))
                .map(|(_, f)| out(f))
                .collect(),
            View::History => {
                let mut all: Vec<km::types::Fact> = self
                    .model
                    .facts()
                    .map_err(convert::error)?
                    .into_iter()
                    .filter(|f| f.series == s && (a..b).contains(&f.t))
                    .collect();
                all.sort_by_key(|f| (f.t, f.position));
                all.iter().map(out).collect()
            }
            View::AsOf(p) => self.as_of(s, a, b, mpos(p), &out)?,
            View::AsOfTime(at) => {
                // As-of time T: the last position whose batch commit_time,
                // max(received_at, previous commit_time), is at or before T
                // (spec §2.3).
                let at = nanos(at)?;
                let mut commit_time = i64::MIN;
                let mut last = km::types::Position::ORIGIN;
                for e in self.model.entries().map_err(convert::error)? {
                    commit_time = commit_time.max(e.received_at);
                    if commit_time > at {
                        break;
                    }
                    last = e.position;
                }
                self.as_of(s, a, b, last, &out)?
            }
        })
    }

    fn as_of(
        &self,
        s: km::types::SeriesId,
        a: i64,
        b: i64,
        p: km::types::Position,
        out: &dyn Fn(&km::types::Fact) -> Fact,
    ) -> Result<Vec<Fact>> {
        let facts = self.model.facts().map_err(convert::error)?;
        Ok(
            km::view::as_of_position(facts.iter().filter(|f| f.series == s), p)
                .range((s, a)..(s, b.max(a)))
                .map(|(_, f)| out(f))
                .collect(),
        )
    }

    pub(crate) fn interval(
        &mut self,
        id: SeriesId,
        t: Timestamp,
    ) -> Result<(Timestamp, Timestamp)> {
        self.up()?;
        match &self.spec(id)?.kind {
            Kind::Interval {
                length: IntervalLength::Fixed { d, phase },
                ..
            } => {
                let (d, phase, t) = (dur_nanos(*d)?, dur_nanos(*phase)?, nanos(t)?);
                let start = (t - phase).div_euclid(d) * d + phase;
                Ok((
                    ts(start),
                    ts(start.checked_add(d).ok_or(Error::TimeOutOfRange)?),
                ))
            }
            Kind::Interval { .. } => Err(Error::NotSupportedYet {
                what: "calendar-length interval series (a local day or month)",
            }),
            _ => Err(Error::InvalidQuery("not an interval series".into())),
        }
    }

    // ----- health and statistics -----

    pub(crate) fn health(&mut self) -> Health {
        if !self.open {
            return Health::ReadOnly {
                cause: HealthReason::Closed,
                reasons: vec![HealthReason::Closed],
            };
        }
        self.sync_clock();
        let h = self.model.health();
        use km::engine::Reason as R;
        let reasons: Vec<HealthReason> = h
            .reasons
            .iter()
            .map(|r| match r {
                R::SyncFailed => HealthReason::SyncFailed,
                R::Superseded => HealthReason::Superseded,
                R::RestoreFailed => HealthReason::RestoreFailed { file: None },
                R::OpenRefused => HealthReason::OpenRefused,
                R::Quarantined(f) => HealthReason::Quarantined {
                    file: format!("{}", f.0),
                },
                R::LowDisk => HealthReason::LowDisk,
                R::CriticalDisk => HealthReason::CriticalDisk,
                R::UploadStalled => HealthReason::UploadStalled,
                R::ClockSkew(s) => HealthReason::ClockSkew(sid(*s)),
                R::DataLost(a, b) => HealthReason::DataLost {
                    from: pos(*a),
                    to: pos(*b),
                },
            })
            .collect();
        match h.level {
            km::engine::Level::Ok if reasons.is_empty() && !self.model.is_read_only() => Health::Ok,
            km::engine::Level::Fatal => Health::ReadOnly {
                cause: reasons
                    .iter()
                    .find(|r| {
                        matches!(
                            r,
                            HealthReason::SyncFailed
                                | HealthReason::Superseded
                                | HealthReason::RestoreFailed { .. }
                                | HealthReason::OpenRefused
                                | HealthReason::Quarantined { .. }
                        )
                    })
                    .or(reasons.first())
                    .cloned()
                    .unwrap_or(HealthReason::Closed),
                reasons,
            },
            _ => Health::Degraded { reasons },
        }
    }

    pub(crate) fn stats(&mut self) -> Stats {
        if self.open {
            self.sync_clock();
        }
        let c = &self.model.counters;
        let mut refused = self.refused.clone();
        for (r, n) in &c.refused {
            if let Some(r) = convert::refusal(r) {
                *refused.entry(r).or_insert(0) += n;
            }
        }
        let facts = self.model.facts().unwrap_or_default();
        let mut series: BTreeMap<SeriesId, SeriesStats> = self
            .specs
            .keys()
            .map(|id| {
                (
                    *id,
                    SeriesStats {
                        counter_regressions: self.model.counter_regressions(msid(*id)),
                        ..SeriesStats::default()
                    },
                )
            })
            .collect();
        for f in &facts {
            if let Some(st) = series.get_mut(&sid(f.series)) {
                st.facts += 1;
                let t = ts(f.t);
                if st.latest_t.is_none_or(|l| t > l) {
                    st.latest_t = Some(t);
                }
            }
        }
        Stats {
            duplicates_dropped: c.duplicates_dropped,
            duplicates_unchecked: c.duplicates_unchecked,
            refused,
            waits: c
                .waits
                .iter()
                .map(|(w, n)| (convert::wait(w), *n))
                .collect(),
            late: c.late,
            future: c.future,
            non_finite: c.non_finite,
            negative_counter: c.negative_counter,
            outranked_on_arrival: c.outranked_on_arrival,
            overflows: c.overflows,
            batches_discarded: c.batches_discarded,
            series,
            consumers: self
                .consumers
                .iter()
                .map(|(name, s)| {
                    (
                        name.clone(),
                        ConsumerStats {
                            cursor: self
                                .model
                                .disk
                                .cursors
                                .get(&s.id)
                                .copied()
                                .map_or(Position::ORIGIN, pos),
                            catching_up: self.model.in_catch_up(s.id),
                            funnels: s.funnels.len(),
                            evaluations_unknown: 0,
                        },
                    )
                })
                .collect(),
            commit_latency: None,
            backup_lag: None,
            restore_remaining: None,
        }
    }

    /// The clock's wall time as the store last saw it.
    pub(crate) fn now(&mut self) -> Timestamp {
        if self.open {
            self.sync_clock();
        }
        ts(self.model.now)
    }
}

fn model_period(p: CalendarPeriod) -> Result<km::calendar::Period> {
    Ok(match p {
        CalendarPeriod::Day(d) => km::calendar::Period::Day(d),
        CalendarPeriod::Month { year, month } => {
            if !(1..=12).contains(&month) {
                return Err(Error::InvalidQuery("a month is 1 to 12".into()));
            }
            km::calendar::Period::Month(km::calendar::Month { year, month })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::matches_pattern;

    #[test]
    fn patterns_match_within_segments() {
        assert!(matches_pattern("plant/*/power", "plant/line-1/power"));
        assert!(matches_pattern("plant/line-*/p*r", "plant/line-12/power"));
        assert!(!matches_pattern("plant/*/power", "plant/line-1/sub/power"));
        assert!(!matches_pattern("plant/*/power", "plant/line-1/energy"));
        assert!(matches_pattern("a/b/c", "a/b/c"));
        assert!(matches_pattern("*/*/*", "a/b/c"));
        assert!(!matches_pattern("*", "a/b"));
    }
}
