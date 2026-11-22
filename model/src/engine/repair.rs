//! Self-repair (§8 Self-repair): the loss life cycle and its closing records,
//! quarantine records, corruptions and attention, and the scheduled check of
//! the bucket (Q-85).

use std::collections::BTreeSet;

use super::{Error, FileClass, FileId, ManifestOp, ManifestRecord, Model};
use crate::recovery::{Acknowledged, Loss, LossCause, LossKind, LossState, Outcome};
use crate::types::{HOUR, MINUTE, Nanos, Position};

/// Self-repair configuration (`api.md` `repair.*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepairConfig {
    /// `repair.acknowledge = manual`: a loss whose stage 2 has ended waits
    /// for `acknowledge_losses`.
    pub manual: bool,
    /// `repair.resend_wait`; 0 says nothing listens (Q-242).
    pub resend_wait: Nanos,
    /// Failed lookups before an object counts as lost (Q-82).
    pub lost_tries: u32,
    /// The span those lookups must cover (Q-82).
    pub lost_over: Nanos,
    /// Corruptions within `suspect_within` that make the storage suspect.
    pub suspect_after: u32,
    /// The window of `suspect_after`.
    pub suspect_within: Nanos,
}

impl Default for RepairConfig {
    fn default() -> Self {
        RepairConfig {
            manual: false,
            resend_wait: 24 * HOUR,
            lost_tries: 3,
            lost_over: 10 * MINUTE,
            suspect_after: 2,
            suspect_within: 30 * 24 * HOUR,
        }
    }
}

/// Why a file was quarantined (`format.md` A.14 `0x26` field 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineCause {
    /// Its copy on a tier failed its hash.
    Damaged,
    /// The bucket has lost it (§8 Self-repair, Q-85).
    LostInBucket,
}

/// A corruption (§8 Self-repair, Attention; `format.md` A.14 corruption seen).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Corruption {
    /// One finding of a file damaged or lost: the file and the epoch of the
    /// quarantine record naming it, `None` while no record does. A second
    /// finding of the same file is a corruption of its own.
    File(FileId, Option<u64>),
    /// A region a start found, named by its loss's identity.
    Range(Position),
}

/// One corruption as `Model::corruptions` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorruptionReport {
    /// Its name.
    pub id: Corruption,
    /// The `data_lost` it caused, if any.
    pub loss: Option<Position>,
    /// Whether a manifest record names it as seen.
    pub seen: bool,
    /// When the record naming it was written; `None` while only found.
    pub at: Option<Nanos>,
}

/// What asks for a person (§8 Self-repair, Attention); only repeated corruption
/// also degrades health (Q-247).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Alert {
    /// A corruption not yet seen, repaired or not (Q-240).
    Corruption(Corruption),
    /// This many corruptions within `repair.suspect_within`.
    StorageSuspect(u32),
    /// A write or sync failed.
    Fault,
    /// Sealed files wait for upload while the bucket is unreachable.
    BackupStalled,
}

/// A re-send request (§8 Self-repair stage 2, `api.md` `ResendRequest`). The
/// model knows positions, not the series a lost range held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResendRequest {
    /// The loss's identity.
    pub identity: Position,
    /// First lost position.
    pub first: Position,
    /// Last lost position.
    pub last: Position,
    /// When stage 2 ends by the wait.
    pub until: Nanos,
}

impl Model {
    /// The self-repair stage of a recorded, open `data_lost` (§8
    /// Self-repair): copying while a quarantined file holding it has a copy
    /// elsewhere; otherwise awaiting re-send until the wait after its latest
    /// lost-range record, then, under manual acknowledgement, awaiting an
    /// operator.
    pub(super) fn loss_stage(&self, l: &Loss) -> LossState {
        if self.copy_pending(l) {
            return LossState::Copying;
        }
        let until = self.resend_until(l.identity);
        if self.config.repair.manual && self.resend_ended(l.identity, until) {
            LossState::AwaitingOperator
        } else {
            LossState::AwaitingResend { until }
        }
    }

    fn resend_until(&self, identity: Position) -> Nanos {
        let at = self
            .disk
            .lost_records()
            .iter()
            .filter(|r| r.identity == identity)
            .map(|r| r.at)
            .max()
            .unwrap_or(self.wall);
        at.saturating_add(self.config.repair.resend_wait)
    }

