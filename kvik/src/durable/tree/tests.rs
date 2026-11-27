use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use kvik_log::fs::{FaultOp, Fs};
use kvik_tree::part::Part;
use kvik_tree::root::Root;
use kvik_tree::set::{Key, Seen, visible};
use kvik_tree::write::{DIR, folded_levels, part_name, read_file, root_name};
use kvik_tree::{LEVELS, Node, Value as TreeValue, fold};

use crate::config::StoreConfig;
use crate::durable::completion::Emitted;
use crate::durable::{Writer, lock, windows};
use crate::env::{Env, ManualClock, MemFs};
use crate::health::{Health, Reason};
use crate::series::{Kind, ParameterChange, RetentionClass, SeriesSpec, ValueType};
use crate::types::{Intent, Quality, Reading, SeriesId, Value};

struct Store {
    fs: MemFs,
    clock: ManualClock,
    dir: PathBuf,
    w: Arc<Writer>,
    id: SeriesId,
    /// The current readings: `t` to value, `None` for bad quality.
    current: BTreeMap<i64, Option<f64>>,
}

pub(super) fn open(fs: &MemFs, clock: &ManualClock, dir: &Path) -> Arc<Writer> {
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(fs.clone());
    env.background_timers = false;
    let config = StoreConfig::new(dir, "Etc/UTC")
        .create_if_missing(true)
        .env(env);
    Writer::open(&config).unwrap()
}

