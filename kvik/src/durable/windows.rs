//! Live state, windows, chart buckets and calendar results of a durable store (roadmap 5.4), computed from the facts
//! of the kept log that a reader holds (spec §2.3, §5, §6).
//!
//! Each call reads the current facts it needs from the reader's [`Index`] and evaluates them with [`crate::fold`];
//! no summary is stored yet. When the summary tree arrives (roadmap 7.x), its stored nodes take the place of the
//! parts they cover in a window's core, and these calls keep their shape.

use std::ops::Bound;
use std::sync::OnceLock;

use km::summary::{CounterRules, Node, Params};
use kvik_log::entries::{
    Definition, IntervalLength as LogInterval, Kind as LogKind, Timing as LogTiming, Versioning,
};
use kvik_log::facts::{Quality as LogQuality, Value as LogValue};
use kvik_model as km;

use super::index::{Index, Stored, fact_out};
use super::reader::DurableReader;
use super::registry::Series;
use crate::backend::{Error, convert};
use crate::error::QueryError;
use crate::fold::{self, Reading};
use crate::read::{Aggregate, ChartData, Live, LiveValue, Resolution, Window, WindowResult};
use crate::rules;
use crate::time::TimeRange;
use crate::types::{Fact, SeriesId};

/// The time-zone rules calendar results are taken with: those the reference model embeds, so that both backends
/// take the same local days. A reader computes calendar results with its own release (spec §5.7).
fn tzdb() -> &'static km::calendar::Tzdb {
    static DB: OnceLock<km::calendar::Tzdb> = OnceLock::new();
    DB.get_or_init(km::calendar::Tzdb::bundled)
}

/// What summaries of a series depend on (spec §2.1), from its definition entry.
pub(crate) fn params(d: &Definition) -> Params {
    use km::types::Kind as K;
    Params {
        kind: match d.kind {
            LogKind::Gauge => K::Gauge,
            LogKind::Counter => K::Counter,
            LogKind::State => K::State,
            LogKind::Text => K::Text,
            LogKind::Interval => K::Interval,
        },
        staleness: d.staleness,
        // Exact: a ceiling off the parts-per-million grid is refused at definition (Q-142).
        counter: d.counter.map_or(CounterRules::default(), |c| CounterRules {
            wrap_at: c.wrap_at.map(f64::from_bits),
            reset_ceiling: f64::from(c.reset_ceiling_ppm) / 1_000_000.0,
        }),
        expected_interval: d.expected_interval,
        timing: match d.timing {
            LogTiming::Observed => km::registry::Timing::Observed,
            LogTiming::Scheduled => km::registry::Timing::Scheduled,
            LogTiming::Forecast => km::registry::Timing::Forecast,
        },
        interval: match d.interval {
            Some(LogInterval::Fixed { length, phase }) => {
                Some(km::types::IntervalSpec { length, phase })
            }
            _ => None,
        },
    }
}

/// A current fact as a summarised reading (spec §5.2): `None` for bad quality, which summaries ignore, and for a
/// state index the series does not declare. A named state is folded by its index.
pub(crate) fn reading_of(series: &Series, s: &Stored) -> Option<Reading> {
    let r = &s.row;
    if r.quality == LogQuality::Bad {
        return None;
    }
    let v = match &r.value {
        LogValue::F64(bits) => kvik_tree::Value::F64(f64::from_bits(*bits)),
        LogValue::State(i) => {
            series.def.state_names.get(usize::from(*i))?;
            kvik_tree::Value::State(*i)
        }
        LogValue::Bool(b) => kvik_tree::Value::Bool(*b),
        LogValue::Text(t) => kvik_tree::Value::Text(t.clone()),
        // Decimal series are refused at definition until roadmap 6.x.
        LogValue::Decimal { .. } | LogValue::Absent { .. } => return None,
    };
    Some((r.t, v))
}

