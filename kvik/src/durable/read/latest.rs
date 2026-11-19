//! Latest values from the series directory and the log tail (roadmap 9.0c; the query design's section 4): what a
//! reader holds instead of every fact for `live`, `series` and `losses`.
//!
//! A reader continues the newest series directory that is valid for it (`format.md` A.18; Q-1003) with the registry
//! file (A.16, Q-1001), and folds every batch above the directory's `through` into the same bounded per-series state
//! the writer's fold keeps ([`State`]), so its memory does not grow with the facts. With no such directory it folds
//! the kept log from its start. A void or a loss that reaches what was folded breaks the state: the reader then
//! answers from its fact index until it is reopened (Q-1006). What a state cannot tell, the reader finds in the log
//! at the call's cut ([`live`], Q-1004).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use km::registry::Timing as KmTiming;
use km::summary::Params;
use kvik_log::content::Item;
use kvik_log::directory::series::SeriesDirectory;
use kvik_log::entries::{Entry, Versioning};
use kvik_log::manifest::LostRange;
use kvik_log::mark::ResetStamp as LogStamp;
use kvik_log::reader::{FoundFact, ReadEvent, Wanted, WantedAt};
use kvik_log::registry::RegistryFile;
use kvik_log::writer::Position as LogPos;
use kvik_model as km;

use super::pmap::PMap;
use super::view::View;
use crate::durable::derived::state::{Kept, State};
use crate::durable::history::WINDOW_SPAN;
use crate::durable::index::{self, Stored};
use crate::durable::loss::{Find, Finder};
use crate::durable::reader::DurableReader;
use crate::durable::registry::{Registry, Series};
use crate::durable::windows;
use crate::error::QueryError;
use crate::health::LossReport;
use crate::read::Live;
use crate::series::{SeriesInfo, SeriesKey};
use crate::types::SeriesId;

/// One series as a reader holds it: the fold's state, and the full facts the fold met of those the state names
/// (an entry holds no flags, receive time or text, A.18).
#[derive(Clone, Debug, Default)]
struct Held {
    state: State,
    rows: Vec<Stored>,
}

impl Held {
    fn named(&self) -> impl Iterator<Item = Kept> + '_ {
        let [p, l] = self.state.ahead();
        self.state.candidate().into_iter().chain(p).chain(l)
    }
}

/// See the module documentation. Clones share structure (Q-900).
#[derive(Clone, Debug, Default)]
pub(crate) struct Latest {
    registry: Registry,
    /// Registry entries with the receive time of their batch.
    entries: PMap<LogPos, (i64, Entry)>,
    /// Entries at or below this came from the registry file.
    file_through: LogPos,
    finder: Finder,
    series: PMap<u32, Arc<Held>>,
    /// Facts at or below this are in the series directory's entries.
    seeded: LogPos,
    /// A batch above `seeded` was read.
    past_seed: bool,
    /// The highest position folded or seeded.
    folded: LogPos,
    /// The fold's `now`: the directory's judging time and every receive time folded since.
    horizon: i64,
    broken: bool,
}

/// What a state tells of a series' live fact at `now`.
#[derive(Debug)]
pub(crate) enum Judged<'a> {
    /// No live fact.
    None,
    /// This fact, met by the fold.
    Fact(&'a Stored),
    /// The fact at this position, which the fold did not meet.
    Row(Kept),
    /// The state cannot tell.
    Unsure,
}

impl Latest {
    /// A state that folds the kept log from its start.
    pub(crate) fn new() -> Self {
        Latest {
            horizon: i64::MIN,
            ..Latest::default()
        }
    }

