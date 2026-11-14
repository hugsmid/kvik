//! Conversions between the interface's types and the reference model's.

use std::time::Duration;

use jiff::Timestamp;
use kvik_model as km;

use crate::consume::{ConditionState, TriggerCause};
use crate::error::{BusyReason, Error, Refusal};
use crate::read::{Aggregate, Aggregated};
use crate::series::{
    ChangedAtKind, IntervalLength, Kind, RetentionClass, SeriesSpec, StateValues, Timing, ValueType,
};
use crate::types::{Fact, FactFlags, Position, Quality, SeriesId, Value};

/// UTC nanoseconds of a timestamp.
pub(crate) fn nanos(t: Timestamp) -> Result<i64, Error> {
    i64::try_from(t.as_nanosecond()).map_err(|_| Error::TimeOutOfRange)
}

/// The timestamp of UTC nanoseconds; every `i64` is in range.
pub(crate) fn ts(n: i64) -> Timestamp {
    Timestamp::from_nanosecond(i128::from(n)).unwrap_or(Timestamp::MAX)
}

/// Nanoseconds of a duration.
pub(crate) fn dur_nanos(d: Duration) -> Result<i64, Error> {
    i64::try_from(d.as_nanos()).map_err(|_| Error::TimeOutOfRange)
}

/// The duration of a non-negative count of nanoseconds (negative counts as
/// zero).
pub(crate) fn dur(n: i128) -> Duration {
    Duration::from_nanos(u64::try_from(n.max(0)).unwrap_or(u64::MAX))
}

pub(crate) fn pos(p: km::types::Position) -> Position {
    Position::new(p.generation, p.sequence)
}

pub(crate) fn mpos(p: Position) -> km::types::Position {
    km::types::Position::new(p.generation, p.sequence)
}

pub(crate) fn sid(s: km::types::SeriesId) -> SeriesId {
    SeriesId::from_raw(s.0)
}

pub(crate) fn msid(s: SeriesId) -> km::types::SeriesId {
    km::types::SeriesId(s.get())
}

pub(crate) fn quality(q: km::types::Quality) -> Quality {
    match q {
        km::types::Quality::Good => Quality::Good,
        km::types::Quality::Estimated => Quality::Estimated,
        km::types::Quality::Suspect => Quality::Suspect,
        km::types::Quality::Bad => Quality::Bad,
    }
}

pub(crate) fn mquality(q: Quality) -> km::types::Quality {
    match q {
        Quality::Good => km::types::Quality::Good,
        Quality::Estimated => km::types::Quality::Estimated,
        Quality::Suspect => km::types::Quality::Suspect,
        Quality::Bad => km::types::Quality::Bad,
    }
}

/// A model value as the interface shows it, given the series' definition.
pub(crate) fn value_out(v: &km::types::Value, spec: Option<&SeriesSpec>) -> Option<Value> {
    match v {
        km::types::Value::Num(x) => Some(Value::F64(*x)),
        km::types::Value::Bool(b) => Some(Value::Bool(*b)),
        km::types::Value::Str(s) => match spec.map(|s| &s.kind) {
            Some(Kind::State {
                values: StateValues::Named(names),
            }) => names
                .iter()
                .position(|n| n == s)
                .and_then(|i| u16::try_from(i).ok())
                .map(Value::State),
            _ => Some(Value::Text(s.clone())),
        },
        km::types::Value::Absent => None,
    }
}

pub(crate) fn fact_out(f: &km::types::Fact, spec: Option<&SeriesSpec>) -> Fact {
    let fl = f.flags;
    Fact {
        series: sid(f.series),
        t: ts(f.t),
        position: pos(f.position),
        received_at: ts(f.received_at),
        value: value_out(&f.value, spec),
        quality: quality(f.quality),
        supplied_quality: quality(f.supplied_quality),
        changed_at: f.changed_at,
        flags: FactFlags {
            future: fl.future,
            late: fl.late,
            revise: fl.revise,
            host_time: fl.host_time,
            unsynced_clock: fl.unsynced_clock,
            no_value: fl.no_value,
        },
    }
}

