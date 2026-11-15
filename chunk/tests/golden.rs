//! Golden block bodies (`tests/golden/README.md`): each file equals what the generator produces now and decodes to
//! the rows it was built from. Regenerate only after a deliberate format change:
//! `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-chunk --test golden`.

mod support;

use std::fs;
use std::path::PathBuf;

use kvik_chunk::{Block, DecodeError, Element, column, encoding, flag, value_type};
use support::*;

const T0: i64 = 1_767_225_600_000_000_000;
const SECOND: i64 = 1_000_000_000;

fn row(i: usize, vt: u8, value: u64) -> Row {
    Row {
        t: T0 + i as i64 * SECOND,
        generation: 1,
        sequence: 100 + 5 * i as u64,
        quality: 0,
        flags: 0,
        value_type: vt,
        value,
        text: None,
        changed_at: 0,
        scale: 0,
    }
}

fn decimal() -> Block {
    let mut b = temperature(300);
    // Exceptions: −0.0, a value with 17 significant digits, a NaN with a payload, a signalling NaN.
    b.value[7] = (-0.0f64).to_bits();
    b.value[100] = (0.1f64 + 0.2).to_bits();
    b.value[200] = 0x7FF8_0000_DEAD_BEEF;
    b.value[201] = 0xFFF0_0000_0000_0001;
    b
}

fn xor() -> Block {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    block(
        (0..40)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let m = 1.0 + (x % (1 << 20)) as f64 / f64::from(1u32 << 20);
                row(
                    i,
                    value_type::F64,
                    (if x >> 63 == 1 { -m } else { m }).to_bits(),
                )
            })
            .collect(),
    )
}

fn xor_zstd() -> Block {
    block(
        (0..2_000)
            .map(|i| {
                row(
                    i,
                    value_type::F64,
                    (f64::from((i / 2 % 5) as u32) / 9.0 + 1.0).to_bits(),
                )
            })
            .collect(),
    )
}

fn scaled_i64() -> Block {
    // A counter at scale 3, raised to scale 4 from row 50; two rounded rows.
    block(
        (0..120)
            .map(|i| {
                let mut r = row(i, value_type::DECIMAL, 0);
                r.scale = if i < 50 { 3 } else { 4 };
                let units = 1_234_000 + 7 * i as i64;
                r.value = if i < 50 { units } else { units * 10 } as u64;
                if i == 10 || i == 90 {
                    r.flags = 1 << 7;
                }
                r
            })
            .collect(),
    )
}

fn state() -> Block {
    block(
        (0..500)
            .map(|i| row(i, value_type::STATE, [0, 3, 65_535][(i / 120) % 3]))
            .collect(),
    )
}

fn boolean() -> Block {
    block(
        (0..600)
            .map(|i| row(i, value_type::BOOL, ((i / 97) % 2) as u64))
            .collect(),
    )
}

fn text() -> Block {
    let texts = ["idle", "", "heating", "dégivrage", "", "停止"];
    block(
        (0..8)
            .map(|i| {
                let mut r = row(i, value_type::TEXT, 0);
                if i == 6 {
                    r.flags = flag::NO_VALUE;
                    r.quality = 3;
                } else {
                    r.text = Some(texts[i % texts.len()].to_owned());
                }
                r
            })
            .collect(),
    )
}

fn versioned() -> Block {
    // A forecast: valid times on a 15-minute grid, issue times as `changed_at`, two issues of each valid time.
    block(
        (0..96)
            .map(|i| {
                let mut r = row(
                    i,
                    value_type::F64,
                    (f64::from((i % 48) as u32) / 4.0).to_bits(),
                );
                r.t = T0 + (i % 48) as i64 * 900 * SECOND;
                r.flags = flag::HAS_CHANGED_AT;
                r.changed_at = (T0 - 3_600 * SECOND + (i / 48) as i64 * 1_800 * SECOND) as u64;
                r
            })
            .collect(),
    )
}

fn gappy() -> Block {
    // Host-stamped at nanosecond resolution with jitter, a run of suspect quality, rows without a value.
    block(
        (0..400)
            .map(|i| {
                let mut r = row(
                    i,
                    value_type::F64,
                    (f64::from((i % 90) as u32) / 10.0).to_bits(),
                );
                r.t += ((i as i64 * 7_919_011) % 8_000_000) + 13;
                r.flags = 1 << 3;
                if (150..170).contains(&i) {
                    r.quality = 2;
                }
                if i % 97 == 5 {
                    r.flags |= flag::NO_VALUE;
                    r.quality = 3;
                    r.value = 0;
                }
                if i == 399 {
                    r.flags |= 1 << 1;
                }
                r
            })
            .collect(),
    )
}

fn mixed() -> Block {
    // Facts of one id with different value types (orphaned facts kept through compaction, `spec.md` §2.1).
    let vts = [
        value_type::F64,
        value_type::DECIMAL,
        value_type::STATE,
        value_type::BOOL,
        value_type::TEXT,
    ];
    block(
        (0..10)
            .map(|i| {
                let vt = vts[i % 5];
                let mut r = row(
                    i,
                    vt,
                    match vt {
                        value_type::F64 => 2.5f64.to_bits(),
                        value_type::DECIMAL => (-42i64) as u64,
                        value_type::STATE => 7,
                        value_type::BOOL => 1,
                        _ => 0,
                    },
                );
                r.scale = if vt == value_type::DECIMAL { 1 } else { 0 };
                r.text = (vt == value_type::TEXT).then(|| format!("t{i}"));
                r
            })
            .collect(),
    )
}

