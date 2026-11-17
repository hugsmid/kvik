//! Delivery on the durable backend (`spec.md` §4, §6; roadmap 5.2): consumers, their bounded queues, condition
//! funnels, latches and durable cursors.
//!
//! Everything is read from the committed log. A [`Deliverer`] holds its own log reader and takes each batch, in
//! commit order, once it is acknowledged (a batch holding an entry that is not a fact, or the first batch in a
//! segment (Q-630), once the mark is synced past it, and every later batch behind it). Taking a batch applies its entries to the routing table and registry,
//! enqueues its facts for the live consumers whose fact funnels select them, and evaluates their condition funnels
//! ([`funnels`]) as the reference model does (`kvik_model::condition`), on the live state of roadmap 5.4. A consumer
//! that is behind (resuming after a restart, or whose queue filled) reads the log from its position with the routes
//! in force at each position, pausing at every reset point for a `resync` carrying `forget_above`, and ends a resume
//! with a `resync` that carries each funnel's state now ([`resync`]). Confirmations move the cursor and the latches,
//! which the cursor file holds ([`confirm`]).
//!
//! The work is done on the calling thread of the call that needs it (an emit's acknowledgement, a poll, a
//! confirmation, [`crate::Store::tick`]) and by the `kvik-deliver` thread ([`Hub`]), never on the writer thread.
//! Timers run on `Env`'s monotonic clock; `now` and live staleness are the writer's ([`super::clock`]).
//!
//! Not here yet: windowed operators in funnels, starting at a position or a time, the saved-point check, timer
//! sequences in the cursor file and the catch-up statistics (roadmap 5.3).

mod confirm;
mod funnels;
mod hub;
mod resync;

pub(crate) use hub::Hub;

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::cursor::{self, Latch};
use kvik_log::entries::Entry;
use kvik_log::manifest::LossCause;
use kvik_log::reader::{LogReader, ReadBatch, ReadEvent, ReaderConfig as LogReaderConfig};
use kvik_log::writer::{Position as LogPos, Watch};
use kvik_model::condition::{ConditionState as MState, Runner};

use super::LogClock;
use super::clock::Clocks;
use super::index::{Index, Stored, pos};
use super::routing::{View, funnel_out};
use crate::backend::Error;
use crate::consume::{Delivery, Funnel, FunnelId, ResyncCause, Start, TriggerCause};
use crate::env::Env;
use crate::health::{ConsumerStats, DeliveryStats};
use crate::types::SavedPoint;

/// How often a consumer's cursor file is written at most, and the timer cadence (`spec.md` §4 Consumer state, §6
/// Cadence).
pub(crate) const SECOND: i64 = 1_000_000_000;

fn next(p: LogPos) -> LogPos {
    LogPos {
        generation: p.generation,
        sequence: p.sequence + 1,
    }
}

fn before(p: LogPos) -> LogPos {
    LogPos {
        generation: p.generation,
        sequence: p.sequence.saturating_sub(1),
    }
}

/// What a call gives the deliverer: the writer's progress and the clocks now.
pub(crate) struct Ctx<'a> {
    pub(crate) watch: Option<&'a Watch>,
    pub(crate) env: &'a Env,
    pub(crate) dir: &'a Path,
    /// The writer's window clock and live staleness instants.
    pub(crate) clocks: &'a Clocks,
    /// The store's `now` (`spec.md` §6).
    pub(crate) now: i64,
    pub(crate) wall: i64,
    pub(crate) mono: i64,
}

/// A kept fact and whether it was outranked when it was committed (`spec.md` §4 Outranked facts).
#[derive(Debug)]
struct FactRec {
    stored: Stored,
    outranked: bool,
}

/// A record of the kept log; facts are shared with the queues that hold them.
#[derive(Clone, Debug)]
enum Rec {
    Fact(Arc<FactRec>),
    Entry(Entry),
}

