//! Invariant T7 (design 7.1b section 4): a chart through the tree equals the one from facts, for every kind and
//! timing, step and edge, with quarter-hours dirty in the stretch, uncovered, in damaged parts or past the credit
//! limit.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::fs::{Fs, OpenMode};
use kvik_tree::part::Part;
use kvik_tree::read::newest_root;
use kvik_tree::root::{Covered, Root};
use kvik_tree::set::{Policy, merge, next_root, plan};
use kvik_tree::write::{DIR, Identity, Opening, TreeFiles, part_name, root_name};
use proptest::prelude::*;

use super::tests::{open, open_with, wait};
use crate::durable::reader::DurableReader;
use crate::durable::windows::nodes::NODES_USED;
use crate::durable::{Writer, buckets_of, windows};
use crate::env::{ManualClock, MemFs};
use crate::read::{ChartData, Resolution};
use crate::series::{IntervalLength, Kind, SeriesSpec, StateValues, Timing, ValueType};
use crate::time::{TimeRange, Timestamp};
use crate::types::{Intent, Quality, Reading, SeriesId, Value};

mod limits;

const S: i64 = 1_000_000_000;
const MIN: i64 = 60 * S;
const HOUR: i64 = 60 * MIN;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    Gauge,
    Counter,
    Named,
    Bool,
    Text,
    Observed,
    Scheduled,
}

const SHAPES: [Shape; 7] = [
    Shape::Gauge,
    Shape::Counter,
    Shape::Named,
    Shape::Bool,
    Shape::Text,
    Shape::Observed,
    Shape::Scheduled,
];

fn spec(shape: Shape) -> SeriesSpec {
    let f64 = ValueType::F64;
    let interval = Kind::Interval {
        value: f64,
        length: IntervalLength::Fixed {
            d: Duration::from_secs(60),
            phase: Duration::ZERO,
        },
    };
    let (kind, name) = match shape {
        Shape::Gauge => (Kind::Gauge { value: f64 }, "g"),
        Shape::Counter => (Kind::Counter { value: f64 }, "c"),
        Shape::Named => (
            Kind::State {
                values: StateValues::Named(vec!["a".into(), "b".into(), "c".into()]),
            },
            "n",
        ),
        Shape::Bool => (
            Kind::State {
                values: StateValues::Bool,
            },
            "b",
        ),
        Shape::Text => (Kind::Text, "t"),
        Shape::Observed => (interval.clone(), "o"),
        Shape::Scheduled => (interval, "s"),
    };
    let s = SeriesSpec::new(format!("x/y/{name}"), kind, "u")
        .expected_interval(Duration::from_secs(30))
        .staleness(Duration::from_secs(150));
    if shape == Shape::Scheduled {
        s.timing(Timing::Scheduled)
    } else {
        s
    }
}

fn value(shape: Shape, code: u8, at: i64) -> Value {
    match shape {
        Shape::Gauge | Shape::Observed | Shape::Scheduled => {
            Value::F64(f64::from(code % 40) * 0.25)
        }
        // Rises with time, and falls back for some codes: a reset.
        Shape::Counter if code.is_multiple_of(11) => Value::F64(f64::from(code % 5)),
        Shape::Counter => Value::F64((at / MIN) as f64 * 0.5 + f64::from(code % 3)),
        Shape::Named => Value::State(u16::from(code % 3)),
        Shape::Bool => Value::Bool(code.is_multiple_of(2)),
        Shape::Text => Value::Text(format!("v{}", code % 4)),
    }
}

struct Rig {
    fs: MemFs,
    clock: ManualClock,
    dir: PathBuf,
    w: Arc<Writer>,
    ids: Vec<(SeriesId, Shape)>,
}

impl Drop for Rig {
    /// Q-2132: a failing generated case drops its rig at every shrink step, and a writer left running holds its
    /// threads and its in-memory files.
    fn drop(&mut self) {
        let _ = self.w.shutdown();
    }
}

