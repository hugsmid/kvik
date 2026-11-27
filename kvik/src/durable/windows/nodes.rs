//! Chart buckets from the summary tree (design 7.1b section 6 Charts): each bucket's core is the merge of the stored
//! nodes covering its whole quarter-hours, with facts for its edges, for quarter-hours dirty in the stretch, stale,
//! uncovered or not yet past the credit limit, then evaluated as [`super::stream::Core`] evaluates the fact path's.
//! Merge is associative and agrees with the fold (spec §5.3), so the buckets equal the fact path's (invariant T7).

use std::collections::BTreeMap;

use km::registry::Timing;
use km::summary::Params;
use kvik_model as km;
use kvik_tree as kt;
use kvik_tree::part::{Entry, Part};
use kvik_tree::root::Root;
use kvik_tree::set::{QUARTER, Tile, cover_by, covered, served, visible_entries};

use super::super::read::source::{each_row, search_back_each};
use super::super::read::view::View;
use super::super::reader::DurableReader;
use super::super::reader::facts::plan;
use super::super::registry::Series;
use super::super::tree::read::{TreeAt, Window};
use super::stream::{needs_last, readable, reading};
use super::{params, reading_of};
use crate::backend::convert;
use crate::error::QueryError;
use crate::fold::{self, Reading};
use crate::read::{ChartData, View as ReadView};
use crate::rules;
use crate::types::SeriesId;

/// The level the tree's cover rests on: 15 minutes (design section 2).
const LEVEL: u8 = 2;

fn floor_q(t: i64) -> i64 {
    t - t.rem_euclid(QUARTER)
}

fn ceil_q(t: i64) -> i64 {
    match t.rem_euclid(QUARTER) {
        0 => t,
        r => t.saturating_add(QUARTER - r),
    }
}

#[cfg(test)]
thread_local! {
    /// Stored nodes this thread's chart reads have used.
    pub(crate) static NODES_USED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

enum Piece<'a> {
    /// A covered span: its stored node, `None` for an empty one.
    Stored(Option<&'a kt::Node>, i64, i64),
    Facts(i64, i64),
}

/// One series' chart read from the tree.
struct Chart<'w> {
    tree: &'w TreeAt,
    root: &'w Root,
    series: &'w Series,
    id: u32,
    p: Params,
    np: kt::Params,
    now: i64,
    vis: BTreeMap<(u8, i64), &'w Entry>,
    /// The current readings of the spans fetched for it, in `t` order.
    fetched: Vec<Reading>,
}