pub(super) fn wait(e: Emitted) {
    if let Emitted::Queued(mut q) = e {
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(r) = q.poll(&mut cx) {
                q.finish(r).unwrap();
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Store {
    fn new(name: &str) -> Self {
        let fs = MemFs::new();
        let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
        let dir = PathBuf::from(format!("tree-unit/{name}"));
        let w = open(&fs, &clock, &dir);
        let spec = SeriesSpec::new(
            "a/b/c",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        )
        .expected_interval(Duration::from_secs(5));
        let id = w.define(&spec).unwrap();
        Store {
            fs,
            clock,
            dir,
            w,
            id,
            current: BTreeMap::new(),
        }
    }

    fn reopen(&mut self) {
        self.w = open(&self.fs, &self.clock, &self.dir);
    }

    /// Emits readings `(seconds before now, value)`, `None` as bad quality.
    fn emit(&mut self, intent: Intent, readings: &[(u64, Option<f64>)]) {
        let now = self.clock.wall();
        let rs: Vec<Reading> = readings
            .iter()
            .map(|&(ago, v)| {
                let mut r = Reading::new(
                    self.id,
                    crate::time::Timestamp(now.0 - ago as i64 * 1_000_000_000),
                    Value::F64(v.unwrap_or(0.0)),
                );
                if v.is_none() {
                    r.quality = Quality::Bad;
                }
                r
            })
            .collect();
        wait(self.w.emit(&rs, intent, true).unwrap());
        for (r, &(_, v)) in rs.iter().zip(readings) {
            self.current.insert(r.t.0, v);
        }
    }

    /// A clean shutdown once the store's reader is open, so that it checkpoints (Q-1950).
    fn stop(&self) {
        self.w.reader().unwrap();
        self.w.shutdown().unwrap();
    }

    fn params(&self) -> kvik_tree::Params {
        let d = lock(&self.w.decider);
        let s = d.registry.get(self.id.0).unwrap();
        crate::fold::node_params(&windows::params(&s.def))
    }
}

/// The newest root on disk, and the visible state of the parts it lists.
fn tree_state(fs: &MemFs, dir: &Path) -> (Root, BTreeMap<Key, Seen>) {
    let tree = dir.join(DIR);
    let newest = fs
        .read_dir(&tree)
        .unwrap()
        .into_iter()
        .filter(|e| e.name.starts_with("root-"))
        .map(|e| e.name)
        .max()
        .expect("a root");
    let root = Root::decode(&read_file(fs, &tree.join(newest)).unwrap()).unwrap();
    let parts: Vec<Part> = root
        .parts
        .iter()
        .map(|r| {
            let path = tree.join(part_name(root.header.generation, r.file_id));
            Part::decode(&read_file(fs, &path).unwrap()).unwrap()
        })
        .collect();
    let vis = visible(&root, &parts).unwrap();
    (root, vis)
}

/// T1 against the readings the test emitted: every non-empty span of the folded levels, and nothing else.
fn assert_t1(
    p: &kvik_tree::Params,
    series: u32,
    current: &BTreeMap<i64, Option<f64>>,
    vis: &BTreeMap<Key, Seen>,
) {
    let mut want: BTreeMap<(u8, i64), Node> = BTreeMap::new();
    for l in folded_levels(&LEVELS) {
        let mut spans: Vec<i64> = current.keys().map(|t| t - t.rem_euclid(l.width)).collect();
        spans.dedup();
        for s in spans {
            let readings: Vec<(i64, TreeValue)> = current
                .range(s..s + l.width)
                .filter_map(|(&t, v)| v.map(|v| (t, TreeValue::F64(v))))
                .collect();
            let n = fold(p, s, s + l.width, &readings);
            if !n.is_empty() {
                want.insert((l.id, s), n);
            }
        }
    }
    let got: BTreeMap<(u8, i64), Node> = vis
        .iter()
        .filter(|((s, _, _), _)| *s == u64::from(series))
        .map(|(&(_, l, start), seen)| ((l, start), seen.node.clone().expect("a node")))
        .collect();
    assert_eq!(got, want);
}

fn history(st: &mut Store) {
    let normal: Vec<(u64, Option<f64>)> = (0..120u64)
        .map(|k| (7_300 - k * 61, (k % 9 != 4).then_some(k as f64 / 4.0)))
        .collect();
    st.emit(Intent::Normal, &normal);
    st.emit(
        Intent::Revise,
        &[
            (7_300 - 61, Some(99.0)),
            (7_300 - 4 * 61, Some(1.5)),
            (7_300 - 5 * 61, None),
        ],
    );
    st.emit(Intent::Normal, &[(6_000, Some(-3.0)), (30, Some(2.0))]);
}

#[test]
fn a_clean_shutdown_brings_every_dirty_quarter_hour_up_to_the_mark() {
    let mut st = Store::new("shutdown");
    history(&mut st);
    let p = st.params();
    st.stop();
    let (root, vis) = tree_state(&st.fs, &st.dir);
    assert!(root.checkpoint.sequence > 0);
    assert_eq!(root.floor, 0);
    assert!(
        vis.values()
            .all(|s| s.as_of.sequence > 0 && s.as_of <= root.checkpoint)
    );
    assert_t1(&p, st.id.0, &st.current, &vis);
    assert!(vis.keys().any(|k| k.1 == 1) && vis.keys().any(|k| k.1 == 2));
}

#[test]
fn a_clean_shutdown_with_the_reader_unopened_leaves_k_and_the_next_start_refills() {
    let mut st = Store::new("unread");
    history(&mut st);
    let p = st.params();
    st.stop();
    let (first, _) = tree_state(&st.fs, &st.dir);
    st.reopen();
    st.emit(Intent::Normal, &[(7_000, Some(5.0)), (5, Some(6.0))]);
    st.w.shutdown().unwrap();
    assert!(
        lock(&st.w.reader).is_none(),
        "the shutdown opened no reader"
    );
    let (unread, _) = tree_state(&st.fs, &st.dir);
    assert_eq!(
        unread.checkpoint, first.checkpoint,
        "no checkpoint without a reader"
    );
    st.reopen();
    st.stop();
    let (root, vis) = tree_state(&st.fs, &st.dir);
    assert!(root.checkpoint > first.checkpoint);
    assert_t1(&p, st.id.0, &st.current, &vis);
}

#[test]
fn a_restart_continues_the_root_and_late_readings_recompute_their_quarter_hours() {
    let mut st = Store::new("restart");
    history(&mut st);
    let p = st.params();
    st.stop();
    let (first, _) = tree_state(&st.fs, &st.dir);
    st.clock.advance(Duration::from_secs(600));
    st.reopen();
    st.emit(Intent::Normal, &[(7_000, Some(5.0)), (5, Some(6.0))]);
    st.emit(Intent::Revise, &[(6_600, Some(7.0))]);
    st.stop();
    let (root, vis) = tree_state(&st.fs, &st.dir);
    assert!(root.seq > first.seq && root.checkpoint > first.checkpoint);
    assert_t1(&p, st.id.0, &st.current, &vis);
}

/// Another process finds the files after this one stopped without a checkpoint: its start refills the dirty set from
/// the log above K (`dirty_from_log`).
#[test]
fn a_start_after_a_stop_refills_the_dirty_set_from_the_log_above_k() {
    let mut st = Store::new("stopped");
    history(&mut st);
    let p = st.params();
    st.stop();
    st.reopen();
    st.emit(Intent::Normal, &[(7_100, Some(8.0)), (3, Some(9.0))]);
    st.emit(Intent::Revise, &[(30, Some(10.0))]);
    let copy = st.fs.snapshot();
    st.stop();
    let w = open(&copy, &st.clock, &st.dir);
    w.reader().unwrap();
    w.shutdown().unwrap();
    let (_, vis) = tree_state(&copy, &st.dir);
    assert_t1(&p, st.id.0, &st.current, &vis);
}

#[test]
fn a_retirement_stops_the_tree_serving_the_series_and_a_retention_change_does_not() {
    let mut st = Store::new("floors");
    history(&mut st);
    let id = st.id;
    let change = ParameterChange {
        retention: Some(RetentionClass::Diagnostic {
            period: Duration::from_secs(86_400 * 40),
        }),
        ..ParameterChange::default()
    };
    st.w.set_parameters(id, &change).unwrap();
    st.stop();
    let (root, vis) = tree_state(&st.fs, &st.dir);
    assert!(
        root.floors.is_empty(),
        "a retention class changes no node (Q-1428)"
    );
    assert!(!vis.is_empty());
    st.reopen();
    st.emit(Intent::Normal, &[(2, Some(4.0))]);
    let t_r = st.clock.wall();
    st.w.retire(id, t_r).unwrap();
    st.stop();
    let (root, vis) = tree_state(&st.fs, &st.dir);
    let tree = st.dir.join(DIR);
    for r in root.parts.iter().filter(|r| r.seq == root.seq) {
        let path = tree.join(part_name(root.header.generation, r.file_id));
        let part = Part::decode(&read_file(&st.fs, &path).unwrap()).unwrap();
        assert!(
            part.runs.iter().all(|x| x.series != u64::from(id.0)),
            "no node of a series being rebuilt is written"
        );
    }
    assert_eq!(root.floors.len(), 1);
    assert!(root.floors[0].rebuilding && root.floors[0].series == u64::from(id.0));
    assert!(
        vis.is_empty(),
        "the series is not served until it is rebuilt"
    );
}

#[test]
fn an_interval_checkpoint_opens_the_stores_reader_and_writes_a_root() {
    let mut st = Store::new("interval");
    st.emit(Intent::Normal, &[(10, Some(1.0)), (5, Some(2.0))]);
    assert!(lock(&st.w.reader).is_none());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        st.clock.advance(Duration::from_secs(16 * 60));
        let tree = st.dir.join(DIR);
        let rooted = st
            .fs
            .read_dir(&tree)
            .is_ok_and(|es| es.iter().any(|e| e.name.ends_with(".kvt")));
        if rooted && st.w.tree.as_ref().unwrap().behind().is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "no interval checkpoint");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        lock(&st.w.reader).is_some(),
        "the job opened the store's reader"
    );
    st.w.shutdown().unwrap();
}

#[test]
fn a_shutdown_step_that_takes_no_checkpoint_does_not_count_as_one() {
    let mut st = Store::new("skipped");
    st.emit(Intent::Normal, &[(10, Some(1.0))]);
    st.w.shutdown().unwrap();
    assert!(lock(&st.w.reader).is_none());
    let job = st.w.tree.clone().unwrap();
    let far = Duration::from_secs(1 << 30);
    job.step(kvik_log::writer::Position::default(), far, true);
    let behind = job
        .behind()
        .expect("tree_behind after intervals without a checkpoint");
    assert!(behind.starts_with("no checkpoint for "), "{behind}");
    drop(st);
    job.step(kvik_log::writer::Position::default(), far, false);
    assert_eq!(
        job.behind().as_deref(),
        Some("tree reader: none"),
        "an interval step with no store to read fails"
    );
}

#[test]
fn a_store_with_series_and_no_tree_starts_a_tree_that_covers_nothing() {
    let mut st = Store::new("untreed");
    history(&mut st);
    st.stop();
    let tree = st.dir.join(DIR);
    for e in st.fs.read_dir(&tree).unwrap() {
        st.fs.remove_file(&tree.join(&e.name)).unwrap();
    }
    st.reopen();
    st.stop();
    let (root, _) = tree_state(&st.fs, &st.dir);
    assert!(
        root.covered.is_empty(),
        "not fresh: covered until backfill (Q-1834, Q-1951)"
    );
}

#[test]
fn failing_tree_writes_degrade_health_with_tree_behind_and_never_stop_ingest() {
    let mut st = Store::new("behind");
    st.emit(Intent::Normal, &[(10, Some(1.0))]);
    for _ in 0..64 {
        st.fs
            .fail_nth(FaultOp::Create, "/tree/", 0, std::io::ErrorKind::Other);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let behind = loop {
        st.clock.advance(Duration::from_secs(16 * 60));
        let reasons = match st.w.health() {
            Health::Degraded { reasons } => reasons,
            _ => Vec::new(),
        };
        if let Some(r) = reasons
            .into_iter()
            .find(|r| matches!(r, Reason::TreeBehind { .. }))
        {
            break r;
        }
        assert!(Instant::now() < deadline, "no tree_behind");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        behind.to_string().starts_with("summary tree behind: "),
        "{behind}"
    );
    st.emit(Intent::Normal, &[(5, Some(2.0))]);
    assert!(st.fs.read(&st.dir.join(DIR).join(root_name(1))).is_none());
    st.w.shutdown().unwrap();
}

#[test]
fn an_interval_checkpoint_after_a_commit_above_the_synced_mark_succeeds() {
    let mut st = Store::new("unsynced");
    st.emit(Intent::Normal, &[(10, Some(1.0))]);
    let r = st.w.reader().unwrap();
    st.clock.advance(Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(300));
    st.emit(Intent::Normal, &[(1, Some(2.0))]);
    let synced = st.w.log().as_ref().unwrap().stats().mark_synced;
    let v = r.snapshot().unwrap();
    assert!(
        v.cut().durable > synced,
        "the commit is above the synced mark"
    );
    let _ = r.end(v);
    let job = st.w.tree.clone().unwrap();
    job.step(synced, Duration::from_secs(1 << 30), false);
    assert_eq!(job.behind(), None);
    st.w.shutdown().unwrap();
}

fn emit_at(st: &Store, t: i64) {
    let r = Reading::new(st.id, crate::time::Timestamp(t), Value::F64(1.0));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        st.w.emit(&[r], Intent::Normal, true).map(wait)
    }));
    assert!(out.is_ok(), "emit panicked at {t}");
}

