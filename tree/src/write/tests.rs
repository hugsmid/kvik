use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::fs::{FaultOp, Fs, MemFs};
use kvik_log::mark::ResetStamp;
use kvik_log::writer::Position;
use proptest::prelude::*;

use super::*;
use crate::fold::fold;
use crate::level::MINUTE;
use crate::node::{Kind, Params, Value};
use crate::set::{self, Policy, QUARTER, Seen, visible};

const S: i64 = 1_000_000_000;
const SERIES: u64 = 3;

fn id() -> Identity {
    Identity {
        generation: 1,
        reset: ResetStamp::default(),
        store_id: [7; 16],
    }
}

fn pos(s: u64) -> Position {
    Position {
        generation: 1,
        sequence: s,
    }
}

fn params() -> Params {
    Params::new(Kind::Gauge, Some(600 * S))
}

type Current = BTreeMap<(u64, i64), (Position, Option<f64>)>;

/// A log of facts and the tree writer of one process over it, on a [`MemFs`] that stops where the history says.
struct World {
    fs: Arc<MemFs>,
    dir: PathBuf,
    /// `(position, series, t, value)`; `None` is a fact summaries ignore.
    log: Vec<(Position, u64, i64, Option<f64>)>,
    synced: Position,
    dirty: BTreeMap<(u64, i64), Position>,
    files: Option<TreeFiles>,
    now: Duration,
    wall: i64,
    /// Bytes of every file under `tree/` when first seen renamed (T6).
    renamed: BTreeMap<PathBuf, Vec<u8>>,
    grace: Duration,
}

impl World {
    fn new() -> Self {
        Self::with_grace(GRACE)
    }

    fn with_grace(grace: Duration) -> Self {
        let mut w = World {
            fs: Arc::new(MemFs::new()),
            dir: PathBuf::from("s"),
            log: Vec::new(),
            synced: Position::default(),
            dirty: BTreeMap::new(),
            files: None,
            now: Duration::from_secs(1),
            wall: 1_000 * S,
            renamed: BTreeMap::new(),
            grace,
        };
        w.start();
        w
    }

    fn tick(&mut self) {
        self.now += Duration::from_secs(60);
        self.wall += 60 * S;
    }

    fn append(&mut self, facts: &[(u64, i64, Option<f64>)]) {
        for &(s, t, v) in facts {
            let last = self.log.last().map_or(0, |l| l.0.sequence);
            let p = pos(last.max(self.synced.sequence) + 1);
            self.log.push((p, s, t, v));
            let e = self
                .dirty
                .entry((s, t - t.rem_euclid(QUARTER)))
                .or_default();
            *e = (*e).max(p);
        }
    }

    fn sync(&mut self) {
        if let Some(l) = self.log.last() {
            self.synced = l.0;
        }
    }

    /// The current facts at `k` per `(series, t)`: the latest at or below it.
    fn current(&self, k: Position) -> Current {
        let mut m = BTreeMap::new();
        for &(p, s, t, v) in self.log.iter().filter(|l| l.0 <= k) {
            m.insert((s, t), (p, v));
        }
        m
    }

    fn facts_in(cur: &Current, s: u64, from: i64, to: i64) -> Vec<Fact> {
        cur.range((s, from)..(s, to))
            .map(|(&(_, t), &(at, v))| Fact {
                t,
                reading: v.map(Value::F64),
                at,
            })
            .collect()
    }

    fn checkpoint(&mut self) -> Result<(), WriteError> {
        self.tick();
        let k = self.synced;
        let cur = self.current(k);
        let levels = folded_levels(&LEVELS);
        let mut entries: BTreeMap<(u64, u8), Vec<Entry>> = BTreeMap::new();
        for &(s, q) in self.dirty.keys() {
            let facts = Self::facts_in(&cur, s, q, q + QUARTER);
            for (l, e) in quarter_entries(&params(), &levels, q, &facts) {
                entries.entry((s, l)).or_default().push(e);
            }
        }
        let (wall, now) = (self.wall, self.now);
        let files = self.files.as_mut().expect("started");
        files.checkpoint(k, self.synced, entries, &BTreeSet::new(), wall, now)?;
        self.dirty.retain(|_, p| *p > k);
        Ok(())
    }

    /// The process stops: its memory is gone, and so are the log's positions above the synced mark.
    fn stop(&mut self) {
        self.files = None;
        self.dirty.clear();
        let synced = self.synced;
        self.log.retain(|l| l.0 <= synced);
    }

