use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;

use kvik_log::fs::{FaultOp, MemFs};
use proptest::prelude::*;

use super::tests::{Current, S, SERIES, Stop, World, id, params, pos};
use super::*;
use crate::node::Value;
use crate::set::{self, DAY, Policy, QUARTER, Seen, settled_period, visible};

const POLICY: Policy = Policy {
    late_window: DAY,
    minute_keep: 7 * DAY,
};

fn live_parts(f: &TreeFiles) -> Vec<Part> {
    f.root()
        .parts
        .iter()
        .map(|r| f.load(r).expect("a sound live part"))
        .collect()
}

/// The rebuild of `series` from the current facts `cur`: every entry `keep` allows, in the part of `period` or else
/// of its settled period.
fn rebuild_of(
    w: &World,
    cur: &Current,
    series: impl IntoIterator<Item = u64>,
    period: Option<(i64, u64)>,
    keep: impl Fn(u8, i64, i64) -> bool,
) -> Rebuild {
    let mut r = Rebuild::default();
    let (a, b) = period.map_or((i64::MIN, i64::MAX), |(s, l)| (s, s + l as i64));
    for s in series {
        let facts = World::facts_in(cur, s, a, b);
        for (l, e) in series_entries(&LEVELS, &folded_levels(&LEVELS), &params(), &facts, &keep) {
            let p = period.unwrap_or_else(|| settled_period(e.start, w.wall, &POLICY).unwrap());
            r.add(p, s, l, e);
        }
    }
    r
}

fn merge(w: &mut World, days: i64) -> Result<usize, WriteError> {
    w.tick();
    w.wall += days * DAY;
    let (wall, now) = (w.wall, w.now);
    w.files
        .as_mut()
        .unwrap()
        .merge_due(wall, &POLICY, |_| Some(params()), 16, wall, now)
}

/// A node-changing entry for `s`: a checkpoint opens its floor, then its rebuild closes it.
fn rebuild_series(w: &mut World, s: u64) -> Result<(), WriteError> {
    w.checkpoint_with(&BTreeSet::from([s]))?;
    close_floors(w)
}

fn close_floors(w: &mut World) -> Result<(), WriteError> {
    let f = w.files.as_ref().unwrap();
    let root = f.root().clone();
    let open: Vec<u64> = root
        .floors
        .iter()
        .filter(|x| x.rebuilding)
        .map(|x| x.series)
        .collect();
    if open.is_empty() {
        return Ok(());
    }
    let cur = w.current(root.checkpoint);
    let mut r = rebuild_of(w, &cur, open.iter().copied(), None, |l, a, b| {
        set::covered(&root, l, a, b)
    });
    r.close = open;
    let (wall, now) = (w.wall, w.now);
    w.files
        .as_mut()
        .unwrap()
        .rebuild_newest_first(root.checkpoint, r, |_| Vec::new(), wall, now)
}

/// After a checkpoint, as the job takes one before any repair or merge (Q-2110), damages one live part; the scrub
/// finds it and its period is rebuilt from facts.
fn damage(w: &mut World, pick: usize) -> Result<bool, WriteError> {
    w.checkpoint()?;
    let f = w.files.as_ref().unwrap();
    let root = f.root().clone();
    if root.parts.is_empty() {
        return Ok(false);
    }
    let r = root.parts[pick % root.parts.len()];
    let path = f.dir().join(part_name(1, r.file_id));
    let mid = (r.byte_len / 2) as usize;
    let byte = w.fs.read(&path).unwrap()[mid];
    w.fs.poke(&path, mid, &[byte ^ 0x5a]);
    w.renamed.insert(path.clone(), w.fs.read(&path).unwrap());
    assert_eq!(
        f.scrub(),
        vec![r.file_id],
        "the scrub finds the damaged part"
    );
    repair(w)?;
    Ok(true)
}

