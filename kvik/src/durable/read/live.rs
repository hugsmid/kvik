//! A live session on the durable backend (the query design's section 3.6; roadmap 9.0d): `Latest` and `Tail` kept up
//! to date from the reader's shared tail ([`super::hub`]). A batch is computed when `next` is called, at the cut of a
//! view the reader published, or at a position inside its round when a `Tail` meets its row limit (Q-1014). In the
//! writer's process a session waits on its bell, which the fan-out thread rings only for rounds and refreshes that
//! reach its reads ([`super::fanout`], Q-1118), or until a `Latest` falls due; a reader process polls the mark.
//!
//! A `Latest` judges again only the series whose facts or entries the batch brings, those due by time alone and those
//! a call refreshed the live staleness of since the last batch; a reset or a loss opens it again. A `Tail` delivers the
//! facts its selection takes as known at each fact's position (Q-895); a reset, a loss and a lag are events, never
//! silence (Q-1011, Q-1013).

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use km::registry::Timing as KmTiming;
use kvik_log::entries::{Entry, Versioning};
use kvik_log::writer::{Position as LogPos, Watch};
use kvik_model as km;

use super::hub::{Bell, Piece, Reach};
use super::latest;
use super::view::View;
use crate::durable::index::{Stored, fact_out, pos};
use crate::durable::reader::DurableReader;
use crate::durable::registry::Registry;
use crate::durable::windows;
use crate::error::QueryError;
use crate::read::{
    Answer, Appended, CloseReason, Live, LiveEvent, LiveId, LiveOptions, Query, Selection,
    SessionOptions, UpdateBatch, Updated, event_rank,
};
use crate::series::SeriesKey;
use crate::time::Timestamp;
use crate::types::SeriesId;

#[derive(Clone, Debug)]
struct LatestRead {
    sel: Selection,
    opened: bool,
    members: Vec<SeriesId>,
    shown: BTreeMap<SeriesId, Live>,
    /// When each member may change with no new fact.
    due: BTreeMap<SeriesId, i64>,
}

#[derive(Clone, Debug)]
struct TailRead {
    sel: Selection,
    opts: LiveOptions,
    opened: bool,
    /// The registry as of `from`: names and retirements as known at each fact's position (Q-895).
    names: Registry,
    /// The position its facts are delivered through.
    from: LogPos,
}

#[derive(Clone, Debug)]
enum Read {
    Latest(LatestRead),
    Tail(TailRead),
}

impl Read {
    fn opened(&self) -> bool {
        match self {
            Read::Latest(l) => l.opened,
            Read::Tail(t) => t.opened,
        }
    }
}

/// The longest a reader process's polls go on the mark alone.
const FULL_REFRESH_EVERY: Duration = Duration::from_secs(1);

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Session {
    reader: Arc<DurableReader>,
    watch: Option<Watch>,
    opts: SessionOptions,
    sub: Option<u64>,
    bell: Option<Arc<Bell>>,
    /// The bell's count before the last turn that gave a batch or found nothing: nothing rung up to it is pending.
    settled: Option<u64>,
    /// The last batch stopped inside its rounds (Q-1014): what is left was rung already, so the rest is taken once
    /// the interval passes with no ring.
    rest: bool,
    /// The last round taken whole, and how many pieces of the next were taken.
    at: (u64, usize),
    started: bool,
    reads: BTreeMap<LiveId, Read>,
    last_batch: Option<Duration>,
    last_poll: Option<Duration>,
    /// When the last full refresh of a poll ran.
    last_full: Option<Duration>,
    progress: u64,
    refreshes: u64,
    /// When the last batch was computed, on the reader's monotonic clock.
    judged: Duration,
    /// The manifest's lost ranges at the last batch.
    recorded: Vec<(LogPos, LogPos)>,
    /// The gaps named that stand after the piece the last batch stopped at.
    named: Vec<(LogPos, LogPos)>,
    /// The process's lost ranges counted at the last refresh.
    found_lost: usize,
    /// Calls of `try_batch`, for the tests of waiting.
    #[cfg(test)]
    pub(crate) turns: u64,
    /// Rung by every round, as before Q-1118 ([`super::hub::Hub::every`]).
    #[cfg(test)]
    pub(crate) every: bool,
    /// Run once by the next turn that computed a batch, between its view and its advance.
    #[cfg(test)]
    pub(crate) before_advance: Hook,
}

