//! The rules for windows, calendar results and chart buckets that both backends share (spec §5.7, §6;
//! `docs/api.md` Reading): the conversion of a window's node into its result, the aggregates refused per kind, window
//! spans, and the steps, spans and contents of chart buckets.

use kvik_model as km;

use crate::backend::convert::{self, dur, dur_nanos, nanos, ts};
use crate::backend::{Error, Result};
use crate::read::{
    Aggregate, Bucket, CalendarPeriod, CalendarWindow, ResultFlags, Window, WindowResult,
};
use crate::series::{Kind, SeriesSpec};
use crate::time::{Span, TimeRange};
use crate::types::Value;

/// A window result from the node of its evaluation (spec §6), as both backends give it.
pub(crate) fn window_result(
    spec: Option<&SeriesSpec>,
    node: &km::summary::Node,
    agg: &km::summary::Aggregate,
    span: (i64, i64),
    restoring: bool,
    summary_stale: bool,
) -> WindowResult {
    let (from, to) = span;
    let width = i128::from(to) - i128::from(from);
    WindowResult {
        value: node.aggregate(agg).map(|v| convert::aggregated(&v, spec)),
        count: node.count,
        coverage: dur(node.coverage),
        coverage_fraction: if width > 0 {
            node.coverage as f64 / width as f64
        } else {
            0.0
        },
        from: ts(from),
        to: ts(to),
        flags: ResultFlags {
            restoring,
            apportioned: node.apportioned(),
            summary_stale,
            rate_flagged: matches!(agg, km::summary::Aggregate::Rate) && node.rate_flagged(),
            ..ResultFlags::default()
        },
    }
}

/// The aggregate to evaluate on a series of `kind`, refused where spec §6 refuses it.
pub(crate) fn aggregate_for(
    spec: &SeriesSpec,
    kind: km::types::Kind,
    agg: &Aggregate,
) -> Result<km::summary::Aggregate> {
    let a = convert::aggregate(agg, Some(spec))?;
    if km::summary::refused(kind, &a) {
        return Err(Error::AggregateRefused(
            "avg, integral, increase and time-in-state are refused on an interval series".into(),
        ));
    }
    Ok(a)
}

/// The span of a rolling or absolute window at `now`; `None` for a calendar window, whose span the calendar gives.
pub(crate) fn plain_span(window: &Window, now: i64) -> Result<Option<(i64, i64)>> {
    let span = match window {
        Window::Relative { start, end } => {
            let at = |d: Span| {
                now.checked_add(d.0)
                    .ok_or_else(|| Error::InvalidQuery("window outside the time range".into()))
            };
            (at(*start)?, at(*end)?)
        }
        Window::Absolute(r) => (nanos(r.from), nanos(r.to)),
        Window::Calendar { .. } => return Ok(None),
    };
    if span.0 >= span.1 {
        return Err(Error::InvalidQuery(
            "a window's start must be before its end".into(),
        ));
    }
    Ok(Some(span))
}

/// A calendar window as the model takes it.
pub(crate) fn calendar_window(w: &CalendarWindow) -> Result<km::calendar::CalendarWindow> {
    Ok(match w {
        CalendarWindow::SinceMidnight => km::calendar::CalendarWindow::SinceMidnight,
        CalendarWindow::ThisMonth => km::calendar::CalendarWindow::ThisMonth,
        CalendarWindow::Day(k) => km::calendar::CalendarWindow::Day(*k),
        CalendarWindow::Month(k) => km::calendar::CalendarWindow::Month(*k),
        CalendarWindow::Period(p) => km::calendar::CalendarWindow::Period(model_period(*p)?),
    })
}

/// The bounds of a chart range, refused when empty.
pub(crate) fn chart_range(range: TimeRange) -> Result<(i64, i64)> {
    let (a, b) = (nanos(range.from), nanos(range.to));
    if a >= b {
        return Err(Error::InvalidQuery(
            "a range's start must be before its end".into(),
        ));
    }
    Ok((a, b))
}

