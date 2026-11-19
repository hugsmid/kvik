//! The shared tail of a reader (the query design's section 3.6): what each refresh read, kept for the live sessions
//! of the reader so that one tail read serves them all. A refresh with no session keeps nothing. Each refresh is one
//! [`Round`], numbered as the view it publishes; a session takes the rounds after the last one it took up to the
//! view it answers at. Memory is bounded by the largest backlog a session allows: past it the oldest rounds keep only
//! their voids and gaps, and a session that still needed their facts lags (Q-1013).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::facts::Value as LogValue;
use kvik_log::reader::ReadEvent;
use kvik_log::writer::Position as LogPos;

use crate::durable::index::Stored;
use crate::durable::loss::Find;

/// One thing a refresh read, in log order.
#[derive(Clone, Debug)]
pub(crate) enum Piece {
    Fact(Stored),
    Entry(Entry),
    /// Everything above this position is void (spec §7 Readers).
    Void(LogPos),
    /// These positions are lost (Q-938): found by the refresh's own pass or by another read of the process.
    Gap(LogPos, LogPos),
}

impl Piece {
    fn bytes(&self) -> u64 {
        let held = match self {
            Piece::Fact(s) => match &s.row.value {
                LogValue::Text(t) => t.len(),
                _ => 0,
            },
            Piece::Entry(e) => usize::try_from(e.encoded_len()).unwrap_or(usize::MAX),
            _ => 0,
        };
        (std::mem::size_of::<Piece>().saturating_add(held)) as u64
    }

    fn kept_when_trimmed(&self) -> bool {
        matches!(self, Piece::Void(_) | Piece::Gap(..))
    }
}

/// What one refresh read, or a run of refreshes that read nothing.
#[derive(Debug)]
pub(crate) struct Round {
    pub(crate) seq: u64,
    /// The number of the last refresh of the run; `seq` for a round with pieces.
    last: u64,
    pieces: Vec<Piece>,
    /// Each piece's index in the round as read, once trimmed; a session stopped inside the round counts by it.
    index: Vec<usize>,
    /// Every fact and entry is held; otherwise only the voids and gaps are.
    pub(crate) whole: bool,
    rows: u64,
    bytes: u64,
}

impl Round {
    /// The pieces held, each with its index in the round as read.
    pub(crate) fn pieces(&self) -> impl Iterator<Item = (usize, &Piece)> {
        self.pieces
            .iter()
            .enumerate()
            .map(|(i, p)| (self.index.get(i).copied().unwrap_or(i), p))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    fn trimmed(&self) -> Round {
        let (index, pieces) = self
            .pieces()
            .filter(|(_, p)| p.kept_when_trimmed())
            .map(|(i, p)| (i, p.clone()))
            .unzip();
        Round {
            seq: self.seq,
            last: self.last,
            pieces,
            index,
            whole: false,
            rows: 0,
            bytes: 0,
        }
    }
}

/// What a refresh with sessions to serve gathers while it reads.
#[derive(Debug)]
pub(crate) struct Collector {
    pieces: Vec<Piece>,
    rows: u64,
    bytes: u64,
    cap: (u64, u64),
    whole: bool,
    /// The most facts it held at once.
    #[cfg(test)]
    most: u64,
}

impl Collector {
    fn push(&mut self, p: Piece) {
        if !p.kept_when_trimmed() {
            if !self.whole {
                return;
            }
            self.rows += 1;
            self.bytes += p.bytes();
            #[cfg(test)]
            {
                self.most = self.most.max(self.rows);
            }
            if self.rows > self.cap.0 || self.bytes > self.cap.1 {
                self.whole = false;
                self.pieces.retain(Piece::kept_when_trimmed);
                (self.rows, self.bytes) = (0, 0);
                return;
            }
        }
        self.pieces.push(p);
    }

