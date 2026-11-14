//! Every candidate round-trips small synthetic spans bit-exactly, in full and
//! for ranges, and the Parquet writer uses the intended layout. Small enough
//! for CI; the measurement itself runs with `cargo run --release -p encodings`.

use encodings::data::{Generator, SECOND, START, SeriesSpan};
use encodings::measure::{DatasetSpec, candidates, run};
use encodings::own::Own;
use encodings::parquet_codec::ParquetCodec;
use encodings::{BLOCK_ROWS, Chunk, Codec, FLAG_NO_VALUE};

const SPAN: i64 = 2 * 3600;

fn spans() -> Vec<Vec<SeriesSpan>> {
    let mut g = Generator::new(11, 10, SPAN);
    (0..2).map(|_| g.next_span()).collect()
}

fn check<C: Codec>(codec: &C, span: &[SeriesSpan], start: i64) {
    let chunks: Vec<&Chunk> = span
        .iter()
        .flat_map(|s| [&s.current, &s.revisions])
        .filter(|c| !c.is_empty())
        .collect();
    let e = codec.encode(&chunks);
    let total: u64 = codec.sizes(&e).iter().map(|s| s.total()).sum();
    assert!(total > 0);
    let block = BLOCK_ROWS as i64 * SECOND;
    let ranges = [
        (start - SECOND, start),
        (start - SECOND, start + SECOND),
        (start + 10 * SECOND, start + 3610 * SECOND),
        (start + block - SECOND, start + block + SECOND),
        (start + block, start + 2 * block),
        (start + SPAN * SECOND - 1, start + 2 * SPAN * SECOND),
        (start, start + SPAN * SECOND),
    ];
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(&codec.decode(&e, i), *c, "{} chunk {i}", codec.name());
        for &(from, to) in &ranges {
            assert_eq!(
                codec.decode_range(&e, i, from, to),
                c.slice_t(from, to),
                "{} chunk {i} range {from}..{to}",
                codec.name()
            );
        }
    }
}

#[test]
fn every_candidate_round_trips() {
    for (k, span) in spans().iter().enumerate() {
        let start = START + k as i64 * SPAN * SECOND;
        check(&Own::PLAIN_ZSTD, span, start);
        check(&Own::DOD_XOR_RLE, span, start);
        check(&Own::DELTA_XOR_RLE, span, start);
        check(&Own::DELTA_XOR_RLE_ZSTD, span, start);
        check(&Own::DELTA_DECIMAL_RLE, span, start);
        check(&Own::DELTA_DECIMAL_RLE_ZSTD, span, start);
        check(&ParquetCodec::BYTE_STREAM_SPLIT, span, start);
        check(&ParquetCodec::ALP, span, start);
    }
}

#[test]
fn data_has_every_feature() {
    let all = spans();
    let facts = all.iter().flatten();
    assert!(facts.clone().any(|s| !s.revisions.is_empty()), "revisions");
    assert!(
        facts
            .clone()
            .any(|s| s.current.flags.iter().any(|f| f & FLAG_NO_VALUE != 0)),
        "rows without a value"
    );
    assert!(
        facts
            .clone()
            .any(|s| s.current.t.windows(2).any(|w| w[1] - w[0] > 60 * SECOND)),
        "gaps"
    );
}

#[test]
fn parquet_layout() {
    let span = &spans()[0];
    let chunks: Vec<&Chunk> = span.iter().map(|s| &s.current).collect();
    for codec in [ParquetCodec::BYTE_STREAM_SPLIT, ParquetCodec::ALP] {
        let f = codec.encode(&chunks);
        codec.check_layout(&f);
    }
}

#[test]
fn harness_runs() {
    let spec = DatasetSpec {
        name: "test",
        seed: 5,
        series: 5,
        spans: 1,
        span_seconds: 1800,
    };
    let r = run(&spec, 1, &candidates());
    assert_eq!(r.candidates.len(), 8);
    for c in &r.candidates {
        assert_eq!(c.rows, r.current_rows + r.revision_rows);
        assert!(c.range_rows > 0);
    }
}
