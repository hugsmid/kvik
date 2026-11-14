//! Every candidate round-trips bit-exactly, in full and for ranges: on
//! generated spans, on hand-built edge cases and on arbitrary chunks. The
//! Parquet writer uses the intended layout. Small enough for CI; the
//! measurement itself runs with `cargo run --release -p encodings`.

use encodings::data::{Generator, NON_DECIMAL_SHAPES, SECOND, START, SeriesSpan};
use encodings::measure::{DatasetSpec, candidates, run};
use encodings::own::Own;
use encodings::parquet_codec::ParquetCodec;
use encodings::quality::BAD;
use encodings::{BLOCK_ROWS, Chunk, Codec, FLAG_NO_VALUE, ValueKind};
use proptest::prelude::*;

const SPAN: i64 = 2 * 3600;

/// Encodes `chunks` as one file and checks every chunk in full and for each
/// range.
fn check<C: Codec>(codec: &C, chunks: &[&Chunk], ranges: &[(i64, i64)]) {
    let e = codec.encode(chunks);
    codec.check(&e);
    let sizes = codec.sizes(&e);
    assert_eq!(sizes.len(), chunks.len());
    assert!(sizes.iter().all(|s| s.total() > 0));
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(&codec.decode(&e, i), *c, "{} chunk {i}", codec.name());
        for &(from, to) in ranges {
            assert_eq!(
                codec.decode_range(&e, i, from, to),
                c.slice_t(from, to),
                "{} chunk {i} range {from}..{to}",
                codec.name()
            );
        }
    }
}

fn check_all(chunks: &[&Chunk], ranges: &[(i64, i64)]) {
    check(&Own::PLAIN_ZSTD, chunks, ranges);
    check(&Own::DOD_XOR_RLE, chunks, ranges);
    check(&Own::DELTA_XOR_RLE, chunks, ranges);
    check(&Own::DELTA_XOR_RLE_ZSTD, chunks, ranges);
    check(&Own::DELTA_DECIMAL_RLE, chunks, ranges);
    check(&Own::DELTA_DECIMAL_RLE_ZSTD, chunks, ranges);
    check(&ParquetCodec::BYTE_STREAM_SPLIT, chunks, ranges);
    check(&ParquetCodec::ALP, chunks, ranges);
}

fn span_ranges(start: i64, len: i64) -> Vec<(i64, i64)> {
    let block = BLOCK_ROWS as i64 * SECOND;
    vec![
        (start - SECOND, start),
        (start - SECOND, start + SECOND),
        (start + 10 * SECOND, start + 3610 * SECOND),
        (start + block - SECOND, start + block + SECOND),
        (start + block, start + 2 * block),
        (start + len * SECOND - 1, start + 2 * len * SECOND),
        (start, start + len * SECOND),
    ]
}

fn chunks_of(span: &[SeriesSpan]) -> Vec<&Chunk> {
    span.iter()
        .flat_map(|s| [&s.current, &s.revisions])
        .filter(|c| !c.is_empty())
        .collect()
}

#[test]
fn generated_spans_round_trip() {
    let mut decimal = Generator::new(11, 10, SPAN);
    let mut integers = Generator::new(11, 10, SPAN).integers();
    let mut other = Generator::with_shapes(11, 4, SPAN, &NON_DECIMAL_SHAPES);
    for k in 0..2 {
        let start = START + k * SPAN * SECOND;
        let ranges = span_ranges(start, SPAN);
        for span in [decimal.next_span(), integers.next_span(), other.next_span()] {
            check_all(&chunks_of(&span), &ranges);
        }
    }
}

