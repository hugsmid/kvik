//! Fold time per reading, stored node sizes, entry sizes in a part and the cost of a decode and merge (Q-180,
//! `docs/format-tree.md` T.3, T.9): `cargo run --release -p kvik-tree --example fold_cost`.

use std::hint::black_box;
use std::time::Instant;

use kvik_log::header::FileHeader;
use kvik_log::writer::Position;
use kvik_tree::part::{self, Class, Entry, Part, Run};
use kvik_tree::{Kind, LEVELS, Node, Params, Value, decode, encode, fold, merge, merge_all};

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
    let temps: Vec<_> = (0..1_440)
        .map(|m| minute(m, |i| 20.0 + (i % 397) as f64 / 100.0))
        .collect();
    let meter: Vec<_> = (0..1_000)
        .map(|m| minute(m, |i| 12_345.0 + i as f64 * 0.125))
        .collect();
    println!(
        "gauge fold: {:.2} µs per reading",
        per_reading(&gauge, &temps[..1_000]) / 1_000.0
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
    part_costs(&gauge, &temps);
}

/// A day part of one 5 s gauge at every level of [`LEVELS`]: bytes per entry at each level, and the node bytes alone.
fn day_part(p: &Params, temps: &[Vec<(i64, Value)>]) -> (Part, Vec<Vec<Node>>) {
    let minutes: Vec<Node> = temps
        .iter()
        .take(1_440)
        .enumerate()
        .map(|(m, r)| {
            let a = T0 + m as i64 * MINUTE;
            fold(p, a, a + MINUTE, r)
        })
        .collect();
    let mut levels = vec![minutes];
    for l in &LEVELS[1..] {
        let prev = levels.last().unwrap();
        let k = (l.width / (prev[0].end - prev[0].start)) as usize;
        let next = prev
            .chunks(k)
            .map(|c| merge_all(p, c[0].start, c))
            .collect();
        levels.push(next);
    }
    let runs = LEVELS
        .iter()
        .zip(&levels)
        .map(|(l, nodes)| Run {
            series: 1_234,
            level: l.id,
            entries: nodes
                .iter()
                .enumerate()
                .map(|(i, n)| Entry {
                    start: n.start,
                    stale: false,
                    as_of: Position {
                        generation: 1,
                        sequence: 5_000_000 + i as u64 * 12,
                    },
                    node: Some(n.clone()),
                })
                .collect(),
        })
        .collect();
    let part = Part {
        header: FileHeader::new(part::KIND, 1, 1, T0, [0; 16]),
        seq: 1,
        class: Class::Merged,
        period_start: T0,
        period_len: 1_440 * MINUTE as u64,
        through: Position {
            generation: 1,
            sequence: 6_000_000,
        },
        levels: LEVELS.to_vec(),
        runs,
    };
    (part, levels)
}

fn part_costs(p: &Params, temps: &[Vec<(i64, Value)>]) {
    let (day, levels) = day_part(p, temps);
    for (l, nodes) in LEVELS.iter().zip(&levels) {
        let mut one = day.clone();
        one.runs.retain(|r| r.level == l.id);
        let empty = Part {
            runs: Vec::new(),
            ..day.clone()
        };
        let bytes = one.encode().unwrap().len() - empty.encode().unwrap().len();
        let node: usize = nodes.iter().map(|n| encode(n).unwrap().len()).sum();
        println!(
            "level {} ({} min): {} entries, {:.1} B an entry in a part, {:.1} B of it node bytes",
            l.id,
            l.width / MINUTE,
            nodes.len(),
            bytes as f64 / nodes.len() as f64,
            node as f64 / nodes.len() as f64,
        );
    }
    let bytes = day.encode().unwrap();
    let entries: usize = day.runs.iter().map(|r| r.entries.len()).sum();
    println!("day part, all levels: {} B, {entries} entries", bytes.len());

    let rounds = 20;
    let start = Instant::now();
    for _ in 0..rounds {
        black_box(Part::decode(black_box(&bytes)).unwrap());
    }
    let ns = start.elapsed().as_nanos() as f64 / (rounds * entries) as f64;
    println!(
        "whole-part decode, every check: {:.2} µs an entry",
        ns / 1_000.0
    );

    let quarters: Vec<Vec<u8>> = levels[1].iter().map(|n| encode(n).unwrap()).collect();
    let rounds = 200;
    let start = Instant::now();
    for _ in 0..rounds {
        let mut acc = Node::empty(Kind::Gauge, T0, T0);
        for b in &quarters {
            let n = decode(black_box(b)).unwrap();
            acc = merge(p, &acc, &n);
        }
        black_box(acc);
    }
    let ns = start.elapsed().as_nanos() as f64 / (rounds * quarters.len()) as f64;
    println!(
        "decode and merge of a 15-minute gauge node: {:.2} µs",
        ns / 1_000.0
    );

    let decoded: Vec<Node> = quarters.iter().map(|b| decode(b).unwrap()).collect();
    let start = Instant::now();
    for _ in 0..rounds {
        let mut acc = Node::empty(Kind::Gauge, T0, T0);
        for n in &decoded {
            acc = merge(p, &acc, black_box(n));
        }
        black_box(acc);
    }
    let merge_ns = start.elapsed().as_nanos() as f64 / (rounds * decoded.len()) as f64;
    println!("of which merge: {:.2} µs", merge_ns / 1_000.0);
}
