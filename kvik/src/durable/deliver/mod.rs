//! Delivery on the durable backend (`spec.md` §4, §6; roadmap 5.2): consumers, their bounded queues, condition
//! funnels, latches and durable cursors.
//!
//! Everything is read from the committed log. A [`Deliverer`] holds its own log reader and takes each batch, in commit
//! order, once it is acknowledged (a batch holding an entry that is not a fact, or the first batch in a segment
//! (Q-630), once the mark is synced past it, and every later batch behind it). Taking a batch applies its entries to
//! the routing table and registry, enqueues its facts for the live consumers whose fact funnels select them, and
//! evaluates their condition funnels ([`funnels`]) as the reference model does (`kvik_model::condition`), on the live
//! state of roadmap 5.4. A consumer that is behind (resuming after a restart, or whose queue filled) reads the log from
//! its position with the routes in force at each position, pausing at every reset point for a `resync` carrying
//! `forget_above`, and ends a resume with a `resync` that carries each funnel's state now ([`resync`]). Confirmations
//! move the cursor and the latches, which the cursor file holds ([`confirm`]).
//!
//! The work is done on the calling thread of the call that needs it (an emit's acknowledgement while a consumer is
//! registered, a poll, a confirmation, [`crate::Store::tick`]) and by the `kvik-deliver` thread ([`Hub`]), never on the writer thread.
//! Timers run on `Env`'s monotonic clock; `now` and live staleness are the writer's ([`super::clock`]).
//!
//! Not here yet: windowed operators in funnels, starting at a position or a time, the saved-point check, timer
//! sequences in the cursor file and the catch-up statistics (roadmap 5.3).

mod confirm;
mod funnels;
mod held;
mod hub;
mod intake;
mod recent;
mod resync;
mod take;

pub(crate) use hub::Hub;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::cursor::{self, Latch};
use kvik_log::entries::Entry;
use kvik_log::manifest::LossCause;
use kvik_log::reader::{
    LogReader, ReadBatch, ReadEvent, ReaderConfig as LogReaderConfig, ReaderError, Update,
};
use kvik_log::writer::{Position as LogPos, Watch};
use kvik_model::condition::{ConditionState as MState, Runner};

use super::LogClock;
use super::clock::Clocks;
use super::index::{Index, Stored, pos};
use super::loss::{Finder, Latch as LossLatch, Regions};
use super::routing::{View, funnel_out};
use crate::backend::Error;
use crate::consume::{Delivery, Funnel, FunnelId, ResyncCause, TriggerCause};
use crate::env::Env;
use crate::health::{ConsumerStats, DeliveryStats};

/// Items the open takes from the log at a time.
const LOAD_PART: usize = 1 << 16;

/// Part sizes tests set for the store in a directory, in place of [`LOAD_PART`].
#[cfg(test)]
pub(crate) static LOAD_PARTS: std::sync::Mutex<Vec<(std::path::PathBuf, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// Items the deliverer's reads of the whole log take at a time.
fn load_part(dir: &Path) -> usize {
    #[cfg(test)]
    if let Some((_, p)) = LOAD_PARTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(d, _)| d == dir)
    {
        return *p;
    }
    let _ = dir;
    LOAD_PART
}

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

