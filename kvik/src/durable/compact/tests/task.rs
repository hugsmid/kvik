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
use super::super::task::RunTask;
use super::{
    DAY, TestBatch, batch, input, put_segment, reading, restored_after_first, segment_bytes,
    segments, temp_dir,
};

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

/// A segment of `n` batches of `per` facts each from `item(batch, k)`.
fn shape(n: u64, per: u64, item: impl Fn(u64, u64) -> Item) -> Vec<(u32, Vec<TestBatch>)> {
    vec![(
        0,
        (0..n)
            .map(|b| {
                batch(
                    1 + b * per,
                    400 * DAY,
                    (0..per).map(|k| item(b, k)).collect(),
                )
            })
            .collect(),
    )]
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
    let shapes: [(&str, Vec<(u32, Vec<TestBatch>)>); 4] = [
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
