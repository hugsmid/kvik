//! Windows and chart buckets of several series folded from one stream of their current facts at one cut, in `t`
//! order then series: the log is read once for all of them (roadmap 9.0h, Q-1110). Each series holds its [`Core`]'s
//! chunk and, for an automatic resolution, its raw facts until they pass the most points.

use std::sync::Arc;

use km::summary::Params;
use kvik_model as km;
use kvik_tree as kt;

use super::super::index::{Stored, fact_into};
use super::super::read::source::{each_row, search_back_each};
use super::super::read::view::View;
use super::super::reader::DurableReader;
use super::super::reader::facts::plan;
use super::super::registry::Series;
use super::stream::{Core, needs_last, readable, reading};
use super::{params, reading_of, series_in, span_in};
use crate::backend::{Error, convert};
use crate::error::QueryError;
use crate::fold::{self, Reading};
use crate::read::{
    Aggregate, Bucket, ChartData, Resolution, View as ReadView, Window, WindowResult,
};
use crate::rules;
use crate::types::{Fact, SeriesId};

/// What one series' spans fold into.
enum Out {
    Buckets(Vec<Bucket>),
    Node(Option<Box<kt::Node>>),
}

/// An automatic resolution's raw facts of `[a, …)`, kept until there are more than `max`, and the readings before `a`
/// an interval series' first bucket reaches back to.
struct Head {
    a: i64,
    max: usize,
    facts: Vec<Fact>,
    before: Vec<Reading>,
}

/// One series of a many-series read: its spans in order, the span open, and what they fold into.
struct Member<'a> {
    series: &'a Series,
    states: usize,
    reach: i64,
    core: Core,
    spans: Box<dyn Iterator<Item = (i64, i64)>>,
    open: Option<(i64, i64)>,
    out: Out,
    head: Option<Head>,
}

impl Member<'_> {
    fn take(&mut self, s: Stored) {
        if s.row.t < self.reach {
            return;
        }
        if let Some(h) = &mut self.head {
            if s.row.t < h.a {
                h.before.extend(reading_of(self.series, &s));
                return;
            }
            h.facts.push(fact_into(s));
            if h.facts.len() > h.max {
                let h = self.head.take().expect("a head");
                for r in h.before {
                    self.feed(r);
                }
                for f in h.facts {
                    if let Some(r) = reading(self.states, f) {
                        self.feed(r);
                    }
                }
            }
            return;
        }
        if let Some(r) = reading_of(self.series, &s) {
            self.feed(r);
        }
    }

    fn feed(&mut self, mut r: Reading) {
        loop {
            if self.open.is_none() {
                let Some((lo, hi)) = self.spans.next() else {
                    return;
                };
                self.core.open(lo, hi);
                self.open = Some((lo, hi));
            }
            match self.core.offer(r) {
                None => return,
                Some(back) => {
                    self.close();
                    r = back;
                }
            }
        }
    }

    fn close(&mut self) {
        let (lo, hi) = self.open.take().expect("an open span");
        let node = self.core.close();
        match &mut self.out {
            Out::Buckets(b) => {
                let node = fold::model_node(node, &self.series.def.state_names);
                b.push(rules::bucket(&self.series.spec, &node, lo, hi, false));
            }
            Out::Node(n) => *n = Some(Box::new(node)),
        }
    }

    /// Closes the open span and folds the spans left, which no reading reaches.
    fn finish(&mut self) {
        if self.head.is_some() {
            return;
        }
        if self.open.is_some() {
            self.close();
        }
        while let Some((lo, hi)) = self.spans.next() {
            self.core.open(lo, hi);
            self.open = Some((lo, hi));
            self.close();
        }
    }
}

/// The distinct series of `ids`, ascending, and where each id's result is among them.
fn distinct(ids: &[SeriesId]) -> (Vec<SeriesId>, Vec<usize>) {
    let mut set = ids.to_vec();
    set.sort_unstable();
    set.dedup();
    let at = ids
        .iter()
        .map(|id| set.binary_search(id).expect("an id of the set"))
        .collect();
    (set, at)
}

