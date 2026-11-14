//! Identifiers, positions, readings, acknowledgements and facts (spec §2.2;
//! `docs/api.md` Core types, Writing).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::error::{EmitError, Refusal};
use crate::time::Timestamp;

/// Series identifier: assigned once by the registry, never reused (spec §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesId(pub u32);

impl fmt::Display for SeriesId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// Position of a log entry (spec §2.2): a generation and a sequence within
/// it, ordered lexicographically. `(g, 0)` means "before the first entry of
/// generation g"; [`Position::ORIGIN`] means none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    /// Generation: 0 until the first restore.
    pub generation: u32,
    /// Sequence within the generation, from 1.
    pub sequence: u64,
}

impl Position {
    /// Before every position.
    pub const ORIGIN: Position = Position {
        generation: 0,
        sequence: 0,
    };

    /// A position.
    pub const fn new(generation: u32, sequence: u64) -> Self {
        Position {
            generation,
            sequence,
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.generation, self.sequence)
    }
}

/// A reading's value. Which variants a series accepts follows from its kind
/// and value type ([`Kind`](crate::Kind)).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// A 64-bit float. NaN and ±∞ are recorded as quality bad with no value
    /// (spec §2.5). A decimal series takes it too, converted to its shortest
    /// decimal representation.
    F64(f64),
    /// `units × 10^−scale`, for a decimal series.
    Decimal {
        /// Units.
        units: i64,
        /// Decimal scale.
        scale: u8,
    },
    /// A named state: an index into the series' declared names.
    State(u16),
    /// A boolean state.
    Bool(bool),
    /// Text, at most 255 bytes of UTF-8.
    Text(String),
    /// No value (spec §2.5).
    Absent,
}

/// Quality of a reading, from best to worst (spec §2.2). The engine may lower
/// it, never raise it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Quality {
    /// Good.
    #[default]
    Good,
    /// Estimated by the source.
    Estimated,
    /// Suspect.
    Suspect,
    /// Bad: kept in views, ignored by summaries (spec §2.5).
    Bad,
}

/// Which clock stamped a reading's `t` (spec §2.2 `t_source`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TimeSource {
    /// The device's own clock.
    #[default]
    Device,
    /// The host's clock: such readings wait while the clock is not
    /// synchronised (spec §2.5).
    Host,
}

/// A reading as an adapter emits it.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// Series.
    pub series: SeriesId,
    /// Event time. For an interval series, the start of its interval.
    pub t: Timestamp,
    /// Value.
    pub value: Value,
    /// Quality as the source reports it.
    pub quality: Quality,
    /// Which clock stamped `t`.
    pub source: TimeSource,
    /// The source's revision time or version (spec §2.2): carried by every
    /// reading of a versioned series and by no other.
    pub changed_at: Option<u64>,
}

impl Reading {
    /// A good-quality, device-stamped reading.
    pub fn new(series: SeriesId, t: Timestamp, value: Value) -> Self {
        Reading {
            series,
            t,
            value,
            quality: Quality::Good,
            source: TimeSource::Device,
            changed_at: None,
        }
    }

    /// The same reading with another quality.
    pub fn with_quality(self, quality: Quality) -> Self {
        Reading { quality, ..self }
    }

    /// The same reading stamped by the host clock.
    pub fn host_stamped(self) -> Self {
        Reading {
            source: TimeSource::Host,
            ..self
        }
    }

    /// The same reading with `changed_at`, for a versioned series.
    pub fn changed_at(self, changed_at: u64) -> Self {
        Reading {
            changed_at: Some(changed_at),
            ..self
        }
    }
}

/// How a batch is submitted (spec §2.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Intent {
    /// Normal: identical repeats are dropped as duplicates.
    #[default]
    Normal,
    /// Revise: bypasses the duplicate check; refused on a versioned series.
    Revise,
}

/// What happened to one reading, decided at commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Recorded as a fact.
    Recorded {
        /// Its position.
        position: Position,
        /// Outranked when committed (spec §4 Outranked facts).
        outranked: bool,
        /// Recorded although the duplicate check could not run (spec §2.4).
        unchecked: bool,
        /// A decimal value rounded to the series' scale.
        rounded: bool,
        /// A decimal value that does not fit `i64` units at the series'
        /// scale, recorded with quality bad and no value (spec §2.5).
        out_of_range: bool,
    },
    /// An identical repeat, dropped (spec §2.4).
    Duplicate,
    /// Refused.
    Refused(Refusal),
}

