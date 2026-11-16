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
    /// A file, named by its quarantine record or its place under
    /// `quarantine/`.
    File(FileId),
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

/// What asks for a person (§8 Self-repair, Attention). None degrades health.
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

    /// Whether a quarantined file holding the loss's first position has a
    /// verified copy to swap in (stage 1).
    fn copy_pending(&self, l: &Loss) -> bool {
        let Some(first) = l.first else {
            return false;
        };
        self.disk.quarantined.iter().any(|f| {
            self.disk.verified_elsewhere(*f)
                && self.disk.files[f].entries().any(|e| e.position == first)
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
    /// range the file `f` now holds again (§8 Self-repair stage 1).
    pub(super) fn close_restored(&mut self, f: FileId) {
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
        for (l, n) in restored {
            self.close(&l, Outcome::Restored(n));
        }
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
    /// block): it does until swapped, except a sealed log segment with no
    /// copy whose lost positions are recorded.
    pub(super) fn quarantine_open(&self, f: FileId) -> bool {
        let file = &self.disk.files[&f];
        if file.class != FileClass::Segment || !file.sealed || self.disk.verified_elsewhere(f) {
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

    /// Appends a quarantine record for `f` unless one names it.
    pub(super) fn record_quarantine(&mut self, f: FileId, cause: QuarantineCause) {
        if self.quarantine_records().iter().any(|(g, _)| *g == f) {
            return;
        }
        let epoch = self.disk.epoch();
        self.disk.manifest.push(ManifestRecord {
            epoch,
            at: self.wall,
            op: ManifestOp::Quarantine { file: f, cause },
        });
    }

    fn quarantine_records(&self) -> Vec<(FileId, Nanos)> {
        self.disk
            .manifest
            .iter()
            .filter_map(|r| match r.op {
                ManifestOp::Quarantine { file, .. } => Some((file, r.at)),
                _ => None,
            })
            .collect()
    }

    fn quarantine_records_of(&self, cause: QuarantineCause) -> Vec<FileId> {
        self.disk
            .manifest
            .iter()
            .filter_map(|r| match r.op {
                ManifestOp::Quarantine { file, cause: c } if c == cause => Some(file),
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

    /// Every corruption (§8 Self-repair, Attention): each file a quarantine
    /// record names or that lies under `quarantine/`, and each region of a
    /// `data_lost` by corruption that no such file holds. A truncated tail
    /// and a `possible_loss` are not corruptions.
    pub fn corruptions(&self) -> Vec<CorruptionReport> {
        let seen = self.seen();
        let mut files: Vec<(FileId, Option<Nanos>)> = self
            .quarantine_records()
            .into_iter()
            .map(|(f, at)| (f, Some(at)))
            .collect();
        for f in &self.disk.quarantined {
            if !files.iter().any(|(g, _)| g == f) {
                files.push((*f, None));
            }
        }
        let mut out: Vec<CorruptionReport> = files
            .iter()
            .map(|(f, at)| CorruptionReport {
                id: Corruption::File(*f),
                loss: None,
                seen: seen.contains(&Corruption::File(*f)),
                at: *at,
            })
            .collect();
        let records = self.disk.lost_records();
        for l in self.losses().into_iter().filter(|l| {
            l.kind == LossKind::DataLost
                && matches!(l.cause, LossCause::Corruption | LossCause::LostInBucket)
        }) {
            let file = l.first.and_then(|first| {
                files
                    .iter()
                    .map(|(f, _)| *f)
                    .find(|f| self.disk.files[f].entries().any(|e| e.position == first))
            });
            match file {
                Some(f) => {
                    if let Some(c) = out.iter_mut().find(|c| c.id == Corruption::File(f)) {
                        c.loss = Some(l.identity);
                    }
                }
                None => out.push(CorruptionReport {
                    id: Corruption::Range(l.identity),
                    loss: Some(l.identity),
                    seen: seen.contains(&Corruption::Range(l.identity)),
                    at: records
                        .iter()
                        .find(|r| r.identity == l.identity)
                        .map(|r| r.at),
                }),
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

    /// What asks for a person now (§8 Self-repair, Attention): each
    /// corruption not yet seen, repeated corruption, a fault and a stalled
    /// backup. It never changes [`Model::health`].
    pub fn attention(&self) -> Vec<Alert> {
        let corruptions = self.corruptions();
        let mut out: Vec<Alert> = corruptions
            .iter()
            .filter(|c| !c.seen)
            .map(|c| Alert::Corruption(c.id))
            .collect();
        let since = self.wall.saturating_sub(self.config.repair.suspect_within);
        let recent = corruptions
            .iter()
            .filter(|c| c.at.is_none_or(|at| at >= since))
            .count();
        let recent = u32::try_from(recent).unwrap_or(u32::MAX);
        if recent >= self.config.repair.suspect_after {
            out.push(Alert::StorageSuspect(recent));
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
        // full (stage 1); a closed one stays closed (Q-246).
        let back: Vec<FileId> = self
            .quarantine_records_of(QuarantineCause::LostInBucket)
            .into_iter()
            .filter(|f| self.disk.in_bucket(*f))
            .collect();
        for f in back {
            self.close_restored(f);
        }
        let wall = self.wall;
        let missing: Vec<FileId> = self
            .disk
            .bucket
            .missing
            .iter()
            .copied()
            .filter(|f| self.disk.bucket.objects.contains_key(f))
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
            let held = self.held_without(f, false);
            let lost: Vec<Position> = self.disk.files[&f]
                .entries()
                .map(|e| e.position)
                .filter(|p| !held.contains(p) && !self.disk.is_lost(*p))
                .collect();
            self.record_runs(&lost, LossCause::LostInBucket);
        }
        Ok(judged)
    }

    /// Records each run of consecutive positions as a lost range of `cause`.
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
            let epoch = self.disk.epoch();
            self.disk.manifest.push(ManifestRecord {
                epoch,
                at: self.wall,
                op: ManifestOp::LostRange {
                    from,
                    to,
                    identity: from,
                    cause,
                    stamp,
                },
            });
        }
    }
}
