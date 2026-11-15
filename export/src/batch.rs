//! Rows into Arrow record batches of the export schema.

use std::sync::Arc;

use arrow_array::builder::{
    BooleanBuilder, Decimal128Builder, Float64Builder, StringBuilder, TimestampNanosecondBuilder,
    UInt8Builder, UInt16Builder, UInt32Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::SchemaRef;
use kvik::{Quality, Value};

use crate::schema::{ChangedAt, DECIMAL_PRECISION, SeriesSchema, ValueKind, decimal_scale};
use crate::{ExportError, ExportRow};

/// The quality names of the `quality` columns (Q-194), best to worst.
pub const QUALITY_NAMES: [&str; 4] = ["good", "estimated", "suspect", "bad"];

pub(crate) fn quality_name(q: Quality) -> Result<&'static str, ExportError> {
    let i = match q {
        Quality::Good => 0,
        Quality::Estimated => 1,
        Quality::Suspect => 2,
        Quality::Bad => 3,
        _ => {
            return Err(ExportError::UnsupportedSeries {
                reason: "a quality the export does not know",
            });
        }
    };
    Ok(QUALITY_NAMES[i])
}

enum ValueColumn {
    F64(Float64Builder),
    Decimal {
        scale: u8,
        value: Decimal128Builder,
        stored: UInt8Builder,
    },
    Bool(BooleanBuilder),
    State {
        names: Vec<String>,
        index: UInt16Builder,
        name: StringBuilder,
    },
    Text(StringBuilder),
}

enum ChangedAtColumn {
    Time(TimestampNanosecondBuilder),
    Version(UInt64Builder),
}

/// A checked `changed_at`.
enum Stamp {
    Time(i64),
    Version(u64),
}

/// The value of one row, checked before anything is appended.
enum Cell<'a> {
    Null,
    F64(f64),
    Decimal(i128, u8),
    Bool(bool),
    State(u16),
    Text(&'a str),
}

/// Column builders for at most one batch of rows.
pub(crate) struct Columns {
    schema: SchemaRef,
    series: kvik::SeriesId,
    name: String,
    interval: Option<i64>,
    changed_at_kind: Option<ChangedAt>,
    rows: usize,
    series_id: UInt32Builder,
    series_name: StringBuilder,
    t: TimestampNanosecondBuilder,
    generation: UInt32Builder,
    sequence: UInt64Builder,
    received_at: TimestampNanosecondBuilder,
    value: ValueColumn,
    quality: StringBuilder,
    supplied_quality: StringBuilder,
    flags: [BooleanBuilder; 7],
    changed_at: Option<ChangedAtColumn>,
    interval_end: Option<TimestampNanosecondBuilder>,
}

fn ts(n: usize) -> TimestampNanosecondBuilder {
    TimestampNanosecondBuilder::with_capacity(n).with_timezone("UTC")
}

impl Columns {
    pub(crate) fn new(series: &SeriesSchema, capacity: usize) -> Result<Self, ExportError> {
        let n = capacity;
        let value = match &series.value {
            ValueKind::F64 => ValueColumn::F64(Float64Builder::with_capacity(n)),
            ValueKind::Decimal { scale } => ValueColumn::Decimal {
                scale: *scale,
                value: Decimal128Builder::with_capacity(n)
                    .with_precision_and_scale(DECIMAL_PRECISION, decimal_scale(*scale)?)?,
                stored: UInt8Builder::with_capacity(n),
            },
            ValueKind::Bool => ValueColumn::Bool(BooleanBuilder::with_capacity(n)),
            ValueKind::State(names) => ValueColumn::State {
                names: names.clone(),
                index: UInt16Builder::with_capacity(n),
                name: StringBuilder::new(),
            },
            ValueKind::Text => ValueColumn::Text(StringBuilder::new()),
        };
        Ok(Columns {
            schema: series.arrow_schema()?,
            series: series.id,
            name: series.name.clone(),
            interval: series.interval,
            changed_at_kind: series.changed_at,
            rows: 0,
            series_id: UInt32Builder::with_capacity(n),
            series_name: StringBuilder::new(),
            t: ts(n),
            generation: UInt32Builder::with_capacity(n),
            sequence: UInt64Builder::with_capacity(n),
            received_at: ts(n),
            value,
            quality: StringBuilder::new(),
            supplied_quality: StringBuilder::new(),
            flags: std::array::from_fn(|_| BooleanBuilder::with_capacity(n)),
            changed_at: series.changed_at.map(|c| match c {
                ChangedAt::Time => ChangedAtColumn::Time(ts(n)),
                ChangedAt::Version => ChangedAtColumn::Version(UInt64Builder::with_capacity(n)),
            }),
            interval_end: series.interval.map(|_| ts(n)),
        })
    }

    pub(crate) fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    pub(crate) fn len(&self) -> usize {
        self.rows
    }

