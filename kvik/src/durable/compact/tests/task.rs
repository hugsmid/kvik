use std::collections::BTreeSet;

use kvik_log::content::Item;
use kvik_log::facts::Value;
use kvik_log::setaside::SetAside;
use kvik_log::writer::share::{Background, Next};
use proptest::prelude::*;

use super::super::Output;
use super::super::plan::Candidate;
use super::super::reference::{place, take, verified_or_removed, write};
use super::super::run::{MINIMAL_MEMORY, RunInput};
use super::super::segment::PIECE;
use super::super::task::{RunTask, window};
use super::{
    DAY, TestBatch, batch, input, put_segment, reading, restored_after_first, segment_bytes,
    segments, temp_dir,
};

type Segments = Vec<(u32, Vec<TestBatch>)>;

fn put_all(dir: &std::path::Path, segs: &[(u32, Vec<TestBatch>)]) -> Vec<Candidate> {
    segs.iter()
        .enumerate()
        .map(|(i, (g, b))| {
            let f = i as u64 + 1;
            put_segment(dir, *g, f, &segment_bytes(*g, f, b))
        })
        .collect()
}

/// Steps a run to its end, checking each step's declared device call; returns the outputs and the peak memory.
fn stepped(inp: RunInput<'_>, cands: Vec<Candidate>, memory: u64) -> (Vec<Output>, u64) {
    let mut task = RunTask::new(inp, cands, memory);
    while !task.step_run() {}
    let peak = task.peak();
    (task.outcome().unwrap().unwrap(), peak)
}

fn file_bytes(outputs: &[Output]) -> Vec<(u64, Vec<u8>)> {
    outputs
        .iter()
        .map(|o| (o.id.file_id, std::fs::read(&o.path).unwrap()))
        .collect()
}

fn same_as_whole_run(
    segs: &[(u32, Vec<TestBatch>)],
    set_aside: &SetAside,
    weeks: &BTreeSet<i64>,
    memory: u64,
) {
    let (a, b) = (temp_dir("stepped"), temp_dir("whole"));
    let (ca, cb) = (put_all(&a, segs), put_all(&b, segs));
    let mut task = RunTask::new(input(&a, set_aside, weeks), ca, memory);
    while !task.step_run() {}
    let got = task.outcome().unwrap();
    let inp = input(&b, set_aside, weeks);
    let want = take(&inp, &cb).and_then(|taken| {
        let parts = place(&inp, &taken)?;
        let written = write(&inp, &taken, parts)?;
        verified_or_removed(&inp, &taken, &written)
    });
    let (got, want) = match (got, want) {
        (Ok(g), Ok(w)) => (g, w),
        (g, w) => {
            assert_eq!(format!("{:?}", g.err()), format!("{:?}", w.err()));
            std::fs::remove_dir_all(&a).unwrap();
            std::fs::remove_dir_all(&b).unwrap();
            return;
        }
    };
    assert_eq!(file_bytes(&got), file_bytes(&want));
    let strip = |o: &[Output]| {
        o.iter()
            .map(|o| (o.id, o.layout.clone(), o.len, o.sha256))
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&got), strip(&want));
    std::fs::remove_dir_all(&a).unwrap();
    std::fs::remove_dir_all(&b).unwrap();
}

proptest! {
    #[test]
    fn the_stepped_run_writes_the_whole_runs_bytes_with_any_number_of_passes(
        segs in segments(),
        merged in any::<bool>(),
        restored in any::<bool>(),
        memory in prop_oneof![Just(0u64), Just(MINIMAL_MEMORY)],
    ) {
        let weeks = if merged { BTreeSet::from([3]) } else { BTreeSet::new() };
        let set_aside = if restored { restored_after_first() } else { SetAside::new(vec![]) };
        same_as_whole_run(&segs, &set_aside, &weeks, memory);
    }
}

#[test]
fn a_backfill_over_more_than_sixteen_partitions_takes_several_windows() {
    let segs = vec![(
        0,
        (0..40)
            .map(|d| {
                batch(
                    1 + 3 * d as u64,
                    400 * DAY,
                    vec![
                        reading(1, d * 9 * DAY + 5, Value::F64(1)),
                        reading(2, d * 9 * DAY + 5, Value::F64(2)),
                        reading(2, d * 9 * DAY + 5, Value::F64(3)),
                    ],
                )
            })
            .collect(),
    )];
    same_as_whole_run(&segs, &SetAside::new(vec![]), &BTreeSet::new(), 0);
    same_as_whole_run(
        &segs,
        &SetAside::new(vec![]),
        &BTreeSet::new(),
        MINIMAL_MEMORY,
    );
}

