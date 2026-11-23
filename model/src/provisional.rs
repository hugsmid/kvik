//! The fast path (spec §4 Provisional streams, Q-1109): each accepted call is handed to the open provisional streams before it
//! is durable, then settled as durable, under a watermark, or withdrawn. Unbounded: the engine's `max_behind` lag is
//! not modelled, as group commit's pacing is not.

use std::collections::{BTreeMap, BTreeSet};

use crate::types::{Position, SeriesId};

/// Why provisional readings were withdrawn: the outcome the writer of their call was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Withdrawal {
    /// Written but not durable when the writer failed or stopped; it may be found after a reboot (§9 row 7a).
    OutcomeUnknown,
    /// Never written: the writer was read-only or shut down.
    NotStored,
}

/// A call's outcome as the writer gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not yet known.
    Open,
    /// Acknowledged; its last position.
    Acknowledged(Position),
    /// Failed.
    Failed(Withdrawal),
}

/// What a stream delivers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event<F> {
    /// A call accepted, with its facts in the stream's selection.
    Accepted {
        /// The call.
        seq: u64,
        /// Durable already when delivered.
        durable: bool,
        /// Its selected facts.
        facts: Vec<F>,
    },
    /// Every call delivered with `seq ≤ through` and not withdrawn is durable; sent when it covers a call delivered
    /// provisional.
    Durable {
        /// The watermark.
        through: u64,
        /// The durable position read.
        mark: Position,
    },
    /// The calls delivered in `first..=last` and not under a watermark will never be durable.
    Withdrawn {
        /// First call.
        first: u64,
        /// Last call.
        last: u64,
        /// Why.
        cause: Withdrawal,
    },
    /// The store was shut down or stopped and every delivered call is settled.
    Closed,
}

#[derive(Clone, Debug)]
struct Call<F> {
    facts: Vec<(SeriesId, F)>,
    outcome: Outcome,
}

#[derive(Clone, Debug)]
struct Stream {
    series: Option<BTreeSet<SeriesId>>,
    looked: u64,
    delivered: Vec<u64>,
    settled: BTreeSet<u64>,
    through: u64,
    closed: bool,
}

/// One open of a store's fast path.
#[derive(Clone, Debug)]
pub struct Provisional<F> {
    calls: BTreeMap<u64, Call<F>>,
    streams: BTreeMap<u64, Stream>,
    mark: Position,
    ended: bool,
    closed: bool,
}

impl<F> Default for Provisional<F> {
    fn default() -> Self {
        Provisional {
            calls: BTreeMap::new(),
            streams: BTreeMap::new(),
            mark: Position::ORIGIN,
            ended: false,
            closed: false,
        }
    }
}

/// A call's state as a stream reads it now.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Durable,
    Withdrawn(Withdrawal),
}

impl<F: Clone> Provisional<F> {
    /// The writer accepted a call; returns its number.
    pub fn accept(&mut self, facts: Vec<(SeriesId, F)>) -> u64 {
        let seq = self.calls.len() as u64 + 1;
        self.calls.insert(
            seq,
            Call {
                facts,
                outcome: Outcome::Open,
            },
        );
        seq
    }

    /// The writer gave the call its outcome.
    pub fn settle(&mut self, seq: u64, outcome: Outcome) {
        if let Some(c) = self.calls.get_mut(&seq) {
            c.outcome = outcome;
        }
    }

    /// The durable position; `ended` once the writer has failed or stopped, `closed` once the store is shut down
    /// or gone with its writer stopped.
    pub fn signal(&mut self, mark: Position, ended: bool, closed: bool) {
        if !self.ended {
            self.mark = mark;
        }
        self.ended |= ended;
        self.closed |= closed;
    }

    /// Opens a stream at the next call; `None` selects every series.
    pub fn open(&mut self, series: Option<BTreeSet<SeriesId>>) -> u64 {
        let id = self.streams.len() as u64 + 1;
        let next = self.calls.len() as u64;
        self.streams.insert(
            id,
            Stream {
                series,
                looked: next,
                delivered: Vec::new(),
                settled: BTreeSet::new(),
                through: next,
                closed: false,
            },
        );
        id
    }

    fn state(&self, seq: u64) -> State {
        match self.calls[&seq].outcome {
            Outcome::Acknowledged(last) if last <= self.mark => State::Durable,
            Outcome::Acknowledged(_) if self.ended => State::Withdrawn(Withdrawal::OutcomeUnknown),
            Outcome::Failed(w) => State::Withdrawn(w),
            // A failed writer still settles its open calls; only a stopped one leaves them unknown.
            Outcome::Open if self.closed => State::Withdrawn(Withdrawal::OutcomeUnknown),
            _ => State::Open,
        }
    }

    /// Every event due to stream `id` now: `Withdrawn`, `Durable`, `Accepted`, `Closed`, in that order.
    pub fn poll(&mut self, id: u64) -> Vec<Event<F>> {
        let Some(s) = self.streams.get(&id).cloned() else {
            return Vec::new();
        };
        if s.closed {
            return Vec::new();
        }
        let mut s = s;
        let mut out = Vec::new();
        let outstanding: Vec<u64> = s
            .delivered
            .iter()
            .copied()
            .filter(|q| !s.settled.contains(q))
            .collect();
        let mut groups: Vec<(u64, u64, Option<Withdrawal>)> = Vec::new();
        let mut settled_durable = false;
        let mut rest = outstanding.as_slice();
        while let Some((&q, tail)) = rest.split_first() {
            let w = match self.state(q) {
                State::Open => break,
                State::Durable => None,
                State::Withdrawn(w) => Some(w),
            };
            settled_durable |= w.is_none();
            match groups.last_mut() {
                Some((_, last, g)) if w.is_some() && *g == w => *last = q,
                _ => groups.push((q, q, w)),
            }
            s.settled.insert(q);
            rest = tail;
        }
        for (first, last, w) in groups {
            if let Some(cause) = w {
                out.push(Event::Withdrawn { first, last, cause });
            }
        }
        let through = rest
            .first()
            .map_or(s.delivered.last().copied().unwrap_or(s.through), |q| q - 1)
            .max(s.through);
        if through > s.through {
            s.through = through;
            // A watermark only when it covers a call delivered provisional.
            if settled_durable {
                out.push(Event::Durable {
                    through,
                    mark: self.mark,
                });
            }
        }
        let next = self.calls.len() as u64;
        for q in s.looked + 1..=next {
            let facts: Vec<F> = self.calls[&q]
                .facts
                .iter()
                .filter(|(series, _)| s.series.as_ref().is_none_or(|sel| sel.contains(series)))
                .map(|(_, f)| f.clone())
                .collect();
            if facts.is_empty() {
                continue;
            }
            let durable = match self.state(q) {
                State::Withdrawn(_) => continue,
                State::Durable => {
                    s.settled.insert(q);
                    true
                }
                State::Open => false,
            };
            s.delivered.push(q);
            out.push(Event::Accepted {
                seq: q,
                durable,
                facts,
            });
        }
        s.looked = next;
        if self.closed && out.is_empty() && s.delivered.iter().all(|q| s.settled.contains(q)) {
            s.closed = true;
            out.push(Event::Closed);
        }
        self.streams.insert(id, s);
        out
    }
}