#[test]
fn a_reading_at_the_least_instants_is_taken_and_checkpointed() {
    let st = Store::new("least");
    emit_at(&st, i64::MIN + 1);
    emit_at(&st, i64::MIN);
    st.stop();
}

#[test]
fn a_reading_at_the_greatest_instant_is_taken_and_checkpointed() {
    let st = Store::new("greatest");
    emit_at(&st, i64::MAX);
    emit_at(&st, i64::MAX - QUARTER_NS);
    st.stop();
}

const QUARTER_NS: i64 = kvik_tree::set::QUARTER;

fn step_at(st: &Store, hours: u64) {
    let job = st.w.tree.clone().unwrap();
    job.step(
        kvik_log::writer::Position::default(),
        Duration::from_secs((1 << 30) + hours * 3_600),
        false,
    );
    assert_eq!(job.behind(), None);
}

fn found_loss(st: &Store, first: u64, last: u64) {
    use crate::health::{LossCause, LossKind, LossReport, MarkState};
    use crate::types::Position;
    let g = st.w.log().as_ref().unwrap().stats().durable.generation;
    let report = LossReport {
        identity: Position::new(g, first),
        kind: LossKind::DataLost,
        cause: LossCause::Corruption,
        first: Some(Position::new(g, first)),
        last: Some(Position::new(g, last)),
        file_id: Some(1),
        bytes: 1,
        readable_commit_blocks: 0,
        mark_state: MarkState::Readable,
        boot_id: None,
        commit_time_before: None,
        commit_time_after: None,
        cursors_missing: Vec::new(),
        recorded: false,
        acknowledged: false,
        absorbed: Vec::new(),
    };
    let at = |sequence| kvik_log::writer::Position {
        generation: g,
        sequence,
    };
    st.w.latch.note(&[crate::durable::loss::Find {
        report,
        file_id: 1,
        span: (0, 1),
        closed: (at(first), at(last)),
    }]);
}