#[test]
fn generated_data_has_every_feature() {
    let mut g = Generator::new(11, 10, SPAN);
    let spans: Vec<_> = (0..2).flat_map(|_| g.next_span()).collect();
    assert!(spans.iter().any(|s| !s.revisions.is_empty()), "revisions");
    assert!(
        spans
            .iter()
            .any(|s| s.current.flags.iter().any(|f| f & FLAG_NO_VALUE != 0)),
        "rows without a value"
    );
    assert!(
        spans
            .iter()
            .any(|s| s.current.t.windows(2).any(|w| w[1] - w[0] > 60 * SECOND)),
        "gaps"
    );
    let ints = Generator::new(11, 10, SPAN).integers().next_span();
    assert!(ints.iter().any(|s| s.current.kind == ValueKind::I64));
}

/// Pushes a row; a row without a value gets value 0, quality bad and the flag.
fn push(c: &mut Chunk, t: i64, value: Option<u64>, quality: u8, flags: u16, position: u64) {
    match value {
        Some(v) => c.push(t, v, quality, flags & !FLAG_NO_VALUE, position),
        None => c.push(t, 0, BAD, flags | FLAG_NO_VALUE, position),
    }
}

#[test]
fn edge_cases_round_trip() {
    let specials = [
        -0.0f64,
        0.0,
        f64::MIN_POSITIVE / 2.0,
        f64::from_bits(1),
        f64::MIN_POSITIVE,
        f64::MAX,
        -f64::MAX,
        f64::from_bits(f64::MAX.to_bits() - 1),
        1.0 / 3.0,
        std::f64::consts::PI,
        -273.15,
        21.37,
        1e15,
        9.007_199_254_740_993e15,
    ];
    let n = 3 * BLOCK_ROWS + 100;
    let mut f = Chunk::new(ValueKind::F64);
    let mut t = -1_000 * SECOND;
    let mut position = u64::MAX - 5 * n as u64;
    for i in 0..n {
        t += if i % 1000 == 999 {
            86_400 * SECOND
        } else {
            (i % 3) as i64 * 7 + 1
        };
        position += 1 + (i % 4) as u64;
        let block = i / BLOCK_ROWS;
        let value = match block {
            // Mostly decimal, some exceptional values: the exception path.
            0 if i % 97 == 0 => Some(specials[i % specials.len()].to_bits()),
            0 => Some((f64::from(i as u32 % 500) / 100.0).to_bits()),
            // No row with a value: an empty value column.
            1 => None,
            // Not decimal: the XOR fallback.
            2 => Some((i as f64 / 7.0).to_bits()),
            _ => Some(specials[i % specials.len()].to_bits()),
        };
        let quality = (i % 4) as u8;
        let flags = [0u16, 1, 1 << 2, 1 << 4, 0b1_0101][i % 5];
        push(&mut f, t, value, quality, flags, position);
    }

    let mut s = Chunk::new(ValueKind::State);
    for (i, v) in [0u64, 1, 7, 300, 65_535, 65_535, 2, 0]
        .iter()
        .cycle()
        .take(5000)
        .enumerate()
    {
        let value = (i % 333 != 0).then_some(*v);
        push(&mut s, i as i64 * SECOND, value, 1, 1 << 2, i as u64 * 3);
    }

    let mut int = Chunk::new(ValueKind::I64);
    for (i, v) in [i64::MIN, i64::MAX, 0, -1, 1, i64::MIN + 1, 12_345]
        .iter()
        .cycle()
        .take(4500)
        .enumerate()
    {
        push(
            &mut int,
            i as i64,
            Some(*v as u64),
            0,
            0,
            (1u64 << 63) + i as u64,
        );
    }

    // A revision chunk: repeated `t`, positions out of order.
    let mut rev = Chunk::new(ValueKind::F64);
    for i in 0..50i64 {
        push(
            &mut rev,
            i / 3 * SECOND,
            Some((i as f64 * 0.5).to_bits()),
            0,
            1 << 1,
            (100 - i) as u64,
        );
    }

    let mut one = Chunk::new(ValueKind::F64);
    push(
        &mut one,
        i64::MAX - 1,
        Some((-0.0f64).to_bits()),
        2,
        1 << 3,
        u64::MAX,
    );

    let ranges = [
        (i64::MIN, i64::MAX),
        (-1_000 * SECOND, -999 * SECOND),
        (0, 10 * SECOND),
        (i64::MIN, 0),
        (5, 4_000),
        (3 * SECOND, 4 * SECOND),
        (i64::MAX - 1, i64::MAX),
    ];
    check_all(&[&f, &s, &int, &rev, &one], &ranges);
}