    /// Takes what a part of the refresh read, and the ranges its pass found lost.
    pub(crate) fn events(&mut self, events: &[ReadEvent], found: &[Find]) {
        for e in events {
            match e {
                ReadEvent::Batch(b) => {
                    let mut sequence = b.first.sequence;
                    for item in &b.content.items {
                        let position = LogPos {
                            generation: b.first.generation,
                            sequence,
                        };
                        sequence += item.sequences();
                        match item {
                            Item::Fact(row) => self.push(Piece::Fact(Stored {
                                position,
                                received_at: b.content.received_at,
                                row: row.clone(),
                            })),
                            Item::Entry(e) if matches!(**e, Entry::Ignored { .. }) => {}
                            Item::Entry(e) => self.push(Piece::Entry((**e).clone())),
                        }
                    }
                }
                ReadEvent::Void { above, .. } => self.push(Piece::Void(*above)),
                ReadEvent::Corruption { .. } => {}
            }
        }
        for f in found {
            self.gap(f.closed.0, f.closed.1);
        }
    }

    /// Positions found lost by another read of the process (Q-938).
    pub(crate) fn gap(&mut self, first: LogPos, last: LogPos) {
        if first <= last {
            self.push(Piece::Gap(first, last));
        }
    }
}

#[derive(Debug)]
struct Sub {
    /// The last round taken whole.
    cursor: u64,
    cap: (u64, u64),
    polled: Duration,
    idle: bool,
}

#[derive(Debug, Default)]
struct State {
    /// The number of the last round finished.
    last: u64,
    rounds: VecDeque<Arc<Round>>,
    subs: BTreeMap<u64, Sub>,
    next_sub: u64,
    rows: u64,
    bytes: u64,
    /// The most facts held at once, collectors included.
    #[cfg(test)]
    peak: u64,
}

/// See the module documentation.
#[derive(Debug, Default)]
pub(crate) struct Hub(Mutex<State>);

/// The rounds a session takes, and what it learns with them.
#[derive(Debug)]
pub(crate) struct Taken {
    pub(crate) rounds: Vec<Arc<Round>>,
    /// A round it needed is gone or kept only its voids and gaps.
    pub(crate) lagged: bool,
    /// Facts and their bytes held after its cursor, to the newest round.
    pub(crate) backlog: (u64, u64),
    /// It was not polled for `cut_max_age` and no longer holds rounds.
    pub(crate) idle: bool,
}

impl Hub {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A collector for the refresh about to read, if a session is subscribed; `None` costs nothing.
    pub(crate) fn collector(&self) -> Option<Collector> {
        let s = self.lock();
        let cap = cap(&s)?;
        Some(Collector {
            pieces: Vec::new(),
            rows: 0,
            bytes: 0,
            cap,
            whole: true,
            #[cfg(test)]
            most: 0,
        })
    }

    /// Ends a refresh: keeps what it gathered, marks sessions idle past `idle_after` and trims to the largest backlog
    /// allowed. Returns the round's number, the view's.
    pub(crate) fn finish(&self, c: Option<Collector>, now: Duration, idle_after: Duration) -> u64 {
        let mut s = self.lock();
        s.last += 1;
        let seq = s.last;
        for sub in s.subs.values_mut() {
            sub.idle |= now.saturating_sub(sub.polled) > idle_after;
        }
        if let Some(c) = c {
            #[cfg(test)]
            {
                s.peak = s.peak.max(s.rows + c.most);
            }
            s.rows += c.rows;
            s.bytes += c.bytes;
            let nothing = c.whole && c.pieces.is_empty();
            match s.rounds.back_mut() {
                // An idle subscriber keeps one round for a run of refreshes that read nothing (Q-1013).
                Some(b) if nothing && b.whole && b.is_empty() => {
                    *b = Arc::new(Round {
                        seq: b.seq,
                        last: seq,
                        pieces: Vec::new(),
                        index: Vec::new(),
                        whole: true,
                        rows: 0,
                        bytes: 0,
                    });
                }
                _ => s.rounds.push_back(Arc::new(Round {
                    seq,
                    last: seq,
                    pieces: c.pieces,
                    index: Vec::new(),
                    whole: c.whole,
                    rows: c.rows,
                    bytes: c.bytes,
                })),
            }
        }
        trim(&mut s);
        seq
    }

