//! The state the duplicate rule is decided against (spec §2.4, §5.1; roadmap 11.0), bounded by the recent window
//! and the revised-key index rather than by the facts stored.
//!
//! Per series: the latest recorded `t` (the fast path), the lateness reference, and the recent window, which holds
//! every fact of the series with `t` at or above its floor. A key below the floor is found in the revised-key index,
//! which holds the complete history of keys with more than one fact that this process saw leave the window or read
//! whole; otherwise the slow path reads the key from the local log ([`super::keys`]), together with the overlay of
//! facts below the floor whose calls the log may not yet show.

use std::collections::hash_map::Entry as Slot;
use std::collections::{BTreeMap, HashMap, VecDeque};

use kvik_log::entries::IntervalLength as LogInterval;
use kvik_log::facts::{FactRow, Quality as LogQuality, Value as LogValue};

use super::registry::Registry;

/// The recent window's event-time span (spec §5.1: 15 minutes by default).
pub(crate) const WINDOW_SPAN: i64 = 15 * 60 * 1_000_000_000;
/// The most facts the recent window holds per series (spec §5.1).
pub(crate) const WINDOW_FACTS: usize = 1_024;

/// The recent window's span for a series: at least an interval series' length (spec §5.1).
pub(crate) fn span_of(registry: &Registry, id: u32) -> i64 {
    match registry.get(id).and_then(|s| s.def.interval.as_ref()) {
        Some(LogInterval::Fixed { length, .. }) => WINDOW_SPAN.max(*length),
        _ => WINDOW_SPAN,
    }
}

/// A key: series and event time.
pub(crate) type Key = (u32, i64);

/// A recorded fact as the duplicate rule compares it: value, adapter-supplied quality and `changed_at`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Known {
    pub(crate) value: LogValue,
    pub(crate) supplied: LogQuality,
    pub(crate) changed_at: Option<u64>,
    /// The call that queued it in this process; 0 for a fact read from the log.
    pub(crate) call: u64,
}

impl Known {
    pub(crate) fn of(r: &FactRow, call: u64) -> Self {
        Known {
            value: r.value.clone(),
            supplied: r.quality,
            changed_at: r.changed_at,
            call,
        }
    }

    /// Identity (spec §2.4): values compare bitwise, a value recorded without one as "no value".
    pub(crate) fn identical(&self, value: &LogValue, supplied: LogQuality) -> bool {
        self.supplied == supplied
            && match (&self.value, value) {
                (LogValue::Absent { .. }, LogValue::Absent { .. }) => true,
                (a, b) => a == b,
            }
    }
}

/// One series' part of the state.
#[derive(Clone, Debug, PartialEq)]
struct Series {
    latest: Option<i64>,
    reference: Option<i64>,
    /// Every fact of the series with `t ≥ floor` is in `window`.
    floor: i64,
    /// The window's facts in `t` order, those of one key in the order they were noted.
    window: VecDeque<(i64, Known)>,
    /// The window's span for this series: at least an interval series' length (spec §5.1).
    span: i64,
}

impl Series {
    fn new(span: i64) -> Self {
        Series {
            latest: None,
            reference: None,
            floor: i64::MIN,
            window: VecDeque::new(),
            span,
        }
    }

    fn key_range(&self, t: i64) -> std::ops::Range<usize> {
        let a = self.window.partition_point(|(u, _)| *u < t);
        let z = self.window.partition_point(|(u, _)| *u <= t);
        a..z
    }
}

