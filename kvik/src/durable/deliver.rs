//! Delivery on the durable backend (`spec.md` §4, §6; roadmap 5.2): consumers, their bounded queues, condition
//! funnels, latches and durable cursors.
//!
//! Everything is read from the committed log. A [`Deliverer`] holds its own log reader and takes each batch, in
//! commit order, once it is acknowledged (a batch holding an entry that is not a fact once the mark is synced past
//! it, and every later batch behind it). Taking a batch applies its entries to the routing table and registry,
//! enqueues its facts for the live consumers whose fact funnels select them, and evaluates their condition funnels
//! as the reference model does (`kvik_model::condition`). A consumer that is behind (resuming after a restart, or
//! whose queue filled) reads the log from its position with the routes in force at each position, pausing at every
//! reset point for a `resync` carrying `forget_above`, and ends a resume with a `resync` that carries each funnel's
//! state now.
//!
//! The work is done on the calling thread of the call that needs it (an emit's acknowledgement, a poll, a
//! confirmation, [`crate::Store::tick`]) and by the `kvik-deliver` thread, which wakes on the writer's progress and
//! every second, never on the writer thread. Timers run on `Env`'s monotonic clock.
//!
//! A cursor file (`format.md` A.15) holds the confirmed position, the latches and the last reset point confirmed.
//! It is written at most once a second (at a confirmation when a second has passed since the last write, and by the
//! `kvik-deliver` thread), when a `resync` carrying `forget_above` is confirmed (before anything further is
//! delivered) and at clean shutdown; never naming a position above the last synced mark, which it forces a sync of
//! first when it would.
//!
//! Not here yet: windowed operators and live state queries (roadmap 5.4), starting at a position or a time, the
//! saved-point check, timer sequences in the cursor file and the catch-up statistics (5.3).

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::cursor::{self, CursorState, Latch, LatchState};
use kvik_log::entries::{Entry, IntervalLength as LogInterval, RouteCondition};
use kvik_log::facts::{Quality as LogQuality, Value as LogValue};
use kvik_log::manifest::LossCause;
use kvik_log::reader::{LogReader, ReadBatch, ReadEvent, ReaderConfig as LogReaderConfig};
use kvik_log::writer::{Position as LogPos, Watch};
use kvik_model::condition::{ConditionState as MState, ResumeHistory, Runner};

use super::LogClock;
use super::index::{Index, Stored, fact_out, pos};
use super::routing::{FunnelRec, View, funnel_out, model_condition, numeric};
use crate::backend::Error;
use crate::consume::{
    ConditionState, Delivery, DeliveryFlags, DeliveryId, DeliveryKind, Funnel, FunnelId,
    FunnelState, RESYNC_PART_FUNNELS, ResyncCause, ResyncPart, Trigger, TriggerCause,
};
use crate::env::Env;
use crate::health::ConsumerStats;
use crate::read::{Live, LiveValue};
use crate::series::Timing;
use crate::time::Timestamp;

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
    /// The store's `now` (`spec.md` §6).
    pub(crate) now: i64,
    pub(crate) wall: i64,
    pub(crate) mono: i64,
}

/// A kept fact and whether it was outranked when it was committed (`spec.md` §4 Outranked facts).
#[derive(Clone, Debug)]
struct FactRec {
    stored: Stored,
    outranked: bool,
}

/// A record of the kept log.
#[derive(Clone, Debug)]
enum Rec {
    Fact(Box<FactRec>),
    Entry(Entry),
}

/// A funnel's part of a resync.
#[derive(Clone, Debug)]
struct FState {
    funnel: u32,
    incarnation: LogPos,
    live: Vec<(u32, Live)>,
    condition: Option<(MState, bool)>,
}

/// A delivery waiting in a consumer's queue.
#[derive(Clone, Debug)]
enum Pend {
    Fact {
        rec: Box<FactRec>,
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
    },
}

/// A delivery handed out and not yet confirmed.
#[derive(Clone, Debug)]
struct Out {
    id: DeliveryId,
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
    /// Reading stopped at a reset point until its resync is confirmed.
    paused: bool,
    runners: BTreeMap<u32, Runner>,
    emitted: BTreeMap<u32, MState>,
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
    items: BTreeMap<LogPos, Rec>,
    entries: BTreeMap<LogPos, Entry>,
    /// Batches read and not yet taken.
    pending: VecDeque<ReadBatch>,
    /// The highest `changed_at` per key of a versioned series.
    top: BTreeMap<(u32, i64), u64>,
    view: View,
    head: LogPos,
    /// Reset points by id: their `forget_above` (`spec.md` §4 Resync).
    resets: BTreeMap<LogPos, LogPos>,
    consumers: BTreeMap<String, Rt>,
    /// Condition delivery sequences, per consumer and funnel, from 1 at every start (`spec.md` §4 Order).
    sequences: BTreeMap<(String, u32), u64>,
    /// The monotonic time of each series' last commit of a fact that counts for live staleness in this run.
    committed_mono: BTreeMap<u32, i64>,
    capacity: usize,
    max_consumers: usize,
    wakers: Vec<Waker>,
    /// Something a consumer may be waiting for changed since the wakers were last woken.
    notify: bool,
    loading: bool,
}

