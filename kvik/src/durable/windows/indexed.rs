//! Windows and chart buckets from the reader's fact index, as they were computed before roadmap 9.0h: the oracle
//! the streamed fold is compared with.

use std::ops::Bound;

use km::summary::Node;
use kvik_model as km;

use super::{Of, reading_of, span_in};
use crate::backend::convert;
use crate::durable::index::{Index, fact_out};
use crate::durable::reader::DurableReader;
use crate::error::QueryError;
use crate::fold::{self, Reading};
use crate::read::{Aggregate, ChartData, Resolution, Window, WindowResult};
use crate::rules;
use crate::time::TimeRange;
use crate::types::{Fact, SeriesId};

impl Of<'_> {
    fn readings(&self, from: i64, to: i64) -> Vec<Reading> {
        if from >= to {
            return Vec::new();
        }
        self.current(Bound::Included(from), Bound::Excluded(to), false)
            .filter_map(|s| reading_of(self.series, s))
            .collect()
    }

    fn prior(&self, t: i64) -> Option<Reading> {
        if self.params.interval.is_some() {
            return None;
        }
        self.current(Bound::Unbounded, Bound::Excluded(t), true)
            .find_map(|s| reading_of(self.series, s))
    }

    fn model(&self, n: kvik_tree::Node) -> Node {
        fold::model_node(n, &self.series.def.state_names)
    }
}

/// A window result from the fact index.
pub(crate) fn window(
    r: &DurableReader,
    id: SeriesId,
    window: &Window,
    agg: &Aggregate,
) -> Result<WindowResult, QueryError> {
    let now = r.now();
    r.with(|i: &Index, _| {
        let of = Of::new(i, id)?;
        let a = rules::aggregate_for(&of.series.spec, of.params.kind, agg)?;
        let (from, to) = span_in(r, i.registry(), window, now)?;
        let readings = of.readings(fold::reach(&of.params, from), to);
        let node = of.model(fold::window(
            &of.params,
            of.prior(from).as_ref(),
            &readings,
            from,
            to,
            now,
        ));
        Ok(rules::window_result(
            Some(&of.series.spec),
            &node,
            &a,
            (from, to),
            false,
            false,
        ))
    })
}

/// A chart query from the fact index.
pub(crate) fn buckets(
    r: &DurableReader,
    id: SeriesId,
    range: TimeRange,
    resolution: Resolution,
) -> Result<ChartData, QueryError> {
    let now = r.now();
    r.with(|i: &Index, _| {
        let of = Of::new(i, id)?;
        let (a, b) = rules::chart_range(range)?;
        let step = match resolution {
            Resolution::Auto { max_points } => {
                let max = max_points.max(1);
                let range = || of.current(Bound::Included(a), Bound::Excluded(b), false);
                if range().take(max.saturating_add(1)).count() <= max {
                    return Ok(ChartData::Facts(
                        range().map(fact_out).collect::<Vec<Fact>>(),
                    ));
                }
                rules::auto_step(a, b, max_points)
            }
            Resolution::Step(d) => rules::fixed_step(d, a, b)?,
        };
        let p = &of.params;
        let readings = of.readings(fold::reach(p, a), b);
        let prior = of.prior(a);
        let buckets = rules::bucket_spans(a, b, step)
            .map(|(lo, hi)| {
                let before = fold::before(&readings, lo).or(prior.as_ref());
                let node = of.model(fold::window(p, before, &readings, lo, hi, now));
                rules::bucket(&of.series.spec, &node, lo, hi, false)
            })
            .collect();
        Ok(ChartData::Buckets {
            step: convert::dur(i128::from(step)),
            buckets,
        })
    })
}
