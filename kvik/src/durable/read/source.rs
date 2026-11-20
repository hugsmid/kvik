//! Sources and the passes over them (the query design's §2.2): a pass reads several series over one event-time range
//! from every source within one memory grant; a read that would pass the grant narrows its range and reads again,
//! pass by pass, down to one key's positions, so that results are equal at any cap (Q-1063, Q-1067). Static `facts`
//! streams pass by pass; `at` searches back from its time in spans that double.

use std::sync::{Arc, PoisonError};

use kvik_log::writer::Position as LogPos;

use super::cache::{Grant, Shelf, held};
use super::log_source::LogTail;
use super::merge;
use super::view::View;
use crate::durable::index::{Stored, fact_into, fact_out};
use crate::durable::reader::DurableReader;
use crate::error::QueryError;
use crate::types::Fact;

/// One pass: series ids in ascending order, an inclusive event-time range, and the positions read: above `above`
/// and at or below `upto`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pass<'a> {
    pub(crate) series: &'a [u32],
    pub(crate) from: i64,
    pub(crate) to: i64,
    pub(crate) above: Option<LogPos>,
    pub(crate) upto: LogPos,
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
    /// The facts of `pass`, keeping at most `budget` bytes (`kvik_log::reader::Within`).
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

fn segment_gone() -> QueryError {
    QueryError::NotAvailable {
        reason: "a log segment the read listed is gone".into(),
    }
}

/// The facts of one pass at `v`'s cut, merged from every source, with the grant that counts them; `None` when they
/// would pass the grant. A pass is read again rather than kept: a lookup verifies every batch it reads, so damage
/// after an earlier read is found (Q-1063). `listed` holds the segments the read's first pass listed: one gone at a
/// later pass ends the read (Q-1066).
fn pass(
    r: &DurableReader,
    v: &View,
    p: &Pass<'_>,
    listed: &mut Option<Vec<u64>>,
) -> Result<Option<(Vec<Stored>, Grant)>, QueryError> {
    let (rows, now) = match pass_once(r, v, p) {
        Err(QueryError::NotAvailable { .. }) => {
            r.find_losses()?;
            pass_once(r, v, p)?
        }
        other => other?,
    };
    match listed {
        Some(first) if first.iter().any(|id| now.binary_search(id).is_err()) => {
            return Err(segment_gone());
        }
        Some(_) => {}
        None => *listed = Some(now),
    }
    Ok(rows)
}

type Once = (Option<(Vec<Stored>, Grant)>, Vec<u64>);

fn pass_once(r: &DurableReader, v: &View, p: &Pass<'_>) -> Result<Once, QueryError> {
    let grant = r.memory().admit();
    let mut tail = LogTail {
        reader: r,
        view: v,
        listed: Vec::new(),
    };
    let rows = match tail.read(p, grant.rows())? {
        Read::Rows(rows) => Some((merge::merge(vec![rows]), grant)),
        Read::TooMany => None,
    };
    Ok((rows, tail.listed))
}

/// A part of a read: an inclusive event-time range over the plan's series from `lo` to `hi`, exclusive, and the
/// positions above `above` and at or below `upto`; `last` when no part of the same key follows it.
#[derive(Clone, Copy, Debug)]
struct Part {
    from: i64,
    to: i64,
    lo: usize,
    hi: usize,
    above: Option<LogPos>,
    upto: LogPos,
    last: bool,
}

/// A position as one number, generation then sequence.
fn linear(p: Option<LogPos>) -> u128 {
    p.map_or(0, |p| {
        (u128::from(p.generation) << 64) | u128::from(p.sequence)
    })
}

fn position(n: u128) -> LogPos {
    LogPos {
        generation: u32::try_from(n >> 64).expect("from two positions"),
        sequence: n as u64,
    }
}

impl Part {
    fn whole(plan: &Plan, from: i64, to: i64) -> Self {
        Part {
            from,
            to,
            lo: 0,
            hi: plan.series.len(),
            above: None,
            upto: plan.bound,
            last: true,
        }
    }

    /// Whether the part is one key's positions narrowed from the plan's.
    fn narrowed(&self, plan: &Plan) -> bool {
        self.above.is_some() || self.upto != plan.bound
    }

