//! What the deliverer holds of the facts (roadmap 11.0): per series a recent window, every fact taken with `t` at or
//! above its floor, and for anything older the local log through [`crate::durable::keys`]. The floor follows the
//! earlier of the series' latest `t` and `now`, less the window's span (spec §5.1), so the facts a live-state fact
//! may come from as `now` advances stay in memory; at most [`WINDOW_FACTS`] facts below that point are kept.
//!
//! A key's earlier facts (whether a fact was outranked when committed, `spec.md` §4), a series' current facts before
//! its window (live state, held-for history) and the records a consumer behind needs are read from the log, counted
//! in `DeliveryStats::slow_path`. Damage there reads as no fact (Q-750).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use kvik_log::content::Item;
use kvik_log::reader::{FoundFact, ReadEvent};
use kvik_log::writer::Position as LogPos;

use super::{Ctx, Deliverer, FactRec};
use crate::durable::history::{Key, WINDOW_FACTS};
use crate::durable::index::Stored;
use crate::durable::keys::{Found, LocalFacts, OnLog};

/// Items a read of the log for a consumer behind takes at a time.
const PART: usize = 1 << 16;

/// How much further back each read of a series' facts before its window reaches than the one before.
const WIDEN: i64 = 16;

#[derive(Debug)]
struct Win {
    latest: i64,
    floor: i64,
    span: i64,
    /// In `(t, position)` order.
    facts: VecDeque<Arc<FactRec>>,
}

fn tp(f: &FactRec) -> (i64, LogPos) {
    (f.stored.row.t, f.stored.position)
}

/// Bytes a fact takes in a window: its slot, its shared record and a text value's bytes.
fn fact_bytes(f: &FactRec) -> usize {
    let heap = match &f.stored.row.value {
        kvik_log::facts::Value::Text(t) => t.capacity(),
        _ => 0,
    };
    std::mem::size_of::<Arc<FactRec>>()
        + std::mem::size_of::<FactRec>()
        + 2 * std::mem::size_of::<usize>()
        + heap
}

/// See the module documentation. Memory is bounded per series by the window's span and `cap` facts whatever `now`
/// is, and over all series by `budget` bytes, above which each series' oldest key leaves in turn (Q-751).
#[derive(Debug)]
pub(super) struct Recent {
    cap: usize,
    budget: usize,
    bytes: usize,
    series: HashMap<u32, Win>,
    order: Vec<u32>,
    cursor: usize,
}

impl Default for Recent {
    fn default() -> Self {
        Recent::with_limits(WINDOW_FACTS, usize::MAX)
    }
}

impl Recent {
    /// Windows of at most `cap` facts a series and `budget` bytes in all.
    pub(super) fn with_limits(cap: usize, budget: usize) -> Self {
        Recent {
            cap,
            budget,
            bytes: 0,
            series: HashMap::new(),
            order: Vec::new(),
            cursor: 0,
        }
    }

