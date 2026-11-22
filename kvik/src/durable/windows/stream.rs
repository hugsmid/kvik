//! Windows, chart buckets and calendar results folded from one series' current facts in the range, streamed at one
//! cut through the sources of roadmap 9.0f within the reader's memory cap; no fact index (roadmap 9.0h, Q-1109).

use std::collections::VecDeque;
use std::sync::Arc;

use km::registry::Timing;
use km::summary::Params;
use kvik_model as km;
use kvik_tree as kt;

use super::super::read::source::{FactStream, search_back};
use super::super::read::view::View;
use super::super::reader::DurableReader;
use super::super::reader::facts::plan;
use super::super::registry::Series;
use crate::error::QueryError;
use crate::fold::{self, Reading};
use crate::read::View as ReadView;
use crate::types::{Fact, Quality, SeriesId, Value};

/// Readings a window's core folds before it merges them in: `kvik_tree::merge` is associative and agrees with the
/// fold exactly (spec §5.3, invariant 6), so the core needs no more than this many at once.
pub(super) const CHUNK: usize = 256;

/// Whether a current fact gives a summarised reading, as [`super::has_reading`] judges a stored one: not of bad
/// supplied quality, and a value of a summarised type, a state index among the series' `states` declared names.
fn readable(states: usize, f: &Fact) -> bool {
    f.supplied_quality != Quality::Bad
        && match f.value {
            Value::F64(_) | Value::Bool(_) | Value::Text(_) => true,
            Value::State(i) => usize::from(i) < states,
            _ => false,
        }
}

/// A current fact as a summarised reading ([`super::reading_of`]).
pub(super) fn reading(states: usize, f: Fact) -> Option<Reading> {
    if !readable(states, &f) {
        return None;
    }
    let v = match f.value {
        Value::F64(x) => kt::Value::F64(x),
        Value::State(i) => kt::Value::State(i),
        Value::Bool(b) => kt::Value::Bool(b),
        Value::Text(t) => kt::Value::Text(t),
        _ => return None,
    };
    Some((f.t.0, v))
}

/// Runs `f` on a view taken after the call began and ends the call on it; once more under a new cut when a reset or
/// an aged cut ended the first (the query design's §3.2 rule 3, as `at` does).
pub(super) fn on_view<R>(
    r: &Arc<DurableReader>,
    f: impl Fn(&Arc<View>) -> Result<R, QueryError>,
) -> Result<R, QueryError> {
    let once = || {
        let v = r.snapshot()?;
        let out = f(&v);
        r.end(v)?;
        out
    };
    match once() {
        Err(QueryError::CutExpired) => once(),
        other => other,
    }
}

/// The current facts of `id` with `t` in `[from, to)` at `v`'s cut, streamed in `t` order.
pub(super) fn current(
    r: &Arc<DurableReader>,
    v: &Arc<View>,
    id: SeriesId,
    from: i64,
    to: i64,
) -> Result<Option<FactStream>, QueryError> {
    if from >= to {
        return Ok(None);
    }
    let plan = plan(r, v, &[id], ReadView::Current)?;
    Ok(Some(FactStream::new(
        r.clone(),
        v.clone(),
        plan,
        from,
        to - 1,
    )))
}

/// Windows over spans taken in ascending order, each starting where the last ended, folded from the readings of one
/// stream of the whole range. It holds a chunk of the core and, for an interval series, the readings whose intervals
/// reach past the span it last folded.
pub(super) struct Folder {
    p: Params,
    np: kt::Params,
    now: i64,
    states: usize,
    chunk: usize,
    facts: Box<dyn Iterator<Item = Result<Fact, QueryError>> + Send>,
    next: Option<Reading>,
    /// The last reading taken, else the last before the range: the next span's reading before its start.
    last: Option<Reading>,
    reaching: VecDeque<Reading>,
    /// The most readings held at once: a chunk, or an interval series' readings reaching past a span (Q-1109).
    held_most: usize,
}

impl Folder {
    /// A folder for the spans of `[from, to)` of `series` at `v`'s cut, evaluated at `now`.
    pub(super) fn new(
        r: &Arc<DurableReader>,
        v: &Arc<View>,
        series: &Series,
        p: Params,
        (from, to): (i64, i64),
        now: i64,
    ) -> Result<Self, QueryError> {
        let id = SeriesId(series.def.id);
        let states = series.def.state_names.len();
        // The reading before the range matters only to a span not wholly in the future, and the first span is the
        // earliest; an interval series credits its readings pro rata instead (spec §6).
        let last = if p.interval.is_none() && !p.wholly_future(from, now) && from > i64::MIN {
            let plan = plan(r, v, &[id], ReadView::Current)?;
            search_back(r, v, &plan, from - 1, &|f| readable(states, f))?
                .and_then(|f| reading(states, f))
        } else {
            None
        };
        let facts = current(r, v, id, fold::reach(&p, from), to)?;
        Ok(Folder::of(
            p,
            now,
            states,
            Box::new(facts.into_iter().flatten()),
            last,
        ))
    }

