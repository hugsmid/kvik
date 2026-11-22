//! The fast path (spec §4 Provisional streams, Q-1109): each accepted call goes, before it is durable, into one ring shared by
//! every provisional stream; a stream learns each call's fate from its completion and the durable position.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use kvik_log::content::Item;
use kvik_log::writer::{Ack as LogAck, CommitError, Position as LogPos, Watch};

use super::completion::Completion;
use super::index::{Stored, fact_out, pos};
use super::lock;
use crate::provisional::{
    Accepted, ProvisionalEvent, ProvisionalFact, ProvisionalOptions, Withdrawal,
};

/// How often a waiting stream reads the durable position while its oldest pending call is acknowledged ahead of its
/// sync (a flagged batch, spec §3 Commit) or the store is closing: neither rings the bell.
const RECHECK: Duration = Duration::from_millis(5);

/// One accepted call.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) seq: u64,
    /// Facts accepted in this open through this call.
    upto: u64,
    /// The durable position before the call was queued: its facts and every later call's lie above.
    floor: LogPos,
    pub(crate) facts: Box<[ProvisionalFact]>,
    outcome: Outcome,
}

#[derive(Debug)]
enum Outcome {
    Completion(Arc<Completion>),
    #[cfg(test)]
    Set(Mutex<Option<Result<LogAck, CommitError>>>),
}

impl Entry {
    fn result(&self) -> Option<Result<LogAck, CommitError>> {
        match &self.outcome {
            Outcome::Completion(c) => match c.poll(None) {
                Poll::Ready(r) => Some(r),
                Poll::Pending => None,
            },
            #[cfg(test)]
            Outcome::Set(s) => lock(s).clone(),
        }
    }

    fn before(&self) -> u64 {
        self.upto - self.facts.len() as u64
    }
}

/// A call's fate as a stream reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    Open,
    Durable,
    Withdrawn(Withdrawal),
}

/// What the writer's progress tells a stream.
pub(crate) trait Signals: Send + std::fmt::Debug {
    /// The writer has failed or stopped: the durable position moves no more.
    fn ended(&self) -> bool;
    /// The writer thread has ended.
    fn stopped(&self) -> bool;
    /// The durable position; read after [`Signals::ended`].
    fn durable(&self) -> LogPos;
}

impl Signals for Watch {
    fn ended(&self) -> bool {
        self.stopped() || self.fault().is_some()
    }
    fn stopped(&self) -> bool {
        Watch::stopped(self)
    }
    fn durable(&self) -> LogPos {
        Watch::durable(self)
    }
}

fn fate(e: &Entry, mark: LogPos, ended: bool) -> Fate {
    match e.result() {
        Some(Ok(a)) if a.last <= mark => Fate::Durable,
        Some(Err(CommitError::OutcomeUnknown)) => Fate::Withdrawn(Withdrawal::OutcomeUnknown),
        Some(Err(_)) => Fate::Withdrawn(Withdrawal::NotStored),
        _ if ended => Fate::Withdrawn(Withdrawal::OutcomeUnknown),
        _ => Fate::Open,
    }
}

#[derive(Debug, Default)]
struct Ring {
    entries: VecDeque<Arc<Entry>>,
    next_seq: u64,
    facts: u64,
    /// Each open stream's `max_behind`, by stream id: the ring keeps the largest.
    wants: BTreeMap<u64, usize>,
    next_stream: u64,
}

impl Ring {
    fn keep(&mut self) {
        let cap = self.wants.values().copied().max().unwrap_or(0) as u64;
        while self.entries.len() > 1
            && self
                .entries
                .front()
                .is_some_and(|f| self.facts - f.before() > cap)
        {
            self.entries.pop_front();
        }
        if cap == 0 {
            self.entries.clear();
        }
    }
}

/// Wakes waiting streams: a new call, a call settled, the store closed.
#[derive(Debug, Default)]
struct Bell {
    state: Mutex<(u64, bool)>,
    cv: Condvar,
}

impl Bell {
    fn ring(&self, close: bool) {
        let mut s = lock(&self.state);
        s.0 += 1;
        s.1 |= close;
        drop(s);
        self.cv.notify_all();
    }
}

impl Wake for Bell {
    fn wake(self: Arc<Self>) {
        self.ring(false);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.ring(false);
    }
}

/// The writer's side of the fast path.
#[derive(Debug)]
pub(crate) struct Fast {
    streams: AtomicUsize,
    ring: Mutex<Ring>,
    bell: Arc<Bell>,
    waker: Waker,
}