/// Whether [`reading_of`] gives a reading, without building it.
pub(crate) fn has_reading(series: &Series, s: &Stored) -> bool {
    s.row.quality != LogQuality::Bad
        && match &s.row.value {
            LogValue::F64(_) | LogValue::Bool(_) | LogValue::Text(_) => true,
            LogValue::State(i) => series.def.state_names.get(usize::from(*i)).is_some(),
            LogValue::Decimal { .. } | LogValue::Absent { .. } => false,
        }
}

/// Whether a current fact can be a series' live-state fact at `now` (spec §2.3): not bad quality, and for an
/// observed interval series ended (`t + d ≤ now`).
pub(crate) fn may_be_live(series: &Series, p: &Params, s: &Stored, now: i64) -> bool {
    reading_of(series, s).is_some() && p.ready_at(s.row.t) <= now
}

/// Live state from its fact, with its staleness (spec §5.4): for a scheduled series when `now` is later than its
/// `t` by more than S; for an observed series when more than S has passed since its last refreshing commit in this
/// process, or else in event time ([`super::clock`]).
pub(crate) fn live_with(
    p: &Params,
    clocks: &super::clock::Clocks,
    id: SeriesId,
    s: &Stored,
    now: i64,
) -> Live {
    let stale = match (p.timing, p.staleness) {
        (_, None) => false,
        (km::registry::Timing::Observed, Some(limit)) => match clocks.since_refresh(id.0) {
            Some(d) => d > limit,
            None => now.saturating_sub(s.row.t) > limit,
        },
        (_, Some(limit)) => now.saturating_sub(s.row.t) > limit,
    };
    Live::Value(LiveValue {
        fact: fact_out(s),
        stale,
    })
}

/// A series of the registry with its summary parameters.
struct Of<'a> {
    index: &'a Index,
    series: &'a Series,
    params: Params,
}

impl<'a> Of<'a> {
    fn new(index: &'a Index, id: SeriesId) -> Result<Self, Error> {
        let series = index
            .registry()
            .get(id.0)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
        Ok(Of {
            index,
            series,
            params: params(&series.def),
        })
    }

    fn id(&self) -> u32 {
        self.series.def.id
    }

    /// The current facts with `t` between the bounds, in `t` order or in reverse.
    fn current(
        &self,
        from: Bound<i64>,
        to: Bound<i64>,
        rev: bool,
    ) -> impl Iterator<Item = &'a Stored> + 'a {
        let versioned = self.series.def.versioning != Versioning::None;
        self.index
            .current_between(self.id(), versioned, from, to, rev)
    }

    fn reading(&self, s: &Stored) -> Option<Reading> {
        reading_of(self.series, s)
    }

    /// The summarised readings with `t` in `[from, to)`.
    fn readings(&self, from: i64, to: i64) -> Vec<Reading> {
        if from >= to {
            return Vec::new();
        }
        self.current(Bound::Included(from), Bound::Excluded(to), false)
            .filter_map(|s| self.reading(s))
            .collect()
    }

    /// The last summarised reading before `t`: the current fact of the greatest such `t` that is not bad quality.
    /// The backward scan reads through a run of bad-quality current facts before `t`, so its cost is that run's
    /// length; nothing bounds it until the summary tree carries each node's last reading (roadmap 7.1).
    fn prior(&self, t: i64) -> Option<Reading> {
        if self.params.interval.is_some() {
            return None;
        }
        self.current(Bound::Unbounded, Bound::Excluded(t), true)
            .find_map(|s| self.reading(s))
    }

    /// A window `[start, end)` evaluated at `now`.
    fn window(&self, start: i64, end: i64, now: i64) -> Node {
        let readings = self.readings(fold::reach(&self.params, start), end);
        self.model(fold::window(
            &self.params,
            self.prior(start).as_ref(),
            &readings,
            start,
            end,
            now,
        ))
    }