    /// Takes in a fact taken from the log at `now`, of a series whose window span `span` gives, asked once per
    /// series.
    pub(super) fn note(&mut self, rec: &Arc<FactRec>, span: impl FnOnce() -> i64, now: i64) {
        let t = rec.stored.row.t;
        let id = rec.stored.row.series_id;
        let w = match self.series.entry(id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                self.order.push(id);
                e.insert(Win {
                    latest: t,
                    floor: i64::MIN,
                    span: span(),
                    facts: VecDeque::new(),
                })
            }
        };
        if t < w.floor {
            return;
        }
        // In-order data goes at the end; only a late fact searches.
        let at = if w.facts.back().is_none_or(|f| tp(f) <= tp(rec)) {
            w.facts.len()
        } else {
            w.facts.partition_point(|f| tp(f) <= tp(rec))
        };
        if w.facts.len() >= self.cap && w.facts.len() == w.facts.capacity() {
            w.facts.reserve_exact(1);
        }
        w.facts.insert(at, rec.clone());
        self.bytes += fact_bytes(rec);
        w.latest = w.latest.max(t);
        let edge = w.latest.min(now);
        w.floor = w.floor.max(edge.saturating_sub(w.span));
        let mut freed = 0;
        while w.facts.front().is_some_and(|f| f.stored.row.t < w.floor) {
            freed += w.facts.pop_front().map_or(0, |f| fact_bytes(&f));
        }
        // The cap holds whatever `now` is: a fact at or ahead of it that leaves is read from the log when needed.
        while w.facts.len() > self.cap {
            let t0 = w.facts[0].stored.row.t;
            while w.facts.front().is_some_and(|f| f.stored.row.t == t0) {
                freed += w.facts.pop_front().map_or(0, |f| fact_bytes(&f));
            }
            w.floor = t0.saturating_add(1);
        }
        self.bytes -= freed;
        self.keep_budget();
    }

    /// Takes each series' oldest key in turn until the bytes held are within the budget.
    fn keep_budget(&mut self) {
        let mut idle = 0;
        while self.bytes > self.budget && idle < self.order.len() {
            self.cursor = (self.cursor + 1) % self.order.len();
            let Some(w) = self.series.get_mut(&self.order[self.cursor]) else {
                idle += 1;
                continue;
            };
            let Some(t0) = w.facts.front().map(|f| f.stored.row.t) else {
                idle += 1;
                continue;
            };
            idle = 0;
            while w.facts.front().is_some_and(|f| f.stored.row.t == t0) {
                self.bytes -= w.facts.pop_front().map_or(0, |f| fact_bytes(&f));
            }
            w.floor = w.floor.max(t0.saturating_add(1));
            if w.facts.capacity() > 2 * w.facts.len() + 16 {
                w.facts.shrink_to(w.facts.len() + w.facts.len() / 2);
            }
        }
    }

    /// Forgets the facts above `above`.
    pub(super) fn void(&mut self, above: LogPos) {
        let mut freed = 0;
        for w in self.series.values_mut() {
            w.facts.retain(|f| {
                let keep = f.stored.position <= above;
                if !keep {
                    freed += fact_bytes(f);
                }
                keep
            });
        }
        self.bytes -= freed;
    }

    /// The facts of a key in position order, or `None` when the key is below the window.
    pub(super) fn key(&self, (series, t): Key) -> Option<Vec<&Stored>> {
        let Some(w) = self.series.get(&series) else {
            return Some(Vec::new());
        };
        if t < w.floor {
            return None;
        }
        let a = w.facts.partition_point(|f| f.stored.row.t < t);
        let z = w.facts.partition_point(|f| f.stored.row.t <= t);
        Some(w.facts.range(a..z).map(|f| &f.stored).collect())
    }

    /// Every fact of the series with `t` at or above the floor is held; `i64::MIN` for a series with none.
    pub(super) fn floor(&self, series: u32) -> i64 {
        self.series.get(&series).map_or(i64::MIN, |w| w.floor)
    }

    /// The window's facts of a series with `t ≤ to`, newest first.
    fn newest_first(&self, series: u32, to: i64) -> impl Iterator<Item = &Stored> {
        let w = self.series.get(&series);
        let z = w.map_or(0, |w| w.facts.partition_point(|f| f.stored.row.t <= to));
        w.into_iter()
            .flat_map(move |w| w.facts.range(..z).rev().map(|f| &f.stored))
    }

    /// Facts held, over all series.
    #[cfg(test)]
    pub(super) fn held(&self) -> usize {
        self.series.values().map(|w| w.facts.len()).sum()
    }

    /// Bytes held, over all series.
    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }
}

/// The rank of a fact (`spec.md` §2.2): its position, or `(changed_at, position)` in a versioned series.
fn rank(versioned: bool, s: &Stored) -> (Option<u64>, LogPos) {
    (if versioned { s.row.changed_at } else { None }, s.position)
}

/// Visits the current facts among `facts` (newest first, those of one `t` together), newest first, while `visit`
/// returns `true`; returns whether it was never stopped.
fn current_newest_first<'a>(
    versioned: bool,
    facts: impl Iterator<Item = &'a Stored>,
    visit: &mut dyn FnMut(&Stored) -> bool,
) -> bool {
    let mut best: Option<&Stored> = None;
    for f in facts {
        match best {
            Some(b) if b.row.t == f.row.t => {
                if rank(versioned, f) > rank(versioned, b) {
                    best = Some(f);
                }
            }
            Some(b) => {
                if !visit(b) {
                    return false;
                }
                best = Some(f);
            }
            None => best = Some(f),
        }
    }
    best.is_none_or(visit)
}

fn stored(f: FoundFact) -> Stored {
    Stored {
        position: f.position,
        received_at: f.received_at,
        row: f.row,
    }
}