/// A funnel's part of a resync.
#[derive(Clone, Debug)]
struct FState {
    funnel: u32,
    incarnation: LogPos,
    live: Vec<(u32, crate::read::Live)>,
    condition: Option<(MState, bool)>,
}

/// A delivery waiting in a consumer's queue.
#[derive(Clone, Debug)]
enum Pend {
    Fact {
        rec: Arc<FactRec>,
        replayed: bool,
    },
    Trigger {
        funnel: u32,
        incarnation: LogPos,
        sequence: u64,
        state: MState,
        cause: TriggerCause,
        position: LogPos,
    },
    Resync {
        cause: ResyncCause,
        forget_above: Option<LogPos>,
        position: LogPos,
        funnels: Vec<FState>,
        /// It is, or ends with, a reset point: its confirmation records its id in the cursor file (field 3).
        reset: bool,
    },
}

/// A delivery handed out and not yet confirmed.
#[derive(Clone, Debug)]
struct Out {
    id: crate::consume::DeliveryId,
    position: LogPos,
    /// The condition states it carries: funnel, incarnation, state.
    latches: Vec<(u32, LogPos, MState)>,
    /// A resync carrying `forget_above`: its confirmation writes the cursor file at once.
    forced: bool,
    /// A reset point's resync.
    reset: bool,
}