#[test]
fn a_loss_found_while_running_discards_the_tree_once() {
    let mut st = Store::new("loss-once");
    st.emit(Intent::Normal, &[(10, Some(1.0))]);
    st.w.reader().unwrap();
    step_at(&st, 0);
    let (first, _) = tree_state(&st.fs, &st.dir);
    assert!(!first.parts.is_empty());
    found_loss(&st, 1, 1);
    st.emit(Intent::Normal, &[(5, Some(2.0))]);
    step_at(&st, 1);
    let (discarded, _) = tree_state(&st.fs, &st.dir);
    assert!(discarded.floor > first.seq, "the loss raised the floor");
    st.emit(Intent::Normal, &[(4, Some(3.0))]);
    step_at(&st, 2);
    let (next, _) = tree_state(&st.fs, &st.dir);
    assert!(next.seq > discarded.seq);
    assert_eq!(
        next.floor, discarded.floor,
        "the same loss discards nothing again"
    );
    assert!(!next.parts.is_empty());
    st.w.shutdown().unwrap();
}

#[test]
fn a_store_with_facts_and_no_tree_starts_without_marking_its_log() {
    let mut st = Store::new("no-pass");
    for _ in 0..3 {
        st.emit(Intent::Normal, &[(10, Some(1.0)), (5, Some(2.0))]);
        st.clock.advance(Duration::from_secs(86_400));
    }
    st.stop();
    let tree = st.dir.join(DIR);
    for e in st.fs.read_dir(&tree).unwrap() {
        st.fs.remove_file(&tree.join(&e.name)).unwrap();
    }
    st.reopen();
    st.stop();
    let (root, _) = tree_state(&st.fs, &st.dir);
    assert!(root.checkpoint.sequence > 0, "K moved to the cut");
    assert!(root.parts.is_empty(), "{} parts", root.parts.len());
    assert!(root.floors.is_empty(), "{} floors", root.floors.len());
}

