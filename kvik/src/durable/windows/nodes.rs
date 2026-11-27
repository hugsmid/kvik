//! Chart buckets from the summary tree (design 7.1b section 6 Charts): each bucket's core is the merge of the stored
//! nodes covering its whole quarter-hours, with facts for its edges, for quarter-hours dirty in the stretch, stale,
//! uncovered or not yet past the credit limit, then evaluated as [`super::stream::Core`] evaluates the fact path's.
//! Merge is associative and agrees with the fold (spec §5.3), so the buckets equal the fact path's (invariant T7).

use std::collections::BTreeMap;

use km::registry::Timing;
use km::summary::Params;
use kvik_model as km;
use kvik_tree as kt;
use kvik_tree::set::{Key, QUARTER, Seen, Tile, cover, covered, served};

use super::super::read::source::{Plan, each_row, search_back_each};
use super::super::read::view::View;
use super::super::reader::DurableReader;
use super::super::reader::facts::plan;
use super::super::registry::Series;
use super::super::tree::read::TreeAt;
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
struct Chart<'a> {
    r: &'a DurableReader,
    v: &'a View,
    tree: &'a TreeAt,
    series: &'a Series,
    id: u32,
    p: Params,
    np: kt::Params,
    now: i64,
    vis: BTreeMap<Key, Seen>,
    plan: Plan,
}

/// The chart of `series` over `[a, b)` at `step` from the tree, or `None` when the tree cannot serve it: a step off
/// the quarter-hours, a series that stores no 15-minute level, is not served or was changed in the stretch, or an
/// automatic resolution that may give raw facts.
#[allow(clippy::too_many_arguments)]
pub(super) fn chart(
    r: &DurableReader,
    v: &View,
    tree: &TreeAt,
    series: &Series,
    (a, b, step): (i64, i64, i64),
    max: Option<usize>,
    now: i64,
) -> Result<Option<ChartData>, QueryError> {
    let id = series.def.id;
    let p = params(&series.def);
    if step % QUARTER != 0 || !p.stores(1) || !served(&tree.root, u64::from(id)) || tree.changed(id)
    {
        return Ok(None);
    }
    let Some(vis) = tree.visible(id) else {
        return Ok(None);
    };
    let c = Chart {
        r,
        v,
        tree,
        series,
        id,
        np: fold::node_params(&p),
        p,
        now,
        vis,
        plan: plan(r, v, &[SeriesId(id)], ReadView::Current)?,
    };
    if let Some(max) = max
        && c.readings_at_most(ceil_q(a), floor_q(b), max)
    {
        return Ok(None);
    }
    let names = &series.def.state_names;
    let mut last = if needs_last(&c.p, a, now) {
        c.prior(a)?
    } else {
        None
    };
    let mut buckets = Vec::new();
    for (lo, hi) in rules::bucket_spans(a, b, step) {
        let node = if c.p.interval.is_some() {
            let limit = match c.p.timing {
                // A reading is credited once its interval has ended; every one of a quarter-hour has by its end.
                Timing::Observed => floor_q(now),
                Timing::Scheduled | Timing::Forecast => i64::MAX,
            };
            c.core(lo, hi, limit)?
        } else if c.p.wholly_future(lo, now) {
            kt::Node::empty(c.np.kind, lo, hi)
        } else {
            let until = c.p.credit_limit(hi, now).max(lo);
            let core = c.core(lo, until, until)?;
            let before = last.clone();
            if let Some(l) = &core.last {
                last = Some(l.clone());
            }
            let mut n = kt::with_stretches(&c.np, core, before.as_ref(), until);
            n.end = hi;
            n
        };
        let node = fold::model_node(node, names);
        buckets.push(rules::bucket(&series.spec, &node, lo, hi, false));
    }
    Ok(Some(ChartData::Buckets {
        step: convert::dur(i128::from(step)),
        buckets,
    }))
}