/// A consumer as this process runs it.
#[derive(Debug, Default)]
struct Rt {
    /// A handle is live.
    handle: bool,
    /// A handle was taken in this run.
    attached: bool,
    /// Known (routes or an unreadable cursor file) but without a cursor, and not taken in this run: it starts with a
    /// resync when it is (`spec.md` §4 Resync, a consumer with no cursor file).
    dormant: bool,
    queue: VecDeque<Pend>,
    parts: VecDeque<(Delivery, Out)>,
    unconfirmed: Vec<Out>,
    /// The last part of a resync was handed out: nothing further until it is confirmed.
    gate: Option<u32>,
    /// Reading from the log from this position (a resume or a catch-up).
    behind: Option<LogPos>,
    /// The routes and registry as of the position before `behind`, while behind.
    view: Option<(LogPos, View)>,
    /// A resume replays up to this position, then sends its resync.
    resume: Option<LogPos>,
    /// A reset point's resync is queued and nothing routed has followed it yet.
    reset_open: bool,
    /// Reading stopped at a reset point until its resync is confirmed and its cursor file synced.
    paused: bool,
    runners: BTreeMap<u32, Runner>,
    emitted: BTreeMap<u32, MState>,
    /// The `now` of each condition funnel's last evaluation, to tell whether its timer is due.
    evaluated: BTreeMap<u32, i64>,
    // Durable state.
    has_cursor: bool,
    cursor: LogPos,
    latches: BTreeMap<u32, Latch>,
    reset: Option<LogPos>,
    file_end: Option<u64>,
    dirty: bool,
    last_write: Option<i64>,
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Deliverer {
    reader: Option<LogReader>,
    store_id: [u8; 16],
    index: Index,
    /// The kept log by position, from `items_from` up (the lowest cursor); below it, a consumer that needs older
    /// records has them rebuilt from the index ([`Deliverer::restore_items`]).
    items: BTreeMap<LogPos, Rec>,
    items_from: LogPos,
    /// Every entry that is not a fact, by position: the routes and registry at any position.
    entries: BTreeMap<LogPos, Entry>,
    /// Batches read and not yet taken.
    pending: VecDeque<ReadBatch>,
    /// The highest `changed_at` per key of a versioned series.
    top: BTreeMap<(u32, i64), u64>,
    /// Per series, the receive time of its last fact that was not outranked (`spec.md` §6 Restart and resume).
    last_arrival: BTreeMap<u32, i64>,
    view: View,
    head: LogPos,
    /// Reset points by id: their `forget_above` (`spec.md` §4 Resync).
    resets: BTreeMap<LogPos, LogPos>,
    /// Per consumer named in a recovery entry (field 47: its cursor file was missing or unreadable), the lowest
    /// `after` among those entries.
    lost: BTreeMap<String, LogPos>,
    consumers: BTreeMap<String, Rt>,
    /// Condition delivery sequences, per consumer and funnel, from 1 at every start (`spec.md` §4 Order).
    sequences: BTreeMap<(String, u32), u64>,
    capacity: usize,
    max_consumers: usize,
    wakers: Vec<Waker>,
    /// Something a consumer may be waiting for changed since the wakers were last woken.
    notify: bool,
    /// A cursor file write or sync failed: no cursor file is written again and nothing further is delivered until a
    /// restart (`spec.md` §3 Write or sync failure: a failed sync is never retried).
    failed: bool,
    stats: DeliveryStats,
    loading: bool,
}

impl Deliverer {
    /// Opens the deliverer on the kept log, before the writer's threads take calls: reads the whole log, the
    /// manifest's lost ranges and every cursor file, and sets each known consumer up to resume (`spec.md` §3 Order of
    /// a start, step 7).
    pub(crate) fn open(
        dir: &Path,
        env: &Env,
        clocks: &Clocks,
        store_id: [u8; 16],
        capacity: usize,
        max_consumers: usize,
    ) -> io::Result<Deliverer> {
        let mut id = [0u8; 16];
        env.entropy.fill(&mut id);
        let mut cfg = LogReaderConfig::new(
            dir,
            env.fs.clone(),
            Arc::new(LogClock::new(env.clock.clone())),
            env.boot.current(),
            id,
        );
        cfg.lease_refresh = Duration::from_secs(30 * 60);
        let reader = LogReader::open(cfg).map_err(|e| io::Error::other(e.to_string()))?;
        let mut d = Deliverer {
            reader: Some(reader),
            store_id,
            index: Index::default(),
            items: BTreeMap::new(),
            items_from: LogPos::default(),
            entries: BTreeMap::new(),
            pending: VecDeque::new(),
            top: BTreeMap::new(),
            last_arrival: BTreeMap::new(),
            view: View::default(),
            head: LogPos::default(),
            resets: BTreeMap::new(),
            lost: BTreeMap::new(),
            consumers: BTreeMap::new(),
            sequences: BTreeMap::new(),
            capacity,
            max_consumers,
            wakers: Vec::new(),
            notify: false,
            failed: false,
            stats: DeliveryStats::default(),
            loading: true,
        };
        let ctx = Ctx {
            watch: None,
            env,
            dir,
            clocks,
            now: 0,
            wall: 0,
            mono: 0,
        };
        // The kept log, read whole before anything is delivered: a read that fails stops the open.
        let first = d
            .reader
            .as_mut()
            .map(LogReader::refresh)
            .transpose()
            .map_err(|e| io::Error::other(e.to_string()))?;
        d.take_events(first.map(|u| u.events).unwrap_or_default());
        d.pump(&ctx);
        d.loading = false;
        // Lost ranges not described by a recovery entry are reset points: `forget_above` the position before, id
        // its last position (`spec.md` §4 Resync).
        let lost: Vec<_> = d
            .reader
            .as_ref()
            .map(|r| r.lost_ranges().to_vec())
            .unwrap_or_default();
        for l in lost {
            let first = LogPos {
                generation: l.first.0,
                sequence: l.first.1,
            };
            let last = LogPos {
                generation: l.last.0,
                sequence: l.last.1,
            };
            let described = l.cause == LossCause::TruncatedTail
                && d.resets
                    .get(&next(last))
                    .is_some_and(|after| *after == before(first));
            if !described {
                let fa = before(first);
                let e = d.resets.entry(last).or_insert(fa);
                *e = (*e).min(fa);
            }
        }
        // Consumers with a cursor file resume after it; known ones without a readable one start without a cursor.
        for (name, r) in cursor::read_all_cursors(&*env.fs, dir, &store_id) {
            let rt = d.consumers.entry(name).or_default();
            match r {
                Ok(f) => {
                    rt.has_cursor = true;
                    rt.cursor = f.state.position;
                    rt.latches = f.state.latches.iter().map(|l| (l.funnel, *l)).collect();
                    rt.reset = f.state.reset;
                    rt.file_end = Some(f.end);
                    rt.behind = Some(next(f.state.position));
                    rt.resume = Some(d.head);
                }
                Err(_) => rt.dormant = true,
            }
        }
        let known: Vec<String> = d
            .view
            .consumers
            .keys()
            .chain(d.lost.keys())
            .cloned()
            .collect();
        for name in known {
            d.consumers.entry(name).or_insert_with(|| Rt {
                dormant: true,
                ..Rt::default()
            });
        }
        d.prune();
        Ok(d)
    }

