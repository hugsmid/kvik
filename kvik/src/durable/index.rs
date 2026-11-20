//! What a process knows of the kept log: the registry, every fact by series, the commit time of every batch, and
//! the losses its reads found ([`Finder`]), built from what a log reader returns and cut back when it voids what it
//! took (`spec.md` §7 Readers). Each reader holds one, and its views share its structure (Q-900): a
//! clone copies no entry, and a change copies the nodes it touches. The writer's start and the deliverer hold one
//! without facts.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::entries::{Entry, MarkState as LogMarkState};
use kvik_log::facts::{FactRow, Quality as LogQuality, Value as LogValue};
use kvik_log::manifest::{LossCause as LogCause, LostRange};
use kvik_log::reader::{ReadBatch, ReadEvent};
use kvik_log::registry::RegistryFile;
use kvik_log::writer::Position as LogPos;

use super::by_id::ById;
use super::loss::{Find, Finder};
use super::read::pmap::PMap;
use super::registry::{Registry, Series};
use super::summary::Summary;
use crate::backend::Error;
use crate::health::{LossCause, LossReport, MarkState};
use crate::series::{SeriesInfo, SeriesKey, Timing};
use crate::time::Timestamp;
use crate::types::{Fact, FactFlags, Position, Quality, SeriesId, Value};

/// A kept fact with its position and its batch's receive time.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Stored {
    pub(crate) position: LogPos,
    pub(crate) received_at: i64,
    pub(crate) row: FactRow,
}

impl Stored {
    pub(crate) fn no_value(&self) -> bool {
        matches!(self.row.value, LogValue::Absent { .. })
    }
}

/// See the module documentation.
#[derive(Clone, Debug, Default)]
pub(crate) struct Index {
    /// Registry entries with the receive time of their batch.
    entries: PMap<LogPos, (i64, Entry)>,
    registry: Registry,
    facts: PMap<u32, PMap<(i64, LogPos), Stored>>,
    /// Each batch's last position and commit time.
    commits: PMap<LogPos, i64>,
    finder: Finder,
    /// Facts are not kept, only each series' [`Summary`] (roadmap 11.0: the writer's start and the deliverer need the
    /// registry and the commit times, not the facts).
    no_facts: bool,
    summary: ById<Summary>,
    /// A void or a loss removed facts the summaries counted: they are rebuilt by
    /// [`read_summary`](super::summary::read_summary) before they are used (Q-930).
    stale: bool,
    /// Told of every batch taken and every void, for the series directory (`format.md` A.18).
    fold: Option<std::sync::Arc<super::derived::SeriesFold>>,
}

pub(crate) fn pos(p: LogPos) -> Position {
    Position::new(p.generation, p.sequence)
}

pub(crate) fn log_pos(p: Position) -> LogPos {
    LogPos {
        generation: p.generation,
        sequence: p.sequence,
    }
}

pub(crate) fn cause_out(c: LogCause) -> LossCause {
    match c {
        LogCause::Corruption => LossCause::Corruption,
        LogCause::TruncatedTail => LossCause::TruncatedTail,
        LogCause::LostInBucket => LossCause::LostInBucket,
        LogCause::InvalidContent => LossCause::InvalidContent,
        LogCause::Unknown => LossCause::Unknown,
    }
}

pub(crate) fn mark_state_out(m: LogMarkState) -> MarkState {
    match m {
        LogMarkState::Readable => MarkState::Readable,
        LogMarkState::Missing => MarkState::Missing,
        LogMarkState::Unreadable => MarkState::Unreadable,
        LogMarkState::Stale => MarkState::Stale,
    }
}

pub(crate) fn quality_out(q: LogQuality) -> Quality {
    match q {
        LogQuality::Good => Quality::Good,
        LogQuality::Estimated => Quality::Estimated,
        LogQuality::Suspect => Quality::Suspect,
        LogQuality::Bad => Quality::Bad,
    }
}

pub(crate) fn quality_in(q: Quality) -> LogQuality {
    match q {
        Quality::Good => LogQuality::Good,
        Quality::Estimated => LogQuality::Estimated,
        Quality::Suspect => LogQuality::Suspect,
        Quality::Bad => LogQuality::Bad,
    }
}

/// [`fact_out`], its text moved rather than copied.
pub(crate) fn fact_into(mut s: Stored) -> Fact {
    let text = match &mut s.row.value {
        LogValue::Text(t) => Some(std::mem::take(t)),
        _ => None,
    };
    let mut f = fact_out(&s);
    if let (Some(t), Value::Text(x)) = (text, &mut f.value) {
        *x = t;
    }
    f
}

