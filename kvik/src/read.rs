//! Query types (spec §2.3, §5.7, §6; `docs/api.md` Reading).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::consume::Select;
use crate::error::QueryError;
use crate::health::LossReport;
use crate::series::{SeriesInfo, SeriesKey};
use crate::time::{LocalDate, Span, TimeRange, Timestamp, ZoneName};

use crate::types::{Fact, Position, SeriesId, Value};

/// An aggregate over a window (spec §6 Aggregates).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Aggregate {
    /// Gauge: integral ÷ covered seconds.
    Avg,
    /// Sum of reading values; for an interval series, the amount in the
    /// window.
    Sum,
    /// Integral in value × seconds.
    Integral,
    /// Smallest reading.
    Min,
    /// Largest reading.
    Max,
    /// Number of readings.
    Count,
    /// Counter increase.
    Increase,
    /// Counter increase per second; interval series: sum ÷ covered seconds.
    Rate,
    /// Last value, or the held value with no reading inside.
    Last,
    /// Seconds in a state ([`Value::Bool`] or [`Value::State`]).
    TimeInState(Value),
}

/// The value of an aggregate.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Aggregated {
    /// A number.
    Number(f64),
    /// A reading's value ([`Aggregate::Last`]); [`Value::Absent`] for a
    /// reading recorded without one.
    Value(Value),
}

/// A calendar period (spec §5.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CalendarPeriod {
    /// A local day.
    Day(LocalDate),
    /// A local month.
    Month {
        /// Year.
        year: i16,
        /// Month, 1–12.
        month: u8,
    },
}

impl From<LocalDate> for CalendarPeriod {
    fn from(d: LocalDate) -> Self {
        CalendarPeriod::Day(d)
    }
}

/// A calendar window (spec §6 Windows), taken at `now`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CalendarWindow {
    /// From local midnight to `now`.
    SinceMidnight,
    /// From the start of the month to `now`.
    ThisMonth,
    /// The local day `k` days after today (before it for negative `k`),
    /// whole.
    Day(i32),
    /// The month `k` months after this one, whole.
    Month(i32),
    /// A named period, whole.
    Period(CalendarPeriod),
}

/// A window (spec §6 Windows). Bounds are event times, evaluated against the
/// store's `now`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Window {
    /// `[now + start, now + end)`, `start < end`, of either sign.
    Relative {
        /// Start offset from `now`.
        start: Span,
        /// End offset from `now`.
        end: Span,
    },
    /// A fixed range of event time.
    Absolute(TimeRange),
    /// A calendar window in the site zone, or in the named IANA zone for its
    /// whole span.
    Calendar {
        /// The window.
        window: CalendarWindow,
        /// A named zone instead of the site zone.
        zone: Option<ZoneName>,
    },
}

impl Window {
    /// The last `width` up to `now`: `[now − width, now)`.
    ///
    /// # Panics
    /// Panics if `width` exceeds the range of [`Span`].
    pub fn rolling(width: Duration) -> Self {
        let w = Span::try_from(width).expect("width within range");
        Window::Relative {
            start: -w,
            end: Span::ZERO,
        }
    }

    /// `[now + start, now + end)`.
    pub fn between(start: Span, end: Span) -> Self {
        Window::Relative { start, end }
    }

    /// `[from, to)` in event time.
    pub fn absolute(range: impl Into<TimeRange>) -> Self {
        Window::Absolute(range.into())
    }

    /// A calendar window in the site zone.
    pub fn calendar(window: CalendarWindow) -> Self {
        Window::Calendar { window, zone: None }
    }
}

/// Flags of a query result (spec §6, §8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ResultFlags {
    /// Served while a restore is bringing detail back.
    pub restoring: bool,
    /// An edge moved inward to where detail is local; `from` and `to` give
    /// the span evaluated.
    pub approximate_edges: bool,
    /// An interval series' amount was credited pro rata at an edge.
    pub apportioned: bool,
    /// A displaced interval reading lies in the span (spec §5.4).
    pub displaced: bool,
    /// A minute in the span awaits detail not on the machine (spec §5.6).
    pub summary_stale: bool,
    /// A counter step in a rate spans more than the staleness limit.
    pub rate_flagged: bool,
}

/// A window result (spec §6).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct WindowResult {
    /// The aggregate's value; `None` when empty.
    pub value: Option<Aggregated>,
    /// Readings in the span.
    pub count: u64,
    /// Time with a valid value.
    pub coverage: Duration,
    /// Coverage as a fraction of the span (0 for an empty span).
    pub coverage_fraction: f64,
    /// Start of the span evaluated.
    pub from: Timestamp,
    /// End of the span evaluated.
    pub to: Timestamp,
    /// Flags.
    pub flags: ResultFlags,
}

