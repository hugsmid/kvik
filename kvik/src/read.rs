//! Query types (spec §2.3, §5.7, §6; `docs/api.md` Reading).

use std::time::Duration;

use crate::error::QueryError;
use crate::time::{LocalDate, Span, TimeRange, Timestamp, ZoneName};

use crate::types::{Fact, Position, Value};

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
    /// The durable mark's position.
    pub durable: Position,
    /// The manifest epoch held.
    pub epoch: u64,
    /// The generation.
    pub generation: u32,
    /// When the lease was last renewed.
    pub lease_renewed_at: Timestamp,
}
