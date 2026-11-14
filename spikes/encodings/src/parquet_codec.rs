//! Candidate (c): the same chunks written as Parquet.
//!
//! One file per span, one row group per chunk. Columns:
//!
//! | Column | Type | Encoding |
//! |---|---|---|
//! | `t` | INT64 | DELTA_BINARY_PACKED |
//! | `value` | DOUBLE, nullable (null for a row without a value, and for state series) | BYTE_STREAM_SPLIT, or ALP |
//! | `state` | INT32 (u16), nullable (null for f64 series) | dictionary (RLE_DICTIONARY) |
//! | `quality` | INT32 (u8) | dictionary |
//! | `flags` | INT32 (u16) | dictionary |
//! | `position` | INT64 (u64) | DELTA_BINARY_PACKED |
//!
//! Every column is compressed with zstd at [`ZSTD_LEVEL`]; data pages hold at
//! most [`BLOCK_ROWS`] rows; page statistics and the page index are written.
//! A reader caches the footer and page index once per file. A range scan uses
//! the page index of `t` to select whole pages, reads them with a row
//! selection and trims the rows outside the range.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type, UInt8Type, UInt16Type, UInt64Type};
use arrow_array::{
    Array, ArrayRef, Float64Array, Int64Array, RecordBatch, UInt8Array, UInt16Array, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use bytes::Bytes;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
};
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::metadata::PageIndexPolicy;
use parquet::file::page_index::column_index::ColumnIndexMetaData;
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use parquet::schema::types::ColumnPath;

use crate::{BLOCK_ROWS, Chunk, Codec, ColumnSizes, FLAG_NO_VALUE, ValueKind, ZSTD_LEVEL};

/// Batch size used when reading.
const READ_BATCH: usize = 16 * 1024;

/// The Parquet candidate, with a choice of encoding for `value`.
#[derive(Clone, Copy, Debug)]
pub struct ParquetCodec {
    value: Encoding,
    name: &'static str,
}

impl ParquetCodec {
    /// (c) `value` as BYTE_STREAM_SPLIT.
    pub const BYTE_STREAM_SPLIT: ParquetCodec = ParquetCodec {
        value: Encoding::BYTE_STREAM_SPLIT,
        name: "Parquet, BYTE_STREAM_SPLIT",
    };
    /// (c) `value` as ALP.
    pub const ALP: ParquetCodec = ParquetCodec {
        value: Encoding::ALP,
        name: "Parquet, ALP",
    };
}

/// One Parquet file holding a set of chunks, with its cached metadata.
pub struct ParquetFile {
    bytes: Bytes,
    meta: ArrowReaderMetadata,
    kinds: Vec<ValueKind>,
}

impl ParquetFile {
    /// The file's bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("t", DataType::Int64, false),
        Field::new("value", DataType::Float64, true),
        Field::new("state", DataType::UInt16, true),
        Field::new("quality", DataType::UInt8, false),
        Field::new("flags", DataType::UInt16, false),
        Field::new("position", DataType::UInt64, false),
    ]))
}

fn properties(columns: &[(&str, Encoding)]) -> WriterProperties {
    let mut b = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(ZSTD_LEVEL).expect("zstd level"),
        ))
        .set_data_page_row_count_limit(BLOCK_ROWS)
        .set_write_batch_size(1024)
        .set_statistics_enabled(EnabledStatistics::Page)
        .set_max_row_group_row_count(None);
    for &(name, encoding) in columns {
        let path = ColumnPath::from(name);
        b = b
            .set_column_dictionary_enabled(path.clone(), false)
            .set_column_encoding(path, encoding);
    }
    b.build()
}