/// A test's action inside a turn ([`Session::before_advance`]).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Hook(pub(crate) Option<Box<dyn FnOnce() + Send>>);

#[cfg(test)]
impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Hook(armed)"
        } else {
            "Hook"
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(s) = self.sub {
            self.reader.hub().unsubscribe(s);
        }
    }
}

/// The manifest's lost ranges at `v`.
fn recorded(v: &View) -> Vec<(LogPos, LogPos)> {
    let at = |(generation, sequence)| LogPos {
        generation,
        sequence,
    };
    v.lost_ranges()
        .iter()
        .map(|l| (at(l.first), at(l.last)))
        .collect()
}

fn next_pos(p: LogPos) -> LogPos {
    LogPos {
        generation: p.generation,
        sequence: p.sequence.saturating_add(1),
    }
}

/// The series an entry names, if any.
fn entry_series(e: &Entry) -> Option<u32> {
    match e {
        Entry::Definition(d) => Some(d.id),
        Entry::Alias { id, .. } | Entry::Retirement { id, .. } => Some(*id),
        Entry::ParameterChange(p) => Some(p.id),
        _ => None,
    }
}

fn selects(sel: &Selection, registry: &Registry, id: u32) -> bool {
    match registry.get(id) {
        Some(s) => sel.selects(SeriesId(id), &s.names, s.retired()),
        None => sel.selects(SeriesId(id), &[], false),
    }
}

/// What can give the session its next batch: the members of its `Latest`s and the series its `Tail`s select, by
/// the registry at `v` or at a `Tail`'s position, and the members alone, whose staleness a refresh changes; anything
/// while a read is not yet opened.
fn reach(reads: &BTreeMap<LiveId, Read>, v: &View) -> (Reach, Reach) {
    let registry = v.latest().registry();
    let mut ids = BTreeSet::new();
    let mut members = BTreeSet::new();
    for r in reads.values() {
        match r {
            Read::Latest(l) if l.opened => members.extend(l.members.iter().map(|s| s.0)),
            Read::Tail(t) if t.opened => {
                for s in registry.iter().chain(t.names.iter()) {
                    let id = s.def.id;
                    if selects(&t.sel, registry, id) || selects(&t.sel, &t.names, id) {
                        ids.insert(id);
                    }
                }
            }
            _ => return (Reach::All, Reach::All),
        }
    }
    ids.extend(&members);
    (Reach::Series(ids), Reach::Series(members))
}

/// What a batch is computed at: the view, the cut (its durable position, or one inside its round) and `now`.
struct At<'a> {
    r: &'a DurableReader,
    v: &'a View,
    cut: LogPos,
    inside: bool,
    now: i64,
}

