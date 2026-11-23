//! Windows, chart buckets and calendar results folded from current facts in the range, streamed at one cut through
//! the sources of roadmap 9.0f within the reader's memory cap; no fact index (roadmap 9.0h, Q-1109).

use std::collections::VecDeque;
use std::sync::Arc;

use km::registry::Timing;
use km::summary::Params;
use kvik_model as km;
use kvik_tree as kt;

use super::super::read::view::View;
use super::super::reader::DurableReader;
use crate::error::QueryError;
use crate::fold::{self, Reading};
use crate::types::{Fact, Quality, Value};

/// Readings a window's core folds before it merges them in: `kvik_tree::merge` is associative and agrees with the
/// fold exactly (spec §5.3, invariant 6), so the core needs no more than this many at once.
pub(super) const CHUNK: usize = 256;

#[cfg(test)]
thread_local! {
    static CHUNK_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn chunk_size() -> usize {
    #[cfg(test)]
    if let Some(n) = CHUNK_OVERRIDE.with(std::cell::Cell::get) {
        return n;
    }
    CHUNK
}

/// Sets, for this thread until dropped, the readings every new [`Core`] folds at a time, so that tests cross chunk
/// boundaries with few readings.
#[cfg(test)]
pub(crate) struct ChunkOverride;

#[cfg(test)]
impl ChunkOverride {
    pub(crate) fn set(chunk: usize) -> Self {
        CHUNK_OVERRIDE.with(|c| c.set(Some(chunk.max(1))));
        ChunkOverride
    }
}

#[cfg(test)]
impl Drop for ChunkOverride {
    fn drop(&mut self) {
        CHUNK_OVERRIDE.with(|c| c.set(None));
    }
}

/// Whether a current fact gives a summarised reading, as [`super::has_reading`] judges a stored one: not of bad
/// supplied quality, and a value of a summarised type, a state index among the series' `states` declared names.
pub(super) fn readable(states: usize, f: &Fact) -> bool {
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

/// Windows over spans taken in ascending order, each starting where the last ended, from readings offered in `t`
/// order: a span is opened, takes the readings below its end and is closed. It holds a chunk of the core and, for an
/// interval series, the readings whose intervals reach past the span it last folded (Q-1109). [`Folder`] pulls its
/// readings from one series' stream; [`super::many`] pushes each series' readings from one stream of several.
pub(super) struct Core {
    p: Params,
    np: kt::Params,
    now: i64,
    chunk: usize,
    /// The last reading taken, else the last before the range: the next span's reading before its start.
    last: Option<Reading>,
    reaching: VecDeque<Reading>,
    /// The most readings held at once: a chunk, or an interval series' readings reaching past a span (Q-1109).
    held_most: usize,
    open: Option<Open>,
}

struct Open {
    start: i64,
    end: i64,
    mode: Mode,
}

enum Mode {
    /// Wholly ahead of `now`: readings only move `last`.
    Future,
    Point {
        before: Option<Reading>,
        until: i64,
        /// The chunks folded so far; `None` before the first, as merging into an empty node only costs.
        core: Option<kt::Node>,
        chunk: Vec<Reading>,
        from: i64,
    },
    Interval {
        node: kt::Node,
        from: i64,
        keep: i64,
    },
}

impl Core {
    /// A core evaluated at `now`, with `last` the reading before the first span.
    pub(super) fn new(p: Params, now: i64, last: Option<Reading>) -> Self {
        Core {
            np: fold::node_params(&p),
            now,
            chunk: chunk_size(),
            last,
            reaching: VecDeque::new(),
            held_most: 0,
            open: None,
            p,
        }
    }

    #[cfg(test)]
    pub(super) fn held_most(&self) -> usize {
        self.held_most
    }

    /// Opens the window `[start, end)`; the span opened before it must be closed.
    pub(super) fn open(&mut self, start: i64, end: i64) {
        debug_assert!(self.open.is_none(), "one span open at a time");
        let (p, np, now) = (self.p, self.np, self.now);
        let mode = if p.interval.is_some() {
            let ready = |t: i64| p.timing != Timing::Observed || p.ready_at(t) <= now;
            let (from, keep) = (fold::reach(&p, start), fold::reach(&p, end));
            while self.reaching.front().is_some_and(|(t, _)| *t < from) {
                self.reaching.pop_front();
            }
            let mut node = kt::Node::empty(np.kind, start, end);
            let held = self.reaching.iter().filter(|(t, _)| ready(*t));
            kt::credit_intervals(&np, &mut node, held, start, end);
            Mode::Interval { node, from, keep }
        } else if p.wholly_future(start, now) {
            Mode::Future
        } else {
            Mode::Point {
                before: self.last.clone(),
                until: p.credit_limit(end, now).max(start),
                core: None,
                chunk: Vec::with_capacity(self.chunk.min(64)),
                from: start,
            }
        };
        self.open = Some(Open { start, end, mode });
    }

    /// Takes `r`, the next reading in `t` order, into the open span; gives it back when it is at or past the span's
    /// end.
    pub(super) fn offer(&mut self, r: Reading) -> Option<Reading> {
        let (p, np, now) = (self.p, self.np, self.now);
        let o = self.open.as_mut().expect("an open span");
        if r.0 >= o.end {
            return Some(r);
        }
        match &mut o.mode {
            Mode::Future => self.last = Some(r),
            Mode::Point {
                until,
                core,
                chunk,
                from,
                ..
            } => {
                // Past the credit limit, `now`: the next span is wholly ahead of it and takes no reading before it.
                if r.0 >= *until {
                    return None;
                }
                chunk.push(r);
                self.held_most = self.held_most.max(chunk.len());
                if chunk.len() == self.chunk {
                    let to = chunk[chunk.len() - 1].0 + 1;
                    let folded = kt::fold(&np, *from, to, chunk);
                    *core = Some(match core.take() {
                        Some(c) => kt::merge(&np, &c, &folded),
                        None => folded,
                    });
                    *from = to;
                    self.last = chunk.pop();
                    chunk.clear();
                }
            }
            Mode::Interval { node, from, keep } => {
                let ready = p.timing != Timing::Observed || p.ready_at(r.0) <= now;
                if r.0 >= *from && ready {
                    kt::credit_intervals(&np, node, std::iter::once(&r), o.start, o.end);
                }
                if r.0 >= *keep {
                    self.reaching.push_back(r);
                    self.held_most = self.held_most.max(self.reaching.len());
                }
            }
        }
        None
    }

    /// Closes the open span: the window as [`fold::window`] (or [`fold::interval_window`]) evaluates it from every
    /// reading of its reach.
    pub(super) fn close(&mut self) -> kt::Node {
        let o = self.open.take().expect("an open span");
        match o.mode {
            Mode::Future => kt::Node::empty(self.np.kind, o.start, o.end),
            Mode::Point {
                before,
                until,
                core,
                mut chunk,
                from,
            } => {
                let folded = kt::fold(&self.np, from, until, &chunk);
                let core = match core {
                    Some(c) => kt::merge(&self.np, &c, &folded),
                    None => folded,
                };
                if let Some(r) = chunk.pop() {
                    self.last = Some(r);
                }
                let mut n = kt::with_stretches(&self.np, core, before.as_ref(), until);
                n.end = o.end;
                n
            }
            Mode::Interval { node, .. } => node,
        }
    }
}

/// [`Core`] fed from one series' stream of the whole range.
#[cfg(test)]
pub(super) struct Folder {
    core: Core,
    states: usize,
    facts: Box<dyn Iterator<Item = Result<Fact, QueryError>> + Send>,
    next: Option<Reading>,
}

#[cfg(test)]
impl Folder {
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
            core: Core::new(p, now, last),
            states,
            facts,
            next: None,
        }
    }

    pub(super) fn held_most(&self) -> usize {
        self.core.held_most()
    }

    pub(super) fn chunked(mut self, chunk: usize) -> Self {
        self.core.chunk = chunk.max(1);
        self
    }

    /// The window `[start, end)`, as [`fold::window`] evaluates it from every reading of its reach.
    pub(super) fn node(&mut self, start: i64, end: i64) -> Result<kt::Node, QueryError> {
        self.core.open(start, end);
        loop {
            let r = match self.next.take() {
                Some(r) => r,
                None => match self.facts.next() {
                    None => break,
                    Some(f) => match reading(self.states, f?) {
                        Some(r) => r,
                        None => continue,
                    },
                },
            };
            if let Some(r) = self.core.offer(r) {
                self.next = Some(r);
                break;
            }
        }
        Ok(self.core.close())
    }
}

/// Whether the first span of `[from, …)` needs the reading before it: not for a span wholly in the future, the
/// earliest, nor for an interval series, which credits its readings pro rata instead (spec §6).
pub(super) fn needs_last(p: &Params, from: i64, now: i64) -> bool {
    p.interval.is_none() && !p.wholly_future(from, now) && from > i64::MIN
}