impl WindowResult {
    /// The value as a number, if it is one.
    pub fn number(&self) -> Option<f64> {
        match &self.value {
            Some(Aggregated::Number(x)) => Some(*x),
            Some(Aggregated::Value(Value::F64(x))) => Some(*x),
            _ => None,
        }
    }
}

/// A view of the facts (spec §2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum View {
    /// For each `(series, t)`, the fact of highest rank.
    Current,
    /// The current view over facts with position ≤ P.
    AsOf(Position),
    /// As-of P, where P is the last position whose batch `commit_time` ≤ T.
    AsOfTime(Timestamp),
    /// Every fact, in `t` then position order.
    History,
}

/// The resolution of a chart query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Resolution {
    /// Raw facts when they number at most `max_points`; otherwise the finest
    /// of 1 minute, 15 minutes and whole multiples of 15 minutes that yields
    /// at most `max_points` buckets.
    Auto {
        /// Most points wanted.
        max_points: usize,
    },
    /// Buckets of a fixed width, aligned to UTC.
    Step(Duration),
}

/// One chart bucket: enough to show gaps and spikes.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Bucket {
    /// Start.
    pub from: Timestamp,
    /// End.
    pub to: Timestamp,
    /// Readings.
    pub count: u64,
    /// Smallest reading starting in the bucket, for a numeric series; `None`
    /// when no reading starts in it.
    pub min: Option<Value>,
    /// Largest reading starting in the bucket, for a numeric series; `None`
    /// when no reading starts in it.
    pub max: Option<Value>,
    /// Time-weighted mean of a gauge, which a value carried in from before
    /// the bucket can give with no reading in it.
    pub mean: Option<f64>,
    /// Time with a valid value.
    pub coverage: Duration,
    /// Flags.
    pub flags: ResultFlags,
}

/// The result of a chart query.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ChartData {
    /// Raw facts of the current view.
    Facts(Vec<Fact>),
    /// Buckets of one width.
    Buckets {
        /// Bucket width.
        step: Duration,
        /// Buckets in time order.
        buckets: Vec<Bucket>,
    },
}

/// A series' live value (spec §2.3).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct LiveValue {
    /// The live-state fact.
    pub fact: Fact,
    /// Whether the series is stale (spec §5.4 live staleness).
    pub stale: bool,
}

/// Live state (spec §2.3).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Live {
    /// The latest valid value.
    Value(LiveValue),
    /// No live state (no valid fact yet, or a forecast series).
    None,
    /// During a restore, the fact may not be local yet. Never produced by the
    /// model backend.
    Unknown,
}

/// The facts of a query, streamed (`docs/api.md` Reading). The model backend
/// holds them in memory already; the engine streams them block by block.
#[derive(Debug)]
pub struct Facts {
    inner: std::vec::IntoIter<Fact>,
}

impl Facts {
    pub(crate) fn new(facts: Vec<Fact>) -> Self {
        Facts {
            inner: facts.into_iter(),
        }
    }
}

impl Iterator for Facts {
    type Item = Result<Fact, QueryError>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(Ok)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// What a reader sees after [`Reader::refresh`](crate::Reader::refresh).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReaderState {
    /// The durable mark's position (in memory with the model backend).
    pub durable: Position,
    /// The manifest epoch held.
    pub epoch: u64,
    /// The generation.
    pub generation: u32,
    /// The mark's reset stamp: with `durable`, the saved point to keep (always zero with the model backend,
    /// which has no resets).
    pub stamp: crate::types::ResetStamp,
    /// When the lease was last renewed.
    pub lease_renewed_at: Timestamp,
}

/// Static reads at one cut (`docs/api.md` Snapshot): live state, series and losses as of one durable position, with
/// `now` taken when it was made. Owned, `Send + 'static` and cheap to clone. With the durable backend it ends with
/// [`QueryError::CutExpired`] once its cut is `cut_max_age` old or its lease was found gone (spec §7 Readers); the
/// caller takes another.
#[derive(Clone, Debug)]
pub struct Snapshot {
    inner: SnapshotInner,
}

#[derive(Clone, Debug)]
enum SnapshotInner {
    Model(Arc<ModelSnapshot>),
    Durable(Arc<DurableSnapshot>),
}

