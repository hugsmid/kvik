//! Recovery entries, reset stamps, reset points, loss reports and saved
//! points (§3 Recovery, §4 Resync, §7 Readers, §8 Acknowledging a loss, §11
//! Health).
//!
//! Everything here is abstract: a tail is a count of batches, not bytes, and a
//! nonce is drawn from a counter, not at random. The functions are pure, so the
//! engine model and a test can both compute what the specification says the
//! store reports, from the log and the manifest alone.

use crate::engine::ConsumerId;
use crate::types::Position;

/// A reset stamp (§7 Readers): the sequence of the reset's first position and
/// a nonce, never zero. The mark carries zero until the first reset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResetStamp {
    /// The sequence of the recovery entry, or the restored generation's first
    /// position.
    pub sequence: u64,
    /// Never zero in a stamp a reset wrote.
    pub nonce: u64,
}

impl ResetStamp {
    /// The stamp of a store that has never been reset.
    pub const ZERO: ResetStamp = ResetStamp {
        sequence: 0,
        nonce: 0,
    };
}

/// What a start found of the mark (§3 Recovery, `api.md` `MarkState`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MarkState {
    /// A valid slot; under rule 3 the mark of another boot.
    #[default]
    Readable,
    /// No mark file.
    Missing,
    /// No valid slot.
    Unreadable,
    /// Readable, of the current boot, with two or more complete batches, or a
    /// sealed segment, beyond it (§3 Recovery rule 1).
    Stale,
}

/// The torn tail a recovery entry reports, abstractly: the complete batches
/// truncated beyond the last kept position and whether a partial batch lay
/// after them (§3 Recovery rule 3: "the tail's length in bytes up to the last
/// non-zero byte, the readable commit blocks in it").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Tail {
    /// Complete batches truncated (each has a readable commit block).
    pub batches: u32,
    /// Whether a partial batch, with no readable commit block, lay at the end.
    pub torn: bool,
}

impl Tail {
    /// Whether the tail holds no byte.
    pub fn is_empty(&self) -> bool {
        self.batches == 0 && !self.torn
    }
}

/// A recovery entry (§3 Recovery rules 1 and 3; `format.md` A.6 type 8). It is
/// a log entry at F, alone in its batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryEntry {
    /// K, the last kept position: the consumers and readers forget every
    /// position above it.
    pub after: Position,
    /// The new reset stamp, which the mark carries from then on.
    pub stamp: ResetStamp,
    /// The boot id of the start that wrote it.
    pub boot: u64,
    /// The tail it reports.
    pub tail: Tail,
    /// What the start found of the mark.
    pub mark_state: MarkState,
    /// The id floor (§2.1): the highest series id known at the start.
    pub id_floor: u32,
    /// The consumers whose cursor file was missing or unreadable.
    pub cursors_missing: Vec<ConsumerId>,
    /// A series was at the `written` level in the registry at `after`, so
    /// flagged batches may have vanished without a byte (`format.md` A.6
    /// field 48, Q-251).
    pub written_level: bool,
}

impl RecoveryEntry {
    /// Whether the entry at `at` names a gap: positions between K and F (§3
    /// Recovery rule 3).
    pub fn gap(&self, at: Position) -> Option<(Position, Position)> {
        (at.generation == self.after.generation && at.sequence > self.after.sequence + 1)
            .then(|| (self.after.next(), before(at)))
    }

    /// Whether the entry is a `possible_loss` (§11 Health): it names a
    /// non-empty tail, a mark that was not readable or stale, a missing or
    /// unreadable cursor file, or a series at the `written` level in force
    /// (Q-251). Its gap, if any, is a `data_lost` (Q-124 (a)).
    pub fn possible_loss(&self) -> bool {
        !self.tail.is_empty()
            || self.mark_state != MarkState::Readable
            || !self.cursors_missing.is_empty()
            || self.written_level
    }
}

/// The position before `p` in its generation; `(g, 0)` before the first.
pub fn before(p: Position) -> Position {
    Position::new(p.generation, p.sequence.saturating_sub(1))
}