#[test]
fn an_idle_store_writes_no_root_at_its_intervals() {
    let mut st = Store::new("idle");
    st.emit(Intent::Normal, &[(10, Some(1.0))]);
    st.w.reader().unwrap();
    step_at(&st, 0);
    let (first, _) = tree_state(&st.fs, &st.dir);
    for h in 1..4 {
        step_at(&st, h);
    }
    let (last, _) = tree_state(&st.fs, &st.dir);
    assert_eq!(last.seq, first.seq);
    st.emit(Intent::Normal, &[(5, Some(2.0))]);
    step_at(&st, 4);
    assert!(tree_state(&st.fs, &st.dir).0.seq > first.seq);
    st.w.shutdown().unwrap();
}

#[test]
fn a_shutdown_before_the_first_step_with_no_reader_open_does_not_start_the_tree() {
    let fs = MemFs::new();
    let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
    let mut env = Env::test(clock);
    env.fs = Arc::new(fs.clone());
    let dir = PathBuf::from("tree-unit/no-start");
    let job = super::Job::new(super::job::Parts {
        dir: dir.clone(),
        env,
        latch: Arc::default(),
        recorded: Vec::new(),
        id: kvik_tree::write::Identity {
            generation: 1,
            reset: Default::default(),
            store_id: [0; 16],
        },
        fresh: false,
        dirty: Arc::default(),
    });
    job.readers(Box::new(|_| Ok(None)));
    fs.create_dir_all(&dir.join(DIR)).unwrap();
    drop(
        fs.open(
            &dir.join(DIR).join("x.tmp"),
            kvik_log::fs::OpenMode::CreateNew,
        )
        .unwrap(),
    );
    assert_eq!(job.step(Default::default(), Duration::ZERO, true), None);
    assert!(fs.read(&dir.join(DIR).join("x.tmp")).is_some());
    assert_eq!(job.behind(), None);
    job.step(Default::default(), Duration::ZERO, false);
    assert!(
        fs.read(&dir.join(DIR).join("x.tmp")).is_none(),
        "an interval step starts it"
    );
}

#[test]
fn a_call_whose_outcome_is_unknown_keeps_its_pairs_marked() {
    let st = Store::new("unknown");
    assert_eq!(st.w.dirty.len(), 0);
    st.fs.fail_syncs(true);
    let r = Reading::new(st.id, st.clock.wall(), Value::F64(1.0));
    if let Ok(Emitted::Queued(mut q)) = st.w.emit(&[r], Intent::Normal, true) {
        let mut cx = Context::from_waker(Waker::noop());
        let out = loop {
            if let Poll::Ready(r) = q.poll(&mut cx) {
                break r;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(out.is_err(), "the sync failed");
    }
    assert_eq!(st.w.dirty.len(), 1);
}

#[test]
fn a_node_changing_entry_committed_under_the_decision_lock_marks_its_series() {
    let st = Store::new("held");
    let t_r = st.clock.wall().0;
    st.w.commit_entry_held(
        &mut lock(&st.w.decider),
        kvik_log::entries::Entry::Retirement { id: st.id.0, t_r },
    )
    .unwrap();
    assert!(st.w.dirty.take().1.contains(&st.id.0));
    st.w.shutdown().unwrap();
}