impl Rig {
    fn new(name: &str) -> Self {
        let fs = MemFs::new();
        let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
        let dir = PathBuf::from(format!("tree-reads/{name}"));
        let w = open(&fs, &clock, &dir);
        let ids = SHAPES
            .iter()
            .map(|&s| (w.define(&spec(s)).unwrap(), s))
            .collect();
        Rig {
            fs,
            clock,
            dir,
            w,
            ids,
        }
    }

    fn now(&self) -> i64 {
        self.clock.wall().0
    }

    /// Emits, per series, readings at `(nanoseconds before now, code)`; codes above 230 are of bad quality, and
    /// interval readings are moved onto their minute.
    fn emit(&self, intent: Intent, readings: &[(i64, u8)]) {
        let now = self.now();
        let mut rs = Vec::new();
        for &(id, shape) in &self.ids {
            for &(ago, code) in readings {
                let mut t = now - ago;
                if matches!(shape, Shape::Observed | Shape::Scheduled) {
                    t -= t.rem_euclid(MIN);
                }
                let mut r = Reading::new(id, Timestamp(t), value(shape, code, t));
                if code > 230 {
                    r.quality = Quality::Bad;
                }
                rs.push(r);
            }
        }
        rs.sort_by_key(|r| (r.t, r.series));
        rs.dedup_by_key(|r| (r.t, r.series));
        wait(self.w.emit(&rs, intent, true).unwrap());
    }

    fn advance(&self, d: i64) {
        self.clock.advance(Duration::from_nanos(d as u64));
    }

    /// A clean shutdown with the reader open, so that it checkpoints, then a start.
    fn checkpoint(&mut self) {
        self.restart(true);
    }

    /// A clean shutdown with the reader open, then a start with the tree on or off.
    fn restart(&mut self, tree: bool) {
        self.w.reader().unwrap();
        self.w.shutdown().unwrap();
        self.w = open_with(&self.fs, &self.clock, &self.dir, tree);
    }

    fn reader(&self) -> Arc<DurableReader> {
        self.w.reader().unwrap()
    }

    fn newest_root(&self) -> Root {
        newest_root(&self.fs, &self.dir, |_| true).expect("a root")
    }

    /// Publishes a copy of the newest root covering only `covered`, as a later root.
    fn narrow(&self, covered: Vec<Covered>) {
        let mut r = self.newest_root();
        r.covered = covered;
        self.publish(r);
    }

    /// Publishes `r` as the root after the newest on disk.
    fn publish(&self, mut r: Root) {
        r.seq = self.newest_root().seq + 1;
        r.header.file_id = r.seq;
        self.write(&root_name(r.seq), &r.encode().unwrap());
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        let f = self
            .fs
            .open(&self.dir.join(DIR).join(name), OpenMode::CreateNew)
            .unwrap();
        f.write_at(bytes, 0).unwrap();
    }

    /// Flips one byte in the middle of a part the newest root lists.
    fn damage(&self, k: usize) {
        let r = self.newest_root();
        let p = r.parts[k % r.parts.len()];
        let path = self
            .dir
            .join(DIR)
            .join(part_name(r.header.generation, p.file_id));
        let f = self.fs.open(&path, OpenMode::ReadWrite).unwrap();
        let at = p.byte_len / 2;
        let mut b = [0u8];
        f.read_at(&mut b, at).unwrap();
        f.write_at(&[b[0] ^ 0x5a], at).unwrap();
    }
}

fn resolutions() -> Vec<Resolution> {
    let step = |m: u64| Resolution::Step(Duration::from_secs(m * 60));
    vec![
        Resolution::Auto { max_points: 40 },
        Resolution::Auto { max_points: 700 },
        Resolution::Auto {
            max_points: 100_000,
        },
        step(15),
        step(45),
        step(60),
        step(240),
        step(1_440),
        step(7),
    ]
}

/// Charts of every series over `ranges` at every resolution, through the tree and from facts, compared; returns the
/// stored nodes the tree reads used.
fn assert_t7(rig: &Rig, ranges: &[(i64, i64)]) -> u64 {
    assert_t7_on(&rig.reader(), rig, ranges)
}