/// Where a key's recorded facts are, as the state knows them.
#[derive(Debug)]
pub(crate) enum Facts<'a> {
    /// All of them, here (none for a key above the latest `t`).
    Known(Vec<&'a Known>),
    /// Not in memory: the slow path reads the log; these are the facts the log may not show yet.
    Slow(Vec<&'a Known>),
}

/// Heap bytes a value holds beyond its own size.
fn heap(v: &LogValue) -> usize {
    match v {
        LogValue::Text(t) => t.capacity(),
        _ => 0,
    }
}

/// Bytes a fact takes in a window.
fn window_bytes(k: &Known) -> usize {
    std::mem::size_of::<(i64, Known)>() + heap(&k.value)
}

/// Bytes a fact takes in the revised-key index, and an entry's own.
fn revised_fact_bytes(k: &Known) -> usize {
    std::mem::size_of::<Known>() + heap(&k.value)
}
const REVISED_ENTRY: usize = std::mem::size_of::<(Key, Vec<Known>)>() + std::mem::size_of::<Key>();

/// See the module documentation. Memory is bounded twice: per series by the window's span and [`WINDOW_FACTS`], and
/// over all series by `budget` bytes for the windows and the revised-key index together (Q-751); above it the
/// revised-key entries the log already shows go first, then each series' oldest key in turn. Correctness never
/// depends on either: what leaves memory is read from the log.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct History {
    /// The window's span for every series in place of its own, and its most facts: other values in tests only.
    span: Option<i64>,
    cap: usize,
    budget: usize,
    /// Bytes held in windows and the revised-key index.
    bytes: usize,
    series: HashMap<u32, Series>,
    /// Series in the order they were first noted, and where the next eviction for the budget looks.
    order: Vec<u32>,
    cursor: usize,
    revised: HashMap<Key, Vec<Known>>,
    revised_order: VecDeque<Key>,
    /// Facts below their series' floor and not in `revised`, of calls the log may not show yet.
    pending: HashMap<Key, Vec<Known>>,
    pending_calls: BTreeMap<u64, Vec<Key>>,
    /// Every call below this one is acknowledged, so the log shows its facts.
    readable_below: u64,
}

impl Default for History {
    fn default() -> Self {
        History::with_budget(usize::MAX)
    }
}

impl History {
    /// A history holding at most `budget` bytes in windows and the revised-key index.
    pub(crate) fn with_budget(budget: usize) -> Self {
        History {
            span: None,
            cap: WINDOW_FACTS,
            budget,
            bytes: 0,
            series: HashMap::new(),
            order: Vec::new(),
            cursor: 0,
            revised: HashMap::new(),
            revised_order: VecDeque::new(),
            pending: HashMap::new(),
            pending_calls: BTreeMap::new(),
            readable_below: 0,
        }
    }

    /// A history whose window spans `span` for every series and holds at most `cap` facts per series.
    #[cfg(test)]
    pub(crate) fn with_window(span: i64, cap: usize) -> Self {
        History {
            span: Some(span),
            cap,
            ..History::default()
        }
    }

    /// [`History::with_window`] with a byte budget.
    #[cfg(test)]
    pub(crate) fn with_window_and_budget(span: i64, cap: usize, budget: usize) -> Self {
        History {
            span: Some(span),
            cap,
            budget,
            ..History::default()
        }
    }

