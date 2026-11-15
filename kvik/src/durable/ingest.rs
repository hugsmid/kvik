//! Deciding a call's outcomes before it is committed (spec §2.2 to §2.5): refusals, stamping and flags, values
//! recorded without one, the duplicate rule and outranked facts. The reference model makes the same decisions
//! inside its commit, which is not callable on its own; `tests/durable_differential.rs` holds the two backends to
//! the same outcomes and facts.
//!
//! Decisions are made in the order calls are queued in the log, against every fact already queued, so a call
//! sees the calls before it whether or not their sync has returned yet.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use kvik_log::content::Item;
use kvik_log::entries::{
    IntervalLength as LogInterval, Kind as LogKind, Timing as LogTiming, Versioning,
};
use kvik_log::facts::{
    FactFlags as LogFlags, FactRow, MAX_TEXT_LEN, Quality as LogQuality, Value as LogValue,
    ValueType as LogType,
};
use kvik_log::writer::Position as LogPos;

use super::index::{Index, quality_in};
use super::registry::Registry;
use crate::error::Refusal;
use crate::types::{Intent, Reading, TimeSource, Value};

/// Where a recorded fact is in the log.
#[derive(Clone, Copy, Debug, PartialEq)]
enum At {
    /// A fact of the kept log read at the start, at its position.
    Kept(LogPos),
    /// The fact with this index among its call's items.
    Item(u64),
}

/// A recorded fact as the duplicate rule compares it: value, adapter-supplied quality and `changed_at`.
#[derive(Clone, Debug, PartialEq)]
struct Known {
    value: LogValue,
    supplied: LogQuality,
    changed_at: Option<u64>,
    /// The call that queued it in this process; 0 for a fact of the kept log read at the start.
    call: u64,
    at: At,
    /// Flagged neither `future` nor `unsynced_clock`: it counts for the lateness reference (spec §2.2).
    reference: bool,
}

impl Known {
    fn of(r: &FactRow, call: u64, at: At) -> Self {
        Known {
            value: r.value.clone(),
            supplied: r.quality,
            changed_at: r.changed_at,
            call,
            at,
            reference: !r.flags.future && !r.flags.unsynced_clock,
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
    /// The call this plan is for.
    call: u64,
}

/// The state decisions are made against: the registry and the history of every key, including calls queued but
/// not yet acknowledged, less the facts of ranges found lost while the store is open (Q-200).
#[derive(Clone, Debug, Default)]
pub(crate) struct Decider {
    pub(crate) registry: Registry,
    history: HashMap<(u32, i64), Vec<Known>>,
    versions: HashMap<(u32, i64), u64>,
    reference: HashMap<u32, i64>,
    /// The first and last positions of each acknowledged call of this process.
    placed: BTreeMap<u64, (LogPos, LogPos)>,
    lost: Vec<(LogPos, LogPos)>,
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
                d.note(&f.row, 0, At::Kept(f.position));
            }
        }
        d
    }

    fn note(&mut self, r: &FactRow, call: u64, at: At) {
        let key = (r.series_id, r.t);
        self.history
            .entry(key)
            .or_default()
            .push(Known::of(r, call, at));
        if let Some(c) = r.changed_at {
            let v = self.versions.entry(key).or_insert(c);
            *v = (*v).max(c);
        }
        if !r.flags.future && !r.flags.unsynced_clock {
            let v = self.reference.entry(r.series_id).or_insert(r.t);
            *v = (*v).max(r.t);
        }
    }

    /// Takes a planned call's facts into the state, once it is queued.
    pub(crate) fn apply(&mut self, plan: &Plan) {
        for (i, item) in plan.items.iter().enumerate() {
            if let Item::Fact(r) = item {
                self.note(r, plan.call, At::Item(i as u64));
            }
        }
    }

    /// Notes the positions the log gave an acknowledged call.
    pub(crate) fn placed(&mut self, call: u64, first: LogPos, last: LogPos) {
        self.placed.insert(call, (first, last));
        if self.lost.iter().any(|&(a, b)| first <= b && a <= last) {
            self.forget_lost();
        }
    }

    /// Takes in a range found lost while the store is open: its facts leave every key's history, so a replay of
    /// one is recorded again (spec §3 Recovery rule 2, Q-200).
    pub(crate) fn lose(&mut self, first: LogPos, last: LogPos) {
        self.lost.push((first, last));
        self.forget_lost();
    }

    fn position(&self, k: &Known) -> Option<LogPos> {
        match k.at {
            At::Kept(p) => Some(p),
            At::Item(i) => self.placed.get(&k.call).map(|(f, _)| LogPos {
                generation: f.generation,
                sequence: f.sequence + i,
            }),
        }
    }

    fn forget_lost(&mut self) {
        let mut history = std::mem::take(&mut self.history);
        let mut changed = Vec::new();
        for (key, ks) in &mut history {
            let n = ks.len();
            ks.retain(|k| {
                !self
                    .position(k)
                    .is_some_and(|p| self.lost.iter().any(|&(a, b)| a <= p && p <= b))
            });
            if ks.len() != n {
                changed.push(*key);
            }
        }
        history.retain(|_, ks| !ks.is_empty());
        self.history = history;
        let series: BTreeSet<u32> = changed.iter().map(|(id, _)| *id).collect();
        for key in changed {
            match self
                .history
                .get(&key)
                .and_then(|ks| ks.iter().filter_map(|k| k.changed_at).max())
            {
                Some(c) => self.versions.insert(key, c),
                None => self.versions.remove(&key),
            };
        }
        for id in &series {
            self.reference.remove(id);
        }
        for ((id, t), ks) in &self.history {
            if series.contains(id) && ks.iter().any(|k| k.reference) {
                let v = self.reference.entry(*id).or_insert(*t);
                *v = (*v).max(*t);
            }
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
        let mut plan = Plan {
            call,
            ..Plan::default()
        };
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
            batch_history.entry(key).or_default().push(Known::of(
                &row,
                call,
                At::Item(plan.items.len() as u64),
            ));
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
            });
            plan.items.push(Item::Fact(row));
        }
        plan
    }
}