    /// The routes and registry in force.
    pub(crate) fn view(&self) -> &View {
        &self.view
    }

    /// Counts of what taking the log met.
    pub(crate) fn delivery_stats(&self) -> DeliveryStats {
        self.stats.clone()
    }

    fn take_events(&mut self, events: Vec<ReadEvent>) {
        for e in events {
            match e {
                ReadEvent::Batch(b) => self.pending.push_back(b),
                ReadEvent::Void { above, .. } => self.void(above),
                // A region below the mark: the writer's next start records the loss; positions in it are never
                // delivered.
                ReadEvent::Corruption { .. } => self.stats.corruptions += 1,
            }
        }
    }

    /// Takes the batches acknowledged since the last call.
    pub(crate) fn pump(&mut self, ctx: &Ctx<'_>) {
        let Some(r) = self.reader.as_mut() else {
            return;
        };
        // A refresh that fails changes nothing the reader holds; the next one reads it again. It is counted.
        match r.refresh() {
            Ok(u) => self.take_events(u.events),
            Err(_) => self.stats.refresh_errors += 1,
        }
        let stats = ctx.watch.map(Watch::stats);
        let mut took = false;
        while let Some(b) = self.pending.front() {
            if let Some(st) = &stats {
                let entry = b.content.items.iter().any(|i| matches!(i, Item::Entry(_)));
                if b.last > st.durable || (entry && b.last > st.mark_synced) {
                    break;
                }
            }
            let b = self.pending.pop_front().expect("looked at above");
            self.take_batch(b, ctx);
            took = true;
        }
        if took {
            self.prune();
        }
    }

    /// Drops the records no consumer can read again: at or below the lowest cursor (a consumer without one starts
    /// at the last position).
    fn prune(&mut self) {
        let floor = self
            .consumers
            .values()
            .filter(|rt| rt.has_cursor)
            .map(|rt| rt.cursor)
            .chain(
                self.consumers
                    .values()
                    .filter_map(|rt| rt.behind.map(before)),
            )
            .min()
            .unwrap_or(self.head);
        let keep = self.items.split_off(&next(floor));
        self.items = keep;
        self.items_from = self.items_from.max(next(floor));
    }

    /// Rebuilds the records from `at` up to where they are kept, from the index's facts and the entries.
    fn restore_items(&mut self, at: LogPos) {
        if at >= self.items_from {
            return;
        }
        let to = self.items_from;
        for (p, e) in self.entries.range(at..to) {
            self.items.insert(*p, Rec::Entry(e.clone()));
        }
        let mut facts = Vec::new();
        for id in self.index.fact_series() {
            // In `(t, position)` order, so the facts of one key are neighbours.
            let all: Vec<&Stored> = self.index.series_facts(id).collect();
            for (k, f) in all.iter().enumerate() {
                if f.position < at || f.position >= to {
                    continue;
                }
                // Outranked at commit: an earlier fact of the key with a higher `changed_at` (`spec.md` §4).
                let outranked = f.row.changed_at.is_some_and(|c| {
                    all[..k]
                        .iter()
                        .rev()
                        .take_while(|g| g.row.t == f.row.t)
                        .chain(all[k + 1..].iter().take_while(|g| g.row.t == f.row.t))
                        .any(|g| {
                            g.position < f.position && g.row.changed_at.is_some_and(|gc| gc > c)
                        })
                });
                facts.push(((*f).clone(), outranked));
            }
        }
        for (stored, outranked) in facts {
            self.items.insert(
                stored.position,
                Rec::Fact(Arc::new(FactRec { stored, outranked })),
            );
        }
        self.items_from = at;
    }

