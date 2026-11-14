//! Series registry (§2.1). Definitions, aliases and retirements are log entries;
//! the registry is rebuilt by applying them in position order.

use std::collections::BTreeMap;

use crate::summary::CounterRules;
use crate::types::{IntervalSpec, Kind, Nanos, SeriesId, ValueType};

/// What `changed_at` holds in a versioned series (§2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangedAtKind {
    /// UTC nanoseconds: the revision or issue time.
    Time,
    /// An opaque counter.
    Version,
}

/// When a series' values apply (§2.1 `timing`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Timing {
    /// Values observed at their `t`.
    #[default]
    Observed,
    /// Values known in advance that take effect at their `t`.
    Scheduled,
    /// Values about future times, re-issued over time; versioned with
    /// `changed_at` kind `time`.
    Forecast,
}

/// Where a series' raw detail is kept (§2.1 `retention class`, §8 Retention
/// class). It never decides which facts exist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RetentionClass {
    /// Raw detail stays on the fast disk under the general tier rules.
    #[default]
    LongTerm,
    /// Raw detail whose span ends more than `period` before `now` leaves
    /// the fast disk once verified in another tier.
    Diagnostic {
        /// Retention period (default 30 days).
        period: Nanos,
    },
}

impl RetentionClass {
    /// The diagnostic class with the default period of 30 days.
    pub const fn diagnostic() -> Self {
        RetentionClass::Diagnostic {
            period: 30 * 24 * 3_600 * 1_000_000_000,
        }
    }
}

/// Definition fields beyond kind, type, unit and staleness limit (§2.1).
/// The model fixes them for the life of a series: it has no parameter
/// entries with an effective time (see `OPEN-QUESTIONS.md`, Q-60).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SeriesOptions {
    /// `Some` for a versioned series, with what its `changed_at` holds.
    pub versioned: Option<ChangedAtKind>,
    /// Timing (§2.1): observed, scheduled or forecast.
    pub timing: Timing,
    /// Expected interval (§2.1), configuration and never learned; for a
    /// record-on-change series the keep-alive interval (§2.6). `None` takes
    /// the interval for which the given staleness limit is the default
    /// (S = 3 × expected interval), and no interval when no limit is given.
    /// It decides which tree levels are stored (§5.1 density rule).
    pub expected_interval: Option<Nanos>,
    /// Record on change (§2.6): the series records a fact when its value
    /// changes and a keep-alive fact at least once per expected interval.
    pub record_on_change: bool,
    /// Counter rules (§5.5); counters only.
    pub counter: CounterRules,
    /// Lateness bound (§2.2); `None` takes the configured default (5 min).
    pub lateness_bound: Option<Nanos>,
    /// Interval length and phase; interval series only, and required there.
    /// Its expected interval defaults to its length, and S to 3 × that; S
    /// serves only the first clause of `late` (§2.2), as an interval reading
    /// is valid for its interval and holds nothing (§5.2, §5.4).
    pub interval: Option<IntervalSpec>,
}

impl SeriesOptions {
    /// A versioned series whose `changed_at` is of the given kind.
    pub fn versioned(kind: ChangedAtKind) -> Self {
        SeriesOptions {
            versioned: Some(kind),
            ..SeriesOptions::default()
        }
    }

    /// A series of the given timing.
    pub fn timing(timing: Timing) -> Self {
        SeriesOptions {
            timing,
            ..SeriesOptions::default()
        }
    }

    /// An interval series of length `length` and phase `phase`.
    pub fn interval(length: Nanos, phase: Nanos) -> Self {
        SeriesOptions {
            interval: Some(IntervalSpec { length, phase }),
            ..SeriesOptions::default()
        }
    }

    /// A series with the given expected interval.
    pub fn expected(interval: Nanos) -> Self {
        SeriesOptions {
            expected_interval: Some(interval),
            ..SeriesOptions::default()
        }
    }
}

