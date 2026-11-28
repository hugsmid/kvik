//! Parts and roots built from synthetic readings, fixed and generated, for the file tests.

#![allow(dead_code)]

use kvik_log::header::FileHeader;
use kvik_log::mark::ResetStamp;
use kvik_log::writer::Position;
use kvik_tree::part::{self, Class, Entry, Part, Run};
use kvik_tree::root::{self, Covered, Floor, PartRef, Retired, Root};
use kvik_tree::{Kind, LEVELS, MINUTE, Node, Params, Value, fold};
use proptest::prelude::*;

/// 2026-01-01T00:00Z.
pub const T0: i64 = 1_767_225_600_000_000_000;
pub const SECOND: i64 = 1_000_000_000;
pub const DAY: i64 = 1_440 * MINUTE;
pub const STORE: [u8; 16] = [0x5A; 16];

pub fn part_header(file_id: u64) -> FileHeader {
    FileHeader::new(part::KIND, 3, file_id, T0 + DAY, STORE)
}

pub fn pos(generation: u32, sequence: u64) -> Position {
    Position {
        generation,
        sequence,
    }
}

pub fn width(level: u8) -> i64 {
    LEVELS.iter().find(|l| l.id == level).unwrap().width
}

/// A node of `kind` over `[start, start + width)` with `n` readings (at least one), values from `seed`.
pub fn node(kind: Kind, start: i64, width: i64, n: u32, seed: u32) -> Node {
    let p = Params::new(kind, Some(width));
    let step = width / i64::from(n + 1);
    let r: Vec<(i64, Value)> = (1..=n)
        .map(|i| {
            let x = seed.wrapping_mul(2_654_435_761).wrapping_add(i * 97) % 1_000;
            let v = match kind {
                Kind::State => Value::Bool(x.is_multiple_of(2)),
                Kind::Counter => Value::F64(f64::from(seed % 50 + i * 3)),
                _ => Value::F64(20.0 + f64::from(x) / 100.0),
            };
            (start + i64::from(i) * step, v)
        })
        .collect();
    fold(&p, start, start + width, &r)
}

/// A 5 s gauge's 1-minute nodes for a whole day: 1,440 entries, more than one nodes block.
pub fn minute_run(series: u64) -> Run {
    Run {
        series,
        level: 1,
        entries: (0..1_440)
            .map(|m| Entry {
                start: T0 + m * MINUTE,
                stale: false,
                as_of: pos(3, 1_000 + m as u64 * 12),
                node: Some(node(Kind::Gauge, T0 + m * MINUTE, MINUTE, 12, m as u32)),
            })
            .collect(),
    }
}

/// A day part: a full 1-minute run, quarter-hours with a gap, a stale entry, an empty marker and an older-generation
/// `as_of`, roll-ups, and series of other forms.
pub fn day_part() -> Part {
    let q = width(2);
    let mut quarters: Vec<Entry> = (0..96)
        .filter(|i| !(40..44).contains(i))
        .map(|i| Entry {
            start: T0 + i * q,
            stale: false,
            as_of: pos(3, 900 + i as u64),
            node: Some(node(Kind::Gauge, T0 + i * q, q, 180, i as u32)),
        })
        .collect();
    quarters[5].stale = true;
    quarters[6].node = None;
    quarters[7].as_of = pos(2, 77);
    quarters[8].stale = true;
    quarters[8].node = None;
    let rollup = |level: u8, kind: Kind, n: i64| Run {
        series: 9,
        level,
        entries: (0..n)
            .map(|i| {
                let w = width(level);
                Entry {
                    start: T0 + i * w,
                    stale: false,
                    as_of: pos(3, 5_000 + i as u64),
                    node: Some(node(kind, T0 + i * w, w, 5, i as u32)),
                }
            })
            .collect(),
    };
    let mut state = rollup(2, Kind::State, 20);
    state.series = 40;
    Part {
        header: part_header(17),
        seq: 12,
        class: Class::Merged,
        period_start: T0,
        period_len: DAY as u64,
        through: pos(3, 20_000),
        levels: LEVELS.to_vec(),
        runs: vec![
            minute_run(7),
            Run {
                series: 7,
                level: 2,
                entries: quarters,
            },
            rollup(3, Kind::Counter, 12),
            rollup(4, Kind::Counter, 1),
            state,
        ],
    }
}