    /// A folder of `facts`, the current facts from the first span's reach in `t` order, with `last` the reading
    /// before the first span; its core folds `CHUNK` readings at a time.
    pub(super) fn of(
        p: Params,
        now: i64,
        states: usize,
        facts: Box<dyn Iterator<Item = Result<Fact, QueryError>> + Send>,
        last: Option<Reading>,
    ) -> Self {
        Folder {
            np: fold::node_params(&p),
            now,
            states,
            chunk: CHUNK,
            facts,
            next: None,
            last,
            reaching: VecDeque::new(),
            held_most: 0,
            p,
        }
    }

    #[cfg(test)]
    pub(super) fn held_most(&self) -> usize {
        self.held_most
    }

    #[cfg(test)]
    pub(super) fn chunked(mut self, chunk: usize) -> Self {
        self.chunk = chunk.max(1);
        self
    }

    /// The next reading with `t < to`, if the stream has one.
    fn take_below(&mut self, to: i64) -> Result<Option<Reading>, QueryError> {
        while self.next.is_none() {
            let Some(f) = self.facts.next() else {
                return Ok(None);
            };
            self.next = reading(self.states, f?);
        }
        if self.next.as_ref().is_some_and(|(t, _)| *t < to) {
            Ok(self.next.take())
        } else {
            Ok(None)
        }
    }

    fn skip_below(&mut self, to: i64) -> Result<(), QueryError> {
        while let Some(r) = self.take_below(to)? {
            self.last = Some(r);
        }
        Ok(())
    }

    /// The window `[start, end)`, as [`fold::window`] evaluates it from every reading of its reach.
    pub(super) fn node(&mut self, start: i64, end: i64) -> Result<kt::Node, QueryError> {
        if self.p.interval.is_some() {
            return self.interval(start, end);
        }
        let kind = self.np.kind;
        let before = self.last.clone();
        if self.p.wholly_future(start, self.now) {
            self.skip_below(end)?;
            return Ok(kt::Node::empty(kind, start, end));
        }
        let until = self.p.credit_limit(end, self.now).max(start);
        let mut core = kt::Node::empty(kind, start, start);
        let mut chunk = Vec::with_capacity(self.chunk.min(64));
        let mut from = start;
        while let Some(r) = self.take_below(until)? {
            chunk.push(r);
            self.held_most = self.held_most.max(chunk.len());
            if chunk.len() == self.chunk {
                let to = chunk[chunk.len() - 1].0 + 1;
                core = kt::merge(&self.np, &core, &kt::fold(&self.np, from, to, &chunk));
                from = to;
                self.last = chunk.pop();
                chunk.clear();
            }
        }
        core = kt::merge(&self.np, &core, &kt::fold(&self.np, from, until, &chunk));
        if let Some(r) = chunk.pop() {
            self.last = Some(r);
        }
        self.skip_below(end)?;
        let mut n = kt::with_stretches(&self.np, core, before.as_ref(), until);
        n.end = end;
        Ok(n)
    }

    /// The window `[start, end)` of an interval series, as [`fold::interval_window`] credits it: readings are
    /// credited as they are taken, and kept only while their interval reaches past `end`.
    fn interval(&mut self, start: i64, end: i64) -> Result<kt::Node, QueryError> {
        let (p, np, now) = (self.p, self.np, self.now);
        let ready = |t: i64| p.timing != Timing::Observed || p.ready_at(t) <= now;
        let from = fold::reach(&p, start);
        let keep = fold::reach(&p, end);
        let mut n = kt::Node::empty(np.kind, start, end);
        while self.reaching.front().is_some_and(|(t, _)| *t < from) {
            self.reaching.pop_front();
        }
        let held = self.reaching.iter().filter(|(t, _)| ready(*t));
        kt::credit_intervals(&np, &mut n, held, start, end);
        while let Some(r) = self.take_below(end)? {
            if r.0 >= from && ready(r.0) {
                kt::credit_intervals(&np, &mut n, std::iter::once(&r), start, end);
            }
            if r.0 >= keep {
                self.reaching.push_back(r);
                self.held_most = self.held_most.max(self.reaching.len());
            }
        }
        Ok(n)
    }
}
