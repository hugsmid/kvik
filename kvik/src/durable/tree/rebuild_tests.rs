use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use kvik_log::fs::Fs;
use kvik_model as km;
use kvik_tree::root::Root;
use kvik_tree::set::{self, DAY, Key, Seen};
use kvik_tree::write::{DIR, part_name};

use super::tests::{Store, assert_t1, history, skew, tree_state};
use crate::durable::{lock, windows};
use crate::env::MemFs;
use crate::types::Intent;

/// Advances the clock an interval at a time until the newest root on disk satisfies `done`; returns the number of
/// intervals. With `lockstep`, each interval waits for the job to have stepped at the new time, so the count is the
/// job's (the job thread wakes about once a second, so only for short waits).
fn advance_until(st: &Store, what: &str, lockstep: bool, done: impl Fn(&Root) -> bool) -> usize {
    let deadline = Instant::now() + Duration::from_secs(30);
    let job = st.w.tree.clone().expect("a tree job");
    let mut steps = 0;
    loop {
        steps += 1;
        let step = Duration::from_secs(16 * 60);
        st.clock.advance(step);
        skew().fetch_add(
            u64::try_from(step.as_nanos()).unwrap(),
            std::sync::atomic::Ordering::SeqCst,
        );
        let at = st.clock.wall().0;
        if lockstep {
            while job.stepped_wall() < at {
                assert!(
                    Instant::now() < deadline,
                    "no job step where {what}: {:?}",
                    st.w.health()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            std::thread::sleep(Duration::from_millis(30));
        }
        let has_root = st
            .fs
            .read_dir(&st.dir.join(DIR))
            .is_ok_and(|es| es.iter().any(|e| e.name.starts_with("root-")));
        if has_root && done(&newest_root(&st.fs, &st.dir)) {
            return steps;
        }
        assert!(
            Instant::now() < deadline,
            "no root where {what}: {:?}",
            st.w.health()
        );
    }
}

fn step_until(st: &Store, what: &str, done: impl Fn(&Root) -> bool) -> usize {
    advance_until(st, what, false, done)
}

fn newest_root(fs: &MemFs, dir: &Path) -> Root {
    let tree = dir.join(DIR);
    let name = fs
        .read_dir(&tree)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n.starts_with("root-"))
        .max()
        .unwrap();
    Root::decode(&kvik_tree::write::read_file(fs, &tree.join(name)).unwrap()).unwrap()
}

fn model_params(st: &Store) -> km::summary::Params {
    let d = lock(&st.w.decider);
    windows::params(&d.registry.get(st.id.0).unwrap().def)
}

/// Every visible node equals the model's over the test's readings: levels 1 and 2 and the roll-ups (Q-1421).
fn assert_model(st: &Store, vis: &BTreeMap<Key, Seen>) {
    let rs: Vec<(i64, km::types::Value)> = st
        .current
        .iter()
        .filter_map(|(&t, v)| v.map(|v| (t, km::types::Value::Num(v))))
        .collect();
    let tree = km::tree::SeriesTree::build(model_params(st), &rs);
    for (&(_, level, start), seen) in vis {
        let got = crate::fold::model_node(seen.node.clone().expect("a node"), &[]);
        let want = match level {
            1 => Some(tree.node(0, start)),
            2 => Some(tree.node(1, start)),
            3 => tree.rollup(km::tree::ROLLUPS[0], start),
            4 => tree.rollup(km::tree::ROLLUPS[1], start),
            _ => None,
        };
        assert_eq!(Some(got), want, "level {level} at {start}");
    }
}

fn remove_tree(fs: &MemFs, dir: &Path) {
    let tree = dir.join(DIR);
    for e in fs.read_dir(&tree).unwrap() {
        fs.remove_file(&tree.join(&e.name)).unwrap();
    }
}

fn day_of(st: &Store) -> i64 {
    let t = *st.current.keys().next().unwrap();
    t - t.rem_euclid(DAY)
}

#[test]
fn a_retired_series_is_rebuilt_at_the_next_step_and_served_again() {
    let mut st = Store::new("reserved");
    history(&mut st);
    let p = st.params();
    let id = st.id;
    st.w.retire(id, st.clock.wall()).unwrap();
    st.stop();
    assert!(tree_state(&st.fs, &st.dir).0.floors[0].rebuilding);
    st.reopen();
    step_until(&st, "the floor is closed", |r| {
        r.floors.iter().all(|f| !f.rebuilding)
    });
    st.stop();
    let (root, vis) = tree_state(&st.fs, &st.dir);
    assert_eq!(root.floors.len(), 1);
    assert!(!vis.is_empty());
    assert_t1(&p, id.0, &st.current, &vis);
}

#[test]
fn a_damaged_part_is_rebuilt_at_the_first_scrub_after_a_start() {
    let mut st = Store::new("scrub");
    history(&mut st);
    let p = st.params();
    st.stop();
    let (root, _) = tree_state(&st.fs, &st.dir);
    let bad = root.parts[0];
    let path = st
        .dir
        .join(DIR)
        .join(part_name(root.header.generation, bad.file_id));
    st.fs
        .poke(&path, (bad.byte_len / 2) as usize, &[0xff, 0x00, 0xff]);
    st.reopen();
    let steps = advance_until(&st, "the damaged part is replaced", true, |r| {
        !r.parts.iter().any(|x| x.file_id == bad.file_id)
    });
    assert!(
        steps <= 4,
        "found by the scrub, not by a merge a day later: {steps} steps"
    );
    st.stop();
    let (root, vis) = tree_state(&st.fs, &st.dir);
    assert!(
        root.retired.iter().any(|x| x.file_id == bad.file_id)
            || !root.parts.iter().any(|x| x.file_id == bad.file_id)
    );
    assert_t1(&p, st.id.0, &st.current, &vis);
}

/// T3 against the live store: checkpoints, merges with roll-ups, late data into a merged day and a kill, then the
/// same log backfilled into an empty tree; both equal the model.
#[test]
fn a_backfill_reaches_full_cover_and_equals_the_live_tree_after_merges_and_a_kill() {
    let mut st = Store::new("backfill");
    history(&mut st);
    let d0 = day_of(&st);
    st.w.reader().unwrap();
    step_until(&st, "the first day's roll-ups are written", |r| {
        set::covered(r, 4, d0, d0 + DAY)
    });
    let late = (st.clock.wall().0 - (d0 + 13 * 3_600_000_000_000)) / 1_000_000_000;
    let late = u64::try_from(late).unwrap();
    st.emit(
        Intent::Normal,
        &[(late, Some(42.0)), (late + 600, Some(-1.0))],
    );
    let copy = st.fs.snapshot();
    st.stop();
    st.fs = copy;
    st.reopen();
    st.w.reader().unwrap();
    st.emit(Intent::Revise, &[(late, Some(43.0))]);
    let k0 = tree_state(&st.fs, &st.dir).0.checkpoint;
    step_until(&st, "a checkpoint after the kill", |r| r.checkpoint > k0);
    st.stop();
    let (live_root, live) = tree_state(&st.fs, &st.dir);
    assert!(live.keys().any(|k| k.1 == 4), "a day roll-up is visible");
    assert_model(&st, &live);

    let fresh = st.fs.snapshot();
    remove_tree(&fresh, &st.dir);
    st.fs = fresh;
    st.reopen();
    st.w.reader().unwrap();
    step_until(&st, "the quarter-hour level covers everything", |r| {
        set::covered(r, 2, i64::MIN / 2, i64::MAX / 2)
    });
    st.stop();
    let (root, rebuilt) = tree_state(&st.fs, &st.dir);
    assert_model(&st, &rebuilt);
    let both = |m: &BTreeMap<Key, Seen>| -> BTreeMap<Key, Seen> {
        m.iter()
            .filter(|&(&(_, l, s), _)| {
                let w = root.levels.iter().find(|x| x.id == l).unwrap().width;
                set::covered(&root, l, s, s + w) && set::covered(&live_root, l, s, s + w)
            })
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    };
    assert_eq!(live_root.checkpoint, root.checkpoint);
    assert_eq!(both(&live), both(&rebuilt), "T3: as_of included");
    assert!(both(&live).keys().any(|k| k.1 == 4));
}

#[test]
fn a_damaged_part_of_a_merged_day_is_rebuilt_when_late_data_arrives() {
    let mut st = Store::new("late-damage");
    history(&mut st);
    let d0 = day_of(&st);
    st.w.reader().unwrap();
    step_until(&st, "the first day's roll-ups are written", |r| {
        set::covered(r, 4, d0, d0 + DAY)
    });
    let late = (st.clock.wall().0 - (d0 + 13 * 3_600_000_000_000)) / 1_000_000_000;
    let late = u64::try_from(late).unwrap();
    st.stop();
    let (root, _) = tree_state(&st.fs, &st.dir);
    let bad = *root
        .parts
        .iter()
        .find(|p| p.period_start <= d0 && d0 < p.period_start + p.period_len as i64)
        .unwrap();
    let path = st
        .dir
        .join(DIR)
        .join(part_name(root.header.generation, bad.file_id));
    st.fs
        .poke(&path, (bad.byte_len / 2) as usize, &[0xff, 0x00, 0xff]);
    st.reopen();
    st.w.reader().unwrap();
    st.emit(Intent::Normal, &[(late, Some(42.0))]);
    let k0 = root.checkpoint;
    step_until(
        &st,
        "the damaged part is replaced after a checkpoint",
        |r| r.checkpoint > k0 && !r.parts.iter().any(|x| x.file_id == bad.file_id),
    );
    st.stop();
    let (_, vis) = tree_state(&st.fs, &st.dir);
    assert_model(&st, &vis);
}

#[test]
fn a_checkpoint_that_meets_a_damaged_part_repairs_it_at_the_same_step_not_at_the_next_scrub() {
    let mut st = Store::new("live-damage");
    history(&mut st);
    let d0 = day_of(&st);
    st.w.reader().unwrap();
    step_until(&st, "the first day's roll-ups are written", |r| {
        set::covered(r, 4, d0, d0 + DAY)
    });
    let root = newest_root(&st.fs, &st.dir);
    let bad = *root
        .parts
        .iter()
        .find(|p| p.period_start <= d0 && d0 < p.period_start + p.period_len as i64)
        .unwrap();
    let path = st
        .dir
        .join(DIR)
        .join(part_name(root.header.generation, bad.file_id));
    st.fs
        .poke(&path, (bad.byte_len / 2) as usize, &[0xff, 0x00, 0xff]);
    let late = (st.clock.wall().0 - (d0 + 13 * 3_600_000_000_000)) / 1_000_000_000;
    st.emit(
        Intent::Normal,
        &[(u64::try_from(late).unwrap(), Some(42.0))],
    );
    let steps = advance_until(&st, "the damaged part is replaced", true, |r| {
        r.checkpoint > root.checkpoint && !r.parts.iter().any(|x| x.file_id == bad.file_id)
    });
    assert!(
        steps <= 2,
        "repaired by the checkpoint's step: {steps} steps"
    );
    st.stop();
    let (_, vis) = tree_state(&st.fs, &st.dir);
    assert_model(&st, &vis);
}

#[test]
fn a_floor_close_over_a_merged_day_equals_the_model_with_rollups() {
    let mut st = Store::new("floor-model");
    history(&mut st);
    let d0 = day_of(&st);
    st.w.reader().unwrap();
    step_until(&st, "the first day's roll-ups are written", |r| {
        set::covered(r, 4, d0, d0 + DAY)
    });
    let id = st.id;
    st.w.retire(id, st.clock.wall()).unwrap();
    step_until(&st, "the floor is opened and closed", |r| {
        r.floors.len() == 1 && r.floors.iter().all(|f| !f.rebuilding)
    });
    st.stop();
    let (root, vis) = tree_state(&st.fs, &st.dir);
    assert!(set::covered(&root, 4, d0, d0 + DAY));
    assert!(vis.keys().any(|k| k.1 == 4), "the day's roll-up is rebuilt");
    assert_model(&st, &vis);
}

/// The visible nodes `live` and `rebuilt` both cover, which must be equal, `as_of` included (T3).
fn assert_t3(live: &(Root, BTreeMap<Key, Seen>), rebuilt: &(Root, BTreeMap<Key, Seen>)) {
    let both = |m: &BTreeMap<Key, Seen>| -> BTreeMap<Key, Seen> {
        m.iter()
            .filter(|&(&(_, l, s), _)| {
                let w = rebuilt.0.levels.iter().find(|x| x.id == l).unwrap().width;
                set::covered(&rebuilt.0, l, s, s + w) && set::covered(&live.0, l, s, s + w)
            })
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    };
    assert_eq!(live.0.checkpoint, rebuilt.0.checkpoint);
    assert!(!both(&live.1).is_empty());
    assert_eq!(both(&live.1), both(&rebuilt.1), "T3: as_of included");
}

/// The same store backfilled from no tree, to full cover of the quarter-hour level.
fn rebuilt(st: &mut Store) -> (Root, BTreeMap<Key, Seen>) {
    let fresh = st.fs.snapshot();
    remove_tree(&fresh, &st.dir);
    st.fs = fresh;
    st.reopen();
    st.w.reader().unwrap();
    step_until(st, "the rebuild covers the quarter-hour level", |r| {
        set::covered(r, 2, i64::MIN / 2, i64::MAX / 2)
    });
    st.stop();
    tree_state(&st.fs, &st.dir)
}

#[test]
fn a_store_written_with_the_tree_on_then_off_then_on_ends_equal_to_a_rebuild() {
    let mut st = Store::new("on-off-on");
    history(&mut st);
    st.stop();
    let (first, _) = tree_state(&st.fs, &st.dir);
    st.clock.advance(Duration::from_secs(600));
    st.reopen_with(false);
    st.emit(Intent::Normal, &[(7_000, Some(5.0)), (5, Some(6.0))]);
    st.emit(Intent::Revise, &[(7_300 - 61, Some(-8.0)), (6_000, None)]);
    st.stop();
    assert_eq!(
        tree_state(&st.fs, &st.dir).0.seq,
        first.seq,
        "no root while off"
    );
    st.reopen();
    st.emit(Intent::Normal, &[(2, Some(1.25))]);
    st.stop();
    let live = tree_state(&st.fs, &st.dir);
    assert!(live.0.checkpoint > first.checkpoint);
    assert_t1(&st.params(), st.id.0, &st.current, &live.1);
    assert_model(&st, &live.1);
    let rebuilt = rebuilt(&mut st);
    assert_model(&st, &rebuilt.1);
    assert_t3(&live, &rebuilt);
}

#[test]
fn turning_the_tree_on_with_its_root_behind_past_stretch_max_discards_it_and_backfills() {
    let mut st = Store::new("far-behind");
    history(&mut st);
    st.stop();
    let (first, _) = tree_state(&st.fs, &st.dir);
    st.reopen_with(false);
    for k in 0..6u64 {
        st.emit(Intent::Normal, &[(4_000 - k * 7, Some(k as f64))]);
    }
    st.stop();
    super::job::STRETCH_MAX_AT_OPEN.set(3);
    st.reopen();
    super::job::STRETCH_MAX_AT_OPEN.set(super::read::STRETCH_MAX);
    st.w.reader().unwrap();
    step_until(&st, "the tree is discarded", |r| r.floor > first.floor);
    step_until(&st, "the backfill covers the quarter-hour level", |r| {
        set::covered(r, 2, i64::MIN / 2, i64::MAX / 2)
    });
    st.stop();
    let (root, vis) = tree_state(&st.fs, &st.dir);
    assert!(root.floor > first.floor, "the old tree was discarded");
    assert_t1(&st.params(), st.id.0, &st.current, &vis);
    assert_model(&st, &vis);
}