/// A model refusal; `None` for the two that concern the whole store.
pub(crate) fn refusal(r: &km::engine::Refusal) -> Option<Refusal> {
    use km::engine::Refusal as R;
    Some(match r {
        R::Down | R::ReadOnly => return None,
        R::UnknownSeries => Refusal::UnknownSeries,
        R::Retired => Refusal::Retired,
        R::WrongType => Refusal::WrongType,
        R::TextTooLong => Refusal::TextTooLong,
        R::ChangedAtMissing => Refusal::ChangedAtMissing,
        R::ChangedAtUnexpected => Refusal::ChangedAtUnexpected,
        R::ReviseOnVersioned => Refusal::ReviseOnVersioned,
        R::OffPhase => Refusal::OffPhase,
    })
}

pub(crate) fn wait(w: &km::engine::Wait) -> BusyReason {
    match w {
        km::engine::Wait::DiskFull => BusyReason::DiskFull,
        km::engine::Wait::QueueFull => BusyReason::QueueFull,
        km::engine::Wait::UnsyncedClock => BusyReason::UnsyncedClock,
    }
}

pub(crate) fn mwait(b: BusyReason) -> km::engine::Wait {
    match b {
        BusyReason::DiskFull => km::engine::Wait::DiskFull,
        BusyReason::QueueFull => km::engine::Wait::QueueFull,
        BusyReason::UnsyncedClock => km::engine::Wait::UnsyncedClock,
    }
}

/// A model error as an interface error.
pub(crate) fn error(e: km::engine::Error) -> Error {
    use km::engine::Error as E;
    use km::registry::RegistryError as R;
    match e {
        E::Crashed => Error::Closed,
        E::ReadOnly => Error::ReadOnly {
            cause: "a write or sync failed earlier".into(),
        },
        E::SyncFailed => Error::Fault("sync failed; the store is read-only".into()),
        E::Registry(R::Ceiling(n)) => Error::SeriesCeiling(n),
        E::Registry(R::NameInUse(n)) => Error::NameInUse(n),
        E::Registry(R::Unknown(s)) => Error::UnknownSeries(sid(s).to_string()),
        E::Registry(R::Retired(s)) => Error::Retired(sid(s)),
        E::Registry(other) => Error::InvalidDefinition(other.to_string()),
        E::Tree(km::tree::TreeError::UnknownZone(z)) => Error::UnknownZone(z),
        E::BadConfirm => Error::BadConfirm,
        E::Restoring => Error::Restoring,
        E::BadRoute(msg) if msg.contains("not modelled") => Error::NotSupportedYet { what: msg },
        E::BadRoute(msg) => Error::Route(msg.to_string()),
        E::SiteZoneMismatch { store, configured } => Error::SiteZoneMismatch { store, configured },
        other => Error::Internal(other.to_string()),
    }
}

fn number_type(v: ValueType) -> Result<km::types::ValueType, Error> {
    match v {
        ValueType::F64 => Ok(km::types::ValueType::Number),
        ValueType::Decimal { .. } => Err(Error::NotSupportedYet {
            what: "decimal value types (exact i64 with a scale)",
        }),
    }
}

/// What the model needs to define a series.
pub(crate) struct ModelDefinition {
    pub(crate) kind: km::types::Kind,
    pub(crate) value_type: km::types::ValueType,
    pub(crate) staleness: Option<i64>,
    pub(crate) options: km::registry::SeriesOptions,
    pub(crate) retention: km::registry::RetentionClass,
}