/// A fact as the interface shows it. The log keeps the adapter-supplied quality; the quality after stamping
/// follows from the flags: bad without a value, at best suspect when `future` (spec §2.2, §2.5).
pub(crate) fn fact_out(s: &Stored) -> Fact {
    let r = &s.row;
    let value = match &r.value {
        LogValue::F64(bits) => Value::F64(f64::from_bits(*bits)),
        LogValue::Decimal { units, scale } => Value::Decimal {
            units: *units,
            scale: *scale,
        },
        LogValue::State(i) => Value::State(*i),
        LogValue::Bool(b) => Value::Bool(*b),
        LogValue::Text(t) => Value::Text(t.clone()),
        LogValue::Absent { .. } => Value::Absent,
    };
    let no_value = s.no_value();
    let supplied = quality_out(r.quality);
    let quality = if no_value {
        Quality::Bad
    } else if r.flags.future {
        supplied.max(Quality::Suspect)
    } else {
        supplied
    };
    Fact {
        series: SeriesId(r.series_id),
        t: Timestamp(r.t),
        position: pos(s.position),
        received_at: Timestamp(s.received_at),
        value,
        quality,
        supplied_quality: supplied,
        flags: FactFlags {
            future: r.flags.future,
            late: r.flags.late,
            revise: r.flags.revise,
            host_time: r.flags.host_time,
            unsynced_clock: r.flags.unsynced_clock,
            no_value,
            rounded: r.flags.rounded,
        },
        changed_at: r.changed_at,
    }
}

/// A series of `registry` by id or name.
pub(crate) fn lookup<'a>(registry: &'a Registry, key: &SeriesKey) -> Result<&'a Series, Error> {
    match key {
        SeriesKey::Id(id) => registry.get(id.0),
        SeriesKey::Name(n) => registry.resolve(n),
    }
    .ok_or_else(|| {
        Error::UnknownSeries(match key {
            SeriesKey::Id(id) => id.to_string(),
            SeriesKey::Name(n) => n.clone(),
        })
    })
}

/// What the registry knows about a series, with its latest recorded `t` and lateness reference (spec §2.2), the
/// reference shown for an observed series only.
pub(crate) fn series_info(s: &Series, latest_t: Option<i64>, reference: Option<i64>) -> SeriesInfo {
    SeriesInfo {
        id: SeriesId(s.def.id),
        name: s.names.last().cloned().unwrap_or_default(),
        names: s.names.clone(),
        spec: s.spec.clone(),
        staleness: s
            .def
            .staleness
            .map(|n| Duration::from_nanos(u64::try_from(n).unwrap_or(0))),
        retired_at: s.retired_at.map(Timestamp),
        latest_t: latest_t.map(Timestamp),
        lateness_reference: reference
            .filter(|_| s.spec.timing == Timing::Observed)
            .map(Timestamp),
    }
}

/// The registry file's content for `entries` up to `through` (`format.md` A.16), with the id floor `floor` or the
/// highest id `registry` names, whichever is higher.
pub(crate) fn registry_file(
    entries: &PMap<LogPos, (i64, Entry)>,
    registry: &Registry,
    through: LogPos,
    floor: u32,
) -> RegistryFile {
    RegistryFile {
        generation: through.generation,
        through: through.sequence,
        max_series_id: floor.max(registry.highest()),
        entries: entries
            .range(Bound::Unbounded, Bound::Included(&through))
            .filter(|(p, _)| p.generation == through.generation)
            .map(|(p, (t, e))| (p.sequence, *t, e.clone()))
            .collect(),
    }
}

impl Index {
    /// An index that keeps no facts: [`Index::series_facts`] and the calls on facts see none.
    pub(crate) fn without_facts() -> Self {
        Index {
            no_facts: true,
            ..Index::default()
        }
    }

    /// Hands every batch taken from now on, and every void, to `fold`.
    pub(crate) fn set_fold(&mut self, fold: std::sync::Arc<super::derived::SeriesFold>) {
        self.fold = Some(fold);
    }

    /// Applies what a refresh up to the mark's position `durable` found, in order. Returns the losses it found.
    pub(crate) fn apply(&mut self, events: &[ReadEvent], durable: LogPos) -> Vec<Find> {
        let found = self.finder.apply(events, durable);
        for e in events {
            match e {
                ReadEvent::Batch(b) => self.batch(b),
                ReadEvent::Void { above, .. } => self.void(*above),
                ReadEvent::Corruption { .. } => {}
            }
        }
        found
    }

