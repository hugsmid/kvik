//! Consumers and funnels (spec §4, §6; `docs/api.md` Consuming).

use std::time::Duration;

use crate::read::{Aggregate, Live};
use crate::time::Timestamp;
use crate::types::{Fact, Position, SeriesId};

/// Where a new consumer starts (spec §4 New consumer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Start {
    /// After the last committed position.
    #[default]
    Now,
    /// After a position. Not supported by the model backend yet.
    Position(Position),
    /// After the last position committed at or before a time. Not supported
    /// by the model backend yet.
    Time(Timestamp),
}

/// Which series a funnel selects.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Select {
    /// The listed series.
    Ids(Vec<SeriesId>),
    /// Live series with a name or alias matching a pattern, re-evaluated when
    /// a series is defined or aliased (spec §4 Routing). `*` matches any run
    /// of characters within one `/`-separated segment. The specification does
    /// not define the grammar yet, so this one is provisional
    /// (`model/OPEN-QUESTIONS.md` Q-88). A condition funnel's pattern is
    /// resolved once, when routed, and must match exactly one series.
    Names(String),
    /// Every series.
    All,
    /// No series: for a consumer's `emits` when it emits nothing.
    None,
}

/// A rolling-window operator over `[now − width, now)` (spec §6).
#[derive(Clone, Debug, PartialEq)]
pub struct Operator {
    /// The aggregate.
    pub aggregate: Aggregate,
    /// Window width.
    pub width: Duration,
}

impl Operator {
    /// A rolling window of `width`.
    pub fn rolling(aggregate: Aggregate, width: Duration) -> Self {
        Operator { aggregate, width }
    }
}

/// A condition (spec §6 Conditions), evaluated on the operator's value or,
/// without an operator, on the live-state value.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Condition {
    /// True when the value rises above `on`; false again only when it falls
    /// below `clear` (`clear ≤ on`).
    Threshold {
        /// Activation level.
        on: f64,
        /// Clear level.
        clear: f64,
    },
    /// Delivers a value whenever it moves beyond the dead-band from the last
    /// one delivered.
    OnChange {
        /// Dead-band.
        deadband: f64,
    },
    /// True once the value has been above `above` for `n` on the monotonic
    /// clock.
    HeldFor {
        /// Level.
        above: f64,
        /// Duration.
        n: Duration,
    },
    /// True once no reading has arrived for the series' staleness limit plus
    /// `n` on the monotonic clock.
    StaleFor {
        /// Extra duration.
        n: Duration,
    },
}

impl Condition {
    /// True above `on`; clears below the same level unless
    /// [`Condition::clear_below`] sets another.
    pub fn above(on: f64) -> Self {
        Condition::Threshold { on, clear: on }
    }

    /// Sets the clear level of a threshold; no effect on other conditions.
    pub fn clear_below(self, clear: f64) -> Self {
        match self {
            Condition::Threshold { on, .. } => Condition::Threshold { on, clear },
            other => other,
        }
    }

    /// On change, with a dead-band.
    pub fn on_change(deadband: f64) -> Self {
        Condition::OnChange { deadband }
    }

    /// Held above a level for a duration.
    pub fn held_for(above: f64, n: Duration) -> Self {
        Condition::HeldFor { above, n }
    }

    /// Stale for a duration beyond the staleness limit.
    pub fn stale_for(n: Duration) -> Self {
        Condition::StaleFor { n }
    }
}

/// A funnel (spec §4): a series selection, an optional operator and an
/// optional condition. Without a condition it delivers every fact selected;
/// with one, it delivers the condition's transitions.
#[derive(Clone, Debug, PartialEq)]
pub struct Funnel {
    /// Series selection; a condition funnel selects exactly one series (spec
    /// §6 Multi-series).
    pub series: Select,
    /// Operator.
    pub operator: Option<Operator>,
    /// Condition.
    pub condition: Option<Condition>,
}

impl Funnel {
    /// A funnel delivering every fact of the selection.
    pub fn facts(series: Select) -> Self {
        Funnel {
            series,
            operator: None,
            condition: None,
        }
    }
}

/// Funnel identifier, unique within its consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunnelId(pub u32);

/// The state of a condition (spec §6 Conditions, Latch).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ConditionState {
    /// The condition is true.
    True,
    /// The condition is false.
    False,
    /// On change: the last value delivered (`None` before any).
    Value(Option<f64>),
    /// During a restore, no local detail to evaluate on (spec §2.3, §6).
    /// Never produced by the model backend.
    Unknown,
}

