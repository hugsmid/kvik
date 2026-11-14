//! Timed harness: sizes, encode time, full-scan and range-scan decode speed.
//!
//! Everything runs in memory on one thread. For each span the generator
//! produces, every candidate encodes all non-empty chunks of the span as one
//! file `reps` times, is checked to round-trip bit-exactly (full and range),
//! then decodes every chunk `reps` times and range-scans every current chunk
//! `reps` times. Times are summed over spans per repetition and the median
//! repetition is reported.

use std::hint::black_box;
use std::time::{Duration, Instant};

use crate::data::{Generator, SECOND, SHAPES, SeriesSpan, Shape};
use crate::own::Own;
use crate::parquet_codec::ParquetCodec;
use crate::{Chunk, Codec, ColumnSizes};

/// A data set to measure.
#[derive(Clone, Debug)]
pub struct DatasetSpec {
    /// Name for tables.
    pub name: &'static str,
    /// Generator seed.
    pub seed: u64,
    /// Number of series.
    pub series: usize,
    /// Number of spans (chunks per series).
    pub spans: usize,
    /// Span length in seconds.
    pub span_seconds: i64,
}

/// Accumulated results of one candidate on one data set.
#[derive(Clone, Debug, Default)]
pub struct CandidateResult {
    /// Candidate name.
    pub name: &'static str,
    /// Rows encoded (current and revisions).
    pub rows: u64,
    /// Sizes by column, all chunks.
    pub sizes: ColumnSizes,
    /// Sizes and row counts by shape.
    pub by_shape: Vec<(ColumnSizes, u64)>,
    /// Bytes for `received_at`, encoded like `t`, all chunks.
    pub received_at: u64,
    /// Encode time per repetition.
    pub encode: Vec<Duration>,
    /// Full decode time per repetition.
    pub decode: Vec<Duration>,
    /// Range-scan time per repetition.
    pub range: Vec<Duration>,
    /// Rows returned by one pass of range scans.
    pub range_rows: u64,
    /// Range scans in one pass.
    pub range_scans: u64,
    /// File-open time per repetition.
    pub open: Vec<Duration>,
    /// Files opened in one pass.
    pub files: u64,
}

/// Median of durations.
pub fn median(xs: &[Duration]) -> Duration {
    let mut v = xs.to_vec();
    v.sort();
    v[v.len() / 2]
}

impl CandidateResult {
    fn new(name: &'static str, reps: usize) -> Self {
        CandidateResult {
            name,
            by_shape: vec![(ColumnSizes::default(), 0); SHAPES.len()],
            encode: vec![Duration::ZERO; reps],
            decode: vec![Duration::ZERO; reps],
            range: vec![Duration::ZERO; reps],
            open: vec![Duration::ZERO; reps],
            ..CandidateResult::default()
        }
    }
}

/// A candidate behind a trait object, so several codecs share one data pass.
pub trait Candidate {
    /// Candidate name.
    fn name(&self) -> &'static str;
    /// Measures one span and adds to `acc`.
    fn run_span(&self, spans: &[SeriesSpan], range: (i64, i64), acc: &mut CandidateResult);
}

fn shape_index(s: Shape) -> usize {
    SHAPES.iter().position(|&x| x == s).expect("known shape")
}

impl<C: Codec> Candidate for C {
    fn name(&self) -> &'static str {
        Codec::name(self)
    }

    fn run_span(&self, spans: &[SeriesSpan], (from, to): (i64, i64), acc: &mut CandidateResult) {
        // Non-empty chunks: current chunks first (indices 0..), then revisions.
        let mut chunks: Vec<(&Chunk, Shape, bool)> = Vec::new();
        for s in spans {
            if !s.current.is_empty() {
                chunks.push((&s.current, s.shape, true));
            }
        }
        for s in spans {
            if !s.revisions.is_empty() {
                chunks.push((&s.revisions, s.shape, false));
            }
        }
        let refs: Vec<&Chunk> = chunks.iter().map(|c| c.0).collect();

        let mut encoded = None;
        for slot in acc.encode.iter_mut() {
            let t0 = Instant::now();
            let e = black_box(self.encode(&refs));
            *slot += t0.elapsed();
            encoded = Some(e);
        }
        let e = encoded.expect("at least one repetition");

        for (i, c) in refs.iter().enumerate() {
            assert!(
                &self.decode(&e, i) == *c,
                "{}: chunk {i} does not round-trip",
                self.name()
            );
        }
        for (i, s) in self.sizes(&e).iter().enumerate() {
            let (c, shape, _) = chunks[i];
            acc.sizes.add(s);
            let entry = &mut acc.by_shape[shape_index(shape)];
            entry.0.add(s);
            entry.1 += c.len() as u64;
            acc.rows += c.len() as u64;
        }
        let recv: Vec<&[i64]> = spans.iter().map(|s| s.received_at.as_slice()).collect();
        acc.received_at += self.i64_column_size(&recv);

        acc.files += 1;
        for slot in acc.open.iter_mut() {
            let t0 = Instant::now();
            self.open(&e);
            *slot += t0.elapsed();
        }

        for slot in acc.decode.iter_mut() {
            let t0 = Instant::now();
            for i in 0..refs.len() {
                black_box(self.decode(&e, i));
            }
            *slot += t0.elapsed();
        }

        let current: Vec<usize> = (0..chunks.len()).filter(|&i| chunks[i].2).collect();
        for &i in &current {
            let expected = refs[i].slice_t(from, to);
            let got = self.decode_range(&e, i, from, to);
            assert!(
                got == expected,
                "{}: range of chunk {i} differs",
                self.name()
            );
            acc.range_rows += expected.len() as u64;
        }
        acc.range_scans += current.len() as u64;
        for slot in acc.range.iter_mut() {
            let t0 = Instant::now();
            for &i in &current {
                black_box(self.decode_range(&e, i, from, to));
            }
            *slot += t0.elapsed();
        }
    }
}

