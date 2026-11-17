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
//!
//! A range found lost while the store is open is taken in by [`Decider::settle`] (Q-921 to Q-923); DF's decider,
//! which keeps every fact with its position, is the oracle the differential test holds this one to.

use std::collections::{HashMap, HashSet};

use kvik_log::content::Item;
use kvik_log::entries::{
    IntervalLength as LogInterval, Kind as LogKind, Timing as LogTiming, Versioning,
};
use kvik_log::facts::{
    FactFlags as LogFlags, FactRow, MAX_TEXT_LEN, Value as LogValue, ValueType as LogType,
};

use kvik_log::writer::Position as LogPos;

use crate::durable::history::{Facts, History, Key, Known, Reference, span_of};
use crate::durable::index::{Index, quality_in};
use crate::durable::keys::LocalFacts;
use crate::durable::registry::Registry;
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
    /// Lateness references a read of the log settled, in the order settled (Q-924).
    references: Vec<(u32, Reference)>,
    /// Readings whose lateness reference could not be read: taken as not late (Q-925).
    pub(crate) reference_unread: u64,
}

/// The state decisions are made against: the registry and [`History`], including calls queued but not yet
/// acknowledged.
#[derive(Clone, Debug, Default)]
pub(crate) struct Decider {
    pub(crate) registry: Registry,
    pub(crate) history: History,
    /// Ranges found lost while the store is open, merged where they overlap or touch (Q-923).
    lost: Vec<(LogPos, LogPos)>,
}

/// What a read of a series' reference facts found (Q-924).
struct ReferenceRead {
    late: bool,
    state: Reference,
    /// Incomplete, with no reference fact found (Q-925).
    unread: bool,
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
    pub(crate) fn from_index(index: &Index, budget: usize) -> Self {
        let mut d = Decider {
            registry: index.registry().clone(),
            history: History::with_budget(budget),
            lost: Vec::new(),
        };
        let mut facts: Vec<_> = index
            .fact_series()
            .flat_map(|id| index.series_facts(id))
            .collect();
        facts.sort_by_key(|f| f.position);
        for f in facts {
            let id = f.row.series_id;
            let span = span_of(&d.registry, id);
            d.note_kept(&f.row, || span);
        }
        d
    }

    /// An empty state whose windows and revised-key index hold at most `budget` bytes.
    pub(crate) fn with_budget(budget: usize) -> Self {
        Decider {
            registry: Registry::default(),
            history: History::with_budget(budget),
            lost: Vec::new(),
        }
    }

    /// Takes in a fact of the kept log, read in log order at the start, of a series whose window span `span` gives.
    pub(crate) fn note_kept(&mut self, r: &FactRow, span: impl FnOnce() -> i64) {
        self.history
            .note((r.series_id, r.t), Known::of(r, 0), span, None);
    }

    /// Notes that every call below `call` is acknowledged.
    pub(crate) fn readable_below(&mut self, call: u64) {
        self.history.readable_below(call);
    }

    /// Takes in what happened since the last decision (Q-923): `acks` are the calls in flight in call order, each
    /// with its first and last positions once acknowledged, and `next` the call after them; `fresh` the ranges found
    /// lost since. The calls seen acknowledged stop at the first still pending, so they are always a prefix; they
    /// are returned and need no tracking. A call seen acknowledged inside a lost range, or a range not already
    /// lost, forgets what the log shows, once.
    pub(crate) fn settle(
        &mut self,
        acks: impl IntoIterator<Item = (u64, Option<(LogPos, LogPos)>)>,
        next: u64,
        fresh: &[(LogPos, LogPos)],
    ) -> Vec<u64> {
        let mut seen = Vec::new();
        let mut readable = next;
        let mut forget = false;
        for (call, ack) in acks {
            let Some((first, last)) = ack else {
                readable = call;
                break;
            };
            forget |= self.lost.iter().any(|&(a, b)| first <= b && a <= last);
            seen.push(call);
        }
        self.history.readable_below(readable);
        for &(first, last) in fresh {
            forget |= !self.lost.iter().any(|&(a, b)| a <= first && last <= b);
            self.lose(first, last);
        }
        if forget {
            self.history.forget_shown();
        }
        seen
    }

    fn lose(&mut self, first: LogPos, last: LogPos) {
        self.lost.push((first, last));
        self.lost.sort();
        let mut merged: Vec<(LogPos, LogPos)> = Vec::with_capacity(self.lost.len());
        for (a, b) in self.lost.drain(..) {
            match merged.last_mut() {
                Some((_, z)) if a <= after(*z) => *z = (*z).max(b),
                _ => merged.push((a, b)),
            }
        }
        self.lost = merged;
    }

    /// Takes a planned call's facts into the state, once it is queued; the same as noting each of its facts in
    /// order.
    pub(crate) fn apply(&mut self, plan: &mut Plan) {
        for (id, r) in std::mem::take(&mut plan.references) {
            self.history.settle_reference(id, r);
        }
        let mut whole = std::mem::take(&mut plan.whole);
        for n in std::mem::take(&mut plan.notes) {
            let w = whole.remove(&n.key);
            let registry = &self.registry;
            self.history
                .note(n.key, n.known, || span_of(registry, n.key.0), w);
        }
    }

