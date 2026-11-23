//! The views a reader's calls run on: taken from the single-flight round or kept while fresh (Q-902, Q-1007), and
//! checked at the call's end against a live cut (Q-853, Q-903).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kvik_log::lease::CutEnd;
use kvik_log::reader::ReaderError;

use super::errors::unshare;
use super::{DurableReader, query_error};
use crate::backend::Error;
use crate::durable::index::Index;
use crate::durable::read::cut;
use crate::durable::read::view::View;
use crate::error::QueryError;

impl DurableReader {
    /// A view read after this call began ([`Flight`]).
    pub(super) fn view(&self) -> Result<Arc<View>, QueryError> {
        let v = self.flight.get(|current| self.round(current, false));
        self.hub.ring_pending();
        v.map_err(unshare)
    }

    /// [`DurableReader::view`] for a call that reads the latest values only: in the writer's process, a view at the
    /// writer's durable position is kept rather than read again (Q-1007). A call that reads facts reads the files,
    /// and with them checks again the last batch its log reader returned.
    pub(super) fn view_latest(&self) -> Result<Arc<View>, QueryError> {
        // The round's quick path without its lock: live sessions woken by one round take its view side by side.
        if let Some(v) = self
            .flight
            .current()
            .filter(|v| self.fresh(v, self.monotonic()))
        {
            return Ok(v);
        }
        let v = self.flight.get(|current| self.round(current, true));
        self.hub.ring_pending();
        v.map_err(unshare)
    }

    /// A view read after this call began that holds every fact: a later round's if the first does not, the fact
    /// index then opened (Q-1000).
    pub(super) fn view_with_facts(&self) -> Result<Arc<View>, QueryError> {
        let v = self.view()?;
        if v.index().is_some() {
            return Ok(v);
        }
        self.want_facts.store(true, Ordering::SeqCst);
        drop(v);
        self.release_dropped();
        let v = self.view()?;
        if v.index().is_none() {
            return Err(QueryError::CutExpired);
        }
        Ok(v)
    }

    /// Takes a view read after this call began that holds every fact, then runs `f` on it with no lock under its cut:
    /// a materialised call that reads facts ([`cut`]).
    pub(crate) fn with<R>(
        &self,
        f: impl FnOnce(&Index, &View) -> Result<R, Error>,
    ) -> Result<R, QueryError> {
        let v = self.view_with_facts()?;
        let r = f(v.index().expect("a view with facts"), &v).map_err(Error::query);
        self.end(v)?;
        r
    }

    /// [`DurableReader::with`] for a call that reads the latest values, the registry or the losses, which no fact
    /// index holds up; a broken state's view holds one (Q-1006).
    pub(crate) fn with_latest<R>(
        &self,
        f: impl FnOnce(&View) -> Result<R, QueryError>,
    ) -> Result<R, QueryError> {
        let v = self.snapshot()?;
        let r = f(&v);
        self.end(v)?;
        r
    }

    /// A view for the latest values, a snapshot's among them (Q-722, Q-1005): read after the call began, with every
    /// fact if its state is broken.
    pub(crate) fn snapshot(&self) -> Result<Arc<View>, QueryError> {
        let v = self.view_latest()?;
        if v.latest().broken() && v.index().is_none() {
            drop(v);
            return self.view_with_facts();
        }
        Ok(v)
    }

    /// The view of the last round, with no refresh, unless it lacks the fact index its broken state needs.
    pub(crate) fn current(&self) -> Option<Arc<View>> {
        self.flight
            .current()
            .filter(|v| !v.latest().broken() || v.index().is_some())
    }

    /// Ends a call on `v`: whether its cut is still live, and the release of views no call holds.
    pub(crate) fn end(&self, v: Arc<View>) -> Result<(), QueryError> {
        let live = self.check(&v);
        drop(v);
        self.release_dropped();
        live
    }

    /// In the writer's process, whether `v` is at the writer's durable position with every closed range taken: what a
    /// read of the files would publish again (the manifest changes only at a start, Q-704), kept while young enough
    /// that no lease renewal is due at `now` (the query design's S1).
    pub(super) fn fresh(&self, v: &View, now: Duration) -> bool {
        self.writer.as_ref().is_some_and(|watch| {
            !watch.stopped()
                && watch.durable() == v.state().durable
                && self.latch.closed_count() == v.closed_taken
                && (v.index().is_some() || !self.want_facts.load(Ordering::SeqCst))
                && v.age(now) < self.lease_refresh / 2
        })
    }

    /// Whether the call on `v` ends under a live cut ([`View::live`]). A call as old as half `lease_refresh` may
    /// have outlived a lease no refresh renewed since: the log reader's check then renews it when due and finds it
    /// gone (Q-853, Q-903).
    pub(crate) fn check(&self, v: &View) -> Result<(), QueryError> {
        let now = self.monotonic();
        if !v.live(now, self.cut_max_age) {
            return Err(QueryError::CutExpired);
        }
        if v.age(now) < self.lease_refresh / 2 {
            return Ok(());
        }
        let mut w = self.lock();
        let Some(log) = w.log.as_mut() else {
            return Err(QueryError::Closed);
        };
        let r = cut::check(log, v.cut());
        if let Err(ReaderError::CutEnded(CutEnd::LeaseLost)) = r {
            self.cuts.lose_all();
        }
        r.map_err(|e| query_error(&self.dir, e))
    }

    /// [`DurableReader::with`], called once more if the first call's cut ended: for the store's own reads, which
    /// have no caller to start again.
    pub(crate) fn with_again<R>(
        &self,
        mut f: impl FnMut(&Index, &View) -> Result<R, Error>,
    ) -> Result<R, QueryError> {
        match self.with(&mut f) {
            Err(QueryError::CutExpired) => self.with(f),
            other => other,
        }
    }

    /// [`DurableReader::with_latest`], called once more if the first call's cut ended.
    pub(crate) fn with_latest_again<R>(
        &self,
        mut f: impl FnMut(&View) -> Result<R, QueryError>,
    ) -> Result<R, QueryError> {
        match self.with_latest(&mut f) {
            Err(QueryError::CutExpired) => self.with_latest(f),
            other => other,
        }
    }
}