fn assert_t7_on(r: &Arc<DurableReader>, rig: &Rig, ranges: &[(i64, i64)]) -> u64 {
    let ids: Vec<SeriesId> = rig.ids.iter().map(|(id, _)| *id).collect();
    let before = NODES_USED.with(std::cell::Cell::get);
    for &(a, b) in ranges {
        let range = TimeRange::new(Timestamp(a), Timestamp(b));
        for res in resolutions() {
            r.no_tree.store(true, std::sync::atomic::Ordering::Relaxed);
            let want = buckets_of(r, &ids, range, res);
            r.no_tree.store(false, std::sync::atomic::Ordering::Relaxed);
            let got = buckets_of(r, &ids, range, res);
            match (&got, &want) {
                (Ok(g), Ok(w)) => {
                    for ((g, w), (_, shape)) in g.iter().zip(w).zip(&rig.ids) {
                        assert_charts(g, w, *shape, (a, b), res);
                    }
                }
                _ => assert_eq!(format!("{got:?}"), format!("{want:?}"), "{range:?} {res:?}"),
            }
        }
    }
    NODES_USED.with(std::cell::Cell::get) - before
}

fn assert_charts(g: &ChartData, w: &ChartData, shape: Shape, range: (i64, i64), res: Resolution) {
    if let (ChartData::Buckets { buckets: gb, .. }, ChartData::Buckets { buckets: wb, .. }) = (g, w)
    {
        for (x, y) in gb.iter().zip(wb) {
            assert_eq!(x, y, "{shape:?} {range:?} {res:?}");
        }
    }
    assert_eq!(g, w, "{shape:?} {range:?} {res:?}");
}

fn ranges(now: i64) -> Vec<(i64, i64)> {
    vec![
        (now - 50 * HOUR, now + 30 * MIN),
        (now - 26 * HOUR + 7 * MIN + 3 * S, now - 2 * HOUR - 11 * S),
        (now - 3 * HOUR - 29 * MIN, now - 20 * MIN + S),
        (now - 72 * HOUR, now - 24 * HOUR),
        (now - 10 * 24 * HOUR, now + 24 * HOUR),
        (now - 50 * MIN, now),
    ]
}

/// A history over two days: readings, a checkpoint, late readings, revisions and new ones; a second checkpoint when
/// `twice`; then readings the stretch holds.
fn history(rig: &mut Rig, first: &[(i64, u8)], late: &[(i64, u8)], twice: bool) {
    rig.emit(Intent::Normal, first);
    rig.checkpoint();
    rig.advance(HOUR);
    rig.emit(Intent::Normal, late);
    let revised: Vec<(i64, u8)> = first
        .iter()
        .step_by(41)
        .map(|&(ago, c)| (ago + HOUR, c.wrapping_add(7)))
        .collect();
    rig.emit(Intent::Revise, &revised);
    if twice {
        rig.checkpoint();
    }
    rig.advance(7 * MIN + 13 * S);
    rig.emit(Intent::Normal, &[(2 * MIN, 3), (40 * S, 250), (5 * S, 9)]);
    let stretch: Vec<(i64, u8)> = late
        .iter()
        .map(|&(ago, c)| (ago + 9 * MIN, c ^ 1))
        .collect();
    rig.emit(Intent::Normal, &stretch);
}

/// Readings at gaps of 1 s to 10 minutes back from now, `n` of them.
fn spaced(gaps: &[(u16, u8)]) -> Vec<(i64, u8)> {
    let mut ago = 0;
    gaps.iter()
        .map(|&(g, c)| {
            ago += i64::from(g % 600 + 1) * S;
            (ago, c)
        })
        .collect()
}