fn batch(schema: &SchemaRef, c: &Chunk) -> RecordBatch {
    let n = c.len();
    let (value, state): (ArrayRef, ArrayRef) = match c.kind {
        ValueKind::F64 => {
            let v: Float64Array = (0..n)
                .map(|i| (c.flags[i] & FLAG_NO_VALUE == 0).then(|| f64::from_bits(c.value[i])))
                .collect();
            (Arc::new(v), Arc::new(UInt16Array::new_null(n)))
        }
        ValueKind::State => {
            let s: UInt16Array = (0..n)
                .map(|i| (c.flags[i] & FLAG_NO_VALUE == 0).then_some(c.value[i] as u16))
                .collect();
            (Arc::new(Float64Array::new_null(n)), Arc::new(s))
        }
    };
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(c.t.clone())),
        value,
        state,
        Arc::new(UInt8Array::from(c.quality.clone())),
        Arc::new(UInt16Array::from(c.flags.clone())),
        Arc::new(UInt64Array::from(c.position.clone())),
    ];
    RecordBatch::try_new(schema.clone(), columns).expect("record batch")
}

fn append(batch: &RecordBatch, kind: ValueKind, out: &mut Chunk) {
    out.t
        .extend_from_slice(batch.column(0).as_primitive::<Int64Type>().values());
    match kind {
        ValueKind::F64 => {
            let v = batch.column(1).as_primitive::<Float64Type>();
            if v.null_count() == 0 {
                out.value.extend(v.values().iter().map(|x| x.to_bits()));
            } else {
                out.value
                    .extend(v.iter().map(|x| x.map_or(0, f64::to_bits)));
            }
        }
        ValueKind::State => {
            let v = batch.column(2).as_primitive::<UInt16Type>();
            out.value.extend(v.iter().map(|x| x.map_or(0, u64::from)));
        }
    }
    out.quality
        .extend_from_slice(batch.column(3).as_primitive::<UInt8Type>().values());
    out.flags
        .extend_from_slice(batch.column(4).as_primitive::<UInt16Type>().values());
    out.position
        .extend_from_slice(batch.column(5).as_primitive::<UInt64Type>().values());
}

fn options() -> ArrowReaderOptions {
    ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required)
}

impl ParquetCodec {
    /// Checks that the writer used the intended encodings and page size.
    /// Panics otherwise.
    pub fn check_layout(&self, file: &ParquetFile) {
        let md = file.meta.metadata();
        assert_eq!(md.num_row_groups(), file.kinds.len());
        for (rg, kind) in md.row_groups().iter().zip(&file.kinds) {
            let has = |col: usize, e: Encoding| rg.column(col).encodings().any(|x| x == e);
            assert!(has(0, Encoding::DELTA_BINARY_PACKED), "t encoding");
            assert!(has(5, Encoding::DELTA_BINARY_PACKED), "position encoding");
            assert!(has(3, Encoding::RLE_DICTIONARY), "quality encoding");
            assert!(has(4, Encoding::RLE_DICTIONARY), "flags encoding");
            if *kind == ValueKind::F64 {
                assert!(has(1, self.value), "value encoding");
            }
        }
        for i in 0..md.num_row_groups() {
            let rows = md.row_group(i).num_rows() as usize;
            let pages = md
                .page_index_for_row_group(i)
                .offset_index(0)
                .expect("offset index")
                .page_locations()
                .len();
            assert_eq!(
                pages,
                rows.div_ceil(BLOCK_ROWS),
                "pages of t in row group {i}"
            );
        }
    }
}

impl Codec for ParquetCodec {
    type Encoded = ParquetFile;