    /// Appends one row; on an error nothing is appended.
    pub(crate) fn push(&mut self, row: &ExportRow) -> Result<(), ExportError> {
        let position = row.position;
        if row.series != self.series {
            return Err(ExportError::WrongSeries {
                position,
                expected: self.series,
                found: row.series,
            });
        }
        let cell = self.cell(row)?;
        let changed_at = match (self.changed_at_kind, row.changed_at) {
            (None, None) => None,
            (Some(ChangedAt::Time), Some(c)) => {
                Some(Stamp::Time(i64::try_from(c).map_err(|_| {
                    ExportError::ChangedAtOutOfRange {
                        position,
                        changed_at: c,
                    }
                })?))
            }
            (Some(ChangedAt::Version), Some(c)) => Some(Stamp::Version(c)),
            _ => return Err(ExportError::ChangedAtMismatch { position }),
        };
        let interval_end = match self.interval {
            None => None,
            Some(d) => Some(
                row.t
                    .0
                    .checked_add(d)
                    .ok_or(ExportError::IntervalEndOutOfRange { position })?,
            ),
        };
        let quality = quality_name(row.quality)?;
        let supplied = quality_name(row.supplied_quality)?;

        self.series_id.append_value(row.series.0);
        self.series_name.append_value(&self.name);
        self.t.append_value(row.t.0);
        self.generation.append_value(position.generation);
        self.sequence.append_value(position.sequence);
        self.received_at.append_value(row.received_at.0);
        self.append_cell(cell);
        self.quality.append_value(quality);
        self.supplied_quality.append_value(supplied);
        for (b, f) in self.flags.iter_mut().zip(row.flags.to_array()) {
            b.append_value(f);
        }
        match (&mut self.changed_at, changed_at) {
            (Some(ChangedAtColumn::Time(b)), Some(Stamp::Time(c))) => b.append_value(c),
            (Some(ChangedAtColumn::Version(b)), Some(Stamp::Version(c))) => b.append_value(c),
            _ => {}
        }
        if let (Some(b), Some(end)) = (&mut self.interval_end, interval_end) {
            b.append_value(end);
        }
        self.rows += 1;
        Ok(())
    }

    fn cell<'r>(&self, row: &'r ExportRow) -> Result<Cell<'r>, ExportError> {
        let position = row.position;
        let wrong = ExportError::WrongValue { position };
        Ok(match (&self.value, &row.value) {
            (_, Value::Absent) => Cell::Null,
            (ValueColumn::F64(_), Value::F64(x)) => Cell::F64(*x),
            (
                ValueColumn::Decimal { scale, .. },
                Value::Decimal {
                    units,
                    scale: stored,
                },
            ) => {
                let shift = scale
                    .checked_sub(*stored)
                    .ok_or(ExportError::ScaleAboveExport {
                        position,
                        stored: *stored,
                        export: *scale,
                    })?;
                let v = 10i128
                    .checked_pow(u32::from(shift))
                    .and_then(|m| i128::from(*units).checked_mul(m))
                    .filter(|v| v.unsigned_abs() < 10u128.pow(u32::from(DECIMAL_PRECISION)))
                    .ok_or(ExportError::DecimalOutOfRange { position })?;
                Cell::Decimal(v, *stored)
            }
            (ValueColumn::Bool(_), Value::Bool(b)) => Cell::Bool(*b),
            (ValueColumn::State { names, .. }, Value::State(i))
                if usize::from(*i) < names.len() =>
            {
                Cell::State(*i)
            }
            (ValueColumn::Text(_), Value::Text(s)) => Cell::Text(s),
            _ => return Err(wrong),
        })
    }

    fn append_cell(&mut self, cell: Cell<'_>) {
        match (&mut self.value, cell) {
            (ValueColumn::F64(b), Cell::F64(x)) => b.append_value(x),
            (ValueColumn::F64(b), _) => b.append_null(),
            (ValueColumn::Decimal { value, stored, .. }, Cell::Decimal(v, s)) => {
                value.append_value(v);
                stored.append_value(s);
            }
            (ValueColumn::Decimal { value, stored, .. }, _) => {
                value.append_null();
                stored.append_null();
            }
            (ValueColumn::Bool(b), Cell::Bool(x)) => b.append_value(x),
            (ValueColumn::Bool(b), _) => b.append_null(),
            (ValueColumn::State { names, index, name }, Cell::State(i)) => {
                index.append_value(i);
                name.append_option(names.get(usize::from(i)));
            }
            (ValueColumn::State { index, name, .. }, _) => {
                index.append_null();
                name.append_null();
            }
            (ValueColumn::Text(b), Cell::Text(s)) => b.append_value(s),
            (ValueColumn::Text(b), _) => b.append_null(),
        }
    }

    /// The rows pushed since the last call, as one batch; the builders start empty again.
    pub(crate) fn finish(&mut self) -> Result<RecordBatch, ExportError> {
        let mut cols: Vec<ArrayRef> = vec![
            Arc::new(self.series_id.finish()),
            Arc::new(self.series_name.finish()),
            Arc::new(self.t.finish()),
            Arc::new(self.generation.finish()),
            Arc::new(self.sequence.finish()),
            Arc::new(self.received_at.finish()),
        ];
        match &mut self.value {
            ValueColumn::F64(b) => cols.push(Arc::new(b.finish())),
            ValueColumn::Decimal { value, stored, .. } => {
                cols.push(Arc::new(value.finish()));
                cols.push(Arc::new(stored.finish()));
            }
            ValueColumn::Bool(b) => cols.push(Arc::new(b.finish())),
            ValueColumn::State { index, name, .. } => {
                cols.push(Arc::new(index.finish()));
                cols.push(Arc::new(name.finish()));
            }
            ValueColumn::Text(b) => cols.push(Arc::new(b.finish())),
        }
        cols.push(Arc::new(self.quality.finish()));
        cols.push(Arc::new(self.supplied_quality.finish()));
        for b in &mut self.flags {
            cols.push(Arc::new(b.finish()));
        }
        match &mut self.changed_at {
            Some(ChangedAtColumn::Time(b)) => cols.push(Arc::new(b.finish())),
            Some(ChangedAtColumn::Version(b)) => cols.push(Arc::new(b.finish())),
            None => {}
        }
        if let Some(b) = &mut self.interval_end {
            cols.push(Arc::new(b.finish()));
        }
        self.rows = 0;
        Ok(RecordBatch::try_new(self.schema.clone(), cols)?)
    }
}