    /// The part in two, in the order they are read (earlier time, lower series or lower positions first, or the
    /// reverse when `later_first`); `None` for one key at one position.
    fn halves(self, later_first: bool) -> Option<(Part, Part)> {
        let (a, b) = if self.from < self.to {
            let mid = i64::try_from((i128::from(self.from) + i128::from(self.to)).div_euclid(2))
                .expect("between two i64");
            (
                Part { to: mid, ..self },
                Part {
                    from: mid + 1,
                    ..self
                },
            )
        } else if self.hi - self.lo > 1 {
            let mid = self.lo + (self.hi - self.lo) / 2;
            (Part { hi: mid, ..self }, Part { lo: mid, ..self })
        } else {
            let (lo, hi) = (linear(self.above), linear(Some(self.upto)));
            if hi - lo < 2 {
                return None;
            }
            let mid = position(lo + (hi - lo) / 2);
            (
                Part {
                    upto: mid,
                    last: false,
                    ..self
                },
                Part {
                    above: Some(mid),
                    ..self
                },
            )
        };
        Some(if later_first {
            (
                Part {
                    last: self.last,
                    ..b
                },
                Part { last: false, ..a },
            )
        } else {
            (
                Part { last: false, ..a },
                Part {
                    last: self.last,
                    ..b
                },
            )
        })
    }
}

fn too_large() -> QueryError {
    QueryError::NotAvailable {
        reason: "one fact passes the reader's memory cap".into(),
    }
}

/// The parts of a read still to come and the state of one key read in several parts (Q-1067).
#[derive(Debug)]
struct Parts {
    stack: Vec<Part>,
    later_first: bool,
    listed: Option<Vec<u64>>,
    /// The current view's fact of a key read in parts, so far.
    fold: Option<Stored>,
}

/// The next rows of a read, in order.
enum Unit {
    Rows(Vec<Stored>, Grant, Part),
    /// The current view's fact of a key read in several parts.
    Folded(Stored),
}

impl Parts {
    fn new(part: Option<Part>, later_first: bool) -> Self {
        Parts {
            stack: part.into_iter().collect(),
            later_first,
            listed: None,
            fold: None,
        }
    }

    /// The next part's rows of `plan`'s view, narrowing parts that do not fit; `None` at the end.
    fn next(
        &mut self,
        r: &DurableReader,
        v: &View,
        plan: &Plan,
    ) -> Result<Option<Unit>, QueryError> {
        let ids: Vec<u32> = plan.series.iter().map(|(s, _)| *s).collect();
        while let Some(p) = self.stack.pop() {
            r.check(v)?;
            let pass_of = Pass {
                series: &ids[p.lo..p.hi],
                from: p.from,
                to: p.to,
                above: p.above,
                upto: p.upto,
            };
            let Some((mut rows, grant)) = pass(r, v, &pass_of, &mut self.listed)? else {
                let (first, second) = p.halves(self.later_first).ok_or_else(too_large)?;
                self.stack.extend([second, first]);
                continue;
            };
            merge::keep_view(&mut rows, plan.history, |id| plan.versioned(id));
            if !plan.history && p.narrowed(plan) {
                let v = plan.versioned(ids[p.lo]);
                for s in rows {
                    if self
                        .fold
                        .as_ref()
                        .is_none_or(|b| merge::rank(&s, v) > merge::rank(b, v))
                    {
                        self.fold = Some(s);
                    }
                }
                drop(grant);
                if p.last
                    && let Some(s) = self.fold.take()
                {
                    return Ok(Some(Unit::Folded(s)));
                }
                continue;
            }
            if !rows.is_empty() {
                return Ok(Some(Unit::Rows(rows, grant, p)));
            }
        }
        Ok(None)
    }
}

