//! The fast path (spec §4 Provisional streams, Q-1109): each accepted call goes, before it is durable, into one ring shared by
//! every provisional stream; a stream learns each call's fate from its completion and the durable position.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
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

/// Waits for the writer's own outcome while it can still give one: after a fault the log fails every queued call,
/// so only a stopped writer leaves a call unresolved (spec §4 Provisional streams: the outcome its writer was given).
fn fate(e: &Entry, at: &Read) -> Fate {
    match e.result() {
        Some(Ok(a)) if a.last <= at.mark => Fate::Durable,
        Some(Ok(_)) if at.ended => Fate::Withdrawn(Withdrawal::OutcomeUnknown),
        Some(Ok(_)) => Fate::Open,
        Some(Err(CommitError::OutcomeUnknown)) => Fate::Withdrawn(Withdrawal::OutcomeUnknown),
        Some(Err(_)) => Fate::Withdrawn(Withdrawal::NotStored),
        None if at.stopped => Fate::Withdrawn(Withdrawal::OutcomeUnknown),
        None => Fate::Open,
    }
}

/// The writer's progress as one poll reads it, in this order: stopped, ended, then the durable position.
struct Read {
    stopped: bool,
    ended: bool,
    mark: LogPos,
}

#[derive(Debug)]
struct Ring {
    entries: VecDeque<Arc<Entry>>,
    next_seq: u64,
    facts: u64,
    /// Each open stream's `max_behind`, by stream id: the ring keeps the largest.
    wants: BTreeMap<u64, usize>,
    next_stream: u64,
}

impl Default for Ring {
    fn default() -> Self {
        Ring {
            entries: VecDeque::new(),
            next_seq: 1,
            facts: 0,
            wants: BTreeMap::new(),
            next_stream: 0,
        }
    }
}

impl Ring {
    /// Moves out the entries no open stream needs. The caller drops them after releasing the ring's lock, which
    /// `publish` takes: freeing a full ring takes tens of milliseconds (spec §4 Provisional streams bounds the hold).
    #[must_use = "drop the evicted entries once the ring's lock is released"]
    fn keep(&mut self) -> VecDeque<Arc<Entry>> {
        let cap = self.wants.values().copied().max().unwrap_or(0) as u64;
        if cap == 0 {
            return std::mem::take(&mut self.entries);
        }
        let n = self
            .entries
            .partition_point(|f| self.facts - f.before() > cap)
            .min(self.entries.len().saturating_sub(1));
        self.entries.drain(..n).collect()
    }
}

/// Wakes one waiting stream: a call it selects accepted or settled, or the store closing.
#[derive(Debug, Default)]
struct Bell {
    rung: Mutex<u64>,
    cv: Condvar,
}

impl Bell {
    fn ring(&self) {
        *lock(&self.rung) += 1;
        self.cv.notify_all();
    }
}

/// An open stream that selects series, with its own bell.
#[derive(Debug)]
struct Listener {
    series: Arc<HashSet<u32>>,
    bell: Arc<Bell>,
}

/// Rings the streams that select a call's series once the call settles.
#[derive(Debug)]
struct Settled {
    fast: Weak<Fast>,
    series: Box<[u32]>,
}

impl Wake for Settled {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(f) = self.fast.upgrade() {
            f.ring_for(&self.series);
        }
    }
}

/// Most calls a stream takes from the ring in one hold of its lock, which `publish` also takes: the bound on how
/// long a stream catching up holds up the writer (spec §4 Provisional streams).
pub(crate) const BATCH: usize = 4096;

/// The writer's side of the fast path.
#[derive(Debug, Default)]
pub(crate) struct Fast {
    streams: AtomicUsize,
    closing: AtomicBool,
    ring: Mutex<Ring>,
    /// Shared by the streams of every series: one wake per call for them all.
    every: Arc<Bell>,
    every_open: AtomicUsize,
    listeners: Mutex<BTreeMap<u64, Listener>>,
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

fn series_of(facts: &[ProvisionalFact]) -> Box<[u32]> {
    let mut s: Vec<u32> = facts.iter().map(|f| f.series.0).collect();
    s.sort_unstable();
    s.dedup();
    s.into()
}

impl Fast {
    /// Hands an accepted call to every open stream. Called in call order (under the decision lock); never waits
    /// for a stream. Returns the call's series, for [`Fast::ring_for`] once the decision lock is released.
    pub(crate) fn publish(
        self: &Arc<Self>,
        facts: Box<[ProvisionalFact]>,
        floor: LogPos,
        completion: Arc<Completion>,
    ) -> Box<[u32]> {
        let series = series_of(&facts);
        let settled = Waker::from(Arc::new(Settled {
            fast: Arc::downgrade(self),
            series: series.clone(),
        }));
        let _ = completion.poll(Some(&Context::from_waker(&settled)));
        self.push(facts, floor, Outcome::Completion(completion));
        series
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
        let evicted = r.keep();
        drop(r);
        drop(evicted);
        e
    }

    /// Rings every stream that selects one of `series`.
    pub(crate) fn ring_for(&self, series: &[u32]) {
        if self.every_open.load(Ordering::SeqCst) > 0 {
            self.every.ring();
        }
        for l in lock(&self.listeners).values() {
            if series.iter().any(|x| l.series.contains(x)) {
                l.bell.ring();
            }
        }
    }