/// The bucket width `Resolution::Auto` picks for more than `max_points` raw facts: 1 minute, 15 minutes or a
/// multiple of 15 minutes, whichever first gives at most `max_points` buckets aligned to UTC.
pub(crate) fn auto_step(a: i64, b: i64, max_points: usize) -> i64 {
    const MINUTE: i64 = 60_000_000_000;
    const QUARTER: i64 = 15 * MINUTE;
    let max = i64::try_from(max_points.max(1)).unwrap_or(i64::MAX);
    let span = b - a;
    // Buckets are aligned to UTC, so they are counted from the aligned floor of the start, not from the start.
    let buckets = |step: i64| {
        let first = a.div_euclid(step) * step;
        (b - first + step - 1) / step
    };
    if buckets(MINUTE) <= max {
        MINUTE
    } else if buckets(QUARTER) <= max {
        QUARTER
    } else {
        let mut k = ((span + QUARTER - 1) / QUARTER + max - 1) / max;
        while buckets(QUARTER * k) > max {
            k += 1;
        }
        QUARTER * k
    }
}

/// The bucket width of `Resolution::Step`, refused when not positive or above 1,000,000 buckets.
pub(crate) fn fixed_step(d: std::time::Duration, a: i64, b: i64) -> Result<i64> {
    let step = dur_nanos(d)?;
    if step <= 0 || (b - a) / step > 1_000_000 {
        return Err(Error::InvalidQuery(
            "a step must be positive and give at most 1,000,000 buckets".into(),
        ));
    }
    Ok(step)
}

/// The buckets of `[a, b)` at width `step`, aligned to UTC and clipped to the range.
pub(crate) fn bucket_spans(a: i64, b: i64, step: i64) -> impl Iterator<Item = (i64, i64)> {
    let first = a.div_euclid(step) * step;
    std::iter::successors(Some(first), move |t| Some(t + step))
        .take_while(move |t| *t < b)
        .map(move |t| (t.max(a), (t + step).min(b)))
}

/// A chart bucket from the window evaluation over its span.
pub(crate) fn bucket(
    spec: &SeriesSpec,
    node: &km::summary::Node,
    lo: i64,
    hi: i64,
    restoring: bool,
) -> Bucket {
    let num = |agg: km::summary::Aggregate| node.aggregate(&agg).and_then(|v| v.to_f64());
    let numeric = !matches!(spec.kind, Kind::State { .. } | Kind::Text);
    // Min and max are over readings that start in the bucket only; the mean can come from a value carried in.
    let readings = numeric && node.count > 0;
    Bucket {
        from: ts(lo),
        to: ts(hi),
        count: node.count,
        min: readings
            .then(|| num(km::summary::Aggregate::Min))
            .flatten()
            .map(Value::F64),
        max: readings
            .then(|| num(km::summary::Aggregate::Max))
            .flatten()
            .map(Value::F64),
        mean: matches!(spec.kind, Kind::Gauge { .. })
            .then(|| num(km::summary::Aggregate::Avg))
            .flatten(),
        coverage: dur(node.coverage),
        flags: ResultFlags {
            restoring,
            apportioned: node.apportioned(),
            ..ResultFlags::default()
        },
    }
}

pub(crate) fn model_period(p: CalendarPeriod) -> Result<km::calendar::Period> {
    Ok(match p {
        CalendarPeriod::Day(d) => km::calendar::Period::Day(
            d.to_jiff()
                .ok_or_else(|| Error::InvalidQuery(format!("no date {d}")))?,
        ),
        CalendarPeriod::Month { year, month } => {
            let month = i8::try_from(month)
                .ok()
                .filter(|m| (1..=12).contains(m))
                .ok_or_else(|| Error::InvalidQuery("a month is 1 to 12".into()))?;
            km::calendar::Period::Month(km::calendar::Month { year, month })
        }
    })
}
