//! Runs the encoding measurement and prints Markdown tables.
//!
//! `cargo run --release -p encodings [-- --reps N] [-- --quick]`

use std::time::Instant;

use encodings::data::NON_DECIMAL_SHAPES;
use encodings::measure::{DatasetSpec, candidates, report, run};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut reps = 5;
    let mut quick = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--reps" => {
                reps = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--reps takes a positive number");
            }
            "--quick" => quick = true,
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(reps > 0, "--reps takes a positive number");
    let datasets = if quick {
        vec![DatasetSpec::decimal("1 day × 10 series (quick)", 1, 10, 1)]
    } else {
        vec![
            DatasetSpec::decimal("1 day × 100 series", 1, 100, 1),
            DatasetSpec::decimal("30 days × 10 series", 2, 10, 30),
            DatasetSpec {
                name: "30 days × 10 series, fixed-scale shapes as i64",
                integers: true,
                ..DatasetSpec::decimal("", 2, 10, 30)
            },
            DatasetSpec {
                name: "30 days × 4 series, non-decimal shapes",
                shapes: &NON_DECIMAL_SHAPES,
                ..DatasetSpec::decimal("", 3, 4, 30)
            },
        ]
    };
    let cands = candidates();
    let t0 = Instant::now();
    for spec in &datasets {
        let r = run(spec, reps, &cands);
        print!("{}", report(&r));
    }
    eprintln!(
        "total {:.1} s with {reps} repetitions",
        t0.elapsed().as_secs_f64()
    );
}
