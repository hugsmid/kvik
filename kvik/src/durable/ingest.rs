//! Deciding a call's outcomes before it is committed (spec §2.2 to §2.5): refusals, stamping and flags, values
//! recorded without one, the duplicate rule and outranked facts. The reference model makes the same decisions
//! inside its commit, which is not callable on its own; this follows it rule by rule, except where the
//! specification has moved on (the first clause of `late` uses the lateness reference, spec §2.2), and the
//! differential test in `tests/durable.rs` holds the two backends to the same outcomes.
//!
//! Decisions are made in the order calls are queued in the log, against every fact already queued, so a call
//! sees the calls before it whether or not their sync has returned yet.

use std::collections::HashMap;

use kvik_log::content::Item;
use kvik_log::entries::{
    IntervalLength as LogInterval, Kind as LogKind, Timing as LogTiming, Versioning,
};
use kvik_log::facts::{
    FactFlags as LogFlags, FactRow, MAX_TEXT_LEN, Quality as LogQuality, Value as LogValue,
    ValueType as LogType,
};

use super::index::{Index, quality_in};
use super::registry::Registry;
use crate::error::Refusal;
use crate::types::{Intent, Reading, TimeSource, Value};

/// A recorded fact as the duplicate rule compares it: value, adapter-supplied quality and `changed_at`.
#[derive(Clone, Debug, PartialEq)]
struct Known {
    value: LogValue,
    supplied: LogQuality,
    changed_at: Option<u64>,
    /// The call that queued it in this process; 0 for a fact of the kept log read at the start.
    call: u64,
}

impl Known {
    fn of(r: &FactRow, call: u64) -> Self {
        Known {
            value: r.value.clone(),
            supplied: r.quality,
            changed_at: r.changed_at,
            call,
        }
    }

    /// Identity (spec §2.4): values compare bitwise, a value recorded without one as "no value".
    fn identical(&self, value: &LogValue, supplied: LogQuality) -> bool {
        self.supplied == supplied
            && match (&self.value, value) {
                (LogValue::Absent { .. }, LogValue::Absent { .. }) => true,
                (a, b) => a == b,
            }
    }
}

/// The clock at decision time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    /// Wall clock, nanoseconds.
    pub(crate) wall: i64,
    /// Whether the system reports clock synchronisation (spec §2.5).
    pub(crate) synced: bool,
    /// `limits.future_tolerance`, nanoseconds.
    pub(crate) future_tolerance: i64,
}

/// What happens to one reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Refused and counted.
    Refused(Refusal),
    /// Dropped as a duplicate.
    Duplicate,
    /// Recorded as the call's `item`-th fact.
    Record {
        /// Its index among the call's items.
        item: usize,
        /// A fact of higher rank for its key is already recorded (spec §2.3).
        outranked: bool,
        /// A supplied `Absent` (spec §2.5 `absent_supplied`).
        absent: bool,
        /// Flagged `late` (spec §2.2).
        late: bool,
        /// Flagged `future` (spec §2.2).
        future: bool,
    },
}

/// A call's decisions and the facts it commits.
#[derive(Clone, Debug, Default)]
pub(crate) struct Plan {
    pub(crate) decisions: Vec<Decision>,
    pub(crate) items: Vec<Item>,
    /// The latest earlier call holding a fact a duplicate matched, if any: the duplicate is final only once that
    /// call is acknowledged (Q-123 (b)).
    pub(crate) depends: Option<u64>,
    /// What [`Decider::apply`] takes into the state: the plan's facts as `history`, `versions` and `reference`
    /// hold them, so that `items` can be moved into the log.
    history: HashMap<(u32, i64), Vec<Known>>,
    versions: HashMap<(u32, i64), u64>,
    reference: HashMap<u32, i64>,
}

/// The state decisions are made against: the registry and the history of every key, including calls queued but
/// not yet acknowledged.
#[derive(Clone, Debug, Default)]
pub(crate) struct Decider {
    pub(crate) registry: Registry,
    history: HashMap<(u32, i64), Vec<Known>>,
    versions: HashMap<(u32, i64), u64>,
    reference: HashMap<u32, i64>,
}

fn reach(observed: bool, interval: Option<&LogInterval>, t: i64) -> i64 {
    match (observed, interval) {
        (true, Some(LogInterval::Fixed { length, .. })) => t.saturating_add(*length),
        _ => t,
    }
}

fn on_phase(interval: Option<&LogInterval>, t: i64) -> bool {
    match interval {
        Some(LogInterval::Fixed { length, phase }) => {
            (i128::from(t) - i128::from(*phase)).rem_euclid(i128::from(*length)) == 0
        }
        _ => true,
    }
}

impl Decider {
    /// The state of the kept log.
    pub(crate) fn from_index(index: &Index) -> Self {
        let mut d = Decider {
            registry: index.registry().clone(),
            ..Decider::default()
        };
        for id in index.fact_series() {
            for f in index.series_facts(id) {
                d.note(&f.row, 0);
            }
        }
        d
    }

