//! What a process knows of the kept log: the registry, every fact by series, the commit time of every batch and
//! the recovery entries, built from the batches a log reader returns and cut back when it voids what it took
//! (`spec.md` §7 Readers). The writer builds one at its start; each reader holds one.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::entries::{Entry, MarkState as LogMarkState, Recovery};
use kvik_log::facts::{FactRow, Quality as LogQuality, Value as LogValue};
use kvik_log::manifest::{LossCause as LogCause, LostRange};
use kvik_log::reader::{ReadBatch, ReadEvent};
use kvik_log::registry::RegistryFile;
use kvik_log::writer::Position as LogPos;

use super::by_id::ById;
use super::registry::{Registry, Series};
use crate::backend::Error;
use crate::health::{LossCause, LossKind, LossReport, MarkState};
use crate::read::View;
use crate::series::{SeriesInfo, SeriesKey, Timing};
use crate::time::{TimeRange, Timestamp};
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
    entries: BTreeMap<LogPos, (i64, Entry)>,
    registry: Registry,
    facts: BTreeMap<u32, BTreeMap<(i64, LogPos), Stored>>,
    /// Each batch's last position and commit time, in log order.
    commits: Vec<(LogPos, i64)>,
    recoveries: BTreeMap<LogPos, Recovery>,
    /// Facts are not kept, only each series' [`Summary`] (roadmap 11.0: the writer's start and the deliverer need the
    /// registry and the commit times, not the facts).
    no_facts: bool,
    summary: ById<Summary>,
    /// A void removed facts the summaries counted: they are rebuilt by [`read_summary`] before they are used.
    stale: bool,
}

/// What an index without facts keeps of a series' facts: the first position, the latest `t` and the lateness
/// reference (spec §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    first: LogPos,
    latest: i64,
    reference: Option<i64>,
}

impl Summary {
    fn note(s: &mut ById<Summary>, p: LogPos, row: &FactRow) {
        let x = s.get_or_insert_with(row.series_id, || Summary {
            first: p,
            latest: row.t,
            reference: None,
        });
        x.latest = x.latest.max(row.t);
        if !row.flags.future && !row.flags.unsynced_clock {
            x.reference = x.reference.max(Some(row.t));
        }
    }
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

impl Index {
    /// An index that keeps no facts: [`Index::series_facts`] and the calls on facts see none.
    pub(crate) fn without_facts() -> Self {
        Index {
            no_facts: true,
            ..Index::default()
        }
    }