    /// Applies one batch a refresh found.
    pub(crate) fn batch(&mut self, b: &ReadBatch) {
        let mut sequence = b.first.sequence;
        for item in &b.content.items {
            let p = LogPos {
                generation: b.first.generation,
                sequence,
            };
            match item {
                Item::Fact(row) if self.no_facts => {
                    self.registry.note_fact(row.series_id);
                    Summary::note(&mut self.summary, p, row);
                }
                Item::Fact(row) => {
                    self.registry.note_fact(row.series_id);
                    self.facts
                        .get_or_insert_with(row.series_id, PMap::default)
                        .insert(
                            (row.t, p),
                            Stored {
                                position: p,
                                received_at: b.content.received_at,
                                row: row.clone(),
                            },
                        );
                }
                Item::Entry(e) if matches!(**e, Entry::Ignored { .. }) => {}
                Item::Entry(e) => {
                    let e: &Entry = e;
                    self.registry.apply(e);
                    self.entries.insert(p, (b.content.received_at, e.clone()));
                }
            }
            sequence += item.sequences();
        }
        self.commits.insert(b.last, b.commit_time);
        if let Some(f) = &self.fold {
            f.batch(b);
        }
    }

    /// Forgets everything above `above` and rebuilds the registry from what is left.
    fn void(&mut self, above: LogPos) {
        if let Some(f) = &self.fold {
            f.void(above);
        }
        let keep = |p: &LogPos| *p <= above;
        self.entries.retain(|p, _| keep(p));
        let taken = self.commits.len();
        self.commits.retain(|p, _| keep(p));
        self.stale |= self.no_facts && self.commits.len() != taken;
        if self
            .facts
            .values()
            .any(|f| f.iter().any(|((_, p), _)| !keep(p)))
        {
            self.facts = PMap::from_sorted(self.facts.iter().filter_map(|(id, f)| {
                let mut f = f.clone();
                f.retain(|(_, p), _| keep(p));
                (!f.is_empty()).then_some((*id, f))
            }));
        }
        self.summary.retain(|_, s| keep(&s.first));
        self.registry = Registry::default();
        for (_, e) in self.entries.values() {
            self.registry.apply(e);
        }
        for id in self
            .facts
            .iter()
            .map(|(id, _)| *id)
            .chain(self.summary.iter().map(|(id, _)| id))
        {
            self.registry.note_fact(id);
        }
    }

    /// Whether a void or a loss left the summaries of an index without facts to be rebuilt.
    pub(crate) fn summary_stale(&self) -> bool {
        self.stale
    }

    /// Replaces the summaries with those of a read of the kept log that reached at least as far as this index.
    pub(crate) fn set_summary(&mut self, s: ById<Summary>) {
        self.summary = s;
        self.stale = false;
    }

    /// The registry of the kept log.
    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The registry file's content for the kept log up to `through` (`format.md` A.16), with the id floor `floor`
    /// or the highest id the kept log names, whichever is higher.
    pub(crate) fn registry_file(&self, through: LogPos, floor: u32) -> RegistryFile {
        registry_file(&self.entries, &self.registry, through, floor)
    }

    /// The facts of a series in `(t, position)` order.
    pub(crate) fn series_facts(&self, id: u32) -> impl Iterator<Item = &Stored> {
        self.facts.get(&id).into_iter().flat_map(PMap::values)
    }