pub fn empty_part() -> Part {
    Part {
        header: part_header(1),
        seq: 1,
        class: Class::Delta,
        period_start: T0,
        period_len: DAY as u64,
        through: pos(3, 0),
        levels: LEVELS.to_vec(),
        runs: Vec::new(),
    }
}

pub fn sample_root() -> Root {
    let part = |file_id, seq, class| PartRef {
        file_id,
        seq,
        class,
        file_crc32c: 0xDEAD_BEEF ^ file_id as u32,
        period_start: T0,
        period_len: DAY as u64,
        least_start: T0,
        greatest_end: T0 + DAY,
        least_series: 7,
        greatest_series: 40,
        byte_len: 140_000 + file_id,
    };
    Root {
        header: FileHeader::new(root::KIND, 3, 12, T0 + DAY, STORE),
        seq: 12,
        checkpoint: pos(3, 20_000),
        reset: ResetStamp {
            sequence: 1,
            nonce: 0x0123_4567_89AB_CDEF,
        },
        next_file_id: 20,
        floor: 2,
        levels: LEVELS.to_vec(),
        parts: vec![
            part(3, 4, Class::Rebuilt),
            part(17, 12, Class::Merged),
            part(18, 11, Class::Delta),
        ],
        covered: vec![
            Covered {
                level: 1,
                from: T0 - 35 * DAY,
                to: T0 + DAY,
            },
            Covered {
                level: 2,
                from: T0 - 400 * DAY,
                to: T0 - 10 * DAY,
            },
            Covered {
                level: 2,
                from: T0 - 9 * DAY,
                to: T0 + DAY,
            },
            Covered {
                level: 4,
                from: T0 - 400 * DAY,
                to: T0,
            },
        ],
        floors: vec![
            Floor {
                series: 7,
                seq: 5,
                rebuilding: false,
            },
            Floor {
                series: 40,
                seq: 12,
                rebuilding: true,
            },
        ],
        retired: vec![
            Retired {
                file_id: 4,
                retired_at: T0 + 3_600 * SECOND,
            },
            Retired {
                file_id: 16,
                retired_at: T0 + 7_200 * SECOND,
            },
        ],
    }
}

/// A generated entry slot: index of the span in the day, what it holds, and its `as_of`.
fn entries(level: u8, kind: Kind) -> impl Strategy<Value = Vec<Entry>> {
    let w = width(level);
    let slots = (DAY / w) as usize;
    prop::collection::btree_map(
        0..slots,
        (0u8..6, 1u32..4, 0u64..1_000_000, 1u32..30),
        1..12,
    )
    .prop_map(move |m| {
        m.into_iter()
            .map(|(i, (what, g, s, n))| {
                let start = T0 + i as i64 * w;
                Entry {
                    start,
                    stale: what == 4 || what == 5,
                    as_of: pos(g, s),
                    node: (what != 3 && what != 5).then(|| node(kind, start, w, n, s as u32)),
                }
            })
            .collect()
    })
}

pub fn generated_part() -> impl Strategy<Value = Part> {
    let run = (0u64..1 << 40, 1u8..5, 0usize..3).prop_flat_map(|(series, level, k)| {
        let kind = [Kind::Gauge, Kind::Counter, Kind::State][k];
        entries(level, kind).prop_map(move |entries| Run {
            series,
            level,
            entries,
        })
    });
    (
        prop::collection::vec(run, 0..5),
        0u64..1 << 50,
        0u8..3,
        any::<u64>(),
    )
        .prop_map(|(mut runs, seq, class, file_id)| {
            runs.sort_by_key(|r| (r.series, r.level));
            runs.dedup_by_key(|r| (r.series, r.level));
            Part {
                header: part_header(file_id),
                seq,
                class: [Class::Delta, Class::Merged, Class::Rebuilt][class as usize],
                period_start: T0,
                period_len: DAY as u64,
                through: pos(3, seq),
                levels: LEVELS.to_vec(),
                runs,
            }
        })
}