/// Refreshes `r`, unless it is at the writer's durable position, where a refresh would read nothing: only this
/// process changes the log while it holds the store lock, and the manifest only at a start (Q-704). Then the lease
/// is only kept alive.
fn read_new(r: &mut LogReader, writer: Option<LogPos>) -> Result<Option<Update>, ReaderError> {
    if writer.is_some_and(|w| r.state().durable >= w) {
        r.keep_alive().map(|_| None)
    } else {
        r.refresh().map(Some)
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
    /// Its key's earlier facts could not all be read, so `outranked` rests on those that could (Q-750).
    unchecked: bool,
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
        unchecked: bool,
    },
    Resync {
        cause: ResyncCause,
        forget_above: Option<LogPos>,
        position: LogPos,
        funnels: Vec<FState>,
        /// It is, or ends with, a reset point: its confirmation records its id in the cursor file (field 3).
        reset: bool,
        unchecked: bool,
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
    /// A void changed the funnels in force: the runners are synced at the next evaluation.
    stale_runners: bool,
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
    /// Also read for facts outside the recent window, from calls that hold the deliverer shared ([`recent`]).
    reader: RefCell<Option<LogReader>>,
    store_id: [u8; 16],
    /// The registry, entries and commit times of the kept log; no facts (roadmap 11.0).
    index: Index,
    recent: recent::Recent,
    /// Invalid regions the reader passed over, by segment and offset: lost (Q-750).
    damaged: Regions,
    /// Finds the losses in what the reader returns, for the latch of the process (Q-930).
    finder: Finder,
    latch: Arc<LossLatch>,
    /// The latch's closed ranges already taken.
    closed_seen: usize,
    /// Reads of the log for facts outside the recent window.
    slow: Cell<u64>,
    /// A read of the log for facts outside the recent window met data it could not read since this was last reset.
    incomplete: Cell<bool>,
    /// Deliveries decided without every fact they rest on (Q-750).
    unchecked: u64,
    /// Condition evaluations made without every fact they rest on (Q-750).
    evaluations_unchecked: u64,
    /// Damaged regions a consumer's read of the log met beyond those passed over at the open (Q-750).
    unread: std::cell::RefCell<Regions>,
    regions_unread: Cell<u64>,
    /// The kept log by position, from `items_from` up (the lowest cursor); below it, a consumer that needs older
    /// records has them read from the log again ([`Deliverer::restore_items`]).
    items: BTreeMap<LogPos, Rec>,
    items_from: LogPos,
    /// Every entry that is not a fact, by position: the routes and registry at any position.
    entries: BTreeMap<LogPos, Entry>,
    /// Batches read and not yet taken.
    pending: VecDeque<ReadBatch>,
    /// Per series, the receive time of its last fact that was not outranked (`spec.md` §6 Restart and resume).
    last_arrival: crate::durable::by_id::ById<i64>,
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
    /// The index held every batch up to the mark the clean shutdown synced when the reader was closed.
    closed_complete: bool,
    /// Records taken batches put in `items`.
    #[cfg(test)]
    records: u64,
}

impl Deliverer {
    /// Opens the deliverer on the kept log, before the writer's threads take calls: reads the whole log, the
    /// manifest's lost ranges and every cursor file, and sets each known consumer up to resume (`spec.md` §3 Order of
    /// a start, step 7).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        dir: &Path,
        env: &Env,
        clocks: &Clocks,
        store_id: [u8; 16],
        capacity: usize,
        max_consumers: usize,
        window_budget: usize,
        latch: Arc<LossLatch>,
        fold: Option<Arc<super::derived::SeriesFold>>,
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
            reader: RefCell::new(Some(reader)),
            store_id,
            index: Index::without_facts(),
            recent: recent::Recent::with_limits(
                crate::durable::history::WINDOW_FACTS,
                window_budget,
            ),
            damaged: Regions::default(),
            finder: Finder::default(),
            latch,
            closed_seen: 0,
            slow: Cell::new(0),
            incomplete: Cell::new(false),
            unchecked: 0,
            evaluations_unchecked: 0,
            unread: std::cell::RefCell::new(Regions::default()),
            regions_unread: Cell::new(0),
            items: BTreeMap::new(),
            items_from: LogPos::default(),
            entries: BTreeMap::new(),
            pending: VecDeque::new(),
            last_arrival: crate::durable::by_id::ById::default(),
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
            closed_complete: false,
            #[cfg(test)]
            records: 0,
        };
        if let Some(f) = fold {
            d.index.set_fold(f);
        }
        // `now` places the recent window while the log is taken (roadmap 11.0).
        let ctx = Ctx {
            watch: None,
            env,
            dir,
            clocks,
            now: clocks.now(),
            wall: 0,
            mono: 0,
        };
        // The kept log, read in parts before anything is delivered: a read that fails stops the open.
        loop {
            let read = d
                .reader
                .get_mut()
                .as_mut()
                .map(|r| r.refresh_some(load_part(dir)))
                .transpose()
                .map_err(|e| io::Error::other(e.to_string()))?;
            let Some((u, more)) = read else { break };
            d.take_events(u.events, u.state.durable);
            d.take_pending(&ctx);
            if !more {
                break;
            }
        }
        d.loading = false;
        // Lost ranges not described by a recovery entry are reset points: `forget_above` the position before, id
        // its last position (`spec.md` §4 Resync).
        let lost: Vec<_> = d
            .reader
            .get_mut()
            .as_ref()
            .map(|r| r.lost_ranges().to_vec())
            .unwrap_or_default();
        // A recovery entry the kept log no longer shows is still a reset point unless a recorded lost range holds it:
        // the registry file keeps every recovery entry (`spec.md` §4 Resync, §11 Health; Q-1033).
        if let Ok(Some((_, file))) = kvik_log::registry::read_latest(&*env.fs, dir, &store_id)
            && file.generation == d.head.generation
        {
            for (seq, _, e) in &file.entries {
                let Entry::Recovery(r) = e else {
                    continue;
                };
                let at = (file.generation, *seq);
                if lost
                    .iter()
                    .any(|l| (l.first.0, l.first.1) <= at && at <= (l.last.0, l.last.1))
                {
                    continue;
                }
                let p = LogPos {
                    generation: file.generation,
                    sequence: *seq,
                };
                let after = LogPos {
                    generation: file.generation,
                    sequence: r.after,
                };
                d.resets.entry(p).or_insert(after);
                for c in &r.consumers {
                    let l = d.lost.entry(c.clone()).or_insert(after);
                    *l = (*l).min(after);
                }
            }
        }
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

    /// The last position taken.
    #[cfg(test)]
    pub(crate) fn head(&self) -> LogPos {
        self.head
    }

    /// Whether any consumer is known: taken in this run, or named by a cursor file or the log.
    pub(crate) fn has_consumers(&self) -> bool {
        !self.consumers.is_empty()
    }

    /// Counts of what taking the log met.
    pub(crate) fn delivery_stats(&self) -> DeliveryStats {
        DeliveryStats {
            slow_path: self.slow.get(),
            unchecked: self.unchecked,
            evaluations_unchecked: self.evaluations_unchecked,
            regions_unread: self.regions_unread.get(),
            ..self.stats.clone()
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
        if let Some(r) = self.reader.get_mut().take() {
            let _ = r.close();
        }
    }
}

#[cfg(test)]
mod tests;
