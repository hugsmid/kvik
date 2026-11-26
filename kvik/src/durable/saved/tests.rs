use kvik_log::entries::{MarkState, Recovery};
use kvik_log::manifest::{LossCause, LostRange};
use kvik_log::writer::Position as LogPos;
use kvik_model::recovery as m;
use proptest::prelude::*;

use super::{Reset, describes, judge, resets};
use crate::types::{PointCheck, Position, ResetStamp, SavedPoint};

fn p(s: u64) -> LogPos {
    LogPos {
        generation: 0,
        sequence: s,
    }
}

fn gp(generation: u32, sequence: u64) -> LogPos {
    LogPos {
        generation,
        sequence,
    }
}

fn stamp(s: u64) -> ResetStamp {
    ResetStamp {
        sequence: s,
        nonce: s + 100,
    }
}

fn log_stamp(s: ResetStamp) -> kvik_log::mark::ResetStamp {
    kvik_log::mark::ResetStamp {
        sequence: s.sequence,
        nonce: s.nonce,
    }
}

fn entry(after: u64, s: u64) -> Reset {
    Reset::Recovery {
        stamp: Some(stamp(s)),
        at: p(s),
        after: p(after),
    }
}

fn point(at: LogPos, stamp: ResetStamp) -> SavedPoint {
    SavedPoint {
        position: Position::new(at.generation, at.sequence),
        stamp,
    }
}

fn void(at: LogPos) -> PointCheck {
    PointCheck::VoidAbove(Position::new(at.generation, at.sequence))
}

fn recovery(after: u64, s: Option<ResetStamp>) -> Recovery {
    Recovery {
        after,
        stamp: s.map(log_stamp),
        boot_id: [0; 16],
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: MarkState::Readable,
        id_floor: 0,
        consumers: Vec::new(),
    }
}

fn lost(from: LogPos, to: LogPos, cause: LossCause, s: Option<ResetStamp>) -> LostRange {
    LostRange {
        identity: (from.generation, from.sequence),
        first: (from.generation, from.sequence),
        last: (to.generation, to.sequence),
        file_id: 1,
        offset: 0,
        length: 0,
        cause,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: s.map(log_stamp),
    }
}

#[test]
fn same_stamp_is_kept() {
    assert_eq!(
        judge(point(p(9), stamp(6)), 0, stamp(6), &[entry(4, 6)]),
        PointCheck::Kept
    );
}

#[test]
fn later_entries_and_lost_ranges_void_above_the_lowest() {
    let r = [
        entry(4, 6),
        Reset::Lost {
            from: p(12),
            to: p(13),
        },
        entry(10, 15),
    ];
    assert_eq!(judge(point(p(14), stamp(6)), 0, stamp(15), &r), void(p(10)));
    assert_eq!(
        judge(point(p(14), ResetStamp::default()), 0, stamp(15), &r),
        void(p(4))
    );
}

#[test]
fn a_stamp_no_kept_entry_carries_is_unknown() {
    assert_eq!(
        judge(
            point(p(3), ResetStamp::default()),
            0,
            stamp(9),
            &[entry(4, 6)]
        ),
        PointCheck::Unknown
    );
}

#[test]
fn a_restore_voids_above_its_restored_end_in_the_point_s_generation() {
    let r = [Reset::Restore {
        stamp: stamp(1),
        generation: 1,
        restored_end: p(7),
    }];
    assert_eq!(
        judge(point(p(9), ResetStamp::default()), 1, stamp(1), &r),
        void(p(7))
    );
}

#[test]
fn a_lost_range_recorded_under_a_later_stamp_counts_below_the_later_entry() {
    // Section 2.2 case 1: point 100 under S; entry at 120 under S'; a lost range 40–50 recorded under S'.
    let (s, s2) = (stamp(60), stamp(120));
    let (r60, r120) = (recovery(59, Some(s)), recovery(110, Some(s2)));
    let rs = resets(
        &[(p(60), &r60), (p(120), &r120)],
        &[lost(p(40), p(50), LossCause::Corruption, Some(s2))],
    );
    assert_eq!(judge(point(p(100), s), 0, s2, &rs), void(p(39)));
}

