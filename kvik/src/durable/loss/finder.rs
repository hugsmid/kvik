//! Finding losses in what a log reader returns (spec §3 Recovery rule 2, Q-200), and the files' loss reports: the
//! manifest's lost ranges and the recovery entries of the kept log (Q-116 (c)).

use std::collections::{BTreeMap, VecDeque};

use kvik_log::content::Item;
use kvik_log::entries::{Entry, MarkState as LogMarkState, Recovery};
use kvik_log::manifest::LostRange;
use kvik_log::reader::{ReadBatch, ReadEvent};
use kvik_log::scan::{InvalidRegion, LossCause as ScanCause};
use kvik_log::writer::Position as LogPos;

use super::{Regions, fold_recorded, identities, merge, widen};
use crate::durable::index::{cause_out, log_pos, mark_state_out};
use crate::health::{LossCause, LossKind, LossReport, MarkState};
use crate::time::Timestamp;
use crate::types::Position;

/// Commits kept for the region after a void: a rewrite voids at most the 64 batches kvik-log's reader remembers, so
/// the commit before them is still here; a recovery entry's void is followed by its own batch.
const KEPT_COMMITS: usize = 65;

/// A loss found by a read, with what the latch keeps beside the report.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Find {
    pub(crate) report: LossReport,
    /// The region's segment and byte span.
    pub(crate) file_id: u64,
    pub(crate) span: (u64, u64),
    /// The positions the region held: an open-ended range closed at the next batch the read returned, or at the
    /// mark it read (Q-938). Empty (first above last) when the next batch follows the region's first position.
    pub(crate) closed: (LogPos, LogPos),
}

/// See the module documentation.
#[derive(Clone, Debug, Default)]
pub(crate) struct Finder {
    /// The last batches' last positions and commit times, in log order.
    commits: VecDeque<(LogPos, i64)>,
    /// The last valid position after a void that went past the commits kept.
    voided_to: Option<LogPos>,
    recoveries: BTreeMap<LogPos, Recovery>,
    /// Lost ranges found by this finder's reads, latched.
    found: Vec<LossReport>,
    /// The regions met, so that a region inside a held report still reaches the latch once.
    regions: Regions,
}

impl Finder {
    /// Takes what a refresh up to `durable` returned, in order. Returns the losses it found.
    pub(crate) fn apply(&mut self, events: &[ReadEvent], durable: LogPos) -> Vec<Find> {
        let mut found: Vec<(Find, bool)> = Vec::new();
        for e in events {
            match e {
                ReadEvent::Batch(b) => {
                    for (f, open) in &mut found {
                        if std::mem::take(open) {
                            f.closed.1 = f.closed.1.min(LogPos {
                                generation: b.first.generation,
                                sequence: b.first.sequence.saturating_sub(1),
                            });
                        }
                    }
                    self.batch(b);
                }
                ReadEvent::Void { above, .. } => self.void(*above),
                ReadEvent::Corruption { file_id, region } => {
                    found.extend(self.corruption(*file_id, region, durable));
                }
            }
        }
        found.into_iter().map(|(f, _)| f).collect()
    }

    /// The lost ranges this finder's reads found, with the commit times learnt since each was found.
    pub(crate) fn found(&self) -> &[LossReport] {
        &self.found
    }

    /// Takes the recovery entries a registry file holds, with their positions (spec §2.1: it keeps every one), for a
    /// reader that reads the log only above it.
    pub(crate) fn seed_recoveries(
        &mut self,
        entries: impl IntoIterator<Item = (LogPos, Recovery)>,
    ) {
        self.recoveries.extend(entries);
    }

    /// For a reader that reads the log only above `through`: the last valid position before what it reads, with no
    /// commit time known.
    pub(crate) fn seed_floor(&mut self, through: LogPos) {
        self.voided_to = Some(through);
    }

    /// The last batch read before now: its last position and, when known, its commit time.
    fn before(&self) -> Option<(LogPos, Option<i64>)> {
        match self.voided_to {
            Some(p) => Some((p, None)),
            None => self.commits.back().map(|&(p, t)| (p, Some(t))),
        }
    }

