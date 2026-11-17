//! What the store answers from the deliverer's held state (`docs/api.md` Memory): series, losses, the latest `t`
//! of a series and the clean shutdown's registry file, without an index of every fact.

use kvik_log::writer::Position as LogPos;

use super::{Ctx, Deliverer, load_part};
use crate::backend::Error;

impl Deliverer {
    /// Takes what is acknowledged and makes the summaries of facts exact: rebuilt by a transient read of the log in
    /// parts after a void ([`crate::durable::index::read_summary`]). Returns whether they are, and the index holds
    /// every batch the writer had acknowledged when the call began; otherwise the store's own reader answers, with
    /// its read error if the log cannot be read.
    pub(crate) fn held(&mut self, ctx: &Ctx<'_>) -> bool {
        if self.reader.get_mut().is_none() {
            return self.closed_complete && !self.index.summary_stale();
        }
        let target = ctx.watch.map(|w| w.stats().durable);
        self.pump(ctx);
        let Some(through) =
            target.or_else(|| self.reader.get_mut().as_ref().map(|r| r.state().durable))
        else {
            return false;
        };
        if !self.taken_through(through) {
            return false;
        }
        if self.index.summary_stale()
            && let Ok(s) = crate::durable::index::read_summary(
                || crate::durable::start::log_reader(ctx.dir, ctx.env),
                load_part(ctx.dir),
            )
        {
            self.index.set_summary(s);
        }
        !self.index.summary_stale()
    }

    /// Whether every batch up to `through` is taken: the reader has read that far and none at or below it waits.
    pub(super) fn taken_through(&mut self, through: LogPos) -> bool {
        self.reader
            .get_mut()
            .as_ref()
            .is_some_and(|r| r.state().durable >= through)
            && self.pending.front().is_none_or(|b| b.last > through)
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

    /// The clean shutdown's registry file (`format.md` A.16): up to the mark it synced, once every batch up to it is
    /// taken; `None` if the log could not be read that far.
    pub(crate) fn registry_file(
        &mut self,
        ctx: &Ctx<'_>,
        floor: u32,
    ) -> Option<kvik_log::registry::RegistryFile> {
        self.pump(ctx);
        let through = self.reader.get_mut().as_ref()?.state().durable;
        let synced = ctx.watch.map_or(through, |w| w.stats().mark_synced);
        let file = self.registry_file_through(through.max(synced), through, floor);
        self.closed_complete = file.is_some();
        file
    }

    /// The registry file after a routing change (Q-130), up to the last synced mark `through`, once every batch up to
    /// it is taken; `None` otherwise.
    pub(crate) fn routing_registry_file(
        &mut self,
        ctx: &Ctx<'_>,
        through: LogPos,
        floor: u32,
    ) -> Option<kvik_log::registry::RegistryFile> {
        self.pump(ctx);
        self.registry_file_through(through, through, floor)
    }

    /// The registry file up to `through` once every batch up to `taken` is taken.
    pub(super) fn registry_file_through(
        &mut self,
        taken: LogPos,
        through: LogPos,
        floor: u32,
    ) -> Option<kvik_log::registry::RegistryFile> {
        self.taken_through(taken)
            .then(|| self.index.registry_file(through, floor))
    }
}
