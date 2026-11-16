//! Deciding a call's outcomes before it is committed (spec §2.2 to §2.5): refusals, stamping and flags, values
//! recorded without one, the duplicate rule and outranked facts. The reference model makes the same decisions
//! inside its commit, which is not callable on its own; this follows it rule by rule, except where the
//! specification has moved on (the first clause of `late` uses the lateness reference, spec §2.2), and the
//! differential test in `tests/durable.rs` holds the two backends to the same outcomes.
//!
//! Decisions are made in the order calls are queued in the log, against every fact already queued, so a call
//! sees the calls before it whether or not their sync has returned yet. The facts are found as [`History`] says: in
//! memory, or for a key outside the recent window and the revised-key index, by the slow path through
//! [`LocalFacts`], once per call for all such keys.

use std::collections::{HashMap, HashSet};

use kvik_log::content::Item;
use kvik_log::entries::{
    IntervalLength as LogInterval, Kind as LogKind, Timing as LogTiming, Versioning,
};
use kvik_log::facts::{
    FactFlags as LogFlags, FactRow, MAX_TEXT_LEN, Value as LogValue, ValueType as LogType,
};

use super::history::{Facts, History, Key, Known, span_of};
use super::index::{Index, quality_in};
use super::keys::LocalFacts;
use super::registry::Registry;
use crate::error::Refusal;
use crate::types::{Intent, Reading, TimeSource, Value};

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
        /// Recorded although the duplicate check could not run (spec §2.4 `duplicates_unchecked`, Q-750).
        unchecked: bool,
    },
}

/// A recorded fact as [`Decider::apply`] takes it into the state.
#[derive(Clone, Debug)]
struct Note {
    key: Key,
    /// It raises the lateness reference: flagged neither `future` nor `unsynced_clock`.
    reference: bool,
    known: Known,
}

/// A call's decisions and the facts it commits.
#[derive(Clone, Debug, Default)]
pub(crate) struct Plan {
    pub(crate) decisions: Vec<Decision>,
    pub(crate) items: Vec<Item>,
    /// The latest earlier call holding a fact a duplicate matched, if any: the duplicate is final only once that
    /// call is acknowledged (Q-123 (b)).
    pub(crate) depends: Option<u64>,
    /// Readings decided by the slow path (spec §2.4; roadmap 11.0).
    pub(crate) slow: u64,
    /// The plan's facts as the state takes them, so that `items` can be moved into the log.
    notes: Vec<Note>,
    /// The complete history the slow path read of each key it read whole.
    whole: HashMap<Key, Vec<Known>>,
}

/// The state decisions are made against: the registry and [`History`], including calls queued but not yet
/// acknowledged.
#[derive(Clone, Debug, Default)]
pub(crate) struct Decider {
    pub(crate) registry: Registry,
    pub(crate) history: History,
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
    /// The state of the kept log in a full index (a start whose read of the log was voided part-way).
    pub(crate) fn from_index(index: &Index) -> Self {
        let mut d = Decider {
            registry: index.registry().clone(),
            ..Decider::default()
        };
        let mut facts: Vec<_> = index
            .fact_series()
            .flat_map(|id| index.series_facts(id))
            .collect();
        facts.sort_by_key(|f| f.position);
        for f in facts {
            d.note_kept(&f.row, span_of(&d.registry, f.row.series_id));
        }
        d
    }

    /// Takes in a fact of the kept log, read in log order at the start, of a series whose window span is `span`.
    pub(crate) fn note_kept(&mut self, r: &FactRow, span: i64) {
        let reference = !r.flags.future && !r.flags.unsynced_clock;
        self.history
            .note((r.series_id, r.t), reference, Known::of(r, 0), span, None);
    }

    /// Notes that every call below `call` is acknowledged.
    pub(crate) fn readable_below(&mut self, call: u64) {
        self.history.readable_below(call);
    }

    /// Takes a planned call's facts into the state, once it is queued; the same as noting each of its facts in
    /// order.
    pub(crate) fn apply(&mut self, plan: &mut Plan) {
        let mut whole = std::mem::take(&mut plan.whole);
        for n in std::mem::take(&mut plan.notes) {
            let span = span_of(&self.registry, n.key.0);
            let w = whole.remove(&n.key);
            self.history.note(n.key, n.reference, n.known, span, w);
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

    /// Decides a call, reading the keys the slow path needs through `local`. Nothing changes until
    /// [`Decider::apply`].
    pub(crate) fn plan(
        &self,
        readings: &[Reading],
        intent: Intent,
        clock: Clock,
        call: u64,
        local: &mut dyn LocalFacts,
    ) -> Plan {
        let revise = intent == Intent::Revise;
        let mut plan = Plan {
            decisions: Vec::with_capacity(readings.len()),
            items: Vec::with_capacity(readings.len()),
            ..Plan::default()
        };
        // A reading with revise intent bypasses the check (§2.4) and is never outranked (it is refused on a
        // versioned series), so it needs no read.
        let mut slow: Vec<Key> = Vec::new();
        if !revise {
            let mut seen = HashSet::new();
            for r in readings {
                let key = (r.series.0, r.t.0);
                if matches!(self.history.facts(key), Facts::Slow(_))
                    && seen.insert(key)
                    && self.refusal(r, revise).is_ok()
                {
                    slow.push(key);
                }
            }
        }
        let found = if slow.is_empty() {
            super::keys::Found::default()
        } else {
            local.at(&slow)
        };
        let read: HashMap<Key, Vec<Known>> = slow
            .iter()
            .map(|k| {
                let facts = found.facts.get(k).map_or_else(Vec::new, |fs| {
                    fs.iter().map(|f| Known::of(&f.row, 0)).collect()
                });
                (*k, facts)
            })
            .collect();
        let mut batch: HashMap<Key, Vec<Known>> = HashMap::with_capacity(readings.len());
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
            let (state, unreadable): (Vec<&Known>, bool) = match self.history.facts(key) {
                Facts::Known(v) => (v, false),
                Facts::Slow(overlay) if !revise => {
                    plan.slow += 1;
                    let mut v: Vec<&Known> = read.get(&key).into_iter().flatten().collect();
                    v.extend(overlay);
                    (v, found.unreadable.contains(&key))
                }
                Facts::Slow(overlay) => (overlay, false),
            };
            let recorded = || {
                state
                    .iter()
                    .copied()
                    .chain(batch.get(&key).into_iter().flatten())
            };
            let top = r
                .changed_at
                .and_then(|_| recorded().filter_map(|k| k.changed_at).max());
            // The facts this reading duplicates (§2.4); a `changed_at` above every one of its key's is never a
            // duplicate.
            let matched: Vec<&Known> = match r.changed_at {
                None => recorded()
                    .filter(|k| k.identical(&value, supplied))
                    .collect(),
                Some(c) if top.is_some_and(|top| c <= top) => recorded()
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
                .history
                .reference(r.series.0)
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
            let known = Known::of(&row, call);
            batch.entry(key).or_default().push(known.clone());
            let raises = !future && clock.synced;
            if raises {
                let v = batch_reference.entry(r.series.0).or_insert(r.t.0);
                *v = (*v).max(r.t.0);
            }
            plan.notes.push(Note {
                key,
                reference: raises,
                known,
            });
            plan.decisions.push(Decision::Record {
                item: plan.items.len(),
                outranked,
                absent,
                late,
                future,
                unchecked: unreadable,
            });
            plan.items.push(Item::Fact(row));
        }
        plan.whole = read
            .into_iter()
            .filter(|(k, _)| !found.unreadable.contains(k))
            .collect();
        plan
    }
}

#[cfg(test)]
mod tests;