    fn void(&mut self, above: LogPos) {
        self.pending.retain(|b| b.last <= above);
        self.index.apply(&[ReadEvent::Void {
            above,
            cause: kvik_log::reader::VoidCause::Rewritten,
        }]);
        self.items.retain(|p, _| *p <= above);
        self.items_from = self.items_from.min(next(above));
        self.entries.retain(|p, _| *p <= above);
        self.resets.retain(|p, _| *p <= above);
        self.head = self.head.min(above);
        let mut view = View::default();
        for (p, e) in &self.entries {
            view.apply(*p, e);
        }
        self.view = view;
    }

    fn take_batch(&mut self, b: ReadBatch, ctx: &Ctx<'_>) {
        self.index.apply(&[ReadEvent::Batch(b.clone())]);
        let mut fed: Vec<u32> = Vec::new();
        let mut sequence = b.first.sequence;
        let names: Vec<String> = self.consumers.keys().cloned().collect();
        for item in b.content.items {
            let p = LogPos {
                generation: b.first.generation,
                sequence,
            };
            sequence += item.sequences();
            match item {
                Item::Fact(row) => {
                    let key = (row.series_id, row.t);
                    let outranked = row
                        .changed_at
                        .is_some_and(|c| self.top.get(&key).is_some_and(|t| c < *t));
                    if let Some(c) = row.changed_at {
                        let t = self.top.entry(key).or_insert(c);
                        *t = (*t).max(c);
                    }
                    let rec = Arc::new(FactRec {
                        stored: Stored {
                            position: p,
                            received_at: b.content.received_at,
                            row,
                        },
                        outranked,
                    });
                    let series = rec.stored.row.series_id;
                    if !outranked {
                        fed.push(series);
                        self.last_arrival.insert(series, b.content.received_at);
                    }
                    if !self.loading {
                        for name in &names {
                            self.deliver_live(name, &rec);
                        }
                    }
                    self.items.insert(p, Rec::Fact(rec));
                }
                Item::Entry(Entry::Ignored { .. }) => {}
                Item::Entry(e) => {
                    self.view.apply(p, &e);
                    if let Entry::Recovery(r) = &e {
                        let after = LogPos {
                            generation: p.generation,
                            sequence: r.after,
                        };
                        self.resets.insert(p, after);
                        for c in &r.consumers {
                            let l = self.lost.entry(c.clone()).or_insert(after);
                            *l = (*l).min(after);
                        }
                    }
                    let routing = matches!(e, Entry::Routing(_));
                    self.entries.insert(p, e.clone());
                    self.items.insert(p, Rec::Entry(e));
                    if routing && !self.loading {
                        for name in &names {
                            self.sync_runners(name, ctx.mono);
                        }
                    }
                }
            }
        }
        self.head = self.head.max(b.last);
        if !self.loading {
            self.notify = true;
            for name in &names {
                self.evaluate(name, Some(&fed), ctx);
            }
        }
    }

    fn deliver_live(&mut self, name: &str, rec: &Arc<FactRec>) {
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        if rt.dormant || rt.behind.is_some() || !self.view.routed(name, rec.stored.row.series_id) {
            return;
        }
        if rt.queue.len() < self.capacity {
            rt.queue.push_back(Pend::Fact {
                rec: rec.clone(),
                replayed: false,
            });
        } else {
            // Catch up from the first position not enqueued in this session (`spec.md` §4 Overflow).
            rt.behind = Some(rec.stored.position);
        }
    }