#[test]
fn charts_through_the_tree_equal_the_fact_path_for_every_kind_step_and_edge() {
    let mut rig = Rig::new("every-kind");
    let first: Vec<(i64, u8)> = (0..2_000i64)
        .map(|k| (k * 83 * S + (k % 7) * S, (k * 37 % 251) as u8))
        .collect();
    let late: Vec<(i64, u8)> = (0..60i64)
        .map(|k| (HOUR + k * 1_931 * S, (k * 13) as u8))
        .collect();
    history(&mut rig, &first, &late, false);
    let used = assert_t7(&rig, &ranges(rig.now()));
    assert!(used > 1_000, "the tree served whole quarter-hours: {used}");
    assert!(rig.reader().tree_counts().served > 0);
}

#[test]
fn charts_equal_the_fact_path_where_the_tree_covers_only_part_of_the_range() {
    let mut rig = Rig::new("uncovered");
    let first: Vec<(i64, u8)> = (0..1_500i64)
        .map(|k| (k * 101 * S, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[(3 * HOUR, 4)], true);
    let now = rig.now();
    let q = 15 * MIN;
    let at = |t: i64| t - t.rem_euclid(q);
    rig.narrow(vec![
        Covered {
            level: 2,
            from: at(now - 40 * HOUR),
            to: at(now - 30 * HOUR),
        },
        Covered {
            level: 2,
            from: at(now - 20 * HOUR) + q,
            to: at(now - 3 * HOUR),
        },
    ]);
    let used = assert_t7(&rig, &ranges(now));
    assert!(used > 100, "covered spans served: {used}");
}

#[test]
fn a_damaged_part_sends_its_period_to_facts_and_charts_still_equal_the_fact_path() {
    let mut rig = Rig::new("damaged");
    let first: Vec<(i64, u8)> = (0..1_500i64)
        .map(|k| (k * 97 * S, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[], true);
    rig.damage(0);
    assert_t7(&rig, &ranges(rig.now()));
    assert!(rig.reader().tree_counts().damaged > 0);
}

#[test]
fn a_reader_in_a_second_process_reads_the_tree_with_no_writer() {
    let mut rig = Rig::new("second");
    let first: Vec<(i64, u8)> = (0..1_500i64)
        .map(|k| (k * 89 * S, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[], true);
    rig.checkpoint();
    let charts = |r: &Arc<DurableReader>| {
        let ids: Vec<SeriesId> = rig.ids.iter().map(|(id, _)| *id).collect();
        let now = rig.now();
        buckets_of(
            r,
            &ids,
            TimeRange::new(Timestamp(now - 40 * HOUR), Timestamp(now)),
            Resolution::Auto { max_points: 200 },
        )
        .unwrap()
    };
    let want = charts(&rig.reader());
    rig.w.shutdown().unwrap();
    let mut env = crate::env::Env::test(rig.clock.clone());
    env.fs = Arc::new(rig.fs.clone());
    env.background_timers = false;
    let hour = Duration::from_secs(3_600);
    let other = Arc::new(DurableReader::open(&rig.dir, &env, hour, hour).unwrap());
    let before = NODES_USED.with(std::cell::Cell::get);
    assert_eq!(charts(&other), want);
    assert!(NODES_USED.with(std::cell::Cell::get) > before);
}

impl Rig {
    /// Every series' node parameters, from the writer's registry.
    fn node_params(&self) -> BTreeMap<u64, kvik_tree::Params> {
        let d = crate::durable::lock(&self.w.decider);
        self.ids
            .iter()
            .map(|(id, _)| {
                let s = d.registry.get(id.0).unwrap();
                (
                    u64::from(id.0),
                    crate::fold::node_params(&windows::params(&s.def)),
                )
            })
            .collect()
    }

    /// Runs every merge the planner gives at `now`, roll-ups included, as the writer's merges publish them.
    fn merge_all(&self, params: &BTreeMap<u64, kvik_tree::Params>, now: i64) -> usize {
        let root = self.newest_root();
        let id = Identity {
            generation: root.header.generation,
            reset: root.reset,
            store_id: root.header.store_id,
        };
        let at = Opening {
            mark: root.checkpoint,
            fresh: false,
            wall: now,
            now: Duration::ZERO,
        };
        let (mut files, _) =
            TreeFiles::start(Arc::new(self.fs.clone()), &self.dir, id, at, |_| false).unwrap();
        let policy = Policy {
            late_window: 0,
            minute_keep: i64::MAX / 4,
        };
        let mut merged = 0;
        for _ in 0..16 {
            let Some(plan) = plan(files.root(), now, &policy).into_iter().next() else {
                break;
            };
            let parts: Vec<Part> = files
                .read_parts()
                .into_iter()
                .map(|(_, p)| p.unwrap())
                .collect();
            let root = files.root().clone();
            let mut out = merge(&root, &parts, &plan, &root.header, |s| {
                params.get(&s).copied()
            })
            .unwrap();
            for p in &mut out {
                p.header.file_id = files.take_file_id();
            }
            files
                .publish(next_root(&root, &plan, &[], now), out, now, Duration::ZERO)
                .unwrap();
            merged += 1;
        }
        merged
    }
}

#[test]
fn a_second_process_reads_through_merges_and_roll_ups_and_equals_the_fact_path() {
    let mut rig = Rig::new("merges");
    let first: Vec<(i64, u8)> = (0..3_000i64)
        .map(|k| (k * 71 * S, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[(5 * HOUR, 1)], true);
    rig.checkpoint();
    let params = rig.node_params();
    let now = rig.now();
    rig.w.shutdown().unwrap();
    let mut env = crate::env::Env::test(rig.clock.clone());
    env.fs = Arc::new(rig.fs.clone());
    env.background_timers = false;
    let hour = Duration::from_secs(3_600);
    let other = Arc::new(DurableReader::open(&rig.dir, &env, hour, hour).unwrap());
    assert!(assert_t7_on(&other, &rig, &ranges(now)) > 0);
    let before = rig.newest_root().parts.len();
    assert!(rig.merge_all(&params, now + 30 * 24 * HOUR) > 0);
    let root = rig.newest_root();
    assert!(
        root.parts.len() < before,
        "{before} parts merged to {}",
        root.parts.len()
    );
    assert!(
        root.covered.iter().any(|c| c.level == 4),
        "day roll-ups covered"
    );
    let used = assert_t7_on(&other, &rig, &ranges(now));
    assert!(used > 0);
    assert_eq!(other.tree_counts().damaged, 0);
}

#[test]
fn a_reader_whose_cut_moves_takes_readings_since_its_last_read_as_dirty() {
    let mut rig = Rig::new("moving-cut");
    let first: Vec<(i64, u8)> = (0..1_500i64)
        .map(|k| (k * 97 * S, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[], true);
    rig.checkpoint();
    assert!(assert_t7(&rig, &ranges(rig.now())) > 0);
    let late: Vec<(i64, u8)> = (0..40i64)
        .map(|k| (2 * HOUR + k * 1_777 * S, 200))
        .collect();
    rig.emit(Intent::Normal, &late);
    assert_t7(&rig, &ranges(rig.now()));
}

#[test]
fn charts_equal_the_fact_path_throughout_a_store_written_with_the_tree_on_then_off_then_on() {
    let mut rig = Rig::new("on-off-on");
    let first: Vec<(i64, u8)> = (0..1_500i64)
        .map(|k| (k * 89 * S, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[], true);
    assert!(assert_t7(&rig, &ranges(rig.now())) > 0);
    rig.restart(false);
    assert!(rig.w.tree.is_none());
    let late: Vec<(i64, u8)> = (0..40i64)
        .map(|k| (2 * HOUR + k * 1_777 * S, 200))
        .collect();
    rig.emit(Intent::Normal, &late);
    rig.emit(Intent::Revise, &[(first[7].0, 9), (first[300].0, 240)]);
    assert!(
        assert_t7(&rig, &ranges(rig.now())) > 0,
        "the old tree still serves"
    );
    rig.restart(false);
    rig.advance(5 * MIN);
    rig.emit(Intent::Normal, &[(MIN, 4), (3 * HOUR + 11 * S, 17)]);
    assert_t7(&rig, &ranges(rig.now()));
    rig.restart(true);
    assert_t7(&rig, &ranges(rig.now()));
    rig.emit(Intent::Revise, &[(late[3].0 + 5 * MIN, 31)]);
    rig.checkpoint();
    assert!(assert_t7(&rig, &ranges(rig.now())) > 0);
}

#[test]
fn a_roll_up_with_a_dirty_or_uncovered_quarter_hour_serves_its_clean_ones_and_equals_the_fact_path()
{
    let mut rig = Rig::new("rollup-dirty");
    let first: Vec<(i64, u8)> = (0..3_000i64)
        .map(|k| (k * 71 * S, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[], true);
    rig.checkpoint();
    let params = rig.node_params();
    let now = rig.now();
    rig.w.shutdown().unwrap();
    assert!(rig.merge_all(&params, now + 30 * 24 * HOUR) > 0);
    // A quarter-hour of a rolled-up day not covered at 15 minutes, its day and two hours still covered.
    let day = (now - 30 * HOUR) - (now - 30 * HOUR).rem_euclid(24 * HOUR);
    let hole = day + 10 * HOUR + 15 * MIN;
    let mut covered = Vec::new();
    for c in rig.newest_root().covered {
        if c.level == 2 && c.from <= hole && hole < c.to {
            covered.push(Covered { to: hole, ..c });
            covered.push(Covered {
                from: hole + 15 * MIN,
                ..c
            });
        } else {
            covered.push(c);
        }
    }
    rig.narrow(covered);
    rig.w = open(&rig.fs, &rig.clock, &rig.dir);
    let mut env = crate::env::Env::test(rig.clock.clone());
    env.fs = Arc::new(rig.fs.clone());
    env.background_timers = false;
    let hour = Duration::from_secs(3_600);
    let other = Arc::new(DurableReader::open(&rig.dir, &env, hour, hour).unwrap());
    // Readings in the stretch inside that day, not in the hole.
    rig.emit(
        Intent::Normal,
        &[
            (now - (day + 3 * HOUR + 7 * S), 9),
            (now - (day + 17 * HOUR), 231),
        ],
    );
    let range = [
        (day - 24 * HOUR, day + 48 * HOUR),
        (day + 7 * MIN, day + 24 * HOUR - 3 * S),
    ];
    assert!(assert_t7_on(&other, &rig, &range) > 0);
}

proptest! {
    // Q-2131: 24 cases unless `PROPTEST_CASES` says otherwise, at most 1,024 a run (Q-2113); Q-2132.
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(24).min(1024),
        max_shrink_iters: 64,
        ..ProptestConfig::default()
    })]

    /// T7 on generated histories: readings at any spacing and quality, late readings and revisions, one or two
    /// checkpoints, a stretch above K, and optionally a covered list with holes.
    #[test]
    fn charts_through_the_tree_equal_the_fact_path_on_generated_histories(
        gaps in prop::collection::vec((any::<u16>(), any::<u8>()), 1..300),
        late in prop::collection::vec((0i64..40 * 3_600, any::<u8>()), 0..30),
        twice in any::<bool>(),
        holes in prop::option::of((1i64..40, 1i64..10)),
    ) {
        let mut rig = Rig::new("generated");
        let late: Vec<(i64, u8)> = late.into_iter().map(|(s, c)| (HOUR + s * S, c)).collect();
        history(&mut rig, &spaced(&gaps), &late, twice);
        let now = rig.now();
        if let Some((from, len)) = holes {
            let q = 15 * MIN;
            let at = |t: i64| t - t.rem_euclid(q);
            rig.narrow(vec![Covered {
                level: 2,
                from: at(now - from * HOUR),
                to: at(now - from * HOUR) + len * HOUR,
            }]);
        }
        assert_t7(&rig, &ranges(now));
    }
}
