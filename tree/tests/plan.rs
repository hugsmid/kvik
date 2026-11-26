//! The merge planner rule by rule (design 7.1b section 5 Merges), and the merge's refusals.

mod files;

use files::{DAY, T0, node, part_header, pos};
use kvik_log::header::FileHeader;
use kvik_log::mark::ResetStamp;
use kvik_tree::part::{Class, Entry, Part, Run};
use kvik_tree::root::{self, Covered, PartRef, Root};
use kvik_tree::set::{MergeError, Plan, Policy, merge, plan};
use kvik_tree::{Kind, LEVELS, Level, MINUTE, Params};

const HOUR: i64 = 60 * MINUTE;
const WEEK: i64 = 7 * DAY;
const POLICY: Policy = Policy {
    late_window: DAY,
    minute_keep: 7 * DAY,
};

fn pref(file_id: u64, seq: u64, class: Class, start: i64, len: i64) -> PartRef {
    PartRef {
        file_id,
        seq,
        class,
        file_crc32c: 0,
        period_start: start,
        period_len: len as u64,
        least_start: start,
        greatest_end: start + len.min(HOUR),
        least_series: 1,
        greatest_series: 1,
        byte_len: 1_000,
    }
}

fn root_with(parts: Vec<PartRef>, covered: Vec<Covered>) -> Root {
    Root {
        header: FileHeader::new(root::KIND, 3, 50, T0, files::STORE),
        seq: 50,
        checkpoint: pos(3, 1_000),
        reset: ResetStamp {
            sequence: 1,
            nonce: 1,
        },
        next_file_id: 500,
        floor: 0,
        levels: LEVELS.to_vec(),
        parts,
        covered,
        floors: Vec::new(),
        retired: Vec::new(),
    }
}

fn all(level: u8, from: i64, to: i64) -> Covered {
    Covered { level, from, to }
}

fn deltas(n: u64, day: i64) -> Vec<PartRef> {
    (1..=n)
        .map(|i| pref(i, i, Class::Delta, day + i as i64 * HOUR, HOUR))
        .collect()
}

#[test]
fn a_delta_crossing_a_day_boundary_is_split_by_day() {
    let mut p = pref(1, 1, Class::Delta, T0 + DAY - HOUR, 2 * HOUR);
    p.greatest_end = T0 + DAY + HOUR;
    let plans = plan(&root_with(vec![p], Vec::new()), T0 + DAY, &POLICY);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].inputs, vec![1]);
    assert_eq!(
        plans[0].periods,
        vec![(T0, DAY as u64), (T0 + DAY, DAY as u64)]
    );
    assert!(!plans[0].rollups);
}

#[test]
fn a_part_of_the_same_seq_blocks_a_split_only_when_its_series_overlap() {
    let mut a = pref(1, 5, Class::Delta, T0 + DAY - HOUR, 2 * HOUR);
    a.greatest_end = T0 + DAY + HOUR;
    let mut b = pref(2, 5, Class::Delta, T0 + DAY - HOUR, HOUR);
    b.least_series = 2;
    b.greatest_series = 2;
    let plans = plan(&root_with(vec![a, b], Vec::new()), T0 + DAY, &POLICY);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].inputs, vec![1]);
    b.least_series = 1;
    assert!(plan(&root_with(vec![a, b], Vec::new()), T0 + DAY, &POLICY).is_empty());
}

#[test]
fn eight_deltas_of_a_day_merge_whatever_their_age_and_seven_young_ones_wait() {
    let now = T0 + 3 * HOUR;
    let p = plan(&root_with(deltas(8, T0), Vec::new()), now, &POLICY);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].inputs, (1..=8).collect::<Vec<_>>());
    assert_eq!(p[0].periods, vec![(T0, DAY as u64)]);
    assert!(plan(&root_with(deltas(7, T0), Vec::new()), now, &POLICY).is_empty());
}

#[test]
fn two_deltas_merge_once_the_oldest_ends_two_hours_before_now() {
    let r = root_with(deltas(2, T0), Vec::new());
    let oldest_end = T0 + 2 * HOUR;
    assert!(plan(&r, oldest_end + 2 * HOUR - 1, &POLICY).is_empty());
    assert_eq!(
        plan(&r, oldest_end + 2 * HOUR, &POLICY)[0].inputs,
        vec![1, 2]
    );
}

