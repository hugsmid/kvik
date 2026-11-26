//! Exact resume: the transitions a consumer receives again after a restart (spec §6 Gates, Resume).

use std::collections::BTreeMap;

use super::{ConditionId, Entry, GateError, GateState, Transition, TransitionId, judge};
use crate::types::{DAY, Nanos, Position, SeriesId};

/// The point after every transition with `at` at or below `at` and due at or before `now`: where a
/// checkpoint or a confirmed resync leaves the judgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frontier {
    /// Last position applied.
    pub at: Position,
    /// Commit-clock instant reached.
    pub now: Nanos,
}

/// A point in [`TransitionId`] order: a consumer's gate cursor (Q-1391).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatePoint {
    /// After this transition, the last one confirmed.
    Transition(TransitionId),
    /// After a frontier, the one a confirmed resync was built at.
    Frontier(Frontier),
}

impl Frontier {
    /// Whether the transition `id` is at or before this frontier.
    pub fn covers(&self, id: &TransitionId) -> bool {
        id.at <= self.at && id.due.is_none_or(|d| d <= self.now)
    }
}

impl GatePoint {
    /// Whether the transition `id` is at or before this point.
    pub fn covers(&self, id: &TransitionId) -> bool {
        match self {
            GatePoint::Transition(t) => id <= t,
            GatePoint::Frontier(f) => f.covers(id),
        }
    }
}

/// How far back a resume replays (`conditions.replay_max`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayBound {
    /// Most commit-clock time from the replay's start to the resume.
    pub span: Nanos,
    /// Most facts after the replay's start.
    pub facts: u64,
}

impl Default for ReplayBound {
    fn default() -> Self {
        ReplayBound {
            span: DAY,
            facts: 1_000_000,
        }
    }
}

/// What a consumer receives when it resumes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resume {
    /// Transitions after the replay's start and the gate cursor, flagged `replayed`, in delivery order.
    pub replayed: Vec<Transition>,
    /// State now per instance and the id that set it, carried by the resync that ends the resume.
    pub states: BTreeMap<(ConditionId, SeriesId), (GateState, TransitionId)>,
    /// The replay was beyond `bound`: nothing replayed, the resync alone (`transitions_not_replayed`).
    pub not_replayed: bool,
}

/// The resume, at commit-clock instant `until`, of a consumer whose replay starts at `start` (its gate
/// checkpoint, at or after its gate cursor) and whose gate cursor is `cursor` (`None`: nothing confirmed
/// yet). The bound is measured from `start`, so a consumer whose gates are quiet for days still replays
/// (Q-1391).
pub fn resume(
    history: &[Entry],
    until: Nanos,
    start: Frontier,
    cursor: Option<GatePoint>,
    bound: ReplayBound,
) -> Result<Resume, GateError> {
    let j = judge(history, until)?;
    let facts = history
        .iter()
        .filter(|e| matches!(e, Entry::Fact { .. }) && e.position() > start.at)
        .count();
    let not_replayed = u64::try_from(facts).unwrap_or(u64::MAX) > bound.facts
        || until.saturating_sub(start.now) > bound.span;
    let replayed = if not_replayed {
        Vec::new()
    } else {
        j.transitions
            .into_iter()
            .filter(|t| !start.covers(&t.id) && cursor.is_none_or(|g| !g.covers(&t.id)))
            .collect()
    };
    Ok(Resume {
        replayed,
        states: j.states,
        not_replayed,
    })
}