    fn name(&self) -> &'static str {
        self.name
    }

    fn encode(&self, chunks: &[&Chunk]) -> ParquetFile {
        let schema = schema();
        let props = properties(&[
            ("t", Encoding::DELTA_BINARY_PACKED),
            ("value", self.value),
            ("position", Encoding::DELTA_BINARY_PACKED),
        ]);
        let mut w = ArrowWriter::try_new(Vec::new(), schema.clone(), Some(props)).expect("writer");
        for c in chunks {
            assert!(!c.is_empty(), "empty chunks are not written");
            w.write(&batch(&schema, c)).expect("write");
            w.flush().expect("flush");
        }
        let bytes = Bytes::from(w.into_inner().expect("close"));
        let meta = ArrowReaderMetadata::load(&bytes, options()).expect("metadata");
        ParquetFile {
            bytes,
            meta,
            kinds: chunks.iter().map(|c| c.kind).collect(),
        }
    }

    fn sizes(&self, f: &ParquetFile) -> Vec<ColumnSizes> {
        let md = f.meta.metadata();
        let mut out: Vec<ColumnSizes> = md
            .row_groups()
            .iter()
            .map(|rg| {
                let c = |i: usize| rg.column(i).compressed_size() as u64;
                ColumnSizes {
                    t: c(0),
                    value: c(1) + c(2),
                    quality_flags: c(3) + c(4),
                    position: c(5),
                    overhead: 0,
                }
            })
            .collect();
        let columns: u64 = out.iter().map(ColumnSizes::total).sum();
        let rest = f.bytes.len() as u64 - columns;
        let n = out.len() as u64;
        for (i, s) in out.iter_mut().enumerate() {
            s.overhead = rest / n + u64::from((i as u64) < rest % n);
        }
        out
    }

    fn open(&self, f: &ParquetFile) {
        std::hint::black_box(ArrowReaderMetadata::load(&f.bytes, options()).expect("metadata"));
    }

    fn decode(&self, f: &ParquetFile, index: usize) -> Chunk {
        let reader =
            ParquetRecordBatchReaderBuilder::new_with_metadata(f.bytes.clone(), f.meta.clone())
                .with_row_groups(vec![index])
                .with_batch_size(READ_BATCH)
                .build()
                .expect("reader");
        let kind = f.kinds[index];
        let mut out = Chunk::new(kind);
        for b in reader {
            append(&b.expect("batch"), kind, &mut out);
        }
        out
    }

    fn decode_range(&self, f: &ParquetFile, index: usize, from: i64, to: i64) -> Chunk {
        let md = f.meta.metadata();
        let rows = md.row_group(index).num_rows() as usize;
        let pi = md.page_index_for_row_group(index);
        let Some(ColumnIndexMetaData::INT64(ci)) = pi.column_index(0) else {
            panic!("no column index for t");
        };
        let locations = pi.offset_index(0).expect("offset index").page_locations();
        let ranges = (0..locations.len()).filter_map(|p| {
            let min = *ci.min_value(p).expect("min");
            let max = *ci.max_value(p).expect("max");
            (max >= from && min < to).then(|| {
                let start = locations[p].first_row_index as usize;
                let end = locations
                    .get(p + 1)
                    .map_or(rows, |l| l.first_row_index as usize);
                start..end
            })
        });
        let selection = RowSelection::from_consecutive_ranges(ranges, rows);
        let reader =
            ParquetRecordBatchReaderBuilder::new_with_metadata(f.bytes.clone(), f.meta.clone())
                .with_row_groups(vec![index])
                .with_row_selection(selection)
                .with_batch_size(READ_BATCH)
                .build()
                .expect("reader");
        let kind = f.kinds[index];
        let mut tmp = Chunk::new(kind);
        for b in reader {
            append(&b.expect("batch"), kind, &mut tmp);
        }
        tmp.slice_t(from, to)
    }

    fn i64_column_size(&self, columns: &[&[i64]]) -> u64 {
        let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)]));
        let props = properties(&[("x", Encoding::DELTA_BINARY_PACKED)]);
        let mut w = ArrowWriter::try_new(Vec::new(), schema.clone(), Some(props)).expect("writer");
        for col in columns.iter().filter(|c| !c.is_empty()) {
            let b = RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(Int64Array::from(col.to_vec())) as ArrayRef],
            )
            .expect("batch");
            w.write(&b).expect("write");
            w.flush().expect("flush");
        }
        let md = w.close().expect("close");
        md.row_groups()
            .iter()
            .map(|rg| rg.column(0).compressed_size() as u64)
            .sum()
    }
}