/// A reset point (§4 Resync): a `resync` carrying `forget_above`, delivered
/// to a consumer whose position is below `id` before anything above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResetPoint {
    /// The position the reset point is delivered at.
    pub id: Position,
    /// The consumer drops every position above this.
    pub forget_above: Position,
}

/// The reset points of a log (§4 Resync), in position order: each recovery
/// entry (`forget_above` its `after`, id its position) and each lost range not
/// described by a recovery entry (`forget_above` the position before it, id
/// its last position). A recovery entry's gap is described by it.
pub fn reset_points(
    recoveries: &[(Position, RecoveryEntry)],
    lost: &[(Position, Position)],
) -> Vec<ResetPoint> {
    let gaps: Vec<(Position, Position)> =
        recoveries.iter().filter_map(|(at, e)| e.gap(*at)).collect();
    let mut out: Vec<ResetPoint> = recoveries
        .iter()
        .map(|(at, e)| ResetPoint {
            id: *at,
            forget_above: e.after,
        })
        .chain(
            lost.iter()
                .filter(|r| !gaps.contains(r))
                .map(|(from, to)| ResetPoint {
                    id: *to,
                    forget_above: before(*from),
                }),
        )
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether a loss is certain (§11 Health).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LossKind {
    /// A lost range, by corruption, a recovery entry's gap or a file lost in
    /// the bucket: health degraded until closed (§8 Self-repair).
    DataLost,
    /// A recovery entry that names a non-empty tail, a mark not readable or
    /// stale, or a missing cursor file: listed and counted, never degrading,
    /// needing no acknowledgement.
    PossibleLoss,
}

/// Why positions were lost (`format.md` Part B, the lost-range record).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LossCause {
    /// An invalid region below a valid mark or followed by a complete batch
    /// (§3 Recovery rule 2), or a corrupt sealed segment with no other copy.
    Corruption,
    /// A truncated tail: a recovery entry's tail (a `possible_loss`) and its
    /// gap (a `data_lost`), §3 Recovery rules 1 and 3.
    TruncatedTail,
    /// A file the bucket has lost or corrupted, with no local copy left
    /// (§8 Self-repair, Q-85).
    LostInBucket,
}

/// How a loss was closed (§8 Self-repair stage 3; `format.md` A.14 `0x24`
/// field 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Its positions hold the same facts again, from a copy; the count.
    Restored(u64),
    /// The re-send stage ended.
    ResendEnded,
    /// By `acknowledge_losses`.
    Operator,
}

/// Where a `data_lost` stands in self-repair (§8 Self-repair, `api.md`
/// `LossState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LossState {
    /// Reported, not yet recorded.
    Found,
    /// Recorded; a copy of its file is being brought back (stage 1).
    Copying,
    /// Offered for re-sending until this instant (stage 2).
    AwaitingResend {
        /// When stage 2 ends by the wait.
        until: i64,
    },
    /// Stage 2 ended; waiting for `acknowledge_losses` (`repair.acknowledge
    /// = manual`).
    AwaitingOperator,
    /// Closed (stage 3).
    Closed(Outcome),
}

/// One loss in the list of losses (§11 Health, `api.md` `LossReport`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loss {
    /// The first position of its range when it was first reported; for a
    /// `possible_loss`, its recovery entry's position, F. Identities are
    /// unique within a kind (§3 Order of a start, step 4).
    pub identity: Position,
    /// Certain or possible.
    pub kind: LossKind,
    /// The cause.
    pub cause: LossCause,
    /// The first lost position, if a position is known to be lost.
    pub first: Option<Position>,
    /// The last lost position.
    pub last: Option<Position>,
    /// The tail a recovery entry reported.
    pub tail: Tail,
    /// The mark's state a recovery entry reported.
    pub mark_state: MarkState,
    /// The consumers whose cursor file was missing or unreadable.
    pub cursors_missing: Vec<ConsumerId>,
    /// Whether a manifest record or a recovery entry holds it; a range found
    /// by a start that could not write is reported as found, not recorded.
    pub recorded: bool,
    /// Whether a closing record covers its present range, by any outcome.
    /// Always false for a `possible_loss`, which needs none.
    pub acknowledged: bool,
    /// Its self-repair state; `None` for a `possible_loss`.
    pub state: Option<LossState>,
    /// Identities of recorded losses folded into it, ascending (Q-1102, Q-1108).
    pub absorbed: Vec<Position>,
}