fn state_in(l: LatchState) -> MState {
    match l {
        LatchState::Active(b) => MState::Active(b),
        LatchState::Value(v) => MState::Value(v),
    }
}

fn state_log(s: &MState) -> LatchState {
    match s {
        MState::Active(b) => LatchState::Active(*b),
        MState::Value(v) => LatchState::Value(*v),
    }
}

fn state_out(s: &MState) -> ConditionState {
    match s {
        MState::Active(true) => ConditionState::True,
        MState::Active(false) => ConditionState::False,
        MState::Value(v) => ConditionState::Value(*v),
    }
}

fn bad(s: &Stored) -> bool {
    s.row.quality == LogQuality::Bad || s.no_value()
}

fn number(s: &Stored) -> Option<f64> {
    match s.row.value {
        LogValue::F64(bits) => Some(f64::from_bits(bits)),
        _ => None,
    }
}

impl Deliverer {
    /// Opens the deliverer on the kept log, before the writer's threads take calls: reads the whole log, the
    /// manifest's lost ranges and every cursor file, and sets each known consumer up to resume (`spec.md` §3 Order of
    /// a start, step 7).
    pub(crate) fn open(
        dir: &Path,
        env: &Env,
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
            entries: BTreeMap::new(),
            pending: VecDeque::new(),
            top: BTreeMap::new(),
            view: View::default(),
            head: LogPos::default(),
            resets: BTreeMap::new(),
            consumers: BTreeMap::new(),
            sequences: BTreeMap::new(),
            committed_mono: BTreeMap::new(),
            capacity,
            max_consumers,
            wakers: Vec::new(),
            notify: false,
            loading: true,
        };
        let ctx = Ctx {
            watch: None,
            env,
            dir,
            now: 0,
            wall: 0,
            mono: 0,
        };
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
        let known: Vec<String> = d.view.consumers.keys().cloned().collect();
        for name in known {
            d.consumers.entry(name).or_insert_with(|| Rt {
                dormant: true,
                ..Rt::default()
            });
        }
        Ok(d)
    }

    /// The routes and registry in force.
    pub(crate) fn view(&self) -> &View {
        &self.view
    }

    /// Takes the batches acknowledged since the last call.
    pub(crate) fn pump(&mut self, ctx: &Ctx<'_>) {
        let Some(r) = self.reader.as_mut() else {
            return;
        };
        if let Ok(u) = r.refresh() {
            for e in u.events {
                match e {
                    ReadEvent::Batch(b) => self.pending.push_back(b),
                    ReadEvent::Void { above, .. } => self.void(above),
                    ReadEvent::Corruption { .. } => {}
                }
            }
        }
        let stats = ctx.watch.map(Watch::stats);
        while let Some(b) = self.pending.front() {
            if let Some(st) = &stats {
                let entry = b.content.items.iter().any(|i| matches!(i, Item::Entry(_)));
                if b.last > st.durable || (entry && b.last > st.mark_synced) {
                    break;
                }
            }
            let b = self.pending.pop_front().expect("looked at above");
            self.take_batch(b, ctx);
        }
    }

    fn void(&mut self, above: LogPos) {
        self.pending.retain(|b| b.last <= above);
        self.index.apply(&[ReadEvent::Void {
            above,
            cause: kvik_log::reader::VoidCause::Rewritten,
        }]);
        self.items.retain(|p, _| *p <= above);
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
                    let rec = Box::new(FactRec {
                        stored: Stored {
                            position: p,
                            received_at: b.content.received_at,
                            row,
                        },
                        outranked,
                    });
                    let series = rec.stored.row.series_id;
                    if !self.loading {
                        if !outranked && !bad(&rec.stored) && !rec.stored.row.flags.future {
                            self.committed_mono.insert(series, ctx.mono);
                        }
                        for name in &names {
                            self.deliver_live(name, &rec);
                        }
                    }
                    if !outranked {
                        fed.push(series);
                    }
                    self.items.insert(p, Rec::Fact(rec));
                }
                Item::Entry(Entry::Ignored { .. }) => {}
                Item::Entry(e) => {
                    self.view.apply(p, &e);
                    if let Entry::Recovery(r) = &e {
                        self.resets.insert(
                            p,
                            LogPos {
                                generation: p.generation,
                                sequence: r.after,
                            },
                        );
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

    fn deliver_live(&mut self, name: &str, rec: &FactRec) {
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        if rt.dormant || rt.behind.is_some() || !self.view.routed(name, rec.stored.row.series_id) {
            return;
        }
        if rt.queue.len() < self.capacity {
            rt.queue.push_back(Pend::Fact {
                rec: Box::new(rec.clone()),
                replayed: false,
            });
        } else {
            // Catch up from the first position not enqueued in this session (`spec.md` §4 Overflow).
            rt.behind = Some(rec.stored.position);
        }
    }

    /// Creates a runner for each condition funnel in force that has none and drops those of funnels no longer in
    /// force. A consumer resuming gets its runners with its resync.
    fn sync_runners(&mut self, name: &str, mono: i64) {
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        if rt.resume.is_some() || rt.dormant {
            return;
        }
        let funnels: BTreeMap<u32, &FunnelRec> = self.view.funnels(name).collect();
        rt.runners.retain(|f, _| funnels.contains_key(f));
        rt.emitted.retain(|f, _| funnels.contains_key(f));
        for (id, f) in funnels {
            if let (Some(c), Some(s)) = (&f.condition, f.condition_series())
                && !rt.runners.contains_key(&id)
            {
                let r = Runner::new(model_condition(c, &self.view.registry, s), mono);
                rt.emitted.insert(id, r.state());
                rt.runners.insert(id, r);
            }
        }
    }

    /// The live-state fact of a series (`spec.md` §2.3): the current fact with the greatest `t ≤ now` that is not
    /// bad quality, an observed interval series' once its interval has ended; a forecast series has none.
    fn latest(&self, s: u32, now: i64) -> Option<&Stored> {
        let series = self.view.registry.get(s)?;
        if series.spec.timing == Timing::Forecast {
            return None;
        }
        let length = match (&series.spec.timing, &series.def.interval) {
            (Timing::Observed, Some(LogInterval::Fixed { length, .. })) => *length,
            _ => 0,
        };
        let versioned = series.spec.versioned.is_some();
        let rank = |x: &Stored| (if versioned { x.row.changed_at } else { None }, x.position);
        let mut facts = self.index.series_facts_back(s, now).peekable();
        while let Some(first) = facts.next() {
            let t = first.row.t;
            let mut best = first;
            while let Some(x) = facts.next_if(|x| x.row.t == t) {
                if rank(x) > rank(best) {
                    best = x;
                }
            }
            if !bad(best) && t.saturating_add(length) <= now {
                return Some(best);
            }
        }
        None
    }

    fn value_of(&self, s: u32, now: i64) -> Option<f64> {
        if !numeric(&self.view.registry, s) {
            return None;
        }
        self.latest(s, now).and_then(number)
    }

    /// For a stale-for condition over a scheduled series, `now − t` of its live-state fact (`None` without one).
    fn scheduled_since(&self, c: &RouteCondition, s: u32, now: i64) -> Option<Option<i64>> {
        let scheduled = self
            .view
            .registry
            .get(s)
            .is_some_and(|x| x.spec.timing == Timing::Scheduled);
        (scheduled && matches!(c, RouteCondition::StaleFor { .. }))
            .then(|| self.latest(s, now).map(|x| now - x.row.t))
    }

    /// What a resume rebuilds of a condition's past (`spec.md` §6 Restart and resume), as the reference model does.
    fn resume_history(&self, c: &RouteCondition, s: u32, ctx: &Ctx<'_>) -> ResumeHistory {
        let mut h = ResumeHistory::default();
        match c {
            RouteCondition::HeldFor { above, .. } => {
                let mut start = None;
                let versioned = self
                    .view
                    .registry
                    .get(s)
                    .is_some_and(|x| x.spec.versioned.is_some());
                let rank =
                    |x: &Stored| (if versioned { x.row.changed_at } else { None }, x.position);
                let mut facts = self.index.series_facts_back(s, ctx.now).peekable();
                while let Some(first) = facts.next() {
                    let t = first.row.t;
                    let mut best = first;
                    while let Some(x) = facts.next_if(|x| x.row.t == t) {
                        if rank(x) > rank(best) {
                            best = x;
                        }
                    }
                    if bad(best) {
                        continue;
                    }
                    if number(best).is_some_and(|v| v > *above) {
                        start = Some(t);
                    } else {
                        break;
                    }
                }
                h.above_for = start.map_or(0, |t| ctx.now - t);
            }
            RouteCondition::StaleFor { .. } => {
                h.since_arrival = self.items.values().rev().find_map(|r| match r {
                    Rec::Fact(f) if f.stored.row.series_id == s && !f.outranked => {
                        Some((ctx.wall - f.stored.received_at).max(0))
                    }
                    _ => None,
                });
            }
            RouteCondition::Threshold { .. } | RouteCondition::OnChange { .. } => {}
        }
        h
    }

    fn live_of(&self, s: u32, ctx: &Ctx<'_>) -> Live {
        let Some(f) = self.latest(s, ctx.now) else {
            return Live::None;
        };
        let series = self.view.registry.get(s);
        let limit = series.and_then(|x| x.def.staleness);
        let stale = match series.map(|x| x.spec.timing) {
            Some(Timing::Scheduled) => limit.is_some_and(|l| ctx.now - f.row.t > l),
            Some(Timing::Forecast) | None => false,
            Some(Timing::Observed) => match limit {
                None => false,
                Some(l) => match self.committed_mono.get(&s) {
                    Some(m) => ctx.mono - m > l,
                    None => ctx.now - f.row.t > l,
                },
            },
        };
        Live::Value(LiveValue {
            fact: fact_out(f),
            stale,
        })
    }

    /// Evaluates a consumer's condition funnels: on a taken batch (those whose series has a fact fed in it) or on
    /// the timer (all of them). Replayed facts never reach a condition (`spec.md` §4 Replay).
    fn evaluate(&mut self, name: &str, batch: Option<&[u32]>, ctx: &Ctx<'_>) {
        match self.consumers.get(name) {
            Some(rt) if rt.resume.is_none() && !rt.dormant => {}
            _ => return,
        }
        self.sync_runners(name, ctx.mono);
        let mut work = Vec::new();
        for (id, f) in self.view.funnels(name) {
            let (Some(c), Some(s)) = (&f.condition, f.condition_series()) else {
                continue;
            };
            let arrived = batch.map(|b| b.contains(&s));
            if arrived == Some(false) {
                continue;
            }
            work.push((
                id,
                arrived,
                self.value_of(s, ctx.now),
                self.scheduled_since(c, s, ctx.now),
            ));
        }
        let cause = if batch.is_some() {
            TriggerCause::Fact
        } else {
            TriggerCause::Timer
        };
        for (id, arrived, value, scheduled) in work {
            let Some(runner) = self
                .consumers
                .get_mut(name)
                .and_then(|rt| rt.runners.get_mut(&id))
            else {
                continue;
            };
            let mut events = Vec::new();
            if let Some(since) = scheduled {
                events.extend(runner.on_scheduled(since));
            } else {
                if arrived == Some(true) {
                    events.extend(runner.on_arrival(ctx.mono));
                }
                events.extend(runner.on_value(ctx.mono, value));
                if batch.is_none() {
                    events.extend(runner.on_tick(ctx.mono));
                }
            }
            for e in events {
                self.emit_trigger(name, id, e.state(), cause);
            }
        }
    }

    /// Queues a condition delivery with the funnel's next sequence and the last position taken. A consumer that
    /// is behind is not sent it; a full queue switches it to catch-up (`spec.md` §4 Overflow).
    fn emit_trigger(&mut self, name: &str, funnel: u32, state: MState, cause: TriggerCause) {
        let head = self.head;
        let incarnation = self
            .view
            .funnels(name)
            .find(|(id, _)| *id == funnel)
            .map_or(LogPos::default(), |(_, f)| f.incarnation);
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        if rt.behind.is_some() {
            return;
        }
        if rt.queue.len() >= self.capacity {
            rt.behind = Some(next(head));
            return;
        }
        let seq = self
            .sequences
            .entry((name.to_string(), funnel))
            .or_insert(0);
        *seq += 1;
        rt.queue.push_back(Pend::Trigger {
            funnel,
            incarnation,
            sequence: *seq,
            state: state.clone(),
            cause,
            position: head,
        });
        rt.emitted.insert(funnel, state);
    }

    /// Runs every condition funnel's timer (`spec.md` §4 Timers, §6 Cadence).
    pub(crate) fn tick(&mut self, ctx: &Ctx<'_>) {
        let names: Vec<String> = self.consumers.keys().cloned().collect();
        for n in names {
            self.evaluate(&n, None, ctx);
        }
    }

    /// Queues a resync at `position` with the state now of every funnel in force (`spec.md` §4 Resync, §6 Restart
    /// and resume): conditions are rebuilt as of now from the latch; a state that differs from it, or with no
    /// latch, is a transition.
    fn issue_resync(
        &mut self,
        name: &str,
        cause: ResyncCause,
        forget_above: Option<LogPos>,
        position: LogPos,
        ctx: &Ctx<'_>,
    ) {
        let mut states = Vec::new();
        let mut runners = BTreeMap::new();
        let mut emitted = BTreeMap::new();
        let latches = self
            .consumers
            .get(name)
            .map(|rt| rt.latches.clone())
            .unwrap_or_default();
        for (id, f) in self.view.funnels(name) {
            let live = self
                .view
                .selected(name, id, f)
                .into_iter()
                .map(|s| (s, self.live_of(s, ctx)))
                .collect();
            let condition = match (&f.condition, f.condition_series()) {
                (Some(c), Some(s)) => {
                    let cond = model_condition(c, &self.view.registry, s);
                    let latch = latches
                        .get(&id)
                        .filter(|l| l.incarnation == f.incarnation)
                        .map(|l| state_in(l.state));
                    let history = self.resume_history(c, s, ctx);
                    let mut runner = Runner::resume(
                        cond.clone(),
                        ctx.mono,
                        self.value_of(s, ctx.now),
                        latch.as_ref(),
                        history,
                    );
                    if let Some(since) = self.scheduled_since(c, s, ctx.now) {
                        runner.on_scheduled(since);
                    }
                    let state = runner.state();
                    let transition = cond.is_transition(&state, latch.as_ref());
                    runners.insert(id, runner);
                    emitted.insert(id, state.clone());
                    Some((state, transition))
                }
                _ => None,
            };
            states.push(FState {
                funnel: id,
                incarnation: f.incarnation,
                live,
                condition,
            });
        }
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        rt.queue.push_back(Pend::Resync {
            cause,
            forget_above,
            position,
            funnels: states,
        });
        rt.runners = runners;
        rt.emitted = emitted;
        rt.resume = None;
        self.notify = true;
    }

    /// The routes and registry as of the position before `at`, built from the entries before it.
    fn view_before(&self, at: LogPos) -> View {
        let mut v = View::default();
        for (p, e) in self.entries.range(..at) {
            v.apply(*p, e);
        }
        v
    }

    /// Reads the log for a consumer that is behind into its queue, as far as its queue allows: replayed facts with
    /// the routes in force at each position, a resync at each reset point (consecutive ones with nothing routed
    /// between them as one), the resync that ends a resume, and back to live at the last position taken.
    fn catch_up(&mut self, name: &str, ctx: &Ctx<'_>) {
        loop {
            let Some(rt) = self.consumers.get(name) else {
                return;
            };
            let Some(mut at) = rt.behind else { return };
            if rt.paused || rt.dormant {
                return;
            }
            let limit = rt.resume.unwrap_or(self.head);
            let mut view = match self.consumers.get_mut(name).and_then(|rt| rt.view.take()) {
                Some((p, v)) if p == at => v,
                _ => self.view_before(at),
            };
            let mut paused = false;
            while at <= limit {
                let a = self.items.range(at..=limit).next().map(|(p, _)| *p);
                let b = self.resets.range(at..=limit).next().map(|(p, _)| *p);
                let Some(p) = a.into_iter().chain(b).min() else {
                    break;
                };
                let rt = self.consumers.get_mut(name).expect("looked up above");
                if rt.queue.len() >= self.capacity {
                    paused = true;
                    break;
                }
                if let Some(fa) = self.resets.get(&p).copied() {
                    match rt.queue.back_mut() {
                        Some(Pend::Resync {
                            cause: ResyncCause::ResetPoint,
                            forget_above: Some(f),
                            position,
                            ..
                        }) if rt.reset_open => {
                            *f = (*f).min(fa);
                            *position = (*position).max(p);
                        }
                        _ => {
                            rt.queue.push_back(Pend::Resync {
                                cause: ResyncCause::ResetPoint,
                                forget_above: Some(fa),
                                position: p,
                                funnels: Vec::new(),
                            });
                            rt.reset_open = true;
                        }
                    }
                }
                match self.items.get(&p) {
                    Some(Rec::Entry(e)) => view.apply(p, e),
                    Some(Rec::Fact(f)) if view.routed(name, f.stored.row.series_id) => {
                        if rt.reset_open {
                            // Nothing further until the reset point's resync is confirmed.
                            rt.paused = true;
                            paused = true;
                            break;
                        }
                        rt.queue.push_back(Pend::Fact {
                            rec: f.clone(),
                            replayed: true,
                        });
                    }
                    _ => {}
                }
                at = next(p);
            }
            let rt = self.consumers.get_mut(name).expect("looked up above");
            if paused {
                rt.behind = Some(at);
                rt.view = Some((at, view));
                return;
            }
            at = at.max(next(limit));
            rt.behind = Some(at);
            rt.view = Some((at, view));
            if let Some(stop) = rt.resume {
                // A reset point at the last position is the same delivery as the resync that ends the resume.
                let mut forget = None;
                if rt.reset_open {
                    match rt.queue.back() {
                        Some(Pend::Resync {
                            cause: ResyncCause::ResetPoint,
                            forget_above,
                            position,
                            ..
                        }) if *position == stop => {
                            forget = *forget_above;
                            rt.queue.pop_back();
                            rt.reset_open = false;
                        }
                        _ => {
                            rt.paused = true;
                            return;
                        }
                    }
                }
                self.issue_resync(name, ResyncCause::Resume, forget, stop, ctx);
                continue;
            }
            if rt.reset_open {
                rt.paused = true;
                return;
            }
            rt.behind = None;
            rt.view = None;
            self.reattach(name);
            return;
        }
    }

    /// On re-attaching to live after catch-up, a condition whose state differs from the last state emitted is sent.
    fn reattach(&mut self, name: &str) {
        let Some(rt) = self.consumers.get(name) else {
            return;
        };
        let changed: Vec<(u32, MState)> = rt
            .runners
            .iter()
            .filter(|(id, r)| rt.emitted.get(id) != Some(&r.state()))
            .map(|(id, r)| (*id, r.state()))
            .collect();
        for (id, state) in changed {
            self.emit_trigger(name, id, state, TriggerCause::Reattach);
        }
    }

    /// Takes a handle for a consumer (`docs/api.md` `Store::consumer`): a new consumer, or one with no cursor file,
    /// starts with a resync whose `forget_above` is the last position taken; one taken again in this run resumes
    /// from its confirmed cursor; one with a cursor file resumes as set up at the start.
    pub(crate) fn take(&mut self, name: &str, ctx: &Ctx<'_>) -> Result<(), Error> {
        match self.consumers.get(name) {
            Some(rt) if rt.handle => return Err(Error::ConsumerInUse(name.to_string())),
            Some(_) => {}
            None => {
                if self.consumers.len() >= self.max_consumers {
                    return Err(Error::TooManyConsumers(self.max_consumers));
                }
                self.consumers.insert(
                    name.to_string(),
                    Rt {
                        dormant: true,
                        ..Rt::default()
                    },
                );
            }
        }
        let head = self.head;
        let rt = self.consumers.get_mut(name).expect("inserted above");
        if rt.attached {
            // The consumer process restarted (Q-95 (b)): what it was handed is dropped and it resumes from its
            // confirmed cursor, with its latches, as after downtime.
            rt.queue.clear();
            rt.parts.clear();
            rt.unconfirmed.clear();
            rt.gate = None;
            rt.runners.clear();
            rt.emitted.clear();
            rt.reset_open = false;
            rt.paused = false;
            rt.view = None;
            if rt.has_cursor {
                rt.behind = Some(next(rt.cursor));
                rt.resume = Some(head);
            } else {
                rt.behind = None;
                rt.resume = None;
                rt.dormant = true;
            }
        }
        rt.handle = true;
        rt.attached = true;
        if rt.dormant {
            rt.dormant = false;
            rt.behind = None;
            rt.resume = None;
            rt.latches.clear();
            self.issue_resync(name, ResyncCause::NoCursor, Some(head), head, ctx);
        }
        Ok(())
    }

    /// The handle was dropped.
    pub(crate) fn release(&mut self, name: &str) {
        if let Some(rt) = self.consumers.get_mut(name) {
            rt.handle = false;
        }
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

    /// The next delivery for a bound consumer, if one is ready: the next part of a resync being handed out, else,
    /// unless a resync awaits confirmation, the next from its queue after running timers and catch-up.
    pub(crate) fn poll(&mut self, name: &str, ctx: &Ctx<'_>) -> Result<Option<Delivery>, Error> {
        self.bound(name)?;
        if let Some(d) = self.hand_out(name) {
            return Ok(Some(d));
        }
        if self.consumers[name].gate.is_some() {
            return Ok(None);
        }
        self.tick(ctx);
        self.catch_up(name, ctx);
        let Some(p) = self
            .consumers
            .get_mut(name)
            .and_then(|rt| rt.queue.pop_front())
        else {
            return Ok(None);
        };
        let parts = self.deliveries(p, ctx);
        let rt = self.consumers.get_mut(name).expect("bound above");
        rt.parts.extend(parts);
        Ok(self.hand_out(name))
    }

    fn hand_out(&mut self, name: &str) -> Option<Delivery> {
        let rt = self.consumers.get_mut(name)?;
        let (d, out) = rt.parts.pop_front()?;
        if let DeliveryKind::Resync(p) = &d.kind
            && p.last
        {
            rt.gate = Some(p.part);
        }
        rt.unconfirmed.push(out);
        Some(d)
    }

    fn deliveries(&self, p: Pend, ctx: &Ctx<'_>) -> Vec<(Delivery, Out)> {
        let delivered_at = Timestamp(ctx.now);
        match p {
            Pend::Fact { rec, replayed } => {
                let position = rec.stored.position;
                let id = DeliveryId::Fact(pos(position));
                vec![(
                    Delivery {
                        id,
                        position: pos(position),
                        kind: DeliveryKind::Fact(fact_out(&rec.stored)),
                        flags: DeliveryFlags {
                            replayed,
                            future: rec.stored.row.flags.future,
                            outranked: rec.outranked,
                            ..DeliveryFlags::default()
                        },
                        delivered_at,
                    },
                    Out {
                        id,
                        position,
                        latches: Vec::new(),
                        forced: false,
                        reset: false,
                    },
                )]
            }
            Pend::Trigger {
                funnel,
                incarnation,
                sequence,
                state,
                cause,
                position,
            } => {
                let id = DeliveryId::Condition {
                    funnel: FunnelId(funnel),
                    sequence,
                };
                vec![(
                    Delivery {
                        id,
                        position: pos(position),
                        kind: DeliveryKind::Trigger(Trigger {
                            funnel: FunnelId(funnel),
                            state: state_out(&state),
                            value: None,
                            coverage: None,
                            cause,
                        }),
                        flags: DeliveryFlags::default(),
                        delivered_at,
                    },
                    Out {
                        id,
                        position,
                        latches: vec![(funnel, incarnation, state)],
                        forced: false,
                        reset: false,
                    },
                )]
            }
            Pend::Resync {
                cause,
                forget_above,
                position,
                funnels,
            } => {
                let latches: Vec<(u32, LogPos, MState)> = funnels
                    .iter()
                    .filter_map(|f| {
                        f.condition
                            .as_ref()
                            .map(|(s, _)| (f.funnel, f.incarnation, s.clone()))
                    })
                    .collect();
                let states: Vec<FunnelState> = funnels
                    .into_iter()
                    .flat_map(|f| {
                        let condition = f.condition.as_ref().map(|(s, _)| state_out(s));
                        let transition = f.condition.as_ref().is_some_and(|(_, t)| *t);
                        let funnel = FunnelId(f.funnel);
                        f.live.into_iter().map(move |(s, live)| FunnelState {
                            funnel,
                            series: crate::types::SeriesId(s),
                            live,
                            condition: condition.clone(),
                            transition,
                        })
                    })
                    .collect();
                let chunks: Vec<Vec<FunnelState>> = if states.is_empty() {
                    vec![Vec::new()]
                } else {
                    states
                        .chunks(RESYNC_PART_FUNNELS)
                        .map(<[FunnelState]>::to_vec)
                        .collect()
                };
                let n = chunks.len();
                chunks
                    .into_iter()
                    .enumerate()
                    .map(|(k, funnels)| {
                        let part = u32::try_from(k + 1).unwrap_or(u32::MAX);
                        let last = k + 1 == n;
                        let id = DeliveryId::Resync { part };
                        (
                            Delivery {
                                id,
                                position: pos(position),
                                kind: DeliveryKind::Resync(ResyncPart {
                                    cause,
                                    forget_above: forget_above.map(pos),
                                    part,
                                    last,
                                    funnels,
                                }),
                                flags: DeliveryFlags::default(),
                                delivered_at,
                            },
                            Out {
                                id,
                                position,
                                latches: if last { latches.clone() } else { Vec::new() },
                                forced: last && forget_above.is_some(),
                                reset: last && cause == ResyncCause::ResetPoint,
                            },
                        )
                    })
                    .collect()
            }
        }
    }

    /// Confirms a delivery and every delivery handed out before it (`docs/api.md` `Consumer::confirm`): the cursor
    /// moves to the highest position among them and each condition state they carry becomes its funnel's latch. A
    /// resync is confirmed by its last part. The confirmation of a resync carrying `forget_above` writes the cursor
    /// file before anything further is delivered.
    pub(crate) fn confirm(
        &mut self,
        name: &str,
        id: DeliveryId,
        ctx: &Ctx<'_>,
    ) -> Result<io::Result<()>, Error> {
        self.bound(name)?;
        let funnels: BTreeMap<u32, LogPos> = self
            .view
            .funnels(name)
            .filter(|(_, f)| f.condition.is_some())
            .map(|(id, f)| (id, f.incarnation))
            .collect();
        let rt = self.consumers.get_mut(name).expect("bound above");
        if let DeliveryId::Resync { part } = id {
            if rt.gate != Some(part) {
                return Ok(Ok(()));
            }
            rt.gate = None;
        }
        let n = rt
            .unconfirmed
            .iter()
            .rposition(|o| o.id == id)
            .ok_or(Error::BadConfirm)?
            + 1;
        let done: Vec<Out> = rt.unconfirmed.drain(..n).collect();
        let mut forced = false;
        for o in &done {
            for (f, inc, st) in &o.latches {
                if funnels.get(f) == Some(inc) {
                    rt.latches.insert(
                        *f,
                        Latch {
                            funnel: *f,
                            incarnation: *inc,
                            emitted: o.position,
                            state: state_log(st),
                        },
                    );
                }
            }
            if o.reset {
                rt.reset = Some(o.position);
            }
            forced |= o.forced;
            rt.cursor = rt.cursor.max(o.position);
        }
        rt.dirty = true;
        self.notify = true;
        if forced {
            rt.paused = false;
            rt.reset_open = false;
            return Ok(self.write_cursor(name, true, ctx));
        }
        Ok(self.write_cursor(name, false, ctx))
    }

    /// Writes a consumer's cursor file if its state changed: at once when `force`, else at most once a second; a
    /// mark sync is forced first if the state names a position above the last synced mark.
    fn write_cursor(&mut self, name: &str, force: bool, ctx: &Ctx<'_>) -> io::Result<()> {
        let funnels: BTreeMap<u32, LogPos> = self
            .view
            .funnels(name)
            .map(|(id, f)| (id, f.incarnation))
            .collect();
        let Some(rt) = self.consumers.get_mut(name) else {
            return Ok(());
        };
        if !rt.dirty {
            return Ok(());
        }
        if !force && rt.last_write.is_some_and(|t| ctx.mono - t < SECOND) {
            return Ok(());
        }
        let state = CursorState {
            consumer: name.to_string(),
            position: rt.cursor,
            latches: rt
                .latches
                .values()
                .filter(|l| funnels.get(&l.funnel) == Some(&l.incarnation))
                .copied()
                .collect(),
            reset: rt.reset,
        };
        // A cursor file never names a position above the last synced mark: a write that would forces a mark sync
        // (`spec.md` §3 Durable mark).
        if let Some(w) = ctx.watch
            && state.highest() > w.stats().mark_synced
        {
            w.sync_mark().map_err(|f| io::Error::other(f.to_string()))?;
        }
        let end = cursor::append(
            &*ctx.env.fs,
            ctx.dir,
            self.store_id,
            rt.cursor.generation,
            ctx.wall,
            &state,
            rt.file_end,
        )?;
        rt.file_end = Some(end);
        rt.dirty = false;
        rt.last_write = Some(ctx.mono);
        rt.has_cursor = true;
        Ok(())
    }

    /// Writes the cursor files that are due (at most once a second each).
    pub(crate) fn flush_due(&mut self, ctx: &Ctx<'_>) -> io::Result<()> {
        let names: Vec<String> = self.consumers.keys().cloned().collect();
        for n in names {
            self.write_cursor(&n, false, ctx)?;
        }
        Ok(())
    }

    /// Writes every cursor file whose state changed (clean shutdown, after the last mark sync).
    pub(crate) fn flush_all(&mut self, ctx: &Ctx<'_>) -> io::Result<()> {
        let names: Vec<String> = self.consumers.keys().cloned().collect();
        for n in names {
            self.write_cursor(&n, true, ctx)?;
        }
        Ok(())
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

/// The deliverer of a writer with what its calls share: the writer's progress, the clocks and a cursor file fault.
/// The `kvik-deliver` thread holds one too, never the writer, so dropping the last `Store` clone stops the store at
/// once, as a crash would.
#[derive(Debug)]
pub(crate) struct Hub {
    deliverer: std::sync::Mutex<Deliverer>,
    watch: std::sync::Mutex<Option<Watch>>,
    env: Env,
    dir: std::path::PathBuf,
    now: Arc<std::sync::atomic::AtomicI64>,
    /// A cursor file write or sync failed: the store is read-only (`spec.md` §3 Write or sync failure).
    pub(crate) fault: std::sync::Mutex<Option<kvik_log::writer::Fault>>,
    pub(crate) closed: std::sync::atomic::AtomicBool,
    /// When the timers last ran, on the process's clock: the `kvik-deliver` thread runs them only when no call has
    /// for a second, so calls that run them (emits, polls, `tick`) keep the order the model backend gives.
    last_tick: std::sync::Mutex<std::time::Instant>,
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Hub {
    pub(crate) fn new(
        d: Deliverer,
        env: Env,
        dir: &Path,
        now: Arc<std::sync::atomic::AtomicI64>,
    ) -> Arc<Hub> {
        Arc::new(Hub {
            deliverer: std::sync::Mutex::new(d),
            watch: std::sync::Mutex::new(None),
            env,
            dir: dir.to_path_buf(),
            now,
            fault: std::sync::Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
            last_tick: std::sync::Mutex::new(std::time::Instant::now()),
        })
    }

    /// Runs `f` on the deliverer with the clocks now, then wakes the consumers waiting if something changed.
    pub(crate) fn run<R>(&self, f: impl FnOnce(&mut Deliverer, &Ctx<'_>) -> R) -> R {
        let watch = lock(&self.watch).clone();
        let wall = self.env.clock.wall().0;
        let now = self
            .now
            .fetch_max(wall, std::sync::atomic::Ordering::SeqCst)
            .max(wall);
        let mono = i64::try_from(self.env.clock.monotonic().0).unwrap_or(i64::MAX);
        let ctx = Ctx {
            watch: watch.as_ref(),
            env: &self.env,
            dir: &self.dir,
            now,
            wall,
            mono,
        };
        let mut d = lock(&self.deliverer);
        let r = f(&mut d, &ctx);
        let closed = self.closed.load(std::sync::atomic::Ordering::SeqCst);
        let wakers = d.take_wakers(closed);
        drop(d);
        for w in wakers {
            w.wake();
        }
        r
    }

    /// Notes that the timers ran.
    pub(crate) fn ticked(&self) {
        *lock(&self.last_tick) = std::time::Instant::now();
    }

    /// Records a cursor file fault.
    pub(crate) fn failed(&self, e: &io::Error) {
        let mut f = lock(&self.fault);
        if f.is_none() {
            *f = Some(kvik_log::writer::Fault {
                op: kvik_log::writer::FaultOp::Write,
                path: self.dir.join(cursor::CURSORS_DIR),
                kind: e.kind(),
                message: e.to_string(),
            });
        }
    }

    /// Starts watching the writer and the `kvik-deliver` thread.
    pub(crate) fn start(self: &Arc<Self>, watch: Watch) -> io::Result<()> {
        *lock(&self.watch) = Some(watch.clone());
        let hub = self.clone();
        std::thread::Builder::new()
            .name("kvik-deliver".into())
            .spawn(move || {
                let mut seen = 0;
                loop {
                    seen = watch.wait_progress(seen, Duration::from_secs(1));
                    if watch.stopped() || hub.closed.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    let due = lock(&hub.last_tick).elapsed() >= Duration::from_secs(1);
                    if due {
                        hub.ticked();
                    }
                    let r = hub.run(|d, c| {
                        d.pump(c);
                        if due {
                            d.tick(c);
                        }
                        d.flush_due(c)
                    });
                    if let Err(e) = r {
                        hub.failed(&e);
                    }
                }
            })?;
        Ok(())
    }
}