#[test]
fn a_delta_between_the_seqs_of_others_breaks_their_run() {
    let mut parts = deltas(9, T0);
    parts[2].class = Class::Merged;
    let p = plan(&root_with(parts, Vec::new()), T0 + 10 * HOUR, &POLICY);
    assert_eq!(p[0].inputs, vec![4, 5, 6, 7, 8, 9]);
}

#[test]
fn a_day_past_its_late_window_gets_its_day_part_with_rollups() {
    let cov = vec![all(2, T0, T0 + DAY)];
    let r = root_with(deltas(1, T0), cov);
    assert!(plan(&r, T0 + 2 * DAY - 1, &POLICY).is_empty());
    let p = plan(&r, T0 + 2 * DAY, &POLICY);
    assert_eq!(p.len(), 1);
    assert!(p[0].rollups);
    assert_eq!(p[0].periods, vec![(T0, DAY as u64)]);
}

#[test]
fn a_merged_day_overlaid_by_five_parts_is_merged_again_and_by_four_is_not() {
    let cov = vec![
        all(2, T0, T0 + DAY),
        all(3, T0, T0 + DAY),
        all(4, T0, T0 + DAY),
    ];
    let day = pref(100, 1, Class::Merged, T0, DAY);
    let late = |n: u64| {
        let mut v = vec![day];
        v.extend((0..n).map(|i| pref(10 + i, 10 + i, Class::Delta, T0 + i as i64 * HOUR, HOUR)));
        v.sort_by_key(|r| r.file_id);
        v
    };
    let now = T0 + 3 * DAY;
    let young = |r: Root| {
        plan(
            &r,
            now,
            &Policy {
                late_window: DAY,
                ..POLICY
            },
        )
    };
    let p = young(root_with(late(5), cov.clone()));
    assert_eq!(p.len(), 1);
    assert!(p[0].rollups && p[0].inputs.contains(&100) && p[0].inputs.len() == 6);
    let p = young(root_with(late(4), cov));
    assert!(p.iter().all(|x| !x.inputs.contains(&100)), "{p:?}");
}

#[test]
fn a_week_of_day_parts_merges_two_weeks_after_it_ends() {
    let monday = T0 + 4 * DAY;
    let cov = vec![
        all(2, monday, monday + WEEK),
        all(3, monday, monday + WEEK),
        all(4, monday, monday + WEEK),
    ];
    let days: Vec<PartRef> = (0..3)
        .map(|i| pref(i + 1, i + 1, Class::Merged, monday + i as i64 * DAY, DAY))
        .collect();
    let r = root_with(days, cov);
    assert!(plan(&r, monday + 3 * WEEK - 1, &POLICY).is_empty());
    let p = plan(&r, monday + 3 * WEEK, &POLICY);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].inputs, vec![1, 2, 3]);
    assert_eq!(p[0].periods, vec![(monday, WEEK as u64)]);
    assert!(p[0].rollups);
}

#[test]
fn without_rollup_levels_a_day_never_waits_for_a_day_part() {
    let mut r = root_with(deltas(1, T0), vec![all(2, T0, T0 + DAY)]);
    r.levels = vec![Level {
        id: 2,
        width: 15 * MINUTE,
    }];
    assert!(plan(&r, T0 + 30 * DAY, &POLICY).is_empty());
}

#[test]
fn the_planner_reads_level_widths_from_the_table() {
    let mut r = root_with(deltas(1, T0), vec![all(2, T0, T0 + DAY)]);
    r.levels = vec![
        Level {
            id: 2,
            width: 15 * MINUTE,
        },
        Level {
            id: 7,
            width: 6 * HOUR,
        },
    ];
    assert_eq!(plan(&r, T0 + 3 * DAY, &POLICY).len(), 1);
    r.covered.push(all(7, T0, T0 + DAY));
    assert!(plan(&r, T0 + 3 * DAY, &POLICY).is_empty());
}

fn quarter_part(file_id: u64, seq: u64, q: i64) -> Part {
    let start = T0 + q * 15 * MINUTE;
    Part {
        header: part_header(file_id),
        seq,
        class: Class::Delta,
        period_start: T0,
        period_len: DAY as u64,
        through: pos(3, seq),
        levels: LEVELS.to_vec(),
        runs: vec![Run {
            series: 1,
            level: 2,
            entries: vec![Entry {
                start,
                stale: false,
                as_of: pos(3, seq),
                node: Some(node(Kind::Gauge, start, 15 * MINUTE, 2, seq as u32)),
            }],
        }],
    }
}

