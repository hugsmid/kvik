//! The fast path's hooks into the model (§4 Provisional streams): calls accepted and settled, the durable mark and
//! the writer's end, and the streams opened and polled.

use std::collections::BTreeSet;

use super::{Entry, Error, Model};
use crate::provisional::{Event as ProvisionalEvent, Outcome as CallOutcome};
use crate::types::{Fact, Position, SeriesId};

impl Model {
    pub(super) fn fast_accept(&mut self, batch: &[Entry]) -> u64 {
        let facts = batch
            .iter()
            .filter_map(Entry::fact)
            .map(|f| (f.series, f.clone()))
            .collect();
        self.provisional.last_mut().map_or(0, |p| p.accept(facts))
    }

    pub(super) fn fast_settle(&mut self, seq: u64, outcome: CallOutcome) {
        if let Some(p) = self.provisional.last_mut() {
            p.settle(seq, outcome);
        }
    }

    /// Tells the running store's fast path the durable mark and whether the writer has failed; `stopped` when the
    /// process stops, before a power loss can move the mark back.
    pub(super) fn fast_signal(&mut self, stopped: bool) {
        let mark = self.disk.mark.map_or(Position::ORIGIN, |m| m.last);
        let ended = stopped || self.vol.as_ref().is_none_or(|v| v.read_only);
        if let Some(p) = self.provisional.last_mut() {
            p.signal(mark, ended, stopped);
        }
    }

    /// Opens a provisional stream on the running store (§4 Provisional streams): its start and its id. `None` selects every
    /// series.
    pub fn provisional_open(
        &mut self,
        series: Option<BTreeSet<SeriesId>>,
    ) -> Result<(usize, u64), Error> {
        self.vol()?;
        let run = self.provisional.len() - 1;
        Ok((run, self.provisional[run].open(series)))
    }

    /// Every event due to a provisional stream now.
    pub fn provisional_poll(&mut self, (run, id): (usize, u64)) -> Vec<ProvisionalEvent<Fact>> {
        if run + 1 == self.provisional.len() && self.vol.is_some() {
            self.fast_signal(false);
        }
        self.provisional
            .get_mut(run)
            .map_or_else(Vec::new, |p| p.poll(id))
    }
}