    /// A state continuing `dir`, judged at `judged_at`, with the registry of `file` (Q-1001, Q-1003).
    pub(crate) fn seeded(file: &RegistryFile, dir: &SeriesDirectory, judged_at: i64) -> Self {
        let mut l = Latest::new();
        let at = |sequence| LogPos {
            generation: file.generation,
            sequence,
        };
        for (sequence, received_at, e) in &file.entries {
            l.registry.apply(e);
            l.entries.insert(at(*sequence), (*received_at, e.clone()));
        }
        l.finder
            .seed_recoveries(file.entries.iter().filter_map(|(sequence, _, e)| match e {
                Entry::Recovery(r) => Some((at(*sequence), r.clone())),
                _ => None,
            }));
        l.registry.note_fact(file.max_series_id);
        l.file_through = at(file.through);
        l.series = PMap::from_sorted(dir.entries.iter().map(|e| {
            l.registry.note_fact(e.series_id);
            let held = Held {
                state: State::of_entry(e, judged_at),
                rows: Vec::new(),
            };
            (e.series_id, Arc::new(held))
        }));
        l.finder.seed_floor(dir.through);
        l.seeded = dir.through;
        l.folded = dir.through;
        l.horizon = judged_at;
        l
    }

    /// Applies what a refresh up to the mark's position `durable` found, in order. Returns the losses it found.
    pub(crate) fn apply(&mut self, events: &[ReadEvent], durable: LogPos) -> Vec<Find> {
        let found = self.finder.apply(events, durable);
        for e in events {
            match e {
                ReadEvent::Batch(b) => {
                    self.past_seed |= b.last > self.seeded;
                    self.horizon = self.horizon.max(b.content.received_at);
                    let mut sequence = b.first.sequence;
                    for item in &b.content.items {
                        let p = LogPos {
                            generation: b.first.generation,
                            sequence,
                        };
                        sequence += item.sequences();
                        match item {
                            Item::Fact(row) if p > self.seeded => {
                                self.fact(Stored {
                                    position: p,
                                    received_at: b.content.received_at,
                                    row: row.clone(),
                                });
                            }
                            Item::Fact(_) => {}
                            Item::Entry(e) if matches!(**e, Entry::Ignored { .. }) => {}
                            Item::Entry(e) if p > self.file_through => {
                                self.folded = self.folded.max(p);
                                self.registry.apply(e);
                                self.entries
                                    .insert(p, (b.content.received_at, (**e).clone()));
                            }
                            Item::Entry(_) => {}
                        }
                    }
                }
                ReadEvent::Void { above, .. } => self.void(*above),
                // Damage met before the first batch above the directory may hold positions it covers (A.18).
                ReadEvent::Corruption { region, .. }
                    if self.seeded != LogPos::default()
                        && !self.past_seed
                        && region
                            .last_sequence_before
                            .is_none_or(|s| s < self.seeded.sequence) =>
                {
                    self.broken = true;
                }
                ReadEvent::Corruption { .. } => {}
            }
        }
        found
    }

    fn fact(&mut self, s: Stored) {
        let id = s.row.series_id;
        let versioned = self
            .registry
            .get(id)
            .is_some_and(|x| x.def.versioning != Versioning::None);
        self.registry.note_fact(id);
        self.folded = self.folded.max(s.position);
        let held = Arc::make_mut(self.series.get_or_insert_with(id, Arc::default));
        held.state
            .apply(&s.row, s.position, versioned, self.horizon);
        let named: Vec<LogPos> = held.named().map(|k| k.position).collect();
        held.rows.retain(|r| named.contains(&r.position));
        if named.contains(&s.position) {
            held.rows.push(s);
        }
    }

    /// Forgets everything above `above`; a void below what was folded breaks the state.
    fn void(&mut self, above: LogPos) {
        if above < self.folded {
            self.broken = true;
        }
        let taken = self.entries.len();
        self.entries.retain(|p, _| *p <= above);
        if self.entries.len() != taken {
            self.registry = Registry::default();
            for (_, e) in self.entries.values() {
                self.registry.apply(e);
            }
            for (id, _) in self.series.iter() {
                self.registry.note_fact(*id);
            }
        }
    }

