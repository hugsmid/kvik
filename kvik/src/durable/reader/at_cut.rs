//! The reads a reader makes at a view's cut because its latest values cannot tell (Q-1004): lookups by event time
//! and by position, the candidates' full facts read once, and the live facts searched for, kept while they hold.

use std::collections::BTreeMap;
use std::sync::PoisonError;
use std::sync::atomic::Ordering;

use kvik_log::mark::ResetStamp as LogStamp;
use kvik_log::reader::{FoundFact, LogReader, Lookup, ReaderError, Wanted, WantedAt, Within};
use kvik_log::writer::Position as LogPos;

use super::{DurableReader, query_error};
use crate::durable::derived::state::Kept;
use crate::durable::index::Stored;
use crate::durable::read::latest::{self, Searched};
use crate::durable::read::view::View;
use crate::error::QueryError;

impl DurableReader {
    /// The facts in the `wanted` ranges at `v`'s cut, read from the log: the regions the manifest records or the
    /// process found are lost, any other damage or a segment that cannot be read is `NotAvailable`, and a mark under
    /// another reset stamp or generation than the cut's ends it.
    pub(crate) fn lookup(&self, v: &View, wanted: &[Wanted]) -> Result<Vec<FoundFact>, QueryError> {
        self.at_cut(v, |r, lost| r.lookup(wanted, lost))
    }

    /// [`DurableReader::lookup`] within `within`, with the segments listed; no facts when they would pass its
    /// budget.
    pub(crate) fn lookup_within(
        &self,
        v: &View,
        wanted: &[Wanted],
        within: Within,
    ) -> Result<(Option<Vec<FoundFact>>, Vec<u64>), QueryError> {
        let mut l = self.read_at_cut(
            v,
            |r, lost| r.lookup_within(wanted, lost, within),
            |l| (l.stamp, l.durable),
        )?;
        #[cfg(test)]
        if let Some(hook) = self
            .after_lookup
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            hook();
        }
        let listed = std::mem::take(&mut l.listed);
        if l.overflowed {
            return Ok((None, listed));
        }
        Ok((Some(complete(l, v)?), listed))
    }

    /// The last position at or below `v`'s cut whose batch was committed at or before `at` (`spec.md` §2.3 As-of
    /// time); the origin when there is none.
    pub(crate) fn position_at(&self, v: &View, at: i64) -> Result<LogPos, QueryError> {
        let f = self.read_at_cut(
            v,
            |r, lost| r.position_at(at, lost),
            |f| (f.stamp, f.durable),
        )?;
        if !f.complete {
            return Err(unreadable());
        }
        Ok(f.position.unwrap_or_default().min(v.cut().durable))
    }

    /// The facts at the positions `wanted` names, at `v`'s cut, each read from the one segment that holds it, as
    /// [`DurableReader::lookup`] reads.
    pub(crate) fn lookup_at(
        &self,
        v: &View,
        wanted: &[WantedAt],
    ) -> Result<Vec<FoundFact>, QueryError> {
        self.at_cut(v, |r, lost| r.lookup_at(wanted, lost))
    }

    fn at_cut(
        &self,
        v: &View,
        read: impl FnOnce(&mut LogReader, &dyn Fn(u64, u64, u64) -> bool) -> Result<Lookup, ReaderError>,
    ) -> Result<Vec<FoundFact>, QueryError> {
        let l = self.read_at_cut(v, read, |l| (l.stamp, l.durable))?;
        complete(l, v)
    }

    fn read_at_cut<T>(
        &self,
        v: &View,
        read: impl FnOnce(&mut LogReader, &dyn Fn(u64, u64, u64) -> bool) -> Result<T, ReaderError>,
        mark: impl FnOnce(&T) -> (LogStamp, LogPos),
    ) -> Result<T, QueryError> {
        let mut g = self.lookups.lock().unwrap_or_else(PoisonError::into_inner);
        if g.is_none() {
            *g = Some(
                self.second_reader()
                    .map_err(|e| query_error(&self.dir, e))?,
            );
        }
        let reader = g.as_mut().expect("opened above");
        #[cfg(test)]
        self.lookups_made.fetch_add(1, Ordering::Relaxed);
        let recorded = v.lost_ranges();
        let skip = |file_id: u64, start: u64, end: u64| {
            recorded.iter().any(|l| {
                l.file_id == file_id
                    && l.offset <= start
                    && end <= l.offset.saturating_add(l.length)
            }) || self.latch.covers(file_id, start, end)
        };
        let l = read(reader, &skip).map_err(|e| query_error(&self.dir, e))?;
        let cut = v.cut();
        let (stamp, durable) = mark(&l);
        #[cfg(test)]
        if self
            .expire_lookups
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(QueryError::CutExpired);
        }
        if stamp != cut.stamp
            || durable.generation != cut.durable.generation
            || durable < cut.durable
        {
            return Err(QueryError::CutExpired);
        }
        Ok(l)
    }

    /// The full fact of series `id`'s candidate `k`, which the fold did not meet: read at its position once, with
    /// every such candidate of `v` the first time.
    pub(crate) fn resolved(
        &self,
        v: &View,
        id: u32,
        k: Kept,
        now: i64,
    ) -> Result<Option<Stored>, QueryError> {
        let hit = |rows: &BTreeMap<u32, Stored>| {
            rows.get(&id).filter(|s| s.position == k.position).cloned()
        };
        if let Some(s) = hit(&self.rows.lock().unwrap_or_else(PoisonError::into_inner)) {
            return Ok(Some(s));
        }
        let first = !self.rows_read.swap(true, Ordering::SeqCst);
        let read = latest::rows_for(self, v, id, k, now, first)?;
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        rows.extend(read);
        Ok(hit(&rows))
    }

    /// Counts a live value found in the log because a state could not tell.
    pub(crate) fn count_search(&self) {
        self.searches.fetch_add(1, Ordering::Relaxed);
    }

    /// The live fact last searched for in series `id`, if any.
    pub(crate) fn searched(&self, id: u32) -> Option<Searched> {
        self.searched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Keeps a live fact searched for in series `id`.
    pub(crate) fn keep_searched(&self, id: u32, s: Searched) {
        self.searched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, s);
    }
}

fn unreadable() -> QueryError {
    QueryError::NotAvailable {
        reason: "a log segment that may hold the series cannot be read".into(),
    }
}

/// The facts of a complete lookup at or below `v`'s cut; an incomplete one is `NotAvailable` (Q-1004).
fn complete(l: Lookup, v: &View) -> Result<Vec<FoundFact>, QueryError> {
    if l.complete.iter().any(|c| !c) {
        return Err(unreadable());
    }
    let durable = v.cut().durable;
    Ok(l.facts
        .into_iter()
        .filter(|f| f.position <= durable)
        .collect())
}