    /// The store is shut down or dropped.
    pub(crate) fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.every.ring();
        for l in lock(&self.listeners).values() {
            l.bell.ring();
        }
    }

    /// Opens a stream at the next call.
    pub(crate) fn open(
        self: &Arc<Self>,
        o: &ProvisionalOptions,
        signals: Box<dyn Signals>,
    ) -> Stream {
        let series: Option<Arc<HashSet<u32>>> =
            (!o.series.is_empty()).then(|| Arc::new(o.series.iter().map(|s| s.0).collect()));
        let mut r = lock(&self.ring);
        let id = r.next_stream;
        r.next_stream += 1;
        r.wants.insert(id, o.max_behind);
        let bell = match &series {
            None => {
                self.every_open.fetch_add(1, Ordering::SeqCst);
                self.every.clone()
            }
            Some(series) => {
                let bell = Arc::new(Bell::default());
                let l = Listener {
                    series: series.clone(),
                    bell: bell.clone(),
                };
                lock(&self.listeners).insert(id, l);
                bell
            }
        };
        self.streams.fetch_add(1, Ordering::SeqCst);
        let cursor = r.next_seq;
        drop(r);
        let after = signals.durable();
        Stream {
            fast: self.clone(),
            id,
            bell,
            signals,
            series,
            max_behind: o.max_behind as u64,
            cursor,
            more: false,
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
    bell: Arc<Bell>,
    signals: Box<dyn Signals>,
    series: Option<Arc<HashSet<u32>>>,
    max_behind: u64,
    /// The next call to look at.
    cursor: u64,
    /// The last take stopped at [`BATCH`] with calls left past the cursor.
    more: bool,
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
        match self.series {
            None => _ = self.fast.every_open.fetch_sub(1, Ordering::SeqCst),
            Some(_) => _ = lock(&self.fast.listeners).remove(&self.id),
        }
        self.fast.streams.fetch_sub(1, Ordering::SeqCst);
        let evicted = r.keep();
        drop(r);
        drop(evicted);
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
            let rung = *lock(&self.bell.rung);
            self.poll();
            if let Some(e) = self.out.pop_front() {
                return Some(e);
            }
            if self.more {
                continue;
            }
            let now = Instant::now();
            if now >= until {
                return None;
            }
            let mut wait = until - now;
            if self.recheck || self.fast.closing.load(Ordering::SeqCst) {
                wait = wait.min(RECHECK);
            }
            let s = lock(&self.bell.rung);
            if *s == rung {
                drop(
                    self.bell
                        .cv
                        .wait_timeout_while(s, wait, |s| *s == rung)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
            }
        }
    }

    /// Queues every event due now, in this order: `Lagged`, `Withdrawn`, `Durable`, `Accepted`, `Closed`.
    fn poll(&mut self) {
        let closing = self.fast.closing.load(Ordering::SeqCst);
        let at = Read {
            stopped: self.signals.stopped(),
            ended: self.signals.ended(),
            mark: self.signals.durable(),
        };
        let fresh = self.take_fresh();
        // A run of withdrawn calls ends at a durable one, which a `Withdrawn` range must not name.
        let mut runs: Vec<(u64, u64, Option<Withdrawal>)> = Vec::new();
        let mut settled_durable = false;
        while let Some(e) = self.pending.front() {
            let w = match fate(e, &at) {
                Fate::Open => break,
                Fate::Durable => None,
                Fate::Withdrawn(c) => Some(c),
            };
            settled_durable |= w.is_none();
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
            // `api.md` Provisional streams: a watermark is sent only when it covers a call delivered provisional.
            if settled_durable {
                self.out.push_back(ProvisionalEvent::Durable {
                    through,
                    mark: pos(at.mark),
                });
            }
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
            let durable = match fate(&e, &at) {
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
        if closing && at.stopped && !self.more && self.pending.is_empty() && self.out.is_empty() {
            self.closed = true;
            self.out.push_back(ProvisionalEvent::Closed);
        }
    }

    /// Up to [`BATCH`] calls past the cursor, after a `Lagged` if the stream fell behind; moves the cursor past them.
    pub(crate) fn take_fresh(&mut self) -> Vec<Arc<Entry>> {
        let r = lock(&self.fast.ring);
        let Some(front) = r.entries.front() else {
            self.cursor = r.next_seq;
            self.more = false;
            return Vec::new();
        };
        let front_seq = front.seq;
        // How far behind each call lies falls along the ring, so the oldest call within bound is a bisection.
        let within = r
            .entries
            .partition_point(|e| r.facts - e.before() > self.max_behind);
        let oldest_within = r.entries.get(within).map_or(r.next_seq, |e| e.seq);
        // A call larger than `max_behind` on its own is delivered whole (spec §4 Provisional streams).
        let start = self
            .cursor
            .max(front_seq)
            .max(oldest_within.min(r.next_seq - 1));
        if start > self.cursor {
            self.out.push_back(ProvisionalEvent::Lagged {
                first: self.cursor,
                last: start - 1,
                after: pos(self.after),
            });
        }
        let from = ((start - front_seq) as usize).min(r.entries.len());
        let fresh: Vec<Arc<Entry>> = r.entries.range(from..).take(BATCH).cloned().collect();
        self.cursor = start + fresh.len() as u64;
        self.more = self.cursor < r.next_seq;
        drop(r);
        if let Some(e) = fresh.last() {
            self.after = e.floor;
        }
        fresh
    }
}

#[cfg(test)]
mod tests;