    /// The current facts of a series (the fact of highest rank per `t`, spec §2.2) with `t` from `from` to `to`
    /// (bounds inclusive or not as given), in `t` order, or in reverse with `rev`. The rank is the position, or
    /// `(changed_at, position)` in a versioned series.
    pub(crate) fn current_between(
        &self,
        id: u32,
        versioned: bool,
        from: Bound<i64>,
        to: Bound<i64>,
        rev: bool,
    ) -> impl Iterator<Item = &Stored> {
        const FIRST: LogPos = LogPos {
            generation: 0,
            sequence: 0,
        };
        const LAST: LogPos = LogPos {
            generation: u32::MAX,
            sequence: u64::MAX,
        };
        let lo = match from {
            Bound::Included(t) => Bound::Included((t, FIRST)),
            Bound::Excluded(t) => Bound::Excluded((t, LAST)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let hi = match to {
            Bound::Included(t) => Bound::Included((t, LAST)),
            Bound::Excluded(t) => Bound::Excluded((t, FIRST)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let mut it: Box<dyn Iterator<Item = &Stored>> = match self.facts.get(&id) {
            None => Box::new(std::iter::empty()),
            Some(m) if rev => Box::new(m.range(lo.as_ref(), hi.as_ref()).rev().map(|(_, s)| s)),
            Some(m) => Box::new(m.range(lo.as_ref(), hi.as_ref()).map(|(_, s)| s)),
        };
        let mut pending = it.next();
        let rank = move |s: &Stored| (if versioned { s.row.changed_at } else { None }, s.position);
        std::iter::from_fn(move || {
            let mut best = pending.take()?;
            for s in it.by_ref() {
                if s.row.t != best.row.t {
                    pending = Some(s);
                    break;
                }
                if rank(s) > rank(best) {
                    best = s;
                }
            }
            Some(best)
        })
    }

    /// Every series id that has a kept fact.
    pub(crate) fn fact_series(&self) -> impl Iterator<Item = u32> + '_ {
        self.facts.iter().map(|(id, _)| *id)
    }

    fn lookup(&self, key: &SeriesKey) -> Result<&Series, Error> {
        lookup(&self.registry, key)
    }

    /// The latest recorded `t` of a series (spec §2.2).
    pub(crate) fn latest_t(&self, id: u32) -> Option<i64> {
        if self.no_facts {
            return self.summary.get(id).map(|s| s.latest);
        }
        self.facts
            .get(&id)
            .and_then(|m| m.last().map(|((t, _), _)| *t))
    }

    /// The lateness reference of an observed series (spec §2.2): the greatest `t` of its facts flagged neither
    /// `future` nor `unsynced_clock`.
    pub(crate) fn lateness_reference(&self, id: u32) -> Option<i64> {
        if self.no_facts {
            return self.summary.get(id).and_then(|s| s.reference);
        }
        self.series_facts(id)
            .filter(|s| !s.row.flags.future && !s.row.flags.unsynced_clock)
            .map(|s| s.row.t)
            .max()
    }

    /// What the registry knows about a series, by id or name.
    pub(crate) fn series(&self, key: &SeriesKey) -> Result<SeriesInfo, Error> {
        let s = self.lookup(key)?;
        let id = s.def.id;
        Ok(series_info(
            s,
            self.latest_t(id),
            self.lateness_reference(id),
        ))
    }

    /// The last position whose batch's commit time is at or before `at` (spec §2.3 As-of time).
    pub(crate) fn position_at(&self, at: i64) -> LogPos {
        self.commits
            .iter()
            .rev()
            .find(|(_, t)| **t <= at)
            .map_or(LogPos::default(), |(p, _)| *p)
    }

    /// The facts of a series with `t` in `range`, in a view (spec §2.3), in `t` order (and position order in the
    /// history view): the oracle of the sources' reads (roadmap 9.0f).
    #[cfg(test)]
    pub(crate) fn facts(
        &self,
        id: SeriesId,
        range: crate::time::TimeRange,
        view: crate::read::View,
    ) -> Result<Vec<Fact>, Error> {
        let s = self.lookup(&SeriesKey::Id(id))?;
        let versioned = s.spec.versioned.is_some();
        let (a, b) = (range.from.0, range.to.0);
        let in_range = self
            .series_facts(id.0)
            .filter(move |f| (a..b).contains(&f.row.t));
        let limit = match view {
            crate::read::View::History => return Ok(in_range.map(fact_out).collect()),
            crate::read::View::Current => None,
            crate::read::View::AsOf(p) => Some(log_pos(p)),
            crate::read::View::AsOfTime(t) => Some(self.position_at(t.0)),
        };
        // The fact of highest rank per `t`: position, or `(changed_at, position)` in a versioned series.
        let mut current: BTreeMap<i64, &Stored> = BTreeMap::new();
        for f in in_range.filter(|f| limit.is_none_or(|l| f.position <= l)) {
            let rank = |s: &Stored| (if versioned { s.row.changed_at } else { None }, s.position);
            let e = current.entry(f.row.t).or_insert(f);
            if rank(f) > rank(e) {
                *e = f;
            }
        }
        Ok(current.into_values().map(fact_out).collect())
    }

    /// Facts whose series has no definition in the kept log (spec §2.1 orphaned facts), counted per id.
    pub(crate) fn orphaned(&self) -> BTreeMap<u32, u64> {
        self.facts
            .iter()
            .filter(|(id, _)| self.registry.get(**id).is_none())
            .map(|(id, f)| (*id, f.len() as u64))
            .collect()
    }

    /// Losses: the manifest's lost-range records, the recovery entries of the kept log and the lost ranges this
    /// index's reads found that no record holds yet.
    pub(crate) fn losses(&self, recorded: &[LostRange]) -> Vec<LossReport> {
        self.finder.losses(recorded)
    }

    /// Forgets the facts at positions from `first` to `last`, a range the process found lost after this index took
    /// it (Q-938); without facts, the summaries are left to be rebuilt (Q-930).
    pub(crate) fn forget(&mut self, first: LogPos, last: LogPos) {
        if self.no_facts {
            self.stale = true;
            return;
        }
        if self
            .facts
            .values()
            .any(|f| f.iter().any(|((_, p), _)| first <= *p && *p <= last))
        {
            self.facts = PMap::from_sorted(self.facts.iter().filter_map(|(id, f)| {
                let mut f = f.clone();
                f.retain(|(_, p), _| *p < first || last < *p);
                (!f.is_empty()).then_some((*id, f))
            }));
        }
    }
}

#[cfg(test)]
mod tests;