/// The model backend's results, all taken when the snapshot was made: the model holds no cut.
#[derive(Debug)]
pub(crate) struct ModelSnapshot {
    pub(crate) state: ReaderState,
    pub(crate) now: crate::time::Timestamp,
    pub(crate) live: BTreeMap<SeriesId, Result<Live, QueryError>>,
    pub(crate) series: BTreeMap<SeriesId, Result<SeriesInfo, QueryError>>,
    /// Every name of a series, with what resolving it gives.
    pub(crate) names: BTreeMap<String, SeriesId>,
    pub(crate) losses: Vec<LossReport>,
}

#[derive(Debug)]
struct DurableSnapshot {
    reader: Arc<crate::durable::DurableReader>,
    view: Arc<crate::durable::SnapshotView>,
    now: i64,
}

impl Snapshot {
    pub(crate) fn model(m: ModelSnapshot) -> Self {
        Snapshot {
            inner: SnapshotInner::Model(Arc::new(m)),
        }
    }

    pub(crate) fn durable(reader: Arc<crate::durable::DurableReader>) -> Result<Self, QueryError> {
        let now = reader.now();
        let view = reader.snapshot()?;
        Ok(Snapshot {
            inner: SnapshotInner::Durable(Arc::new(DurableSnapshot { reader, view, now })),
        })
    }

    /// Runs `f` on a durable snapshot's view while its cut is live.
    fn on<R>(
        d: &DurableSnapshot,
        f: impl FnOnce(&crate::durable::SnapshotView) -> Result<R, QueryError>,
    ) -> Result<R, QueryError> {
        d.reader.check(&d.view)?;
        let r = f(&d.view);
        d.reader.check(&d.view)?;
        r
    }

    /// Live state at the snapshot's cut and `now` (spec §2.3).
    pub fn live(&self, id: SeriesId) -> Result<Live, QueryError> {
        match &self.inner {
            SnapshotInner::Model(m) => m
                .live
                .get(&id)
                .map_or(Err(QueryError::UnknownSeries), copied),
            SnapshotInner::Durable(d) => {
                Self::on(d, |v| crate::durable::live_at(&d.reader, v, id, d.now))
            }
        }
    }

    /// What the store knew about a series at the snapshot's cut, by id or by name.
    pub fn series(&self, key: impl Into<SeriesKey>) -> Result<SeriesInfo, QueryError> {
        let key = key.into();
        match &self.inner {
            SnapshotInner::Model(m) => {
                let id = match &key {
                    SeriesKey::Id(id) => Some(*id),
                    SeriesKey::Name(n) => m.names.get(n).copied(),
                };
                id.and_then(|id| m.series.get(&id))
                    .map_or(Err(QueryError::UnknownSeries), copied)
            }
            SnapshotInner::Durable(d) => Self::on(d, |v| crate::durable::series_at(v, &key)),
        }
    }

    /// Every loss report the files held at the snapshot's cut.
    pub fn losses(&self) -> Result<Vec<LossReport>, QueryError> {
        match &self.inner {
            SnapshotInner::Model(m) => Ok(m.losses.clone()),
            SnapshotInner::Durable(d) => Self::on(d, |v| Ok(crate::durable::losses_at(v))),
        }
    }

    /// The series `selection` reads at the snapshot's cut, in its canonical order: the members of a static `Latest`.
    pub fn select(&self, selection: &Selection) -> Result<Vec<SeriesId>, QueryError> {
        selection.check()?;
        match &self.inner {
            SnapshotInner::Model(m) => {
                let known: Vec<(SeriesId, &[String], bool)> = m
                    .series
                    .iter()
                    .filter_map(|(id, i)| {
                        let i = i.as_ref().ok()?;
                        Some((*id, i.names.as_slice(), i.retired_at.is_some()))
                    })
                    .collect();
                Ok(selection.resolve(&known))
            }
            SnapshotInner::Durable(d) => Self::on(d, |v| {
                let known: Vec<(SeriesId, &[String], bool)> = v
                    .latest()
                    .registry()
                    .iter()
                    .map(|s| (SeriesId(s.def.id), s.names.as_slice(), s.retired()))
                    .collect();
                Ok(selection.resolve(&known))
            }),
        }
    }

    /// What the reader held at the snapshot's cut.
    pub fn state(&self) -> ReaderState {
        match &self.inner {
            SnapshotInner::Model(m) => m.state,
            SnapshotInner::Durable(d) => crate::durable::state_at(&d.view),
        }
    }

    /// The `now` the snapshot's live state is judged at (spec §6 `now`).
    pub fn now(&self) -> crate::time::Timestamp {
        match &self.inner {
            SnapshotInner::Model(m) => m.now,
            SnapshotInner::Durable(d) => crate::time::Timestamp(d.now),
        }
    }
}

