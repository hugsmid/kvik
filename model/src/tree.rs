//! The per-series summary tree (spec §5.1, §5.6), calendar days (§5.7) and
//! window evaluation (§6).
//!
//! Stored levels are 1 minute and 15 minutes, aligned to UTC; the 1-second
//! level is never stored. A 1-minute node is the fold over its readings; a
//! 15-minute node is the merge of its minutes. The tree keeps the series'
//! summarised current readings, which stand in for the raw facts a window edge
//! is computed from.

use std::collections::{BTreeMap, BTreeSet};

use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::summary::{Node, Params, direct, merge, merge_all, with_stretches};
use crate::types::{MINUTE, Nanos, QUARTER, Value, floor_to};

/// Node widths of the stored levels: index 0 is 1 minute, 1 is 15 minutes.
pub const LEVELS: [Nanos; 2] = [MINUTE, QUARTER];

/// Errors from calendar evaluation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// A local day boundary does not fall on a 15-minute UTC boundary.
    #[error("local day {date} in {zone} starts {offset_ns} ns past a 15-minute boundary")]
    UnalignedDay {
        /// The date.
        date: Date,
        /// Time zone name.
        zone: String,
        /// Misalignment.
        offset_ns: Nanos,
    },
    /// A date could not be placed in the zone.
    #[error("calendar error: {0}")]
    Calendar(String),
}

/// Summary tree for one series.
#[derive(Clone, Debug, PartialEq)]
pub struct SeriesTree {
    /// Parameters the summaries depend on.
    pub params: Params,
    readings: BTreeMap<Nanos, Value>,
    levels: [BTreeMap<Nanos, Node>; 2],
    versions: BTreeMap<Nanos, u64>,
    stale_minutes: BTreeSet<Nanos>,
}

impl SeriesTree {
    /// An empty tree.
    pub fn new(params: Params) -> Self {
        SeriesTree {
            params,
            readings: BTreeMap::new(),
            levels: Default::default(),
            versions: BTreeMap::new(),
            stale_minutes: BTreeSet::new(),
        }
    }

    /// Builds a tree from scratch from the summarised current readings
    /// (any order): rebuild (§1.2).
    pub fn build(params: Params, readings: &[(Nanos, Value)]) -> Self {
        let mut tree = SeriesTree::new(params);
        tree.readings = readings.iter().cloned().collect();
        let minutes: BTreeSet<Nanos> = tree.readings.keys().map(|t| floor_to(*t, MINUTE)).collect();
        for m in &minutes {
            tree.recompute_minute(*m);
        }
        let quarters: BTreeSet<Nanos> = minutes.iter().map(|m| floor_to(*m, QUARTER)).collect();
        for q in quarters {
            tree.recompute_quarter(q);
        }
        tree
    }

    /// Replaces the readings of one minute with `readings` (that minute's
    /// summarised current readings) and recomputes the minute and its
    /// 15-minute ancestor (§5.6).
    pub fn update_minute(&mut self, minute: Nanos, readings: &[(Nanos, Value)]) {
        assert_eq!(minute, floor_to(minute, MINUTE));
        let old: Vec<Nanos> = self
            .readings
            .range(minute..minute + MINUTE)
            .map(|(t, _)| *t)
            .collect();
        for t in old {
            self.readings.remove(&t);
        }
        for (t, v) in readings {
            debug_assert_eq!(floor_to(*t, MINUTE), minute);
            self.readings.insert(*t, v.clone());
        }
        self.recompute_minute(minute);
        let q = floor_to(minute, QUARTER);
        self.recompute_quarter(q);
        *self.versions.entry(q).or_insert(0) += 1;
    }

    fn set(&mut self, level: usize, start: Nanos, node: Node) {
        if node.is_empty() {
            self.levels[level].remove(&start);
        } else {
            self.levels[level].insert(start, node);
        }
    }