impl Default for Fast {
    fn default() -> Self {
        let bell = Arc::new(Bell::default());
        Fast {
            streams: AtomicUsize::new(0),
            ring: Mutex::new(Ring {
                next_seq: 1,
                ..Ring::default()
            }),
            waker: Waker::from(bell.clone()),
            bell,
        }
    }
}

/// The facts of a call's items, as the fast path hands them out; `None` when no stream is open or it holds none.
pub(crate) fn facts_of(fast: &Fast, items: &[Item], wall: i64) -> Option<Box<[ProvisionalFact]>> {
    if fast.streams.load(Ordering::SeqCst) == 0 {
        return None;
    }
    let facts: Box<[ProvisionalFact]> = items
        .iter()
        .filter_map(|i| match i {
            Item::Fact(row) => {
                let f = fact_out(&Stored {
                    position: LogPos::default(),
                    received_at: wall,
                    row: row.clone(),
                });
                Some(ProvisionalFact {
                    series: f.series,
                    t: f.t,
                    value: f.value,
                    quality: f.quality,
                    supplied_quality: f.supplied_quality,
                    flags: f.flags,
                    changed_at: f.changed_at,
                })
            }
            Item::Entry(_) => None,
        })
        .collect();
    (!facts.is_empty()).then_some(facts)
}

impl Fast {
    /// Hands an accepted call to every open stream. Called in call order (under the decision lock); never waits
    /// for a stream.
    pub(crate) fn publish(
        &self,
        facts: Box<[ProvisionalFact]>,
        floor: LogPos,
        completion: Arc<Completion>,
    ) {
        let _ = completion.poll(Some(&Context::from_waker(&self.waker)));
        self.push(facts, floor, Outcome::Completion(completion));
    }

    fn push(&self, facts: Box<[ProvisionalFact]>, floor: LogPos, outcome: Outcome) -> Arc<Entry> {
        let mut r = lock(&self.ring);
        let seq = r.next_seq;
        r.next_seq += 1;
        r.facts += facts.len() as u64;
        let e = Arc::new(Entry {
            seq,
            upto: r.facts,
            floor,
            facts,
            outcome,
        });
        r.entries.push_back(e.clone());
        r.keep();
        drop(r);
        self.bell.ring(false);
        e
    }

    /// The store is shut down or dropped.
    pub(crate) fn close(&self) {
        self.bell.ring(true);
    }

    /// Opens a stream at the next call.
    pub(crate) fn open(
        self: &Arc<Self>,
        o: &ProvisionalOptions,
        signals: Box<dyn Signals>,
    ) -> Stream {
        let mut r = lock(&self.ring);
        let id = r.next_stream;
        r.next_stream += 1;
        r.wants.insert(id, o.max_behind);
        self.streams.fetch_add(1, Ordering::SeqCst);
        let cursor = r.next_seq;
        drop(r);
        let after = signals.durable();
        Stream {
            fast: self.clone(),
            id,
            signals,
            series: (!o.series.is_empty()).then(|| o.series.iter().map(|s| s.0).collect()),
            max_behind: o.max_behind as u64,
            cursor,
            after,
            pending: VecDeque::new(),
            through: cursor - 1,
            delivered: cursor - 1,
            recheck: false,
            out: VecDeque::new(),
            closed: false,
        }
    }
}

/// One provisional stream's state (`crate::Provisional`).
#[derive(Debug)]
pub(crate) struct Stream {
    fast: Arc<Fast>,
    id: u64,
    signals: Box<dyn Signals>,
    series: Option<HashSet<u32>>,
    max_behind: u64,
    /// The next call to look at.
    cursor: u64,
    /// Below every fact of a call not yet looked at.
    after: LogPos,
    /// Calls delivered provisional and not yet settled, in call order.
    pending: VecDeque<Arc<Entry>>,
    through: u64,
    /// The last call delivered.
    delivered: u64,
    /// The oldest pending call is acknowledged ahead of its sync, which rings no bell.
    recheck: bool,
    out: VecDeque<ProvisionalEvent>,
    closed: bool,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut r = lock(&self.fast.ring);
        r.wants.remove(&self.id);
        self.fast.streams.fetch_sub(1, Ordering::SeqCst);
        r.keep();
    }
}