impl At<'_> {
    fn registry(&self) -> Cow<'_, Registry> {
        if self.inside {
            Cow::Owned(self.v.latest().registry_at(self.cut))
        } else {
            Cow::Borrowed(self.v.latest().registry())
        }
    }

    fn live(&self, id: SeriesId) -> Result<Live, QueryError> {
        if self.inside {
            latest::live_upto(self.r, self.v, id, (self.now, self.cut))
        } else {
            latest::live(self.r, self.v, id, self.now)
        }
    }

    /// When series `id`, whose live state is `live`, may change with no new fact: a fact falling due or its
    /// staleness passing.
    fn due(&self, registry: &Registry, id: SeriesId, live: &Live) -> Option<i64> {
        let s = registry.get(id.0)?;
        let p = windows::params(&s.def);
        let versioned = s.def.versioning != Versioning::None;
        let state = self.v.latest().due(id.0, versioned, &p, self.now);
        let stale = match (live, p.staleness) {
            (Live::Value(v), Some(limit)) if !v.stale => Some(
                match (p.timing, self.r.clocks().since_refresh(id.0)) {
                    (KmTiming::Observed, Some(d)) => self.now.saturating_add(limit - d.min(limit)),
                    _ => v.fact.t.0.saturating_add(limit),
                }
                .saturating_add(1),
            ),
            _ => None,
        };
        state.into_iter().chain(stale).min()
    }

    fn resolve(&self, sel: &Selection, registry: &Registry) -> Vec<SeriesId> {
        let known: Vec<(SeriesId, &[String], bool)> = registry
            .iter()
            .map(|s| (SeriesId(s.def.id), s.names.as_slice(), s.retired()))
            .collect();
        sel.resolve(&known)
    }
}

/// What a call of `next` found: a batch, a time to sleep before anything can come, or nothing new yet.
enum Turn {
    Batch(UpdateBatch),
    Sleep(Duration),
    Wait,
}

/// A batch and the session's state after it, kept once the batch's cut proved live.
struct Next {
    batch: Option<UpdateBatch>,
    /// `None` when nothing changed.
    reads: Option<BTreeMap<LiveId, Read>>,
    at: (u64, usize),
    recorded: Vec<(LogPos, LogPos)>,
    named: Vec<(LogPos, LogPos)>,
    refreshes: u64,
    /// The session was closed as idle.
    idle: bool,
}

/// What the pieces of a batch brought, before the live reads are brought to its cut.
#[derive(Default)]
struct Walk {
    touched: BTreeSet<u32>,
    entries: bool,
    reset: Option<LogPos>,
    gaps: Vec<(LogPos, LogPos)>,
    /// Per `Tail`, its facts in position order.
    facts: BTreeMap<LiveId, Vec<Stored>>,
    /// Where the batch stopped inside a round: (round, pieces taken, cut).
    stop: Option<(u64, usize, LogPos)>,
    /// The gaps named that stand after the stop.
    named: Vec<(LogPos, LogPos)>,
}

impl Session {
    pub(crate) fn new(
        reader: Arc<DurableReader>,
        watch: Option<Watch>,
        opts: SessionOptions,
    ) -> Self {
        Session {
            reader,
            watch,
            opts,
            sub: None,
            bell: None,
            settled: None,
            rest: false,
            at: (0, 0),
            started: false,
            reads: BTreeMap::new(),
            last_batch: None,
            last_poll: None,
            last_full: None,
            progress: 0,
            refreshes: 0,
            judged: Duration::ZERO,
            recorded: Vec::new(),
            named: Vec::new(),
            found_lost: 0,
            #[cfg(test)]
            turns: 0,
            #[cfg(test)]
            every: false,
            #[cfg(test)]
            before_advance: Hook::default(),
        }
    }

    /// Opens live read `id` of a checked query.
    pub(crate) fn open(&mut self, id: LiveId, q: Query, opts: LiveOptions) {
        let read = match q {
            Query::Latest(sel) => Read::Latest(LatestRead {
                sel,
                opened: false,
                members: Vec::new(),
                shown: BTreeMap::new(),
                due: BTreeMap::new(),
            }),
            Query::Tail { sel } => Read::Tail(TailRead {
                sel,
                opts,
                opened: false,
                names: Registry::default(),
                from: LogPos::default(),
            }),
        };
        self.reads.insert(id, read);
        if let Some(s) = self.sub {
            self.reader.hub().open(s, self.cap());
        }
    }

    pub(crate) fn cancel(&mut self, id: LiveId) -> bool {
        let was = self.reads.remove(&id).is_some();
        if self.reads.is_empty() {
            self.leave();
        }
        was
    }