/// A chart to evaluate: its place among the series asked for, and each bucket's evaluation and pieces.
type Planned<'w> = (usize, Chart<'w>, Vec<(Eval, Vec<Piece<'w>>)>);

/// How a bucket is evaluated: as an interval series' credit, wholly ahead of `now`, or up to its credit limit.
enum Eval {
    Interval,
    Future,
    Point(i64),
}

/// The charts of `series` over `[a, b)` at `step` from the tree; `None` for a series the tree cannot serve: a step off
/// the quarter-hours, a series that stores no 15-minute level, is not served or was changed in the stretch, or an
/// automatic resolution that may give raw facts. The facts every chart needs are read once for them all.
#[allow(clippy::too_many_arguments)]
pub(super) fn charts(
    r: &DurableReader,
    v: &View,
    tree: &TreeAt,
    w: &Window,
    series: &[&Series],
    (a, b, step): (i64, i64, i64),
    max: Option<usize>,
    now: i64,
) -> Result<Vec<Option<ChartData>>, QueryError> {
    let mut out: Vec<Option<ChartData>> = vec![None; series.len()];
    if step % QUARTER != 0 {
        return Ok(out);
    }
    let live: Vec<&Part> = w.live.iter().map(|p| &**p).collect();
    let spans: Vec<(i64, i64)> = rules::bucket_spans(a, b, step).collect();
    let mut planned: Vec<Planned<'_>> = Vec::new();
    let mut wanted: Vec<(i64, i64)> = Vec::new();
    for (k, s) in series.iter().enumerate() {
        let id = s.def.id;
        let p = params(&s.def);
        if !p.stores(1) || !served(&w.root, u64::from(id)) || tree.changed(id) {
            continue;
        }
        let Ok(vis) = visible_entries(&w.root, &live, u64::from(id)) else {
            continue;
        };
        let c = Chart {
            tree,
            root: &w.root,
            series: s,
            id,
            np: fold::node_params(&p),
            p,
            now,
            vis,
            fetched: Vec::new(),
        };
        if let Some(max) = max
            && c.readings_at_most(ceil_q(a), floor_q(b), max)
        {
            continue;
        }
        let limit = match c.p.timing {
            // A reading is credited once its interval has ended; every one of a quarter-hour has by its end.
            Timing::Observed => floor_q(now),
            Timing::Scheduled | Timing::Forecast => i64::MAX,
        };
        let mut buckets = Vec::with_capacity(spans.len());
        for &(lo, hi) in &spans {
            let (eval, pieces) = if c.p.interval.is_some() {
                (Eval::Interval, c.pieces(lo, hi, limit))
            } else if c.p.wholly_future(lo, now) {
                (Eval::Future, Vec::new())
            } else {
                let until = c.p.credit_limit(hi, now).max(lo);
                (Eval::Point(until), c.pieces(lo, until, until))
            };
            for piece in &pieces {
                if let Piece::Facts(s, e) = piece {
                    wanted.push((c.reach(*s), *e));
                }
            }
            buckets.push((eval, pieces));
        }
        if needs_last(&c.p, a, now) && floor_q(a) < a {
            wanted.push((floor_q(a), a));
        }
        planned.push((k, c, buckets));
    }
    fetch(r, v, &mut planned, wanted)?;
    for (k, c, buckets) in planned {
        let mut last = if needs_last(&c.p, a, now) {
            c.prior(r, v, a)?
        } else {
            None
        };
        let mut out_buckets = Vec::with_capacity(spans.len());
        for (&(lo, hi), (eval, pieces)) in spans.iter().zip(buckets) {
            let node = match eval {
                Eval::Interval => c.core(lo, hi, pieces),
                Eval::Future => kt::Node::empty(c.np.kind, lo, hi),
                Eval::Point(until) => {
                    let core = c.core(lo, until, pieces);
                    let before = last.clone();
                    if let Some(l) = &core.last {
                        last = Some(l.clone());
                    }
                    let mut n = kt::with_stretches(&c.np, core, before.as_ref(), until);
                    n.end = hi;
                    n
                }
            };
            let node = fold::model_node(node, &c.series.def.state_names);
            out_buckets.push(rules::bucket(&c.series.spec, &node, lo, hi, false));
        }
        out[k] = Some(ChartData::Buckets {
            step: convert::dur(i128::from(step)),
            buckets: out_buckets,
        });
    }
    Ok(out)
}

/// Reads the spans `wanted` once for every chart, joined where they touch, and gives each chart its readings there.
fn fetch(
    r: &DurableReader,
    v: &View,
    planned: &mut [Planned<'_>],
    mut wanted: Vec<(i64, i64)>,
) -> Result<(), QueryError> {
    if planned.is_empty() {
        return Ok(());
    }
    wanted.sort_unstable();
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for (s, e) in wanted {
        match ranges.last_mut() {
            Some(l) if s <= l.1 => l.1 = l.1.max(e),
            _ => ranges.push((s, e)),
        }
    }
    let mut ids: Vec<(u32, usize)> = planned
        .iter()
        .enumerate()
        .map(|(i, (_, c, _))| (c.id, i))
        .collect();
    ids.sort_unstable();
    let plan = plan(
        r,
        v,
        &ids.iter().map(|(id, _)| SeriesId(*id)).collect::<Vec<_>>(),
        ReadView::Current,
    )?;
    for (s, e) in ranges {
        each_row(r, v, &plan, s, e - 1, |row| {
            if let Ok(k) = ids.binary_search_by_key(&row.row.series_id, |(id, _)| *id) {
                let c = &mut planned[ids[k].1].1;
                c.fetched.extend(reading_of(c.series, &row));
            }
        })?;
    }
    Ok(())
}

impl<'w> Chart<'w> {
    /// Whether the nodes of the whole quarter-hours `[from, to)` hold at most `max` readings, so that the facts may be
    /// fewer than an automatic resolution's most points. A node's count stays a lower bound on its span's facts at any
    /// later cut, dirty or stale included: a fact gives at most one reading, and a `t` once held keeps a fact.
    fn readings_at_most(&self, from: i64, to: i64, max: usize) -> bool {
        let mut n: u64 = 0;
        for seen in self.vis.range((LEVEL, from)..(LEVEL, to)).map(|(_, e)| e) {
            n = n.saturating_add(seen.node.as_ref().map_or(0, |x| x.count));
            if n > max as u64 {
                return false;
            }
        }
        true
    }

    /// The pieces of `[lo, end)`: nodes for the whole quarter-hours ending at or before `limit`, facts elsewhere.
    fn pieces(&self, lo: i64, end: i64, limit: i64) -> Vec<Piece<'w>> {
        let (qlo, x) = (ceil_q(lo), floor_q(end.min(limit)));
        let mut pieces = Vec::new();
        if qlo >= x {
            push_facts(&mut pieces, lo, end);
        } else {
            push_facts(&mut pieces, lo, qlo);
            self.tiles(&mut pieces, qlo, x);
            push_facts(&mut pieces, x, end);
        }
        pieces
    }

    /// Where the readings a facts piece `[s, …)` folds begin: an interval series' readings reach back into it.
    fn reach(&self, s: i64) -> i64 {
        match self.p.interval {
            Some(_) => fold::reach(&self.p, s),
            None => s,
        }
    }

    /// The core of `[lo, end)`, the merge of its pieces.
    fn core(&self, lo: i64, end: i64, pieces: Vec<Piece<'w>>) -> kt::Node {
        let mut core: Option<kt::Node> = None;
        for piece in pieces {
            let folded;
            let n = match piece {
                Piece::Stored(Some(n), _, _) => {
                    #[cfg(test)]
                    NODES_USED.with(|c| c.set(c.get() + 1));
                    n
                }
                Piece::Stored(None, s, e) => {
                    folded = kt::Node::empty(self.np.kind, s, e);
                    &folded
                }
                Piece::Facts(s, e) => {
                    folded = self.folded(s, e);
                    &folded
                }
            };
            core = Some(match core {
                Some(c) => kt::merge(&self.np, &c, n),
                None => n.clone(),
            });
        }
        core.unwrap_or_else(|| kt::Node::empty(self.np.kind, lo, end))
    }

    fn tiles(&self, out: &mut Vec<Piece<'w>>, from: i64, to: i64) {
        let tiles = cover_by(self.root, u64::from(self.id), from, to, |level, start| {
            self.vis.get(&(level, start)).copied()
        });
        for tile in tiles {
            match tile {
                Tile::Node {
                    level,
                    start,
                    end,
                    seen,
                } => {
                    if seen.is_some_and(|s| s.stale) || self.tree.dirty(self.id, start, end) {
                        if level == LEVEL {
                            push_facts(out, start, end);
                        } else {
                            // A roll-up with a dirty or stale child: its clean quarter-hours still serve.
                            for q in (start..end).step_by(QUARTER as usize) {
                                self.quarter(out, q);
                            }
                        }
                    } else {
                        push_stored(out, seen.and_then(|s| s.node.as_ref()), start, end);
                    }
                }
                Tile::Facts { start, end } => push_facts(out, start, end),
            }
        }
    }

    fn quarter(&self, out: &mut Vec<Piece<'w>>, q: i64) {
        let seen = self.vis.get(&(LEVEL, q)).copied();
        if !covered(self.root, LEVEL, q, q + QUARTER)
            || seen.is_some_and(|s| s.stale)
            || self.tree.dirty(self.id, q, q + QUARTER)
        {
            push_facts(out, q, q + QUARTER);
        } else {
            push_stored(out, seen.and_then(|s| s.node.as_ref()), q, q + QUARTER);
        }
    }

    /// The fetched readings of `[from, to)`, in `t` order.
    fn readings(&self, from: i64, to: i64) -> &[Reading] {
        let lo = self.fetched.partition_point(|r| r.0 < from);
        let hi = self.fetched.partition_point(|r| r.0 < to);
        &self.fetched[lo..hi.max(lo)]
    }

    /// `[s, e)` folded from facts as the fact path folds it: a fold of the point readings in it, or the credit of the
    /// interval readings that reach it and are ready.
    fn folded(&self, s: i64, e: i64) -> kt::Node {
        if self.p.interval.is_none() {
            return kt::fold(&self.np, s, e, self.readings(s, e));
        }
        let ready = |t: i64| self.p.timing != Timing::Observed || self.p.ready_at(t) <= self.now;
        let rs = self
            .readings(self.reach(s), e)
            .iter()
            .filter(|(t, _)| ready(*t));
        let mut n = kt::Node::empty(self.np.kind, s, e);
        kt::credit_intervals(&self.np, &mut n, rs, s, e);
        n
    }

    /// The last reading before `a`: from the facts of its quarter-hour before it, then the nearest stored node before
    /// that when every quarter-hour between is covered and clean; else the fact path's search back.
    fn prior(&self, r: &DurableReader, v: &View, a: i64) -> Result<Option<Reading>, QueryError> {
        let qa = floor_q(a);
        if let Some(found) = self.readings(qa, a).last() {
            return Ok(Some(found.clone()));
        }
        if let Some(found) = self.prior_node(qa) {
            return Ok(found);
        }
        let states = self.series.def.state_names.len();
        let plan = plan(r, v, &[SeriesId(self.id)], ReadView::Current)?;
        let found = search_back_each(r, v, &plan, a - 1, &|f| readable(states, f))?;
        Ok(found
            .into_iter()
            .next()
            .flatten()
            .and_then(|f| reading(states, f)))
    }

    /// `Some` with the last reading before `qa` when the tree alone tells it: from the chart's window, else, when the
    /// window holds no node before `qa`, from every part before it.
    fn prior_node(&self, qa: i64) -> Option<Option<Reading>> {
        if self
            .vis
            .range((LEVEL, i64::MIN)..(LEVEL, qa))
            .next()
            .is_some()
        {
            return told(self.root, &self.vis, qa, |a, b| {
                self.tree.dirty(self.id, a, b)
            });
        }
        let w = self.tree.window(i64::MIN, qa);
        let live: Vec<&Part> = w.live.iter().map(|p| &**p).collect();
        let vis = visible_entries(&w.root, &live, u64::from(self.id)).ok()?;
        told(&w.root, &vis, qa, |a, b| self.tree.dirty(self.id, a, b))
    }
}

/// The last reading before `qa` from the nearest node before it, when every quarter-hour between is covered and
/// clean; with no node before, no reading when the covered range reaches back to the first quarter-hour.
fn told(
    root: &Root,
    vis: &BTreeMap<(u8, i64), &Entry>,
    qa: i64,
    dirty: impl Fn(i64, i64) -> bool,
) -> Option<Option<Reading>> {
    let found = vis.range((LEVEL, i64::MIN)..(LEVEL, qa)).next_back();
    let from = match found {
        Some(((_, start), _)) => *start,
        None => {
            let c = root
                .covered
                .iter()
                .find(|c| c.level == LEVEL && c.from < qa && qa <= c.to)?;
            (c.from <= ceil_q(i64::MIN)).then_some(c.from)?
        }
    };
    if !covered(root, LEVEL, from, qa) || dirty(from, qa) {
        return None;
    }
    match found {
        Some((_, seen)) if seen.stale => None,
        Some((_, seen)) => Some(seen.node.as_ref().and_then(|n| n.last.clone())),
        None => Some(None),
    }
}

/// Pushes a stored piece; empty nodes side by side are one empty piece, as the empty node is the merge's identity.
fn push_stored<'w>(out: &mut Vec<Piece<'w>>, node: Option<&'w kt::Node>, s: i64, e: i64) {
    match (out.last_mut(), node) {
        (Some(Piece::Stored(None, _, end)), None) if *end == s => *end = e,
        _ => out.push(Piece::Stored(node, s, e)),
    }
}

fn push_facts(out: &mut Vec<Piece<'_>>, s: i64, e: i64) {
    if s >= e {
        return;
    }
    match out.last_mut() {
        Some(Piece::Facts(_, end)) if *end == s => *end = e,
        _ => out.push(Piece::Facts(s, e)),
    }
}
