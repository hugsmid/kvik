//! Live read types (`docs/api.md` Live reads; the query design's section 3.6).

use std::time::Duration;

use super::Live;
use crate::consume::Select;
use crate::error::QueryError;
use crate::series::SeriesInfo;
use crate::time::Timestamp;
use crate::types::{Fact, Position, SeriesId};

/// Which series a query reads (`docs/api.md` Live reads): every series an entry of `select` selects, in the order of
/// the entries, each once. A series selected by name or by `All` must not be retired.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Selection {
    /// The entries; [`Select::Names`] patterns follow the routing grammar (spec §4 Funnel).
    pub select: Vec<Select>,
}

impl Selection {
    /// The series these entries select.
    pub fn new(select: impl IntoIterator<Item = Select>) -> Self {
        Selection {
            select: select.into_iter().collect(),
        }
    }

    /// Every series not retired.
    pub fn all() -> Self {
        Selection::new([Select::All])
    }

    /// Whether a series with these names, retired or not, is selected (Q-895: `Tail` asks this of each fact, with
    /// what was known at its position).
    pub(crate) fn selects(&self, id: SeriesId, names: &[String], retired: bool) -> bool {
        !retired
            && self.select.iter().any(|s| match s {
                Select::Ids(ids) => ids.contains(&id),
                Select::Names(p) => names.iter().any(|n| crate::pattern::matches(p, n)),
                Select::All => true,
                Select::None => false,
            })
    }

    /// The selected series among `known` (id order, with names and whether retired), in the selection's order: the
    /// canonical order of `LiveEvent::Joined`'s `index` (Q-1012).
    pub(crate) fn resolve(&self, known: &[(SeriesId, &[String], bool)]) -> Vec<SeriesId> {
        let mut out: Vec<SeriesId> = Vec::new();
        let push = |id: SeriesId, out: &mut Vec<SeriesId>| {
            if !out.contains(&id) {
                out.push(id);
            }
        };
        for s in &self.select {
            match s {
                Select::Ids(ids) => {
                    for id in ids {
                        if known.iter().any(|(k, _, retired)| k == id && !retired) {
                            push(*id, &mut out);
                        }
                    }
                }
                Select::None => {}
                one => {
                    let sel = Selection::new([one.clone()]);
                    for (id, names, retired) in known {
                        if sel.selects(*id, names, *retired) {
                            push(*id, &mut out);
                        }
                    }
                }
            }
        }
        out
    }

    /// Refuses a pattern outside the routing grammar.
    pub(crate) fn check(&self) -> Result<(), QueryError> {
        for s in &self.select {
            if let Select::Names(p) = s {
                crate::pattern::check(p).map_err(|e| QueryError::Invalid {
                    reason: format!("pattern {p:?}: {e}"),
                })?;
            }
        }
        Ok(())
    }
}

impl From<Select> for Selection {
    fn from(s: Select) -> Self {
        Selection::new([s])
    }
}

/// A query a [`LiveSession`](crate::LiveSession) keeps up to date (`docs/api.md` Live reads).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Query {
    /// The live state of each selected series (spec §2.3), as [`Snapshot::live`] gives it.
    Latest(Selection),
    /// The facts of the selected series as they become durable, in position order; opened at the cut of its
    /// `Opened`. Live only.
    Tail {
        /// The series, chosen for each fact by what was known at its position (Q-895).
        sel: Selection,
    },
}

/// Limits of one live read (`docs/api.md` Live reads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LiveOptions {
    /// Most facts a `Tail` carries in one batch; the batch's cut stops at the last (default 10,000).
    pub max_rows: usize,
    /// Facts between a `Tail` and the mark past which it gets `Lagged` and continues at the mark (default
    /// 1,000,000).
    pub max_backlog_rows: u64,
    /// The same in bytes held (default 64 MiB).
    pub max_backlog_bytes: u64,
}

impl Default for LiveOptions {
    fn default() -> Self {
        LiveOptions {
            max_rows: 10_000,
            max_backlog_rows: 1_000_000,
            max_backlog_bytes: 64 << 20,
        }
    }
}

impl LiveOptions {
    /// These options with `max_rows`, the other limits unchanged.
    pub fn max_rows(mut self, n: usize) -> Self {
        self.max_rows = n;
        self
    }

    /// These options with both backlog limits.
    pub fn max_backlog(mut self, rows: u64, bytes: u64) -> Self {
        self.max_backlog_rows = rows;
        self.max_backlog_bytes = bytes;
        self
    }
}

