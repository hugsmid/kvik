//! Series definitions (spec §2.1).

use std::time::Duration;

use crate::time::{LocalTime, Timestamp, ZoneName};
use crate::types::SeriesId;

/// Value type of a numeric series, fixed for its life (the scale of a
/// decimal series can only be raised).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ValueType {
    /// A 64-bit float.
    #[default]
    F64,
    /// An `i64` in units of 10<sup>−scale</sup>: exact sums (spec §5.6).
    /// Not supported by the model backend yet.
    Decimal {
        /// Decimal scale.
        scale: u8,
    },
}

impl ValueType {
    /// A decimal type with the default scale of 4.
    pub const fn decimal() -> Self {
        ValueType::Decimal { scale: 4 }
    }
}

/// What a decimal series does with a value that has more decimal places than
/// its scale (spec §2.1 `excess precision`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExcessPrecision {
    /// Rounded half to even and flagged `rounded`.
    #[default]
    Round,
    /// Refused with `Refusal::ExcessPrecision`.
    Refuse,
    /// The scale raised as far as needed, up to `max_scale`; beyond it
    /// rounded.
    AutoRaise {
        /// Highest scale raised to.
        max_scale: u8,
    },
}

/// The values of a state series.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StateValues {
    /// A boolean state.
    Bool,
    /// Named states; a reading carries an index into the names.
    Named(Vec<String>),
}

/// The calendar unit of a calendar-length interval series (spec §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CalendarUnit {
    /// A local day from one day start to the next (default midnight).
    Day {
        /// Local time at which each day starts.
        start: LocalTime,
    },
    /// A local month.
    Month,
}

/// The length of an interval series' intervals (spec §2.1).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IntervalLength {
    /// A fixed length `d > 0` with a phase, `0 ≤ phase < d`: readings are at
    /// `t ≡ phase (mod d)` on UTC nanoseconds.
    Fixed {
        /// Length.
        d: Duration,
        /// Phase.
        phase: Duration,
    },
    /// A local day or month in a zone fixed for the series. Not supported by
    /// the model backend yet.
    Calendar {
        /// Day or month.
        unit: CalendarUnit,
        /// The interval zone, independent of the site zone.
        zone: ZoneName,
    },
}

/// The kind of a series (spec §2.1), with its value type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Kind {
    /// An instantaneous value.
    Gauge {
        /// Value type.
        value: ValueType,
    },
    /// A cumulative value that may reset (spec §5.5).
    Counter {
        /// Value type.
        value: ValueType,
    },
    /// A named or boolean state.
    State {
        /// Its values.
        values: StateValues,
    },
    /// UTF-8 text, at most 255 bytes.
    Text,
    /// An amount over an interval of fixed or calendar length.
    Interval {
        /// Value type.
        value: ValueType,
        /// Interval length.
        length: IntervalLength,
    },
}

impl Default for Kind {
    fn default() -> Self {
        Kind::Gauge {
            value: ValueType::F64,
        }
    }
}

/// When a series' values apply (spec §2.1 `timing`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Timing {
    /// Values observed at their `t`.
    #[default]
    Observed,
    /// Values known in advance that take effect at their `t`.
    Scheduled,
    /// Values about future times, re-issued over time; must be versioned with
    /// [`ChangedAtKind::Time`].
    Forecast,
}

/// What `changed_at` holds in a versioned series (spec §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChangedAtKind {
    /// UTC nanoseconds: the revision or issue time.
    Time,
    /// An opaque counter.
    Version,
}

/// Where a series' raw detail is kept (spec §2.1, §8 Retention class).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RetentionClass {
    /// Kept under the general tier rules.
    #[default]
    LongTerm,
    /// Raw detail older than the period leaves the fast disk once verified
    /// elsewhere; nothing is deleted.
    Diagnostic {
        /// Retention period (default 30 days).
        period: Duration,
    },
}

/// Counter rules (spec §5.5).
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct CounterRules {
    /// The value at which the counter wraps to zero, if it does.
    pub wrap_at: Option<f64>,
    /// A decrease to below this fraction of the previous value is a reset;
    /// any other decrease is a regression (default 0.1).
    pub reset_ceiling: f64,
}

impl Default for CounterRules {
    fn default() -> Self {
        CounterRules {
            wrap_at: None,
            reset_ceiling: 0.1,
        }
    }
}

impl CounterRules {
    /// Counter rules.
    pub fn new(wrap_at: Option<f64>, reset_ceiling: f64) -> Self {
        CounterRules {
            wrap_at,
            reset_ceiling,
        }
    }
}