    /// The position before a start (`spec.md` §4 New consumer, Resync): the last position for `start: now`, the
    /// last position committed before `T` for `start: time` (the position before the first fact at or after `T`),
    /// and for `start: position` the saved point judged against the resets since it was read: kept, its position;
    /// void above X, X. An unknown point refuses the registration.
    fn start_point(&self, start: Start) -> Result<LogPos, Error> {
        match start {
            Start::Now => Ok(self.head),
            Start::Time(t) => Ok(self.index.position_at(t.0.saturating_sub(1)).min(self.head)),
            Start::Position(p) => self.judge(p),
        }
    }

    /// A saved point judged against the current mark (`spec.md` §7 Readers, Reset stamp and saved points).
    fn judge(&self, point: SavedPoint) -> Result<LogPos, Error> {
        let p = super::index::log_pos(point.position);
        let current = self
            .reader
            .as_ref()
            .map(|r| r.state().stamp)
            .unwrap_or_default();
        let saved = (point.stamp.sequence, point.stamp.nonce);
        let carries = |st: Option<kvik_log::mark::ResetStamp>, s: (u64, u64)| {
            st.is_some_and(|st| (st.sequence, st.nonce) == s)
        };
        if p.generation == self.head.generation && (current.sequence, current.nonce) == saved {
            return Ok(p.min(self.head));
        }
        let recoveries: Vec<(LogPos, &kvik_log::entries::Recovery)> = self
            .entries
            .iter()
            .filter_map(|(q, e)| match e {
                Entry::Recovery(r) => Some((*q, r)),
                _ => None,
            })
            .collect();
        // The reset the mark names must be kept, or nothing can be told.
        if current.nonce != 0
            && !recoveries
                .iter()
                .any(|(_, r)| carries(r.stamp, (current.sequence, current.nonce)))
        {
            return Err(Error::UnknownPoint);
        }
        let from = recoveries
            .iter()
            .position(|(_, r)| saved.1 != 0 && carries(r.stamp, saved))
            .map_or(0, |i| i + 1);
        let since = recoveries.get(from).map(|(q, _)| *q);
        let mut x = p;
        for (_, r) in &recoveries[from..] {
            x = x.min(LogPos {
                generation: p.generation,
                sequence: r.after,
            });
        }
        let lost: Vec<_> = self
            .reader
            .as_ref()
            .map(|r| r.lost_ranges().to_vec())
            .unwrap_or_default();
        for l in lost {
            let first = LogPos {
                generation: l.first.0,
                sequence: l.first.1,
            };
            if first.generation == p.generation && since.is_none_or(|q| first >= before(q)) {
                x = x.min(before(first));
            }
        }
        Ok(x.min(self.head))
    }