impl Loss {
    /// Whether the loss holds health degraded (§11 Health).
    pub fn degrades(&self) -> bool {
        self.kind == LossKind::DataLost && !self.acknowledged
    }
}

/// A loss recorded in the manifest (§3 Order of a start, step 4): its
/// identity, the range a record names and the cause. A record that extends a
/// loss names its identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LostRecord {
    /// The loss's identity.
    pub identity: Position,
    /// First position.
    pub from: Position,
    /// Last position.
    pub to: Position,
    /// The cause.
    pub cause: LossCause,
    /// When the record was written.
    pub at: i64,
    /// The record's epoch.
    pub epoch: u64,
}

/// An acknowledgement or closing record's entry (§8 Acknowledging a loss,
/// Self-repair): a loss's identity, its first and last position as reported
/// then, and the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acknowledged {
    /// The loss's identity.
    pub identity: Position,
    /// First position as reported.
    pub from: Position,
    /// Last position as reported.
    pub to: Position,
    /// How it was closed.
    pub outcome: Outcome,
}

/// The list of losses (§11 Health), from the manifest's lost-range records,
/// the ranges found but not yet recorded, the recovery entries and the
/// acknowledgements. Each loss is listed once, under its identity, with the
/// union of the ranges its records name; losses whose ranges overlap are one
/// under the lowest identity, naming the others it absorbed (Q-1102). A
/// recovery entry's gap is a `data_lost` of cause truncated tail; the entry
/// itself is a `possible_loss` when it names a tail, a mark not readable or
/// stale, or a missing cursor file. A `data_lost` is closed when the latest
/// closing records of the identities folded into it cover its present range
/// (Q-1108); `stage` gives the self-repair stage of one recorded and open.
pub fn losses(
    recorded: &[LostRecord],
    found: &[(Position, Position)],
    recoveries: &[(Position, RecoveryEntry)],
    acknowledged: &[(u64, Acknowledged)],
    stage: impl Fn(&Loss) -> LossState,
) -> Vec<Loss> {
    let mut out: Vec<Loss> = Vec::new();
    let add = |out: &mut Vec<Loss>,
               identity: Position,
               cause: LossCause,
               from: Position,
               to: Position,
               recorded: bool| {
        if let Some(l) = out.iter_mut().find(|l| l.identity == identity) {
            l.first = l.first.min(Some(from));
            l.last = l.last.max(Some(to));
            l.recorded |= recorded;
        } else {
            out.push(Loss {
                identity,
                kind: LossKind::DataLost,
                cause,
                first: Some(from),
                last: Some(to),
                tail: Tail::default(),
                mark_state: MarkState::Readable,
                cursors_missing: Vec::new(),
                recorded,
                acknowledged: false,
                state: None,
                absorbed: Vec::new(),
            });
        }
    };
    for r in recorded {
        add(&mut out, r.identity, r.cause, r.from, r.to, true);
    }
    // Records of identities whose ranges overlap are one loss under the
    // lowest identity (Q-1102).
    out.sort_by_key(|l| l.first);
    let mut folded: Vec<Loss> = Vec::with_capacity(out.len());
    for l in out.drain(..) {
        match folded.last_mut() {
            Some(f) if l.first <= f.last => fold(f, l),
            _ => folded.push(l),
        }
    }
    out = folded;
    let mut found_again = Vec::new();
    for (from, to) in found {
        // A range found again before it is recorded is the same loss; one
        // over several losses makes them one under the lowest identity,
        // recorded only if one recorded loss covered it all (Q-1102).
        let (mut same, rest): (Vec<Loss>, Vec<Loss>) = out
            .drain(..)
            .partition(|l| l.first.is_some_and(|f| f <= *to) && l.last.is_some_and(|t| *from <= t));
        out = rest;
        same.sort_by_key(|l| l.identity);
        let mut it = same.into_iter();
        let mut l = it.next().unwrap_or_else(|| Loss {
            identity: *from,
            kind: LossKind::DataLost,
            cause: LossCause::Corruption,
            first: Some(*from),
            last: Some(*to),
            tail: Tail::default(),
            mark_state: MarkState::Readable,
            cursors_missing: Vec::new(),
            recorded: false,
            acknowledged: false,
            state: None,
            absorbed: Vec::new(),
        });
        let parts: Vec<_> = std::iter::once(&l)
            .chain(it.as_slice())
            .filter(|x| x.recorded)
            .map(|x| (x.first, x.last))
            .collect();
        for by in it {
            fold(&mut l, by);
        }
        l.first = l.first.min(Some(*from));
        l.last = l.last.max(Some(*to));
        l.recorded = parts.iter().any(|(a, b)| *a <= l.first && l.last <= *b);
        found_again.push(l.identity);
        out.push(l);
    }
    let restored_after = |ids: &[Position], epoch: u64, a: &Acknowledged| {
        recorded.iter().any(|r| {
            ids.contains(&r.identity) && r.epoch > epoch && r.from <= a.to && a.from <= r.to
        })
    };
    for l in &mut out {
        // Per identity folded into it, its own included, the latest closing
        // record; one "restored in full" no longer closes a loss whose
        // positions were lost again after it (§8 Self-repair stage 1). They
        // close the loss when together they cover its present range
        // (Q-1108).
        let ids: Vec<Position> = std::iter::once(l.identity)
            .chain(l.absorbed.iter().copied())
            .collect();
        let latest: Vec<(usize, &Acknowledged)> = ids
            .iter()
            .filter_map(|id| {
                acknowledged
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, (_, a))| a.identity == *id)
                    .filter(|(_, (epoch, a))| {
                        !matches!(a.outcome, Outcome::Restored(_))
                            || !(found_again.contains(&l.identity)
                                || restored_after(&ids, *epoch, a))
                    })
                    .map(|(k, (_, a))| (k, a))
            })
            .collect();
        let closed = covers(l, latest.iter().map(|(_, a)| (a.from, a.to)))
            .then(|| latest.iter().max_by_key(|(k, _)| *k).map(|(_, a)| *a))
            .flatten();
        l.acknowledged = closed.is_some();
        l.state = Some(match closed {
            Some(a) => LossState::Closed(a.outcome),
            None if l.recorded => stage(l),
            None => LossState::Found,
        });
    }
    for (at, e) in recoveries.iter().filter(|(_, e)| e.possible_loss()) {
        out.push(Loss {
            identity: *at,
            kind: LossKind::PossibleLoss,
            cause: LossCause::TruncatedTail,
            first: None,
            last: None,
            tail: e.tail,
            mark_state: e.mark_state,
            cursors_missing: e.cursors_missing.clone(),
            recorded: true,
            acknowledged: false,
            state: None,
            absorbed: Vec::new(),
        });
    }
    out.sort_by_key(|l| (l.identity, l.kind == LossKind::PossibleLoss));
    out
}