/// Static `facts` (spec §2.3), streamed pass by pass at one cut: event-time order, then series, then position. The
/// rows of a pass wait for delivery on a shelf under their grant; an admission that empties the shelf has the stream
/// read that part again and pass over what it delivered (Q-1066).
#[derive(Debug)]
pub(crate) struct FactStream {
    reader: Arc<DurableReader>,
    view: Option<Arc<View>>,
    plan: Plan,
    parts: Parts,
    shelf: Option<(Arc<Shelf>, Part)>,
    /// Rows taken from the shelf.
    popped: u64,
    /// Rows still to pass over, delivered before the shelf was emptied.
    skip: u64,
    folded: Option<Stored>,
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
        let whole = (from <= to && !plan.series.is_empty()).then(|| Part::whole(&plan, from, to));
        FactStream {
            reader,
            view: Some(view),
            parts: Parts::new(whole, false),
            plan,
            shelf: None,
            popped: 0,
            skip: 0,
            folded: None,
        }
    }

    fn end(&mut self) {
        self.parts.stack.clear();
        self.shelf = None;
        if self.view.take().is_some() {
            self.reader.release_dropped();
        }
    }

    /// The next row of the shelf; `None` once it is empty or emptied.
    fn pop(&mut self) -> Option<Stored> {
        let (shelf, part) = self.shelf.as_ref()?;
        let mut g = shelf.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((rows, _)) = g.as_mut() else {
            let part = *part;
            drop(g);
            self.shelf = None;
            self.parts.stack.push(part);
            self.skip += self.popped;
            return None;
        };
        if let Some(s) = rows.next() {
            self.popped += 1;
            return Some(s);
        }
        *g = None;
        drop(g);
        self.shelf = None;
        None
    }

    fn step(&mut self) -> Result<bool, QueryError> {
        let v = self.view.clone().expect("a view while parts remain");
        match self.parts.next(&self.reader, &v, &self.plan)? {
            None => Ok(false),
            Some(Unit::Folded(s)) => {
                self.folded = Some(s);
                Ok(true)
            }
            Some(Unit::Rows(rows, grant, part)) => {
                let bytes = held(&rows);
                let shelf = self.reader.memory().shelve(rows, grant, bytes);
                self.shelf = Some((shelf, part));
                self.popped = 0;
                Ok(true)
            }
        }
    }
}

impl Iterator for FactStream {
    type Item = Result<Fact, QueryError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(s) = self.pop().or_else(|| self.folded.take()) {
                if self.skip > 0 {
                    self.skip -= 1;
                    continue;
                }
                return Some(Ok(fact_into(s)));
            }
            if self.shelf.is_some() {
                continue;
            }
            if self.view.is_none() {
                return None;
            }
            match self.step() {
                Ok(true) => {}
                Ok(false) => {
                    self.end();
                    return None;
                }
                Err(e) => {
                    self.end();
                    return Some(Err(e));
                }
            }
        }
    }
}

impl Drop for FactStream {
    fn drop(&mut self) {
        self.end();
    }
}

/// The span `at` first searches back over, doubled at each step.
const FIRST_SPAN: i64 = 3_600_000_000_000;

/// What a probe for earlier facts may keep: past it, there are some.
const PROBE: u64 = 4 << 10;

/// Whether the plan's series hold no fact at or before `to`, from one lookup that keeps a few facts at most; `false`
/// when it cannot tell.
fn nothing_at_or_before(
    r: &DurableReader,
    v: &View,
    plan: &Plan,
    to: i64,
) -> Result<bool, QueryError> {
    let grant = r.memory().admit();
    let wanted: Vec<kvik_log::reader::Wanted> = plan
        .series
        .iter()
        .map(|&(series_id, _)| kvik_log::reader::Wanted {
            series_id,
            from: i64::MIN,
            to,
        })
        .collect();
    let within = kvik_log::reader::Within {
        budget: PROBE.min(grant.rows()),
        above: None,
        upto: Some(plan.bound),
    };
    match r.lookup_within(v, &wanted, within) {
        Ok((Some(found), _)) => Ok(found.is_empty()),
        Ok((None, _)) | Err(QueryError::NotAvailable { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// The fact `pick` chooses among the view's facts at or before `t`, searching back from `t` in spans that double,
/// each read in passes that fit, later first; the first span holding one decides. After the first empty span one
/// probe ends the search when the series hold no earlier fact (Q-1065).
pub(crate) fn search_back(
    r: &DurableReader,
    v: &View,
    plan: &Plan,
    t: i64,
    pick: &dyn Fn(&Fact) -> bool,
) -> Result<Option<Fact>, QueryError> {
    if plan.series.is_empty() {
        return Ok(None);
    }
    let mut to = t;
    let mut span = FIRST_SPAN;
    let mut probed = false;
    loop {
        let from = to.saturating_sub(span - 1);
        let mut parts = Parts::new(Some(Part::whole(plan, from, to)), true);
        while let Some(unit) = parts.next(r, v, plan)? {
            let found = match &unit {
                Unit::Rows(rows, _, _) => rows
                    .iter()
                    .map(fact_out)
                    .filter(|f| pick(f))
                    .max_by_key(|f| (f.t, f.position)),
                Unit::Folded(s) => Some(fact_out(s)).filter(|f| pick(f)),
            };
            if found.is_some() {
                return Ok(found);
            }
        }
        if from == i64::MIN {
            return Ok(None);
        }
        if !probed {
            probed = true;
            if nothing_at_or_before(r, v, plan, from - 1)? {
                return Ok(None);
            }
        }
        to = from - 1;
        span = span.saturating_mul(2);
    }
}

#[cfg(test)]
mod tests;
