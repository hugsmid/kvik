//! Fold time per reading and stored node sizes (Q-180, `docs/format-tree.md` T.3):
//! `cargo run --release -p kvik-tree --example fold_cost`.

use std::hint::black_box;
use std::time::Instant;

use kvik_tree::{Kind, Node, Params, Value, encode, fold, merge_all};

const SECOND: i64 = 1_000_000_000;
const MINUTE: i64 = 60 * SECOND;
const T0: i64 = 1_767_225_600_000_000_000;

/// A gauge read every 5 s at 0.01 resolution, 12 readings a minute.
fn minute(m: i64, value: impl Fn(i64) -> f64) -> Vec<(i64, Value)> {
    (0..12)
        .map(|k| {
            let i = m * 12 + k;
            (T0 + m * MINUTE + k * 5 * SECOND, Value::F64(value(i)))
        })
        .collect()
}

fn per_reading(p: &Params, minutes: &[Vec<(i64, Value)>]) -> f64 {
    let rounds = 20;
    let start = Instant::now();
    for _ in 0..rounds {
        for (m, r) in minutes.iter().enumerate() {
            let a = T0 + m as i64 * MINUTE;
            black_box(fold(p, a, a + MINUTE, black_box(r)));
        }
    }
    start.elapsed().as_nanos() as f64 / (rounds * minutes.len() * 12) as f64
}

fn main() {
    let gauge = Params::new(Kind::Gauge, Some(15 * SECOND));
    let counter = Params::new(Kind::Counter, Some(15 * SECOND));
    let temps: Vec<_> = (0..1_000)
        .map(|m| minute(m, |i| 20.0 + (i % 397) as f64 / 100.0))
        .collect();
    let meter: Vec<_> = (0..1_000)
        .map(|m| minute(m, |i| 12_345.0 + i as f64 * 0.125))
        .collect();
    println!(
        "gauge fold: {:.2} µs per reading",
        per_reading(&gauge, &temps) / 1_000.0
    );
    println!(
        "counter fold: {:.2} µs per reading",
        per_reading(&counter, &meter) / 1_000.0
    );

    let one = fold(&gauge, T0, T0 + MINUTE, &temps[0]);
    let quarter: Vec<Node> = temps[..15]
        .iter()
        .enumerate()
        .map(|(m, r)| {
            let a = T0 + m as i64 * MINUTE;
            fold(&gauge, a, a + MINUTE, r)
        })
        .collect();
    let quarter = merge_all(&gauge, T0, &quarter);
    let mut worst = temps[0].clone();
    worst[3].1 = Value::F64(5e-324);
    let worst = fold(&gauge, T0, T0 + MINUTE, &worst);
    let size = |n: &Node| encode(n).unwrap().len();
    println!("1-minute gauge node, 12 readings: {} B", size(&one));
    println!("15-minute gauge node, 180 readings: {} B", size(&quarter));
    println!(
        "1-minute gauge node with one subnormal reading: {} B",
        size(&worst)
    );
}
