//! Runs the encoding measurement and prints Markdown tables.
//!
//! `cargo run --release -p encodings [-- --reps N] [-- --quick]`

use std::time::Instant;

use encodings::data::DAY_SECONDS;
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
        vec![DatasetSpec {
            name: "1 day × 10 series (quick)",
            seed: 1,
            series: 10,
            spans: 1,
            span_seconds: DAY_SECONDS,
        }]
    } else {
        vec![
            DatasetSpec {
                name: "1 day × 100 series",
                seed: 1,
                series: 100,
                spans: 1,
                span_seconds: DAY_SECONDS,
            },
            DatasetSpec {
                name: "30 days × 10 series",
                seed: 2,
                series: 10,
                spans: 30,
                span_seconds: DAY_SECONDS,
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