    /// Gives up the subscription: a later open starts afresh.
    fn leave(&mut self) {
        if let Some(s) = self.sub.take() {
            self.reader.hub().unsubscribe(s);
        }
        self.bell = None;
        self.settled = None;
        self.started = false;
    }

    fn mono(&self) -> Duration {
        self.reader.monotonic()
    }

    /// The largest backlog of the session's `Tail`s, the default with none.
    fn cap(&self) -> (u64, u64) {
        self.reads
            .values()
            .filter_map(|r| match r {
                Read::Tail(t) => Some((t.opts.max_backlog_rows, t.opts.max_backlog_bytes)),
                Read::Latest(_) => None,
            })
            .reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)))
            .unwrap_or_else(|| {
                let d = LiveOptions::default();
                (d.max_backlog_rows, d.max_backlog_bytes)
            })
    }

    /// The earliest due time of the session's `Latest`s.
    fn due(&self) -> Option<i64> {
        self.reads
            .values()
            .filter_map(|r| match r {
                Read::Latest(l) => l.due.values().min().copied(),
                Read::Tail(_) => None,
            })
            .min()
    }

    /// How long a session waiting on its bell may wait before a `Latest` falls due: rechecked at least once a second
    /// while one is due, as `now` follows the wall clock, which can jump.
    fn until_due(&self) -> Duration {
        match self.due() {
            None => Duration::MAX,
            Some(d) => {
                let ns = u64::try_from(d.saturating_sub(self.reader.now())).unwrap_or(0);
                // A state that cannot tell is due at `now + 1` on every turn; a batch cannot come sooner than the interval.
                Duration::from_nanos(ns)
                    .max(self.opts.min_batch_interval)
                    .min(FULL_REFRESH_EVERY)
            }
        }
    }

    pub(crate) fn next(&mut self, timeout: Duration) -> Result<Option<UpdateBatch>, QueryError> {
        let until = Instant::now() + timeout;
        loop {
            // Read before the turn: a ring during it makes the wait below return at once.
            let rung = self.bell.as_ref().map(|b| b.rung());
            let turn = self.try_batch()?;
            let left = until.saturating_duration_since(Instant::now());
            if !matches!(turn, Turn::Sleep(_)) {
                self.settled = rung.filter(|_| !self.rest);
            }
            let rest = match turn {
                Turn::Batch(b) => return Ok(Some(b)),
                _ if left.is_zero() => return Ok(None),
                Turn::Sleep(d) => d,
                Turn::Wait => self.opts.poll_interval,
            };
            let bell = self
                .bell
                .as_ref()
                .zip(rung)
                .filter(|_| !self.reader.fanout_failed.load(Ordering::SeqCst));
            match (&self.watch, bell) {
                (Some(w), Some((b, seen))) if matches!(turn, Turn::Wait) && !w.stopped() => {
                    b.wait(seen, left.min(self.until_due()));
                }
                // Inside the batch interval with nothing rung since the last batch there is nothing to sleep out: the
                // ring that brings something ends the wait, and the next turn sleeps out what is left of the interval.
                (Some(w), Some((b, _)))
                    if self.settled.is_some_and(|s| b.rung() == s) && !w.stopped() =>
                {
                    let s = self.settled.unwrap_or_default();
                    b.wait(s, left.min(self.until_due()));
                }
                (Some(w), _) if matches!(turn, Turn::Wait) && !w.stopped() => {
                    w.wait_progress(self.progress, left.min(rest));
                }
                _ => std::thread::sleep(left.min(rest)),
            }
        }
    }

    fn try_batch(&mut self) -> Result<Turn, QueryError> {
        #[cfg(test)]
        {
            self.turns += 1;
        }
        if self.reads.is_empty() {
            return Ok(Turn::Sleep(Duration::MAX));
        }
        let mono = self.mono();
        if let Some(s) = self.sub {
            self.reader.hub().touch(s, mono);
        }
        let opening = self.reads.values().any(|r| !r.opened());
        if !opening && let Some(t) = self.last_batch {
            let since = mono.saturating_sub(t);
            if since < self.opts.min_batch_interval {
                return Ok(Turn::Sleep(self.opts.min_batch_interval - since));
            }
        }
        let r = self.reader.clone();
        let sub = match self.sub {
            Some(s) => s,
            None => {
                let (s, bell) = r.hub().subscribe(self.cap(), mono);
                self.sub = Some(s);
                self.bell = Some(bell);
                #[cfg(test)]
                if self.every {
                    r.hub().every(s);
                }
                super::fanout::ensure(&r);
                s
            }
        };
        let progress = self
            .watch
            .as_ref()
            .map(|w| w.wait_progress(self.progress, Duration::ZERO));
        let due = self.due().is_some_and(|d| d <= r.now());
        let refreshed = r.clocks().refreshes() != self.refreshes;
        // A loss another read found reaches the shared tail by a refresh, before any batch could show its fact.
        let found_lost = r.found_lost();
        let must = opening
            || due
            || refreshed
            || found_lost != self.found_lost
            || progress.is_some_and(|p| p != self.progress);
        let interval = progress.is_none()
            && self
                .last_poll
                .is_none_or(|t| mono.saturating_sub(t) >= self.opts.poll_interval);
        // Within a second of its last refresh a reader process's poll reads the mark alone (S13): unchanged, the log
        // holds no new batch; a change to the manifest alone waits for the next full refresh.
        let fresh = self
            .last_full
            .is_some_and(|t| mono.saturating_sub(t) < FULL_REFRESH_EVERY);
        let refresh = must || (interval && (!fresh || r.mark_moved() != Some(false)));
        // Rounds other calls ran since the last batch serve it with no refresh, while their cut is live.
        let current = if refresh {
            None
        } else {
            r.current()
                .filter(|v| v.round() > self.at.0 && r.check(v).is_ok())
        };
        let v = match current {
            Some(v) => v,
            None if refresh => {
                // A failed refresh leaves the poll due, so its error comes at every call it lasts (Q-951).
                let v = r.snapshot().inspect_err(|_| self.last_full = None)?;
                (self.last_poll, self.last_full) = (Some(mono), Some(mono));
                if let Some(p) = progress {
                    self.progress = p;
                }
                self.found_lost = found_lost;
                v
            }
            None => {
                if interval {
                    self.last_poll = Some(mono);
                }
                return Ok(Turn::Wait);
            }
        };
        if !self.started {
            self.started = true;
            self.at = (v.round(), 0);
            self.recorded = recorded(&v);
            self.refreshes = r.clocks().refreshes();
            r.hub().start(sub, v.round(), self.cap());
        }
        let out = self.batch(&r, &v, sub, mono);
        let reach = match &out {
            Ok(n) => reach(n.reads.as_ref().unwrap_or(&self.reads), &v),
            Err(_) => (Reach::All, Reach::All),
        };
        let round = v.round();
        let live = r.end(v);
        let next = out?;
        live?;
        if let Some(reads) = next.reads {
            self.reads = reads;
        }
        self.at = next.at;
        self.rest = next.at != (round, 0);
        self.recorded = next.recorded;
        self.named = next.named;
        self.refreshes = next.refreshes;
        self.judged = mono;
        #[cfg(test)]
        if let Some(f) = self.before_advance.0.take() {
            f();
        }
        if next.idle {
            self.leave();
        } else {
            r.hub().advance(sub, next.at.0, reach);
        }
        Ok(match next.batch {
            Some(b) => {
                self.last_batch = Some(mono);
                Turn::Batch(b)
            }
            None => Turn::Wait,
        })
    }

    /// How many times its bell has rung, once subscribed.
    #[cfg(test)]
    pub(crate) fn rung(&self) -> Option<u64> {
        self.bell.as_ref().map(|b| b.rung())
    }

    /// A `Latest` falls due by time: its wait ends without a ring.
    #[cfg(test)]
    pub(crate) fn due_now(&self) -> bool {
        self.due().is_some_and(|d| d <= self.reader.now())
    }

    /// The batch at view `v` and the session's next state, which the caller keeps once the cut proved live.
    fn batch(
        &self,
        r: &DurableReader,
        v: &View,
        sub: u64,
        mono: Duration,
    ) -> Result<Next, QueryError> {
        let taken = r.hub().take(sub, v.round(), mono);
        let refreshes = r.clocks().refreshes();
        let recorded = recorded(v);
        let now = r.now();
        let quiet = !taken.idle
            && !taken.lagged
            && taken.backlog == (0, 0)
            && taken.rounds.iter().all(|x| x.is_empty())
            && refreshes == self.refreshes
            && recorded == self.recorded
            && self.reads.values().all(Read::opened)
            && self.due().is_none_or(|d| d > now);
        if quiet {
            return Ok(Next {
                batch: None,
                reads: None,
                at: (v.round(), 0),
                recorded,
                named: Vec::new(),
                refreshes,
                idle: false,
            });
        }
        let mut reads = self.reads.clone();
        let mut events: Vec<(LiveId, LiveEvent)> = Vec::new();
        if taken.idle {
            for id in std::mem::take(&mut reads).into_keys() {
                events.push((id, LiveEvent::Closed(CloseReason::Idle)));
            }
            let b = UpdateBatch {
                durable: pos(v.state().durable),
                now: Timestamp(now),
                events,
            };
            return Ok(Next {
                batch: Some(b),
                reads: Some(reads),
                at: self.at,
                recorded,
                named: Vec::new(),
                refreshes,
                idle: true,
            });
        }
        let lagged = taken.lagged
            || reads.values().any(|x| match x {
                Read::Tail(t) => {
                    t.opened
                        && (taken.backlog.0 > t.opts.max_backlog_rows
                            || taken.backlog.1 > t.opts.max_backlog_bytes)
                }
                Read::Latest(_) => false,
            });
        let mut walk = self.walk(v, &taken.rounds, &mut reads, lagged);
        for (a, b) in &recorded {
            if !self.recorded.contains(&(*a, *b)) && !walk.gaps.contains(&(*a, *b)) {
                walk.gaps.push((*a, *b));
            }
        }
        let named = std::mem::take(&mut walk.named);
        let (cut, inside, at) = match walk.stop {
            Some((seq, n, cut)) => (cut, true, (seq - 1, n)),
            None => (v.state().durable, false, (v.round(), 0)),
        };
        let at_cut = At {
            r,
            v,
            cut,
            inside,
            now,
        };
        let registry = at_cut.registry();
        let refreshed: BTreeSet<u32> = if refreshes == self.refreshes {
            BTreeSet::new()
        } else {
            let since = u64::try_from(self.judged.as_nanos()).unwrap_or(u64::MAX);
            r.clocks().refreshed_since(since).into_iter().collect()
        };
        for (id, read) in reads.iter_mut() {
            let mut mine = Vec::new();
            match read {
                Read::Tail(t) => walk::tail(&at_cut, (*id, t), &walk, lagged, &mut mine),
                Read::Latest(l) => walk::latest_read(
                    &at_cut,
                    &registry,
                    l,
                    &walk,
                    (lagged, &refreshed),
                    &mut mine,
                )?,
            }
            mine.sort_by_key(event_rank);
            events.extend(mine.into_iter().map(|e| (*id, e)));
        }
        let batch = (!events.is_empty()).then(|| UpdateBatch {
            durable: pos(cut),
            now: Timestamp(now),
            events,
        });
        Ok(Next {
            batch,
            reads: Some(reads),
            at,
            recorded,
            named,
            refreshes,
            idle: false,
        })
    }
}

mod walk;

#[cfg(test)]
mod cases;
#[cfg(test)]
mod differential;
#[cfg(test)]
mod fanout;
#[cfg(test)]
mod lags;
#[cfg(test)]
mod races;
#[cfg(test)]
mod stops;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod waits;
