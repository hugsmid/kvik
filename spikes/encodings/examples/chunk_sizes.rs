//! Roadmap 6.1: sizes of the `kvik-chunk` block encodings on the 2.1 data sets, beside the spike's
//! "delta/decimal/RLE" candidate on the same chunks. Results in `docs/measurements/6.1-encodings.md`.
//!
//! `cargo run --release -p encodings --example chunk_sizes [-- --reps N]`

use std::time::{Duration, Instant};

use encodings::data::{Generator, NON_DECIMAL_SHAPES, Shape};
use encodings::measure::DatasetSpec;
use encodings::own::Own;
use encodings::{BLOCK_ROWS, Chunk, FLAG_NO_VALUE, ValueKind};
use kvik_chunk::{Block, column, value_type};

/// Bytes per part, as in the 2.1 tables.
#[derive(Default, Clone, Copy)]
struct Sizes {
    t: u64,
    value: u64,
    quality_flags: u64,
    position: u64,
    overhead: u64,
}

impl Sizes {
    fn total(&self) -> u64 {
        self.t + self.value + self.quality_flags + self.position + self.overhead
    }
}

fn block_of(c: &Chunk, rows: std::ops::Range<usize>, scale: u8) -> Block {
    let n = rows.len();
    let vt = match c.kind {
        ValueKind::F64 => value_type::F64,
        ValueKind::I64 => value_type::DECIMAL,
        ValueKind::State => value_type::BOOL,
    };
    Block {
        t: c.t[rows.clone()].to_vec(),
        generation: vec![1; n],
        sequence: c.position[rows.clone()].to_vec(),
        quality: c.quality[rows.clone()].to_vec(),
        flags: c.flags[rows.clone()].to_vec(),
        value_type: vec![vt; n],
        value: rows
            .clone()
            .map(|i| {
                if c.flags[i] & FLAG_NO_VALUE != 0 {
                    0
                } else {
                    c.value[i]
                }
            })
            .collect(),
        heap: vec![],
        changed_at: vec![0; n],
        scale: vec![if vt == value_type::DECIMAL { scale } else { 0 }; n],
    }
}

/// Adds the parts of one body: the 3-byte prefix and each directory entry are overhead.
fn add(body: &[u8], s: &mut Sizes) {
    let count = usize::from(body[2]);
    let mut at = 3;
    let mut dir = Vec::new();
    for _ in 0..count {
        let (id, start) = (body[at], at);
        at += 2;
        let mut len = 0u64;
        let mut shift = 0;
        loop {
            let b = body[at];
            at += 1;
            len |= u64::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        s.overhead += (at - start) as u64;
        dir.push((id, len));
    }
    s.overhead += 3;
    for (id, len) in dir {
        match id {
            column::T => s.t += len,
            column::QUALITY | column::FLAGS => s.quality_flags += len,
            column::GENERATION | column::SEQUENCE => s.position += len,
            _ => s.value += len,
        }
    }
}

fn scale_of(shape: Shape) -> u8 {
    shape.integer_scale().map_or(0, |s| s as u8)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let reps: usize = match args.as_slice() {
        [flag, n] if flag == "--reps" => n.parse().expect("--reps takes a number"),
        [] => 3,
        _ => panic!("usage: chunk_sizes [--reps N]"),
    };
    assert!(reps > 0, "--reps takes a positive number");
    let datasets = vec![
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
    ];
    println!(
        "| Data set | Encoding | Total | `t` | value | quality + flags | position | overhead | Encode, M rows/s | Decode, M rows/s |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for spec in &datasets {
        let mut g = Generator::with_shapes(spec.seed, spec.series, spec.span_seconds, spec.shapes);
        if spec.integers {
            g = g.integers();
        }
        let (mut ours, mut spike) = (Sizes::default(), Sizes::default());
        let mut rows = 0u64;
        let (mut enc_t, mut dec_t) = (vec![Duration::ZERO; reps], vec![Duration::ZERO; reps]);
        for _ in 0..spec.spans {
            for s in g.next_span() {
                let scale = if spec.integers { scale_of(s.shape) } else { 0 };
                for c in [&s.current, &s.revisions] {
                    if c.is_empty() {
                        continue;
                    }
                    rows += c.len() as u64;
                    let (_, sz) = Own::DELTA_DECIMAL_RLE.encode_chunk(c);
                    spike.t += sz.t;
                    spike.value += sz.value;
                    spike.quality_flags += sz.quality_flags;
                    spike.position += sz.position;
                    // The spike's chunk header and block directory, and its 24-byte file directory entry.
                    spike.overhead += sz.overhead + 24;
                    let blocks: Vec<Block> = (0..c.len())
                        .step_by(BLOCK_ROWS)
                        .map(|a| block_of(c, a..(a + BLOCK_ROWS).min(c.len()), scale))
                        .collect();
                    let mut bodies = Vec::new();
                    for r in 0..reps {
                        let t0 = Instant::now();
                        bodies = blocks
                            .iter()
                            .map(|b| b.encode().expect("valid block"))
                            .collect::<Vec<_>>();
                        enc_t[r] += t0.elapsed();
                        let t0 = Instant::now();
                        let decoded: Vec<Block> = bodies
                            .iter()
                            .map(|body| Block::decode(body).expect("decodes"))
                            .collect();
                        dec_t[r] += t0.elapsed();
                        assert!(decoded == blocks, "round trip");
                    }
                    // The chunk layout is roadmap 6.2's; counted here as the spike counts its own
                    // (9 bytes per chunk, 20 per block, 24 per chunk in the file directory).
                    ours.overhead += 9 + 20 * blocks.len() as u64 + 24;
                    for body in &bodies {
                        add(body, &mut ours);
                    }
                }
            }
        }
        enc_t.sort();
        dec_t.sort();
        let per_row = |b: u64| b as f64 / rows as f64;
        let speed = |d: Duration| rows as f64 / d.as_secs_f64() / 1e6;
        for (name, s, speeds) in [
            ("delta/decimal/RLE (2.1 spike)", spike, None),
            (
                "kvik-chunk (6.1)",
                ours,
                Some((speed(enc_t[reps / 2]), speed(dec_t[reps / 2]))),
            ),
        ] {
            let (e, d) = speeds.map_or(("—".to_owned(), "—".to_owned()), |(e, d)| {
                (format!("{e:.0}"), format!("{d:.0}"))
            });
            println!(
                "| {} | {name} | **{:.2}** | {:.2} | {:.2} | {:.2} | {:.2} | {:.3} | {e} | {d} |",
                spec.name,
                per_row(s.total()),
                per_row(s.t),
                per_row(s.value),
                per_row(s.quality_flags),
                per_row(s.position),
                per_row(s.overhead),
            );
        }
    }
}
