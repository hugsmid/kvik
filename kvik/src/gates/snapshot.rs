//! The whole core as a value, and the core back from it.

use std::collections::BTreeMap;

use kvik_model::gate::{ConditionId, Gate};
use kvik_model::types::SeriesId;

use super::engine::{Frontier, bring_up};
use super::rules::Inst;
use super::{Core, History, SeriesState};

/// A core's instances and series at its frontier. Facts ahead are not kept: [`Core::restore`] reads them
///
/// again.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub(super) series: BTreeMap<SeriesId, SeriesState>,
    pub(super) instances: BTreeMap<(ConditionId, SeriesId), Inst>,
    pub(super) frontier: Frontier,
}

impl Core {
    /// The core's state now.
    pub fn snapshot(&self) -> Snapshot {
        let now = self.frontier.now;
        let series = self
            .series
            .iter()
            .map(|(s, st)| {
                let mut st = st.clone();
                bring_up(&mut st, now);
                st.ahead.clear();
                (*s, st)
            })
            .collect();
        Snapshot {
            series,
            instances: self.instances.clone(),
            frontier: self.frontier,
        }
    }

    /// The core a snapshot was taken of, with `defs` the gates in force at its frontier.
    pub fn restore(
        snap: Snapshot,
        defs: BTreeMap<ConditionId, Gate>,
        history: &dyn History,
    ) -> Self {
        let Snapshot {
            mut series,
            instances,
            frontier,
        } = snap;
        for (s, st) in &mut series {
            st.ahead = history.series(*s, frontier.now, frontier.at).ahead;
        }
        let mut core = Core {
            defs,
            series,
            instances,
            frontier,
            ..Core::new()
        };
        let keys: Vec<(ConditionId, SeriesId)> = core.instances.keys().copied().collect();
        for k in keys {
            core.readers.entry(k.1).or_default().insert(k.0);
            core.reschedule(k);
        }
        core
    }
}