    /// Reads series `id`'s reference facts with `t` at or below `upper`, highest part first, for a reading at `t`
    /// whose lateness reference is `AtMost { upper, least }` and the clause undecided (Q-924). A part spans the
    /// series' window span and doubles; the read stops at the first reference fact found, which proves the clause,
    /// or once no lower reference could. The settled state is exact only when every part read was complete.
    fn read_reference(
        &self,
        local: &mut dyn LocalFacts,
        id: u32,
        (upper, least): (i64, Option<i64>),
        t: i64,
        staleness: i64,
    ) -> ReferenceRead {
        let unchanged = |unread| ReferenceRead {
            late: false,
            state: Reference::AtMost { upper, least },
            unread,
        };
        // The highest reference for which the clause fails; none when `t + 2S` overflows, and then it never holds.
        let Some(fails) = t.checked_add(staleness.saturating_mul(2)) else {
            return unchanged(false);
        };
        let floor = fails.saturating_add(1);
        let mut hi = upper;
        let mut len = span_of(&self.registry, id).max(1);
        let mut complete = true;
        while hi >= floor {
            let lo = hi.saturating_sub(len - 1).max(floor);
            let (facts, whole) = local.read_between(id, lo, hi);
            complete &= whole;
            let found = facts
                .iter()
                .filter(|f| !f.row.flags.future && !f.row.flags.unsynced_clock)
                .map(|f| f.row.t)
                .max();
            if let Some(m) = found {
                let state = if complete {
                    Reference::Exact(Some(m))
                } else {
                    Reference::AtMost {
                        upper,
                        least: least.max(Some(m)),
                    }
                };
                return ReferenceRead {
                    late: true,
                    state,
                    unread: false,
                };
            }
            let Some(next) = lo.checked_sub(1) else {
                break;
            };
            hi = next;
            len = len.saturating_mul(2);
        }
        if !complete {
            return unchanged(true);
        }
        let state = match least {
            Some(l) if fails <= l => Reference::Exact(Some(l)),
            _ => Reference::AtMost {
                upper: fails,
                least,
            },
        };
        ReferenceRead {
            late: false,
            state,
            unread: false,
        }
    }

    /// The refusal of a reading, if any, in the order the model backend checks: unknown series, value type,
    /// retirement, phase, `changed_at` (then its range, Q-195), text length.
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
        if d.versioning == Versioning::Time
            && r.changed_at.is_some_and(|c| i64::try_from(c).is_err())
        {
            return Err(Refusal::ChangedAtOutOfRange);
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
            crate::durable::keys::Found::default()
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
        // The call's own facts and lateness reference matter only to a later reading of the same key or series.
        let (key_repeats, series_repeats) = repeats(readings);
        let mut batch: HashMap<Key, Vec<Known>> = HashMap::new();
        let mut batch_reference: HashMap<u32, i64> = HashMap::new();
        let mut references: HashMap<u32, Reference> = HashMap::new();
        for (k, r) in readings.iter().enumerate() {
            let (in_batch, series_in_batch) = (key_repeats[k], series_repeats[k]);
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
                    .chain(batch.get(&key).filter(|_| in_batch).into_iter().flatten())
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
            let id = r.series.0;
            let out_of_order = match d.staleness {
                Some(st) if observed && !future && clock.synced => {
                    let needs = |rf: i64| r.t.0 < rf.saturating_sub(st.saturating_mul(2));
                    let in_call = batch_reference.get(&id).filter(|_| series_in_batch);
                    let state = references
                        .get(&id)
                        .copied()
                        .unwrap_or_else(|| self.history.reference(id));
                    match state {
                        _ if in_call.is_some_and(|rf| needs(*rf)) => true,
                        Reference::Exact(rf) => rf.is_some_and(needs),
                        Reference::AtMost { least, .. } if least.is_some_and(needs) => true,
                        Reference::AtMost { upper, .. } if !needs(upper) => false,
                        Reference::AtMost { upper, least } => {
                            let read = self.read_reference(local, id, (upper, least), r.t.0, st);
                            references.insert(id, read.state);
                            plan.references.push((id, read.state));
                            plan.reference_unread += u64::from(read.unread);
                            read.late
                        }
                    }
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
            if in_batch {
                batch.entry(key).or_default().push(known.clone());
            }
            let raises = !future && clock.synced;
            if raises && series_in_batch {
                let v = batch_reference.entry(r.series.0).or_insert(r.t.0);
                *v = (*v).max(r.t.0);
            }
            plan.notes.push(Note { key, known });
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

/// The position after `p`, in its generation.
fn after(p: LogPos) -> LogPos {
    LogPos {
        generation: p.generation,
        sequence: p.sequence.saturating_add(1),
    }
}

/// Per reading, whether another reading of the call has its key, and whether one has its series.
fn repeats(readings: &[Reading]) -> (Vec<bool>, Vec<bool>) {
    let mut key = vec![false; readings.len()];
    let mut series = vec![false; readings.len()];
    let mut order: Vec<usize> = (0..readings.len()).collect();
    order.sort_unstable_by_key(|&i| (readings[i].series.0, readings[i].t.0));
    for w in order.windows(2) {
        let (a, b) = (&readings[w[0]], &readings[w[1]]);
        if a.series == b.series {
            series[w[0]] = true;
            series[w[1]] = true;
            if a.t == b.t {
                key[w[0]] = true;
                key[w[1]] = true;
            }
        }
    }
    (key, series)
}

#[cfg(test)]
mod differential;
#[cfg(test)]
mod tests;