fn repair(w: &mut World) -> Result<(), WriteError> {
    loop {
        let f = w.files.as_ref().unwrap();
        let Some(&bad) = f.scrub().first() else {
            return Ok(());
        };
        let (period, retire) = f.damaged_period(bad).unwrap();
        let root = f.root().clone();
        let cur = w.current(root.checkpoint);
        let served = (0..SERIES).filter(|&s| set::served(&root, s));
        let mut r = rebuild_of(w, &cur, served, Some(period), |l, a, b| {
            set::covered(&root, l, a, b)
        });
        r.retire = retire;
        let (wall, now) = (w.wall, w.now);
        w.files
            .as_mut()
            .unwrap()
            .rebuild(root.checkpoint, r, wall, now)?;
    }
}

/// T3: the visible state equals that of a tree rebuilt from the log at K, `as_of` included, wherever both cover.
fn t3(w: &World) {
    let Some(f) = w.files.as_ref() else { return };
    let root = f.root().clone();
    if w.fs.read(&f.dir().join(root_name(root.seq))).is_none() {
        return;
    }
    let live = visible(&root, &live_parts(f)).unwrap();
    let k = root.checkpoint;
    let (mut g, _) = TreeFiles::start(
        Arc::new(MemFs::new()),
        &w.dir,
        id(),
        Opening {
            mark: k,
            fresh: false,
            wall: w.wall,
            now: w.now,
        },
        |_| false,
    )
    .unwrap();
    g.checkpoint(k, k, BTreeMap::new(), &BTreeSet::new(), w.wall, w.now)
        .unwrap();
    let cover = backfill_cover(&LEVELS, None, None, w.wall, &POLICY);
    let probe = Root {
        covered: cover.clone(),
        ..g.root().clone()
    };
    let cur = w.current(k);
    let mut r = rebuild_of(w, &cur, 0..SERIES, None, |l, a, b| {
        set::covered(&probe, l, a, b)
    });
    r.cover = cover;
    g.rebuild(k, r, w.wall, w.now).unwrap();
    let rebuilt = visible(g.root(), &live_parts(&g)).unwrap();
    let both = |m: BTreeMap<set::Key, Seen>| -> BTreeMap<set::Key, Seen> {
        m.into_iter()
            .filter(|&((s, l, start), _)| {
                let wd = LEVELS.iter().find(|x| x.id == l).unwrap().width;
                set::served(&root, s)
                    && set::covered(&root, l, start, start + wd)
                    && set::covered(g.root(), l, start, start + wd)
            })
            .collect()
    };
    let (live, rebuilt) = (both(live), both(rebuilt));
    let diff: Vec<_> = live
        .keys()
        .chain(rebuilt.keys())
        .filter(|x| live.get(x) != rebuilt.get(x))
        .map(|x| {
            (
                x,
                live.get(x).map(|s| s.as_of),
                rebuilt.get(x).map(|s| s.as_of),
            )
        })
        .collect();
    assert!(
        diff.is_empty(),
        "T3 at root {} (K {k:?}): {diff:?}",
        root.seq
    );
}

#[derive(Clone, Debug)]
enum Act {
    Checkpoint,
    Merge(i64),
    Rebuild(u64),
}

#[derive(Clone, Debug)]
enum Op {
    Facts(Vec<(u64, i64, Option<f64>)>),
    Sync,
    Act(Act),
    Damage(usize),
    Stopped { at: usize, stop: Stop, act: Act },
}

fn fact() -> impl Strategy<Value = (u64, i64, Option<f64>)> {
    (
        0..SERIES,
        0..9i64,
        0..300i64,
        prop::option::weighted(0.8, -40..40i32),
    )
        .prop_map(|(s, d, k, v)| {
            (
                s,
                d * DAY - 3_600 * S + k * 47 * S,
                v.map(|v| f64::from(v) / 4.0),
            )
        })
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        3 => Just(Act::Checkpoint),
        2 => prop_oneof![0..3i64, Just(20i64)].prop_map(Act::Merge),
        1 => (0..SERIES).prop_map(Act::Rebuild),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    let stop = prop_oneof![
        Just(Stop::Kill),
        Just(Stop::Power),
        any::<u64>().prop_map(Stop::Partial),
        (0..4usize).prop_map(Stop::FailSync),
        Just(Stop::FullDisk),
    ];
    prop_oneof![
        4 => prop::collection::vec(fact(), 1..12).prop_map(Op::Facts),
        2 => Just(Op::Sync),
        4 => act().prop_map(Op::Act),
        1 => any::<usize>().prop_map(Op::Damage),
        2 => (0..30usize, stop, act()).prop_map(|(at, stop, act)| Op::Stopped { at, stop, act }),
    ]
}