fn generations() -> Block {
    // Positions across a restore: generation 1, then generation 2 from sequence 1.
    block(
        (0..60)
            .map(|i| {
                let mut r = row(i, value_type::F64, (f64::from(i as u32) / 2.0).to_bits());
                if i >= 40 {
                    r.generation = 2;
                    r.sequence = 1 + (i as u64 - 40) * 3;
                }
                r
            })
            .collect(),
    )
}

/// The name and rows of every golden file whose bytes the encoder writes.
fn generated() -> Vec<(&'static str, Block)> {
    vec![
        ("decimal.blk", decimal()),
        ("xor.blk", xor()),
        ("xor-zstd.blk", xor_zstd()),
        ("scaled-i64.blk", scaled_i64()),
        ("state.blk", state()),
        ("bool.blk", boolean()),
        ("text.blk", text()),
        ("versioned.blk", versioned()),
        ("gappy.blk", gappy()),
        ("mixed-types.blk", mixed()),
        ("generations.blk", generations()),
        ("full.blk", temperature(4096)),
    ]
}

/// A golden file for each value encoding (C.12).
const VALUE_ENCODINGS: [(&str, u8); 6] = [
    ("mixed-types.blk", encoding::PLAIN),
    ("state.blk", encoding::RUN_LENGTH),
    ("scaled-i64.blk", encoding::DELTA),
    ("decimal.blk", encoding::DECIMAL),
    ("xor.blk", encoding::XOR),
    ("xor-zstd.blk", encoding::XOR_ZSTD),
];

/// A body with one more column appended after the last (hand-built, not canonical).
fn with_column(body: &[u8], id: u8, enc: u8, payload: &[u8]) -> Vec<u8> {
    let count = usize::from(body[2]);
    let mut at = 3;
    for _ in 0..count {
        at += 2;
        while body[at] & 0x80 != 0 {
            at += 1;
        }
        at += 1;
    }
    let mut out = body[..at].to_vec();
    out[2] += 1;
    out.extend_from_slice(&[id, enc, payload.len() as u8]);
    out.extend_from_slice(&body[at..]);
    out.extend_from_slice(payload);
    out
}

fn hand_built() -> Vec<(&'static str, Vec<u8>)> {
    let base = temperature(5).encode().unwrap();
    vec![
        (
            "unknown-ignorable.blk",
            with_column(&base, 0x90, 9, &[1, 2, 3]),
        ),
        (
            "unknown-critical.blk",
            with_column(&base, 0x12, 0, &[1, 2, 3]),
        ),
    ]
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn value_encoding(body: &[u8]) -> u8 {
    let mut at = 3;
    loop {
        let (id, enc) = (body[at], body[at + 1]);
        at += 2;
        while body[at] & 0x80 != 0 {
            at += 1;
        }
        at += 1;
        if id == column::VALUE {
            return enc;
        }
    }
}

#[test]
fn golden_files() {
    let bless = std::env::var_os("KVIK_BLESS_GOLDEN").is_some();
    let mut all: Vec<(&str, Vec<u8>)> = generated()
        .into_iter()
        .map(|(n, b)| (n, b.encode().unwrap()))
        .collect();
    all.extend(hand_built());
    if bless {
        for (name, bytes) in &all {
            fs::write(dir().join(name), bytes).unwrap();
        }
    }
    for (name, block) in generated() {
        let file = fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            Block::decode(&file).as_ref(),
            Ok(&block),
            "{name} decodes to its rows"
        );
        assert_eq!(
            Block::decode_canonical(&file).as_ref(),
            Ok(&block),
            "{name} is canonical"
        );
        let now = block.encode().unwrap();
        if value_encoding(&file) == encoding::XOR_ZSTD {
            // Compressed bytes are a writer's choice (C.8): compared by what they decode to.
            assert_eq!(value_encoding(&now), encoding::XOR_ZSTD, "{name}");
        } else {
            assert_eq!(now, file, "{name} is what the encoder writes now");
        }
    }
    for (name, enc) in VALUE_ENCODINGS {
        let file = fs::read(dir().join(name)).unwrap();
        assert_eq!(value_encoding(&file), enc, "{name}");
    }
    for (name, bytes) in hand_built() {
        assert_eq!(fs::read(dir().join(name)).unwrap(), bytes, "{name}");
    }
    let ignorable = fs::read(dir().join("unknown-ignorable.blk")).unwrap();
    assert_eq!(Block::decode(&ignorable), Ok(temperature(5)));
    assert_eq!(
        Block::decode_canonical(&ignorable),
        Err(DecodeError::NonCanonical)
    );
    let critical = fs::read(dir().join("unknown-critical.blk")).unwrap();
    assert_eq!(
        Block::decode(&critical),
        Err(DecodeError::Unsupported(Element::Column(0x12)))
    );
}

#[test]
fn golden_rows_show_what_the_readme_says() {
    let texts = text();
    assert_eq!(
        texts.texts(),
        [
            Some("idle"),
            Some(""),
            Some("heating"),
            Some("dégivrage"),
            Some(""),
            Some("停止"),
            None,
            Some("")
        ]
    );
    let d = decimal();
    assert_eq!(d.value[200], 0x7FF8_0000_DEAD_BEEF);
    assert!(scaled_i64().scale.contains(&4));
}