/// A registry event, recorded in the log.
#[derive(Clone, Debug, PartialEq)]
pub enum RegistryEvent {
    /// A new series.
    Define {
        /// Assigned identifier.
        id: SeriesId,
        /// Name, `<instance>/<device>/<metric>`.
        name: String,
        /// Kind.
        kind: Kind,
        /// Value type.
        value_type: ValueType,
        /// Unit.
        unit: String,
        /// Staleness limit (§2.1); recorded so that rebuild is deterministic.
        staleness: Option<Nanos>,
        /// Versioned and timing (§2.1).
        options: SeriesOptions,
    },
    /// An additional name for an existing series (rename).
    Alias {
        /// Series.
        id: SeriesId,
        /// New name.
        name: String,
    },
    /// A change of retention class, in force from its position (§2.1).
    Retention {
        /// Series.
        id: SeriesId,
        /// The class.
        class: RetentionClass,
    },
    /// The series is hidden from name resolution and refuses facts with
    /// `t ≥ at`; its facts stay and it stays reachable by id (§2.1).
    Retire {
        /// Series.
        id: SeriesId,
        /// Effective event time `t_r`.
        at: Nanos,
    },
}

/// A series definition.
#[derive(Clone, Debug, PartialEq)]
pub struct SeriesDef {
    /// Identifier.
    pub id: SeriesId,
    /// Names in the order given; the last is the display name.
    pub names: Vec<String>,
    /// Kind.
    pub kind: Kind,
    /// Value type.
    pub value_type: ValueType,
    /// Unit.
    pub unit: String,
    /// Staleness limit; `None` means valid until the next reading.
    pub staleness: Option<Nanos>,
    /// Versioned and timing (§2.1).
    pub options: SeriesOptions,
    /// Retention class in force (§2.1).
    pub retention: RetentionClass,
    /// Whether the series is retired.
    pub retired: bool,
    /// Effective time of the retirement, if retired.
    pub retired_at: Option<Nanos>,
}

impl SeriesDef {
    /// The expected interval (§2.1): as defined, or for a series defined
    /// with a staleness limit only, the interval for which that limit is the
    /// default (S = 3 × expected interval); `None` without either.
    pub fn expected_interval(&self) -> Option<Nanos> {
        self.options
            .expected_interval
            .or(match self.options.interval {
                Some(iv) => Some(iv.length),
                None => self.staleness.map(|s| s / 3),
            })
    }
}

/// Registry errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// The configured ceiling on series has been reached (§9 row 30).
    #[error("series ceiling of {0} reached")]
    Ceiling(usize),
    /// A live series already uses the name with a different definition.
    #[error("name {0:?} is in use by a live series with a different kind, type or unit")]
    NameInUse(String),
    /// The kind does not accept the value type.
    #[error("kind {0:?} does not accept value type {1:?}")]
    BadType(Kind, ValueType),
    /// Unknown series.
    #[error("unknown series {0}")]
    Unknown(SeriesId),
    /// The series is retired.
    #[error("series {0} is retired")]
    Retired(SeriesId),
    /// Staleness limit must be positive.
    #[error("staleness limit must be positive")]
    BadStaleness,
    /// A forecast series must be versioned with `changed_at` kind `time`
    /// (§2.1 `timing`).
    #[error("a forecast series must be versioned with changed_at kind time")]
    ForecastNotVersioned,
    /// A retention period must be positive.
    #[error("retention period must be positive")]
    BadRetention,
    /// An expected interval or lateness bound must be positive.
    #[error("expected interval and lateness bound must be positive")]
    BadInterval,
    /// Interval length and phase missing on an interval series, given on
    /// another, or out of range (§2.1: d > 0, 0 ≤ phase < d).
    #[error(
        "interval length and phase are for interval series only, with d > 0 and 0 <= phase < d"
    )]
    BadIntervalSpec,
    /// Record on change on an interval series (§2.1).
    #[error("record on change does not apply to an interval series")]
    RecordOnChangeInterval,
    /// Counter rules on a series that is not a counter, or out of range
    /// (§5.5: `wrap_at` positive, `reset_ceiling` between 0 and 1).
    #[error("counter rules apply to counters only, with wrap_at > 0 and 0 <= reset_ceiling <= 1")]
    BadCounterRules,
}

/// The registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Registry {
    series: BTreeMap<SeriesId, SeriesDef>,
}

