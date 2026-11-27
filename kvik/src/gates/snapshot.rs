//! The whole core as a value, and the core back from it.

use std::collections::BTreeMap;

use kvik_model::gate::{ConditionId, Gate, Selector};
use kvik_model::types::SeriesId;

use super::engine::{Frontier, bring_up};
use super::rules::Inst;
use super::{Core, CoreError, History, SeriesState};

/// A core's instances and series at its frontier. Facts ahead are not kept: [`Core::restore`] reads them
/// again.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub(super) series: BTreeMap<SeriesId, SeriesState>,
    pub(super) instances: BTreeMap<(ConditionId, SeriesId), Inst>,
    pub(super) frontier: Frontier,
}

impl Snapshot {
    /// The frontier it was taken at.
    pub fn frontier(&self) -> Frontier {
        self.frontier
    }
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
                // A no-op while every usable fact ahead has a timer at its `t` (`rules::next_due`); kept so a
                // fact due by `now` is never cleared with the rest.
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
    ) -> Result<Self, CoreError> {
        let Snapshot {
            mut series,
            instances,
            frontier,
        } = snap;
        for (s, st) in &mut series {
            st.ahead = history.series(*s, frontier.now, frontier.at)?.ahead;
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
        // An `Each` gate's refusals are every series its pattern matches beyond its instances (Q-1143).
        let each: Vec<(ConditionId, String)> = core
            .defs
            .iter()
            .filter_map(|(id, g)| match &g.selector {
                Selector::Each(p) => Some((*id, p.clone())),
                Selector::Series(_) => None,
            })
            .collect();
        for (id, p) in each {
            let refused = history
                .matching(&p, frontier.at)?
                .into_iter()
                .filter(|s| !core.instances.contains_key(&(id, *s)))
                .collect();
            core.refused.insert(id, refused);
        }
        Ok(core)
    }
}