    /// A range the process found lost after it was read (Q-938): one at or below what was folded, that no lost range
    /// of the manifest holds, breaks the state (A.18; Q-952 for the writer's fold).
    pub(crate) fn lose(&mut self, first: LogPos, last: LogPos, recorded: &[LostRange]) {
        let at = |p: LogPos| (p.generation, p.sequence);
        let held = recorded
            .iter()
            .any(|r| r.first <= at(first) && at(last) <= r.last);
        if first <= self.folded && !held {
            self.broken = true;
        }
    }

    /// A range the manifest records since the state was seeded or last compared: one that starts at or below what was
    /// folded breaks the state, since the log reader does not read again what it returned (Q-1006).
    pub(crate) fn lose_recorded(&mut self, first: LogPos) {
        if first <= self.folded {
            self.broken = true;
        }
    }

    /// Whether a void or a loss reached what was folded (Q-1006).
    pub(crate) fn broken(&self) -> bool {
        self.broken
    }

    /// The state no longer follows the log: a reset met before the first pass read the mark again.
    pub(crate) fn break_off(&mut self) {
        self.broken = true;
    }

    /// The registry.
    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    /// What the registry knows about a series, with its latest recorded `t` and lateness reference.
    pub(crate) fn series(&self, key: &SeriesKey) -> Result<SeriesInfo, QueryError> {
        let s = index::lookup(&self.registry, key).map_err(crate::backend::Error::query)?;
        let (latest, reference) = self
            .series
            .get(&s.def.id)
            .map_or((None, None), |h| h.state.latest());
        Ok(index::series_info(s, latest, reference))
    }

    /// The registry file's content up to `through` (`format.md` A.16).
    pub(crate) fn registry_file(&self, through: LogPos, floor: u32) -> RegistryFile {
        index::registry_file(&self.entries, &self.registry, through, floor)
    }

    /// Losses: the manifest's lost ranges, the recovery entries of the registry file and of the log read, and what
    /// the reads found.
    pub(crate) fn losses(&self, recorded: &[LostRange]) -> Vec<LossReport> {
        self.finder.losses(recorded)
    }

    /// What the state tells of series `id`'s live fact at `now` (spec §2.3): the candidate once the facts ahead
    /// that fell due are promoted, unless it may not be the live fact (A.18 bit 3) or is not ready at `now`: above
    /// it (a reader's clock behind the writer's, A.18), or an observed interval reading not yet ended.
    pub(crate) fn judge(&self, id: u32, versioned: bool, p: &Params, now: i64) -> Judged<'_> {
        let Some(h) = self.series.get(&id) else {
            return Judged::None;
        };
        let (candidate, unsure) = h.state.promoted(now, versioned);
        if unsure {
            return Judged::Unsure;
        }
        match candidate {
            None => Judged::None,
            // `ready_at(t)` is never before `t`.
            Some(c) if p.ready_at(c.t) > now => Judged::Unsure,
            Some(c) => match h.rows.iter().find(|r| r.position == c.position) {
                Some(r) => Judged::Fact(r),
                None => Judged::Row(c),
            },
        }
    }

    /// The facts of series `id` folded, those of a continued entry counted.
    pub(crate) fn fact_count(&self, id: u32) -> u64 {
        self.series.get(&id).map_or(0, |h| h.state.count())
    }

    /// Every series whose candidate at `now` the fold did not meet, with it.
    pub(crate) fn unmet(&self, now: i64) -> Vec<(u32, Kept)> {
        self.series
            .iter()
            .filter_map(|(id, h)| {
                let versioned = self
                    .registry
                    .get(*id)
                    .is_some_and(|s| s.def.versioning != Versioning::None);
                let c = h.state.promoted(now, versioned).0?;
                (!h.rows.iter().any(|r| r.position == c.position)).then_some((*id, c))
            })
            .collect()
    }

    /// Series held and full facts kept, for the memory tests.
    #[cfg(test)]
    pub(crate) fn footprint(&self) -> (usize, usize, usize) {
        let rows = self.series.values().map(|h| h.rows.len()).sum();
        (self.series.len(), rows, self.entries.len())
    }
}

