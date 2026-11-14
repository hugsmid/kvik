//! Timed harness: sizes, encode time, full-scan and range-scan decode speed.
//!
//! Everything runs in memory on one thread. For each span the generator
//! produces, every candidate encodes all non-empty chunks of the span as one
//! file; this is repeated `reps` times, the candidates taking turns within
//! each repetition (in an order rotated by one each time), so that no
//! candidate always runs first or last. Every candidate is then checked
//! (layout, sizes, bit-exact round trip in full and for the range scanned),
//! untimed. Then, again taking turns, each candidate opens the file, decodes
//! every chunk and range-scans every current chunk, `reps` times. Times are
//! summed over spans per repetition; the minimum, median and maximum
//! repetition are reported.

use std::any::Any;
use std::hint::black_box;
use std::time::{Duration, Instant};

use crate::data::{DECIMAL_SHAPES, Generator, SECOND, SHAPES, SeriesSpan, Shape};
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
    /// Shapes, cycled through by series.
    pub shapes: &'static [Shape],
    /// Emit the shapes that have a fixed scale as `i64`.
    pub integers: bool,
}

impl DatasetSpec {
    /// A data set of the decimal shapes as `f64`.
    pub fn decimal(name: &'static str, seed: u64, series: usize, spans: usize) -> Self {
        DatasetSpec {
            name,
            seed,
            series,
            spans,
            span_seconds: crate::data::DAY_SECONDS,
            shapes: &DECIMAL_SHAPES,
            integers: false,
        }
    }
}

/// Accumulated results of one candidate on one data set.
#[derive(Clone, Debug, Default)]
pub struct CandidateResult {
    /// Candidate name.
    pub name: &'static str,
    /// Rows encoded (current and revisions).
    pub rows: u64,
    /// Chunks encoded.
    pub chunks: u64,
    /// Sizes by column, all chunks.
    pub sizes: ColumnSizes,
    /// Sizes and row counts by shape, indexed like [`SHAPES`].
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
    /// Files in one pass.
    pub files: u64,
}

/// Minimum, median and maximum of durations.
pub fn spread(xs: &[Duration]) -> (Duration, Duration, Duration) {
    let mut v = xs.to_vec();
    v.sort();
    (v[0], v[v.len() / 2], v[v.len() - 1])
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

/// One span's chunks: non-empty current chunks first, then revisions.
pub struct SpanChunks<'a> {
    /// Chunk, its series' shape, and whether it holds current facts.
    pub chunks: Vec<(&'a Chunk, Shape, bool)>,
    /// `received_at` of every series.
    pub received_at: Vec<&'a [i64]>,
    /// The range scanned.
    pub range: (i64, i64),
}

impl<'a> SpanChunks<'a> {
    fn new(spans: &'a [SeriesSpan], range: (i64, i64)) -> Self {
        let mut chunks = Vec::new();
        for s in spans.iter().filter(|s| !s.current.is_empty()) {
            chunks.push((&s.current, s.shape, true));
        }
        for s in spans.iter().filter(|s| !s.revisions.is_empty()) {
            chunks.push((&s.revisions, s.shape, false));
        }
        SpanChunks {
            chunks,
            received_at: spans.iter().map(|s| s.received_at.as_slice()).collect(),
            range,
        }
    }

    fn refs(&self) -> Vec<&'a Chunk> {
        self.chunks.iter().map(|c| c.0).collect()
    }

    fn current(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.chunks.len()).filter(|&i| self.chunks[i].2)
    }
}