fn perform(w: &mut World, a: &Act) -> Result<(), WriteError> {
    match a {
        Act::Checkpoint => w.checkpoint(),
        Act::Merge(days) => {
            w.checkpoint()?;
            merge(w, *days).map(|_| ())
        }
        Act::Rebuild(s) => rebuild_series(w, *s),
    }
}

fn run(ops: &[Op]) {
    let mut w = World::new();
    for op in ops {
        match op {
            Op::Facts(f) => w.append(f),
            Op::Sync => w.sync(),
            Op::Act(a) => perform(&mut w, a).expect("a working file system"),
            Op::Damage(pick) => {
                damage(&mut w, *pick).expect("a working file system");
            }
            Op::Stopped { at, stop, act } => {
                match stop {
                    Stop::Kill | Stop::Power | Stop::Partial(_) => w.fs.crash_after(*at),
                    Stop::FailSync(n) => {
                        w.fs.fail_nth(FaultOp::Sync, "tree", *n, io::ErrorKind::Other)
                    }
                    Stop::FullDisk => w.fs.set_free_space(*at as u64 * 512),
                }
                let _ = perform(&mut w, act);
                match stop {
                    Stop::Kill => {
                        w.stop();
                        w.restart();
                    }
                    Stop::Power => {
                        w.stop();
                        w.fs.power_loss();
                        w.restart();
                    }
                    Stop::Partial(seed) => {
                        w.stop();
                        w.fs.power_loss_partial(*seed);
                        w.restart();
                    }
                    Stop::FailSync(_) | Stop::FullDisk => {
                        w.fs.restart();
                        w.fs.set_free_space(1 << 40);
                    }
                }
            }
        }
        w.check();
        t3(&w);
    }
    w.sync();
    w.checkpoint().expect("a last checkpoint");
    close_floors(&mut w).expect("the open floors close");
    merge(&mut w, 30).expect("the last merges");
    w.check();
    t3(&w);
    let f = w.files.as_ref().unwrap();
    assert!(f.root().floors.iter().all(|x| !x.rebuilding));
    assert!(f.scrub().is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: ProptestConfig::default().cases.min(1024), ..ProptestConfig::default() })]

    #[test]
    fn t3_holds_over_histories_with_merges_rebuilds_damage_and_stops(ops in prop::collection::vec(op(), 1..40)) {
        run(&ops);
    }
}

#[test]
fn t1_holds_when_a_series_rebuild_resumed_after_a_kill_finds_a_node_now_empty() {
    let h = 3_600_000_000_000;
    run(&[
        Op::Facts(vec![
            (0, 692_394 * S, Some(0.0)),
            (0, -h, Some(0.0)),
            (0, 23 * h, Some(0.0)),
        ]),
        Op::Sync,
        Op::Sync,
        Op::Stopped {
            at: 14,
            stop: Stop::Kill,
            act: Act::Rebuild(0),
        },
        Op::Facts(vec![(0, 692_394 * S, None)]),
    ]);
}

#[test]
fn a_one_period_floor_close_raises_the_floor_to_its_own_root_seq() {
    let mut w = World::new();
    w.append(&[(0, 600 * S, Some(1.0))]);
    w.sync();
    w.checkpoint_with(&BTreeSet::from([0])).unwrap();
    for _ in 0..3 {
        w.checkpoint().unwrap();
    }
    close_floors(&mut w).unwrap();
    let root = w.files.as_ref().unwrap().root();
    assert_eq!(root.parts.len(), 1, "one period rebuilt");
    assert_eq!(
        root.floors,
        vec![root::Floor {
            series: 0,
            seq: root.seq,
            rebuilding: false,
        }]
    );
}