#[test]
fn a_lost_range_recorded_before_the_point_s_reset_does_not_count() {
    // Section 2.2 case 2.
    let (s0, s, s2) = (stamp(30), stamp(60), stamp(120));
    let (r30, r60, r120) = (
        recovery(29, Some(s0)),
        recovery(59, Some(s)),
        recovery(119, Some(s2)),
    );
    let rs = resets(
        &[(p(30), &r30), (p(60), &r60), (p(120), &r120)],
        &[lost(p(40), p(50), LossCause::Corruption, Some(s0))],
    );
    assert_eq!(judge(point(p(100), s), 0, s2, &rs), void(p(100)));
}

#[test]
fn resets_of_an_earlier_generation_are_skipped() {
    // Section 2.2 case 3.
    let r = [
        Reset::Recovery {
            stamp: Some(stamp(50)),
            at: gp(1, 50),
            after: gp(1, 3),
        },
        Reset::Recovery {
            stamp: Some(stamp(20)),
            at: gp(2, 20),
            after: gp(2, 8),
        },
    ];
    let pt = point(gp(2, 10), ResetStamp::default());
    assert_eq!(judge(pt, 2, stamp(20), &r), void(gp(2, 8)));
}

#[test]
fn a_stamp_with_a_sequence_and_a_zero_nonce_is_compared_whole() {
    // Section 2.2 case 4.
    let current = ResetStamp {
        sequence: 5,
        nonce: 0,
    };
    let pt = point(p(10), ResetStamp::default());
    assert_eq!(judge(pt, 0, current, &[entry(4, 9)]), PointCheck::Unknown);
}

#[test]
fn a_point_judged_after_a_restore_is_bounded_by_the_restored_end() {
    // Section 2.2 case 5: never `Kept` across generations, the restored end rather than an entry's K.
    let r = [
        entry(40, 50),
        Reset::Restore {
            stamp: stamp(1),
            generation: 1,
            restored_end: p(30),
        },
    ];
    assert_eq!(judge(point(p(45), stamp(50)), 1, stamp(1), &r), void(p(30)));
}

#[test]
fn an_entry_without_a_stamp_counts_as_zero() {
    let r = [Reset::Recovery {
        stamp: None,
        at: p(9),
        after: p(5),
    }];
    let zero = ResetStamp::default();
    assert_eq!(judge(point(p(7), zero), 0, zero, &r), PointCheck::Kept);
    assert_eq!(judge(point(p(7), zero), 1, zero, &r), void(p(5)));
}

#[test]
fn the_gap_record_of_the_carrying_entry_voids_nothing_saved_under_it() {
    let (s, s2) = (stamp(12), stamp(30));
    let (r12, r30) = (recovery(9, Some(s)), recovery(29, Some(s2)));
    let rs = resets(
        &[(p(12), &r12), (p(30), &r30)],
        &[lost(p(10), p(11), LossCause::TruncatedTail, Some(s))],
    );
    assert_eq!(rs.len(), 2);
    assert_eq!(judge(point(p(20), s), 0, s2, &rs), void(p(20)));
}

#[test]
fn only_a_truncated_tail_exactly_the_gap_is_described() {
    let at = p(12);
    let after = p(9);
    let tail = |a, b, c| lost(p(a), p(b), c, None);
    assert!(describes(
        at,
        after,
        &tail(10, 11, LossCause::TruncatedTail)
    ));
    assert!(!describes(at, after, &tail(10, 11, LossCause::Corruption)));
    assert!(!describes(
        at,
        after,
        &tail(10, 10, LossCause::TruncatedTail)
    ));
    assert!(!describes(
        at,
        after,
        &tail(11, 11, LossCause::TruncatedTail)
    ));
    assert!(!describes(
        gp(1, 12),
        after,
        &tail(10, 11, LossCause::TruncatedTail)
    ));
}