impl Registry {
    /// Applies an event (already validated).
    pub fn apply(&mut self, e: &RegistryEvent) {
        match e {
            RegistryEvent::Define {
                id,
                name,
                kind,
                value_type,
                unit,
                staleness,
                options,
            } => {
                self.series.insert(
                    *id,
                    SeriesDef {
                        id: *id,
                        names: vec![name.clone()],
                        kind: *kind,
                        value_type: *value_type,
                        unit: unit.clone(),
                        staleness: *staleness,
                        options: *options,
                        retention: RetentionClass::LongTerm,
                        retired: false,
                        retired_at: None,
                    },
                );
            }
            RegistryEvent::Alias { id, name } => {
                if let Some(d) = self.series.get_mut(id) {
                    d.names.push(name.clone());
                }
            }
            RegistryEvent::Retention { id, class } => {
                if let Some(d) = self.series.get_mut(id) {
                    d.retention = *class;
                }
            }
            RegistryEvent::Retire { id, at } => {
                if let Some(d) = self.series.get_mut(id) {
                    d.retired = true;
                    d.retired_at = Some(*at);
                }
            }
        }
    }

    /// The next unused identifier. Identifiers are never reused because
    /// retired series stay in the registry.
    pub fn next_id(&self) -> SeriesId {
        SeriesId(self.series.keys().next_back().map_or(0, |s| s.0 + 1))
    }

    /// Looks up a series by id.
    pub fn get(&self, id: SeriesId) -> Option<&SeriesDef> {
        self.series.get(&id)
    }

    /// Resolves a name or alias to the live series that carries it.
    pub fn resolve(&self, name: &str) -> Option<&SeriesDef> {
        self.series
            .values()
            .find(|d| !d.retired && d.names.iter().any(|n| n == name))
    }

    /// Live (not retired) series, the ones shown to users.
    pub fn live(&self) -> impl Iterator<Item = &SeriesDef> {
        self.series.values().filter(|d| !d.retired)
    }

    /// All series, retired included.
    pub fn all(&self) -> impl Iterator<Item = &SeriesDef> {
        self.series.values()
    }

    /// Validates a definition and returns the event to record, or the id of
    /// an identical live series (defining is idempotent).
    #[allow(clippy::too_many_arguments)]
    pub fn plan_define(
        &self,
        name: &str,
        kind: Kind,
        value_type: ValueType,
        unit: &str,
        staleness: Option<Nanos>,
        options: SeriesOptions,
        ceiling: usize,
    ) -> Result<Result<RegistryEvent, SeriesId>, RegistryError> {
        if !kind.accepts(value_type) {
            return Err(RegistryError::BadType(kind, value_type));
        }
        if staleness.is_some_and(|s| s <= 0) {
            return Err(RegistryError::BadStaleness);
        }
        if options.expected_interval.is_some_and(|e| e <= 0)
            || options.lateness_bound.is_some_and(|l| l <= 0)
        {
            return Err(RegistryError::BadInterval);
        }
        let rules = options.counter;
        if (kind != Kind::Counter && rules != CounterRules::default())
            || rules.wrap_at.is_some_and(|w| !(w > 0.0 && w.is_finite()))
            || !(0.0..=1.0).contains(&rules.reset_ceiling)
        {
            return Err(RegistryError::BadCounterRules);
        }
        match (kind, options.interval) {
            (Kind::Interval, Some(iv)) if iv.length > 0 && (0..iv.length).contains(&iv.phase) => {}
            (Kind::Interval, _) | (_, Some(_)) => return Err(RegistryError::BadIntervalSpec),
            _ => {}
        }
        if kind == Kind::Interval && options.record_on_change {
            return Err(RegistryError::RecordOnChangeInterval);
        }
        // S = 3 × expected interval unless set (§2.1); an interval series'
        // expected interval defaults to its length.
        let expected = options
            .expected_interval
            .or(options.interval.map(|iv| iv.length));
        let staleness = staleness.or(expected.map(|e| 3 * e));
        if options.timing == Timing::Forecast && options.versioned != Some(ChangedAtKind::Time) {
            return Err(RegistryError::ForecastNotVersioned);
        }
        if let Some(d) = self.resolve(name) {
            if d.kind == kind
                && d.value_type == value_type
                && d.unit == unit
                && d.options == options
            {
                return Ok(Err(d.id));
            }
            return Err(RegistryError::NameInUse(name.to_string()));
        }
        if self.series.len() >= ceiling {
            return Err(RegistryError::Ceiling(ceiling));
        }
        Ok(Ok(RegistryEvent::Define {
            id: self.next_id(),
            name: name.to_string(),
            kind,
            value_type,
            unit: unit.to_string(),
            staleness,
            options,
        }))
    }
}