/// Folds loss `by` into `f`; the lower identity names the loss and the
/// other's recorded identity is absorbed (Q-1102, Q-1108).
fn fold(f: &mut Loss, mut by: Loss) {
    if by.identity < f.identity {
        std::mem::swap(f, &mut by);
    }
    f.first = f.first.min(by.first);
    f.last = f.last.max(by.last);
    f.absorbed.extend(by.recorded.then_some(by.identity));
    f.absorbed.extend(by.absorbed);
    let own = f.identity;
    f.absorbed.retain(|i| *i != own);
    f.absorbed.sort();
    f.absorbed.dedup();
}

/// Whether `ranges` together hold every position of `l`'s range;
/// positions follow one another within a generation.
fn covers(l: &Loss, ranges: impl Iterator<Item = (Position, Position)>) -> bool {
    let (Some(first), Some(last)) = (l.first, l.last) else {
        return false;
    };
    let mut ranges: Vec<_> = ranges.collect();
    ranges.sort();
    let mut need = first;
    for (a, b) in ranges {
        if a > need {
            return false;
        }
        if b >= last {
            return true;
        }
        if b >= need {
            let Some(s) = b.sequence.checked_add(1) else {
                return false;
            };
            need = Position::new(b.generation, s);
        }
    }
    false
}