    /// The facts of a key, or that the slow path must read it.
    pub(crate) fn facts(&self, key: Key) -> Facts<'_> {
        let Some(s) = self.series.get(&key.0) else {
            return Facts::Known(Vec::new());
        };
        if s.latest.is_none_or(|l| key.1 > l) {
            return Facts::Known(Vec::new());
        }
        if key.1 >= s.floor {
            return Facts::Known(s.window.range(s.key_range(key.1)).map(|(_, k)| k).collect());
        }
        if let Some(r) = self.revised.get(&key) {
            return Facts::Known(r.iter().collect());
        }
        Facts::Slow(self.pending.get(&key).into_iter().flatten().collect())
    }

    /// The lateness reference of a series (spec §2.2).
    pub(crate) fn reference(&self, id: u32) -> Option<i64> {
        self.series.get(&id).and_then(|s| s.reference)
    }

    /// Notes that every call below `call` is acknowledged, dropping the overlay facts the log now shows.
    pub(crate) fn readable_below(&mut self, call: u64) {
        self.readable_below = self.readable_below.max(call);
        let later = self.pending_calls.split_off(&self.readable_below);
        for (c, keys) in std::mem::replace(&mut self.pending_calls, later) {
            for key in keys {
                if let Slot::Occupied(mut e) = self.pending.entry(key) {
                    e.get_mut().retain(|k| k.call != c);
                    if e.get().is_empty() {
                        e.remove();
                    }
                }
            }
        }
    }

    fn shown(&self, k: &Known) -> bool {
        k.call == 0 || k.call < self.readable_below
    }

    /// Takes in a recorded fact of a series whose window span `span` gives, asked once per series; `reference`: it
    /// raises the lateness reference. `whole` is the key's complete history before it when the slow path read it, so
    /// that a key with more than one fact enters the revised-key index.
    pub(crate) fn note(
        &mut self,
        key: Key,
        reference: bool,
        known: Known,
        span: impl FnOnce() -> i64,
        whole: Option<Vec<Known>>,
    ) {
        let (id, t) = key;
        let fixed = self.span;
        let s = match self.series.entry(id) {
            Slot::Occupied(e) => e.into_mut(),
            Slot::Vacant(e) => {
                self.order.push(id);
                e.insert(Series::new(fixed.unwrap_or_else(span)))
            }
        };
        if reference {
            s.reference = s.reference.max(Some(t));
        }
        if t >= s.floor {
            self.bytes += window_bytes(&known);
            // In-order data goes at the end; only a late fact searches.
            let at = if s.window.back().is_none_or(|(u, _)| *u <= t) {
                s.window.len()
            } else {
                s.window.partition_point(|(u, _)| *u <= t)
            };
            // At the cap the fact is inserted before the eviction: room for one more, not a doubled queue.
            if s.window.len() >= self.cap && s.window.len() == s.window.capacity() {
                s.window.reserve_exact(1);
            }
            s.window.insert(at, (t, known));
            if s.latest.is_none_or(|l| t > l) {
                s.latest = Some(t);
                self.evict(id);
            } else if s.window.len() > self.cap {
                self.evict(id);
            }
        } else if let Some(h) = self.revised.get_mut(&key) {
            self.bytes += revised_fact_bytes(&known);
            h.push(known);
        } else {
            match whole {
                Some(mut h) => {
                    h.extend(self.pending.remove(&key).unwrap_or_default());
                    h.push(known);
                    if h.len() > 1 {
                        self.insert_revised(key, h);
                    } else {
                        for k in h {
                            self.overlay(key, k);
                        }
                    }
                }
                None => self.overlay(key, known),
            }
        }
        self.keep_budget();
    }

    fn insert_revised(&mut self, key: Key, facts: Vec<Known>) {
        self.bytes += REVISED_ENTRY + facts.iter().map(revised_fact_bytes).sum::<usize>();
        if let Some(old) = self.revised.insert(key, facts) {
            self.bytes -= REVISED_ENTRY + old.iter().map(revised_fact_bytes).sum::<usize>();
        } else {
            self.revised_order.push_back(key);
        }
    }

    fn overlay(&mut self, key: Key, k: Known) {
        if self.shown(&k) {
            return;
        }
        self.pending_calls.entry(k.call).or_default().push(key);
        self.pending.entry(key).or_default().push(k);
    }

    /// Raises a series' floor to its latest `t` less its span, then while the window holds more than `cap` facts,
    /// past its lowest key; keys that leave go to the revised-key index (more than one fact) or the overlay.
    fn evict(&mut self, id: u32) {
        let cap = self.cap;
        let readable = self.readable_below;
        let Some(s) = self.series.get_mut(&id) else {
            return;
        };
        let Some(latest) = s.latest else { return };
        s.floor = s.floor.max(latest.saturating_sub(s.span));
        let mut out: Vec<(i64, Vec<Known>)> = Vec::new();
        let mut freed = 0;
        while let Some(&(t, _)) = s.window.front() {
            if t >= s.floor {
                if s.window.len() <= cap {
                    break;
                }
                s.floor = t.saturating_add(1);
            }
            let (_, k) = s.window.pop_front().expect("looked at above");
            freed += window_bytes(&k);
            let alone = s.window.front().is_none_or(|(u, _)| *u != t)
                && out.last().is_none_or(|(u, _)| *u != t);
            // The common case, a key of one fact the log already shows, leaves without a trace.
            if alone && (k.call == 0 || k.call < readable) {
                continue;
            }
            match out.last_mut() {
                Some((u, facts)) if *u == t => facts.push(k),
                _ => out.push((t, vec![k])),
            }
        }
        self.bytes -= freed;
        for (t, facts) in out {
            if facts.len() > 1 {
                self.insert_revised((id, t), facts);
            } else {
                for k in facts {
                    self.overlay((id, t), k);
                }
            }
        }
    }

    /// Brings the bytes held under the budget: first revised-key entries whose facts the log shows, oldest first,
    /// then the oldest key of each series' window in turn, which the slow path then reads.
    fn keep_budget(&mut self) {
        if self.bytes <= self.budget {
            return;
        }
        let mut tries = self.revised_order.len();
        while self.bytes > self.budget && tries > 0 {
            tries -= 1;
            let Some(key) = self.revised_order.pop_front() else {
                break;
            };
            let Some(facts) = self.revised.get(&key) else {
                continue;
            };
            if facts.iter().all(|k| self.shown(k)) {
                let facts = self.revised.remove(&key).expect("looked up above");
                self.bytes -= REVISED_ENTRY + facts.iter().map(revised_fact_bytes).sum::<usize>();
            } else {
                self.revised_order.push_back(key);
            }
        }
        let mut idle = 0;
        while self.bytes > self.budget && idle < self.order.len() {
            self.cursor = (self.cursor + 1) % self.order.len();
            let id = self.order[self.cursor];
            let readable = self.readable_below;
            let Some(s) = self.series.get_mut(&id) else {
                idle += 1;
                continue;
            };
            let Some(&(t, _)) = s.window.front() else {
                idle += 1;
                continue;
            };
            idle = 0;
            s.floor = s.floor.max(t.saturating_add(1));
            let mut facts = Vec::new();
            while s.window.front().is_some_and(|(u, _)| *u == t) {
                let (_, k) = s.window.pop_front().expect("looked at above");
                self.bytes -= window_bytes(&k);
                facts.push(k);
            }
            if s.window.capacity() > 2 * s.window.len() + 16 {
                s.window.shrink_to(s.window.len() + s.window.len() / 2);
            }
            // A key the log shows in full leaves; one with facts in flight keeps them where the slow path looks.
            if facts.iter().all(|k| k.call == 0 || k.call < readable) {
                continue;
            }
            if facts.len() > 1 {
                self.insert_revised((id, t), facts);
            } else {
                for k in facts {
                    self.overlay((id, t), k);
                }
            }
        }
    }

    /// What the state holds: facts in windows, keys and facts in the revised-key index, facts in the overlay.
    #[cfg(test)]
    pub(crate) fn held(&self) -> Held {
        Held {
            bytes: self.bytes,
            window: self.series.values().map(|s| s.window.len()).sum(),
            capacity: self.series.values().map(|s| s.window.capacity()).sum(),
            series: self.series.len(),
            revised_keys: self.revised.len(),
            revised_facts: self.revised.values().map(Vec::len).sum(),
            overlay: self.pending.values().map(Vec::len).sum(),
        }
    }
}

/// Sizes of a [`History`].
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) bytes: usize,
    pub(crate) window: usize,
    pub(crate) capacity: usize,
    pub(crate) series: usize,
    pub(crate) revised_keys: usize,
    pub(crate) revised_facts: usize,
    pub(crate) overlay: usize,
}