    /// A start: the files, then the dirty set from the log above K.
    fn start(&mut self) {
        self.fs.restart();
        self.tick();
        let fresh = self.log.is_empty();
        let (files, _) = TreeFiles::start(
            self.fs.clone(),
            &self.dir,
            id(),
            Opening {
                mark: self.synced,
                fresh,
                wall: self.wall,
                now: self.now,
            },
            |_| false,
        )
        .expect("a start on a working file system");
        let k = files.root().checkpoint;
        self.files = Some(files.with_grace(self.grace));
        for &(p, s, t, _) in self.log.iter().filter(|l| l.0 > k) {
            let e = self
                .dirty
                .entry((s, t - t.rem_euclid(QUARTER)))
                .or_default();
            *e = (*e).max(p);
        }
    }

    fn restart(&mut self) {
        self.start();
        self.check();
    }

    fn tree_files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        self.fs
            .paths()
            .into_iter()
            .filter(|p| p.starts_with(self.dir.join(DIR)) && !p.to_string_lossy().ends_with(".tmp"))
            .filter_map(|p| self.fs.read(&p).map(|b| (p, b)))
            .collect()
    }

    fn temporaries(&self) -> usize {
        self.fs
            .paths()
            .iter()
            .filter(|p| p.to_string_lossy().ends_with(".tmp"))
            .count()
    }

    /// T5, T6, T8 on the files as they are, and T1 on the writer's root when it is on disk.
    fn check(&mut self) {
        let files = self.tree_files();
        let names: BTreeSet<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
        self.renamed.retain(|p, _| names.contains(p));
        for (p, b) in &files {
            let first = self.renamed.entry(p.clone()).or_insert_with(|| b.clone());
            assert_eq!(first, b, "T6: {} changed after its rename", p.display());
        }
        let mut roots: Vec<Root> = files
            .iter()
            .filter(|(p, _)| p.extension().is_some_and(|e| e == "kvr"))
            .filter_map(|(_, b)| Root::decode(b).ok())
            .collect();
        roots.sort_by_key(|r| r.seq);
        for r in &roots {
            assert!(
                r.checkpoint <= self.synced,
                "T5: root {} above the mark",
                r.seq
            );
        }
        for w in roots.windows(2) {
            assert!(
                w[0].checkpoint <= w[1].checkpoint,
                "T8: K fell from root {} to {}",
                w[0].seq,
                w[1].seq
            );
        }
        if let Some(f) = &self.files
            && names.contains(&f.dir().join(root_name(f.root().seq)))
        {
            t1(self, f);
        }
    }
}

/// T1: every covered span of a folded level holds the canonical node of the current view at K, at its version,
/// and no entry where that node is empty.
fn t1(w: &World, f: &TreeFiles) {
    let root = f.root();
    let parts: Vec<Part> = f
        .read_parts()
        .into_iter()
        .map(|(r, p)| {
            p.unwrap_or_else(|| panic!("T6: part {:#x} of the root missing or damaged", r.file_id))
        })
        .collect();
    let vis = visible(root, &parts).expect("a visible state");
    let cur = w.current(root.checkpoint);
    let mut expect: BTreeMap<set::Key, Seen> = BTreeMap::new();
    let spans: BTreeSet<(u64, i64)> = cur
        .keys()
        .map(|&(s, t)| (s, t - t.rem_euclid(QUARTER)))
        .collect();
    for (s, q) in spans {
        let facts = World::facts_in(&cur, s, q, q + QUARTER);
        for (l, e) in quarter_entries(&params(), &folded_levels(&root.levels), q, &facts) {
            let width = root.levels.iter().find(|x| x.id == l).unwrap().width;
            let Some(node) = e.node else { continue };
            if !set::covered(root, l, e.start, e.start + width) {
                continue;
            }
            let readings: Vec<(i64, Value)> = facts
                .iter()
                .filter(|x| e.start <= x.t && x.t < e.start + width)
                .filter_map(|x| x.reading.clone().map(|v| (x.t, v)))
                .collect();
            assert_eq!(node, fold(&params(), e.start, e.start + width, &readings));
            expect.insert(
                (s, l, e.start),
                Seen {
                    as_of: e.as_of,
                    stale: false,
                    node: Some(node),
                },
            );
        }
    }
    assert_eq!(
        vis, expect,
        "T1 at root {} (K {:?})",
        root.seq, root.checkpoint
    );
}

#[derive(Clone, Debug)]
enum Stop {
    Kill,
    Power,
    Partial(u64),
    FailSync(usize),
    FullDisk,
}