    /// Applies what a refresh found, in order.
    pub(crate) fn apply(&mut self, events: &[ReadEvent]) {
        for e in events {
            match e {
                ReadEvent::Batch(b) => self.batch(b),
                ReadEvent::Void { above, .. } => self.void(*above),
                // A region below the mark is a loss for the writer's next start to record in the manifest.
                ReadEvent::Corruption { .. } => {}
            }
        }
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
                    self.facts.entry(row.series_id).or_default().insert(
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
                    if let Entry::Recovery(r) = e {
                        self.recoveries.insert(p, r.clone());
                    }
                    self.entries.insert(p, (b.content.received_at, e.clone()));
                }
            }
            sequence += item.sequences();
        }
        self.commits.push((b.last, b.commit_time));
    }

    /// Forgets everything above `above` and rebuilds the registry from what is left.
    fn void(&mut self, above: LogPos) {
        let keep = |p: &LogPos| *p <= above;
        self.entries.retain(|p, _| keep(p));
        self.recoveries.retain(|p, _| keep(p));
        let taken = self.commits.len();
        self.commits.retain(|(p, _)| keep(p));
        self.stale |= self.no_facts && self.commits.len() != taken;
        for f in self.facts.values_mut() {
            f.retain(|(_, p), _| keep(p));
        }
        self.facts.retain(|_, f| !f.is_empty());
        self.summary.retain(|_, s| keep(&s.first));
        self.registry = Registry::default();
        for (_, e) in self.entries.values() {
            self.registry.apply(e);
        }
        for id in self
            .facts
            .keys()
            .copied()
            .chain(self.summary.iter().map(|(id, _)| id))
        {
            self.registry.note_fact(id);
        }
    }

    /// Whether a void left the summaries of an index without facts to be rebuilt ([`read_summary`]).
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
        RegistryFile {
            generation: through.generation,
            through: through.sequence,
            max_series_id: floor.max(self.registry.highest()),
            entries: self
                .entries
                .range(..=through)
                .filter(|(p, _)| p.generation == through.generation)
                .map(|(p, (t, e))| (p.sequence, *t, e.clone()))
                .collect(),
        }
    }

    /// The facts of a series in `(t, position)` order.
    pub(crate) fn series_facts(&self, id: u32) -> impl Iterator<Item = &Stored> {
        self.facts.get(&id).into_iter().flat_map(|m| m.values())
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
        // An empty or inverted range reads nothing (`BTreeMap::range` panics on one).
        let empty = match (&lo, &hi) {
            (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
                a > b || (a == b && !matches!((&lo, &hi), (Bound::Included(_), Bound::Included(_))))
            }
            _ => false,
        };
        let facts = self.facts.get(&id).filter(|_| !empty);
        let mut it: Box<dyn Iterator<Item = &Stored>> = match facts {
            None => Box::new(std::iter::empty()),
            Some(m) if rev => Box::new(m.range((lo, hi)).rev().map(|(_, s)| s)),
            Some(m) => Box::new(m.range((lo, hi)).map(|(_, s)| s)),
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
        self.facts.keys().copied()
    }

    fn lookup(&self, key: &SeriesKey) -> Result<&Series, Error> {
        match key {
            SeriesKey::Id(id) => self.registry.get(id.0),
            SeriesKey::Name(n) => self.registry.resolve(n),
        }
        .ok_or_else(|| {
            Error::UnknownSeries(match key {
                SeriesKey::Id(id) => id.to_string(),
                SeriesKey::Name(n) => n.clone(),
            })
        })
    }

    /// The latest recorded `t` of a series (spec §2.2).
    pub(crate) fn latest_t(&self, id: u32) -> Option<i64> {
        if self.no_facts {
            return self.summary.get(id).map(|s| s.latest);
        }
        self.facts
            .get(&id)
            .and_then(|m| m.keys().map(|(t, _)| *t).max())
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
        Ok(SeriesInfo {
            id: SeriesId(id),
            name: s.names.last().cloned().unwrap_or_default(),
            names: s.names.clone(),
            spec: s.spec.clone(),
            staleness: s
                .def
                .staleness
                .map(|n| Duration::from_nanos(u64::try_from(n).unwrap_or(0))),
            retired_at: s.retired_at.map(Timestamp),
            latest_t: self.latest_t(id).map(Timestamp),
            lateness_reference: (s.spec.timing == Timing::Observed)
                .then(|| self.lateness_reference(id).map(Timestamp))
                .flatten(),
        })
    }

    /// The last position whose batch's commit time is at or before `at` (spec §2.3 As-of time).
    pub(crate) fn position_at(&self, at: i64) -> LogPos {
        self.commits
            .iter()
            .rev()
            .find(|(_, t)| *t <= at)
            .map_or(LogPos::default(), |(p, _)| *p)
    }

    /// The facts of a series with `t` in `range`, in a view (spec §2.3), in `t` order (and position order in the
    /// history view).
    pub(crate) fn facts(
        &self,
        id: SeriesId,
        range: TimeRange,
        view: View,
    ) -> Result<Vec<Fact>, Error> {
        let s = self.lookup(&SeriesKey::Id(id))?;
        let versioned = s.spec.versioned.is_some();
        let (a, b) = (range.from.0, range.to.0);
        let in_range = self
            .series_facts(id.0)
            .filter(move |f| (a..b).contains(&f.row.t));
        let limit = match view {
            View::History => return Ok(in_range.map(fact_out).collect()),
            View::Current => None,
            View::AsOf(p) => Some(log_pos(p)),
            View::AsOfTime(t) => Some(self.position_at(t.0)),
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

    /// Losses: the manifest's lost-range records and the recovery entries of the kept log (Q-116 (c)).
    pub(crate) fn losses(&self, recorded: &[LostRange]) -> Vec<LossReport> {
        let mut out: BTreeMap<(Position, bool), LossReport> = BTreeMap::new();
        for l in recorded {
            let identity = Position::new(l.identity.0, l.identity.1);
            let first = Position::new(l.first.0, l.first.1);
            let last = Position::new(l.last.0, l.last.1);
            let e = out.entry((identity, false)).or_insert_with(|| LossReport {
                identity,
                kind: LossKind::DataLost,
                cause: match l.cause {
                    LogCause::Corruption => LossCause::Corruption,
                    LogCause::TruncatedTail => LossCause::TruncatedTail,
                    LogCause::LostInBucket => LossCause::LostInBucket,
                    LogCause::InvalidContent => LossCause::InvalidContent,
                    LogCause::Unknown => LossCause::Unknown,
                },
                first: Some(first),
                last: Some(last),
                file_id: Some(l.file_id),
                bytes: l.length,
                readable_commit_blocks: 0,
                mark_state: MarkState::Readable,
                boot_id: (l.boot_id != [0; 16]).then_some(l.boot_id),
                commit_time_before: l.time_before.map(Timestamp),
                commit_time_after: l.time_after.map(Timestamp),
                cursors_missing: Vec::new(),
                recorded: true,
                acknowledged: false,
            });
            // A record that extends a loss widens its range; the loss is counted once.
            e.first = e.first.min(Some(first));
            e.last = e.last.max(Some(last));
        }
        // A recovery entry is a possible loss when it names a non-empty tail, a mark not readable or stale, or a
        // missing or unreadable cursor file; its identity is its own position (`spec.md` §11 Health, §3 step 4).
        for (p, r) in &self.recoveries {
            if r.tail_bytes == 0
                && r.readable_commits == 0
                && r.mark_state == LogMarkState::Readable
                && r.consumers.is_empty()
            {
                continue;
            }
            let identity = Position::new(p.generation, p.sequence);
            out.insert(
                (identity, true),
                LossReport {
                    identity,
                    kind: LossKind::PossibleLoss,
                    cause: LossCause::TruncatedTail,
                    first: None,
                    last: None,
                    file_id: None,
                    bytes: r.tail_bytes,
                    readable_commit_blocks: r.readable_commits,
                    mark_state: match r.mark_state {
                        LogMarkState::Readable => MarkState::Readable,
                        LogMarkState::Missing => MarkState::Missing,
                        LogMarkState::Unreadable => MarkState::Unreadable,
                        LogMarkState::Stale => MarkState::Stale,
                    },
                    boot_id: (r.boot_id != [0; 16]).then_some(r.boot_id),
                    commit_time_before: None,
                    commit_time_after: None,
                    cursors_missing: r.consumers.clone(),
                    recorded: true,
                    acknowledged: false,
                },
            );
        }
        out.into_values().collect()
    }
}

/// The second pass of [`read_summary`]: each fact counted unless a later void of the stream removes it, which the
/// first pass found.
struct SummaryPass {
    /// Per void, in stream order, the least `above` of it and every later one.
    bounds: Vec<LogPos>,
    seen: usize,
    summary: ById<Summary>,
}

impl SummaryPass {
    fn new(mut voids: Vec<LogPos>) -> Self {
        for k in (0..voids.len().saturating_sub(1)).rev() {
            voids[k] = voids[k].min(voids[k + 1]);
        }
        SummaryPass {
            bounds: voids,
            seen: 0,
            summary: ById::default(),
        }
    }

    fn apply(&mut self, events: &[ReadEvent]) {
        for e in events {
            match e {
                ReadEvent::Batch(b) => {
                    let bound = self.bounds.get(self.seen).copied();
                    let mut sequence = b.first.sequence;
                    for item in &b.content.items {
                        let p = LogPos {
                            generation: b.first.generation,
                            sequence,
                        };
                        if let Item::Fact(row) = item
                            && bound.is_none_or(|a| p <= a)
                        {
                            Summary::note(&mut self.summary, p, row);
                        }
                        sequence += item.sequences();
                    }
                }
                ReadEvent::Void { .. } => self.seen += 1,
                ReadEvent::Corruption { .. } => {}
            }
        }
    }
}

fn voids_of(events: &[ReadEvent], out: &mut Vec<LogPos>) {
    out.extend(events.iter().filter_map(|e| match e {
        ReadEvent::Void { above, .. } => Some(*above),
        _ => None,
    }));
}

/// The summaries of the kept log, read in parts of `part` items with two fresh readers: the first pass finds where
/// voids fall, the second counts each fact no later void removes, so that memory holds one part at a time.
pub(crate) fn read_summary(
    open: impl Fn() -> Result<kvik_log::reader::LogReader, kvik_log::reader::ReaderError>,
    part: usize,
) -> Result<ById<Summary>, kvik_log::reader::ReaderError> {
    let mut voids = Vec::new();
    let mut r = open()?;
    loop {
        let (u, more) = r.refresh_some(part)?;
        voids_of(&u.events, &mut voids);
        if !more {
            break;
        }
    }
    let _ = r.close();
    let mut pass = SummaryPass::new(voids);
    let mut r = open()?;
    loop {
        let (u, more) = r.refresh_some(part)?;
        pass.apply(&u.events);
        if !more {
            break;
        }
    }
    let _ = r.close();
    Ok(pass.summary)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kvik_log::content::BatchContent;
    use kvik_log::facts::FactFlags as LogFlags;
    use kvik_log::reader::VoidCause;

    use super::*;
    use crate::series::{Kind, SeriesSpec, ValueType};

    fn batch(first: u64, items: Vec<Item>) -> ReadEvent {
        let n: u64 = items.iter().map(Item::sequences).sum();
        let p = |sequence| LogPos {
            generation: 0,
            sequence,
        };
        ReadEvent::Batch(ReadBatch {
            file_id: 1,
            offset: 0,
            first: p(first),
            last: p(first + n - 1),
            commit_time: first as i64,
            content: BatchContent {
                first_sequence: first,
                received_at: first as i64,
                items,
            },
        })
    }

    fn fact(t: i64, v: f64) -> Item {
        Item::Fact(FactRow {
            series_id: 1,
            t,
            quality: LogQuality::Good,
            flags: LogFlags::default(),
            value: LogValue::F64(v.to_bits()),
            changed_at: None,
        })
    }

    #[test]
    fn a_void_cuts_facts_and_entries_back_and_rebuilds_the_registry() {
        let spec = SeriesSpec::new(
            "a/b/c",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        let def =
            super::super::registry::definition_of(&spec, 1, Duration::from_secs(300)).unwrap();
        let mut i = Index::default();
        i.apply(&[
            batch(1, vec![Item::from(Entry::Definition(Box::new(def)))]),
            batch(2, vec![fact(10, 1.0), fact(20, 2.0)]),
            batch(
                4,
                vec![Item::from(Entry::Alias {
                    id: 1,
                    name: "a/b/d".into(),
                })],
            ),
        ]);
        assert!(i.registry().resolve("a/b/d").is_some());
        assert_eq!(i.series_facts(1).count(), 2);
        // A start truncated everything above position 2: the second fact and the alias are void.
        i.apply(&[ReadEvent::Void {
            above: LogPos {
                generation: 0,
                sequence: 2,
            },
            cause: VoidCause::Rewritten,
        }]);
        assert!(i.registry().resolve("a/b/d").is_none());
        assert!(i.registry().resolve("a/b/c").is_some());
        let left: Vec<_> = i.series_facts(1).map(|s| s.row.t).collect();
        assert_eq!(left, vec![10]);
        assert_eq!(i.latest_t(1), Some(10));
        // Positions above the void are taken again.
        i.apply(&[batch(3, vec![fact(30, 3.0)])]);
        assert_eq!(i.latest_t(1), Some(30));
        // Everything void.
        i.apply(&[ReadEvent::Void {
            above: LogPos::default(),
            cause: VoidCause::Rewritten,
        }]);
        assert!(i.registry().get(1).is_none());
        assert_eq!(i.series_facts(1).count(), 0);
    }

    fn fact_of(series_id: u32, t: i64, future: bool) -> Item {
        Item::Fact(FactRow {
            series_id,
            t,
            quality: LogQuality::Good,
            flags: LogFlags {
                future,
                ..LogFlags::default()
            },
            value: LogValue::F64(1),
            changed_at: None,
        })
    }

    #[test]
    fn summaries_held_and_read_in_two_passes_equal_those_of_every_fact() {
        let void = |above| ReadEvent::Void {
            above: LogPos {
                generation: 0,
                sequence: above,
            },
            cause: VoidCause::Rewritten,
        };
        // Facts of series 1 and 2; a void above 3 removes the later ones, which are taken again lower; then a second
        // void above 5 and more facts.
        let stream = vec![
            batch(1, vec![fact_of(1, 10, false), fact_of(2, 50, true)]),
            batch(3, vec![fact_of(1, 30, false), fact_of(2, 40, false)]),
            batch(5, vec![fact_of(1, 99, false)]),
            void(3),
            batch(4, vec![fact_of(1, 20, false), fact_of(2, 45, false)]),
            batch(6, vec![fact_of(2, 70, true)]),
            void(5),
            batch(6, vec![fact_of(3, 5, false)]),
        ];
        let mut full = Index::default();
        full.apply(&stream);
        let mut held = Index::without_facts();
        let mut voids = Vec::new();
        for (k, e) in stream.iter().enumerate() {
            held.apply(std::slice::from_ref(e));
            voids_of(std::slice::from_ref(e), &mut voids);
            assert_eq!(held.summary_stale(), k >= 3);
        }
        let mut pass = SummaryPass::new(voids);
        for e in &stream {
            pass.apply(std::slice::from_ref(e));
        }
        held.set_summary(pass.summary);
        assert!(!held.summary_stale());
        for id in 1..=4 {
            assert_eq!(held.latest_t(id), full.latest_t(id), "series {id}");
            assert_eq!(
                held.lateness_reference(id),
                full.lateness_reference(id),
                "series {id}"
            );
        }
        assert_eq!(
            (held.latest_t(2), held.lateness_reference(2)),
            (Some(50), Some(45))
        );
        assert_eq!(held.lateness_reference(1), Some(30));
        // Without a void the summaries are kept exactly as facts are taken.
        let mut plain = Index::without_facts();
        plain.apply(&stream[..3]);
        assert!(!plain.summary_stale());
        assert_eq!(
            (plain.latest_t(1), plain.lateness_reference(2)),
            (Some(99), Some(40))
        );
    }
}