impl Stream {
    pub(crate) fn next(&mut self, timeout: Duration) -> Option<ProvisionalEvent> {
        let until = Instant::now() + timeout;
        loop {
            if let Some(e) = self.out.pop_front() {
                return Some(e);
            }
            if self.closed {
                return None;
            }
            let rung = lock(&self.fast.bell.state).0;
            self.poll();
            if let Some(e) = self.out.pop_front() {
                return Some(e);
            }
            let now = Instant::now();
            if now >= until {
                return None;
            }
            let mut wait = until - now;
            let s = lock(&self.fast.bell.state);
            if self.recheck || s.1 {
                wait = wait.min(RECHECK);
            }
            if s.0 == rung {
                drop(
                    self.fast
                        .bell
                        .cv
                        .wait_timeout_while(s, wait, |s| s.0 == rung)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
            }
        }
    }

    /// Queues every event due now, in this order: `Lagged`, `Withdrawn`, `Durable`, `Accepted`, `Closed`.
    fn poll(&mut self) {
        let closing = lock(&self.fast.bell.state).1;
        let stopped = self.signals.stopped();
        let ended = self.signals.ended();
        let mark = self.signals.durable();
        let fresh = self.take_fresh();
        // A run of withdrawn calls ends at a durable one, which a `Withdrawn` range must not name.
        let mut runs: Vec<(u64, u64, Option<Withdrawal>)> = Vec::new();
        while let Some(e) = self.pending.front() {
            let w = match fate(e, mark, ended) {
                Fate::Open => break,
                Fate::Durable => None,
                Fate::Withdrawn(c) => Some(c),
            };
            match runs.last_mut() {
                Some((_, last, rc)) if w.is_some() && *rc == w => *last = e.seq,
                _ => runs.push((e.seq, e.seq, w)),
            }
            self.pending.pop_front();
        }
        for (first, last, cause) in runs {
            if let Some(cause) = cause {
                self.out
                    .push_back(ProvisionalEvent::Withdrawn { first, last, cause });
            }
        }
        let through = self.pending.front().map_or(self.delivered, |e| e.seq - 1);
        if through > self.through {
            self.through = through;
            self.out.push_back(ProvisionalEvent::Durable {
                through,
                mark: pos(mark),
            });
        }
        for e in fresh {
            let pick = match &self.series {
                None => None,
                Some(s) => {
                    let idx: Vec<u32> = (0..e.facts.len() as u32)
                        .filter(|&i| s.contains(&e.facts[i as usize].series.0))
                        .collect();
                    if idx.is_empty() {
                        continue;
                    }
                    (idx.len() < e.facts.len()).then(|| Arc::from(idx))
                }
            };
            let durable = match fate(&e, mark, ended) {
                Fate::Withdrawn(_) => continue,
                Fate::Durable => true,
                Fate::Open => {
                    self.pending.push_back(e.clone());
                    false
                }
            };
            self.delivered = e.seq;
            self.out.push_back(ProvisionalEvent::Accepted(Accepted {
                entry: e,
                pick,
                durable,
            }));
        }
        self.recheck = self
            .pending
            .front()
            .is_some_and(|e| matches!(e.result(), Some(Ok(_))));
        if closing && stopped && self.pending.is_empty() && self.out.is_empty() {
            self.closed = true;
            self.out.push_back(ProvisionalEvent::Closed);
        }
    }

    /// The calls past the cursor, after a `Lagged` if the stream fell behind; moves the cursor past them.
    fn take_fresh(&mut self) -> Vec<Arc<Entry>> {
        let r = lock(&self.fast.ring);
        let Some(front) = r.entries.front() else {
            self.cursor = r.next_seq;
            return Vec::new();
        };
        let behind = |e: &Entry| r.facts - e.before();
        let mut start = self.cursor.max(front.seq);
        let at = |seq: u64| (seq - front.seq) as usize;
        let stale = self.cursor < front.seq
            || r.entries
                .get(at(start))
                .is_some_and(|e| behind(e) > self.max_behind);
        if stale {
            start = r
                .entries
                .iter()
                .find(|e| e.seq >= start && behind(e) <= self.max_behind)
                .map_or(r.next_seq - 1, |e| e.seq)
                .max(start);
            self.out.push_back(ProvisionalEvent::Lagged {
                first: self.cursor,
                last: start - 1,
                after: pos(self.after),
            });
        }
        let fresh: Vec<Arc<Entry>> = r
            .entries
            .range(at(start).min(r.entries.len())..)
            .cloned()
            .collect();
        self.cursor = r.next_seq;
        drop(r);
        if let Some(e) = fresh.last() {
            self.after = e.floor;
        }
        fresh
    }
}

#[cfg(test)]
mod tests;
