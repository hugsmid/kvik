//! A starting instance's hold seeded from history (Q-1145 (b), Q-1390).

use kvik_model::gate::{ConditionId, Gate};
use kvik_model::types::{DAY, Nanos, Position, Quality, SeriesId};

use super::rules::{Inst, longest_hold};
use super::{Body, Core, FactItem, History, Item, Replay};

/// The instance a gate starting at `now` (entry at `at`) has after being judged, emitting nothing, from
/// the look-back `[now − min(2L, 24 h), now)` over the series' facts committed inside it; when none has a
/// usable value, the live-state fact at `now` stands in from the look-back's start. `None` when `L` is 0.
pub(super) fn seed(
    gate: &Gate,
    id: ConditionId,
    s: SeriesId,
    now: Nanos,
    at: Position,
    history: &dyn History,
) -> Option<Inst> {
    let longest = longest_hold(&gate.kind);
    if longest == 0 {
        return None;
    }
    let from = now.saturating_sub(longest.saturating_mul(2).min(DAY));
    let inside = history.facts_since(s, from, at);
    let mut feed = Vec::with_capacity(inside.len() + 1);
    if !inside.iter().any(|(_, _, f)| f.usable())
        && let Some(l) = history.series(s, now, at).live
    {
        let fact = FactItem {
            series: s,
            t: l.t,
            value: l.value,
            quality: Quality::Good,
            changed_at: l.changed_at,
        };
        feed.push((from, l.position, fact));
    }
    feed.extend(inside);
    // The look-back is judged on its own: outranking, arrivals and staleness see only its facts.
    let mut replay = Replay::default();
    let mut core = Core::unseeded();
    let watch = Item {
        position: Position::ORIGIN,
        body: Body::Watch {
            id,
            gate: gate.clone(),
        },
    };
    core.apply_batch(from, &[watch], &replay).ok()?;
    for (commit, position, fact) in feed {
        let outranked = replay.push(commit, position, fact.clone());
        let item = Item {
            position,
            body: Body::Fact { fact, outranked },
        };
        core.apply_batch(commit, &[item], &replay).ok()?;
    }
    core.advance(now);
    core.instances.remove(&(id, s))
}