    fn note(&mut self, r: &FactRow, call: u64) {
        let key = (r.series_id, r.t);
        self.history
            .entry(key)
            .or_default()
            .push(Known::of(r, call));
        if let Some(c) = r.changed_at {
            let v = self.versions.entry(key).or_insert(c);
            *v = (*v).max(c);
        }
        if !r.flags.future && !r.flags.unsynced_clock {
            let v = self.reference.entry(r.series_id).or_insert(r.t);
            *v = (*v).max(r.t);
        }
    }

    /// Takes a planned call's facts into the state, once it is queued; the same as noting each of its facts in
    /// order.
    pub(crate) fn apply(&mut self, plan: &mut Plan) {
        for (key, known) in std::mem::take(&mut plan.history) {
            self.history.entry(key).or_default().extend(known);
        }
        for (key, c) in std::mem::take(&mut plan.versions) {
            let v = self.versions.entry(key).or_insert(c);
            *v = (*v).max(c);
        }
        for (id, t) in std::mem::take(&mut plan.reference) {
            let v = self.reference.entry(id).or_insert(t);
            *v = (*v).max(t);
        }
    }

    /// The refusal of a reading, if any, in the order the model backend checks: unknown series, value type,
    /// retirement, phase, `changed_at`, text length.
    fn refusal(&self, r: &Reading, revise: bool) -> Result<LogValue, Refusal> {
        let s = self
            .registry
            .get(r.series.0)
            .ok_or(Refusal::UnknownSeries)?;
        let d = &s.def;
        let value = match (&r.value, d.value_type) {
            (Value::Absent, vt) => LogValue::Absent {
                value_type: vt,
                scale: 0,
            },
            (Value::F64(x), LogType::F64) => LogValue::F64(x.to_bits()),
            (Value::Bool(b), LogType::Bool) => LogValue::Bool(*b),
            (Value::State(i), LogType::State) => {
                if usize::from(*i) >= d.state_names.len() {
                    return Err(Refusal::StateOutOfRange);
                }
                LogValue::State(*i)
            }
            (Value::Text(t), LogType::Text) => LogValue::Text(t.clone()),
            _ => return Err(Refusal::WrongType),
        };
        if s.retired_at.is_some_and(|at| r.t.0 >= at) {
            return Err(Refusal::Retired);
        }
        if !on_phase(d.interval.as_ref(), r.t.0) {
            return Err(Refusal::OffPhase);
        }
        match (d.versioning != Versioning::None, r.changed_at.is_some()) {
            (true, false) => return Err(Refusal::ChangedAtMissing),
            (false, true) => return Err(Refusal::ChangedAtUnexpected),
            (true, true) if revise => return Err(Refusal::ReviseOnVersioned),
            _ => {}
        }
        if matches!(&value, LogValue::Text(t) if t.len() > MAX_TEXT_LEN) {
            return Err(Refusal::TextTooLong);
        }
        Ok(value)
    }