#[derive(Clone, Debug)]
enum Op {
    Facts(Vec<(u64, i64, Option<f64>)>),
    Sync,
    Checkpoint,
    /// A checkpoint during which the file system stops after `at` operations.
    Stopped {
        at: usize,
        stop: Stop,
    },
}

fn fact() -> impl Strategy<Value = (u64, i64, Option<f64>)> {
    (
        0..SERIES,
        0..2i64,
        0..400i64,
        prop::option::weighted(0.8, -40..40i32),
    )
        .prop_map(|(s, d, k, v)| {
            (
                s,
                d * DAY - 3_600 * S + k * 20 * S,
                v.map(|v| f64::from(v) / 4.0),
            )
        })
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
        2 => Just(Op::Checkpoint),
        2 => (0..20usize, stop).prop_map(|(at, stop)| Op::Stopped { at, stop }),
    ]
}

fn run(ops: &[Op]) {
    let mut w = World::new();
    for op in ops {
        match op {
            Op::Facts(f) => w.append(f),
            Op::Sync => w.sync(),
            Op::Checkpoint => w
                .checkpoint()
                .expect("a checkpoint on a working file system"),
            Op::Stopped { at, stop } => {
                match stop {
                    Stop::Kill | Stop::Power | Stop::Partial(_) => w.fs.crash_after(*at),
                    Stop::FailSync(n) => {
                        w.fs.fail_nth(FaultOp::Sync, "tree", *n, io::ErrorKind::Other)
                    }
                    Stop::FullDisk => w.fs.set_free_space(*at as u64 * 512),
                }
                let r = w.checkpoint();
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
                        if r.is_err() {
                            assert_eq!(w.temporaries(), 0, "a failed job leaves no temporary file");
                        }
                    }
                }
            }
        }
        w.check();
    }
    w.sync();
    w.checkpoint().expect("a last checkpoint");
    w.check();
    let f = w.files.as_ref().unwrap();
    assert_eq!(f.root().checkpoint, w.synced, "the tree reaches the mark");
    assert!(w.dirty.is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn t1_t5_t6_t8_hold_over_histories_with_a_stop_at_any_write_step(ops in prop::collection::vec(op(), 1..30)) {
        run(&ops);
    }
}

#[test]
fn every_write_step_of_a_checkpoint_can_be_stopped_by_each_kind_of_stop() {
    let base = vec![
        Op::Facts(vec![
            (0, 0, Some(1.0)),
            (1, DAY - 60 * S, Some(2.0)),
            (1, DAY + 60 * S, None),
        ]),
        Op::Sync,
        Op::Checkpoint,
        Op::Facts(vec![
            (0, 0, Some(3.0)),
            (2, 5 * MINUTE, Some(4.0)),
            (1, DAY + 60 * S, Some(5.0)),
        ]),
        Op::Sync,
    ];
    for at in 0..24 {
        for stop in [
            Stop::Kill,
            Stop::Power,
            Stop::Partial(at as u64),
            Stop::Partial(!(at as u64)),
            Stop::FailSync(at % 3),
            Stop::FullDisk,
        ] {
            let mut ops = base.clone();
            ops.push(Op::Stopped { at, stop });
            ops.push(Op::Facts(vec![(0, 7 * MINUTE, Some(6.0))]));
            ops.push(Op::Sync);
            ops.push(Op::Checkpoint);
            run(&ops);
        }
    }
}

#[test]
fn a_checkpoint_above_the_synced_mark_is_refused_and_writes_nothing() {
    let mut w = World::new();
    w.append(&[(0, 0, Some(1.0))]);
    let before = w.tree_files();
    let f = w.files.as_mut().unwrap();
    let r = f.checkpoint(
        pos(1),
        Position::default(),
        BTreeMap::new(),
        &BTreeSet::new(),
        0,
        Duration::ZERO,
    );
    assert!(matches!(r, Err(WriteError::AboveMark { .. })), "{r:?}");
    assert_eq!(w.tree_files(), before);
}

#[test]
fn a_checkpoint_below_the_roots_is_refused() {
    let mut w = World::new();
    w.append(&[(0, 0, Some(1.0))]);
    w.sync();
    w.checkpoint().unwrap();
    let f = w.files.as_mut().unwrap();
    let r = f.checkpoint(
        Position::default(),
        pos(1),
        BTreeMap::new(),
        &BTreeSet::new(),
        0,
        Duration::ZERO,
    );
    assert!(matches!(r, Err(WriteError::Backwards { .. })), "{r:?}");
}

