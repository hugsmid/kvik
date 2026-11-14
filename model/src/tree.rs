//! The per-series summary tree (spec §5.1, §5.6) and window evaluation (§6);
//! calendar days and months are in [`crate::calendar`].
//!
//! Stored levels are 1 minute and 15 minutes, aligned to UTC; the 1-second
//! level is never stored. A 1-minute node is the fold over its readings; a
//! 15-minute node is the merge of its minutes. The tree keeps the series'
//! summarised current readings, which stand in for the raw facts a window edge
//! is computed from.

use std::collections::{BTreeMap, BTreeSet};

use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::registry::Timing;
use crate::summary::{Node, Params, credit_intervals, direct, merge, merge_all, with_stretches};
use crate::types::{MINUTE, Nanos, QUARTER, Value, floor_to};

/// Node widths of the stored levels: index 0 is 1 minute, 1 is 15 minutes.
pub const LEVELS: [Nanos; 2] = [MINUTE, QUARTER];

/// Errors from calendar evaluation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// A local day boundary does not fall on a 15-minute UTC boundary, so no
    /// calendar node is stored for it and it has no whole number of
    /// quarter-hours; its result is still a window evaluation (§5.7).
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
    /// The series stores no 15-minute level, so no calendar node (§5.1, §5.7):
    /// its calendar results are window evaluations over its facts.
    #[error("no calendar node: the series stores no 15-minute level")]
    NoCalendarNode,
    /// The zone is not in the time-zone database.
    #[error("unknown time zone {0}")]
    UnknownZone(String),
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
        if node.is_empty() || !self.params.stores(level) {
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

    /// The node at `level` (index into [`LEVELS`]) starting at `start`: the
    /// stored node, or for a level the series does not store (§5.1 density
    /// rule), the node computed from its readings (a 15-minute node as the
    /// merge of its minutes).
    pub fn node(&self, level: usize, start: Nanos) -> Node {
        assert_eq!(
            start,
            floor_to(start, LEVELS[level]),
            "unaligned node start"
        );
        let width = LEVELS[level];
        if !self.params.stores(level) {
            return match level {
                0 => direct(
                    &self.params,
                    start,
                    start + width,
                    &self.raw(start, start + width),
                ),
                _ => {
                    let minutes: Vec<Node> = (0..width / MINUTE)
                        .map(|i| self.node(0, start + i * MINUTE))
                        .collect();
                    let mut n = merge_all(&self.params, start, minutes.iter());
                    n.start = start;
                    n.end = start + width;
                    n
                }
            };
        }
        self.levels[level]
            .get(&start)
            .cloned()
            .unwrap_or_else(|| Node::empty(self.params.kind, start, start + width))
    }

    /// Whether the series stores the level (§5.1 density rule).
    pub fn stores(&self, level: usize) -> bool {
        self.params.stores(level)
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

    /// The last summarised reading strictly before `t`. A tree loaded from a
    /// checkpoint holds nodes without their readings (§8 Restore), so the last
    /// reading of every whole node of the finest stored level before `t` is
    /// also taken from its node.
    pub fn last_before(&self, t: Nanos) -> Option<(Nanos, Value)> {
        let reading = self
            .readings
            .range(..t)
            .next_back()
            .map(|(t, v)| (*t, v.clone()));
        let node = (0..LEVELS.len())
            .find(|l| self.params.stores(*l))
            .and_then(|l| {
                self.levels[l]
                    .range(..floor_to(t, LEVELS[l]))
                    .next_back()
                    .and_then(|(_, n)| n.last.clone())
            });
        match (reading, node) {
            (Some(r), Some(n)) => Some(if n.0 > r.0 { n } else { r }),
            (r, n) => r.or(n),
        }
    }

    /// The stored nodes alone, without readings, versions or flags: what a
    /// tree checkpoint carries (§5.1, §8 Continuous backup).
    pub fn nodes_only(&self) -> SeriesTree {
        SeriesTree {
            params: self.params,
            readings: BTreeMap::new(),
            levels: self.levels.clone(),
            versions: BTreeMap::new(),
            stale_minutes: BTreeSet::new(),
        }
    }

    /// Whether any minute overlapping `[start, end)` is flagged
    /// `summary_stale`.
    pub fn any_stale(&self, start: Nanos, end: Nanos) -> bool {
        self.stale_minutes
            .range(floor_to(start, MINUTE)..end.max(start))
            .next()
            .is_some()
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
    /// the credit limit (`now` for an observed series, the window end for a
    /// scheduled or forecast one) are ignored and no time is credited past it;
    /// a window wholly after `now` on an observed series is empty.
    pub fn window(&self, start: Nanos, end: Nanos, now: Nanos) -> Node {
        self.window_with(start, end, now, &[])
    }

    /// [`SeriesTree::window`] with `inputs`, nodes over sub-spans of the
    /// window (stored calendar nodes, §5.7), taken whole in place of the nodes
    /// and readings they cover. Inputs must be in order, disjoint, and lie
    /// inside the window before its credit limit; an interval series needs
    /// them on edges of its finest stored level. With exact sums the result
    /// is the same with or without them.
    pub fn window_with(&self, start: Nanos, end: Nanos, now: Nanos, inputs: &[Node]) -> Node {
        if self.params.interval.is_some() {
            return self.interval_window(start, end, now, inputs);
        }
        if self.params.wholly_future(start, now) {
            return Node::empty(self.params.kind, start, end);
        }
        let until = self.params.credit_limit(end, now).max(start);
        let mut parts = Vec::new();
        let mut t = start;
        for inp in inputs {
            assert!(
                t <= inp.start && inp.end <= until,
                "input outside the window"
            );
            parts.push(self.core(t, inp.start));
            parts.push(inp.clone());
            t = inp.end;
        }
        parts.push(self.core(t, until));
        let mut core = merge_all(&self.params, start, parts.iter());
        core.start = start;
        core.end = until;
        let mut n = with_stretches(&self.params, core, self.last_before(start).as_ref(), until);
        n.end = end;
        n
    }

    /// A window over an interval series (§6 Aggregates): whole nodes of the
    /// finest stored level in the middle (no interval crosses their edges,
    /// §5.4), and at each edge the readings whose intervals reach into it,
    /// apportioned; edge lookups reach back d. A reading of an observed
    /// series enters only once `t + d ≤ now`.
    fn interval_window(&self, start: Nanos, end: Nanos, now: Nanos, inputs: &[Node]) -> Node {
        let p = &self.params;
        let d = p.interval.expect("interval series").length;
        let end = end.max(start);
        let observed = p.timing == Timing::Observed;
        let ready = |t: Nanos| !observed || t.saturating_add(d) <= now;
        let reaching = |lo: Nanos, hi: Nanos| -> Vec<(Nanos, Value)> {
            self.readings
                .range(lo..hi.max(lo))
                .filter(|(t, _)| ready(**t))
                .map(|(t, v)| (*t, v.clone()))
                .collect()
        };
        let middle = (0..LEVELS.len()).find(|l| p.stores(*l)).and_then(|l| {
            let w = LEVELS[l];
            let a = floor_to(start + w - 1, w);
            // For an observed series only nodes ending by `now`, whose
            // readings are all ready.
            let b = floor_to(if observed { end.min(now) } else { end }, w);
            (a < b).then(|| {
                let mut nodes: Vec<Node> = Vec::new();
                let mut t = a;
                for inp in inputs.iter().filter(|i| a <= i.start && i.end <= b) {
                    while t < inp.start {
                        nodes.push(self.node(l, t));
                        t += w;
                    }
                    assert_eq!(t, inp.start, "input off the stored level's edges");
                    nodes.push(inp.clone());
                    t = inp.end;
                }
                while t < b {
                    nodes.push(self.node(l, t));
                    t += w;
                }
                let mut n = merge_all(p, a, nodes.iter());
                n.start = a;
                n.end = b;
                n
            })
        });
        let edge = |lo: Nanos, hi: Nanos, from: Nanos| {
            let mut n = Node::empty(p.kind, lo, hi);
            credit_intervals(p, &mut n, reaching(from, hi).iter(), start, end);
            n
        };
        let lookback = start.saturating_sub(d - 1);
        match middle {
            None => edge(start, end, lookback),
            Some(m) => {
                let left = edge(start, m.start, lookback);
                let right = edge(m.end, end, m.end);
                merge(p, &merge(p, &left, &m), &right)
            }
        }
    }

    /// The calendar node for `date` taken in `zone`: the merge of its
    /// 15-minute nodes (§5.7), what a stored day node holds. It holds no edge
    /// stretches and is only an input to the day's result, which is a window
    /// evaluation ([`crate::calendar`]). Refused for a series without a stored
    /// 15-minute level and for a day whose edges are off 15-minute boundaries.
    pub fn day(&self, zone: &TimeZone, date: Date) -> Result<Node, TreeError> {
        if !self.params.stores(1) {
            return Err(TreeError::NoCalendarNode);
        }
        let (start, end) = day_span(zone, date)?;
        unaligned(zone, date, start, end)?;
        let quarters: Vec<Node> = (0..(end - start) / QUARTER)
            .map(|i| self.node(1, start + i * QUARTER))
            .collect();
        let mut n = merge_all(&self.params, start, quarters.iter());
        n.end = end;
        Ok(n)
    }
}

/// UTC span `[start, end)` of local `date` in `zone`, wherever its edges fall
/// (§5.7). A day whose midnight does not exist starts at its first instant; a
/// skipped day is empty.
pub fn day_span(zone: &TimeZone, date: Date) -> Result<(Nanos, Nanos), TreeError> {
    crate::calendar::day_span(zone, date)
}

fn unaligned(zone: &TimeZone, date: Date, start: Nanos, end: Nanos) -> Result<(), TreeError> {
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
    Ok(())
}

/// Number of 15-minute nodes in local `date`.
/// Refused for a day whose edges are off 15-minute boundaries.
pub fn quarters_in_day(zone: &TimeZone, date: Date) -> Result<i64, TreeError> {
    let (s, e) = day_span(zone, date)?;
    unaligned(zone, date, s, e)?;
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
        if tree.params.interval.is_some() {
            return tree.window(start, end, now);
        }
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