/// A series definition (spec §2.1). Defining a series that already exists
/// with the same definition returns its id. Build it with
/// [`SeriesSpec::new`] and the setters, or set fields on `default()`.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct SeriesSpec {
    /// Name, `<instance>/<device>/<metric>`.
    pub name: String,
    /// Kind and value type.
    pub kind: Kind,
    /// Unit, fixed for the life of the series.
    pub unit: String,
    /// Expected interval; `None` takes the kind's default (gauge and counter
    /// 60 s, interval its length, state and text none).
    pub expected_interval: Option<Duration>,
    /// Staleness limit S; `None` takes 3 × the expected interval.
    pub staleness: Option<Duration>,
    /// Record on change (spec §2.6).
    pub record_on_change: bool,
    /// Counter rules; counters only. `None` takes the defaults.
    pub counter: Option<CounterRules>,
    /// Lateness bound; `None` takes the default of 5 minutes.
    pub lateness_bound: Option<Duration>,
    /// `Some` for a versioned series (spec §2.1).
    pub versioned: Option<ChangedAtKind>,
    /// Timing.
    pub timing: Timing,
    /// Retention class.
    pub retention: RetentionClass,
    /// Excess-precision policy of a decimal series.
    pub excess_precision: ExcessPrecision,
}

impl SeriesSpec {
    /// A definition with every other field at its default.
    pub fn new(name: impl Into<String>, kind: Kind, unit: impl Into<String>) -> Self {
        SeriesSpec {
            name: name.into(),
            kind,
            unit: unit.into(),
            ..SeriesSpec::default()
        }
    }

    /// Sets the name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Sets the expected interval.
    pub fn expected_interval(mut self, d: Duration) -> Self {
        self.expected_interval = Some(d);
        self
    }

    /// Sets the staleness limit.
    pub fn staleness(mut self, d: Duration) -> Self {
        self.staleness = Some(d);
        self
    }

    /// Sets record on change.
    pub fn record_on_change(mut self, on: bool) -> Self {
        self.record_on_change = on;
        self
    }

    /// Sets the counter rules.
    pub fn counter(mut self, rules: CounterRules) -> Self {
        self.counter = Some(rules);
        self
    }

    /// Sets the lateness bound.
    pub fn lateness_bound(mut self, d: Duration) -> Self {
        self.lateness_bound = Some(d);
        self
    }

    /// Makes the series versioned.
    pub fn versioned(mut self, kind: ChangedAtKind) -> Self {
        self.versioned = Some(kind);
        self
    }

    /// Sets the timing.
    pub fn timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    /// Sets the retention class.
    pub fn retention(mut self, retention: RetentionClass) -> Self {
        self.retention = retention;
        self
    }

    /// Sets the excess-precision policy.
    pub fn excess_precision(mut self, policy: ExcessPrecision) -> Self {
        self.excess_precision = policy;
        self
    }
}

/// A change of a series' parameters from an effective time (spec §2.1).
/// Fields left `None` are unchanged.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct ParameterChange {
    /// New expected interval.
    pub expected_interval: Option<Duration>,
    /// New staleness limit.
    pub staleness: Option<Duration>,
    /// New lateness bound.
    pub lateness_bound: Option<Duration>,
    /// New counter rules.
    pub counter: Option<CounterRules>,
    /// New retention class: takes effect at its position, not at the
    /// effective time (spec §2.1).
    pub retention: Option<RetentionClass>,
}

impl ParameterChange {
    /// Sets the new expected interval.
    pub fn expected_interval(mut self, d: Duration) -> Self {
        self.expected_interval = Some(d);
        self
    }

    /// Sets the new staleness limit.
    pub fn staleness(mut self, d: Duration) -> Self {
        self.staleness = Some(d);
        self
    }

    /// Sets the new lateness bound.
    pub fn lateness_bound(mut self, d: Duration) -> Self {
        self.lateness_bound = Some(d);
        self
    }

    /// Sets the new counter rules.
    pub fn counter(mut self, rules: CounterRules) -> Self {
        self.counter = Some(rules);
        self
    }

    /// Sets the new retention class.
    pub fn retention(mut self, retention: RetentionClass) -> Self {
        self.retention = Some(retention);
        self
    }
}

/// A series named by id or by name (a name or alias of a live series).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SeriesKey {
    /// By id; retired series stay reachable this way.
    Id(SeriesId),
    /// By name or alias.
    Name(String),
}

impl From<SeriesId> for SeriesKey {
    fn from(id: SeriesId) -> Self {
        SeriesKey::Id(id)
    }
}

impl From<&str> for SeriesKey {
    fn from(name: &str) -> Self {
        SeriesKey::Name(name.to_string())
    }
}

impl From<String> for SeriesKey {
    fn from(name: String) -> Self {
        SeriesKey::Name(name)
    }
}

/// What the store knows about a series.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct SeriesInfo {
    /// Identifier.
    pub id: SeriesId,
    /// Display name: the latest name given.
    pub name: String,
    /// Every name, in the order given (the first is the defining name).
    pub names: Vec<String>,
    /// The definition as given.
    pub spec: SeriesSpec,
    /// Staleness limit in force, if any.
    pub staleness: Option<Duration>,
    /// Retirement time, if retired.
    pub retired_at: Option<Timestamp>,
    /// The greatest `t` over all recorded facts (spec §2.2).
    pub latest_t: Option<Timestamp>,
    /// Lateness reference of an observed series (spec §2.2).
    pub lateness_reference: Option<Timestamp>,
}