/// The current fact of each `t` among `facts` (spec §2.2): the highest rank, `(changed_at, position)` in a versioned
/// series.
fn current(facts: Vec<FoundFact>, versioned: bool) -> BTreeMap<i64, Stored> {
    let rank = |s: &Stored| (if versioned { s.row.changed_at } else { None }, s.position);
    let mut out: BTreeMap<i64, Stored> = BTreeMap::new();
    for f in facts {
        let s = Stored {
            position: f.position,
            received_at: f.received_at,
            row: f.row,
        };
        match out.get(&s.row.t) {
            Some(e) if rank(e) >= rank(&s) => {}
            _ => {
                out.insert(s.row.t, s);
            }
        }
    }
    out
}

/// The live fact of `series` at `now` from the log at `v`'s cut: event time read backwards from `now` in spans that
/// double from the recent window's, so that a series whose live fact is recent costs one span (Q-1004).
fn search(
    r: &DurableReader,
    v: &View,
    series: &Series,
    p: &Params,
    now: i64,
) -> Result<Option<Stored>, QueryError> {
    r.count_search();
    let versioned = series.def.versioning != Versioning::None;
    let (mut hi, mut span) = (now, WINDOW_SPAN);
    loop {
        let lo = hi.saturating_sub(span);
        let wanted = [Wanted {
            series_id: series.def.id,
            from: lo,
            to: hi,
        }];
        let found = current(r.lookup(v, &wanted)?, versioned);
        if let Some((_, s)) = found
            .range((Bound::Unbounded, Bound::Included(now)))
            .rev()
            .find(|(_, s)| windows::may_be_live(series, p, s, now))
        {
            return Ok(Some(s.clone()));
        }
        if lo == i64::MIN {
            return Ok(None);
        }
        hi = lo - 1;
        span = span.saturating_mul(2);
    }
}

/// A live fact searched for in the log, kept while it is the answer: while its series folds no further fact, under
/// the same reset stamp, for a `now` from the one it was found at up to the first time a fact the search saw ahead of
/// it may become live (the query design's section 4: the reader finds the next value once).
#[derive(Clone, Debug)]
pub(crate) struct Searched {
    facts: u64,
    stamp: LogStamp,
    from: i64,
    until: i64,
    found: Option<Stored>,
}

/// [`search`], or the fact a search found before while it still holds.
fn searched(
    r: &DurableReader,
    v: &View,
    series: &Series,
    p: &Params,
    now: i64,
) -> Result<Option<Stored>, QueryError> {
    let id = series.def.id;
    let facts = v.latest().fact_count(id);
    let stamp = v.state().stamp;
    if let Some(s) = r
        .searched(id)
        .filter(|s| s.facts == facts && s.stamp == stamp && s.from <= now && now < s.until)
    {
        return Ok(s.found);
    }
    let found = search(r, v, series, p, now)?;
    let versioned = series.def.versioning != Versioning::None;
    // Every current fact after the one found that is valid is ahead of `now` or not yet ready: the first of them to
    // become live ends what the search found.
    let after = found
        .as_ref()
        .map_or(i64::MIN, |s| s.row.t.saturating_add(1));
    let ahead = [Wanted {
        series_id: id,
        from: after,
        to: i64::MAX,
    }];
    let until = current(r.lookup(v, &ahead)?, versioned)
        .values()
        .filter(|s| windows::reading_of(series, s).is_some())
        .map(|s| p.ready_at(s.row.t))
        .filter(|t| *t > now)
        .min()
        .unwrap_or(i64::MAX);
    r.keep_searched(
        id,
        Searched {
            facts,
            stamp,
            from: now,
            until,
            found: found.clone(),
        },
    );
    Ok(found)
}

