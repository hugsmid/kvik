//! A starting instance's hold seeded from history (Q-1145 (b), Q-1390, Q-1560).

use kvik_model::gate::{ConditionId, Gate};
use kvik_model::types::{DAY, Nanos, Position, SeriesId};

use super::rules::{Inst, longest_hold};
use super::{Body, Core, CoreError, History, Item};

/// The instance a gate starting at `now` (entry at `at`) has after being judged, emitting nothing, as the
/// same gate watched at the look-back's start `now − min(2L, 24 h)`: from the series' state there, then
/// over the facts committed inside the look-back. `None` when `L` is 0; an error only from a history read.
pub(super) fn seed(
    gate: &Gate,
    id: ConditionId,
    s: SeriesId,
    now: Nanos,
    at: Position,
    history: &dyn History,
) -> Result<Option<Inst>, CoreError> {
    let longest = longest_hold(&gate.kind);
    if longest == 0 {
        return Ok(None);
    }
    let from = now.saturating_sub(longest.saturating_mul(2).min(DAY));
    let mut core = Core::unseeded();
    core.series.insert(s, history.at_look_back(s, from, at)?);
    let watch = Item {
        position: Position::ORIGIN,
        body: Body::Watch {
            id,
            gate: gate.clone(),
        },
    };
    core.apply_batch(from, &[watch], history)?;
    for (commit, position, fact, outranked) in history.facts_since(s, from, at)? {
        let item = Item {
            position,
            body: Body::Fact { fact, outranked },
        };
        core.apply_batch(commit, &[item], history)?;
    }
    core.advance(now);
    Ok(core.instances.remove(&(id, s)))
}