/// Identity of a delivery (spec §4 At-least-once): fact deliveries are
/// de-duplicated by position, condition deliveries by funnel and sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DeliveryId {
    /// A fact delivery.
    Fact(Position),
    /// A condition delivery, caused by a routed fact or by the funnel's
    /// timer.
    Condition {
        /// Funnel.
        funnel: FunnelId,
        /// Per-funnel sequence, valid until the next resync.
        sequence: u64,
    },
    /// `generation_changed`.
    GenerationChanged,
    /// A part of a resync; confirming the last part confirms the resync,
    /// confirming an earlier one does nothing.
    Resync {
        /// The part, from 1.
        part: u32,
    },
}

/// What produced a condition delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TriggerCause {
    /// An evaluation on a routed fact.
    Fact,
    /// An evaluation on the funnel's timer.
    Timer,
    /// Re-attaching to live after catch-up.
    Reattach,
}

/// A condition transition.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Trigger {
    /// Funnel.
    pub funnel: FunnelId,
    /// The state the transition leads to.
    pub state: ConditionState,
    /// The value evaluated, if known; `None` on the model backend.
    pub value: Option<f64>,
    /// The operator window's coverage, if known; `None` on the model backend.
    pub coverage: Option<Duration>,
    /// What produced it.
    pub cause: TriggerCause,
}

/// The state of one funnel and series in the resync that ends a resume.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct FunnelState {
    /// Funnel.
    pub funnel: FunnelId,
    /// Series.
    pub series: SeriesId,
    /// Its live state.
    pub live: Live,
    /// Condition state now, for a condition funnel.
    pub condition: Option<ConditionState>,
    /// Whether the condition state differs from the latch, or there is no
    /// latch: a transition, to be acted on (spec §6 Restart and resume). An
    /// addition of the model backend.
    pub transition: bool,
}

/// Why a resync is sent (spec §4 Resync).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResyncCause {
    /// At the end of a resume, after a restart or reconnect.
    Resume,
    /// At a reset point: positions above `forget_above` may name other facts.
    ResetPoint,
    /// After a restore started a new generation.
    GenerationChanged,
    /// The cursor is older than the oldest local log segment.
    CursorBehindLog,
}

/// One part of a resync: a resync is 1..n deliveries, all evaluated at one
/// position and carrying the same `cause` and `forget_above`. Confirm the
/// `last` part; nothing further is delivered until then.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ResyncPart {
    /// Why it is sent.
    pub cause: ResyncCause,
    /// At a reset point, the position above which the consumer must forget
    /// every position it has seen, durably, before confirming.
    pub forget_above: Option<Position>,
    /// The part, from 1.
    pub part: u32,
    /// Whether this is the last part.
    pub last: bool,
    /// At most 256 funnel states; empty unless the resync ends a resume.
    pub funnels: Vec<FunnelState>,
}

/// Funnel states per resync part (`docs/api.md` Consuming).
pub const RESYNC_PART_FUNNELS: usize = 256;

/// What a delivery carries.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DeliveryKind {
    /// A fact.
    Fact(Fact),
    /// A condition transition.
    Trigger(Trigger),
    /// A part of a resync: live and condition state now.
    Resync(ResyncPart),
    /// A restore started a new generation (spec §8).
    GenerationChanged {
        /// The generation before.
        from: u32,
        /// The new generation.
        to: u32,
    },
}

/// Flags of a delivery (spec §4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DeliveryFlags {
    /// Produced in catch-up: for record-keeping, never to act on.
    pub replayed: bool,
    /// A fact flagged `future` (spec §2.2).
    pub future: bool,
    /// A fact outranked when it was committed: must not be acted on.
    pub outranked: bool,
    /// A trigger evaluated over a window whose edges moved inward (spec §6
    /// Old spans).
    pub approximate_edges: bool,
    /// Delivered while a restore is bringing detail back.
    pub restoring: bool,
}

/// A delivery to a consumer.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Delivery {
    /// Identity, for [`Consumer::confirm`](crate::Consumer::confirm).
    pub id: DeliveryId,
    /// For a fact its position; for a condition delivery the last committed
    /// position at evaluation; for `resync` the position up to which the
    /// stream before it is complete.
    pub position: Position,
    /// Content.
    pub kind: DeliveryKind,
    /// Flags.
    pub flags: DeliveryFlags,
    /// When the store handed it over (engine `now`), to judge its age
    /// together with a fact's `received_at`.
    pub delivered_at: Timestamp,
}