/// The facts at the positions `wanted` names, from the log at `v`'s cut, by series.
fn resolve(
    r: &DurableReader,
    v: &View,
    wanted: &[(u32, Kept)],
) -> Result<BTreeMap<u32, Stored>, QueryError> {
    let at: Vec<WantedAt> = wanted
        .iter()
        .map(|(id, k)| WantedAt {
            series_id: *id,
            t: k.t,
            position: k.position,
        })
        .collect();
    let mut out = BTreeMap::new();
    for f in r.lookup_at(v, &at)? {
        let id = f.row.series_id;
        if wanted
            .iter()
            .any(|(w, k)| *w == id && k.position == f.position)
        {
            out.insert(
                id,
                Stored {
                    position: f.position,
                    received_at: f.received_at,
                    row: f.row,
                },
            );
        }
    }
    Ok(out)
}

/// Live state (spec §2.3, §5.4) of series `id` at `now`, at `v`'s cut: from the state when it can tell, the full
/// fact read once at its position where the fold did not meet it (all such series at the first, Q-1004); from the
/// log otherwise, counted; from the fact index when the state is broken (Q-1006).
pub(crate) fn live(
    r: &DurableReader,
    v: &View,
    id: SeriesId,
    now: i64,
) -> Result<Live, QueryError> {
    let latest = v.latest();
    if latest.broken() {
        let facts = v.index().ok_or(QueryError::CutExpired)?;
        return windows::live_in(facts, r.clocks(), id, now).map_err(crate::backend::Error::query);
    }
    let series = latest
        .registry()
        .get(id.0)
        .ok_or(QueryError::UnknownSeries)?;
    let p = windows::params(&series.def);
    if p.timing == KmTiming::Forecast {
        return Ok(Live::None);
    }
    let versioned = series.def.versioning != Versioning::None;
    let fact: Option<Cow<'_, Stored>> = match latest.judge(id.0, versioned, &p, now) {
        Judged::None => None,
        Judged::Fact(s) if windows::has_reading(series, s) => Some(Cow::Borrowed(s)),
        Judged::Row(k) => match r.resolved(v, id.0, k, now)? {
            Some(s) if windows::has_reading(series, &s) => Some(Cow::Owned(s)),
            Some(_) => searched(r, v, series, &p, now)?.map(Cow::Owned),
            None if recorded_lost(v.lost_ranges(), k.position) => {
                searched(r, v, series, &p, now)?.map(Cow::Owned)
            }
            // A fact the directory names that the log no longer holds, as a segment removed before the lookup's
            // reader opened (Q-951): never read as absence (Q-1004).
            None => {
                return Err(QueryError::NotAvailable {
                    reason: "the log segment holding the series' live fact cannot be read".into(),
                });
            }
        },
        Judged::Fact(_) | Judged::Unsure => searched(r, v, series, &p, now)?.map(Cow::Owned),
    };
    Ok(match fact {
        Some(s) => windows::live_with(&p, r.clocks(), id, &s, now),
        None => Live::None,
    })
}

fn recorded_lost(recorded: &[LostRange], p: LogPos) -> bool {
    let p = (p.generation, p.sequence);
    recorded.iter().any(|r| r.first <= p && p <= r.last)
}

/// What the registry knows about a series at `v`'s cut, from the fact index when the state is broken (Q-1006).
pub(crate) fn series(v: &View, key: &SeriesKey) -> Result<SeriesInfo, QueryError> {
    if v.latest().broken() {
        let facts = v.index().ok_or(QueryError::CutExpired)?;
        return facts.series(key).map_err(crate::backend::Error::query);
    }
    v.latest().series(key)
}

/// The full facts of candidates the fold did not meet: `id`'s from those already read, else read with every other
/// such candidate of `v` the first time, and alone afterwards.
pub(crate) fn rows_for(
    r: &DurableReader,
    v: &View,
    id: u32,
    k: Kept,
    now: i64,
    first: bool,
) -> Result<BTreeMap<u32, Stored>, QueryError> {
    let wanted = if first {
        v.latest().unmet(now)
    } else {
        vec![(id, k)]
    };
    let mut wanted = wanted;
    if !wanted.iter().any(|(w, _)| *w == id) {
        wanted.push((id, k));
    }
    resolve(r, v, &wanted)
}

#[cfg(test)]
mod cases;
#[cfg(test)]
mod snapshots;
#[cfg(test)]
mod tests;