    /// A node as the result rules take it.
    fn model(&self, n: kvik_tree::Node) -> Node {
        fold::model_node(n, &self.series.def.state_names)
    }
}

/// The live-state fact of a series (spec §2.3) in an index at `now`: the current fact with the greatest `t ≤ now`
/// that is not bad quality (for an observed interval series, `t + d ≤ now`); a forecast series has none.
pub(crate) fn live_fact(index: &Index, id: SeriesId, now: i64) -> Result<Option<&Stored>, Error> {
    let of = Of::new(index, id)?;
    let p = &of.params;
    if p.timing == km::registry::Timing::Forecast {
        return Ok(None);
    }
    // As for `prior`, the scan reads through bad-quality facts, and here also through readings of an observed
    // interval series not yet ready (at most d of event time).
    Ok(of
        .current(Bound::Unbounded, Bound::Included(now), true)
        .find(|s| may_be_live(of.series, p, s, now)))
}

/// Live state (spec §2.3) in an index at `now`, with its staleness ([`live_with`]).
pub(crate) fn live_in(
    index: &Index,
    clocks: &super::clock::Clocks,
    id: SeriesId,
    now: i64,
) -> Result<Live, Error> {
    let Some(s) = live_fact(index, id, now)? else {
        return Ok(Live::None);
    };
    let p = params(&Of::new(index, id)?.series.def);
    Ok(live_with(&p, clocks, id, s, now))
}

/// Live state (spec §2.3, §5.4) at the reader's `now`, from the series directory and the log tail
/// ([`super::read::latest::live`]).
pub(crate) fn live(r: &DurableReader, id: SeriesId) -> Result<Live, QueryError> {
    let now = r.now();
    r.with_latest(|v| super::read::latest::live(r, v, id, now))
}

/// A window result (spec §6), a calendar result among them (spec §5.7: a window over its span, in the site zone or
/// a named one).
pub(crate) fn window(
    r: &DurableReader,
    id: SeriesId,
    window: &Window,
    agg: &Aggregate,
) -> Result<WindowResult, QueryError> {
    let now = r.now();
    r.with(|i, _| {
        let of = Of::new(i, id)?;
        let a = rules::aggregate_for(&of.series.spec, of.params.kind, agg)?;
        let spec = Some(&of.series.spec);
        let (from, to) = match rules::plain_span(window, now)? {
            Some(span) => span,
            None => {
                let Window::Calendar { window, zone } = window else {
                    unreachable!("a window without a plain span is a calendar window")
                };
                let zoning = match zone {
                    Some(z) => km::calendar::Zoning::Named(z.as_str().to_string()),
                    None => km::calendar::Zoning::Site {
                        initial: r.created_zone().to_string(),
                        entries: i.registry().site_zones().clone(),
                    },
                };
                let (from, to, _) = km::calendar::window_span(
                    &zoning,
                    tzdb(),
                    rules::calendar_window(window)?,
                    now,
                )
                .map_err(|e| convert::error(km::engine::Error::Tree(e)))?;
                (from, to)
            }
        };
        let node = of.window(from, to, now);
        Ok(rules::window_result(
            spec,
            &node,
            &a,
            (from, to),
            false,
            false,
        ))
    })
}

/// A chart query: the raw current facts of the range when there are few enough, else buckets aligned to UTC, each
/// the window evaluation over its span (`docs/api.md` Reading).
pub(crate) fn buckets(
    r: &DurableReader,
    id: SeriesId,
    range: TimeRange,
    resolution: Resolution,
) -> Result<ChartData, QueryError> {
    let now = r.now();
    r.with(|i, _| {
        let of = Of::new(i, id)?;
        let (a, b) = rules::chart_range(range)?;
        let step = match resolution {
            Resolution::Auto { max_points } => {
                let max = max_points.max(1);
                let range = || of.current(Bound::Included(a), Bound::Excluded(b), false);
                // Counted before any is converted: a long range gives buckets.
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