    fn recompute_minute(&mut self, m: Nanos) {
        let rs = self.raw(m, m + MINUTE);
        let n = direct(&self.params, m, m + MINUTE, &rs);
        self.set(0, m, n);
    }

    fn recompute_quarter(&mut self, q: Nanos) {
        let children: Vec<Node> = (0..QUARTER / MINUTE)
            .map(|i| self.node(0, q + i * MINUTE))
            .collect();
        let n = merge_all(&self.params, q, children.iter());
        self.set(1, q, n);
    }

    /// The summarised current readings in `[start, end)`.
    pub fn raw(&self, start: Nanos, end: Nanos) -> Vec<(Nanos, Value)> {
        if start >= end {
            return Vec::new();
        }
        self.readings
            .range(start..end)
            .map(|(t, v)| (*t, v.clone()))
            .collect()
    }

    /// All summarised current readings.
    pub fn readings(&self) -> Vec<(Nanos, Value)> {
        self.readings.iter().map(|(t, v)| (*t, v.clone())).collect()
    }

    /// The node at `level` (index into [`LEVELS`]) starting at `start`.
    pub fn node(&self, level: usize, start: Nanos) -> Node {
        assert_eq!(
            start,
            floor_to(start, LEVELS[level]),
            "unaligned node start"
        );
        self.levels[level]
            .get(&start)
            .cloned()
            .unwrap_or_else(|| Node::empty(self.params.kind, start, start + LEVELS[level]))
    }

    /// All stored (non-empty) nodes at a level.
    pub fn stored(&self, level: usize) -> impl Iterator<Item = &Node> {
        self.levels[level].values()
    }

    /// Modification counter of a 15-minute node (for window caches).
    pub fn version(&self, quarter: Nanos) -> u64 {
        self.versions.get(&quarter).copied().unwrap_or(0)
    }

    /// Marks a minute `summary_stale` (§5.6) or clears the mark.
    pub fn set_stale(&mut self, minute: Nanos, stale: bool) {
        if stale {
            self.stale_minutes.insert(minute);
        } else {
            self.stale_minutes.remove(&minute);
        }
    }

    /// Whether any minute under the node is flagged `summary_stale`.
    pub fn is_stale(&self, level: usize, start: Nanos) -> bool {
        self.stale_minutes
            .range(start..start + LEVELS[level])
            .next()
            .is_some()
    }

    /// Equality of the stored summaries, ignoring versions and flags.
    pub fn same_summaries(&self, other: &SeriesTree) -> bool {
        self.params == other.params && self.levels == other.levels
    }

    /// The last summarised reading strictly before `t`.
    pub fn last_before(&self, t: Nanos) -> Option<(Nanos, Value)> {
        self.readings
            .range(..t)
            .next_back()
            .map(|(t, v)| (*t, v.clone()))
    }

    /// The merge over `[start, end)` from whole stored nodes in the middle and
    /// raw readings for partial minutes at both edges, without edge stretches.
    pub fn core(&self, start: Nanos, end: Nanos) -> Node {
        let mut parts: Vec<Node> = Vec::new();
        let mut t = start;
        while t < end {
            if t % MINUTE != 0 || t + MINUTE > end {
                let to = (floor_to(t, MINUTE) + MINUTE).min(end);
                parts.push(direct(&self.params, t, to, &self.raw(t, to)));
                t = to;
            } else if t % QUARTER == 0 && t + QUARTER <= end {
                parts.push(self.node(1, t));
                t += QUARTER;
            } else {
                parts.push(self.node(0, t));
                t += MINUTE;
            }
        }
        let mut n = merge_all(&self.params, start, parts.iter());
        n.end = end.max(start);
        n
    }

    /// Evaluates the window `[start, end)` at time `now` (§6): readings after
    /// `now` are ignored and no time is credited past `now`.
    pub fn window(&self, start: Nanos, end: Nanos, now: Nanos) -> Node {
        let until = end.min(now).max(start);
        let core = self.core(start, until);
        let mut n = with_stretches(&self.params, core, self.last_before(start).as_ref(), until);
        n.end = end;
        n
    }