/// How a session paces its batches (`docs/api.md` Live reads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionOptions {
    /// Least time between two batches (default 200 ms, at least 20 ms), so that a burst is one update.
    pub min_batch_interval: Duration,
    /// How often a reader process reads the mark while waiting (default 100 ms, at least 10 ms); the writer's
    /// process is woken when the mark moves.
    pub poll_interval: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            min_batch_interval: Duration::from_millis(200),
            poll_interval: Duration::from_millis(100),
        }
    }
}

impl SessionOptions {
    /// Options with these intervals.
    pub fn new(min_batch_interval: Duration, poll_interval: Duration) -> Self {
        SessionOptions {
            min_batch_interval,
            poll_interval,
        }
    }

    pub(crate) fn check(&self) -> Result<(), QueryError> {
        let low = |what: &str, least: u64| QueryError::Invalid {
            reason: format!("{what} must be at least {least} ms"),
        };
        if self.min_batch_interval < Duration::from_millis(20) {
            return Err(low("min_batch_interval", 20));
        }
        if self.poll_interval < Duration::from_millis(10) {
            return Err(low("poll_interval", 10));
        }
        Ok(())
    }
}

/// A live read of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LiveId(pub(crate) u64);

/// What one call of [`LiveSession::next`](crate::LiveSession::next) brings: every live read of the session at one
/// durable position and one `now`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct UpdateBatch {
    /// The batch's cut: every live read is as of this position.
    pub durable: Position,
    /// The `now` its values are judged at (spec §6 `now`).
    pub now: Timestamp,
    /// Per live read, in the order `Reset`, `Gap` or `Lagged`, `Opened`, `Joined` and `Left`, then `Updated` and
    /// `Appended`, then `Closed`.
    pub events: Vec<(LiveId, LiveEvent)>,
}

/// A change to a live read's result (`docs/api.md` Live reads).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum LiveEvent {
    /// The result at the batch's cut. A `Latest`'s replaces whatever the consumer held; a `Tail`'s holds no fact, and
    /// after a `Reset` says only that the read goes on from `above`, the consumer keeping what it holds up to it.
    Opened(Answer),
    /// New facts at the end.
    Appended(Appended),
    /// A value replaced in place.
    Updated(Updated),
    /// A reset voided every position above `above`: the consumer drops what it holds above it; an `Opened` follows.
    Reset {
        /// The last position still valid.
        above: Position,
    },
    /// Positions found lost (spec §3 Recovery rule 2): the consumer drops what it holds between them.
    Gap {
        /// First lost position.
        first: Position,
        /// Last lost position.
        last: Position,
    },
    /// The consumer fell behind by more than the backlog limit: the facts between these positions were not sent
    /// and may be read statically.
    Lagged {
        /// First position not sent.
        from: Position,
        /// Last position not sent: the batch's cut.
        to: Position,
    },
    /// A series entered the selection (a definition, or a name given); `index` is its place in the canonical order.
    Joined {
        /// The series.
        series: SeriesId,
        /// What the store knew about it at the cut.
        info: Box<SeriesInfo>,
        /// Its place among the selected series.
        index: usize,
        /// Its share of the result.
        part: Part,
    },
    /// A series left the selection (a retirement); the consumer drops it.
    Left {
        /// The series.
        series: SeriesId,
    },
    /// The live read ended.
    Closed(CloseReason),
}

/// A live read's result at its opening cut.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Answer {
    /// `Latest`: each selected series' live state, in the canonical order.
    Latest(Vec<(SeriesId, Live)>),
    /// `Tail`: nothing yet; facts follow in `Appended`.
    Tail,
}

/// New facts at the end of a live read's result.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Appended {
    /// `Tail`: facts in position order.
    Facts(Vec<Fact>),
}

/// A value replaced in a live read's result.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Updated {
    /// `Latest`: a series' live state.
    Live {
        /// The series.
        series: SeriesId,
        /// Its live state now.
        live: Live,
    },
}

/// A joining series' share of a live read's result.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Part {
    /// `Latest`: its live state.
    Live(Live),
}

/// Why a live read ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseReason {
    /// The session was not polled for `cut_max_age`.
    Idle,
}

/// The rank of an event in a live read's part of a batch (the `UpdateBatch::events` order).
pub(crate) fn event_rank(e: &LiveEvent) -> u8 {
    match e {
        LiveEvent::Reset { .. } | LiveEvent::Gap { .. } | LiveEvent::Lagged { .. } => 0,
        LiveEvent::Opened(_) => 1,
        LiveEvent::Joined { .. } | LiveEvent::Left { .. } => 2,
        LiveEvent::Updated(_) | LiveEvent::Appended(_) => 3,
        LiveEvent::Closed(_) => 4,
    }
}
