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
    /// In `(t, position)` order.
    facts: VecDeque<Arc<FactRec>>,
}

fn tp(f: &FactRec) -> (i64, LogPos) {
    (f.stored.row.t, f.stored.position)
}

/// See the module documentation.
#[derive(Debug, Default)]
pub(super) struct Recent {
    series: HashMap<u32, Win>,
}

impl Recent {
    /// Takes in a fact taken from the log, of a series whose window spans `span`, at `now`.
    pub(super) fn note(&mut self, rec: &Arc<FactRec>, span: i64, now: i64) {
        let t = rec.stored.row.t;
        let w = self
            .series
            .entry(rec.stored.row.series_id)
            .or_insert_with(|| Win {
                latest: t,
                floor: i64::MIN,
                facts: VecDeque::new(),
            });
        if t < w.floor {
            return;
        }
        let at = w.facts.partition_point(|f| tp(f) <= tp(rec));
        w.facts.insert(at, rec.clone());
        w.latest = w.latest.max(t);
        let edge = w.latest.min(now);
        w.floor = w.floor.max(edge.saturating_sub(span));
        while w.facts.front().is_some_and(|f| f.stored.row.t < w.floor) {
            w.facts.pop_front();
        }
        while w.facts.len() > WINDOW_FACTS {
            let t0 = w.facts[0].stored.row.t;
            if t0 >= edge {
                break;
            }
            while w.facts.front().is_some_and(|f| f.stored.row.t == t0) {
                w.facts.pop_front();
            }
            w.floor = t0.saturating_add(1);
        }
    }

    /// Forgets the facts above `above`.
    pub(super) fn void(&mut self, above: LogPos) {
        for w in self.series.values_mut() {
            w.facts.retain(|f| f.stored.position <= above);
        }
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
            let Some(found) = read else { return };
            let mut facts: Vec<Stored> = found.into_iter().map(stored).collect();
            facts.sort_by_key(|f| std::cmp::Reverse((f.row.t, f.position)));
            if !current_newest_first(versioned, facts.iter(), visit) || lo == i64::MIN {
                return;
            }
            hi = lo - 1;
            width = width.saturating_mul(WIDEN);
        }
    }

    /// Whether a fact of a versioned series at `p` with `changed_at` `c` was outranked when committed: an earlier fact
    /// of its key has a higher `changed_at` (`spec.md` §4). `read` holds the keys read from the log in this pass.
    pub(super) fn outranked(
        &self,
        key: Key,
        p: LogPos,
        c: u64,
        read: &HashMap<Key, Vec<FoundFact>>,
    ) -> bool {
        let higher = |q: LogPos, d: Option<u64>| q < p && d.is_some_and(|d| d > c);
        match self.recent.key(key) {
            Some(facts) => facts.iter().any(|f| higher(f.position, f.row.changed_at)),
            None => read
                .get(&key)
                .into_iter()
                .flatten()
                .any(|f| higher(f.position, f.row.changed_at)),
        }
    }

    /// The facts with positions from `at` up to `to` (not included), read from the log, each with whether it was
    /// outranked when committed.
    pub(super) fn facts_between(
        &self,
        at: LogPos,
        to: LogPos,
        ctx: &Ctx<'_>,
    ) -> Vec<(Stored, bool)> {
        self.slow.set(self.slow.get() + 1);
        let Ok(mut reader) = crate::durable::start::log_reader(ctx.dir, ctx.env) else {
            return Vec::new();
        };
        let mut out: Vec<Stored> = Vec::new();
        'read: while let Ok((u, more)) = reader.refresh_some(PART) {
            for e in u.events {
                match e {
                    ReadEvent::Batch(b) => {
                        if b.first >= to {
                            break 'read;
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
                break;
            }
        }
        let _ = reader.close();
        let mut keys: Vec<Key> = out
            .iter()
            .filter(|s| s.row.changed_at.is_some())
            .map(|s| (s.row.series_id, s.row.t))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        let read = if keys.is_empty() {
            HashMap::new()
        } else {
            self.read_keys(&keys).facts
        };
        out.into_iter()
            .map(|s| {
                let key = (s.row.series_id, s.row.t);
                let outranked = s.row.changed_at.is_some_and(|c| {
                    read.get(&key)
                        .into_iter()
                        .flatten()
                        .any(|f| f.position < s.position && f.row.changed_at.is_some_and(|d| d > c))
                });
                (s, outranked)
            })
            .collect()
    }
}
