//! What the store answers from the deliverer's held state (`docs/api.md` Memory): series, losses, the latest `t`
//! of a series and the clean shutdown's registry file, without an index of every fact.

use super::{Ctx, Deliverer, LOAD_PART};
use crate::backend::Error;

impl Deliverer {
    /// Takes what is acknowledged and makes the summaries of facts exact: rebuilt by a transient read of the log in
    /// parts after a void ([`crate::durable::index::read_summary`]). Returns whether they are.
    pub(crate) fn held(&mut self, ctx: &Ctx<'_>) -> bool {
        self.pump(ctx);
        if self.index.summary_stale()
            && let Ok(s) = crate::durable::index::read_summary(
                || crate::durable::start::log_reader(ctx.dir, ctx.env),
                LOAD_PART,
            )
        {
            self.index.set_summary(s);
        }
        !self.index.summary_stale()
    }

    /// What the registry and the summaries of the log taken know about a series ([`Deliverer::held`] first).
    pub(crate) fn series(
        &self,
        key: &crate::series::SeriesKey,
    ) -> Result<crate::series::SeriesInfo, Error> {
        self.index.series(key)
    }

    /// The latest recorded `t` of a series in the log taken ([`Deliverer::held`] first).
    pub(crate) fn latest_t(&self, id: u32) -> Option<i64> {
        self.index.latest_t(id)
    }

    /// The losses the files report: the manifest's lost ranges and the recovery entries of the log taken.
    pub(crate) fn losses(&self) -> Vec<crate::health::LossReport> {
        match self.reader.borrow().as_ref() {
            Some(r) => self.index.losses(r.lost_ranges()),
            None => self.index.losses(&self.closed_lost),
        }
    }

    /// The registry file's content up to the mark the writer last synced (`format.md` A.16), once every batch up to it
    /// is taken; `None` if the log could not be read that far.
    pub(crate) fn registry_file(
        &mut self,
        ctx: &Ctx<'_>,
        floor: u32,
    ) -> Option<kvik_log::registry::RegistryFile> {
        self.pump(ctx);
        let through = self.reader.get_mut().as_ref()?.state().durable;
        let reached = ctx.watch.is_none_or(|w| through >= w.stats().mark_synced);
        (reached && self.pending.is_empty()).then(|| self.index.registry_file(through, floor))
    }
}
