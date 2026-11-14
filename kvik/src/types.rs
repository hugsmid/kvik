//! Identifiers, positions, readings and facts (spec §2.2).

use std::fmt;

use jiff::Timestamp;

use crate::error::Refusal;

/// Series identifier: assigned once by the registry, never reused (spec §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesId(u32);

impl SeriesId {
    /// The identifier with this number, as [`SeriesId::get`] returns it.
    pub const fn from_raw(n: u32) -> Self {
        SeriesId(n)
    }

    /// Its number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

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
    /// A 64-bit float (gauge, counter or interval series of value type
    /// [`ValueType::F64`](crate::ValueType::F64)). NaN and ±∞ are recorded as
    /// bad quality without a value (spec §2.5).
    F64(f64),
    /// An exact integer in units of the series' decimal scale (value type
    /// [`ValueType::Decimal`](crate::ValueType::Decimal)).
    Decimal(i64),
    /// A boolean state.
    Bool(bool),
    /// A named state: an index into the series' declared names.
    State(u16),
    /// Text, at most 255 bytes of UTF-8.
    Text(String),
}

/// Quality of a reading, from best to worst (spec §2.2). The engine may lower
/// it, never raise it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// Event time, UTC. For an interval series, the start of its interval.
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
pub enum Intent {
    /// Normal: identical repeats are dropped as duplicates.
    #[default]
    Normal,
    /// Revise: bypasses the duplicate check, for a deliberate return to an
    /// earlier value; refused on a versioned series.
    Revise,
}

/// The result of an emit: the batch is durable when it is returned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ack {
    /// Position of the first fact recorded, if any.
    pub first: Option<Position>,
    /// Position of the last fact recorded, if any.
    pub last: Option<Position>,
    /// Facts recorded.
    pub recorded: u32,
    /// Readings dropped as identical repeats (spec §2.4).
    pub duplicates_dropped: u32,
    /// Readings refused, by index in the batch.
    pub refused: Vec<(usize, Refusal)>,
}

/// Flags the engine stamps on a fact; immutable (spec §2.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FactFlags {
    /// Observed series: `t` more than 60 s after `received_at`.
    pub future: bool,
    /// Late by either clause of spec §2.2.
    pub late: bool,
    /// Submitted with [`Intent::Revise`].
    pub revise: bool,
    /// `t` was stamped by the host clock.
    pub host_time: bool,
    /// Committed before the clock was synchronised (spec §2.5).
    pub unsynced_clock: bool,
    /// Recorded without a value: NaN, ±∞ or a negative counter value.
    pub no_value: bool,
}

/// A recorded fact (spec §2.2). Facts are immutable.
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    /// Series.
    pub series: SeriesId,
    /// Event time.
    pub t: Timestamp,
    /// Position.
    pub position: Position,
    /// Host clock at commit.
    pub received_at: Timestamp,
    /// Value; `None` when recorded without one.
    pub value: Option<Value>,
    /// Quality after stamping.
    pub quality: Quality,
    /// Quality as the adapter supplied it.
    pub supplied_quality: Quality,
    /// The source's revision time or version, in a versioned series.
    pub changed_at: Option<u64>,
    /// Engine flags.
    pub flags: FactFlags,
}

impl Fact {
    /// The numeric value, for a value of type `F64`.
    pub fn number(&self) -> Option<f64> {
        match self.value {
            Some(Value::F64(v)) => Some(v),
            _ => None,
        }
    }
}
