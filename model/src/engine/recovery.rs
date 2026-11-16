//! The start's recovery and the store's resets (§3 Recovery rules 1–3 and
//! Order of a start, §4 Resync reset points, §7 Readers saved points, §8
//! Acknowledging a loss and Self-repair, §11 Health losses), and the fault injections that
//! make them happen: a remount, a power loss that loses acknowledged batches,
//! a lost mark or cursor file.

use std::collections::BTreeMap;

use super::{
    Acknowledged, ConsumerId, Entry, EntryBody, Error, FileClass, FileId, Loss, LossCause,
    ManifestOp, ManifestRecord, Mark, MarkState, MarkSync, Model, PointCheck, QuarantineCause,
    RecoveryEntry, Reset, ResetPoint, ResetStamp, SavedPoint, StoredFile, Tail, WriteOutcome,
};
use crate::recovery::{LossKind, LossState, Outcome};
use crate::registry::RegistryEvent;
use crate::types::Position;

impl Model {
    /// The store's file system is unmounted and mounted again within the
    /// boot: the process stops and the page cache is lost, so the mark falls
    /// back to the last synced mark (§3 Recovery rule 1, row 7c's same boot).
    pub fn remount(&mut self) {
        self.drop_page_cache();
        self.crash();
    }

    /// A power loss after which the last `n` complete batches of the open
    /// segment are found invalid, at most those holding only facts beyond the
    /// last synced mark (all of them with the mark lost): the accepted loss
    /// of invariant 10, which can hold acknowledged and delivered batches.
    /// Returns how many were lost.
    pub fn power_loss_losing(&mut self, n: usize) -> usize {
        self.boot += 1;
        self.drop_page_cache();
        self.crash();
        let Some(active) = self.disk.active else {
            return 0;
        };
        let floor = match self.disk.synced_mark {
            Some(m) if m.file == active => m.batches,
            _ => 0,
        };
        let mut lost = 0;
        while lost < n {
            let file = self.disk.files.get_mut(&active).expect("segment");
            let len = file.batches.len();
            let eligible = len > floor
                && !self.disk.damaged.contains_key(&(active, len - 1))
                && file.batches[len - 1]
                    .iter()
                    .all(|e| matches!(e.body, EntryBody::Fact(_)))
                && !file.batches[len - 1].is_empty();
            if !eligible {
                break;
            }
            file.batches.pop();
            lost += 1;
        }
        self.disk.invalid_tail.batches += u32::try_from(lost).unwrap_or(u32::MAX);
        lost
    }

    /// What the page cache held is gone: the batch written and not synced is
    /// kept, torn or lost as its write outcome says, and the mark falls back
    /// to the last synced mark.
    pub(super) fn drop_page_cache(&mut self) {
        // Only the last batch of the open segment can have been written and
        // not synced: a batch is written only after the previous one's sync.
        if let (Some(active), Some(fate)) = (self.disk.active, self.disk.tail_fate)
            && fate != WriteOutcome::Survived
        {
            self.disk
                .files
                .get_mut(&active)
                .expect("segment")
                .batches
                .pop();
            self.disk.invalid_tail.torn |= fate == WriteOutcome::Torn;
        }
        self.disk.tail_fate = None;
        self.disk.mark = self.disk.synced_mark;
    }

    /// Fault injection while the engine is down: the mark file is missing or
    /// holds no valid slot (§3 Recovery rule 3, row 12a).
    pub fn lose_mark(&mut self, state: MarkState) -> Result<(), Error> {
        if self.vol.is_some() {
            return Err(Error::BadFault("stop the engine first"));
        }
        self.disk.mark = None;
        self.disk.synced_mark = None;
        self.disk.mark_damage = Some(state);
        Ok(())
    }

    /// Fault injection while the engine is down: a consumer's cursor file is
    /// missing or unreadable. The next recovery entry names it, and the
    /// consumer is treated as having no cursor (§3 Recovery rule 3).
    pub fn lose_cursor_file(&mut self, c: ConsumerId) -> Result<(), Error> {
        if self.vol.is_some() {
            return Err(Error::BadFault("stop the engine first"));
        }
        self.disk.cursors.remove(&c);
        self.disk.latches.retain(|(id, _), _| *id != c);
        self.disk.lost_cursors.insert(c);
        Ok(())
    }