/// Members for each of `series`, spans from `spans`, evaluated at `now`, with the reading before `from` for those whose
/// first span needs it, found by one search for them all.
fn members<'a>(
    r: &Arc<DurableReader>,
    v: &Arc<View>,
    series: &[(&'a Series, Params)],
    from: i64,
    now: i64,
    member: impl Fn(&'a Series, Core) -> Member<'a>,
) -> Result<Vec<Member<'a>>, QueryError> {
    let wanted: Vec<SeriesId> = series
        .iter()
        .filter(|(_, p)| needs_last(p, from, now))
        .map(|(s, _)| SeriesId(s.def.id))
        .collect();
    let mut lasts = vec![None; series.len()];
    if !wanted.is_empty() {
        let plan = plan(r, v, &wanted, ReadView::Current)?;
        let states = |id: SeriesId| {
            series
                .iter()
                .find(|(s, _)| s.def.id == id.0)
                .map_or(0, |(s, _)| s.def.state_names.len())
        };
        let found = search_back_each(r, v, &plan, from - 1, &|f| readable(states(f.series), f))?;
        for ((id, _), f) in plan.series.iter().zip(found) {
            let k = series
                .iter()
                .position(|(s, _)| s.def.id == *id)
                .expect("a series asked for");
            lasts[k] = f.and_then(|f| reading(states(SeriesId(*id)), f));
        }
    }
    Ok(series
        .iter()
        .zip(lasts)
        .map(|((s, p), last)| member(s, Core::new(*p, now, last)))
        .collect())
}

/// Streams the current facts of the members' series from the least reach to `to`, exclusive, into each member, then
/// finishes them.
fn drive(
    r: &Arc<DurableReader>,
    v: &Arc<View>,
    members: &mut [Member<'_>],
    to: i64,
) -> Result<(), QueryError> {
    let ids: Vec<SeriesId> = members.iter().map(|m| SeriesId(m.series.def.id)).collect();
    let from = members.iter().map(|m| m.reach).min().unwrap_or(to);
    if from < to {
        let plan = plan(r, v, &ids, ReadView::Current)?;
        each_row(r, v, &plan, from, to - 1, |s| {
            let k = ids
                .binary_search(&SeriesId(s.row.series_id))
                .expect("a series of the read");
            members[k].take(s);
        })?;
    }
    for m in members.iter_mut() {
        m.finish();
    }
    Ok(())
}

/// Chart queries of several series over one range at one resolution, from one stream of the range: each as
/// [`super::buckets`] answers it for that series alone.
pub(crate) fn buckets_of(
    r: &Arc<DurableReader>,
    ids: &[SeriesId],
    range: crate::time::TimeRange,
    resolution: Resolution,
) -> Result<Vec<ChartData>, QueryError> {
    let now = r.now();
    let (set, at) = distinct(ids);
    super::stream::on_view(r, |v| {
        // Checked in the order one series' call checks them, series by series, so that the first series in id order
        // that would fail alone is the one the call fails as.
        let mut series = Vec::with_capacity(set.len());
        let mut shape = None;
        for id in &set {
            let s = series_in(v, *id)?;
            series.push((s, params(&s.def)));
            if shape.is_none() {
                let (a, b) = rules::chart_range(range).map_err(Error::query)?;
                let (step, max) = match resolution {
                    Resolution::Auto { max_points } => {
                        (rules::auto_step(a, b, max_points), Some(max_points.max(1)))
                    }
                    Resolution::Step(d) => {
                        (rules::fixed_step(d, a, b).map_err(Error::query)?, None)
                    }
                };
                shape = Some((a, b, step, max));
            }
        }
        let Some((a, b, step, max)) = shape else {
            return Ok(Vec::new());
        };
        let mut members = members(r, v, &series, a, now, |s, core| Member {
            series: s,
            states: s.def.state_names.len(),
            reach: fold::reach(&params(&s.def), a),
            core,
            spans: Box::new(rules::bucket_spans(a, b, step)),
            open: None,
            out: Out::Buckets(Vec::new()),
            head: max.map(|max| Head {
                a,
                max,
                facts: Vec::new(),
                before: Vec::new(),
            }),
        })?;
        drive(r, v, &mut members, b)?;
        let charts: Vec<ChartData> = members
            .into_iter()
            .map(|m| match (m.head, m.out) {
                (Some(h), _) => ChartData::Facts(h.facts),
                (None, Out::Buckets(buckets)) => ChartData::Buckets {
                    step: convert::dur(i128::from(step)),
                    buckets,
                },
                (None, Out::Node(_)) => unreachable!("buckets fold into buckets"),
            })
            .collect();
        Ok(at.iter().map(|&k| charts[k].clone()).collect())
    })
}

/// Window results of several series over one window and aggregate, from one stream of its span: each as
/// [`super::window`] answers it for that series alone.
pub(crate) fn window_of(
    r: &Arc<DurableReader>,
    ids: &[SeriesId],
    window: &Window,
    agg: &Aggregate,
) -> Result<Vec<WindowResult>, QueryError> {
    let now = r.now();
    let (set, at) = distinct(ids);
    super::stream::on_view(r, |v| {
        let mut series = Vec::with_capacity(set.len());
        let mut aggs = Vec::with_capacity(set.len());
        let mut span = None;
        for id in &set {
            let s = series_in(v, *id)?;
            let p = params(&s.def);
            aggs.push(rules::aggregate_for(&s.spec, p.kind, agg).map_err(Error::query)?);
            series.push((s, p));
            if span.is_none() {
                span = Some(span_in(r, v.latest().registry(), window, now).map_err(Error::query)?);
            }
        }
        let Some(span) = span else {
            return Ok(Vec::new());
        };
        let mut members = members(r, v, &series, span.0, now, |s, core| Member {
            series: s,
            states: s.def.state_names.len(),
            reach: fold::reach(&params(&s.def), span.0),
            core,
            spans: Box::new(std::iter::once(span)),
            open: None,
            out: Out::Node(None),
            head: None,
        })?;
        drive(r, v, &mut members, span.1)?;
        let results: Vec<WindowResult> = members
            .into_iter()
            .zip(&aggs)
            .map(|(m, a)| {
                let node = match m.out {
                    Out::Node(Some(n)) => *n,
                    _ => unreachable!("a window folds its one span"),
                };
                let node = fold::model_node(node, &m.series.def.state_names);
                rules::window_result(Some(&m.series.spec), &node, a, span, false, false)
            })
            .collect();
        Ok(at.iter().map(|&k| results[k].clone()).collect())
    })
}
