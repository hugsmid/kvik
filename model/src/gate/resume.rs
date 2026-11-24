//! Exact resume: the transitions a consumer receives again after a restart (spec §6 Gates, Resume).

use std::collections::BTreeMap;

use super::{ConditionId, Entry, GateError, GateState, Transition, TransitionId, judge};
use crate::types::{DAY, Nanos, Position, SeriesId};

/// A consumer's latches: per instance, the id of the last transition it confirmed.
pub type Latches = BTreeMap<(ConditionId, SeriesId), TransitionId>;

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
    /// Transitions after the latches, flagged `replayed`, in delivery order.
    pub replayed: Vec<Transition>,
    /// State now per instance and the id that set it, carried by the resync that ends the resume.
    pub states: BTreeMap<(ConditionId, SeriesId), (GateState, TransitionId)>,
    /// The replay was beyond `bound`: nothing replayed, the resync alone (`transitions_not_replayed`).
    pub not_replayed: bool,
}

/// The resume of a consumer whose cursor is `cursor` and whose latches are `latches`, at commit-clock
/// instant `until`. An instance with a latch receives its transitions after it; one without, those at or
/// after the cursor (transitions at a position follow its fact, so the cursor never covers them).
pub fn resume(
    history: &[Entry],
    until: Nanos,
    cursor: Position,
    latches: &Latches,
    bound: ReplayBound,
) -> Result<Resume, GateError> {
    let j = judge(history, until)?;
    // Confirmation is cumulative, so latches only filter and never lengthen the replay (spec §6 Gates).
    let start = cursor;
    let facts = history
        .iter()
        .filter(|e| matches!(e, Entry::Fact { .. }) && e.position() > start)
        .count();
    let from = history
        .iter()
        .take_while(|e| e.position() <= start)
        .last()
        .or(history.first())
        .map_or(until, Entry::commit_time);
    let not_replayed = u64::try_from(facts).unwrap_or(u64::MAX) > bound.facts
        || until.saturating_sub(from) > bound.span;
    let replayed = if not_replayed {
        Vec::new()
    } else {
        j.transitions
            .into_iter()
            .filter(|t| match latches.get(&(t.id.condition, t.id.series)) {
                Some(l) => t.id > *l,
                None => t.id.at >= cursor,
            })
            .collect()
    };
    Ok(Resume {
        replayed,
        states: j.states,
        not_replayed,
    })
}