#[test]
fn a_start_removes_temporary_files_unlisted_parts_and_roots_older_than_the_previous() {
    let mut w = World::new();
    for i in 0..4 {
        w.append(&[(0, i * QUARTER, Some(1.0))]);
        w.sync();
        w.checkpoint().unwrap();
    }
    let dir = w.dir.join(DIR);
    for name in [
        "x.kvt.tmp".to_string(),
        part_name(1, 99),
        part_name(2, 1),
        "root-0000000000000001.kvr".to_string(),
    ] {
        let f = w.fs.open(&dir.join(&name), OpenMode::CreateNew).unwrap();
        f.write_at(b"junk", 0).unwrap();
    }
    w.stop();
    w.start();
    let names: BTreeSet<String> =
        w.fs.read_dir(&dir)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
    let roots: Vec<&String> = names.iter().filter(|n| n.starts_with("root-")).collect();
    assert_eq!(roots, [&root_name(3), &root_name(4)]);
    assert_eq!(
        names.iter().filter(|n| n.ends_with(".kvt")).count(),
        4,
        "{names:?}"
    );
    assert_eq!(w.temporaries(), 0);
    w.check();
}

#[test]
fn a_start_falls_back_to_the_previous_root_when_the_newest_is_damaged_or_names_a_short_part() {
    let mut w = World::new();
    w.append(&[(0, 0, Some(1.0))]);
    w.sync();
    w.checkpoint().unwrap();
    w.append(&[(0, 60 * S, Some(2.0))]);
    w.sync();
    w.checkpoint().unwrap();
    let dir = w.dir.join(DIR);
    w.fs.poke(&dir.join(root_name(2)), 40, &[0xff; 4]);
    w.stop();
    w.start();
    assert_eq!(w.files.as_ref().unwrap().root().seq, 1);
    assert_eq!(
        w.dirty.len(),
        1,
        "the stretch above the older K is dirty again"
    );
    w.renamed.clear();
    w.check();
    w.checkpoint().unwrap();
    w.check();
    let newest = w
        .files
        .as_ref()
        .unwrap()
        .root()
        .parts
        .last()
        .unwrap()
        .file_id;
    let p = dir.join(part_name(1, newest));
    let len = w.fs.read(&p).unwrap().len();
    let f = w.fs.open(&p, OpenMode::ReadWrite).unwrap();
    f.set_len(len as u64 - 1).unwrap();
    drop(f);
    let k = w.files.as_ref().unwrap().root().seq;
    w.stop();
    w.start();
    assert!(w.files.as_ref().unwrap().root().seq < k);
    w.renamed.clear();
    w.check();
}

#[test]
fn a_root_of_another_reset_stamp_or_above_the_mark_is_not_continued() {
    let mut w = World::new();
    w.append(&[(0, 0, Some(1.0))]);
    w.sync();
    w.checkpoint().unwrap();
    let copy = Arc::new(w.fs.snapshot());
    let mut other = id();
    other.reset = ResetStamp {
        sequence: 1,
        nonce: 9,
    };
    let (f, r) = TreeFiles::start(
        w.fs.clone(),
        &w.dir,
        other,
        Opening {
            mark: w.synced,
            fresh: false,
            wall: 0,
            now: Duration::ZERO,
        },
        |_| false,
    )
    .unwrap();
    assert_eq!((r.continued, r.refused), (None, 1));
    assert!(
        f.root().covered.is_empty(),
        "a store with facts and no root covers nothing"
    );
    assert_eq!(
        f.root().seq,
        1,
        "the next root's name is past every root on disk"
    );
    let fs = copy;
    let (_, r) = TreeFiles::start(
        fs,
        &w.dir,
        id(),
        Opening {
            mark: Position::default(),
            fresh: false,
            wall: 0,
            now: Duration::ZERO,
        },
        |_| false,
    )
    .unwrap();
    assert_eq!(r.continued, None);
}

#[test]
fn a_loss_below_k_raises_the_global_floor_and_retires_every_part() {
    let mut w = World::new();
    w.append(&[(0, 0, Some(1.0)), (1, DAY, Some(1.0))]);
    w.sync();
    w.checkpoint().unwrap();
    w.stop();
    let (f, r) = TreeFiles::start(
        w.fs.clone(),
        &w.dir,
        id(),
        Opening {
            mark: w.synced,
            fresh: false,
            wall: 0,
            now: Duration::ZERO,
        },
        |k| k >= pos(1),
    )
    .unwrap();
    assert!(r.discarded);
    let root = f.root();
    assert_eq!((root.seq, root.floor), (2, 2));
    assert!(root.parts.is_empty() && root.covered.is_empty());
    assert_eq!(root.retired.len(), 2);
    let again = Root::decode(&w.fs.read(&w.dir.join(DIR).join(root_name(2))).unwrap()).unwrap();
    assert_eq!(
        &again, root,
        "the discarding root is on disk before the start returns"
    );
}