/// A position kept for later use with the stamp of the mark it was read under
/// (§7 Readers, Reset stamp and saved points; `api.md` `SavedPoint`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedPoint {
    /// P.
    pub position: Position,
    /// S.
    pub stamp: ResetStamp,
}

/// The answer for a saved point (`api.md` `PointCheck`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointCheck {
    /// Nothing was reset since.
    Kept,
    /// The holder forgets every position above this and continues from it.
    VoidAbove(Position),
    /// The reset the mark names is not known: the holder starts again as if
    /// it kept no position.
    Unknown,
}

/// A reset as the store holds it, in the order it was written: the recovery
/// entries in log order and the restore records, each with its stamp, and the
/// lost ranges, in the order they were recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reset {
    /// A kept recovery entry: its stamp and `after`.
    Recovery {
        /// Its stamp; zero for an entry written before the stamp existed.
        stamp: ResetStamp,
        /// Its position.
        at: Position,
        /// K.
        after: Position,
    },
    /// A restore record: its stamp, its generation and its restored end.
    Restore {
        /// Its stamp.
        stamp: ResetStamp,
        /// The generation it starts.
        generation: u32,
        /// The restored end, in an earlier generation.
        restored_end: Position,
    },
    /// A lost range recorded (not one a recovery entry describes).
    Lost {
        /// First position.
        from: Position,
        /// Last position.
        to: Position,
    },
}

impl Reset {
    fn stamp(&self) -> Option<ResetStamp> {
        match self {
            Reset::Recovery { stamp, .. } | Reset::Restore { stamp, .. } => Some(*stamp),
            Reset::Lost { .. } => None,
        }
    }

    fn generation(&self) -> u32 {
        match self {
            Reset::Recovery { at, .. } => at.generation,
            Reset::Restore { generation, .. } => *generation,
            Reset::Lost { from, .. } => from.generation,
        }
    }
}