#[test]
fn parquet_layout() {
    let span = Generator::new(11, 10, SPAN).next_span();
    let ints = Generator::new(11, 10, SPAN).integers().next_span();
    for codec in [ParquetCodec::BYTE_STREAM_SPLIT, ParquetCodec::ALP] {
        codec.check(&codec.encode(&chunks_of(&span)));
        codec.check(&codec.encode(&chunks_of(&ints)));
    }
}

#[test]
fn harness_runs() {
    let spec = DatasetSpec {
        span_seconds: 1800,
        ..DatasetSpec::decimal("test", 5, 5, 1)
    };
    let r = run(&spec, 1, &candidates());
    assert_eq!(r.candidates.len(), 8);
    for c in &r.candidates {
        assert_eq!(c.rows, r.current_rows + r.revision_rows);
        assert!(c.range_rows > 0);
    }
}

fn value_strategy(kind: ValueKind) -> BoxedStrategy<u64> {
    match kind {
        ValueKind::F64 => prop_oneof![
            // Any finite double: a NaN or infinity is never stored.
            any::<u64>().prop_map(|b| if f64::from_bits(b).is_finite() {
                b
            } else {
                b & !(1 << 62)
            }),
            (-100_000i64..100_000, 0i32..4).prop_map(|(n, e)| (n as f64 / 10f64.powi(e)).to_bits()),
            Just((-0.0f64).to_bits()),
            Just(1),
            Just(f64::MAX.to_bits()),
        ]
        .boxed(),
        ValueKind::State => any::<u16>().prop_map(u64::from).boxed(),
        ValueKind::I64 => {
            prop_oneof![any::<u64>(), (-1000i64..1000).prop_map(|v| v as u64)].boxed()
        }
    }
}

prop_compose! {
    fn row(kind: ValueKind)(
        dt in prop_oneof![Just(SECOND), 0i64..2 * SECOND, any::<u32>().prop_map(i64::from)],
        value in prop::option::weighted(0.95, value_strategy(kind)),
        quality in 0u8..4,
        flags in 0u16..32,
        dp in prop_oneof![Just(1u64), 0u64..300],
    ) -> (i64, Option<u64>, u8, u16, u64) {
        (dt, value, quality, flags, dp)
    }
}

fn chunk_strategy() -> impl Strategy<Value = Chunk> {
    prop_oneof![
        Just(ValueKind::F64),
        Just(ValueKind::State),
        Just(ValueKind::I64)
    ]
    .prop_flat_map(|kind| {
        (
            i64::MIN / 2..i64::MAX / 2,
            any::<u64>(),
            prop::collection::vec(row(kind), 1..2 * BLOCK_ROWS + 10),
        )
            .prop_map(move |(t0, p0, rows)| {
                let mut c = Chunk::new(kind);
                let (mut t, mut p) = (t0, p0);
                for (dt, value, quality, flags, dp) in rows {
                    t += dt;
                    p = p.wrapping_add(dp);
                    push(&mut c, t, value, quality, flags, p);
                }
                c
            })
    })
}

proptest! {
    #[test]
    fn arbitrary_chunks_round_trip(chunks in prop::collection::vec(chunk_strategy(), 1..3), a in 0usize..10_000, b in 0usize..10_000) {
        let first = &chunks[0];
        let at = |i: usize| first.t[i % first.len()];
        let (lo, hi) = (at(a).min(at(b)), at(a).max(at(b)));
        let ranges = [(lo, hi), (lo, hi.saturating_add(1)), (i64::MIN, lo), (hi, i64::MAX)];
        let refs: Vec<&Chunk> = chunks.iter().collect();
        check_all(&refs, &ranges);
    }
}