#[test]
fn retired_parts_are_unlinked_only_after_the_grace_time_and_when_no_kept_root_lists_them() {
    let mut w = World::with_grace(Duration::from_secs(600));
    for i in 0..3 {
        w.append(&[(0, i * QUARTER, Some(1.0))]);
        w.sync();
        w.checkpoint().unwrap();
    }
    let f = w.files.as_mut().unwrap();
    let root = f.root().clone();
    let policy = Policy {
        late_window: 0,
        minute_keep: i64::MAX / 2,
    };
    let now_t = 30 * DAY;
    let plan = set::plan(&root, now_t, &policy)
        .into_iter()
        .next()
        .expect("a merge of the day's deltas");
    let parts: Vec<Part> = f
        .read_parts()
        .into_iter()
        .map(|(_, p)| p.unwrap())
        .collect();
    let template = parts[0].header;
    let mut out = set::merge(&root, &parts, &plan, &template, |_| Some(params())).unwrap();
    for p in &mut out {
        p.header.file_id = f.take_file_id();
    }
    let mut next = root.clone();
    next.parts.retain(|p| !plan.inputs.contains(&p.file_id));
    next.retired
        .extend(plan.inputs.iter().map(|&file_id| crate::root::Retired {
            file_id,
            retired_at: now_t,
        }));
    let at = w.now;
    f.publish(next, out, w.wall, at).unwrap();
    let dir = w.dir.join(DIR);
    let exists = |w: &World, id: u64| w.fs.read(&dir.join(part_name(1, id))).is_some();
    let f = w.files.as_mut().unwrap();
    assert_eq!(f.remove_retired(at + Duration::from_secs(599)), 0);
    assert_eq!(
        f.remove_retired(at + Duration::from_secs(600)),
        0,
        "the previous root still lists the inputs as live"
    );
    w.check();
    w.append(&[(1, 0, Some(1.0))]);
    w.sync();
    w.checkpoint().unwrap();
    assert!(
        plan.inputs.iter().all(|&i| exists(&w, i)),
        "too soon after the publish"
    );
    w.now += Duration::from_secs(600);
    let now = w.now;
    let f = w.files.as_mut().unwrap();
    assert_eq!(f.remove_retired(now), plan.inputs.len());
    assert!(plan.inputs.iter().all(|&i| !exists(&w, i)));
    w.check();
    w.append(&[(2, 0, Some(1.0))]);
    w.sync();
    w.checkpoint().unwrap();
    assert!(
        w.files.as_ref().unwrap().root().retired.is_empty(),
        "the next root drops what was unlinked"
    );
    w.check();
}

#[test]
fn a_removal_a_reader_refuses_is_tried_again() {
    let mut w = World::with_grace(Duration::ZERO);
    w.append(&[(0, 0, Some(1.0))]);
    w.sync();
    w.checkpoint().unwrap();
    let (wall, now) = (w.wall, w.now);
    let f = w.files.as_mut().unwrap();
    f.discard(wall, now).unwrap();
    let id0 = f.root().retired[0].file_id;
    w.fs.fail_nth(FaultOp::Remove, ".kvt", 0, io::ErrorKind::PermissionDenied);
    f.discard(wall, now).unwrap();
    assert!(
        w.fs.read(&w.dir.join(DIR).join(part_name(1, id0)))
            .is_some()
    );
    let f = w.files.as_mut().unwrap();
    assert_eq!(f.remove_retired(now), 1);
    assert!(
        w.fs.read(&w.dir.join(DIR).join(part_name(1, id0)))
            .is_none()
    );
}

#[test]
fn a_failed_directory_sync_fails_the_checkpoint_and_the_renamed_root_becomes_the_root() {
    let mut w = World::new();
    w.append(&[(0, 0, Some(1.0))]);
    w.sync();
    w.fs.fail_nth(FaultOp::SyncDir, "tree", 0, io::ErrorKind::Other);
    let r = w.checkpoint();
    assert!(matches!(r, Err(WriteError::Io { op: "sync", .. })), "{r:?}");
    assert_eq!(
        w.files.as_ref().unwrap().root().seq,
        1,
        "a reader may already hold it"
    );
    assert_eq!(w.dirty.len(), 1, "the job is not done");
    w.check();
    w.checkpoint().unwrap();
    assert_eq!(w.files.as_ref().unwrap().root().seq, 2);
    w.check();
}