    /// A new subscriber whose sessions allow `cap` (facts, bytes) behind the mark; its cursor is set by
    /// [`Hub::start`].
    pub(crate) fn subscribe(&self, cap: (u64, u64), now: Duration) -> u64 {
        let mut s = self.lock();
        s.next_sub += 1;
        let id = s.next_sub;
        let cursor = s.last;
        s.subs.insert(
            id,
            Sub {
                cursor,
                cap,
                polled: now,
                idle: false,
            },
        );
        id
    }

    pub(crate) fn unsubscribe(&self, sub: u64) {
        let mut s = self.lock();
        s.subs.remove(&sub);
        trim(&mut s);
    }

    /// Sets a subscriber's cursor and backlog limit.
    pub(crate) fn start(&self, sub: u64, seq: u64, cap: (u64, u64)) {
        if let Some(x) = self.lock().subs.get_mut(&sub) {
            x.cursor = seq;
            x.cap = cap;
        }
    }

    /// Notes that `sub` was polled at `now`, whatever its call found.
    pub(crate) fn touch(&self, sub: u64, now: Duration) {
        if let Some(x) = self.lock().subs.get_mut(&sub) {
            x.polled = now;
        }
    }

    /// The rounds after `sub`'s cursor up to `upto`, at `now`.
    pub(crate) fn take(&self, sub: u64, upto: u64, now: Duration) -> Taken {
        let mut s = self.lock();
        let Some(x) = s.subs.get_mut(&sub) else {
            return Taken {
                rounds: Vec::new(),
                lagged: true,
                backlog: (0, 0),
                idle: true,
            };
        };
        x.polled = now;
        let (cursor, idle) = (x.cursor, x.idle);
        let rounds: Vec<Arc<Round>> = s
            .rounds
            .iter()
            .filter(|r| r.last > cursor && r.seq <= upto)
            .cloned()
            .collect();
        let backlog = s
            .rounds
            .iter()
            .filter(|r| r.last > cursor)
            .fold((0, 0), |(a, b), r| (a + r.rows, b + r.bytes));
        let counted: u64 = rounds
            .iter()
            .map(|r| (r.last.min(upto) + 1).saturating_sub(r.seq.max(cursor + 1)))
            .sum();
        let lagged =
            idle || counted < upto.saturating_sub(cursor) || rounds.iter().any(|r| !r.whole);
        Taken {
            rounds,
            lagged,
            backlog,
            idle,
        }
    }

    /// Moves `sub`'s cursor to round `seq`, taken whole.
    pub(crate) fn advance(&self, sub: u64, seq: u64) {
        let mut s = self.lock();
        if let Some(x) = s.subs.get_mut(&sub) {
            x.cursor = x.cursor.max(seq);
        }
        trim(&mut s);
    }

    /// Facts and bytes held and rounds kept, for the memory tests.
    #[cfg(test)]
    pub(crate) fn held(&self) -> (u64, u64, usize) {
        let s = self.lock();
        (s.rows, s.bytes, s.rounds.len())
    }

    /// The most facts held at once, while a refresh read included.
    #[cfg(test)]
    pub(crate) fn peak(&self) -> u64 {
        self.lock().peak
    }
}

/// The largest backlog an active subscriber allows, or `None` with none.
fn cap(s: &State) -> Option<(u64, u64)> {
    s.subs
        .values()
        .filter(|x| !x.idle)
        .map(|x| x.cap)
        .reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)))
}

/// Drops rounds every active subscriber has taken, then keeps only the voids and gaps of the oldest rounds while
/// more than the largest backlog allowed is held.
fn trim(s: &mut State) {
    let floor = s.subs.values().filter(|x| !x.idle).map(|x| x.cursor).min();
    while let Some(r) = s.rounds.front() {
        if floor.is_some_and(|f| r.last > f) {
            break;
        }
        s.rows -= r.rows;
        s.bytes -= r.bytes;
        s.rounds.pop_front();
    }
    let (rows, bytes) = cap(s).unwrap_or((0, 0));
    let mut i = 0;
    while (s.rows > rows || s.bytes > bytes) && i < s.rounds.len() {
        if s.rounds[i].whole {
            s.rows -= s.rounds[i].rows;
            s.bytes -= s.rounds[i].bytes;
            s.rounds[i] = Arc::new(s.rounds[i].trimmed());
        }
        i += 1;
    }
}
