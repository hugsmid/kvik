use std::collections::BTreeSet;

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::setaside::SetAside;

use super::super::run::run;
use super::super::{Cause, RunError, entry_digest};
use super::{
    DAY, batch, cause, check_lossless, failure, input, put_segment, reading, segment_bytes,
    temp_dir,
};

#[test]
fn a_failed_write_removes_the_outputs_written_before_it() {
    let dir = temp_dir("leak");
    let segs = [batch(
        1,
        30 * DAY,
        vec![
            reading(1, 29 * DAY, Value::F64(1)),
            reading(1, 28 * DAY, Value::F64(2)),
        ],
    )];
    let c = put_segment(&dir, 0, 1, &segment_bytes(0, 1, &segs));
    let data = dir.join("data");
    // A directory under the second output's `.tmp` name makes its write fail.
    std::fs::create_dir_all(data.join(format!("{:08x}-{:016x}.kvc.tmp", 1, 101))).unwrap();
    let (sa, weeks) = (SetAside::new(vec![]), BTreeSet::new());
    let e = run(&input(&dir, &sa, &weeks), &[c]).unwrap_err();
    assert!(matches!(e, RunError::Io(_)), "{e:?}");
    let first = data.join(format!("{:08x}-{:016x}.kvc.tmp", 1, 100));
    assert!(!first.exists(), "the first output is left behind");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Runs over `bytes` twice and checks the same stall comes back each time and is named permanent (Q-1520, Q-1521).
fn permanent_stall(segs: Vec<(u32, u64, Vec<u8>)>, expected: (Cause, u64)) {
    for _ in 0..2 {
        let e = failure(segs.clone(), &SetAside::new(vec![]));
        assert_eq!(cause(&e), Some(expected), "{e:?}");
        assert!(e.to_string().contains("permanent"), "{e}");
    }
}

#[test]
fn a_time_whose_period_leaves_i64_stalls_permanently() {
    let a = segment_bytes(
        0,
        1,
        &[batch(
            1,
            30 * DAY,
            vec![reading(1, i64::MAX, Value::F64(1))],
        )],
    );
    permanent_stall(vec![(0, 1, a)], (Cause::PeriodOutOfRange, 1));
}

#[test]
fn commit_times_that_decrease_inside_one_segment_stall_permanently() {
    let a = segment_bytes(
        0,
        1,
        &[
            batch(1, 30 * DAY, vec![reading(1, 5, Value::F64(1))]),
            batch(2, 29 * DAY, vec![reading(1, 6, Value::F64(1))]),
        ],
    );
    permanent_stall(vec![(0, 1, a)], (Cause::CommitTimeDecreases, 1));
}

#[test]
fn a_damaged_segment_is_not_named_a_permanent_stall() {
    let mut a = segment_bytes(
        0,
        1,
        &[batch(1, 30 * DAY, vec![reading(1, 5, Value::F64(1))])],
    );
    a[200] ^= 0xFF;
    let e = failure(vec![(0, 1, a)], &SetAside::new(vec![]));
    assert!(!e.to_string().contains("permanent"), "{e}");
}

fn changed(series_id: u32, t: i64, changed_at: u64, v: u64) -> Item {
    Item::Fact(FactRow {
        series_id,
        t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(v),
        changed_at: Some(changed_at),
    })
}

#[test]
fn the_earlier_position_with_the_higher_changed_at_is_the_current_row() {
    let segs = vec![(
        0,
        vec![
            batch(1, 30 * DAY, vec![changed(1, 29 * DAY, 9, 1)]),
            batch(2, 30 * DAY, vec![changed(1, 29 * DAY, 3, 2)]),
        ],
    )];
    check_lossless(&segs, &SetAside::new(vec![]), &BTreeSet::new());
}

#[test]
fn a_segment_whose_file_checksum_disagrees_with_its_seal_stalls() {
    let mut a = segment_bytes(
        0,
        1,
        &[batch(1, 30 * DAY, vec![reading(1, 5, Value::F64(1))])],
    );
    // A byte the file checksum covers and no block checksum does, found by flipping each byte of this segment.
    a[270] ^= 1;
    let e = failure(vec![(0, 1, a)], &SetAside::new(vec![]));
    assert_eq!(
        cause(&e),
        Some((Cause::Unreadable("not sealed whole"), 1)),
        "{e:?}"
    );
}

#[test]
fn the_entry_digest_covers_the_commit_time() {
    let e = Entry::Tzdata {
        version: "2026a".into(),
    };
    assert_ne!(
        entry_digest((0, 1), &e, 5, 10).unwrap(),
        entry_digest((0, 1), &e, 5, 11).unwrap()
    );
}