/// The acknowledgement of a call: one outcome per reading, by index. Returned
/// once the batch is committed (synced in the durable engine; held in memory
/// only with the model backend).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Ack {
    /// One outcome per reading of the call, in order.
    pub outcomes: Vec<Outcome>,
}

impl Ack {
    pub(crate) fn new(outcomes: Vec<Outcome>) -> Self {
        Ack { outcomes }
    }

    fn positions(&self) -> impl Iterator<Item = Position> + '_ {
        self.outcomes.iter().filter_map(|o| match o {
            Outcome::Recorded { position, .. } => Some(*position),
            _ => None,
        })
    }

    /// The first position recorded, if any.
    pub fn first(&self) -> Option<Position> {
        self.positions().min()
    }

    /// The last position recorded, if any.
    pub fn last(&self) -> Option<Position> {
        self.positions().max()
    }

    /// Readings dropped as identical repeats.
    pub fn duplicates_dropped(&self) -> u32 {
        let n = self
            .outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Duplicate))
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// The refused readings, by index.
    pub fn refused(&self) -> impl Iterator<Item = (usize, Refusal)> + '_ {
        self.outcomes
            .iter()
            .enumerate()
            .filter_map(|(i, o)| match o {
                Outcome::Refused(r) => Some((i, *r)),
                _ => None,
            })
    }

    /// Facts recorded.
    pub fn recorded(&self) -> usize {
        self.positions().count()
    }
}

/// A batch accepted by [`Store::try_emit`](crate::Store::try_emit): a future
/// of its [`Ack`], runtime-neutral, that may be dropped (dropping cancels
/// nothing). With the model backend it is ready at once.
#[derive(Debug)]
#[must_use = "awaiting a Pending gives the outcomes; dropping it cancels nothing"]
pub struct Pending {
    result: Option<Result<Ack, EmitError>>,
}

impl Pending {
    pub(crate) fn ready(result: Result<Ack, EmitError>) -> Self {
        Pending {
            result: Some(result),
        }
    }
}

impl Future for Pending {
    type Output = Result<Ack, EmitError>;
    /// # Panics
    /// Panics if polled after it completed.
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(self.result.take().expect("Pending polled after completion"))
    }
}

/// Flags the engine stamps on a fact; immutable (spec §2.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FactFlags {
    /// Observed series: `t` more than the future tolerance after
    /// `received_at`.
    pub future: bool,
    /// Late by either clause of spec §2.2.
    pub late: bool,
    /// Submitted with [`Intent::Revise`].
    pub revise: bool,
    /// `t` was stamped by the host clock.
    pub host_time: bool,
    /// Committed before the clock was synchronised (spec §2.5).
    pub unsynced_clock: bool,
    /// Recorded without a value: NaN, ±∞, `Absent` or a negative counter
    /// value.
    pub no_value: bool,
    /// A decimal value rounded to the series' scale.
    pub rounded: bool,
}

/// A recorded fact (spec §2.2). Facts are immutable.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Fact {
    /// Series.
    pub series: SeriesId,
    /// Event time.
    pub t: Timestamp,
    /// Position.
    pub position: Position,
    /// Host clock at commit.
    pub received_at: Timestamp,
    /// Value; [`Value::Absent`] when recorded without one.
    pub value: Value,
    /// Quality after stamping.
    pub quality: Quality,
    /// Quality as the adapter supplied it.
    pub supplied_quality: Quality,
    /// Engine flags.
    pub flags: FactFlags,
    /// The source's revision time or version, in a versioned series.
    pub changed_at: Option<u64>,
}

/// A fact as [`Reader::facts`](crate::Reader::facts) returns it.
pub type FactRow = Fact;

impl Fact {
    /// The numeric value, for a value of type `F64`.
    pub fn number(&self) -> Option<f64> {
        match self.value {
            Value::F64(v) => Some(v),
            _ => None,
        }
    }
}
