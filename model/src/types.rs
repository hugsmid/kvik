//! Basic data types: series identifiers, kinds, values, quality, flags and facts.

use std::fmt;

/// Event or wall-clock time: UTC nanoseconds since the Unix epoch.
pub type Nanos = i64;

/// Commit-order position of a log entry. Positions start at 1.
pub type Position = u64;

/// Nanoseconds in one second.
pub const SECOND: Nanos = 1_000_000_000;
/// Nanoseconds in one minute.
pub const MINUTE: Nanos = 60 * SECOND;
/// Nanoseconds in fifteen minutes.
pub const QUARTER: Nanos = 15 * MINUTE;
/// Nanoseconds in one hour.
pub const HOUR: Nanos = 60 * MINUTE;
/// Nanoseconds in one day of 24 hours.
pub const DAY: Nanos = 24 * HOUR;

/// Exact rational number used for sums and integrals so that the model is free
/// of floating-point rounding. Every finite `f64` is a dyadic rational, so this
/// is lossless.
pub type Exact = num_rational::BigRational;

/// Converts a finite `f64` to an exact rational.
///
/// # Panics
/// Panics on NaN or infinity; ingest refuses those before they reach a summary.
pub fn exact(v: f64) -> Exact {
    Exact::from_float(v).expect("finite value")
}

/// Converts an integer count of nanoseconds to an exact rational.
pub fn exact_ns(ns: i128) -> Exact {
    Exact::from_integer(num_bigint::BigInt::from(ns))
}

/// Floor of `t` to a multiple of `step` (works for negative times).
pub fn floor_to(t: Nanos, step: Nanos) -> Nanos {
    t.div_euclid(step) * step
}

/// Series identifier, assigned once by the registry and never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesId(pub u32);

impl fmt::Display for SeriesId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// The kind of a series (spec §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// Instantaneous value.
    Gauge,
    /// Cumulative value that may reset.
    Counter,
    /// Named or boolean state.
    State,
    /// Short text, at most 255 bytes.
    Text,
}

/// Value type of a series, fixed for its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueType {
    /// A finite 64-bit float.
    Number,
    /// A boolean.
    Boolean,
    /// A named state.
    Name,
    /// Text up to 255 bytes.
    Text,
}

impl Kind {
    /// Whether `vt` is a permitted value type for this kind.
    pub fn accepts(self, vt: ValueType) -> bool {
        matches!(
            (self, vt),
            (Kind::Gauge | Kind::Counter, ValueType::Number)
                | (Kind::State, ValueType::Boolean | ValueType::Name)
                | (Kind::Text, ValueType::Text)
        )
    }
}

/// Maximum length of a text value in bytes.
pub const MAX_TEXT_BYTES: usize = 255;

/// A reading's value.
#[derive(Clone, Debug)]
pub enum Value {
    /// Numeric value (gauge, counter).
    Num(f64),
    /// Boolean state.
    Bool(bool),
    /// Named state or text.
    Str(String),
    /// No value: a NaN or infinite input, recorded with bad quality (§2.5).
    Absent,
}

impl Value {
    /// The value type this value belongs to, given the series kind.
    pub fn fits(&self, vt: ValueType) -> bool {
        matches!(
            (self, vt),
            (Value::Num(_), ValueType::Number)
                | (Value::Bool(_), ValueType::Boolean)
                | (Value::Str(_), ValueType::Name | ValueType::Text)
                | (Value::Absent, _)
        )
    }

    /// Identity used by the duplicate rule (§2.4). Numbers compare by bit
    /// pattern, so `0.0` and `-0.0` are different values.
    pub fn identical(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Num(a), Value::Num(b)) => a.to_bits() == b.to_bits(),
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Absent, Value::Absent) => true,
            _ => false,
        }
    }

    /// Numeric value, if any.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(v) => Some(*v),
            _ => None,
        }
    }

    /// The state key for a state-kind value.
    pub fn state_key(&self) -> StateKey {
        match self {
            Value::Bool(b) => StateKey::Bool(*b),
            Value::Str(s) => StateKey::Name(s.clone()),
            Value::Num(v) => StateKey::Name(format!("{v}")),
            Value::Absent => StateKey::Name(String::new()),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.identical(other)
    }
}

/// Key of a state in time-in-state summaries.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateKey {
    /// Boolean state.
    Bool(bool),
    /// Named state.
    Name(String),
}

/// Quality of a reading, ordered by severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Quality {
    /// Good.
    Good,
    /// Estimated by the source.
    Estimated,
    /// Suspect.
    Suspect,
    /// Bad: kept in views, excluded from summaries.
    Bad,
}

impl Quality {
    /// The more severe of two qualities.
    pub fn worst(self, other: Quality) -> Quality {
        self.max(other)
    }
}

/// Flags stamped by the engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags {
    /// Event time was more than the future tolerance ahead of `received_at`.
    pub future: bool,
    /// The reading arrived late (§2.2): earlier than the series' latest
    /// recorded `t` by more than 2 × S, or older than the lateness bound.
    pub late: bool,
    /// Submitted with `revise` intent, bypassing the duplicate check (§2.4).
    pub revise: bool,
}

/// A reading as an adapter emits it.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// Series.
    pub series: SeriesId,
    /// Event time.
    pub t: Nanos,
    /// Value.
    pub value: Value,
    /// Quality as reported by the source.
    pub quality: Quality,
}

impl Reading {
    /// A good-quality numeric reading.
    pub fn num(series: SeriesId, t: Nanos, v: f64) -> Self {
        Reading {
            series,
            t,
            value: Value::Num(v),
            quality: Quality::Good,
        }
    }
}

/// A recorded fact (§2.2). Facts are immutable.
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    /// Series.
    pub series: SeriesId,
    /// Event time.
    pub t: Nanos,
    /// Commit-order position.
    pub position: Position,
    /// Host clock at commit.
    pub received_at: Nanos,
    /// Value.
    pub value: Value,
    /// Quality after stamping (a future reading is at least suspect).
    pub quality: Quality,
    /// Engine flags.
    pub flags: Flags,
}

impl Fact {
    /// Whether this fact takes part in summaries (§2.3: bad quality is excluded).
    pub fn summarised(&self) -> bool {
        self.quality != Quality::Bad
    }
}
