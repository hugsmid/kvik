//! A live session on the durable backend (the query design's section 3.6; roadmap 9.0d): `Latest` and `Tail` kept up
//! to date from the reader's shared tail ([`super::hub`]). A batch is computed when `next` is called, at the cut of a
//! view the reader published, or at a position inside its round when a `Tail` meets its row limit (Q-1014). Nothing
//! runs between calls: the writer's process waits on the writer's progress, a reader process polls the mark.
//!
//! A `Latest` judges again only the series whose facts or entries the batch brings, those due by time alone and those
//! a call refreshed the live staleness of since the last batch; a reset or a loss opens it again. A `Tail` delivers the
//! facts its selection takes as known at each fact's position (Q-895); a reset, a loss and a lag are events, never
//! silence (Q-1011, Q-1013).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use km::registry::Timing as KmTiming;
use kvik_log::entries::{Entry, Versioning};
use kvik_log::writer::{Position as LogPos, Watch};
use kvik_model as km;

use super::hub::Piece;
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

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Session {
    reader: Arc<DurableReader>,
    watch: Option<Watch>,
    opts: SessionOptions,
    sub: Option<u64>,
    /// The last round taken whole, and how many pieces of the next were taken.
    at: (u64, usize),
    started: bool,
    reads: BTreeMap<LiveId, Read>,
    last_batch: Option<Duration>,
    last_poll: Option<Duration>,
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

/// What a batch is computed at: the view, the cut (its durable position, or one inside its round) and `now`.
struct At<'a> {
    r: &'a DurableReader,
    v: &'a View,
    cut: LogPos,
    inside: bool,
    now: i64,
}