#[test]
fn each_step_declares_at_most_one_piece_of_device_work() {
    let dir: &'static std::path::Path = Box::leak(temp_dir("steps").into_boxed_path());
    let set_aside: &'static SetAside = Box::leak(Box::new(SetAside::new(vec![])));
    let weeks: &'static BTreeSet<i64> = Box::leak(Box::new(BTreeSet::new()));
    let segs = vec![(
        0,
        (0..200)
            .map(|i| {
                let items = (0..50)
                    .map(|k| reading(k, 29 * DAY + i * 1_000, Value::Text("t".repeat(200))))
                    .collect();
                batch(1 + 50 * i as u64, 30 * DAY, items)
            })
            .collect::<Vec<_>>(),
    )];
    let cands = put_all(dir, &segs);
    let mut task = RunTask::new(input(dir, set_aside, weeks), cands, MINIMAL_MEMORY);
    let (mut device, mut steps) = (0u64, 0u64);
    loop {
        match task.next() {
            Next::Idle => break,
            Next::Device(n) => {
                assert!(n <= PIECE as u64, "a step of {n} bytes");
                device += 1;
            }
            Next::Cpu => {}
        }
        task.step();
        steps += 1;
    }
    assert!(task.outcome().unwrap().is_ok());
    assert!(
        device > 20 && steps > device,
        "{device} device steps of {steps}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Segments of about 3 MB at most, `n` batches in all of `per` facts each from `item(batch, k)`.
fn shape(n: u64, per: u64, item: impl Fn(u64, u64) -> Item) -> Vec<(u32, Vec<TestBatch>)> {
    let batches: Vec<TestBatch> = (0..n)
        .map(|b| {
            batch(
                1 + b * per,
                400 * DAY,
                (0..per).map(|k| item(b, k)).collect(),
            )
        })
        .collect();
    let per_segment = (3_000_000 / (per * 300)).max(1) as usize;
    batches
        .chunks(per_segment)
        .map(|c| (0, c.to_vec()))
        .collect()
}

fn peak_of(tag: &str, segs: &[(u32, Vec<TestBatch>)]) -> u64 {
    let dir = temp_dir(tag);
    let set_aside = SetAside::new(vec![]);
    let weeks = BTreeSet::new();
    let len: u64 = put_all(&dir, segs)
        .iter()
        .map(|c| std::fs::metadata(&c.path).unwrap().len())
        .sum();
    let (out, peak) = stepped(
        input(&dir, &set_aside, &weeks),
        put_all(&dir, segs),
        MINIMAL_MEMORY,
    );
    eprintln!(
        "{tag}: {len} bytes of log, {} outputs, peak {peak} bytes",
        out.len()
    );
    std::fs::remove_dir_all(&dir).unwrap();
    peak
}

#[test]
fn peak_memory_stays_within_the_minimal_cap_on_the_worst_shapes() {
    let shapes: [(&str, Segments); 4] = [
        (
            "many-series",
            shape(200, 1_000, |b, k| {
                reading(
                    (b * 1_000 + k) as u32 / 2,
                    399 * DAY + (k % 2) as i64,
                    Value::F64(k),
                )
            }),
        ),
        (
            "full-text",
            shape(30, 1_000, |b, k| {
                reading(
                    1,
                    399 * DAY + (b * 1_000 + k) as i64,
                    Value::Text("é".repeat(127) + "x"),
                )
            }),
        ),
        (
            "backfill",
            shape(730, 40, |b, k| {
                reading(k as u32, b as i64 * DAY, Value::F64(k))
            }),
        ),
        (
            "one-series",
            shape(350, 1_000, |b, k| {
                reading(1, 399 * DAY + (b * 1_000 + k) as i64 * 100, Value::F64(k))
            }),
        ),
    ];
    for (tag, segs) in &shapes {
        let peak = peak_of(tag, segs);
        assert!(peak <= MINIMAL_MEMORY, "{tag}: peak {peak} above the cap");
    }
}

/// The shapes that reach the cap hardest: many series, many series by many days, many batches by many days.
fn heavy_shapes() -> Vec<(&'static str, Segments)> {
    vec![
        (
            "many-series",
            shape(200, 1_000, |b, k| {
                reading(
                    (b * 1_000 + k) as u32 / 2,
                    399 * DAY + (k % 2) as i64,
                    Value::F64(k),
                )
            }),
        ),
        (
            "many-series-with-revisions",
            shape(150, 1_000, |b, k| {
                reading((b * 1_000 + k) as u32 / 3, 399 * DAY + 5, Value::F64(k))
            }),
        ),
        (
            "series-by-days",
            shape(16, 12_000, |b, k| {
                reading(k as u32, (384 + b as i64) * DAY + 7, Value::F64(k))
            }),
        ),
        (
            "batches-by-days",
            shape(17_000, 15, |b, k| {
                reading(k as u32, (385 + k as i64) * DAY + b as i64, Value::F64(k))
            }),
        ),
        (
            "one-series",
            shape(350, 1_000, |b, k| {
                reading(1, 399 * DAY + (b * 1_000 + k) as i64 * 100, Value::F64(k))
            }),
        ),
        (
            "full-text",
            shape(30, 1_000, |b, k| {
                reading(
                    1,
                    399 * DAY + (b * 1_000 + k) as i64,
                    Value::Text("é".repeat(127) + "x"),
                )
            }),
        ),
    ]
}

const ALLOC_CHILD: &str = "KVIK_COMPACT_ALLOC_CHILD";

/// Runs `allocated_peaks` alone in a process of its own, so that the heap counts the run and nothing else (Q-1841).
#[test]
fn the_run_allocates_within_the_minimal_cap_on_the_heavy_shapes() {
    let name = format!(
        "{}::allocated_peaks",
        module_path!().split_once("::").expect("in a crate").1
    );
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
            &name,
        ])
        .env(ALLOC_CHILD, "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && text.contains("1 passed"), "{text}");
    eprintln!("{text}");
}

#[test]
#[ignore = "run alone by the_run_allocates_within_the_minimal_cap_on_the_heavy_shapes"]
fn allocated_peaks() {
    if std::env::var_os(ALLOC_CHILD).is_none() {
        return;
    }
    let heap = crate::heap();
    let mut over = Vec::new();
    for (tag, segs) in heavy_shapes() {
        let dir = temp_dir(tag);
        let set_aside = SetAside::new(vec![]);
        let weeks = BTreeSet::new();
        let cands = put_all(&dir, &segs);
        let base = heap.current_usage();
        heap.reset_peak_usage();
        let mut task = RunTask::new(input(&dir, &set_aside, &weeks), cands, MINIMAL_MEMORY);
        let mut stages: Vec<(&str, usize)> = Vec::new();
        let mut real = 0;
        loop {
            let stage = task.stage();
            let before = heap.peak_usage();
            heap.reset_peak_usage();
            let done = task.step_run();
            let p = heap.peak_usage().saturating_sub(base);
            real = real.max(p).max(before.saturating_sub(base));
            match stages.iter_mut().find(|s| s.0 == stage) {
                Some(s) => s.1 = s.1.max(p),
                None => stages.push((stage, p)),
            }
            if done {
                break;
            }
        }
        let real = real as u64;
        let counted = task.peak();
        task.outcome().unwrap().unwrap();
        eprintln!("{tag}: allocated {real}, counted {counted}, by stage {stages:?}");
        if real > MINIMAL_MEMORY || counted > MINIMAL_MEMORY {
            over.push((tag, real, counted));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
    assert!(over.is_empty(), "above the cap: {over:?}");
}

/// A writer at its fullest: its index reserved for `series`, `batches` commit-time rows, and a block of rows held.
fn fullest_writer(series: u64, batches: u64) -> u64 {
    use kvik_chunk::file::{
        BLOCK_ROWS, Batch, Class, Description, Fact, FileWriter, Identity, Input, Level, Reach,
    };
    let per = (BLOCK_ROWS as u64).div_ceil(batches);
    let rows = batches * per;
    let d = Description {
        class: Class::Current,
        level: Level::L0,
        period_start: 0,
        period_len: DAY,
        run_id: 1,
        run_count: 1,
        inputs: vec![Input {
            kind: 1,
            file_id: 1,
        }],
        name_range: None,
        positions: vec![(1, rows)],
        has_entries: false,
        has_set_aside: false,
        complete: true,
    };
    let batches: Vec<Batch> = (0..batches)
        .map(|b| Batch {
            generation: 1,
            first_sequence: b * per,
            last_sequence: b * per + per - 1,
            received_at: 0,
            commit_time: 0,
        })
        .collect();
    let id = Identity {
        store_id: [1; 16],
        generation: 1,
        file_id: 1,
    };
    let mut w = FileWriter::new(Vec::new(), id, 0, &d, batches).unwrap();
    w.reserve_index(usize::try_from(2 * series).unwrap());
    w.begin_series(1, Some("s"), Reach::Point, Vec::new())
        .unwrap();
    let mut most = w.held_bytes();
    for i in 0..BLOCK_ROWS as u64 {
        w.push(Fact::value(1, (1, i), i as i64, 1, i)).unwrap();
        most = most.max(w.held_bytes());
    }
    assert!(w.data_blocks() <= 1);
    most
}

#[test]
fn a_window_of_writers_at_their_fullest_holds_at_most_half_the_cap() {
    for (cap, series, batches) in [
        (MINIMAL_MEMORY, 1_000, 1_000),
        (MINIMAL_MEMORY, 20_000, 17_000),
        (MINIMAL_MEMORY / 4, 10, 10),
        (8 * MINIMAL_MEMORY, 100, 100),
    ] {
        let (open, per_writer) = window(cap, series, batches);
        let held = fullest_writer(series, batches) + 2 * PIECE as u64;
        assert!(
            held <= per_writer,
            "a writer holds {held}, reserved {per_writer}"
        );
        assert!(
            open as u64 * held <= cap / 2,
            "{open} writers of {held} bytes under a cap of {cap}"
        );
    }
}