/// Judges a saved point against the current mark, of generation `generation`
/// and stamp `current`, and the resets the store holds (§7 Readers, Reset
/// stamp and saved points).
pub fn check_saved_point(
    point: SavedPoint,
    generation: u32,
    current: ResetStamp,
    resets: &[Reset],
) -> PointCheck {
    let p = point.position;
    if p.generation == generation && current == point.stamp {
        return PointCheck::Kept;
    }
    let carries = |s: ResetStamp| resets.iter().position(|r| r.stamp() == Some(s));
    if current != ResetStamp::ZERO && carries(current).is_none() {
        return PointCheck::Unknown;
    }
    let later: &[Reset] = match carries(point.stamp).filter(|_| point.stamp != ResetStamp::ZERO) {
        Some(i) => &resets[i + 1..],
        None => resets,
    };
    let mut void = p;
    for r in later {
        if r.generation() < p.generation {
            continue;
        }
        let v = match *r {
            Reset::Recovery { after, .. } => after,
            Reset::Restore { restored_end, .. } if restored_end.generation == p.generation => {
                restored_end
            }
            Reset::Restore { .. } => continue,
            Reset::Lost { from, .. } if from.generation == p.generation => before(from),
            Reset::Lost { .. } => continue,
        };
        void = void.min(v);
    }
    PointCheck::VoidAbove(void)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: u64) -> Position {
        Position::new(0, s)
    }

    fn stamp(s: u64) -> ResetStamp {
        ResetStamp {
            sequence: s,
            nonce: s + 100,
        }
    }

    fn entry(after: u64, s: u64) -> Reset {
        Reset::Recovery {
            stamp: stamp(s),
            at: p(s),
            after: p(after),
        }
    }

    #[test]
    fn same_stamp_is_kept() {
        let resets = [entry(4, 6)];
        let point = SavedPoint {
            position: p(9),
            stamp: stamp(6),
        };
        assert_eq!(
            check_saved_point(point, 0, stamp(6), &resets),
            PointCheck::Kept
        );
    }

    #[test]
    fn later_entries_and_lost_ranges_void_above_the_lowest() {
        let resets = [
            entry(4, 6),
            Reset::Lost {
                from: p(12),
                to: p(13),
            },
            entry(10, 15),
        ];
        let point = SavedPoint {
            position: p(14),
            stamp: stamp(6),
        };
        assert_eq!(
            check_saved_point(point, 0, stamp(15), &resets),
            PointCheck::VoidAbove(p(10))
        );
        // Read before any reset: every one counts, the first entry's K too.
        let point = SavedPoint {
            position: p(14),
            stamp: ResetStamp::ZERO,
        };
        assert_eq!(
            check_saved_point(point, 0, stamp(15), &resets),
            PointCheck::VoidAbove(p(4))
        );
    }

    #[test]
    fn a_stamp_no_kept_entry_carries_is_unknown() {
        let point = SavedPoint {
            position: p(3),
            stamp: ResetStamp::ZERO,
        };
        assert_eq!(
            check_saved_point(point, 0, stamp(9), &[entry(4, 6)]),
            PointCheck::Unknown
        );
    }

    #[test]
    fn a_restore_voids_above_its_restored_end_in_the_point_s_generation() {
        let resets = [Reset::Restore {
            stamp: stamp(1),
            generation: 1,
            restored_end: p(7),
        }];
        let point = SavedPoint {
            position: p(9),
            stamp: ResetStamp::ZERO,
        };
        assert_eq!(
            check_saved_point(point, 1, stamp(1), &resets),
            PointCheck::VoidAbove(p(7))
        );
    }

    #[test]
    fn a_gap_is_described_by_its_entry() {
        let e = RecoveryEntry {
            after: p(4),
            stamp: stamp(7),
            boot: 2,
            tail: Tail::default(),
            mark_state: MarkState::Readable,
            id_floor: 0,
            cursors_missing: Vec::new(),
            written_level: false,
        };
        assert_eq!(e.gap(p(7)), Some((p(5), p(6))));
        assert!(!e.possible_loss(), "a gap alone is a `data_lost`");
        let gap = LostRecord {
            identity: p(5),
            from: p(5),
            to: p(6),
            cause: LossCause::TruncatedTail,
            at: 0,
            epoch: 1,
        };
        let l = losses(&[gap], &[], &[(p(7), e.clone())], &[], |_| LossState::Found);
        assert_eq!(l.len(), 1);
        assert_eq!(
            (l[0].kind, l[0].cause, l[0].identity),
            (LossKind::DataLost, LossCause::TruncatedTail, p(5))
        );
        let rp = reset_points(&[(p(7), e)], &[(p(5), p(6)), (p(9), p(9))]);
        assert_eq!(
            rp,
            vec![
                ResetPoint {
                    id: p(7),
                    forget_above: p(4)
                },
                ResetPoint {
                    id: p(9),
                    forget_above: p(8)
                },
            ]
        );
    }

    fn record(identity: u64, from: u64, to: u64) -> LostRecord {
        LostRecord {
            identity: p(identity),
            from: p(from),
            to: p(to),
            cause: LossCause::Corruption,
            at: 0,
            epoch: 1,
        }
    }

    fn brief(records: &[LostRecord], found: &[(Position, Position)]) -> Vec<(u64, u64, u64, bool)> {
        losses(records, found, &[], &[], |_| LossState::Found)
            .iter()
            .map(|l| {
                (
                    l.identity.sequence,
                    l.first.unwrap().sequence,
                    l.last.unwrap().sequence,
                    l.recorded,
                )
            })
            .collect()
    }

    #[test]
    fn records_of_identities_whose_ranges_overlap_are_one_loss_under_the_lowest() {
        let bridge = [record(10, 10, 10), record(8, 8, 8), record(8, 8, 10)];
        assert_eq!(brief(&bridge, &[]), [(8, 8, 10, true)]);
        let under_the_higher = [record(10, 10, 10), record(8, 8, 8), record(10, 8, 10)];
        assert_eq!(brief(&under_the_higher, &[]), [(8, 8, 10, true)]);
        let touching = [record(8, 8, 9), record(10, 10, 11)];
        assert_eq!(brief(&touching, &[]), [(8, 8, 9, true), (10, 10, 11, true)]);
        assert_eq!(
            brief(&under_the_higher, &[(p(9), p(10))]),
            [(8, 8, 10, true)],
            "a range found again over the higher identity is the folded loss"
        );
    }

    #[test]
    fn a_found_range_over_two_recorded_losses_folds_them_as_the_engine_does() {
        let apart = [record(10, 10, 10), record(8, 8, 8)];
        assert_eq!(brief(&apart, &[(p(8), p(10))]), [(8, 8, 10, false)]);
        let touching = [record(8, 8, 9), record(10, 10, 11)];
        assert_eq!(brief(&touching, &[(p(9), p(10))]), [(8, 8, 11, false)]);
        assert_eq!(
            brief(&[record(8, 8, 10)], &[(p(9), p(9))]),
            [(8, 8, 10, true)]
        );
        let l = losses(&touching, &[(p(9), p(10))], &[], &[], |_| LossState::Found);
        assert_eq!(l[0].absorbed, [p(10)]);
    }

    fn ack(identity: u64, from: u64, to: u64, outcome: Outcome) -> Acknowledged {
        Acknowledged {
            identity: p(identity),
            from: p(from),
            to: p(to),
            outcome,
        }
    }

    /// The one loss's state for `records` and the closing records `acks`, in order, each at `epoch`.
    fn state(records: &[LostRecord], acks: &[(u64, Acknowledged)]) -> Option<LossState> {
        let l = losses(records, &[], &[], acks, |_| LossState::Found);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(
            l[0].acknowledged,
            matches!(l[0].state, Some(LossState::Closed(_)))
        );
        l[0].state
    }

    #[test]
    fn closing_records_of_the_identities_folded_in_carry_over_while_they_cover_the_loss() {
        let bridged = |epoch| {
            let mut b = record(8, 8, 11);
            b.epoch = epoch;
            [record(8, 8, 9), record(10, 10, 11), b]
        };
        let both = [
            (2, ack(8, 8, 9, Outcome::Operator)),
            (3, ack(10, 10, 11, Outcome::ResendEnded)),
        ];
        let closed = |o| Some(LossState::Closed(o));
        assert_eq!(state(&bridged(4), &both), closed(Outcome::ResendEnded));
        assert_eq!(state(&bridged(4), &both[..1]), Some(LossState::Found));
        assert_eq!(state(&bridged(4), &both[1..]), Some(LossState::Found));
        let gap = [record(8, 8, 8), record(10, 10, 10), record(8, 8, 10)];
        let apart = [
            (2, ack(8, 8, 8, Outcome::Operator)),
            (3, ack(10, 10, 10, Outcome::Operator)),
        ];
        assert_eq!(
            state(&gap, &apart),
            Some(LossState::Found),
            "position 9 is new"
        );
        let old_bridge = [record(10, 10, 10), record(8, 8, 8), record(10, 8, 10)];
        assert_eq!(
            state(&old_bridge, &[(3, ack(10, 8, 10, Outcome::Operator))]),
            closed(Outcome::Operator)
        );
        let restored = [
            (2, ack(8, 8, 9, Outcome::Operator)),
            (3, ack(10, 10, 11, Outcome::Restored(2))),
        ];
        assert_eq!(
            state(&bridged(4), &restored),
            Some(LossState::Found),
            "lost again after its restore"
        );
        assert_eq!(state(&bridged(1), &restored), closed(Outcome::Restored(2)));
        let mut late = record(10, 9, 11);
        late.epoch = 4;
        let under_the_higher = [record(8, 8, 9), record(10, 10, 11), late];
        assert_eq!(
            state(&under_the_higher, &restored),
            Some(LossState::Found),
            "lost again under the absorbed identity"
        );
    }
}
