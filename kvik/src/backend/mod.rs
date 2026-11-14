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
mod error;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Waker;

use km::engine::{ConsumerDecl, ConsumerId, EmitError, Model, Selection};
use kvik_model as km;

pub(crate) use error::Error;
use error::Result;

use crate::config::StoreConfig;
use crate::consume::{
    Condition, Delivery, DeliveryFlags, DeliveryId, DeliveryKind, Funnel, FunnelId, FunnelState,
    RESYNC_PART_FUNNELS, ResyncCause, ResyncPart, Select, Start, Trigger,
};
use crate::env::{Clock, Monotonic};
use crate::error::{Fault, Refusal, Wait};
use crate::health::{
    ClockJump, ClockStats, ConsumerStats, DiskStats, Health, IngestStats, LogStats, LossCause,
    LossKind, LossReport, MarkState, Reason, SeriesStats, Stats,
};
use crate::read::{
    Aggregate, Bucket, CalendarPeriod, CalendarWindow, ChartData, Facts, Live, LiveValue,
    ReaderState, Resolution, ResultFlags, View, Window, WindowResult,
};
use crate::series::{
    IntervalLength, Kind, ParameterChange, RetentionClass, SeriesInfo, SeriesKey, SeriesSpec,
    StateValues, Timing,
};
use crate::time::{Span, TimeRange, Timestamp};
use crate::types::{Ack, Fact, Intent, Outcome, Position, Reading, SeriesId, TimeSource, Value};
use convert::{dur, dur_nanos, fact_out, mpos, mquality, msid, mwait, nanos, pos, sid, ts};

/// The most readings in one call, whatever the ingest queue
/// (`docs/api.md` Writing).
const CALL_READINGS: usize = 65_536;
/// The most plain-encoded bytes in one call.
const CALL_BYTES: usize = 8 << 20;

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
    /// Runs `f` under the store's lock, then wakes the wakers it released,
    /// after the lock is dropped.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut Slot) -> R) -> R {
        let (r, wake) = {
            let mut s = lock(&self.state);
            let r = f(&mut s);
            (r, std::mem::take(&mut s.to_wake))
        };
        for w in wake {
            w.wake();
        }
        r
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Finds the in-memory store of a directory, for a reader.
pub(crate) fn find(dir: &Path) -> Result<Arc<Shared>> {
    lock(&STORES).get(dir).cloned().ok_or(Error::NotAStore)
}