/// Which series a query reads (`docs/api.md` Live reads): every series an entry of `select` selects, in the order of
/// the entries, each once. A series selected by name or by `All` must not be retired.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Selection {
    /// The entries; [`Select::Names`] patterns follow the routing grammar (spec §4 Funnel).
    pub select: Vec<Select>,
}

impl Selection {
    /// The series these entries select.
    pub fn new(select: impl IntoIterator<Item = Select>) -> Self {
        Selection {
            select: select.into_iter().collect(),
        }
    }

    /// Every series not retired.
    pub fn all() -> Self {
        Selection::new([Select::All])
    }

    /// Whether a series with these names, retired or not, is selected (Q-895: `Tail` asks this of each fact, with
    /// what was known at its position).
    pub(crate) fn selects(&self, id: SeriesId, names: &[String], retired: bool) -> bool {
        !retired
            && self.select.iter().any(|s| match s {
                Select::Ids(ids) => ids.contains(&id),
                Select::Names(p) => names.iter().any(|n| crate::pattern::matches(p, n)),
                Select::All => true,
                Select::None => false,
            })
    }

    /// The selected series among `known` (id order, with names and whether retired), in the selection's order: the
    /// canonical order of `LiveEvent::Joined`'s `index` (Q-1012).
    pub(crate) fn resolve(&self, known: &[(SeriesId, &[String], bool)]) -> Vec<SeriesId> {
        let mut out: Vec<SeriesId> = Vec::new();
        let push = |id: SeriesId, out: &mut Vec<SeriesId>| {
            if !out.contains(&id) {
                out.push(id);
            }
        };
        for s in &self.select {
            match s {
                Select::Ids(ids) => {
                    for id in ids {
                        if known.iter().any(|(k, _, retired)| k == id && !retired) {
                            push(*id, &mut out);
                        }
                    }
                }
                Select::None => {}
                one => {
                    let sel = Selection::new([one.clone()]);
                    for (id, names, retired) in known {
                        if sel.selects(*id, names, *retired) {
                            push(*id, &mut out);
                        }
                    }
                }
            }
        }
        out
    }

    /// Refuses a pattern outside the routing grammar.
    pub(crate) fn check(&self) -> Result<(), QueryError> {
        for s in &self.select {
            if let Select::Names(p) = s {
                crate::pattern::check(p).map_err(|e| QueryError::Invalid {
                    reason: format!("pattern {p:?}: {e}"),
                })?;
            }
        }
        Ok(())
    }
}

impl From<Select> for Selection {
    fn from(s: Select) -> Self {
        Selection::new([s])
    }
}

/// A query a [`LiveSession`](crate::LiveSession) keeps up to date (`docs/api.md` Live reads).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Query {
    /// The live state of each selected series (spec §2.3), as [`Snapshot::live`] gives it.
    Latest(Selection),
    /// The facts of the selected series as they become durable, in position order; opened at the cut of its
    /// `Opened`. Live only.
    Tail {
        /// The series, chosen for each fact by what was known at its position (Q-895).
        sel: Selection,
    },
}

/// Limits of one live read (`docs/api.md` Live reads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LiveOptions {
    /// Most facts a `Tail` carries in one batch; the batch's cut stops at the last (default 10,000).
    pub max_rows: usize,
    /// Facts between a `Tail` and the mark past which it gets `Lagged` and continues at the mark (default
    /// 1,000,000).
    pub max_backlog_rows: u64,
    /// The same in bytes held (default 64 MiB).
    pub max_backlog_bytes: u64,
}

impl Default for LiveOptions {
    fn default() -> Self {
        LiveOptions {
            max_rows: 10_000,
            max_backlog_rows: 1_000_000,
            max_backlog_bytes: 64 << 20,
        }
    }
}

impl LiveOptions {
    /// These options with `max_rows`, the other limits unchanged.
    pub fn max_rows(mut self, n: usize) -> Self {
        self.max_rows = n;
        self
    }

    /// These options with both backlog limits.
    pub fn max_backlog(mut self, rows: u64, bytes: u64) -> Self {
        self.max_backlog_rows = rows;
        self.max_backlog_bytes = bytes;
        self
    }
}

/// How a session paces its batches (`docs/api.md` Live reads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionOptions {
    /// Least time between two batches (default 200 ms, at least 20 ms), so that a burst is one update.
    pub min_batch_interval: Duration,
    /// How often a reader process reads the mark while waiting (default 100 ms, at least 10 ms); the writer's
    /// process is woken when the mark moves.
    pub poll_interval: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            min_batch_interval: Duration::from_millis(200),
            poll_interval: Duration::from_millis(100),
        }
    }
}