    fn resend_ended(&self, identity: Position, until: Nanos) -> bool {
        self.config.repair.resend_wait == 0
            || self.wall >= until
            || self
                .vol
                .as_ref()
                .is_some_and(|v| v.resend_done.contains(&identity))
    }

    /// Whether a quarantined file holding the loss's first position may
    /// still get a copy to swap in (stage 1).
    fn copy_pending(&self, l: &Loss) -> bool {
        let Some(first) = l.first else {
            return false;
        };
        self.disk.quarantined.iter().any(|f| {
            self.disk.copy_may_come(*f) && self.disk.files[f].entries().any(|e| e.position == first)
        })
    }

    /// The open re-send requests (§8 Self-repair stage 2; `api.md`
    /// `repairs()`): every recorded `data_lost` whose stage 2 has not ended.
    pub fn resend_requests(&self) -> Vec<ResendRequest> {
        self.losses()
            .into_iter()
            .filter_map(|l| match l.state {
                Some(LossState::AwaitingResend { until })
                    if !self.resend_ended(l.identity, until) =>
                {
                    Some(ResendRequest {
                        identity: l.identity,
                        first: l.first?,
                        last: l.last?,
                        until,
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// The application ends stage 2 of a loss (`api.md` `repair_done`): the
    /// loss is closed as re-send ended, or under manual acknowledgement
    /// awaits an operator. A loss not awaiting re-send writes nothing.
    /// Refused while read-only.
    pub fn repair_done(&mut self, identity: Position) -> Result<(), Error> {
        if self.vol()?.read_only {
            return Err(Error::ReadOnly);
        }
        let Some(l) = self.losses().into_iter().find(|l| {
            l.identity == identity
                && matches!(l.state, Some(LossState::AwaitingResend { until }) if !self.resend_ended(identity, until))
        }) else {
            return Ok(());
        };
        if self.config.repair.manual {
            self.vol_mut()?.resend_done.insert(identity);
        } else {
            self.close(&l, Outcome::ResendEnded);
        }
        Ok(())
    }

    /// The engine's self-repair step (§8 Self-repair stage 3): under
    /// automatic acknowledgement, every recorded `data_lost` whose stage 2
    /// has ended gets its closing record; under manual acknowledgement such a
    /// loss awaits an operator instead (`loss_stage`). Returns how many were closed.
    /// Refused while read-only.
    pub fn repair(&mut self) -> Result<usize, Error> {
        if self.vol()?.read_only {
            return Err(Error::ReadOnly);
        }
        let due: Vec<Loss> = self
            .losses()
            .into_iter()
            .filter(|l| {
                matches!(l.state, Some(LossState::AwaitingResend { until }) if self.resend_ended(l.identity, until))
            })
            .collect();
        for l in &due {
            self.close(l, Outcome::ResendEnded);
        }
        Ok(due.len())
    }

    /// Appends a closing record for one loss as reported now.
    fn close(&mut self, l: &Loss, outcome: Outcome) {
        let (Some(from), Some(to)) = (l.first, l.last) else {
            return;
        };
        // A record names no position above the last synced mark (invariant
        // 26).
        self.disk.cover(to);
        let epoch = self.disk.epoch();
        self.disk.manifest.push(ManifestRecord {
            epoch,
            at: self.wall,
            op: ManifestOp::Acknowledge {
                losses: vec![Acknowledged {
                    identity: l.identity,
                    from,
                    to,
                    outcome,
                }],
                seen: Vec::new(),
            },
        });
    }

    /// Closes as restored in full every recorded, open `data_lost` whose
    /// range the file `f` now holds again (§8 Self-repair stage 1). Returns
    /// how many were closed.
    pub(super) fn close_restored(&mut self, f: FileId) -> usize {
        let held: BTreeSet<Position> = self.disk.files[&f].entries().map(|e| e.position).collect();
        let restored: Vec<(Loss, u64)> = self
            .losses()
            .into_iter()
            .filter(|l| l.kind == LossKind::DataLost && l.recorded && !l.acknowledged)
            .filter_map(|l| {
                let (from, to) = (l.first?, l.last?);
                let n = held.range(from..=to).count() as u64;
                let whole = held.contains(&from) && held.contains(&to);
                whole.then_some((l, n))
            })
            .collect();
        for (l, n) in &restored {
            self.close(l, Outcome::Restored(*n));
        }
        restored.len()
    }

    /// Whether the file `f` holds a position of a closed loss that was not
    /// restored: its copy is not swapped in (Q-246).
    pub(super) fn holds_closed_loss(&self, f: FileId) -> bool {
        let held: BTreeSet<Position> = self.disk.files[&f].entries().map(|e| e.position).collect();
        self.losses().iter().any(|l| {
            matches!(l.state, Some(LossState::Closed(o)) if !matches!(o, Outcome::Restored(_)))
                && l.first
                    .zip(l.last)
                    .is_some_and(|(a, b)| held.range(a..=b).next().is_some())
        })
    }

    /// Whether a quarantined file still holds an error open (§8 Corrupt
    /// block, Self-repair): a sealed log segment does only while a swap can
    /// still happen or its lost positions are only found, not recorded.
    pub(super) fn quarantine_open(&self, f: FileId) -> bool {
        let file = &self.disk.files[&f];
        if file.class != FileClass::Segment || !file.sealed {
            return true;
        }
        if self.disk.copy_may_come(f) && !self.holds_closed_loss(f) {
            return true;
        }
        file.entries().any(|e| {
            self.disk
                .found_lost
                .iter()
                .any(|(a, b)| *a <= e.position && e.position <= *b)
        })
    }

    /// The positions of `f` that no other file holds, nor `f`'s own copy on
    /// another tier when `copy` is false.
    pub(super) fn held_without(&self, f: FileId, copy: bool) -> BTreeSet<Position> {
        let mut held = BTreeSet::new();
        for g in self.disk.published() {
            if g == f && !copy {
                continue;
            }
            if let Some(file) = self.disk.readable(g) {
                held.extend(
                    self.disk
                        .visible_entries(g, file)
                        .into_iter()
                        .map(|e| e.position),
                );
            }
        }
        held
    }

    /// Appends a quarantine record for one finding of `f`.
    pub(super) fn record_quarantine(&mut self, f: FileId, cause: QuarantineCause) {
        let epoch = self.disk.epoch();
        self.disk.manifest.push(ManifestRecord {
            epoch,
            at: self.wall,
            op: ManifestOp::Quarantine { file: f, cause },
        });
    }

    /// Every quarantine record: the file, the record's epoch and time.
    fn quarantine_records(&self) -> Vec<(FileId, u64, Nanos)> {
        self.disk
            .manifest
            .iter()
            .filter_map(|r| match r.op {
                ManifestOp::Quarantine { file, .. } => Some((file, r.epoch, r.at)),
                _ => None,
            })
            .collect()
    }

    fn seen(&self) -> BTreeSet<Corruption> {
        self.disk
            .manifest
            .iter()
            .flat_map(|r| match &r.op {
                ManifestOp::Acknowledge { seen, .. } => seen.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    /// Every corruption (§8 Self-repair, Attention): each quarantine record,
    /// each quarantined file no record names yet, and each region of a
    /// `data_lost` by corruption that no such finding caused. A loss is
    /// linked to the finding of a file holding its range recorded last before
    /// each of its lost-range records. A truncated tail and a
    /// `possible_loss` are not corruptions.
    pub fn corruptions(&self) -> Vec<CorruptionReport> {
        let seen = self.seen();
        let unrecorded = self
            .disk
            .unrecorded
            .iter()
            .filter(|f| self.disk.quarantined.contains(f))
            .map(|f| (Corruption::File(*f, None), None));
        let mut out: Vec<CorruptionReport> = self
            .quarantine_records()
            .into_iter()
            .map(|(f, epoch, at)| (Corruption::File(f, Some(epoch)), Some(at)))
            .chain(unrecorded)
            .map(|(id, at)| CorruptionReport {
                id,
                loss: None,
                seen: seen.contains(&id),
                at,
            })
            .collect();
        let holds = |f: FileId, p: Position| self.disk.files[&f].entries().any(|e| e.position == p);
        let records = self.disk.lost_records();
        for l in self.losses().into_iter().filter(|l| {
            l.kind == LossKind::DataLost
                && matches!(l.cause, LossCause::Corruption | LossCause::LostInBucket)
        }) {
            let mut causes: Vec<Corruption> = records
                .iter()
                .filter(|r| r.identity == l.identity)
                .filter_map(|r| {
                    out.iter()
                        .filter_map(|c| match c.id {
                            Corruption::File(f, Some(e)) if e < r.epoch && holds(f, r.from) => {
                                Some((e, c.id))
                            }
                            _ => None,
                        })
                        .max()
                        .map(|(_, id)| id)
                })
                .collect();
            if let Some(first) = l.first.filter(|_| causes.is_empty()) {
                causes.extend(
                    out.iter()
                        .map(|c| c.id)
                        .filter(|id| matches!(id, Corruption::File(f, None) if holds(*f, first))),
                );
            }
            if causes.is_empty() {
                let id = Corruption::Range(l.identity);
                out.push(CorruptionReport {
                    id,
                    loss: Some(l.identity),
                    seen: seen.contains(&id),
                    at: records
                        .iter()
                        .find(|r| r.identity == l.identity)
                        .map(|r| r.at),
                });
            }
            for c in out.iter_mut().filter(|c| causes.contains(&c.id)) {
                c.loss = Some(l.identity);
            }
        }
        out.sort_by_key(|c| c.id);
        out
    }

    /// Names corruptions as seen (`api.md` `acknowledge_corruptions`): one
    /// record naming those not yet seen; none, nothing written. Refused
    /// while read-only.
    pub fn acknowledge_corruptions(&mut self, ids: &[Corruption]) -> Result<(), Error> {
        if self.vol()?.read_only {
            return Err(Error::ReadOnly);
        }
        let seen: Vec<Corruption> = self
            .corruptions()
            .into_iter()
            .filter(|c| !c.seen && ids.contains(&c.id))
            .map(|c| c.id)
            .collect();
        if !seen.is_empty() {
            let epoch = self.disk.epoch();
            self.disk.manifest.push(ManifestRecord {
                epoch,
                at: self.wall,
                op: ManifestOp::Acknowledge {
                    losses: Vec::new(),
                    seen,
                },
            });
        }
        Ok(())
    }

    /// The corruptions not yet seen that the losses `identities` came from.
    pub(super) fn unseen_corruptions_of(&self, identities: &[Position]) -> Vec<Corruption> {
        self.corruptions()
            .into_iter()
            .filter(|c| !c.seen && c.loss.is_some_and(|l| identities.contains(&l)))
            .map(|c| c.id)
            .collect()
    }

    /// The corruptions within `repair.suspect_within`, seen or not, when at
    /// or above `repair.suspect_after` (§8 Self-repair, Q-247).
    pub(super) fn suspect_count(&self) -> Option<u32> {
        let since = self.wall.saturating_sub(self.config.repair.suspect_within);
        let recent = self
            .corruptions()
            .iter()
            .filter(|c| c.at.is_none_or(|at| at >= since))
            .count();
        let recent = u32::try_from(recent).unwrap_or(u32::MAX);
        (recent >= self.config.repair.suspect_after).then_some(recent)
    }

    /// What asks for a person now (§8 Self-repair, Attention): each
    /// corruption not yet seen, repeated corruption, a fault and a stalled
    /// backup. Only repeated corruption also degrades [`Model::health`].
    pub fn attention(&self) -> Vec<Alert> {
        let mut out: Vec<Alert> = self
            .corruptions()
            .iter()
            .filter(|c| !c.seen)
            .map(|c| Alert::Corruption(c.id))
            .collect();
        if let Some(n) = self.suspect_count() {
            out.push(Alert::StorageSuspect(n));
        }
        if self.vol.as_ref().is_some_and(|v| v.sync_failed) {
            out.push(Alert::Fault);
        }
        if !self.disk.bucket.reachable && !self.disk.upload_queue().is_empty() {
            out.push(Alert::BackupStalled);
        }
        out
    }

    /// Fault injection: the bucket reports the object of `f` absent (§8
    /// Self-repair, Q-85). [`Model::find_object`] undoes it.
    pub fn lose_object(&mut self, f: FileId) {
        self.disk.bucket.missing.insert(f);
    }

    /// Fault injection: a lagging listing shows the object of `f` again.
    pub fn find_object(&mut self, f: FileId) {
        self.disk.bucket.missing.remove(&f);
        if let Some(v) = self.vol.as_mut() {
            v.lookups.remove(&f);
        }
    }

    /// The scheduled check of the bucket (§8 Self-repair, Lost in the
    /// bucket; Q-82, Q-85): one lookup of every uploaded object. An object
    /// is lost after `repair.lost_tries` failed lookups spanning at least
    /// `repair.lost_over` while the bucket answers; an unreachable bucket
    /// counts nothing. A lost file is quarantined by record and uploaded
    /// again from a verified local copy, or its positions no other file
    /// holds become a lost range of cause lost in the bucket; one that
    /// answers again while its loss is open restores it in full. Returns the
    /// files judged lost. Refused while read-only.
    pub fn check_bucket(&mut self) -> Result<Vec<FileId>, Error> {
        if self.vol()?.read_only {
            return Err(Error::ReadOnly);
        }
        if !self.disk.bucket.reachable {
            return Ok(Vec::new());
        }
        // An object judged lost that answers again restores its open loss in
        // full (stage 1) and is looked up again from then on; one whose loss
        // is closed stays closed and judged (Q-246, Q-600).
        let back: Vec<FileId> = self
            .disk
            .judged_lost
            .iter()
            .copied()
            .filter(|f| self.disk.in_bucket(*f))
            .collect();
        for f in back {
            if self.close_restored(f) > 0 {
                self.disk.judged_lost.remove(&f);
            }
        }
        let wall = self.wall;
        let missing: Vec<FileId> = self
            .disk
            .bucket
            .missing
            .iter()
            .copied()
            .filter(|f| {
                self.disk.bucket.objects.contains_key(f) && !self.disk.judged_lost.contains(f)
            })
            .collect();
        let cfg = self.config.repair;
        let mut judged = Vec::new();
        for f in missing {
            let v = self.vol_mut()?;
            let tries = v.lookups.entry(f).or_default();
            tries.push(wall);
            let span = tries.last().copied().unwrap_or(wall) - tries[0];
            if tries.len() < cfg.lost_tries as usize || span < cfg.lost_over {
                continue;
            }
            v.lookups.remove(&f);
            judged.push(f);
            self.record_quarantine(f, QuarantineCause::LostInBucket);
            let local = (self.disk.hot.contains(&f)
                && !self.disk.corrupt.contains(&f)
                && !self.disk.quarantined.contains(&f))
                || self.disk.cold.contains(&f);
            if local {
                self.disk.bucket.missing.remove(&f);
                continue;
            }
            self.disk.judged_lost.insert(f);
            let held = self.held_without(f, false);
            let lost: Vec<Position> = self.disk.files[&f]
                .entries()
                .map(|e| e.position)
                .filter(|p| !held.contains(p) && !self.disk.is_hidden(*p))
                .collect();
            self.record_runs(&lost, LossCause::LostInBucket);
        }
        Ok(judged)
    }

    /// Records each run of consecutive positions as a lost range of `cause`;
    /// a run overlapping a recorded loss names its identity (§3 Order of a
    /// start, step 4), so a loss restored in full and lost again is open
    /// again under the same identity (§8 Self-repair stage 1).
    pub(super) fn record_runs(&mut self, lost: &[Position], cause: LossCause) {
        let mut runs: Vec<(Position, Position)> = Vec::new();
        for p in lost {
            match runs.last_mut() {
                Some((_, end)) if end.next() == *p => *end = *p,
                _ => runs.push((*p, *p)),
            }
        }
        let stamp = self.stamp_in_force();
        for (from, to) in runs {
            let identity = self
                .disk
                .lost_records()
                .iter()
                .filter(|r| r.from <= to && from <= r.to)
                .map(|r| r.identity)
                .min()
                .unwrap_or(from);
            let epoch = self.disk.epoch();
            self.disk.manifest.push(ManifestRecord {
                epoch,
                at: self.wall,
                op: ManifestOp::LostRange {
                    from,
                    to,
                    identity,
                    cause,
                    stamp,
                },
            });
        }
    }
}
