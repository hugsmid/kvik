//! Sources and the passes over them (the query design's §2.2): a pass reads several series over one event-time range
//! from every source within one memory grant; a read that would pass the grant narrows its range and reads again,
//! pass by pass, so that results are equal at any cap (Q-1063). Static `facts` streams pass by pass; `at` searches
//! back from its time in spans that double.

use std::sync::Arc;

use kvik_log::writer::Position as LogPos;

use super::cache::PassKey;
use super::log_source::LogTail;
use super::merge;
use super::view::View;
use crate::durable::index::{Stored, fact_out};
use crate::durable::reader::DurableReader;
use crate::error::QueryError;
use crate::types::Fact;

/// One pass: series ids in ascending order, an inclusive event-time range, and the highest position read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pass<'a> {
    pub(crate) series: &'a [u32],
    pub(crate) from: i64,
    pub(crate) to: i64,
    pub(crate) bound: LogPos,
}

/// What a source read for a pass.
#[derive(Debug)]
pub(crate) enum Read {
    /// The pass's facts in the merge's order ([`merge::sort`]).
    Rows(Vec<Stored>),
    /// They would pass the budget.
    TooMany,
}

/// A stream of facts within a position bound (the query design's §2.2 One trait, two homes).
pub(crate) trait Source {
    /// The facts of `pass`, keeping at most `budget` bytes (`kvik_log::reader::fact_bytes`).
    fn read(&mut self, pass: &Pass<'_>, budget: u64) -> Result<Read, QueryError>;
}

/// What a read selects: series with whether each is versioned, ascending by id; history or the current view; the
/// position bound (the cut, or a position or time as of which it reads).
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub(crate) series: Vec<(u32, bool)>,
    pub(crate) history: bool,
    pub(crate) bound: LogPos,
}

impl Plan {
    fn versioned(&self, id: u32) -> bool {
        self.series
            .binary_search_by_key(&id, |(s, _)| *s)
            .is_ok_and(|k| self.series[k].1)
    }
}

/// The facts of one pass over `series` of `plan` at `v`'s cut, merged from every source; `None` when they would
/// pass the pass's grant.
pub(crate) fn pass(
    r: &DurableReader,
    v: &View,
    series: &[u32],
    from: i64,
    to: i64,
    bound: LogPos,
) -> Result<Option<Arc<[Stored]>>, QueryError> {
    let key = PassKey {
        series: series.to_vec(),
        from,
        to,
        bound,
        stamp: v.cut().stamp,
        losses: (v.lost_ranges().len(), r.found_lost()),
    };
    let memory = r.memory();
    if let Some(rows) = memory.cached(&key) {
        return Ok(Some(rows));
    }
    let grant = memory.admit();
    let p = Pass {
        series,
        from,
        to,
        bound,
    };
    let mut sources: Vec<Box<dyn Source + '_>> = vec![Box::new(LogTail { reader: r, view: v })];
    let budget = grant.rows() / sources.len() as u64;
    let mut parts = Vec::with_capacity(sources.len());
    for s in &mut sources {
        match s.read(&p, budget)? {
            Read::Rows(rows) => parts.push(rows),
            Read::TooMany => return Ok(None),
        }
    }
    let rows: Arc<[Stored]> = merge::merge(parts).into();
    let bytes = rows
        .iter()
        .map(|s| kvik_log::reader::fact_bytes(&s.row))
        .sum();
    drop(grant);
    memory.keep(key, rows.clone(), bytes);
    Ok(Some(rows))
}

/// A part of a read: an inclusive event-time range over the plan's series from `lo` to `hi`, exclusive.
#[derive(Clone, Copy, Debug)]
struct Part {
    from: i64,
    to: i64,
    lo: usize,
    hi: usize,
}

impl Part {
    /// The part in two, earlier time (or lower series) first; `None` for one key.
    fn halves(self) -> Option<(Part, Part)> {
        if self.from < self.to {
            let mid = i64::try_from((i128::from(self.from) + i128::from(self.to)).div_euclid(2))
                .expect("between two i64");
            return Some((
                Part { to: mid, ..self },
                Part {
                    from: mid + 1,
                    ..self
                },
            ));
        }
        if self.hi - self.lo > 1 {
            let mid = self.lo + (self.hi - self.lo) / 2;
            return Some((Part { hi: mid, ..self }, Part { lo: mid, ..self }));
        }
        None
    }
}

fn too_large() -> QueryError {
    QueryError::NotAvailable {
        reason: "the facts of one key pass the reader's memory cap".into(),
    }
}

