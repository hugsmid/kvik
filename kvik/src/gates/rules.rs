//! One instance's rules, as `kvik_model::gate` states them (spec §6 Gates; Q-1141, Q-1144, Q-1146,
//! Q-1148, Q-1149): boundaries, holds, the dead-band and the next deadline.

use kvik_model::gate::{Gate, GateState, Kind, TimerKind, TransitionId};
use kvik_model::types::{Nanos, Value};

use super::SeriesState;

/// One instance of a gate on one series.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Inst {
    /// State last reported.
    pub state: GateState,
    /// The transition that set it.
    pub set_by: TransitionId,
    /// Boundaries crossed, one bit each from the lowest.
    pub crossed: u16,
    /// Boundaries whose far side holds, by index, with the first instant it held (index order).
    pub pending: Vec<(u8, Nanos)>,
    /// `Change`: the value the state carries.
    pub reference: Option<Value>,
    /// Commit-clock instant of the last evaluation.
    pub last_eval: Nanos,
}

impl Inst {
    /// An instance with nothing crossed, nothing pending and no value yet.
    pub fn fresh(set_by: TransitionId, now: Nanos) -> Self {
        Inst {
            state: GateState::Unknown,
            set_by,
            crossed: 0,
            pending: Vec::new(),
            reference: None,
            last_eval: now,
        }
    }

    fn on(&self, k: usize) -> bool {
        self.crossed >> k & 1 == 1
    }

    fn since(&self, k: usize) -> Option<Nanos> {
        self.pending
            .iter()
            .find(|(i, _)| usize::from(*i) == k)
            .map(|(_, s)| *s)
    }

    fn set_since(&mut self, k: usize, since: Option<Nanos>) {
        let k8 = u8::try_from(k).expect("at most 16 boundaries");
        self.pending.retain(|(i, _)| *i != k8);
        if let Some(s) = since {
            let at = self.pending.partition_point(|(i, _)| *i < k8);
            self.pending.insert(at, (k8, s));
        }
    }

    /// One boundary: flips once its far side has held for `hold` without a break.
    fn cross(&mut self, k: usize, toward: bool, hold: Nanos, now: Nanos) {
        if !toward {
            self.set_since(k, None);
            return;
        }
        let since = self.since(k).unwrap_or(now);
        if now.saturating_sub(since) >= hold {
            self.crossed ^= 1 << k;
            self.set_since(k, None);
        } else {
            self.set_since(k, Some(since));
        }
    }
}

/// The state a value gives at `now`, moving the boundaries.
pub(crate) fn step(kind: &Kind, i: &mut Inst, v: &Value, now: Nanos) -> GateState {
    match kind {
        Kind::Levels(ls) => {
            let Some(x) = v.as_f64() else {
                i.pending.clear();
                return GateState::Unknown;
            };
            for (k, l) in ls.iter().enumerate() {
                let toward = if i.on(k) { x < l.clear } else { x > l.at };
                let h = hold(kind, k, i.on(k));
                i.cross(k, toward, h, now);
            }
            let level = (0..ls.len()).take_while(|k| i.on(*k)).count();
            GateState::Level(u8::try_from(level).expect("at most 16 levels"))
        }
        Kind::Set { members, .. } => {
            let inside = members.iter().any(|m| m.identical(v));
            let toward = inside != i.on(0);
            let h = hold(kind, 0, i.on(0));
            i.cross(0, toward, h, now);
            GateState::Match(i.on(0))
        }
        Kind::Change { deadband } => {
            let moved = match (&i.reference, v) {
                (None, _) => true,
                (Some(Value::Num(r)), Value::Num(x)) => (x - r).abs() > *deadband,
                (Some(r), x) => !r.identical(x),
            };
            if moved {
                i.reference = Some(v.clone());
            }
            GateState::Value(i.reference.clone().expect("reference set"))
        }
    }
}

/// The hold boundary `k` needs before it flips from `on`.
fn hold(kind: &Kind, k: usize, on: bool) -> Nanos {
    match kind {
        Kind::Levels(ls) if on => ls[k].hold_down,
        Kind::Levels(ls) => ls[k].hold_up,
        Kind::Set { hold_off, .. } if on => *hold_off,
        Kind::Set { hold_on, .. } => *hold_on,
        Kind::Change { .. } => 0,
    }
}

/// The longest hold of any boundary, either way: the seed's `L` (Q-1390).
pub(crate) fn longest_hold(kind: &Kind) -> Nanos {
    match kind {
        Kind::Levels(ls) => ls
            .iter()
            .map(|l| l.hold_up.max(l.hold_down))
            .max()
            .unwrap_or(0),
        Kind::Set {
            hold_on, hold_off, ..
        } => (*hold_on).max(*hold_off),
        Kind::Change { .. } => 0,
    }
}

/// Whether the series is stale at `now`: no arrival for `stale_after` (spec §6 Gates, Stale).
pub(crate) fn stale(gate: &Gate, s: &SeriesState, now: Nanos) -> bool {
    gate.stale_after
        .is_some_and(|lim| s.last_arrival.is_some_and(|a| now.saturating_sub(a) >= lim))
}

/// The instance's next deadline after its last evaluation, the first kind in `TimerKind` order when
/// several fall together; one beyond the clock's range never falls due. The series must have been
/// brought up to the last evaluation.
pub(crate) fn next_due(gate: &Gate, i: &Inst, s: &SeriesState) -> Option<(Nanos, TimerKind)> {
    let after = i.last_eval;
    let mut cands: Vec<(Nanos, TimerKind)> = Vec::new();
    if let Some(lim) = gate.stale_after
        && i.state != GateState::Stale
        && let Some(a) = s.last_arrival
    {
        cands.extend(a.checked_add(lim).map(|d| (d, TimerKind::Stale)));
    }
    cands.extend(i.pending.iter().filter_map(|(k, since)| {
        let k = usize::from(*k);
        let due = since.checked_add(hold(&gate.kind, k, i.on(k)))?;
        Some((due, TimerKind::Hold))
    }));
    // A usable fact ahead is an arrival at its `t` (Q-1200).
    if let Some((t, _)) = s.ahead.iter().find(|(t, h)| **t > after && h.usable) {
        cands.push((*t, TimerKind::Mature));
    }
    cands
        .into_iter()
        .filter(|(d, _)| *d > after)
        .min_by_key(|(d, k)| (*d, *k as u8))
}