pub fn generated_root() -> impl Strategy<Value = Root> {
    (
        prop::collection::btree_map(0u64..1_000, (0u8..3, any::<u32>()), 0..8),
        prop::collection::btree_set(0i64..400, 0..10),
        prop::collection::btree_map(any::<u64>(), (any::<u64>(), any::<bool>()), 0..6),
        prop::collection::btree_set(1_000u64..2_000, 0..4),
    )
        .prop_map(|(parts, cuts, floors, retired)| {
            let mut r = sample_root();
            r.parts = parts
                .iter()
                .enumerate()
                .map(|(i, (id, (class, crc)))| PartRef {
                    file_id: *id,
                    seq: i as u64 * 3,
                    class: [Class::Delta, Class::Merged, Class::Rebuilt][*class as usize],
                    file_crc32c: *crc,
                    ..r.parts[0]
                })
                .collect();
            let cuts: Vec<i64> = cuts.into_iter().collect();
            r.covered = cuts
                .as_chunks::<2>()
                .0
                .iter()
                .filter(|c| c[1] > c[0] + 1)
                .map(|c| Covered {
                    level: 4,
                    from: T0 + c[0] * DAY,
                    to: T0 + (c[1] - 1) * DAY,
                })
                .collect();
            r.floors = floors
                .into_iter()
                .map(|(series, (seq, rebuilding))| Floor {
                    series,
                    seq,
                    rebuilding,
                })
                .collect();
            r.retired = retired
                .into_iter()
                .map(|file_id| Retired {
                    file_id,
                    retired_at: file_id as i64,
                })
                .collect();
            r.next_file_id = 2_000;
            r
        })
}

/// The header and the `(type, body)` of every block of a part, trailer last.
pub fn split(bytes: &[u8]) -> (Vec<u8>, Vec<(u8, Vec<u8>)>) {
    let mut at = 64;
    let end = bytes.len() - 16;
    let mut out = Vec::new();
    while at < end {
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        out.push((bytes[at], bytes[at + 16..at + 16 + len].to_vec()));
        at += 16 + len.div_ceil(8) * 8;
    }
    (bytes[..64].to_vec(), out)
}

/// A part from a header and blocks, trailer last: its `file_crc32c`, the trailer offset and the end marker are
/// recomputed; nothing else is.
pub fn assemble(header: &[u8], blocks: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut out = header.to_vec();
    for (i, (t, body)) in blocks.iter().enumerate() {
        let mut body = body.clone();
        let at = out.len() as u64;
        if i + 1 == blocks.len() {
            if body.len() >= 100 {
                let crc = kvik_log::crc::crc32c(&out);
                body[96..100].copy_from_slice(&crc.to_le_bytes());
            }
            kvik_log::block::encode_block(*t, 0, &body, &mut out).unwrap();
            out.extend_from_slice(&at.to_le_bytes());
            out.extend_from_slice(b"KVIKEND\0");
        } else {
            kvik_log::block::encode_block(*t, 0, &body, &mut out).unwrap();
        }
    }
    out
}

/// One raw entry of a nodes body: gap, flags, generation delta, sequence, node bytes.
pub type RawEntry = (u64, u8, u64, u64, Vec<u8>);

pub fn uv(w: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        w.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    w.push(v as u8);
}

/// A nodes block body written field by field (T.9), valid or not.
pub fn nodes_body(
    series: u64,
    level: u8,
    base_start: i64,
    base: Position,
    entries: &[RawEntry],
) -> Vec<u8> {
    let mut b = Vec::new();
    uv(&mut b, series);
    b.push(level);
    b.extend_from_slice(&base_start.to_le_bytes());
    b.extend_from_slice(&base.generation.to_le_bytes());
    b.extend_from_slice(&base.sequence.to_le_bytes());
    for (gap, flags, dg, ds, node) in entries {
        uv(&mut b, *gap);
        b.push(*flags);
        uv(&mut b, *dg);
        uv(&mut b, *ds);
        uv(&mut b, node.len() as u64);
        b.extend_from_slice(node);
    }
    b
}

/// The blocks of [`empty_part`] with `nodes` inserted before its directory root.
pub fn with_nodes(nodes: Vec<Vec<u8>>) -> Vec<u8> {
    let (h, mut blocks) = split(&empty_part().encode().unwrap());
    for (i, b) in nodes.into_iter().enumerate() {
        blocks.insert(i, (part::block_type::NODES, b));
    }
    assemble(&h, &blocks)
}