impl Deliverer {
    fn local(&self) -> Option<std::cell::RefMut<'_, kvik_log::reader::LogReader>> {
        std::cell::RefMut::filter_map(self.reader.borrow_mut(), Option::as_mut).ok()
    }

    /// Every local fact of each key, read from the log.
    pub(super) fn read_keys(&self, keys: &[Key]) -> Found {
        self.slow.set(self.slow.get() + 1);
        match self.local() {
            Some(mut r) => OnLog {
                reader: &mut r,
                lost: &self.damaged,
            }
            .at(keys),
            None => Found {
                unreadable: keys.iter().copied().collect(),
                ..Found::default()
            },
        }
    }

    /// Visits the current facts of a series with `t ≤ to`, newest first, while `visit` returns `true`: the window's,
    /// then the log's before it, read a span at a time.
    pub(super) fn back(&self, series: u32, to: i64, visit: &mut dyn FnMut(&Stored) -> bool) {
        let versioned = self
            .view
            .registry
            .get(series)
            .is_some_and(|s| s.def.versioning != kvik_log::entries::Versioning::None);
        if !current_newest_first(versioned, self.recent.newest_first(series, to), visit) {
            return;
        }
        let floor = self.recent.floor(series);
        if floor == i64::MIN {
            return;
        }
        let span = crate::durable::history::span_of(&self.view.registry, series);
        let mut hi = to.min(floor - 1);
        let mut width = span;
        loop {
            let lo = hi.saturating_sub(width);
            self.slow.set(self.slow.get() + 1);
            let read = self.local().and_then(|mut r| {
                OnLog {
                    reader: &mut r,
                    lost: &self.damaged,
                }
                .between(series, lo, hi)
            });
            let Some(found) = read else {
                self.incomplete.set(true);
                return;
            };
            // Only what has been taken: a batch read and not yet taken is not yet in force.
            let head = self.head;
            let mut facts: Vec<Stored> = found
                .into_iter()
                .filter(|f| f.position <= head)
                .map(stored)
                .collect();
            facts.sort_by_key(|f| std::cmp::Reverse((f.row.t, f.position)));
            if !current_newest_first(versioned, facts.iter(), visit) || lo == i64::MIN {
                return;
            }
            hi = lo - 1;
            width = width.saturating_mul(WIDEN);
        }
    }

    /// Whether a fact of a versioned series at `p` with `changed_at` `c` was outranked when committed: an earlier fact
    /// of its key has a higher `changed_at` (`spec.md` §4); and whether that rests on fewer than all its key's earlier
    /// facts. `read` holds the keys read from the log in this pass, each with whether it could not all be read.
    pub(super) fn outranked(
        &self,
        key: Key,
        p: LogPos,
        c: u64,
        read: &HashMap<Key, (Vec<FoundFact>, bool)>,
    ) -> (bool, bool) {
        let higher = |q: LogPos, d: Option<u64>| q < p && d.is_some_and(|d| d > c);
        match self.recent.key(key) {
            Some(facts) => (
                facts.iter().any(|f| higher(f.position, f.row.changed_at)),
                false,
            ),
            None => match read.get(&key) {
                Some((facts, unreadable)) => {
                    let outranked = facts.iter().any(|f| higher(f.position, f.row.changed_at));
                    (outranked, *unreadable && !outranked)
                }
                None => (false, true),
            },
        }
    }

    /// The facts with positions from `at` up to `to` (not included), read from the log, each with whether it was
    /// outranked when committed and whether that rests on fewer than all its key's earlier facts; `None` when the log
    /// could not be read to `to`.
    pub(super) fn facts_between(
        &self,
        at: LogPos,
        to: LogPos,
        ctx: &Ctx<'_>,
    ) -> Option<Vec<(Stored, bool, bool)>> {
        self.slow.set(self.slow.get() + 1);
        let mut reader = crate::durable::start::log_reader(ctx.dir, ctx.env).ok()?;
        let read = read_positions(&mut reader, at, to);
        let _ = reader.close();
        let out = read?;
        let mut keys: Vec<Key> = out
            .iter()
            .filter(|s| s.row.changed_at.is_some())
            .map(|s| (s.row.series_id, s.row.t))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        let found = if keys.is_empty() {
            Found::default()
        } else {
            self.read_keys(&keys)
        };
        Some(
            out.into_iter()
                .map(|s| {
                    let key = (s.row.series_id, s.row.t);
                    let Some(c) = s.row.changed_at else {
                        return (s, false, false);
                    };
                    let outranked = found.facts.get(&key).into_iter().flatten().any(|f| {
                        f.position < s.position && f.row.changed_at.is_some_and(|d| d > c)
                    });
                    let unchecked = !outranked && found.unreadable.contains(&key);
                    (s, outranked, unchecked)
                })
                .collect(),
        )
    }
}