/// Reads `part` of `plan`, narrowing it until each piece fits; the rows of each piece, in the order `later_first`
/// does not reverse, go to `each`, which stops the read by returning `true`.
fn read_parts(
    r: &DurableReader,
    v: &View,
    plan: &Plan,
    part: Part,
    later_first: bool,
    each: &mut dyn FnMut(&[Stored]) -> bool,
) -> Result<bool, QueryError> {
    let ids: Vec<u32> = plan.series.iter().map(|(s, _)| *s).collect();
    let mut stack = vec![part];
    while let Some(p) = stack.pop() {
        r.check(v)?;
        match pass(r, v, &ids[p.lo..p.hi], p.from, p.to, plan.bound)? {
            Some(rows) => {
                if each(&rows) {
                    return Ok(true);
                }
            }
            None => {
                let (a, b) = p.halves().ok_or_else(too_large)?;
                if later_first {
                    stack.extend([a, b]);
                } else {
                    stack.extend([b, a]);
                }
            }
        }
    }
    Ok(false)
}

/// Static `facts` (spec §2.3), streamed pass by pass at one cut: event-time order, then series, then position.
#[derive(Debug)]
pub(crate) struct FactStream {
    reader: Arc<DurableReader>,
    view: Option<Arc<View>>,
    plan: Plan,
    parts: Vec<Part>,
    ready: std::vec::IntoIter<Fact>,
}

impl FactStream {
    /// The facts of `plan` with `t` from `from` to `to`, both inclusive, at `view`'s cut.
    pub(crate) fn new(
        reader: Arc<DurableReader>,
        view: Arc<View>,
        plan: Plan,
        from: i64,
        to: i64,
    ) -> Self {
        let parts = if from <= to && !plan.series.is_empty() {
            vec![Part {
                from,
                to,
                lo: 0,
                hi: plan.series.len(),
            }]
        } else {
            Vec::new()
        };
        FactStream {
            reader,
            view: Some(view),
            plan,
            parts,
            ready: Vec::new().into_iter(),
        }
    }

    fn end(&mut self) {
        self.parts.clear();
        if self.view.take().is_some() {
            self.reader.release_dropped();
        }
    }

    /// The next part's facts, narrowing it first if it does not fit.
    fn step(&mut self) -> Result<(), QueryError> {
        let v = self.view.clone().expect("a view while parts remain");
        let p = self.parts.pop().expect("a part");
        self.reader.check(&v)?;
        match pass(
            &self.reader,
            &v,
            &self.plan.series[p.lo..p.hi]
                .iter()
                .map(|(s, _)| *s)
                .collect::<Vec<_>>(),
            p.from,
            p.to,
            self.plan.bound,
        )? {
            Some(rows) => {
                let plan = &self.plan;
                self.ready = merge::select(&rows, plan.history, |id| plan.versioned(id))
                    .map(fact_out)
                    .collect::<Vec<_>>()
                    .into_iter();
            }
            None => {
                let (a, b) = p.halves().ok_or_else(too_large)?;
                self.parts.extend([b, a]);
            }
        }
        Ok(())
    }
}

impl Iterator for FactStream {
    type Item = Result<Fact, QueryError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(f) = self.ready.next() {
                return Some(Ok(f));
            }
            if self.parts.is_empty() {
                self.end();
                return None;
            }
            if let Err(e) = self.step() {
                self.end();
                return Some(Err(e));
            }
        }
    }
}

/// The span `at` first searches back over, doubled at each step.
const FIRST_SPAN: i64 = 3_600_000_000_000;

/// The fact `pick` chooses among the view's facts at or before `t`, searching back from `t` in spans that double,
/// each read in passes that fit; the first span holding one decides.
pub(crate) fn search_back(
    r: &DurableReader,
    v: &View,
    plan: &Plan,
    t: i64,
    pick: &dyn Fn(&Stored) -> bool,
) -> Result<Option<Fact>, QueryError> {
    let mut to = t;
    let mut span = FIRST_SPAN;
    loop {
        let from = to.saturating_sub(span - 1);
        let mut found: Option<Fact> = None;
        let part = Part {
            from,
            to,
            lo: 0,
            hi: plan.series.len(),
        };
        read_parts(r, v, plan, part, true, &mut |rows| {
            found = merge::select(rows, plan.history, |id| plan.versioned(id))
                .filter(|s| pick(s))
                .max_by_key(|s| (s.row.t, s.position))
                .map(fact_out);
            found.is_some()
        })?;
        if found.is_some() || from == i64::MIN {
            return Ok(found);
        }
        to = from - 1;
        span = span.saturating_mul(2);
    }
}

#[cfg(test)]
mod tests;