    /// Decides a call. Nothing changes until [`Decider::apply`].
    pub(crate) fn plan(
        &self,
        readings: &[Reading],
        intent: Intent,
        clock: Clock,
        call: u64,
    ) -> Plan {
        let revise = intent == Intent::Revise;
        let mut plan = Plan::default();
        let mut batch_history: HashMap<(u32, i64), Vec<Known>> = HashMap::new();
        let mut batch_versions: HashMap<(u32, i64), u64> = HashMap::new();
        let mut batch_reference: HashMap<u32, i64> = HashMap::new();
        for r in readings {
            let value = match self.refusal(r, revise) {
                Ok(v) => v,
                Err(x) => {
                    plan.decisions.push(Decision::Refused(x));
                    continue;
                }
            };
            let s = self.registry.get(r.series.0).expect("checked by refusal");
            let d = &s.def;
            let supplied = quality_in(r.quality);
            // NaN and ±∞ (spec §12 D1), a negative counter value (§5.5) and a supplied `Absent` (§2.5) are
            // recorded without a value; the duplicate rule compares them as "no value" (§2.4).
            let absent = matches!(r.value, Value::Absent);
            let value = match value {
                LogValue::F64(bits)
                    if !f64::from_bits(bits).is_finite()
                        || (d.kind == LogKind::Counter && f64::from_bits(bits) < 0.0) =>
                {
                    LogValue::Absent {
                        value_type: d.value_type,
                        scale: 0,
                    }
                }
                v => v,
            };
            let observed = d.timing == LogTiming::Observed;
            let reach = reach(observed, d.interval.as_ref(), r.t.0);
            let future = clock.synced
                && observed
                && reach > clock.wall.saturating_add(clock.future_tolerance);
            let key = (r.series.0, r.t.0);
            let recorded = self
                .history
                .get(&key)
                .into_iter()
                .flatten()
                .chain(batch_history.get(&key).into_iter().flatten());
            let top = self
                .versions
                .get(&key)
                .copied()
                .max(batch_versions.get(&key).copied());
            // The facts this reading duplicates (§2.4); a `changed_at` above every one of its key's is never a
            // duplicate.
            let matched: Vec<&Known> = match r.changed_at {
                None => recorded
                    .into_iter()
                    .filter(|k| k.identical(&value, supplied))
                    .collect(),
                Some(c) if top.is_some_and(|top| c <= top) => recorded
                    .into_iter()
                    .filter(|k| k.changed_at == Some(c) && k.identical(&value, supplied))
                    .collect(),
                Some(_) => Vec::new(),
            };
            if !matched.is_empty() && !revise {
                // The earliest call holding a match suffices: once it is acknowledged the reading is durable.
                let earliest = matched.iter().map(|k| k.call).min().unwrap_or(0);
                if earliest != 0 && earliest != call {
                    plan.depends = plan.depends.max(Some(earliest));
                }
                plan.decisions.push(Decision::Duplicate);
                continue;
            }
            // `late` (§2.2): on an observed series, a fact not flagged `future` earlier than the lateness
            // reference by more than 2 × S; on any series, older than the lateness bound. Neither before the clock
            // is synchronised (§2.5).
            let reference = self
                .reference
                .get(&r.series.0)
                .copied()
                .max(batch_reference.get(&r.series.0).copied());
            let out_of_order = match (reference, d.staleness) {
                (Some(rf), Some(st)) => {
                    observed && !future && r.t.0 < rf.saturating_sub(st.saturating_mul(2))
                }
                _ => false,
            };
            let late = clock.synced
                && (out_of_order || clock.wall.saturating_sub(reach) > d.lateness_bound);
            let row = FactRow {
                series_id: r.series.0,
                t: r.t.0,
                quality: supplied,
                flags: LogFlags {
                    future,
                    late,
                    revise,
                    host_time: r.source == TimeSource::Host,
                    unsynced_clock: !clock.synced,
                    rounded: false,
                },
                value,
                changed_at: r.changed_at,
            };
            let outranked = r.changed_at.is_some_and(|c| top.is_some_and(|top| c < top));
            batch_history
                .entry(key)
                .or_default()
                .push(Known::of(&row, call));
            if let Some(c) = r.changed_at {
                let v = batch_versions.entry(key).or_insert(c);
                *v = (*v).max(c);
            }
            if !future && clock.synced {
                let v = batch_reference.entry(r.series.0).or_insert(r.t.0);
                *v = (*v).max(r.t.0);
            }
            plan.decisions.push(Decision::Record {
                item: plan.items.len(),
                outranked,
                absent,
                late,
                future,
            });
            plan.items.push(Item::Fact(row));
        }
        plan.history = batch_history;
        plan.versions = batch_versions;
        plan.reference = batch_reference;
        plan
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kvik_log::entries::Entry;

    use super::*;
    use crate::durable::registry::definition_of;
    use crate::series::{ChangedAtKind, Kind, SeriesSpec, ValueType};
    use crate::time::Timestamp;
    use crate::types::{Quality, SeriesId};

    const T0: i64 = 1_767_225_600_000_000_000;
    const S: i64 = 1_000_000_000;

    fn decider() -> Decider {
        let mut d = Decider::default();
        let gauge = SeriesSpec::new(
            "a/b/c",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        let versioned = gauge.clone().versioned(ChangedAtKind::Version);
        for (id, spec) in [(1, gauge), (2, versioned)] {
            let def = definition_of(&spec, id, Duration::from_secs(300)).unwrap();
            d.registry.apply(&Entry::Definition(Box::new(def)));
        }
        d
    }

    fn reading(series: u32, t: i64, v: f64, changed_at: Option<u64>) -> Reading {
        Reading {
            changed_at,
            quality: if v < 0.0 { Quality::Bad } else { Quality::Good },
            ..Reading::new(SeriesId(series), Timestamp(t), Value::F64(v))
        }
    }

    #[test]
    fn apply_takes_in_what_noting_each_planned_fact_in_order_would() {
        let clock = Clock {
            wall: T0 + 3_600 * S,
            synced: true,
            future_tolerance: 60 * S,
        };
        let mut d = decider();
        let mut first = d.plan(&[reading(1, T0, 1.0, None)], Intent::Normal, clock, 1);
        d.apply(&mut first);
        let readings = [
            reading(1, T0, 2.0, None),
            reading(1, T0 + S, 3.0, None),
            reading(1, T0, 4.0, None),
            reading(1, T0 - 7_200 * S, 5.0, None),
            reading(1, T0 + 7_200 * S, 6.0, None),
            reading(2, T0, 1.0, Some(3)),
            reading(2, T0, 2.0, Some(9)),
            reading(2, T0, 3.0, Some(5)),
            reading(1, T0 + 2 * S, -1.0, None),
        ];
        let mut plan = d.plan(&readings, Intent::Normal, clock, 2);
        let mut noted = d.clone();
        for item in &plan.items {
            if let Item::Fact(r) = item {
                noted.note(r, 2);
            }
        }
        assert_eq!(plan.items.len(), readings.len());
        d.apply(&mut plan);
        assert_eq!(d.history, noted.history);
        assert_eq!(d.versions, noted.versions);
        assert_eq!(d.reference, noted.reference);
        assert_eq!(d.history[&(1, T0)].len(), 3);
        assert_eq!(d.versions[&(2, T0)], 9);
    }
}