impl Chart<'_> {
    /// Whether the clean nodes of `[from, to)` hold at most `max` readings, so that the facts may be fewer than an
    /// automatic resolution's most points; a fact gives at most one reading, so more readings mean more facts.
    fn readings_at_most(&self, from: i64, to: i64, max: usize) -> bool {
        let mut n: u64 = 0;
        for ((_, _, start), seen) in self
            .vis
            .range((u64::from(self.id), LEVEL, from)..(u64::from(self.id), LEVEL, to))
        {
            if start.saturating_add(QUARTER) > to
                || seen.stale
                || self.tree.dirty(self.id, *start, start + QUARTER)
            {
                continue;
            }
            n = n.saturating_add(seen.node.as_ref().map_or(0, |x| x.count));
            if n > max as u64 {
                return false;
            }
        }
        true
    }

    /// The core of `[lo, end)`: nodes for the whole quarter-hours ending at or before `limit`, facts elsewhere.
    fn core(&self, lo: i64, end: i64, limit: i64) -> Result<kt::Node, QueryError> {
        let (qlo, x) = (ceil_q(lo), floor_q(end.min(limit)));
        let mut pieces = Vec::new();
        if qlo >= x {
            push_facts(&mut pieces, lo, end);
        } else {
            push_facts(&mut pieces, lo, qlo);
            self.tiles(&mut pieces, qlo, x);
            push_facts(&mut pieces, x, end);
        }
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
                    folded = self.folded(s, e)?;
                    &folded
                }
            };
            core = Some(match core {
                Some(c) => kt::merge(&self.np, &c, n),
                None => n.clone(),
            });
        }
        Ok(core.unwrap_or_else(|| kt::Node::empty(self.np.kind, lo, end)))
    }

    fn tiles<'a>(&'a self, out: &mut Vec<Piece<'a>>, from: i64, to: i64) {
        let root = &self.tree.root;
        for tile in cover(root, &self.vis, u64::from(self.id), from, to) {
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
                        out.push(Piece::Stored(
                            seen.and_then(|s| s.node.as_ref()),
                            start,
                            end,
                        ));
                    }
                }
                Tile::Facts { start, end } => push_facts(out, start, end),
            }
        }
    }

    fn quarter<'a>(&'a self, out: &mut Vec<Piece<'a>>, q: i64) {
        let seen = self.vis.get(&(u64::from(self.id), LEVEL, q));
        if !covered(&self.tree.root, LEVEL, q, q + QUARTER)
            || seen.is_some_and(|s| s.stale)
            || self.tree.dirty(self.id, q, q + QUARTER)
        {
            push_facts(out, q, q + QUARTER);
        } else {
            out.push(Piece::Stored(
                seen.and_then(|s| s.node.as_ref()),
                q,
                q + QUARTER,
            ));
        }
    }

    /// The current readings of `[from, to)` in `t` order.
    fn readings(&self, from: i64, to: i64) -> Result<Vec<Reading>, QueryError> {
        let mut out = Vec::new();
        each_row(self.r, self.v, &self.plan, from, to - 1, |s| {
            out.extend(reading_of(self.series, &s));
        })?;
        Ok(out)
    }

    /// `[s, e)` folded from facts as the fact path folds it: a fold of the point readings in it, or the credit of the
    /// interval readings that reach it and are ready.
    fn folded(&self, s: i64, e: i64) -> Result<kt::Node, QueryError> {
        if self.p.interval.is_none() {
            return Ok(kt::fold(&self.np, s, e, &self.readings(s, e)?));
        }
        let ready = |t: i64| self.p.timing != Timing::Observed || self.p.ready_at(t) <= self.now;
        let rs: Vec<Reading> = self
            .readings(fold::reach(&self.p, s), e)?
            .into_iter()
            .filter(|(t, _)| ready(*t))
            .collect();
        let mut n = kt::Node::empty(self.np.kind, s, e);
        kt::credit_intervals(&self.np, &mut n, &rs, s, e);
        Ok(n)
    }

    /// The last reading before `a`: from the facts of its quarter-hour before it, then the nearest stored node before
    /// that when every quarter-hour between is covered and clean; else the fact path's search back.
    fn prior(&self, a: i64) -> Result<Option<Reading>, QueryError> {
        let qa = floor_q(a);
        if qa < a
            && let Some(r) = self.readings(qa, a)?.pop()
        {
            return Ok(Some(r));
        }
        if let Some(found) = self.prior_node(qa) {
            return Ok(found);
        }
        let states = self.series.def.state_names.len();
        let found = search_back_each(self.r, self.v, &self.plan, a - 1, &|f| readable(states, f))?;
        Ok(found
            .into_iter()
            .next()
            .flatten()
            .and_then(|f| reading(states, f)))
    }

    /// `Some` with the last reading before `qa` when the tree alone tells it.
    fn prior_node(&self, qa: i64) -> Option<Option<Reading>> {
        let s = u64::from(self.id);
        let root = &self.tree.root;
        let found = self
            .vis
            .range((s, LEVEL, i64::MIN)..(s, LEVEL, qa))
            .next_back();
        let from = match found {
            Some(((_, _, start), _)) => *start,
            None => {
                // No node before: no reading, if the covered range reaches back to the first quarter-hour.
                let c = root
                    .covered
                    .iter()
                    .find(|c| c.level == LEVEL && c.from < qa && qa <= c.to)?;
                (c.from <= ceil_q(i64::MIN)).then_some(c.from)?
            }
        };
        if !covered(root, LEVEL, from, qa) || self.tree.dirty(self.id, from, qa) {
            return None;
        }
        match found {
            Some((_, seen)) if seen.stale => None,
            Some((_, seen)) => Some(seen.node.as_ref().and_then(|n| n.last.clone())),
            None => Some(None),
        }
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
