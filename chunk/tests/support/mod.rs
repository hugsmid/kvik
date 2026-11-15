//! Strategies that build valid blocks by construction, and helpers shared by the test files.

#![allow(dead_code)]

use kvik_chunk::{Block, flag, value_type};
use proptest::prelude::*;

/// One row before it is put into columns; `text` is the row's text when it has one.
#[derive(Clone, Debug)]
pub struct Row {
    pub t: i64,
    pub generation: u32,
    pub sequence: u64,
    pub quality: u8,
    pub flags: u16,
    pub value_type: u8,
    pub value: u64,
    pub text: Option<String>,
    pub changed_at: u64,
    pub scale: u8,
}

/// Columns from rows; texts are appended to the heap in row order and their lengths become the value words.
pub fn block(rows: Vec<Row>) -> Block {
    let mut b = Block::default();
    for r in rows {
        let value = match &r.text {
            Some(s) if r.flags & flag::NO_VALUE == 0 => {
                b.heap.extend_from_slice(s.as_bytes());
                s.len() as u64
            }
            _ if r.flags & flag::NO_VALUE != 0 => 0,
            _ => r.value,
        };
        b.t.push(r.t);
        b.generation.push(r.generation);
        b.sequence.push(r.sequence);
        b.quality.push(r.quality);
        b.flags.push(r.flags);
        b.value_type.push(r.value_type);
        b.value.push(value);
        b.changed_at.push(if r.flags & flag::HAS_CHANGED_AT != 0 {
            r.changed_at
        } else {
            0
        });
        b.scale.push(if r.value_type == value_type::DECIMAL {
            r.scale
        } else {
            0
        });
    }
    b
}

/// `f64` bits: decimals at a few scales, special patterns (NaN payloads, ±0, ±∞, subnormals) and any bits.
pub fn f64_bits() -> impl Strategy<Value = u64> {
    prop_oneof![
        6 => (-1_000_000i64..1_000_000, 0u32..4).prop_map(|(n, e)| (n as f64 / 10f64.powi(e as i32)).to_bits()),
        1 => prop::sample::select(vec![
            0u64,
            (-0.0f64).to_bits(),
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            f64::NAN.to_bits(),
            0x7FF8_0000_DEAD_BEEF,
            0xFFF0_0000_0000_0001,
            1,
            f64::MAX.to_bits(),
            (0.1f64 + 0.2).to_bits(),
        ]),
        2 => any::<u64>(),
    ]
}

/// A value word and text for one value type.
pub fn value(vt: u8) -> BoxedStrategy<(u64, Option<String>)> {
    match vt {
        value_type::F64 => f64_bits().prop_map(|v| (v, None)).boxed(),
        value_type::DECIMAL => prop_oneof![
            4 => -100_000i64..100_000,
            1 => any::<i64>().prop_map(|v| v.max(i64::MIN + 1)),
        ]
        .prop_map(|v| (v as u64, None))
        .boxed(),
        value_type::STATE => prop_oneof![3 => 0u64..4, 1 => 0u64..=65_535]
            .prop_map(|v| (v, None))
            .boxed(),
        value_type::BOOL => (0u64..=1).prop_map(|v| (v, None)).boxed(),
        _ => "\\PC{0,24}"
            .prop_map(|s: String| {
                let mut s = s;
                while s.len() > 255 {
                    s.pop();
                }
                (0, Some(s))
            })
            .boxed(),
    }
}

/// Block-wide choices that make rows look like a real series (or not).
#[derive(Clone, Debug)]
pub struct Shape {
    pub n: usize,
    /// `None`: each row draws its own type.
    pub value_type: Option<u8>,
    pub versioned: bool,
    pub grid: bool,
    pub quality_varies: bool,
    pub flags_vary: bool,
    pub generations: u32,
}

pub fn shape() -> impl Strategy<Value = Shape> {
    (
        prop_oneof![9 => 1usize..=300, 1 => 3_900usize..=4_096],
        prop_oneof![4 => (1u8..=5).prop_map(Some), 1 => Just(None)],
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        prop_oneof![3 => Just(1u32), 1 => 2u32..4],
    )
        .prop_map(
            |(n, value_type, versioned, grid, quality_varies, flags_vary, generations)| Shape {
                n,
                value_type,
                versioned,
                grid,
                quality_varies,
                flags_vary,
                generations,
            },
        )
}

fn row(s: &Shape, i: usize) -> impl Strategy<Value = Row> + use<> {
    let vt = match s.value_type {
        Some(v) => Just(v).boxed(),
        None => (1u8..=5).boxed(),
    };
    let grid = s.grid;
    let base = 1_767_225_600_000_000_000i64 + i as i64 * 1_000_000_000;
    let t = if grid {
        Just(base).boxed()
    } else {
        prop_oneof![3 => (0i64..8_000_000).prop_map(move |j| base + j), 1 => any::<i64>()].boxed()
    };
    let quality = if s.quality_varies {
        (0u8..=3).boxed()
    } else {
        Just(0u8).boxed()
    };
    let flags = if s.flags_vary {
        prop_oneof![3 => Just(0u16), 1 => 0u16..=0xFF].boxed()
    } else {
        Just(0u16).boxed()
    };
    let versioned = s.versioned;
    let gens = s.generations;
    let gen_at = (i as u32 * gens) / (s.n as u32).max(1);
    (t, quality, flags, vt, any::<u64>(), 0u8..=18, 0u64..1_000).prop_flat_map(
        move |(t, quality, flags, vt, changed_at, scale, step)| {
            let flags = if versioned {
                flags | flag::HAS_CHANGED_AT
            } else {
                flags & !flag::HAS_CHANGED_AT
            };
            value(vt).prop_map(move |(value, text)| Row {
                t,
                generation: 1 + gen_at,
                sequence: 1_000 + i as u64 * 1_000 + step,
                quality,
                flags,
                value_type: vt,
                value: if flags & flag::NO_VALUE != 0 {
                    0
                } else {
                    value
                },
                text,
                changed_at,
                scale,
            })
        },
    )
}

/// A valid block.
pub fn any_block() -> impl Strategy<Value = Block> {
    shape().prop_flat_map(|s| {
        let rows: Vec<_> = (0..s.n).map(|i| row(&s, i)).collect();
        rows.prop_map(block)
    })
}

/// A hand-made block of `n` rows of a decimal `f64` series with a slow walk, as a temperature sensor reports.
pub fn temperature(n: usize) -> Block {
    block(
        (0..n)
            .map(|i| {
                let centi = 2_000 + ((i as i64 * 37) % 200) - 100 + (i as i64 / 60);
                Row {
                    t: 1_767_225_600_000_000_000 + i as i64 * 1_000_000_000,
                    generation: 1,
                    sequence: 10 + 4 * i as u64 + (i as u64 * 7) % 3,
                    quality: 0,
                    flags: 0,
                    value_type: value_type::F64,
                    value: (centi as f64 / 100.0).to_bits(),
                    text: None,
                    changed_at: 0,
                    scale: 0,
                }
            })
            .collect(),
    )
}
