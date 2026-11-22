//! `facts` and `at` from the sources (roadmap 9.0f, the query design's §2.2): no fact index, one cut per call, the
//! memory cap held by passes; as of a time through the log's commit times (spec §2.3).

use std::sync::Arc;

use super::DurableReader;
use crate::durable::index::log_pos;
use crate::durable::read::latest;
use crate::durable::read::source::{FactStream, Plan, search_back};
use crate::durable::read::view::View;
use crate::error::QueryError;
use crate::read::{Facts, View as ReadView};
use crate::series::SeriesKey;
use crate::time::{TimeRange, Timestamp};
use crate::types::{Fact, SeriesId};

/// What a read of `ids` in `view` selects at `v`'s cut. As of a time meets damage as a pass does: the scanning read
/// finds it first, and the search runs once more (Q-1064).
pub(crate) fn plan(
    r: &DurableReader,
    v: &View,
    ids: &[SeriesId],
    view: ReadView,
) -> Result<Plan, QueryError> {
    let mut series = Vec::with_capacity(ids.len());
    for id in ids {
        let info = latest::series(v, &SeriesKey::Id(*id))?;
        series.push((id.0, info.spec.versioned.is_some()));
    }
    series.sort_unstable();
    series.dedup();
    let cut = v.cut().durable;
    let bound = match view {
        ReadView::History | ReadView::Current => cut,
        ReadView::AsOf(p) => log_pos(p).min(cut),
        ReadView::AsOfTime(t) => match r.position_at(v, t.0) {
            Err(QueryError::NotAvailable { .. }) => {
                r.find_losses()?;
                r.position_at(v, t.0)?
            }
            other => other?,
        },
    };
    Ok(Plan {
        series,
        history: matches!(view, ReadView::History),
        bound,
    })
}

/// The facts of several series with `t` in `range`, in a view, streamed at one cut in event-time order, then series,
/// then position (Q-1062).
pub(crate) fn facts_of(
    r: Arc<DurableReader>,
    ids: &[SeriesId],
    range: TimeRange,
    view: ReadView,
) -> Result<FactStream, QueryError> {
    let v = r.snapshot()?;
    let plan = match plan(&r, &v, ids, view) {
        Ok(p) => p,
        Err(e) => {
            drop(v);
            r.release_dropped();
            return Err(e);
        }
    };
    let (from, to) = if range.from.0 < range.to.0 {
        (range.from.0, range.to.0 - 1)
    } else {
        (0, -1)
    };
    Ok(FactStream::new(r, v, plan, from, to))
}

/// The facts of a series with `t` in `range`, in a view (spec §2.3), streamed.
pub(crate) fn facts(
    r: Arc<DurableReader>,
    id: SeriesId,
    range: TimeRange,
    view: ReadView,
) -> Result<Facts, QueryError> {
    facts_of(r, &[id], range, view).map(Facts::stream)
}

/// The latest fact at or before `t` that a window can credit (not bad quality), in a view, ignoring the staleness
/// limit; read once more under a new cut if a reset ended the first (the query design's §3.2 rule 3).
pub(crate) fn at(
    r: &DurableReader,
    id: SeriesId,
    t: Timestamp,
    view: ReadView,
) -> Result<Option<Fact>, QueryError> {
    let once = || {
        r.with_latest(|v| {
            let plan = plan(r, v, &[id], view)?;
            search_back(r, v, &plan, t.0, &|f| {
                f.quality != crate::types::Quality::Bad
            })
        })
    };
    match once() {
        Err(QueryError::CutExpired) => once(),
        other => other,
    }
}
