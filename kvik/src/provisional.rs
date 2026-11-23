//! The fast path's public types (`docs/api.md` Provisional streams, spec §4 Provisional streams, Q-1109).

use std::sync::Arc;
use std::time::Duration;

use crate::durable::fast::{Entry, Stream};
use crate::error::QueryError;
use crate::time::Timestamp;
use crate::types::{FactFlags, Position, Quality, SeriesId, Value};

/// The most facts a provisional stream may fall behind before it lags.
pub const PROVISIONAL_MAX_BEHIND: usize = 1 << 20;

/// What a provisional stream selects and how far it may fall behind.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProvisionalOptions {
    /// The series whose facts it receives; empty for every series.
    pub series: Vec<SeriesId>,
    /// Facts accepted after the last one taken, of any series, before the stream lags: 1 to
    /// [`PROVISIONAL_MAX_BEHIND`], default 65,536.
    pub max_behind: usize,
}

impl Default for ProvisionalOptions {
    fn default() -> Self {
        ProvisionalOptions {
            series: Vec::new(),
            max_behind: 65_536,
        }
    }
}

impl ProvisionalOptions {
    pub(crate) fn check(&self) -> Result<(), QueryError> {
        if (1..=PROVISIONAL_MAX_BEHIND).contains(&self.max_behind) {
            Ok(())
        } else {
            Err(QueryError::Invalid {
                reason: format!("max_behind must be 1 to {PROVISIONAL_MAX_BEHIND}"),
            })
        }
    }
}

/// A fact as the writer accepted it, before it has a position (spec §4 Provisional streams).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ProvisionalFact {
    /// Series.
    pub series: SeriesId,
    /// Event time.
    pub t: Timestamp,
    /// Value; [`Value::Absent`] when recorded without one.
    pub value: Value,
    /// Quality after stamping.
    pub quality: Quality,
    /// Quality as the adapter supplied it.
    pub supplied_quality: Quality,
    /// Engine flags.
    pub flags: FactFlags,
    /// The source's revision time or version, in a versioned series.
    pub changed_at: Option<u64>,
}

/// Why provisional readings were withdrawn: the outcome the writer of their call was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Withdrawal {
    /// [`EmitError::OutcomeUnknown`](crate::EmitError::OutcomeUnknown); a call acknowledged as written whose sync
    /// failed or whose writer stopped before the mark covered it; or a call its stopped writer never settled. It may
    /// still be found after a reboot (spec §3 Write or sync failure, §9 row 7a).
    OutcomeUnknown,
    /// [`EmitError::ReadOnly`](crate::EmitError::ReadOnly) or [`EmitError::Closed`](crate::EmitError::Closed):
    /// it was not stored.
    NotStored,
}

/// One accepted call's facts in the stream's selection, shared with every other stream.
#[derive(Clone, Debug)]
pub struct Accepted {
    pub(crate) entry: Arc<Entry>,
    pub(crate) pick: Option<Arc<[u32]>>,
    pub(crate) durable: bool,
}

impl Accepted {
    /// The call's number in this open of the store; calls are numbered in the order they were accepted.
    pub fn seq(&self) -> u64 {
        self.entry.seq
    }

    /// Whether the call was already durable when delivered; otherwise it is provisional until a
    /// [`ProvisionalEvent::Durable`] covers it or a [`ProvisionalEvent::Withdrawn`] names it.
    pub fn durable(&self) -> bool {
        self.durable
    }

    /// Its facts in the stream's selection, in call order.
    pub fn facts(&self) -> impl Iterator<Item = &ProvisionalFact> {
        let all = &self.entry.facts;
        let picked: Box<dyn Iterator<Item = &ProvisionalFact>> = match &self.pick {
            None => Box::new(all.iter()),
            Some(p) => Box::new(p.iter().map(move |&i| &all[i as usize])),
        };
        picked
    }
}

/// What a provisional stream delivers (spec §4 Provisional streams).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ProvisionalEvent {
    /// A call the writer accepted.
    Accepted(Accepted),
    /// Every call delivered with `seq ≤ through` and not withdrawn is durable; `mark` is the durable position read.
    /// Sent only when it covers a call delivered provisional.
    Durable {
        /// The watermark.
        through: u64,
        /// The durable position.
        mark: Position,
    },
    /// The calls delivered with `first ≤ seq ≤ last` and not yet covered by a watermark will never be durable in
    /// this open: drop them.
    Withdrawn {
        /// The first call withdrawn.
        first: u64,
        /// The last call withdrawn.
        last: u64,
        /// Why.
        cause: Withdrawal,
    },
    /// The stream fell more than `max_behind` facts behind: calls `first..=last` were not delivered. Every fact
    /// among them lies above `after`; once durable they can be read there. A call larger than `max_behind` on its
    /// own is delivered whole.
    Lagged {
        /// The first call missed.
        first: u64,
        /// The last call missed.
        last: u64,
        /// A position below every missed fact.
        after: Position,
    },
    /// The store was shut down or dropped, its writer has stopped and every call delivered is settled; nothing
    /// follows (Q-1113).
    Closed,
}

/// A stream of readings as the writer accepts them, before they are durable (spec §4 Provisional streams). It has no
/// thread: [`Provisional::next`] computes what is due when called. Dropping it ends it.
#[derive(Debug)]
pub struct Provisional {
    pub(crate) stream: Stream,
}

impl Provisional {
    /// The next event, waiting up to `timeout` for one; `None` if none came by then.
    pub fn next(&mut self, timeout: Duration) -> Option<ProvisionalEvent> {
        self.stream.next(timeout)
    }

    /// The next event if one is due now.
    pub fn try_next(&mut self) -> Option<ProvisionalEvent> {
        self.stream.next(Duration::ZERO)
    }
}