#[test]
fn t8_holds_when_a_repair_follows_a_start_that_refused_a_newer_root() {
    use Act::Checkpoint as C;
    let h = 3_600_000_000_000;
    run(&[
        Op::Act(C),
        Op::Facts(vec![
            (0, 71 * h, None),
            (0, 72 * h + 19 * S, None),
            (0, 47 * h, None),
        ]),
        Op::Facts(vec![(0, 96 * h + 19 * S, None), (0, -h, Some(0.0))]),
        Op::Sync,
        Op::Act(C),
        Op::Facts(vec![
            (0, 120 * h + 19 * S, None),
            (0, 24 * h + 19 * S, None),
            (0, 72 * h + 19 * S, None),
            (0, 96 * h + 19 * S, None),
            (0, 48 * h + 19 * S, None),
            (0, -h, None),
        ]),
        Op::Sync,
        Op::Stopped {
            at: 28,
            stop: Stop::Partial(10_612_798_949_906_852_736),
            act: C,
        },
        Op::Damage(0),
    ]);
}

#[test]
fn merges_write_rollups_that_equal_a_rebuild_and_late_facts_recompute_them() {
    let mut w = World::new();
    let facts: Vec<(u64, i64, Option<f64>)> = (0..200)
        .map(|i| (i % 2, i as i64 * 600 * S, Some(i as f64)))
        .collect();
    w.append(&facts);
    w.sync();
    w.checkpoint().unwrap();
    assert!(merge(&mut w, 20).unwrap() > 0);
    let root = w.files.as_ref().unwrap().root().clone();
    assert!(set::covered(&root, 4, 0, DAY), "the first day is merged");
    let vis = visible(&root, &live_parts(w.files.as_ref().unwrap())).unwrap();
    assert!(vis.contains_key(&(0, 3, 0)) && vis.contains_key(&(1, 4, 0)));
    t3(&w);
    w.append(&[(0, 5 * QUARTER + 7 * S, Some(-9.0)), (1, 3 * QUARTER, None)]);
    w.sync();
    w.checkpoint().unwrap();
    w.check();
    t3(&w);
}

#[test]
fn a_checkpoint_over_a_damaged_part_marks_its_rollups_stale_and_a_repair_restores_t3() {
    let mut w = World::new();
    let facts: Vec<(u64, i64, Option<f64>)> = (0..200)
        .map(|i| (0, i as i64 * 600 * S, Some(i as f64)))
        .collect();
    w.append(&facts);
    w.sync();
    w.checkpoint().unwrap();
    merge(&mut w, 20).unwrap();
    let f = w.files.as_ref().unwrap();
    let bad = *f
        .root()
        .parts
        .iter()
        .find(|p| p.period_start <= 0 && 0 < p.period_start + p.period_len as i64)
        .unwrap();
    let path = f.dir().join(part_name(1, bad.file_id));
    let mid = (bad.byte_len / 2) as usize;
    let byte = w.fs.read(&path).unwrap()[mid];
    w.fs.poke(&path, mid, &[byte ^ 0x5a]);
    w.renamed.insert(path.clone(), w.fs.read(&path).unwrap());
    w.append(&[(0, 5 * QUARTER + 7 * S, Some(-9.0))]);
    w.sync();
    w.checkpoint().expect("a checkpoint past the damaged part");
    let f = w.files.as_ref().unwrap();
    let mut root = f.root().clone();
    root.parts.retain(|p| p.file_id != bad.file_id);
    let parts: Vec<Part> = root.parts.iter().map(|r| f.load(r).unwrap()).collect();
    let vis = visible(&root, &parts).unwrap();
    let day = vis.get(&(0, 4, 0)).expect("the day's roll-up");
    assert!(
        day.stale && day.node.is_none(),
        "the day over the damaged part is stale"
    );
    assert_eq!(f.scrub(), vec![bad.file_id]);
    repair(&mut w).unwrap();
    w.check();
    t3(&w);
}