#[test]
fn lost_ranges_are_placed_after_the_reset_their_stamp_names_in_recorded_order() {
    let (s, s2) = (stamp(10), stamp(20));
    let (r10, r20) = (recovery(8, Some(s)), recovery(18, Some(s2)));
    let c = LossCause::Corruption;
    let rs = resets(
        &[(p(10), &r10), (p(20), &r20)],
        &[
            lost(p(30), p(30), c, None),
            lost(p(3), p(3), c, Some(s)),
            lost(p(4), p(4), c, Some(ResetStamp::default())),
            lost(p(5), p(5), c, Some(s)),
            lost(p(6), p(6), c, Some(stamp(99))),
        ],
    );
    let l = |a| Reset::Lost {
        from: p(a),
        to: p(a),
    };
    let e = |at: u64, after: u64, st| Reset::Recovery {
        stamp: Some(st),
        at: p(at),
        after: p(after),
    };
    assert_eq!(
        rs,
        vec![l(4), e(10, 8, s), l(3), l(5), e(20, 18, s2), l(30), l(6)]
    );
}

fn mp(p: LogPos) -> kvik_model::types::Position {
    kvik_model::types::Position::new(p.generation, p.sequence)
}

fn ms(s: ResetStamp) -> m::ResetStamp {
    m::ResetStamp {
        sequence: s.sequence,
        nonce: s.nonce,
    }
}

fn to_model(r: &Reset) -> m::Reset {
    match *r {
        Reset::Recovery { stamp, at, after } => m::Reset::Recovery {
            stamp: ms(stamp.unwrap_or_default()),
            at: mp(at),
            after: mp(after),
        },
        Reset::Restore {
            stamp,
            generation,
            restored_end,
        } => m::Reset::Restore {
            stamp: ms(stamp),
            generation,
            restored_end: mp(restored_end),
        },
        Reset::Lost { from, to } => m::Reset::Lost {
            from: mp(from),
            to: mp(to),
        },
    }
}

fn from_model(c: m::PointCheck) -> PointCheck {
    match c {
        m::PointCheck::Kept => PointCheck::Kept,
        m::PointCheck::VoidAbove(x) => {
            PointCheck::VoidAbove(Position::new(x.generation, x.sequence))
        }
        m::PointCheck::Unknown => PointCheck::Unknown,
    }
}

const POOL: usize = 5;

/// A stamp from the pool, zero, one no reset carries, or one with a sequence and a zero nonce.
fn arb_stamp() -> impl Strategy<Value = ResetStamp> {
    prop_oneof![
        3 => (0..POOL as u64).prop_map(|i| stamp(10 * i + 1)),
        1 => Just(ResetStamp::default()),
        1 => Just(stamp(999)),
        1 => Just(ResetStamp { sequence: 11, nonce: 0 }),
    ]
}

fn arb_pos() -> impl Strategy<Value = LogPos> {
    (0u32..3, 0u64..40).prop_map(|(g, s)| gp(g, s))
}

fn arb_reset() -> impl Strategy<Value = Reset> {
    prop_oneof![
        4 => (proptest::option::weighted(0.9, arb_stamp()), arb_pos(), 0u64..40).prop_map(|(stamp, at, k)| {
            Reset::Recovery { stamp, at, after: gp(at.generation, k.min(at.sequence.saturating_sub(1))) }
        }),
        1 => (arb_stamp(), 0u32..3, arb_pos()).prop_map(|(stamp, generation, restored_end)| {
            Reset::Restore { stamp, generation, restored_end }
        }),
        3 => (arb_pos(), 0u64..4).prop_map(|(from, n)| Reset::Lost {
            from,
            to: gp(from.generation, from.sequence + n),
        }),
    ]
}

proptest! {
    /// S1: for any reset list, the engine's answer is the model's.
    #[test]
    fn s1_the_check_gives_the_model_s_answer_on_any_reset_list(
        rs in proptest::collection::vec(arb_reset(), 0..9),
        at in arb_pos(),
        saved in arb_stamp(),
        generation in 0u32..3,
        current in arb_stamp(),
    ) {
        let model: Vec<m::Reset> = rs.iter().map(to_model).collect();
        let pt = point(at, saved);
        let want = from_model(m::check_saved_point(
            m::SavedPoint { position: mp(at), stamp: ms(saved) },
            generation,
            ms(current),
            &model,
        ));
        prop_assert_eq!(judge(pt, generation, current, &rs), want);
    }
}