    /// The start's recovery (§3 Recovery rules 1–3, Order of a start steps
    /// 2–6). Rule 1 truncates after the mark; with exactly one complete batch
    /// beyond it the truncation is a reset point. Rule 3 (another boot, the
    /// mark missing, unreadable or stale) keeps every complete batch and
    /// truncates the invalid tail. Either reset writes a recovery entry at F
    /// with a new reset stamp, records its gap and moves the mark to it.
    /// Returns whether the engine must stay read-only because the start's
    /// sync failed, in which case nothing is truncated or written.
    pub(super) fn recover_tail(&mut self, sync_fails: bool) -> bool {
        let generation = self.disk.generation;
        let mark = self.disk.mark;
        // The first start of a store, or of a restored generation: no mark
        // and nothing of the current generation in the log, not even a torn
        // tail, which rule 3's entry reports (Q-320).
        let in_use = self
            .disk
            .files
            .values()
            .filter(|f| f.class == FileClass::Segment)
            .flat_map(StoredFile::entries)
            .any(|e| e.position.generation == generation);
        if mark.is_none()
            && !in_use
            && self.disk.damaged.is_empty()
            && self.disk.invalid_tail.is_empty()
        {
            self.disk.mark_damage = None;
            self.disk.tail_fate = None;
            self.disk.invalid_tail = Tail::default();
            return false;
        }
        let same_boot = mark.is_some_and(|m| m.boot == self.boot);
        let (beyond, sealed_beyond) = mark.map_or((Vec::new(), false), |m| self.beyond(m.last));
        let state = match mark {
            None => self.disk.mark_damage.unwrap_or(MarkState::Missing),
            Some(_) if same_boot && (beyond.len() > 1 || sealed_beyond) => MarkState::Stale,
            Some(_) => MarkState::Readable,
        };
        let rule_1 = same_boot && state == MarkState::Readable;
        let mut tail = self.disk.invalid_tail;
        if let (true, Some(m)) = (rule_1, mark) {
            // Rule 1 truncates everything after the mark. A complete batch or
            // any non-zero byte there, damaged batches included, may have
            // been acknowledged, so the truncation is a reset point (Q-125).
            let damaged_beyond = self.disk.damaged.values().any(|(from, _)| *from > m.last);
            if beyond.is_empty() && !damaged_beyond && tail.is_empty() {
                // Step 2 syncs the segments holding a lost range found and
                // the directory, with step 3's rule for a failed sync.
                if sync_fails {
                    return true;
                }
                self.disk.tail_fate = None;
                self.record_lost_ranges();
                // The mark's slot is written again in place and synced.
                self.disk.synced_mark = self.disk.mark;
                return false;
            }
            if sync_fails {
                return true;
            }
            let (batches, torn) = self.truncate_after(m);
            self.counters.batches_discarded += u64::from(batches);
            tail.batches += batches;
            tail.torn |= torn;
        } else {
            if sync_fails {
                return true;
            }
            // Damaged batches at the end of the open segment, beyond the mark
            // or with none, are not below a valid mark and no complete batch
            // follows them: a torn tail, not corruption (§3 Recovery rules 2
            // and 3).
            if let Some(active) = self.disk.active {
                loop {
                    let n = self.disk.files[&active].batches.len();
                    let Some(&(from, _)) = n
                        .checked_sub(1)
                        .and_then(|b| self.disk.damaged.get(&(active, b)))
                    else {
                        break;
                    };
                    if mark.is_some_and(|m| from <= m.last) {
                        break;
                    }
                    self.disk.damaged.remove(&(active, n - 1));
                    self.disk
                        .files
                        .get_mut(&active)
                        .expect("segment")
                        .batches
                        .pop();
                    tail.torn = true;
                }
            }
        }
        self.disk.tail_fate = None;
        self.disk.invalid_tail = Tail::default();
        // Step 4: the lost ranges found.
        self.record_lost_ranges();
        let k = match mark {
            Some(m) if rule_1 => m.last,
            _ => self.last_kept(generation),
        };
        let f = self.first_new(k);
        let id_floor = self.id_floor(mark.map(|m| m.max_series_id));
        self.disk.nonce += 1;
        let stamp = ResetStamp {
            sequence: f.sequence,
            nonce: self.disk.nonce,
        };
        let entry = RecoveryEntry {
            after: k,
            stamp,
            boot: self.boot,
            tail,
            mark_state: state,
            id_floor,
            cursors_missing: std::mem::take(&mut self.disk.lost_cursors)
                .into_iter()
                .collect(),
        };
        // Step 5: the entry, alone in its batch.
        self.append_batch(vec![Entry {
            position: f,
            received_at: self.wall,
            body: EntryBody::Recovery(entry.clone()),
        }]);
        self.disk.recoveries.insert(f, entry.clone());
        // Step 6: the mark, synced; then the entry's gap.
        let active = self.disk.active.expect("active");
        self.disk.mark = Some(Mark {
            boot: self.boot,
            file: active,
            batches: self.disk.files[&active].batches.len(),
            last: f,
            max_series_id: id_floor,
            stamp,
        });
        self.disk.synced_mark = self.disk.mark;
        self.disk.mark_damage = None;
        if let Some((from, to)) = entry.gap(f) {
            let epoch = self.disk.epoch();
            self.disk.manifest.push(ManifestRecord {
                epoch,
                at: self.wall,
                op: ManifestOp::LostRange {
                    from,
                    to,
                    identity: from,
                    cause: LossCause::TruncatedTail,
                    stamp: Some(stamp),
                },
            });
        }
        false
    }