#[test]
fn a_floor_close_publishes_one_settled_period_a_root_and_closes_with_the_last() {
    let mut w = World::new();
    let facts: Vec<(u64, i64, Option<f64>)> = (0..4)
        .map(|d| (0, d * 8 * DAY + 10 * S, Some(d as f64)))
        .collect();
    w.append(&facts);
    w.sync();
    w.wall += 40 * DAY;
    w.checkpoint_with(&BTreeSet::from([0])).unwrap();
    let seq = w.files.as_ref().unwrap().root().seq;
    close_floors(&mut w).unwrap();
    let root = w.files.as_ref().unwrap().root().clone();
    let periods: BTreeSet<i64> = root
        .parts
        .iter()
        .filter(|p| p.class == part::Class::Rebuilt)
        .map(|p| p.period_start)
        .collect();
    assert!(periods.len() >= 2, "{periods:?}");
    assert_eq!(root.seq, seq + periods.len() as u64);
    assert!(root.floors.iter().all(|f| !f.rebuilding));
    w.check();
    t3(&w);
}

#[test]
fn rebuild_newest_first_covers_from_each_period_down_and_without_bound_last() {
    let mut w = World::new();
    let facts: Vec<(u64, i64, Option<f64>)> = (0..4)
        .map(|d| (0, d * 8 * DAY + 10 * S, Some(d as f64)))
        .collect();
    w.append(&facts);
    w.sync();
    w.wall += 40 * DAY;
    w.checkpoint().unwrap();
    let root = w.files.as_ref().unwrap().root().clone();
    let cur = w.current(root.checkpoint);
    let r = rebuild_of(&w, &cur, [0], None, |l, a, b| set::covered(&root, l, a, b));
    let mut want: Vec<Option<i64>> = r.parts.keys().skip(1).rev().map(|p| Some(p.0)).collect();
    want.push(None);
    assert!(want.len() >= 3, "{want:?}");
    let calls = std::cell::RefCell::new(Vec::new());
    let (wall, now) = (w.wall, w.now);
    w.files
        .as_mut()
        .unwrap()
        .rebuild_newest_first(
            root.checkpoint,
            r,
            |from| {
                calls.borrow_mut().push(from);
                Vec::new()
            },
            wall,
            now,
        )
        .unwrap();
    assert_eq!(calls.into_inner(), want);
}

#[test]
fn a_damaged_part_is_found_by_the_scrub_and_its_period_rebuilt() {
    let mut w = World::new();
    w.append(&[(0, 10 * S, Some(1.0)), (1, DAY + 10 * S, Some(2.0))]);
    w.sync();
    w.checkpoint().unwrap();
    let before = w.files.as_ref().unwrap().root().clone();
    assert!(damage(&mut w, 0).unwrap());
    let root = w.files.as_ref().unwrap().root().clone();
    assert!(!root.parts.contains(&before.parts[0]));
    assert!(root.parts.iter().any(|p| p.class == part::Class::Rebuilt));
    w.check();
    t3(&w);
}

#[test]
fn a_backfill_of_a_store_without_a_tree_reaches_full_cover_and_t1() {
    let mut w = World::new();
    w.append(&[(0, -40 * DAY, Some(1.0)), (2, 10 * S, Some(2.0))]);
    w.sync();
    w.stop();
    w.fs = Arc::new(MemFs::new());
    w.renamed.clear();
    w.start();
    assert!(w.files.as_ref().unwrap().root().covered.is_empty());
    w.checkpoint().unwrap();
    let k = w.synced;
    let cur = w.current(k);
    let cover = backfill_cover(&LEVELS, None, None, w.wall, &POLICY);
    let probe = Root {
        covered: cover.clone(),
        ..w.files.as_ref().unwrap().root().clone()
    };
    let mut r = rebuild_of(&w, &cur, 0..SERIES, None, |l, a, b| {
        set::covered(&probe, l, a, b)
    });
    r.cover = cover;
    let (wall, now) = (w.wall, w.now);
    w.files.as_mut().unwrap().rebuild(k, r, wall, now).unwrap();
    let root = w.files.as_ref().unwrap().root().clone();
    assert!(set::covered(&root, 2, i64::MIN / 2, i64::MAX / 2));
    w.check();
}

#[test]
fn a_rebuild_off_the_roots_k_is_refused() {
    let mut w = World::new();
    w.append(&[(0, 0, Some(1.0))]);
    w.sync();
    let (wall, now) = (w.wall, w.now);
    let r = w
        .files
        .as_mut()
        .unwrap()
        .rebuild(pos(1), Rebuild::default(), wall, now);
    assert!(
        matches!(r, Err(WriteError::NotAtCheckpoint { .. })),
        "{r:?}"
    );
}