/// Opens (creates, or restarts) the in-memory store of a directory. Returns
/// it with the store session's epoch.
pub(crate) fn open(config: &StoreConfig) -> Result<(Arc<Shared>, u64)> {
    config.validate().map_err(|e| match e {
        crate::error::OpenError::Config { key, reason } => Error::Config { key, reason },
        other => Error::Internal(other.to_string()),
    })?;
    jiff::tz::TimeZone::get(&config.time_zone)
        .map_err(|_| Error::UnknownZone(config.time_zone.clone()))?;
    let mut stores = lock(&STORES);
    if let Some(shared) = stores.get(&config.dir).cloned() {
        drop(stores);
        let epoch = shared.with(|s| s.restart(config))?;
        return Ok((shared, epoch));
    }
    if !config.create_if_missing {
        return Err(Error::NotAStore);
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
    /// Parts of a resync not yet handed out.
    parts: VecDeque<Delivery>,
    /// The last part of a resync was handed out and not yet confirmed:
    /// nothing further is delivered until it is.
    awaiting_resync: Option<u32>,
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
    dir: PathBuf,
    mark_sync: crate::health::MarkSyncMode,
    model: Model,
    /// A writer handle is open.
    open: bool,
    /// Store session, raised at every open.
    epoch: u64,
    clock: Arc<dyn Clock>,
    last_mono: Monotonic,
    specs: BTreeMap<SeriesId, SeriesSpec>,
    consumers: BTreeMap<String, ConsumerSlot>,
    max_consumers: usize,
    wakers: Vec<Waker>,
    /// Wakers to wake once the lock is released.
    to_wake: Vec<Waker>,
    /// Model monotonic time of each series' last commit of a fact that counts
    /// for live staleness (spec §5.4).
    committed_mono: BTreeMap<SeriesId, i64>,
    /// Per-series statistics kept by the backend from outcomes.
    series_stats: BTreeMap<SeriesId, SeriesStats>,
    /// Emits waiting now, by a token of each waiting call.
    waiting: BTreeMap<u64, Wait>,
    next_wait_token: u64,
    /// Waits by reason.
    waits: BTreeMap<Wait, u64>,
    batches: u64,
    clock_jumps: u64,
    last_jump: Option<ClockJump>,
    /// Loss identities acknowledged.
    acknowledged: BTreeSet<Position>,
}

fn mark_sync_mode(config: &StoreConfig) -> crate::health::MarkSyncMode {
    match config.log.mark_sync {
        crate::config::MarkSync::EveryCommit => crate::health::MarkSyncMode::EveryCommit {
            reason: "configured".into(),
        },
        _ => crate::health::MarkSyncMode::Interval,
    }
}

fn model_config(config: &StoreConfig) -> Result<km::engine::Config> {
    Ok(km::engine::Config {
        ingest_capacity: config.limits.ingest_queue,
        series_ceiling: config.limits.series_ceiling as usize,
        low_mark: config.limits.disk_low.bytes,
        critical_mark: config.limits.disk_critical,
        seal_interval: dur_nanos(config.log.seal_interval)?,
        future_tolerance: dur_nanos(config.limits.future_tolerance)?,
        lateness_bound: dur_nanos(config.limits.default_lateness_bound)?,
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
        let wall = nanos(clock.wall());
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
            dir: config.dir.clone(),
            mark_sync: mark_sync_mode(config),
            model,
            open: true,
            epoch: 1,
            last_mono: clock.monotonic(),
            clock,
            specs: BTreeMap::new(),
            consumers: BTreeMap::new(),
            max_consumers: max,
            wakers: Vec::new(),
            to_wake: Vec::new(),
            committed_mono: BTreeMap::new(),
            series_stats: BTreeMap::new(),
            waiting: BTreeMap::new(),
            next_wait_token: 0,
            waits: BTreeMap::new(),
            batches: 0,
            clock_jumps: 0,
            last_jump: None,
            acknowledged: BTreeSet::new(),
        })
    }

    fn restart(&mut self, config: &StoreConfig) -> Result<u64> {
        if self.open {
            return Err(Error::Locked);
        }
        self.clock = config.env.clock.clone();
        self.last_mono = self.clock.monotonic();
        let fresh = model_config(config)?;
        let cfg = &mut self.model.config;
        cfg.ingest_capacity = fresh.ingest_capacity;
        cfg.series_ceiling = fresh.series_ceiling;
        cfg.low_mark = fresh.low_mark;
        cfg.critical_mark = fresh.critical_mark;
        cfg.seal_interval = fresh.seal_interval;
        cfg.future_tolerance = fresh.future_tolerance;
        cfg.lateness_bound = fresh.lateness_bound;
        cfg.site_zone = fresh.site_zone;
        self.mark_sync = mark_sync_mode(config);
        self.sync_clock();
        self.model.restart();
        if !self.model.is_up() {
            return Err(self.model.disk.open_refused.clone().map_or_else(
                || Error::Internal("the store did not start".into()),
                convert::error,
            ));
        }
        // The monotonic clock of the last session says nothing about this
        // one: live staleness falls back to event time until the next commit.
        self.committed_mono.clear();
        self.open = true;
        self.epoch += 1;
        for c in self.consumers.values_mut() {
            c.handle = false;
            c.attached = None;
            c.parts.clear();
            c.awaiting_resync = None;
        }
        self.waiting.clear();
        Ok(self.epoch)
    }

    /// Stops the writer, as at a shutdown or crash: everything committed
    /// stays, and readers keep reading it (`docs/api.md`: readers are
    /// independent of the writer). The next open restarts the model from what
    /// it holds as committed, discarding its volatile state.
    pub(crate) fn close(&mut self, epoch: u64) {
        if self.open && self.epoch == epoch {
            self.open = false;
            self.wake_all();
        }
    }

    /// Brings the model's clocks to the injected clock's: the monotonic
    /// delta advances both, the remaining wall difference is a jump.
    /// Wall-clock jumps are counted as statistics (`docs/api.md` Health and
    /// statistics).
    fn sync_clock(&mut self) {
        let mono = self.clock.monotonic();
        let dn = i64::try_from(mono.0.saturating_sub(self.last_mono.0)).unwrap_or(i64::MAX);
        self.last_mono = mono;
        if dn > 0 {
            self.model.advance(dn);
        }
        let w = nanos(self.clock.wall());
        let dw = w - self.model.wall;
        if dw != 0 {
            self.model.jump_wall(dw);
            self.clock_jumps += 1;
            self.last_jump = Some(ClockJump {
                at: ts(w),
                by: Span(dw),
            });
        }
        self.model.clock_synchronised = self.clock.synchronised();
    }

    /// The writer is open.
    fn up(&mut self) -> Result<()> {
        if !self.open || !self.model.is_up() {
            return Err(Error::Closed);
        }
        self.sync_clock();
        Ok(())
    }

    /// Committed data can be read, whether or not the writer is open.
    fn readable(&mut self) -> Result<()> {
        if !self.model.is_up() {
            return Err(Error::Closed);
        }
        if self.open {
            self.sync_clock();
        }
        Ok(())
    }

    fn writable(&mut self, epoch: u64) -> Result<()> {
        if self.epoch != epoch {
            return Err(Error::Closed);
        }
        self.up()?;
        if self.model.is_read_only() {
            return Err(Error::ReadOnly(self.read_only_cause()));
        }
        Ok(())
    }

    pub(crate) fn register(&mut self, w: &Waker) {
        if !self.wakers.iter().any(|x| x.will_wake(w)) {
            self.wakers.push(w.clone());
        }
    }

    /// Moves every registered waker to the list woken once the store's
    /// lock is released ([`Shared::with`]), so that no waker runs, or is
    /// dropped, under the lock.
    fn wake_all(&mut self) {
        self.to_wake.append(&mut self.wakers);
    }

    fn read_only_cause(&mut self) -> String {
        match self.health() {
            Health::ReadOnly { cause, .. } => cause.to_string(),
            _ => "a write or sync failed".into(),
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
        let defined = self.model.define_with(
            &spec.name,
            d.kind,
            d.value_type,
            &spec.unit,
            d.staleness,
            d.options,
        );
        let id = match defined {
            Ok(id) => sid(id),
            Err(km::engine::Error::Registry(km::registry::RegistryError::NameInUse(_))) => {
                return Err(Error::Conflict(self.conflict(&spec)));
            }
            Err(e) => return Err(convert::error(e)),
        };
        if let Some(existing) = self.specs.get(&id) {
            if without_name(existing) == without_name(&spec) {
                return Ok(id);
            }
            return Err(Error::Conflict(self.conflict(&spec)));
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

    /// The first field in which `spec` differs from the live series of its
    /// name.
    fn conflict(&self, spec: &SeriesSpec) -> &'static str {
        let existing = self
            .model
            .registry()
            .ok()
            .and_then(|r| r.resolve(&spec.name))
            .and_then(|d| self.specs.get(&sid(d.id)));
        let Some(e) = existing else {
            return "definition";
        };
        if e.kind != spec.kind {
            "kind"
        } else if e.unit != spec.unit {
            "unit"
        } else if e.expected_interval != spec.expected_interval {
            "expected_interval"
        } else if e.staleness != spec.staleness {
            "staleness"
        } else if e.versioned != spec.versioned {
            "versioned"
        } else if e.timing != spec.timing {
            "timing"
        } else {
            "definition"
        }
    }

    pub(crate) fn set_site_zone(
        &mut self,
        epoch: u64,
        zone: &str,
        effective_from: Timestamp,
    ) -> Result<Position> {
        self.writable(epoch)?;
        jiff::tz::TimeZone::get(zone).map_err(|_| Error::UnknownZone(zone.into()))?;
        let p = self
            .model
            .set_site_zone(zone, nanos(effective_from))
            .map_err(convert::error)?;
        Ok(pos(p))
    }

    pub(crate) fn raise_scale(&mut self, epoch: u64, id: SeriesId, _scale: u8) -> Result<()> {
        self.writable(epoch)?;
        self.spec(id)?;
        Err(Error::NotSupportedYet(
            "decimal series and scale raises: the reference model has no exact decimal values",
        ))
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
            return Err(Error::NotSupportedYet(
                "parameter changes other than the retention class",
            ));
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
            .retire(msid(id), nanos(t_r))
            .map_err(convert::error)
    }

    pub(crate) fn series(&mut self, key: SeriesKey) -> Result<SeriesInfo> {
        self.readable()?;
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

    fn model_reading(&self, r: &Reading) -> std::result::Result<km::types::Reading, Refusal> {
        let spec = self.specs.get(&r.series).ok_or(Refusal::UnknownSeries)?;
        use km::types::Value as V;
        let value = match (&spec.kind, &r.value) {
            (_, Value::Absent) => V::Absent,
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
                    .ok_or(Refusal::StateOutOfRange)?
                    .clone(),
            ),
            (Kind::Text, Value::Text(s)) => V::Str(s.clone()),
            _ => return Err(Refusal::WrongType),
        };
        // The reference model summarises a value-less reading only when it
        // is bad quality, so an `Absent` is submitted as bad (its supplied
        // quality is not kept).
        let quality = if matches!(value, V::Absent) {
            km::types::Quality::Bad
        } else {
            mquality(r.quality)
        };
        Ok(km::types::Reading {
            series: msid(r.series),
            t: nanos(r.t),
            value,
            quality,
            changed_at: r.changed_at,
            t_source: match r.source {
                TimeSource::Device => km::types::TSource::Device,
                TimeSource::Host => km::types::TSource::Host,
            },
        })
    }

    /// A token for one waiting emit call.
    pub(crate) fn wait_token(&mut self) -> u64 {
        self.next_wait_token += 1;
        self.next_wait_token
    }

    /// Records that a call waits (counted once per call, `first`).
    pub(crate) fn note_wait(&mut self, token: Option<u64>, w: Wait, first: bool) {
        if first {
            *self.waits.entry(w).or_insert(0) += 1;
            *self.model.counters.waits.entry(mwait(w)).or_insert(0) += 1;
        }
        if let Some(t) = token {
            self.waiting.insert(t, w);
        }
    }

    /// A call no longer waits (it completed or was dropped).
    pub(crate) fn clear_wait(&mut self, token: u64) {
        self.waiting.remove(&token);
    }

    fn stats_of(&mut self, id: SeriesId) -> &mut SeriesStats {
        self.series_stats.entry(id).or_default()
    }

    /// The entries of the batch that ends at `last`.
    fn batch_ending(&self, last: km::types::Position) -> Vec<km::engine::Entry> {
        self.model
            .disk
            .files
            .values()
            .rev()
            .find_map(|f| {
                f.batches
                    .last()
                    .filter(|b| b.last().is_some_and(|e| e.position == last))
                    .cloned()
            })
            .unwrap_or_default()
    }

    /// Emits a call's readings as one batch and commits it. `Ok(Err(_))` is
    /// back-pressure: nothing of the call was queued.
    pub(crate) fn emit(
        &mut self,
        epoch: u64,
        readings: &[Reading],
        intent: Intent,
    ) -> Result<std::result::Result<Ack, Wait>> {
        self.writable(epoch)?;
        let bytes: usize = readings
            .iter()
            .map(|r| {
                32 + match &r.value {
                    Value::Text(t) => t.len(),
                    _ => 8,
                }
            })
            .sum();
        let limit = CALL_READINGS.min(self.model.config.ingest_capacity);
        if readings.len() > limit || bytes > CALL_BYTES {
            return Err(Error::TooLarge {
                readings: readings.len(),
                bytes,
            });
        }
        let m = &mut self.model;
        // The model queues reading by reading and cannot take one back, so
        // every cause of a wait is checked before anything is queued.
        let busy = if m.free_bytes() < m.config.critical_mark {
            Some(Wait::DiskFull)
        } else if !m.clock_synchronised && readings.iter().any(|r| r.source == TimeSource::Host) {
            Some(Wait::UnsyncedClock)
        } else if m.queued() + readings.len() > m.config.ingest_capacity {
            Some(Wait::QueueFull)
        } else {
            None
        };
        if let Some(b) = busy {
            return Ok(Err(b));
        }
        m.tick().map_err(convert::error)?;
        let mut outcomes: Vec<Option<Outcome>> = vec![None; readings.len()];
        let mut emitted = Vec::new();
        for (i, r) in readings.iter().enumerate() {
            let mr = match self.model_reading(r) {
                Ok(mr) => mr,
                Err(x) => {
                    outcomes[i] = Some(Outcome::Refused(x));
                    continue;
                }
            };
            let res = match intent {
                Intent::Revise => self.model.emit_revise(mr.clone()),
                _ => self.model.emit(mr.clone()),
            };
            match res {
                Ok(()) => emitted.push((i, mr)),
                Err(EmitError::Refused(x)) => match convert::refusal(&x) {
                    Some(x) => outcomes[i] = Some(Outcome::Refused(x)),
                    None => return Err(Error::Internal(format!("refused mid-call: {x}"))),
                },
                Err(EmitError::Wait(w)) => {
                    return Err(Error::Internal(format!(
                        "unexpected wait mid-call: {}",
                        convert::wait(&w)
                    )));
                }
            }
        }
        let report = self.model.commit(None).map_err(convert::error)?;
        self.batches += 1;
        // The batch holds the recorded readings in order; a reading with no
        // entry was dropped as a duplicate.
        let entries = report
            .positions
            .last()
            .map(|p| self.batch_ending(*p))
            .unwrap_or_default();
        let facts: Vec<km::types::Fact> =
            entries.iter().filter_map(|e| e.fact().cloned()).collect();
        let mut next = facts.iter().peekable();
        let versioned: BTreeSet<SeriesId> = self
            .specs
            .iter()
            .filter(|(_, s)| s.versioned.is_some())
            .map(|(id, _)| *id)
            .collect();
        for (i, mr) in emitted {
            let matched = next.peek().is_some_and(|f| {
                f.series == mr.series
                    && f.t == mr.t
                    && f.supplied_quality == mr.quality
                    && f.changed_at == mr.changed_at
                    && (f.value.identical(&mr.value) || f.flags.no_value)
            });
            if !matched {
                outcomes[i] = Some(Outcome::Duplicate);
                continue;
            }
            let f = next.next().expect("peeked");
            let current = self.model.current().map_err(convert::error)?;
            let outranked = versioned.contains(&sid(f.series))
                && current
                    .get(&(f.series, f.t))
                    .is_some_and(|c| c.position < f.position);
            if !outranked && f.summarised() && !f.flags.future {
                self.committed_mono.insert(sid(f.series), self.model.mono);
            }
            let st = self.stats_of(sid(f.series));
            st.late += u64::from(f.flags.late);
            st.future += u64::from(f.flags.future);
            st.outranked_on_arrival += u64::from(outranked);
            outcomes[i] = Some(Outcome::Recorded {
                position: pos(f.position),
                outranked,
                unchecked: false,
                rounded: false,
            });
        }
        let outcomes: Vec<Outcome> = outcomes
            .into_iter()
            .map(|o| o.expect("every reading has an outcome"))
            .collect();
        for (r, o) in readings.iter().zip(&outcomes) {
            let st = self.stats_of(r.series);
            match o {
                Outcome::Recorded { .. } => st.recorded += 1,
                Outcome::Duplicate => st.duplicates_dropped += 1,
                Outcome::Refused(x) => *st.refused.entry(*x).or_insert(0) += 1,
            }
        }
        self.wake_all();
        Ok(Ok(Ack::new(outcomes)))
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

    pub(crate) fn consumer(
        &mut self,
        epoch: u64,
        name: &str,
        start: Start,
        emits: &Select,
    ) -> Result<()> {
        if self.epoch != epoch {
            return Err(Error::Closed);
        }
        self.up()?;
        if *emits != Select::None {
            return Err(Error::NotSupportedYet(
                "a consumer that emits readings (emits other than Select::None): the model \
                       has no routing-cycle check",
            ));
        }
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
                return Err(Error::NotSupportedYet(
                    "a new consumer starting at a position",
                ));
            }
            Start::Time(_) => {
                return Err(Error::NotSupportedYet("a new consumer starting at a time"));
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
                parts: VecDeque::new(),
                awaiting_resync: None,
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

    /// Readings a consumer emits (spec §4 Routing): only series in its
    /// `emits` are accepted. A consumer can only declare [`Select::None`] for
    /// now, so every reading is refused [`Refusal::NotDeclared`].
    pub(crate) fn consumer_emit(
        &mut self,
        epoch: u64,
        name: &str,
        readings: &[Reading],
    ) -> Result<Ack> {
        self.bound(epoch, name)?;
        self.writable(epoch)?;
        for r in readings {
            *self
                .stats_of(r.series)
                .refused
                .entry(Refusal::NotDeclared)
                .or_insert(0) += 1;
        }
        Ok(Ack::new(
            readings
                .iter()
                .map(|_| Outcome::Refused(Refusal::NotDeclared))
                .collect(),
        ))
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
            return Err(Error::ReadOnly("routing changes are log entries".into()));
        }
        let registry = self.model.registry().map_err(convert::error)?;
        let (selection, pattern_set) = match &funnel.series {
            Select::All => (Selection::All, None),
            Select::None => (Selection::Series(BTreeSet::new()), None),
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
        // What the model does not support, before anything is recorded.
        match (&funnel.operator, &funnel.condition) {
            (Some(_), None) => {
                return Err(Error::NotSupportedYet(
                    "a funnel with an operator but no condition",
                ));
            }
            (Some(_), Some(Condition::HeldFor { .. })) => {
                return Err(Error::NotSupportedYet(
                    "a held-for condition over an operator",
                ));
            }
            _ => {}
        }
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
    /// Takes the next delivery, if one is ready: the next part of a resync
    /// being handed out, else, unless a resync awaits confirmation, a
    /// delivery from the model after running due timers and catch-up.
    pub(crate) fn poll_delivery(&mut self, epoch: u64, name: &str) -> Result<Option<Delivery>> {
        let cid = self.bound(epoch, name)?;
        let c = self.consumers.get_mut(name).ok_or(Error::Closed)?;
        if let Some(d) = c.parts.pop_front() {
            if let DeliveryKind::Resync(p) = &d.kind
                && p.last
            {
                c.awaiting_resync = Some(p.part);
            }
            return Ok(Some(d));
        }
        if c.awaiting_resync.is_some() {
            return Ok(None);
        }
        self.model.tick().map_err(convert::error)?;
        self.model
            .catch_up(cid, usize::MAX)
            .map_err(convert::error)?;
        let mut got = self.model.receive(cid, 1).map_err(convert::error)?;
        self.forget_observations();
        let Some(e) = got.pop() else { return Ok(None) };
        let parts = self.deliveries(name, e);
        let c = self.consumers.get_mut(name).ok_or(Error::Closed)?;
        c.parts.extend(parts);
        let d = c.parts.pop_front();
        if let Some(Delivery {
            kind: DeliveryKind::Resync(p),
            ..
        }) = &d
            && p.last
        {
            c.awaiting_resync = Some(p.part);
        }
        Ok(d)
    }

    fn api_funnel(&self, name: &str, model: km::engine::FunnelId) -> FunnelId {
        self.consumers
            .get(name)
            .and_then(|c| c.to_api.get(&model.0).copied())
            .unwrap_or(FunnelId(model.0))
    }

    /// The deliveries of one model envelope: one, or the parts of a resync.
    fn deliveries(&self, name: &str, e: km::engine::Envelope) -> Vec<Delivery> {
        use km::engine::DeliveryKind as K;
        let mut flags = DeliveryFlags {
            replayed: e.replayed,
            outranked: e.outranked,
            restoring: self.model.restoring(),
            ..DeliveryFlags::default()
        };
        let position = pos(e.position);
        let delivered_at = ts(self.model.now);
        let one = |id, kind, flags| {
            vec![Delivery {
                id,
                position,
                kind,
                flags,
                delivered_at,
            }]
        };
        match e.kind {
            K::Fact(f) => {
                flags.future = f.flags.future;
                one(
                    DeliveryId::Fact(pos(f.position)),
                    DeliveryKind::Fact(fact_out(&f, self.specs.get(&sid(f.series)))),
                    flags,
                )
            }
            K::Trigger(t) => one(
                DeliveryId::Condition {
                    funnel: self.api_funnel(name, t.funnel),
                    sequence: t.sequence,
                },
                DeliveryKind::Trigger(Trigger {
                    funnel: self.api_funnel(name, t.funnel),
                    state: convert::condition_state(&t.state),
                    value: None,
                    coverage: None,
                    cause: convert::trigger_cause(t.cause),
                }),
                flags,
            ),
            K::GenerationChanged { restored_end, .. } => one(
                DeliveryId::GenerationChanged,
                DeliveryKind::GenerationChanged {
                    from: restored_end.generation,
                    to: self.model.disk.generation,
                },
                flags,
            ),
            K::Resync(r) => {
                let cause = if r.replaced.is_some() {
                    ResyncCause::CursorBehindLog
                } else {
                    ResyncCause::Resume
                };
                let states: Vec<FunnelState> = r
                    .funnels
                    .iter()
                    .filter(|f| f.funnel != km::engine::FunnelId::CONFIGURED)
                    .flat_map(|f| {
                        let funnel = self.api_funnel(name, f.funnel);
                        let condition = f
                            .condition
                            .as_ref()
                            .map(|c| convert::condition_state(&c.state));
                        let transition = f.condition.as_ref().is_some_and(|c| c.transition);
                        f.live.iter().map(move |(s, fact)| FunnelState {
                            funnel,
                            series: sid(*s),
                            live: self.live_of(sid(*s), fact.as_ref()),
                            condition: condition.clone(),
                            transition,
                        })
                    })
                    .collect();
                let chunks: Vec<Vec<FunnelState>> = if states.is_empty() {
                    vec![Vec::new()]
                } else {
                    states
                        .chunks(RESYNC_PART_FUNNELS)
                        .map(<[FunnelState]>::to_vec)
                        .collect()
                };
                let n = chunks.len();
                chunks
                    .into_iter()
                    .enumerate()
                    .map(|(k, funnels)| {
                        let part = u32::try_from(k + 1).unwrap_or(u32::MAX);
                        Delivery {
                            id: DeliveryId::Resync { part },
                            position,
                            kind: DeliveryKind::Resync(ResyncPart {
                                cause,
                                forget_above: None,
                                part,
                                last: k + 1 == n,
                                funnels,
                            }),
                            flags,
                            delivered_at,
                        }
                    })
                    .collect()
            }
        }
    }

    pub(crate) fn confirm(&mut self, epoch: u64, name: &str, id: DeliveryId) -> Result<()> {
        let cid = self.bound(epoch, name)?;
        let mid = match id {
            DeliveryId::Fact(p) => km::engine::DeliveryId::Fact(mpos(p)),
            // Condition funnels are never re-routed: their ids are the
            // model's.
            DeliveryId::Condition { funnel, sequence } => km::engine::DeliveryId::Timer {
                generation: self.model.disk.generation,
                funnel: km::engine::FunnelId(funnel.0),
                sequence,
            },
            DeliveryId::GenerationChanged => km::engine::DeliveryId::GenerationChanged,
            DeliveryId::Resync { part } => {
                // Only the last part confirms the resync; an earlier part
                // does nothing.
                let c = self.consumers.get_mut(name).ok_or(Error::Closed)?;
                if c.awaiting_resync != Some(part) {
                    return Ok(());
                }
                c.awaiting_resync = None;
                km::engine::DeliveryId::Resync
            }
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
        self.readable()?;
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
            return Err(Error::AggregateRefused(
                "avg, integral, increase and time-in-state are refused on an interval series"
                    .into(),
            ));
        }
        Ok(a)
    }

    pub(crate) fn window(
        &mut self,
        id: SeriesId,
        window: &Window,
        agg: &Aggregate,
    ) -> Result<WindowResult> {
        self.readable()?;
        let a = self.model_aggregate(id, agg)?;
        let now = self.model.now;
        let span = match window {
            Window::Relative { start, end } => {
                let at = |d: Span| {
                    now.checked_add(d.0)
                        .ok_or_else(|| Error::InvalidQuery("window outside the time range".into()))
                };
                (at(*start)?, at(*end)?)
            }
            Window::Absolute(r) => (nanos(r.from), nanos(r.to)),
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
                    .calendar_window(msid(id), cw, zone.as_ref().map(|z| z.as_str()))
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
        range: TimeRange,
        resolution: Resolution,
    ) -> Result<ChartData> {
        self.readable()?;
        let spec = self.spec(id)?.clone();
        let (a, b) = (nanos(range.from), nanos(range.to));
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
                // Buckets are aligned to UTC, so they are counted from the
                // aligned floor of the start, not from the start.
                let buckets = |step: i64| {
                    let first = a.div_euclid(step) * step;
                    (b - first + step - 1) / step
                };
                if buckets(MINUTE) <= max {
                    MINUTE
                } else if buckets(QUARTER) <= max {
                    QUARTER
                } else {
                    let mut k = ((span + QUARTER - 1) / QUARTER + max - 1) / max;
                    while buckets(QUARTER * k) > max {
                        k += 1;
                    }
                    QUARTER * k
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
            // Min and max are over readings that start in the bucket only;
            // the mean can come from a value carried in.
            let readings = numeric && node.count > 0;
            buckets.push(Bucket {
                from: ts(lo),
                to: ts(hi),
                count: node.count,
                min: readings.then(|| num(km::summary::Aggregate::Min)).flatten(),
                max: readings.then(|| num(km::summary::Aggregate::Max)).flatten(),
                mean: matches!(spec.kind, Kind::Gauge { .. })
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

    /// The last position whose batch commit time, max(`received_at`,
    /// previous commit time), is at or before `at` (spec §2.3).
    fn position_at(&self, at: i64) -> Result<km::types::Position> {
        let mut commit_time = i64::MIN;
        let mut last = km::types::Position::ORIGIN;
        for e in self.model.entries().map_err(convert::error)? {
            commit_time = commit_time.max(e.received_at);
            if commit_time > at {
                break;
            }
            last = e.position;
        }
        Ok(last)
    }

    /// The facts of a series in a view, in `t` then position order: one per
    /// key, or every fact for [`View::History`].
    fn view_facts(&self, s: km::types::SeriesId, view: View) -> Result<Vec<km::types::Fact>> {
        let as_of = |p| -> Result<Vec<km::types::Fact>> {
            let facts = self.model.facts().map_err(convert::error)?;
            Ok(
                km::view::as_of_position(facts.iter().filter(|f| f.series == s), p)
                    .into_values()
                    .collect(),
            )
        };
        Ok(match view {
            View::Current => self
                .model
                .current()
                .map_err(convert::error)?
                .range((s, i64::MIN)..=(s, i64::MAX))
                .map(|(_, f)| f.clone())
                .collect(),
            View::History => {
                let mut all: Vec<km::types::Fact> = self
                    .model
                    .facts()
                    .map_err(convert::error)?
                    .into_iter()
                    .filter(|f| f.series == s)
                    .collect();
                all.sort_by_key(|f| (f.t, f.position));
                all
            }
            View::AsOf(p) => as_of(mpos(p))?,
            View::AsOfTime(at) => as_of(self.position_at(nanos(at))?)?,
        })
    }

    pub(crate) fn facts(&mut self, id: SeriesId, range: TimeRange, view: View) -> Result<Facts> {
        self.readable()?;
        let spec = self.spec(id)?.clone();
        let (a, b) = (nanos(range.from), nanos(range.to));
        Ok(Facts::new(
            self.view_facts(msid(id), view)?
                .iter()
                .filter(|f| (a..b).contains(&f.t))
                .map(|f| fact_out(f, Some(&spec)))
                .collect(),
        ))
    }

    /// The latest fact at or before `t` that a window can credit (not bad
    /// quality; the model has no displaced readings), in a view, ignoring
    /// the staleness limit.
    pub(crate) fn at(&mut self, id: SeriesId, t: Timestamp, view: View) -> Result<Option<Fact>> {
        self.readable()?;
        let spec = self.spec(id)?.clone();
        let t = nanos(t);
        let facts = self.view_facts(msid(id), view)?;
        Ok(facts
            .iter()
            .rev()
            .filter(|f| f.t <= t && f.summarised())
            // In the history view, the latest recorded fact at the greatest
            // such `t`.
            .max_by_key(|f| (f.t, f.position))
            .map(|f| fact_out(f, Some(&spec))))
    }

    pub(crate) fn interval(
        &mut self,
        id: SeriesId,
        t: Timestamp,
    ) -> Result<(Timestamp, Timestamp)> {
        self.readable()?;
        match &self.spec(id)?.kind {
            Kind::Interval {
                length: IntervalLength::Fixed { d, phase },
                ..
            } => {
                let (d, phase, t) = (dur_nanos(*d)?, dur_nanos(*phase)?, nanos(t));
                let start = (t - phase).div_euclid(d) * d + phase;
                let end = start
                    .checked_add(d)
                    .ok_or_else(|| Error::InvalidQuery("interval outside the time range".into()))?;
                Ok((ts(start), ts(end)))
            }
            Kind::Interval { .. } => Err(Error::NotSupportedYet(
                "calendar-length interval series (a local day or month)",
            )),
            _ => Err(Error::InvalidQuery("not an interval series".into())),
        }
    }

    // ----- health and statistics -----

    /// The last committed position.
    fn head(&self) -> Position {
        self.model
            .disk
            .files
            .values()
            .rev()
            .find_map(|f| {
                f.batches
                    .last()
                    .and_then(|b| b.last())
                    .map(|e| pos(e.position))
            })
            .unwrap_or(Position::ORIGIN)
    }

    pub(crate) fn losses(&self) -> Vec<LossReport> {
        self.model
            .disk
            .lost_ranges()
            .into_iter()
            .map(|(a, b)| LossReport {
                identity: pos(a),
                kind: LossKind::DataLost,
                cause: LossCause::Corruption,
                first: Some(pos(a)),
                last: Some(pos(b)),
                file_id: None,
                bytes: 0,
                readable_commit_blocks: 0,
                mark_state: MarkState::Readable,
                boot_id: None,
                commit_time_before: None,
                commit_time_after: None,
                cursors_missing: Vec::new(),
                recorded: true,
                acknowledged: self.acknowledged.contains(&pos(a)),
            })
            .collect()
    }

    pub(crate) fn acknowledge_losses(&mut self, epoch: u64, ids: &[Position]) -> Result<()> {
        self.writable(epoch)?;
        self.acknowledged.extend(ids.iter().copied());
        Ok(())
    }

    /// No manifest record is ever rebuilt by the model backend, so there is
    /// nothing to acknowledge.
    pub(crate) fn acknowledge_manifest_rebuild(
        &mut self,
        epoch: u64,
        _epoch_id: u64,
    ) -> Result<()> {
        self.writable(epoch)
    }

    pub(crate) fn health(&mut self) -> Health {
        if !self.open {
            return Health::Closed;
        }
        self.sync_clock();
        let h = self.model.health();
        let free = self.model.free_bytes();
        let path = self.dir.clone();
        let losses = self.losses();
        use km::engine::Reason as R;
        let mut fault = None;
        let mut reasons = Vec::new();
        for r in &h.reasons {
            match r {
                R::SyncFailed => {
                    fault.get_or_insert(Fault::SyncFailed { path: path.clone() });
                }
                R::Superseded | R::RestoreFailed | R::OpenRefused | R::Quarantined(_) => {
                    fault.get_or_insert(Fault::Io {
                        op: "start",
                        path: path.clone(),
                        kind: std::io::ErrorKind::Other,
                    });
                }
                R::LowDisk => reasons.push(Reason::DiskLow { free }),
                R::CriticalDisk => reasons.push(Reason::DiskCritical { free }),
                R::UploadStalled => {}
                R::ClockSkew(s) => reasons.push(Reason::ClockSkew(sid(*s))),
                R::DataLost(a, _) => {
                    if let Some(l) = losses.iter().find(|l| l.identity == pos(*a))
                        && !l.acknowledged
                    {
                        reasons.push(Reason::DataLost(l.clone()));
                    }
                }
            }
        }
        let waiting: BTreeSet<Wait> = self.waiting.values().copied().collect();
        reasons.extend(waiting.into_iter().map(Reason::IngestWaiting));
        if fault.is_none() && self.model.is_read_only() {
            fault = Some(Fault::SyncFailed { path });
        }
        match (fault, reasons.is_empty()) {
            (Some(cause), _) => Health::ReadOnly { cause, reasons },
            (None, true) => Health::Ok,
            (None, false) => Health::Degraded { reasons },
        }
    }

    pub(crate) fn stats(&mut self) -> Stats {
        if self.open {
            self.sync_clock();
        }
        let head = self.head();
        Stats {
            ingest: IngestStats {
                queue_depth: self.model.queued(),
                waiting: self.waiting.values().next().copied(),
                waits: self.waits.clone(),
                batches: self.batches,
                syncs: self.batches,
                ..IngestStats::default()
            },
            log: LogStats {
                durable: head,
                mark_synced: head,
                mark_sync: self.mark_sync.clone(),
                open_file_id: self.model.disk.active.map(|f| f.0),
                sealed_segments: self
                    .model
                    .disk
                    .files
                    .values()
                    .filter(|f| f.sealed && f.class == km::engine::FileClass::Segment)
                    .count() as u64,
                bytes: self.model.disk.used_bytes(self.model.config.entry_bytes),
                manifest_rebuilds: 0,
            },
            disk: DiskStats {
                free: self.model.free_bytes(),
            },
            clock: ClockStats {
                jumps: self.clock_jumps,
                last: self.last_jump,
            },
        }
    }

    pub(crate) fn series_stats(&mut self, id: SeriesId) -> Option<SeriesStats> {
        self.specs.get(&id)?;
        let mut st = self.series_stats.get(&id).cloned().unwrap_or_default();
        st.counter_regressions = self.model.counter_regressions(msid(id));
        st.latest_t = self.model.facts().ok().and_then(|fs| {
            fs.iter()
                .filter(|f| f.series == msid(id))
                .map(|f| ts(f.t))
                .max()
        });
        Some(st)
    }

    pub(crate) fn consumer_stats(&self, name: &str) -> Option<ConsumerStats> {
        let c = self.consumers.get(name)?;
        Some(ConsumerStats {
            cursor: self
                .model
                .disk
                .cursors
                .get(&c.id)
                .copied()
                .map_or(Position::ORIGIN, pos),
            catching_up: self.model.in_catch_up(c.id),
            funnels: c.funnels.len(),
        })
    }

    pub(crate) fn refresh(&mut self) -> Result<ReaderState> {
        self.readable()?;
        Ok(ReaderState {
            durable: self.head(),
            epoch: self.model.disk.latest_epoch(),
            generation: self.model.disk.generation,
            lease_renewed_at: ts(self.model.now),
        })
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
        CalendarPeriod::Day(d) => km::calendar::Period::Day(
            d.to_jiff()
                .ok_or_else(|| Error::InvalidQuery(format!("no date {d}")))?,
        ),
        CalendarPeriod::Month { year, month } => {
            let month = i8::try_from(month)
                .ok()
                .filter(|m| (1..=12).contains(m))
                .ok_or_else(|| Error::InvalidQuery("a month is 1 to 12".into()))?;
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