/// The candidates, in table order.
pub fn candidates() -> Vec<Box<dyn Candidate>> {
    vec![
        Box::new(Own::PLAIN_ZSTD),
        Box::new(Own::DOD_XOR_RLE),
        Box::new(Own::DELTA_XOR_RLE),
        Box::new(Own::DELTA_XOR_RLE_ZSTD),
        Box::new(Own::DELTA_DECIMAL_RLE),
        Box::new(Own::DELTA_DECIMAL_RLE_ZSTD),
        Box::new(ParquetCodec::BYTE_STREAM_SPLIT),
        Box::new(ParquetCodec::ALP),
    ]
}

/// Results for one data set.
pub struct DatasetResult {
    /// The data set.
    pub spec: DatasetSpec,
    /// Current rows.
    pub current_rows: u64,
    /// Revision rows.
    pub revision_rows: u64,
    /// Time spent generating data.
    pub generate: Duration,
    /// Per candidate.
    pub candidates: Vec<CandidateResult>,
}

/// The range scanned in a span: one hour from the middle of the span, or a
/// quarter of a span shorter than four hours.
pub fn range_in_span(start: i64, span_seconds: i64) -> (i64, i64) {
    let from = start + span_seconds / 2 * SECOND;
    (from, from + span_seconds.min(4 * 3600) / 4 * SECOND)
}

/// Measures `candidates` on `spec`.
pub fn run(spec: &DatasetSpec, reps: usize, candidates: &[Box<dyn Candidate>]) -> DatasetResult {
    let mut g = Generator::new(spec.seed, spec.series, spec.span_seconds);
    let mut accs: Vec<CandidateResult> = candidates
        .iter()
        .map(|c| CandidateResult::new(c.name(), reps))
        .collect();
    let (mut current_rows, mut revision_rows) = (0, 0);
    let mut generate = Duration::ZERO;
    for index in 0..spec.spans {
        let t0 = Instant::now();
        let spans = g.next_span();
        generate += t0.elapsed();
        current_rows += spans.iter().map(|s| s.current.len() as u64).sum::<u64>();
        revision_rows += spans.iter().map(|s| s.revisions.len() as u64).sum::<u64>();
        let range = range_in_span(g.span_start(index as i64), spec.span_seconds);
        for (c, acc) in candidates.iter().zip(accs.iter_mut()) {
            c.run_span(&spans, range, acc);
        }
    }
    DatasetResult {
        spec: spec.clone(),
        current_rows,
        revision_rows,
        generate,
        candidates: accs,
    }
}

fn per_row(bytes: u64, rows: u64) -> String {
    format!("{:.2}", bytes as f64 / rows as f64)
}

fn mrows(rows: u64, d: Duration) -> String {
    format!("{:.0}", rows as f64 / d.as_secs_f64() / 1e6)
}

/// Markdown tables for one data set.
pub fn report(r: &DatasetResult) -> String {
    let mut s = String::new();
    let rows = r.current_rows + r.revision_rows;
    s += &format!(
        "### {}\n\n{} current rows, {} revision rows; generated in {:.1} s. Plain, uncompressed: 27 bytes per row.\n\n",
        r.spec.name,
        r.current_rows,
        r.revision_rows,
        r.generate.as_secs_f64()
    );
    s += "Size, bytes per row (all chunks):\n\n";
    s += "| Candidate | Total | `t` | value | quality + flags | position | overhead | `received_at` (extra) |\n";
    s += "|---|---:|---:|---:|---:|---:|---:|---:|\n";
    for c in &r.candidates {
        let z = &c.sizes;
        s += &format!(
            "| {} | **{}** | {} | {} | {} | {} | {} | {} |\n",
            c.name,
            per_row(z.total(), rows),
            per_row(z.t, rows),
            per_row(z.value, rows),
            per_row(z.quality_flags, rows),
            per_row(z.position, rows),
            per_row(z.overhead, rows),
            per_row(c.received_at, rows),
        );
    }
    s += "\nSize by shape, bytes per row:\n\n| Candidate |";
    for sh in SHAPES {
        s += &format!(" {} |", sh.name());
    }
    s += "\n|---|";
    for _ in SHAPES {
        s += "---:|";
    }
    s += "\n";
    for c in &r.candidates {
        s += &format!("| {} |", c.name);
        for (z, n) in &c.by_shape {
            s += &format!(" {} |", per_row(z.total(), *n));
        }
        s += "\n";
    }
    s += "\nSpeed (median of repetitions; one thread; in memory):\n\n";
    s += "| Candidate | Encode, M rows/s | Full decode, M rows/s | 1-hour range scan, µs per chunk | Range scan, M rows/s | File open, µs |\n";
    s += "|---|---:|---:|---:|---:|---:|\n";
    for c in &r.candidates {
        let range = median(&c.range);
        s += &format!(
            "| {} | {} | {} | {:.0} | {} | {:.0} |\n",
            c.name,
            mrows(c.rows, median(&c.encode)),
            mrows(c.rows, median(&c.decode)),
            range.as_secs_f64() * 1e6 / c.range_scans as f64,
            mrows(c.range_rows, range),
            median(&c.open).as_secs_f64() * 1e6 / c.files as f64,
        );
    }
    s += "\n";
    s
}