#[test]
fn a_rebuild_records_only_live_parts_as_retired() {
    let mut w = World::new();
    w.append(&[(0, 0, Some(1.0))]);
    w.sync();
    w.checkpoint().unwrap();
    let f = w.files.as_ref().unwrap();
    let k = f.root().checkpoint;
    let live = f.root().parts[0].file_id;
    let (wall, now) = (w.wall, w.now);
    let r = Rebuild {
        retire: vec![live, live + 1000],
        ..Rebuild::default()
    };
    w.files.as_mut().unwrap().rebuild(k, r, wall, now).unwrap();
    let ids: Vec<u64> = w
        .files
        .as_ref()
        .unwrap()
        .root()
        .retired
        .iter()
        .map(|x| x.file_id)
        .collect();
    assert_eq!(ids, vec![live]);
}

#[test]
fn a_reopened_floor_rises_to_the_new_checkpoints_seq() {
    let mut w = World::new();
    w.append(&[(1, 0, Some(1.0))]);
    w.sync();
    rebuild_series(&mut w, 1).unwrap();
    let first = w.files.as_ref().unwrap().root().floors[0];
    assert!(!first.rebuilding);
    w.checkpoint_with(&BTreeSet::from([1])).unwrap();
    let again = w.files.as_ref().unwrap().root().floors[0];
    assert!(again.rebuilding && again.seq > first.seq);
    close_floors(&mut w).unwrap();
    t3(&w);
}

#[test]
fn a_part_unlike_its_record_in_the_root_is_damaged_though_it_decodes() {
    let mut w = World::new();
    w.append(&[(0, 10 * S, Some(1.0))]);
    w.sync();
    w.checkpoint().unwrap();
    let f = w.files.as_mut().unwrap();
    assert!(f.scrub().is_empty());
    f.root.parts[0].seq += 1;
    let id = f.root.parts[0].file_id;
    assert_eq!(f.scrub(), vec![id]);
}

#[test]
fn backfill_covers_finer_levels_only_within_minute_keep_and_rollups_only_over_settled_days() {
    let now = 100 * DAY + 5 * QUARTER;
    let c = backfill_cover(&LEVELS, Some(10 * DAY), None, now, &POLICY);
    let of = |l: u8| c.iter().find(|x| x.level == l).copied();
    assert_eq!(of(1).unwrap().from, 93 * DAY + 5 * QUARTER);
    assert_eq!(of(2).unwrap().from, 10 * DAY);
    assert_eq!(
        (of(4).unwrap().from, of(4).unwrap().to),
        (10 * DAY, 99 * DAY)
    );
    assert_eq!(of(3).unwrap().to, 99 * DAY);
}

#[test]
fn series_entries_leave_out_the_rollups_keep_refuses() {
    let facts: Vec<Fact> = (0..4)
        .map(|i| Fact {
            t: i * 3 * QUARTER,
            reading: Some(Value::F64(1.0)),
            at: pos(i as u64 + 1),
        })
        .collect();
    let levels = folded_levels(&LEVELS);
    let all = series_entries(&LEVELS, &levels, &params(), &facts, |_, _, _| true);
    assert!(all.iter().any(|(l, _)| *l == 3) && all.iter().any(|(l, _)| *l == 4));
    let some = series_entries(&LEVELS, &levels, &params(), &facts, |l, a, _| {
        l != 3 || a != 0
    });
    assert!(!some.iter().any(|(l, e)| *l == 3 && e.start == 0));
    assert!(some.iter().any(|(l, _)| *l == 4));
}

#[test]
fn a_settled_period_is_the_widest_whose_merge_is_due_and_no_wider() {
    let monday = 4 * DAY;
    let t = monday + 2 * DAY;
    let week_due = monday + 7 * DAY + 14 * DAY;
    assert_eq!(
        settled_period(t, week_due, &POLICY),
        Some((monday, (7 * DAY) as u64))
    );
    assert_eq!(
        settled_period(t, week_due - 1, &POLICY),
        Some((t, DAY as u64))
    );
}