fn listed(parts: &[Part]) -> Root {
    root_with(
        parts
            .iter()
            .map(|p| kvik_tree::set::part_ref(p, &p.encode().unwrap()))
            .collect(),
        vec![all(2, T0, T0 + DAY)],
    )
}

fn try_merge(
    parts: &[Part],
    inputs: Vec<u64>,
    periods: Vec<(i64, u64)>,
    rollups: bool,
) -> Result<Vec<Part>, MergeError> {
    let plan = Plan {
        inputs,
        periods,
        rollups,
        minute_from: i64::MIN,
    };
    merge(&listed(parts), parts, &plan, &part_header(0), |_| {
        Some(Params::new(Kind::Gauge, None))
    })
}

#[test]
fn merge_refuses_a_part_between_the_inputs_seqs() {
    let parts = [
        quarter_part(1, 1, 0),
        quarter_part(2, 2, 1),
        quarter_part(3, 3, 2),
    ];
    let day = vec![(T0, DAY as u64)];
    assert_eq!(
        try_merge(&parts, vec![1, 3], day.clone(), false),
        Err(MergeError::Interleaved(2))
    );
    assert_eq!(
        try_merge(&parts, vec![2, 3], day.clone(), false)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        try_merge(&parts, vec![2, 3], day, true),
        Err(MergeError::Incomplete)
    );
}

#[test]
fn merge_refuses_malformed_plans_and_parts_it_cannot_take() {
    let parts = [quarter_part(1, 1, 0), quarter_part(2, 2, 1)];
    let day = vec![(T0, DAY as u64)];
    assert_eq!(
        try_merge(&parts, vec![], day.clone(), false),
        Err(MergeError::Plan)
    );
    assert_eq!(
        try_merge(&parts, vec![1], vec![(T0, 0)], false),
        Err(MergeError::Plan)
    );
    assert_eq!(
        try_merge(
            &parts,
            vec![1],
            vec![(T0, DAY as u64), (T0 + HOUR, 1)],
            false
        ),
        Err(MergeError::Plan)
    );
    assert_eq!(
        try_merge(&parts, vec![1, 1], day.clone(), false),
        Err(MergeError::NotLive(1))
    );
    assert_eq!(
        try_merge(&parts, vec![9], day, false),
        Err(MergeError::NotLive(9))
    );
    assert_eq!(
        try_merge(&parts, vec![2], vec![(T0 + HOUR, HOUR as u64)], false),
        Err(MergeError::Outside {
            series: 1,
            level: 2,
            start: T0 + 15 * MINUTE
        })
    );
    let mut odd = parts.clone();
    odd[0].levels[1].width = 30 * MINUTE;
    let r = listed(&parts);
    let plan = Plan {
        inputs: vec![1],
        periods: vec![(T0, DAY as u64)],
        rollups: false,
        minute_from: i64::MIN,
    };
    assert_eq!(
        merge(&r, &odd, &plan, &part_header(0), |_| None),
        Err(MergeError::Levels(1))
    );
}

#[test]
fn merge_refuses_a_rollup_over_a_node_of_another_kind() {
    let parts = [quarter_part(1, 1, 0)];
    let plan = Plan {
        inputs: vec![1],
        periods: vec![(T0, DAY as u64)],
        rollups: true,
        minute_from: i64::MIN,
    };
    assert_eq!(
        merge(&listed(&parts), &parts, &plan, &part_header(0), |_| Some(
            Params::new(Kind::Counter, None)
        )),
        Err(MergeError::Kind(1))
    );
}

#[test]
fn a_merge_takes_the_inputs_highest_seq_and_through_and_fresh_file_ids() {
    let parts = [quarter_part(1, 4, 0), quarter_part(2, 6, 1)];
    let out = try_merge(&parts, vec![1, 2], vec![(T0, DAY as u64)], true).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].seq, out[0].through, out[0].header.file_id),
        (6, pos(3, 6), 500)
    );
    assert_eq!(out[0].class, Class::Merged);
    let levels: Vec<u8> = out[0].runs.iter().map(|r| r.level).collect();
    assert_eq!(levels, vec![2, 3, 4]);
}