    /// The stored calendar node for `date` in `zone`: the merge of its
    /// 15-minute nodes (§5.7). It holds no edge stretches; the specification
    /// makes the calendar result a window evaluation (see `OPEN-QUESTIONS.md`).
    pub fn day(&self, zone: &TimeZone, date: Date) -> Result<Node, TreeError> {
        let (start, end) = day_span(zone, date)?;
        let quarters: Vec<Node> = (0..(end - start) / QUARTER)
            .map(|i| self.node(1, start + i * QUARTER))
            .collect();
        let mut n = merge_all(&self.params, start, quarters.iter());
        n.end = end;
        Ok(n)
    }
}

/// UTC span `[start, end)` of local `date` in `zone`. A day whose midnight
/// does not exist starts at its first instant; a skipped day is empty.
pub fn day_span(zone: &TimeZone, date: Date) -> Result<(Nanos, Nanos), TreeError> {
    let at = |d: Date| -> Result<Nanos, TreeError> {
        let z = d
            .to_zoned(zone.clone())
            .map_err(|e| TreeError::Calendar(e.to_string()))?;
        Ok(i64::try_from(z.timestamp().as_nanosecond()).expect("in range"))
    };
    let start = at(date)?;
    let next = date
        .tomorrow()
        .map_err(|e| TreeError::Calendar(e.to_string()))?;
    let end = at(next)?;
    for b in [start, end] {
        let off = b - floor_to(b, QUARTER);
        if off != 0 {
            return Err(TreeError::UnalignedDay {
                date,
                zone: zone.iana_name().unwrap_or("?").to_string(),
                offset_ns: off,
            });
        }
    }
    Ok((start, end.max(start)))
}

/// Number of 15-minute nodes in local `date`.
pub fn quarters_in_day(zone: &TimeZone, date: Date) -> Result<i64, TreeError> {
    let (s, e) = day_span(zone, date)?;
    Ok((e - s) / QUARTER)
}

/// A rolling window that caches its merged middle of whole 15-minute nodes,
/// invalidated by node versions (§6 Cache).
#[derive(Clone, Debug)]
pub struct SlidingWindow {
    /// Window length.
    pub len: Nanos,
    cache: Option<(Nanos, Nanos, Vec<u64>, Node)>,
    /// Evaluations that reused the cached middle.
    pub hits: u64,
    /// Evaluations that recomputed the middle.
    pub misses: u64,
}

impl SlidingWindow {
    /// A window of `len`.
    pub fn new(len: Nanos) -> Self {
        assert!(len > 0);
        SlidingWindow {
            len,
            cache: None,
            hits: 0,
            misses: 0,
        }
    }

    /// Evaluates the window `[now − len, now)`.
    pub fn evaluate(&mut self, tree: &SeriesTree, now: Nanos) -> Node {
        let (start, end) = (now - self.len, now);
        let q_lo = floor_to(start + QUARTER - 1, QUARTER);
        let q_hi = floor_to(end, QUARTER);
        if q_lo >= q_hi {
            return tree.window(start, end, now);
        }
        let versions: Vec<u64> = (0..(q_hi - q_lo) / QUARTER)
            .map(|i| tree.version(q_lo + i * QUARTER))
            .collect();
        let middle = match &self.cache {
            Some((lo, hi, v, n)) if *lo == q_lo && *hi == q_hi && *v == versions => {
                self.hits += 1;
                n.clone()
            }
            _ => {
                self.misses += 1;
                let n = tree.core(q_lo, q_hi);
                self.cache = Some((q_lo, q_hi, versions, n.clone()));
                n
            }
        };
        let p = &tree.params;
        let merged = merge(
            p,
            &merge(p, &tree.core(start, q_lo), &middle),
            &tree.core(q_hi, end),
        );
        with_stretches(p, merged, tree.last_before(start).as_ref(), end)
    }
}