/// A candidate behind a trait object, so several codecs share one data pass
/// and take turns.
pub trait Candidate {
    /// Candidate name.
    fn name(&self) -> &'static str;
    /// Encodes the span's chunks as one file.
    fn encode_timed(&self, span: &SpanChunks<'_>) -> (Box<dyn Any>, Duration);
    /// Checks the file and adds its sizes to `acc`. Untimed.
    fn verify(&self, file: &dyn Any, span: &SpanChunks<'_>, acc: &mut CandidateResult);
    /// Opens the file once.
    fn open_timed(&self, file: &dyn Any) -> Duration;
    /// Decodes every chunk.
    fn decode_timed(&self, file: &dyn Any, span: &SpanChunks<'_>) -> Duration;
    /// Range-scans every current chunk.
    fn range_timed(&self, file: &dyn Any, span: &SpanChunks<'_>) -> Duration;
}

fn shape_index(s: Shape) -> usize {
    SHAPES.iter().position(|&x| x == s).expect("known shape")
}

fn file<C: Codec>(f: &dyn Any) -> &C::Encoded
where
    C::Encoded: 'static,
{
    f.downcast_ref().expect("file of this candidate")
}

impl<C: Codec> Candidate for C
where
    C::Encoded: 'static,
{
    fn name(&self) -> &'static str {
        Codec::name(self)
    }

    fn encode_timed(&self, span: &SpanChunks<'_>) -> (Box<dyn Any>, Duration) {
        let refs = span.refs();
        let t0 = Instant::now();
        let e = black_box(self.encode(&refs));
        let d = t0.elapsed();
        (Box::new(e), d)
    }

    fn verify(&self, f: &dyn Any, span: &SpanChunks<'_>, acc: &mut CandidateResult) {
        let e = file::<C>(f);
        self.check(e);
        let (from, to) = span.range;
        for (i, &(c, _, _)) in span.chunks.iter().enumerate() {
            assert!(
                &self.decode(e, i) == c,
                "{}: chunk {i} does not round-trip",
                self.name()
            );
        }
        for i in span.current() {
            let expected = span.chunks[i].0.slice_t(from, to);
            let got = self.decode_range(e, i, from, to);
            assert!(
                got == expected,
                "{}: range of chunk {i} differs",
                self.name()
            );
            acc.range_rows += expected.len() as u64;
            acc.range_scans += 1;
        }
        for (i, s) in self.sizes(e).iter().enumerate() {
            let (c, shape, _) = span.chunks[i];
            acc.sizes.add(s);
            let entry = &mut acc.by_shape[shape_index(shape)];
            entry.0.add(s);
            entry.1 += c.len() as u64;
            acc.rows += c.len() as u64;
        }
        acc.chunks += span.chunks.len() as u64;
        acc.files += 1;
        acc.received_at += self.i64_column_size(&span.received_at);
    }

    fn open_timed(&self, f: &dyn Any) -> Duration {
        let e = file::<C>(f);
        let t0 = Instant::now();
        self.open(e);
        t0.elapsed()
    }

    fn decode_timed(&self, f: &dyn Any, span: &SpanChunks<'_>) -> Duration {
        let e = file::<C>(f);
        let t0 = Instant::now();
        for i in 0..span.chunks.len() {
            black_box(self.decode(e, i));
        }
        t0.elapsed()
    }

    fn range_timed(&self, f: &dyn Any, span: &SpanChunks<'_>) -> Duration {
        let e = file::<C>(f);
        let (from, to) = span.range;
        let t0 = Instant::now();
        for i in span.current() {
            black_box(self.decode_range(e, i, from, to));
        }
        t0.elapsed()
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
        Box::new(ParquetCodec::DICTIONARY),
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

#[expect(
    clippy::needless_range_loop,
    reason = "the repetition number indexes the results and rotates the order"
)]
fn run_span(
    candidates: &[Box<dyn Candidate>],
    span: &SpanChunks<'_>,
    accs: &mut [CandidateResult],
    reps: usize,
) {
    let n = candidates.len();
    let mut files: Vec<Option<Box<dyn Any>>> = (0..n).map(|_| None).collect();
    for r in 0..reps {
        for j in 0..n {
            let k = (j + r) % n;
            let (f, d) = candidates[k].encode_timed(span);
            accs[k].encode[r] += d;
            files[k] = Some(f);
        }
    }
    let files: Vec<Box<dyn Any>> = files.into_iter().map(|f| f.expect("encoded")).collect();
    for k in 0..n {
        candidates[k].verify(files[k].as_ref(), span, &mut accs[k]);
    }
    for r in 0..reps {
        for j in 0..n {
            let k = (j + r) % n;
            let f = files[k].as_ref();
            accs[k].open[r] += candidates[k].open_timed(f);
            accs[k].decode[r] += candidates[k].decode_timed(f, span);
            accs[k].range[r] += candidates[k].range_timed(f, span);
        }
    }
}

/// Measures `candidates` on `spec`.
pub fn run(spec: &DatasetSpec, reps: usize, candidates: &[Box<dyn Candidate>]) -> DatasetResult {
    let mut g = Generator::with_shapes(spec.seed, spec.series, spec.span_seconds, spec.shapes);
    if spec.integers {
        g = g.integers();
    }
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
        run_span(candidates, &SpanChunks::new(&spans, range), &mut accs, reps);
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

/// "median (min–max)" of a rate in million rows per second.
fn mrows(rows: u64, xs: &[Duration]) -> String {
    let (lo, mid, hi) = spread(xs);
    let rate = |d: Duration| rows as f64 / d.as_secs_f64() / 1e6;
    format!("{:.0} ({:.0}–{:.0})", rate(mid), rate(hi), rate(lo))
}

/// "median (min–max)" of a time in µs per item.
fn micros(items: u64, xs: &[Duration]) -> String {
    let (lo, mid, hi) = spread(xs);
    let us = |d: Duration| d.as_secs_f64() * 1e6 / items as f64;
    format!("{:.0} ({:.0}–{:.0})", us(mid), us(lo), us(hi))
}

/// Markdown tables for one data set.
pub fn report(r: &DatasetResult) -> String {
    let mut s = String::new();
    let rows = r.current_rows + r.revision_rows;
    let first = &r.candidates[0];
    s += &format!(
        "### {}\n\n{} current rows, {} revision rows, {} chunks in {} files; generated in {:.1} s. \
         Plain, uncompressed: 27 bytes per row.\n\n",
        r.spec.name,
        r.current_rows,
        r.revision_rows,
        first.chunks,
        first.files,
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
    let present: Vec<usize> = (0..SHAPES.len())
        .filter(|&i| first.by_shape[i].1 > 0)
        .collect();
    s += "\nSize by shape, bytes per row (value column in brackets):\n\n| Candidate |";
    for &i in &present {
        s += &format!(" {} |", SHAPES[i].name());
    }
    s += "\n|---|";
    for _ in &present {
        s += "---:|";
    }
    s += "\n";
    for c in &r.candidates {
        s += &format!("| {} |", c.name);
        for &i in &present {
            let (z, n) = &c.by_shape[i];
            s += &format!(" {} ({}) |", per_row(z.total(), *n), per_row(z.value, *n));
        }
        s += "\n";
    }
    s += "\nSpeed, median (min–max) of the repetitions; one thread; in memory:\n\n";
    s += "| Candidate | Encode, M rows/s | Full decode, M rows/s | 1-hour range scan, µs per chunk | File open, µs per file |\n";
    s += "|---|---:|---:|---:|---:|\n";
    for c in &r.candidates {
        s += &format!(
            "| {} | {} | {} | {} | {} |\n",
            c.name,
            mrows(c.rows, &c.encode),
            mrows(c.rows, &c.decode),
            micros(c.range_scans, &c.range),
            micros(c.files, &c.open),
        );
    }
    s += "\n";
    s
}