/// The facts with positions from `at` up to `to` (not included); `None` if a part of the log could not be read
/// before `to` was reached.
fn read_positions(
    reader: &mut kvik_log::reader::LogReader,
    at: LogPos,
    to: LogPos,
) -> Option<Vec<Stored>> {
    let mut out: Vec<Stored> = Vec::new();
    loop {
        let (u, more) = reader.refresh_some(PART).ok()?;
        for e in u.events {
            match e {
                ReadEvent::Batch(b) => {
                    if b.first >= to {
                        return Some(out);
                    }
                    if b.last < at {
                        continue;
                    }
                    let mut sequence = b.first.sequence;
                    for item in b.content.items {
                        let p = LogPos {
                            generation: b.first.generation,
                            sequence,
                        };
                        sequence += item.sequences();
                        if let Item::Fact(row) = item
                            && p >= at
                            && p < to
                        {
                            out.push(Stored {
                                position: p,
                                received_at: b.content.received_at,
                                row,
                            });
                        }
                    }
                }
                ReadEvent::Void { above, .. } => out.retain(|s| s.position <= above),
                ReadEvent::Corruption { .. } => {}
            }
        }
        if !more {
            return Some(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use kvik_log::facts::{FactFlags, FactRow, Quality, Value};

    use super::*;

    const S: i64 = 1_000_000_000;
    const SPAN: i64 = 10 * S;

    fn rec(series: u32, t: i64, sequence: u64) -> Arc<FactRec> {
        Arc::new(FactRec {
            stored: Stored {
                position: LogPos {
                    generation: 0,
                    sequence,
                },
                received_at: 0,
                row: FactRow {
                    series_id: series,
                    t,
                    quality: Quality::Good,
                    flags: FactFlags::default(),
                    value: Value::F64(0),
                    changed_at: None,
                },
            },
            outranked: false,
            unchecked: false,
        })
    }

    /// Notes one fact a second for series 1, `t` from 0 to `n - 1`, at `now`.
    fn filled(r: &mut Recent, n: i64, now: i64) {
        for k in 0..n {
            r.note(&rec(1, k * S, k as u64 + 1), || SPAN, now);
        }
    }

    #[test]
    fn the_window_holds_every_fact_from_its_floor_and_none_below() {
        let mut r = Recent::with_limits(1_000, usize::MAX);
        filled(&mut r, 100, 1_000 * S);
        let floor = 99 * S - SPAN;
        assert_eq!(r.floor(1), floor);
        assert_eq!(r.key((1, floor)).map(|v| v.len()), Some(1));
        assert!(r.key((1, floor - S)).is_none());
        assert_eq!(r.held(), 11);
    }

    #[test]
    fn the_window_follows_now_when_it_is_behind_the_data() {
        let mut r = Recent::with_limits(1_000, usize::MAX);
        filled(&mut r, 100, 50 * S);
        assert_eq!(r.floor(1), 50 * S - SPAN);
        assert_eq!(r.held(), 60);
    }

    #[test]
    fn the_cap_holds_whatever_now_is_and_raises_the_floor_past_the_key_that_left() {
        let mut r = Recent::with_limits(4, usize::MAX);
        filled(&mut r, 8, 0);
        assert_eq!(r.held(), 4);
        assert_eq!(r.floor(1), 3 * S + 1);
        assert!(r.key((1, 3 * S)).is_none());
        assert_eq!(r.key((1, 4 * S)).map(|v| v.len()), Some(1));
    }

    #[test]
    fn the_windows_stay_within_their_byte_budget() {
        let one = fact_bytes(&rec(1, 0, 1));
        let mut r = Recent::with_limits(1_000, 50 * one);
        for k in 0..400u64 {
            r.note(
                &rec((k % 20) as u32, (k / 20) as i64 * S, k + 1),
                || SPAN,
                i64::MAX,
            );
            assert!(r.bytes() <= 50 * one);
        }
        assert_eq!(r.held(), 50);
        // Each series keeps its newest facts; what left is below its floor.
        for s in 0..20 {
            assert!(r.key((s, 0)).is_none());
            assert_eq!(r.key((s, 19 * S)).map(|v| v.len()), Some(1));
        }
    }
}