    /// Rule 1: truncates every batch after the mark `m`, damaged ones
    /// included. Returns the complete batches truncated and whether a damaged
    /// one was among them.
    fn truncate_after(&mut self, m: Mark) -> (u32, bool) {
        let (mut batches, mut torn) = (0u32, false);
        let ids: Vec<FileId> = self
            .disk
            .files
            .iter()
            .filter(|(id, f)| f.class == FileClass::Segment && !f.sealed && **id >= m.file)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let keep = if id == m.file { m.batches } else { 0 };
            let file = self.disk.files.get_mut(&id).expect("segment");
            for (b, batch) in file.batches.iter().enumerate().skip(keep) {
                if self.disk.damaged.remove(&(id, b)).is_some() {
                    torn = true;
                } else if !batch.is_empty() {
                    batches += 1;
                }
            }
            file.batches.truncate(keep);
        }
        (batches, torn)
    }

    /// The complete batches of the log beyond `last`, as segment and index,
    /// and whether a sealed segment holds a position above it (§3 Recovery
    /// rule 1).
    fn beyond(&self, last: Position) -> (Vec<(FileId, usize)>, bool) {
        let mut out = Vec::new();
        let mut sealed = false;
        for (id, f) in &self.disk.files {
            if f.class != FileClass::Segment {
                continue;
            }
            for (b, batch) in f.batches.iter().enumerate() {
                if batch.first().is_some_and(|e| e.position > last) {
                    out.push((*id, b));
                    sealed |= f.sealed;
                }
            }
        }
        (out, sealed)
    }

    /// K under rule 3: the last kept position of the current generation, or
    /// the end of a lost range at the tail (§3 Recovery rule 3).
    fn last_kept(&self, generation: u32) -> Position {
        self.disk
            .files
            .values()
            .filter(|f| f.class == FileClass::Segment)
            .flat_map(StoredFile::entries)
            .map(|e| e.position)
            .chain(self.disk.lost_ranges().into_iter().map(|(_, to)| to))
            .filter(|p| p.generation == generation)
            .max()
            .unwrap_or(Position::new(generation, 0))
    }

    /// F: one above the highest sequence of the current generation among K
    /// and every position named by a local cursor file, manifest record or
    /// derived file (§3 Recovery rule 3).
    fn first_new(&self, k: Position) -> Position {
        let named = self
            .disk
            .cursors
            .values()
            .copied()
            .chain(self.disk.latches.values().map(|l| l.position))
            .chain(self.disk.manifest.iter().flat_map(|r| self.disk.named(r)))
            .chain(self.disk.checkpoints.iter().map(|c| c.position))
            // The registry file keeps every recovery entry with its position
            // (§2.1): a derived file.
            .chain(self.disk.recoveries.keys().copied())
            .filter(|p| p.generation == k.generation);
        named.fold(k, Position::max).next()
    }

    /// The id floor (§2.1): the highest id known to the mark, the kept log
    /// (definitions and the series of kept facts), earlier recovery entries
    /// and the restore's checkpoint.
    fn id_floor(&self, mark: Option<u32>) -> u32 {
        // Every complete batch of the log, the open segment's included,
        // whatever the mark covers.
        let log = self
            .disk
            .files
            .values()
            .filter(|f| f.class == FileClass::Segment)
            .flat_map(StoredFile::entries)
            .filter_map(|e| match &e.body {
                EntryBody::Registry(RegistryEvent::Define { id, .. }) => Some(id.0),
                EntryBody::Fact(f) => Some(f.series.0),
                EntryBody::Recovery(r) => Some(r.id_floor),
                _ => None,
            })
            .max();
        let checkpoint = self
            .disk
            .restore
            .as_ref()
            .and_then(|r| r.checkpoint.as_ref())
            .map(|c| c.registry.next_id().0 - 1);
        let earlier = self.disk.recoveries.values().map(|r| r.id_floor).max();
        [mark, log, checkpoint, earlier]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(0)
    }

    /// §3 Recovery rule 2: a damaged batch of the open segment lies below the
    /// mark (injection allows nothing else), so it was synced: corruption, not
    /// a torn tail. Later batches are kept and nothing is overwritten; each run
    /// of adjacent damaged batches is recorded as one lost range. The engine
    /// takes the range from the last valid batch before it to the next valid
    /// batch or, with none, to the mark's last position; the model keeps the
    /// positions each damaged batch held, which give the same range when its
    /// neighbours are valid, and do not repeat an earlier lost range next to it.
    /// A recovery whose sync fails writes no record (Q-79), but reports the
    /// ranges it found as `data_lost` (§8 "recorded when found"); the next
    /// recovery that can write records them, with any a scrub found while the
    /// engine could not write. A range that overlaps a recorded loss names its
    /// identity (§3 Order of a start, step 4).
    fn record_lost_ranges(&mut self) {
        let quarantined: Vec<FileId> = self.disk.quarantined.iter().copied().collect();
        for f in quarantined {
            self.record_quarantine(f, QuarantineCause::Damaged);
        }
        let at = self.wall;
        let stamp = self.stamp_in_force();
        let found = std::mem::take(&mut self.disk.found_lost);
        for (from, to) in found.into_iter().chain(self.disk.damaged_runs()) {
            let identity = self
                .disk
                .lost_records()
                .iter()
                .find(|r| r.from <= to && from <= r.to)
                .map_or(from, |r| r.identity);
            let epoch = self.disk.epoch();
            self.disk.manifest.push(ManifestRecord {
                epoch,
                at,
                op: ManifestOp::LostRange {
                    from,
                    to,
                    identity,
                    cause: LossCause::Corruption,
                    stamp,
                },
            });
        }
        self.disk.damaged.clear();
    }

    /// Moves the durable mark (in the page cache) to the end of the active
    /// segment, stamped with the current boot id, after a successful sync; the
    /// mark file is synced too when [`Config::mark_sync`] is every commit.
    pub(super) fn sync_mark(&mut self) {
        let Some(active) = self.disk.active else {
            return;
        };
        let file = &self.disk.files[&active];
        // A lost range at the end of the segment stays covered (§3 Recovery
        // rule 2: its positions are not reused).
        let last = file
            .entries()
            .last()
            .map(|e| e.position)
            .max(self.disk.mark.map(|m| m.last));
        let defined = file
            .batches
            .last()
            .into_iter()
            .flatten()
            .filter_map(|e| match &e.body {
                EntryBody::Registry(RegistryEvent::Define { id, .. }) => Some(id.0),
                _ => None,
            })
            .max();
        if let Some(last) = last {
            let prev = self.disk.mark;
            self.disk.mark = Some(Mark {
                boot: self.boot,
                file: active,
                batches: file.batches.len(),
                last,
                max_series_id: prev.map(|m| m.max_series_id).max(defined).unwrap_or(0),
                stamp: self.stamp(),
            });
            if self.config.mark_sync == MarkSync::EveryCommit {
                self.disk.synced_mark = self.disk.mark;
            }
        }
    }

    /// The mark file is synced now: at once after a batch holding an entry
    /// that is not a fact (§3 Commit).
    pub(super) fn force_mark_sync(&mut self) {
        self.disk.synced_mark = self.disk.mark;
    }

    /// The timed mark sync fires (§3 Durable mark, `mark_sync_interval`):
    /// the mark file is synced. Nothing happens while the engine is down or
    /// read-only.
    pub fn sync_mark_file(&mut self) {
        if self.vol.as_ref().is_some_and(|v| !v.read_only) {
            self.force_mark_sync();
        }
    }

    /// The reset stamp in force when a lost range is recorded (Q-124 (c)):
    /// the mark's; with the mark lost, that of the last kept recovery entry,
    /// or of the restore record when its generation holds none; `None` when
    /// none is known.
    pub(super) fn stamp_in_force(&self) -> Option<ResetStamp> {
        if let Some(m) = self.disk.mark {
            return Some(m.stamp);
        }
        let generation = self.disk.generation;
        self.disk
            .files
            .values()
            .filter(|f| f.class == FileClass::Segment)
            .flat_map(StoredFile::entries)
            .filter_map(|e| match &e.body {
                EntryBody::Recovery(r) => Some((e.position, r.stamp)),
                _ => None,
            })
            .max()
            .filter(|(p, _)| p.generation == generation)
            .map(|(_, s)| s)
            .or_else(|| self.disk.restore.as_ref().map(|r| r.stamp))
    }

    /// The current reset stamp (§7 Readers): the mark's, or with no mark yet
    /// the restore record's; zero before the first reset.
    pub fn stamp(&self) -> ResetStamp {
        self.disk
            .mark
            .map(|m| m.stamp)
            .or_else(|| self.disk.restore.as_ref().map(|r| r.stamp))
            .unwrap_or(ResetStamp::ZERO)
    }

    /// The recovery entries the store knows, by position: those of the kept
    /// log and those the registry file keeps (§11 Health).
    pub fn recovery_entries(&self) -> BTreeMap<Position, RecoveryEntry> {
        let mut out = self.disk.recoveries.clone();
        for (p, e) in self.disk.live_entries() {
            if let EntryBody::Recovery(r) = e.body {
                out.insert(p, r);
            }
        }
        out
    }

    /// The reset points of the log (§4 Resync): the kept recovery entries and
    /// the lost ranges not described by one.
    pub fn reset_points(&self) -> Vec<ResetPoint> {
        let kept: Vec<(Position, RecoveryEntry)> = self
            .disk
            .live_entries()
            .into_iter()
            .filter_map(|(p, e)| match e.body {
                EntryBody::Recovery(r) => Some((p, r)),
                _ => None,
            })
            .collect();
        crate::recovery::reset_points(&kept, &self.disk.lost_ranges())
    }

    /// The list of losses (§11 Health): every loss, for ever, each once.
    pub fn losses(&self) -> Vec<Loss> {
        let mut found = self.disk.found_lost.clone();
        if self.vol.is_some() {
            found.extend(self.disk.damaged_runs());
        }
        let recoveries: Vec<(Position, RecoveryEntry)> =
            self.recovery_entries().into_iter().collect();
        crate::recovery::losses(
            &self.disk.lost_records(),
            &found,
            &recoveries,
            &self.disk.acknowledged(),
            |l| self.loss_stage(l),
        )
    }

    /// Acknowledges the `data_lost` losses named by their identities (§8
    /// Acknowledging a loss): one closing record, by an operator, naming each
    /// with its range as reported now and, as seen, the corruptions they came
    /// from. Acknowledging an acknowledged loss, a `possible_loss` or none
    /// writes nothing, except to name a corruption not yet seen. Refused
    /// while read-only and in a drill.
    pub fn acknowledge_losses(&mut self, identities: &[Position]) -> Result<(), Error> {
        if self.vol()?.read_only {
            return Err(Error::ReadOnly);
        }
        let losses: Vec<Acknowledged> = self
            .losses()
            .into_iter()
            // A loss whose copy is being brought back closes as restored in
            // full (Q-248).
            .filter(|l| {
                l.kind == LossKind::DataLost
                    && !l.acknowledged
                    && l.state != Some(LossState::Copying)
                    && identities.contains(&l.identity)
            })
            .filter_map(|l| {
                Some(Acknowledged {
                    identity: l.identity,
                    from: l.first?,
                    to: l.last?,
                    outcome: Outcome::Operator,
                })
            })
            .collect();
        let seen = self.unseen_corruptions_of(identities);
        if losses.is_empty() && seen.is_empty() {
            return Ok(());
        }
        if let Some(to) = losses.iter().map(|a| a.to).max() {
            self.disk.cover(to);
        }
        let epoch = self.disk.epoch();
        self.disk.manifest.push(ManifestRecord {
            epoch,
            at: self.wall,
            op: ManifestOp::Acknowledge { losses, seen },
        });
        Ok(())
    }

    /// The resets the store holds, in the order they were written (§7
    /// Readers): the kept recovery entries in log order with the restore
    /// record before the first entry of its generation, and each lost range
    /// after the reset whose stamp the mark carried when it was recorded.
    fn resets(&self) -> Vec<Reset> {
        let mut out = Vec::new();
        let mut restore = self.disk.restore.as_ref().map(|r| Reset::Restore {
            stamp: r.stamp,
            generation: r.generation,
            restored_end: r.restored_end,
        });
        let mut gaps = Vec::new();
        for (at, e) in self.disk.live_entries() {
            let EntryBody::Recovery(r) = e.body else {
                continue;
            };
            if let Some(Reset::Restore { generation, .. }) = restore
                && at.generation >= generation
            {
                out.extend(restore.take());
            }
            gaps.extend(r.gap(at));
            out.push(Reset::Recovery {
                stamp: r.stamp,
                at,
                after: r.after,
            });
        }
        out.extend(restore);
        let mut placed: Vec<(usize, Reset)> = Vec::new();
        for r in &self.disk.manifest {
            let ManifestOp::LostRange {
                from, to, stamp, ..
            } = r.op
            else {
                continue;
            };
            // A gap a kept entry describes is that entry's reset.
            if gaps.contains(&(from, to)) {
                continue;
            }
            // After the reset carrying the stamp in force when it was
            // recorded; with none known, after every reset (Q-124 (c)).
            let after = match stamp {
                Some(ResetStamp::ZERO) => 0,
                Some(stamp) => out
                    .iter()
                    .position(|x| match x {
                        Reset::Recovery { stamp: s, .. } | Reset::Restore { stamp: s, .. } => {
                            *s == stamp
                        }
                        Reset::Lost { .. } => false,
                    })
                    .map_or(out.len(), |i| i + 1),
                None => out.len(),
            };
            placed.push((after, Reset::Lost { from, to }));
        }
        // Insert from the back so that indices stay valid; ranges placed at
        // one index keep the order they were recorded in.
        placed.sort_by_key(|x| x.0);
        for (i, r) in placed.into_iter().rev() {
            out.insert(i, r);
        }
        out
    }

    /// Judges a saved point against the current mark (§7 Readers, Reset stamp
    /// and saved points; `api.md` `Reader::check`): kept, void above a
    /// position, or unknown.
    pub fn check_point(&self, point: SavedPoint) -> PointCheck {
        crate::recovery::check_saved_point(
            point,
            self.disk.generation,
            self.stamp(),
            &self.resets(),
        )
    }
}