impl At<'_> {
    fn registry(&self) -> Registry {
        if self.inside {
            self.v.latest().registry_at(self.cut)
        } else {
            self.v.latest().registry().clone()
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
            at: (0, 0),
            started: false,
            reads: BTreeMap::new(),
            last_batch: None,
            last_poll: None,
            progress: 0,
            refreshes: 0,
            judged: Duration::ZERO,
            recorded: Vec::new(),
            named: Vec::new(),
            found_lost: 0,
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
            self.reader.hub().start(s, self.at.0, self.cap());
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

    pub(crate) fn next(&mut self, timeout: Duration) -> Result<Option<UpdateBatch>, QueryError> {
        let until = Instant::now() + timeout;
        loop {
            if let Some(b) = self.try_batch()? {
                return Ok(Some(b));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let wait = left.min(self.opts.poll_interval);
            match &self.watch {
                Some(w) if !w.stopped() => {
                    w.wait_progress(self.progress, wait);
                }
                _ => std::thread::sleep(wait),
            }
        }
    }

    fn try_batch(&mut self) -> Result<Option<UpdateBatch>, QueryError> {
        if self.reads.is_empty() {
            return Ok(None);
        }
        let mono = self.mono();
        let opening = self.reads.values().any(|r| !r.opened());
        if !opening
            && self
                .last_batch
                .is_some_and(|t| mono.saturating_sub(t) < self.opts.min_batch_interval)
        {
            return Ok(None);
        }
        let r = self.reader.clone();
        let sub = match self.sub {
            Some(s) => s,
            None => {
                let s = r.hub().subscribe(self.cap(), mono);
                self.sub = Some(s);
                s
            }
        };
        let progress = self
            .watch
            .as_ref()
            .map(|w| w.wait_progress(self.progress, Duration::ZERO));
        let poll = opening
            || match progress {
                Some(p) => p != self.progress,
                None => self
                    .last_poll
                    .is_none_or(|t| mono.saturating_sub(t) >= self.opts.poll_interval),
            };
        let due = self.due().is_some_and(|d| d <= r.now());
        let refreshed = r.clocks().refreshes() != self.refreshes;
        // A loss another read found reaches the shared tail by a refresh, before any batch could show its fact.
        let found_lost = r.found_lost();
        let poll = poll || found_lost != self.found_lost;
        // Rounds other calls ran since the last batch serve it with no read of the mark, while their cut is live.
        let current = if poll || due || refreshed {
            None
        } else {
            r.current()
                .filter(|v| v.round() > self.at.0 && r.check(v).is_ok())
        };
        let v = match current {
            Some(v) => v,
            None if poll || due || refreshed => {
                self.last_poll = Some(mono);
                if let Some(p) = progress {
                    self.progress = p;
                }
                self.found_lost = found_lost;
                r.snapshot()?
            }
            None => return Ok(None),
        };
        if !self.started {
            self.started = true;
            self.at = (v.round(), 0);
            self.recorded = recorded(&v);
            self.refreshes = r.clocks().refreshes();
            r.hub().start(sub, v.round(), self.cap());
        }
        let out = self.batch(&r, &v, sub, mono);
        let live = r.end(v);
        let next = out?;
        live?;
        if let Some(reads) = next.reads {
            self.reads = reads;
        }
        self.at = next.at;
        self.recorded = next.recorded;
        self.named = next.named;
        self.refreshes = next.refreshes;
        self.judged = mono;
        if next.idle {
            self.leave();
        } else {
            r.hub().advance(sub, next.at.0);
        }
        if next.batch.is_some() {
            self.last_batch = Some(mono);
        }
        Ok(next.batch)
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
                Read::Tail(t) => tail(&at_cut, (*id, t), &walk, lagged, &mut mine),
                Read::Latest(l) => latest_read(
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

    /// Walks the pieces of the rounds after the session's cursor: the facts each opened `Tail` takes, up to the
    /// first row limit met, and what the `Latest`s must judge again. Every gap taken is named, past the stop too, so
    /// no batch carries a fact the reader knows lost nor leaves a loss below its cut unnamed (Q-1011); a fact a
    /// later void or gap drops is never taken, so the stop's cut lies below every void after it, which waits.
    fn walk(
        &self,
        v: &View,
        rounds: &[Arc<super::hub::Round>],
        reads: &mut BTreeMap<LiveId, Read>,
        lagged: bool,
    ) -> Walk {
        let pieces: Vec<(u64, usize, &Piece)> = rounds
            .iter()
            .flat_map(|r| r.pieces().map(move |(i, p)| (r.seq, i, p)))
            .filter(|(seq, i, _)| *seq != self.at.0 + 1 || *i >= self.at.1)
            .collect();
        let mut later_void = vec![None; pieces.len()];
        let mut least: Option<LogPos> = None;
        for (k, (_, _, p)) in pieces.iter().enumerate().rev() {
            later_void[k] = least;
            if let Piece::Void(above) = p {
                least = Some(least.map_or(*above, |l| l.min(*above)));
            }
        }
        let gaps: Vec<(usize, LogPos, LogPos)> = pieces
            .iter()
            .enumerate()
            .filter_map(|(k, (_, _, p))| match p {
                Piece::Gap(a, b) => Some((k, *a, *b)),
                _ => None,
            })
            .collect();
        let dropped = |k: usize, at: LogPos| {
            later_void[k].is_some_and(|above| at > above)
                || gaps.iter().any(|(j, a, b)| *j > k && *a <= at && at <= *b)
        };
        let mut w = Walk::default();
        for (k, (seq, i, piece)) in pieces.iter().enumerate() {
            if w.stop.is_some() {
                if let Piece::Gap(a, b) = piece {
                    w.named.push((*a, *b));
                    self.name(&mut w, (*a, *b));
                }
                continue;
            }
            match piece {
                Piece::Fact(s) => {
                    let id = s.row.series_id;
                    w.touched.insert(id);
                    if lagged || dropped(k, s.position) {
                        continue;
                    }
                    let wants: Vec<LiveId> = reads
                        .iter()
                        .filter_map(|(k, x)| match x {
                            Read::Tail(t) if t.opened && selects(&t.sel, &t.names, id) => Some(*k),
                            _ => None,
                        })
                        .collect();
                    let full = wants.iter().any(|k| {
                        let max = match &reads[k] {
                            Read::Tail(t) => t.opts.max_rows,
                            Read::Latest(_) => usize::MAX,
                        };
                        w.facts.get(k).map_or(0, Vec::len) >= max
                    });
                    if full {
                        let cut = LogPos {
                            generation: s.position.generation,
                            sequence: s.position.sequence.saturating_sub(1),
                        };
                        w.stop = Some((*seq, *i, cut));
                        continue;
                    }
                    for k in wants {
                        w.facts.entry(k).or_default().push(s.clone());
                    }
                }
                Piece::Entry(e) => {
                    w.entries = true;
                    w.touched.extend(entry_series(e));
                    for x in reads.values_mut() {
                        if let Read::Tail(t) = x
                            && t.opened
                        {
                            t.names.apply(e);
                        }
                    }
                }
                Piece::Void(above) => {
                    w.reset = Some(w.reset.map_or(*above, |r| r.min(*above)));
                    for x in reads.values_mut() {
                        if let Read::Tail(t) = x
                            && t.opened
                        {
                            t.from = t.from.min(*above);
                            t.names = v.latest().registry_at(*above);
                        }
                    }
                }
                Piece::Gap(a, b) => self.name(&mut w, (*a, *b)),
            }
        }
        w
    }

    /// Names gap `g` in the batch unless a batch that stopped before it named it already.
    fn name(&self, w: &mut Walk, g: (LogPos, LogPos)) {
        if !self.named.contains(&g) && !w.gaps.contains(&g) {
            w.gaps.push(g);
        }
    }
}

/// Brings `Tail` `t` to the batch's cut.
fn tail(
    at: &At<'_>,
    (id, t): (LiveId, &mut TailRead),
    w: &Walk,
    lagged: bool,
    out: &mut Vec<LiveEvent>,
) {
    if !t.opened {
        t.opened = true;
        t.from = at.cut;
        t.names = at.registry();
        out.push(LiveEvent::Opened(Answer::Tail));
        return;
    }
    if let Some(above) = w.reset {
        out.push(LiveEvent::Reset { above: pos(above) });
        out.push(LiveEvent::Opened(Answer::Tail));
    }
    for (a, b) in &w.gaps {
        out.push(LiveEvent::Gap {
            first: pos(*a),
            last: pos(*b),
        });
    }
    if lagged {
        if t.from < at.cut {
            out.push(LiveEvent::Lagged {
                from: pos(next_pos(t.from)),
                to: pos(at.cut),
            });
        }
        t.names = at.registry();
    } else if let Some(facts) = w.facts.get(&id).filter(|f| !f.is_empty()) {
        out.push(LiveEvent::Appended(Appended::Facts(
            facts.iter().map(fact_out).collect(),
        )));
    }
    t.from = at.cut;
}

/// Brings `Latest` `l` to the batch's cut: opened again after a reset or a loss, else its membership when entries
/// came and the members the batch touched, due by time, or `refreshed` (their live staleness).
fn latest_read(
    at: &At<'_>,
    registry: &Registry,
    l: &mut LatestRead,
    w: &Walk,
    (lagged, refreshed): (bool, &BTreeSet<u32>),
    out: &mut Vec<LiveEvent>,
) -> Result<(), QueryError> {
    let reopen = !l.opened || w.reset.is_some() || !w.gaps.is_empty();
    if l.opened {
        if let Some(above) = w.reset {
            out.push(LiveEvent::Reset { above: pos(above) });
        }
        for (a, b) in &w.gaps {
            out.push(LiveEvent::Gap {
                first: pos(*a),
                last: pos(*b),
            });
        }
    }
    if reopen {
        l.opened = true;
        l.members = at.resolve(&l.sel, registry);
        l.shown.clear();
        l.due.clear();
        let mut answer = Vec::new();
        for id in &l.members {
            let live = at.live(*id)?;
            if let Some(d) = at.due(registry, *id, &live) {
                l.due.insert(*id, d);
            }
            l.shown.insert(*id, live.clone());
            answer.push((*id, live));
        }
        out.push(LiveEvent::Opened(Answer::Latest(answer)));
        return Ok(());
    }
    let mut judge: BTreeSet<SeriesId> = BTreeSet::new();
    if w.entries || lagged {
        let members = at.resolve(&l.sel, registry);
        for id in l.members.iter().filter(|id| !members.contains(id)) {
            out.push(LiveEvent::Left { series: *id });
            l.shown.remove(id);
            l.due.remove(id);
        }
        for (index, id) in members.iter().enumerate() {
            if l.shown.contains_key(id) {
                continue;
            }
            let live = at.live(*id)?;
            let info = latest::series(at.v, &SeriesKey::Id(*id))?;
            if let Some(d) = at.due(registry, *id, &live) {
                l.due.insert(*id, d);
            }
            l.shown.insert(*id, live.clone());
            out.push(LiveEvent::Joined {
                series: *id,
                info: Box::new(info),
                index,
                part: crate::read::Part::Live(live),
            });
        }
        l.members = members;
        if lagged {
            judge.extend(l.members.iter().copied());
        }
    }
    judge.extend(
        l.members
            .iter()
            .filter(|id| {
                w.touched.contains(&id.0)
                    || refreshed.contains(&id.0)
                    || l.due.get(id).is_some_and(|d| *d <= at.now)
            })
            .copied(),
    );
    for id in judge {
        let Some(old) = l.shown.get(&id) else {
            continue;
        };
        let live = at.live(id)?;
        match at.due(registry, id, &live) {
            Some(d) => l.due.insert(id, d),
            None => l.due.remove(&id),
        };
        if *old != live {
            l.shown.insert(id, live.clone());
            out.push(LiveEvent::Updated(Updated::Live { series: id, live }));
        }
    }
    Ok(())
}

#[cfg(test)]
mod cases;
#[cfg(test)]
mod differential;
#[cfg(test)]
mod stops;
#[cfg(test)]
mod tests;