impl SessionOptions {
    /// Options with these intervals.
    pub fn new(min_batch_interval: Duration, poll_interval: Duration) -> Self {
        SessionOptions {
            min_batch_interval,
            poll_interval,
        }
    }

    pub(crate) fn check(&self) -> Result<(), QueryError> {
        let low = |what: &str, least: u64| QueryError::Invalid {
            reason: format!("{what} must be at least {least} ms"),
        };
        if self.min_batch_interval < Duration::from_millis(20) {
            return Err(low("min_batch_interval", 20));
        }
        if self.poll_interval < Duration::from_millis(10) {
            return Err(low("poll_interval", 10));
        }
        Ok(())
    }
}

/// A live read of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LiveId(pub(crate) u64);

/// What one call of [`LiveSession::next`](crate::LiveSession::next) brings: every live read of the session at one
/// durable position and one `now`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct UpdateBatch {
    /// The batch's cut: every live read is as of this position.
    pub durable: Position,
    /// The `now` its values are judged at (spec §6 `now`).
    pub now: Timestamp,
    /// Per live read, in the order `Reset`, `Gap` or `Lagged`, `Opened`, `Joined` and `Left`, then `Updated` and
    /// `Appended`, then `Closed`.
    pub events: Vec<(LiveId, LiveEvent)>,
}

/// A change to a live read's result (`docs/api.md` Live reads).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum LiveEvent {
    /// The result at the batch's cut: replaces whatever the consumer held.
    Opened(Answer),
    /// New facts at the end.
    Appended(Appended),
    /// A value replaced in place.
    Updated(Updated),
    /// A reset voided every position above `above`: the consumer drops what it holds above it; an `Opened` follows.
    Reset {
        /// The last position still valid.
        above: Position,
    },
    /// Positions found lost (spec §3 Recovery rule 2): the consumer drops what it holds between them.
    Gap {
        /// First lost position.
        first: Position,
        /// Last lost position.
        last: Position,
    },
    /// The consumer fell behind by more than the backlog limit: the facts between these positions were not sent
    /// and may be read statically.
    Lagged {
        /// First position not sent.
        from: Position,
        /// Last position not sent: the batch's cut.
        to: Position,
    },
    /// A series entered the selection (a definition, or a name given); `index` is its place in the canonical order.
    Joined {
        /// The series.
        series: SeriesId,
        /// What the store knew about it at the cut.
        info: SeriesInfo,
        /// Its place among the selected series.
        index: usize,
        /// Its share of the result.
        part: Part,
    },
    /// A series left the selection (a retirement); the consumer drops it.
    Left {
        /// The series.
        series: SeriesId,
    },
    /// The live read ended.
    Closed(CloseReason),
}

/// A live read's result at its opening cut.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Answer {
    /// `Latest`: each selected series' live state, in the canonical order.
    Latest(Vec<(SeriesId, Live)>),
    /// `Tail`: nothing yet; facts follow in `Appended`.
    Tail,
}

/// New facts at the end of a live read's result.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Appended {
    /// `Tail`: facts in position order.
    Facts(Vec<Fact>),
}

/// A value replaced in a live read's result.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Updated {
    /// `Latest`: a series' live state.
    Live {
        /// The series.
        series: SeriesId,
        /// Its live state now.
        live: Live,
    },
}

/// A joining series' share of a live read's result.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Part {
    /// `Latest`: its live state.
    Live(Live),
}

/// Why a live read ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseReason {
    /// The session was not polled for `cut_max_age`.
    Idle,
}

/// The rank of an event in a live read's part of a batch (the `UpdateBatch::events` order).
pub(crate) fn event_rank(e: &LiveEvent) -> u8 {
    match e {
        LiveEvent::Reset { .. } | LiveEvent::Gap { .. } | LiveEvent::Lagged { .. } => 0,
        LiveEvent::Opened(_) => 1,
        LiveEvent::Joined { .. } | LiveEvent::Left { .. } => 2,
        LiveEvent::Updated(_) | LiveEvent::Appended(_) => 3,
        LiveEvent::Closed(_) => 4,
    }
}

fn copied<T: Clone>(r: &Result<T, QueryError>) -> Result<T, QueryError> {
    match r {
        Ok(v) => Ok(v.clone()),
        Err(e) => Err(crate::durable::copy_error(e)),
    }
}