pub(crate) fn definition(spec: &SeriesSpec) -> Result<ModelDefinition, Error> {
    use km::types::{Kind as K, ValueType as V};
    let mut interval = None;
    let (kind, value_type) = match &spec.kind {
        Kind::Gauge { value } => (K::Gauge, number_type(*value)?),
        Kind::Counter { value } => (K::Counter, number_type(*value)?),
        Kind::State {
            values: StateValues::Bool,
        } => (K::State, V::Boolean),
        Kind::State {
            values: StateValues::Named(names),
        } => {
            if names.is_empty() || names.len() > usize::from(u16::MAX) + 1 {
                return Err(Error::InvalidDefinition(
                    "a named state series declares between 1 and 65,536 names".into(),
                ));
            }
            let mut sorted: Vec<&String> = names.iter().collect();
            sorted.sort();
            sorted.dedup();
            if sorted.len() != names.len() {
                return Err(Error::InvalidDefinition(
                    "state names must be distinct".into(),
                ));
            }
            (K::State, V::Name)
        }
        Kind::Text => (K::Text, V::Text),
        Kind::Interval { value, length } => {
            let vt = number_type(*value)?;
            match length {
                IntervalLength::Fixed { d, phase } => {
                    interval = Some(km::types::IntervalSpec {
                        length: dur_nanos(*d)?,
                        phase: dur_nanos(*phase)?,
                    });
                }
                IntervalLength::Calendar { .. } => {
                    return Err(Error::NotSupportedYet {
                        what: "calendar-length interval series (a local day or month)",
                    });
                }
            }
            (K::Interval, vt)
        }
    };
    let opt = |d: Option<Duration>| d.map(dur_nanos).transpose();
    let counter = spec
        .counter
        .map_or(km::summary::CounterRules::default(), |c| {
            km::summary::CounterRules {
                wrap_at: c.wrap_at,
                reset_ceiling: c.reset_ceiling,
            }
        });
    let options = km::registry::SeriesOptions {
        versioned: spec.versioned.map(|k| match k {
            ChangedAtKind::Time => km::registry::ChangedAtKind::Time,
            ChangedAtKind::Version => km::registry::ChangedAtKind::Version,
        }),
        timing: match spec.timing {
            Timing::Observed => km::registry::Timing::Observed,
            Timing::Scheduled => km::registry::Timing::Scheduled,
            Timing::Forecast => km::registry::Timing::Forecast,
        },
        expected_interval: opt(spec.expected_interval)?,
        record_on_change: spec.record_on_change,
        counter,
        lateness_bound: opt(spec.lateness_bound)?,
        interval,
    };
    Ok(ModelDefinition {
        kind,
        value_type,
        staleness: opt(spec.staleness)?,
        options,
        retention: retention(spec.retention)?,
    })
}

pub(crate) fn retention(r: RetentionClass) -> Result<km::registry::RetentionClass, Error> {
    Ok(match r {
        RetentionClass::LongTerm => km::registry::RetentionClass::LongTerm,
        RetentionClass::Diagnostic { period } => km::registry::RetentionClass::Diagnostic {
            period: dur_nanos(period)?,
        },
    })
}

/// An aggregate for the model, given the series' definition (named states
/// are stored by name).
pub(crate) fn aggregate(
    a: &Aggregate,
    spec: Option<&SeriesSpec>,
) -> Result<km::summary::Aggregate, Error> {
    use km::summary::Aggregate as A;
    Ok(match a {
        Aggregate::Avg => A::Avg,
        Aggregate::Sum => A::Sum,
        Aggregate::Integral => A::Integral,
        Aggregate::Min => A::Min,
        Aggregate::Max => A::Max,
        Aggregate::Count => A::Count,
        Aggregate::Increase => A::Increase,
        Aggregate::Rate => A::Rate,
        Aggregate::Last => A::Last,
        Aggregate::TimeInState(v) => {
            let key = match (v, spec.map(|s| &s.kind)) {
                (
                    Value::Bool(b),
                    Some(Kind::State {
                        values: StateValues::Bool,
                    }),
                ) => km::types::StateKey::Bool(*b),
                (
                    Value::State(i),
                    Some(Kind::State {
                        values: StateValues::Named(names),
                    }),
                ) => km::types::StateKey::Name(
                    names
                        .get(usize::from(*i))
                        .ok_or(Error::Refused(Refusal::UnknownState))?
                        .clone(),
                ),
                _ => return Err(Error::AggregateNotApplicable),
            };
            A::TimeInState(key)
        }
    })
}

pub(crate) fn aggregated(v: &km::summary::AggValue, spec: Option<&SeriesSpec>) -> Aggregated {
    match v {
        km::summary::AggValue::Exact(_) => Aggregated::Number(v.to_f64().unwrap_or(f64::NAN)),
        km::summary::AggValue::Value(x) => Aggregated::Value(value_out(x, spec)),
    }
}

pub(crate) fn condition_state(s: &km::condition::ConditionState) -> ConditionState {
    match s {
        km::condition::ConditionState::Active(true) => ConditionState::True,
        km::condition::ConditionState::Active(false) => ConditionState::False,
        km::condition::ConditionState::Value(v) => ConditionState::Value(*v),
    }
}

pub(crate) fn trigger_cause(c: km::engine::TriggerCause) -> TriggerCause {
    match c {
        km::engine::TriggerCause::Fact => TriggerCause::Fact,
        km::engine::TriggerCause::Timer => TriggerCause::Timer,
        km::engine::TriggerCause::Reattach => TriggerCause::Reattach,
    }
}