    /// A region below the mark: the positions from the last batch before it to the next batch after it or, at the
    /// tail, to the mark's last position are lost (spec §3 Recovery rule 2), with the identity the next start
    /// records them under. The flag says whether the range is open-ended.
    fn corruption(
        &mut self,
        file_id: u64,
        region: &InvalidRegion,
        durable: LogPos,
    ) -> Option<(Find, bool)> {
        let before = self.before();
        let first = region
            .last_sequence_before
            .or(before.map(|(p, _)| p.sequence))
            .map_or(1, |s| s.saturating_add(1));
        let last = region
            .first_sequence_after
            .map_or(durable.sequence, |s| s.saturating_sub(1));
        if last < first {
            return None;
        }
        let g = durable.generation;
        let report = LossReport {
            identity: Position::new(g, first),
            kind: LossKind::DataLost,
            cause: match region.cause {
                ScanCause::Corruption => LossCause::Corruption,
                ScanCause::InvalidContent => LossCause::InvalidContent,
            },
            first: Some(Position::new(g, first)),
            last: Some(Position::new(g, last)),
            file_id: Some(file_id),
            bytes: region.end.saturating_sub(region.start),
            readable_commit_blocks: 0,
            mark_state: MarkState::Readable,
            boot_id: None,
            commit_time_before: before.and_then(|(_, t)| t).map(Timestamp),
            commit_time_after: None,
            cursors_missing: Vec::new(),
            recorded: false,
            acknowledged: false,
            absorbed: Vec::new(),
        };
        let at = |sequence| LogPos {
            generation: g,
            sequence,
        };
        let find = |report| Find {
            report,
            file_id,
            span: (region.start, region.end),
            closed: (at(first), at(last)),
        };
        let open = region.first_sequence_after.is_none();
        let met = !self.regions.covers(file_id, region.start, region.end);
        self.regions.insert(file_id, region.start, region.end);
        // A region that overlaps a loss already found is that loss, grown, under its first identity (spec §3 Order
        // of a start step 4); one inside it, met for the first time, is handed on with that loss unchanged.
        let overlaps = |f: &&mut LossReport| f.first <= report.last && report.first <= f.last;
        if let Some(f) = self.found.iter_mut().find(overlaps) {
            if !widen(f, &report) && !met {
                return None;
            }
            return Some((find(f.clone()), open));
        }
        self.found.push(report.clone());
        Some((find(report), open))
    }

    fn batch(&mut self, b: &ReadBatch) {
        if let Some(l) = self.found.last_mut()
            && l.commit_time_after.is_none()
            && l.last
                .is_some_and(|p| log_pos(p).sequence.checked_add(1) == Some(b.first.sequence))
        {
            l.commit_time_after = Some(Timestamp(b.commit_time));
        }
        let mut sequence = b.first.sequence;
        for item in &b.content.items {
            if let Item::Entry(e) = item
                && let Entry::Recovery(r) = &**e
            {
                let p = LogPos {
                    generation: b.first.generation,
                    sequence,
                };
                self.recoveries.insert(p, r.clone());
            }
            sequence += item.sequences();
        }
        self.voided_to = None;
        self.commits.push_back((b.last, b.commit_time));
        if self.commits.len() > KEPT_COMMITS {
            self.commits.pop_front();
        }
    }

    /// Forgets everything above `above`; found losses outlive it, as lost positions are never reused (spec §3
    /// Recovery rule 2).
    fn void(&mut self, above: LogPos) {
        self.recoveries.retain(|p, _| *p <= above);
        self.commits.retain(|(p, _)| *p <= above);
        self.voided_to = (above != LogPos::default()
            && self.commits.back().is_none_or(|(p, _)| *p < above))
        .then_some(above);
    }

    /// Losses: the manifest's lost-range records `recorded`, the recovery entries read (Q-116 (c)) and the lost
    /// ranges found that no record holds yet.
    pub(crate) fn losses(&self, recorded: &[LostRange]) -> Vec<LossReport> {
        let mut out: BTreeMap<(Position, bool), LossReport> = BTreeMap::new();
        for l in recorded {
            let identity = Position::new(l.identity.0, l.identity.1);
            let first = Position::new(l.first.0, l.first.1);
            let last = Position::new(l.last.0, l.last.1);
            let e = out.entry((identity, false)).or_insert_with(|| LossReport {
                identity,
                kind: LossKind::DataLost,
                cause: cause_out(l.cause),
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
                absorbed: Vec::new(),
            });
            // A record that extends a loss widens its range, counted once, with the times of the record that sets
            // each end.
            if Some(first) < e.first {
                e.first = Some(first);
                e.commit_time_before = l.time_before.map(Timestamp);
            }
            if Some(last) > e.last {
                e.last = Some(last);
                e.commit_time_after = l.time_after.map(Timestamp);
            }
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
                    mark_state: mark_state_out(r.mark_state),
                    boot_id: (r.boot_id != [0; 16]).then_some(r.boot_id),
                    commit_time_before: None,
                    commit_time_after: None,
                    cursors_missing: r.consumers.clone(),
                    recorded: true,
                    acknowledged: false,
                    absorbed: Vec::new(),
                },
            );
        }
        let mut out = fold_recorded(out.into_values().collect());
        merge(&mut out, self.found.iter().cloned(), &identities(recorded));
        out
    }
}