    /// Takes a handle for a consumer (`docs/api.md` `Store::consumer`; Q-131, Q-95):
    /// - a new consumer starts after the position before its start, with no resync (it has nothing to forget), its
    ///   cursor file written at once naming that position; facts after it up to the last position are replayed;
    /// - one whose cursor file was missing or unreadable at the start is first sent a resync of cause `NoCursor`
    ///   whose `forget_above` is the position before its start, carrying each funnel's state (its latches are lost),
    ///   then continues from there;
    /// - one taken again in this run resumes from its confirmed cursor, and one with a cursor file as set up at the
    ///   start, whatever `start`.
    pub(crate) fn take(
        &mut self,
        name: &str,
        start: Start,
        ctx: &Ctx<'_>,
    ) -> Result<io::Result<()>, Error> {
        let new = match self.consumers.get(name) {
            Some(rt) if rt.handle => return Err(Error::ConsumerInUse(name.to_string())),
            Some(_) => false,
            None => {
                if self.consumers.len() >= self.max_consumers {
                    return Err(Error::TooManyConsumers(self.max_consumers));
                }
                true
            }
        };
        let head = self.head;
        let lost = !new && self.consumers[name].dormant;
        let point = if new || lost {
            Some(self.start_point(start)?)
        } else {
            None
        };
        let rt = self.consumers.entry(name.to_string()).or_default();
        if rt.attached {
            // The consumer process restarted (Q-95 (b)): what it was handed is dropped and it resumes from its
            // confirmed cursor, with its latches, as after downtime.
            rt.queue.clear();
            rt.parts.clear();
            rt.unconfirmed.clear();
            rt.gate = None;
            rt.runners.clear();
            rt.emitted.clear();
            rt.evaluated.clear();
            rt.reset_open = false;
            rt.paused = false;
            rt.view = None;
            rt.behind = Some(next(rt.cursor));
            rt.resume = Some(head);
        }
        rt.handle = true;
        rt.attached = true;
        let Some(point) = point else {
            return Ok(Ok(()));
        };
        rt.dormant = false;
        rt.resume = None;
        rt.cursor = point;
        rt.behind = (point < head).then(|| next(point));
        if lost {
            rt.latches.clear();
            self.issue_resync(name, ResyncCause::NoCursor, Some(point), point, false, ctx);
            return Ok(Ok(()));
        }
        rt.dirty = true;
        self.sync_runners(name, ctx.mono);
        Ok(self.write_now(name, ctx))
    }

    /// The handle was dropped.
    pub(crate) fn release(&mut self, name: &str) {
        if let Some(rt) = self.consumers.get_mut(name) {
            rt.handle = false;
        }
    }

    fn writable(&self) -> Result<(), Error> {
        if self.failed {
            return Err(Error::ReadOnly("a cursor file write failed".into()));
        }
        Ok(())
    }

    fn bound(&self, name: &str) -> Result<&Rt, Error> {
        match self.consumers.get(name) {
            Some(rt) if rt.handle => Ok(rt),
            _ => Err(Error::Closed),
        }
    }

    /// The funnels in force for a bound consumer.
    pub(crate) fn routes(&self, name: &str) -> Result<Vec<(FunnelId, Funnel)>, Error> {
        self.bound(name)?;
        Ok(self
            .view
            .funnels(name)
            .map(|(id, f)| (FunnelId(id), funnel_out(f, &self.view.registry)))
            .collect())
    }

    /// Drops a removed funnel's latch (`spec.md` §6 Latch).
    pub(crate) fn forget_latch(&mut self, name: &str, funnel: u32) {
        if let Some(rt) = self.consumers.get_mut(name) {
            rt.latches.remove(&funnel);
        }
    }

    /// Whether a consumer is bound to a live handle.
    pub(crate) fn is_bound(&self, name: &str) -> bool {
        self.bound(name).is_ok()
    }

    /// Per-consumer statistics.
    pub(crate) fn stats(&self, name: &str) -> Option<ConsumerStats> {
        let rt = self.consumers.get(name)?;
        Some(ConsumerStats {
            cursor: pos(rt.cursor),
            catching_up: rt.behind.is_some(),
            funnels: self.view.funnels(name).count(),
        })
    }

    /// Registers a waker to wake when deliveries may be ready.
    pub(crate) fn register(&mut self, w: &Waker) {
        if !self.wakers.iter().any(|x| x.will_wake(w)) {
            self.wakers.push(w.clone());
        }
    }

    /// The wakers registered, to wake once the lock is released: all of them with `all`, else only if something
    /// changed since they were last woken.
    pub(crate) fn take_wakers(&mut self, all: bool) -> Vec<Waker> {
        if all || std::mem::take(&mut self.notify) {
            self.notify = false;
            std::mem::take(&mut self.wakers)
        } else {
            Vec::new()
        }
    }

    /// Closes the log reader (its lease is deleted).
    pub(crate) fn close(&mut self) {
        if let Some(r) = self.reader.take() {
            let _ = r.close();
        }
    }
}
