//! Facts block bodies (`format.md` A.5): typed rows, the canonical encoder and the decoder with every check.

use crate::block::block_type;
use crate::body::{DATA_PREFIX_LEN, DataPrefix, MAX_ROWS};
use crate::bytes::{i64_at, put, u32_at, u64_at};
use crate::content_error::{BodyError, ContentError};
use crate::error::{Element, EncodeError};
use crate::rle;

/// Size of a v1 column directory entry.
pub const DIR_ENTRY_LEN: u8 = 12;
/// The longest text value, in bytes.
pub const MAX_TEXT_LEN: usize = 255;
/// The largest decimal scale.
pub const MAX_SCALE: u8 = 18;
/// Bytes per row of the plain-encoded size that closes a facts block (A.4): every fixed-width column.
pub const PLAIN_ROW_LEN: u64 = 33;
/// The plain-encoded size at which a facts block is closed (A.4).
pub const MAX_PLAIN_LEN: u64 = 8 << 20;

/// Column ids (A.5).
pub mod column {
    /// `series_id`, u32, mandatory.
    pub const SERIES_ID: u8 = 1;
    /// `t`, i64, mandatory.
    pub const T: u8 = 2;
    /// `quality`, u8, optional.
    pub const QUALITY: u8 = 3;
    /// `flags`, u16, optional.
    pub const FLAGS: u8 = 4;
    /// `value_type`, u8, mandatory.
    pub const VALUE_TYPE: u8 = 5;
    /// `value`, 8 bytes, mandatory.
    pub const VALUE: u8 = 6;
    /// Text heap, present exactly when a row is text.
    pub const TEXT_HEAP: u8 = 7;
    /// `changed_at`, u64, present exactly when a row has flag bit 6.
    pub const CHANGED_AT: u8 = 8;
    /// `scale`, u8, present exactly when a row is decimal.
    pub const SCALE: u8 = 9;
}

/// Column encodings (A.5).
pub mod encoding {
    /// Plain: `row_count` fixed-width values.
    pub const PLAIN: u8 = 0;
    /// Run-length.
    pub const RUN_LENGTH: u8 = 1;
}

/// Fact flag bits (A.5).
pub mod flag {
    /// `t` was ahead of `received_at` by more than the future tolerance.
    pub const FUTURE: u16 = 1 << 0;
    /// The reading arrived late.
    pub const LATE: u16 = 1 << 1;
    /// Submitted with `revise` intent.
    pub const REVISE: u16 = 1 << 2;
    /// `t` stamped from the host clock.
    pub const HOST_TIME: u16 = 1 << 3;
    /// Committed before the clock was synchronised.
    pub const UNSYNCED_CLOCK: u16 = 1 << 4;
    /// Recorded without a value.
    pub const NO_VALUE: u16 = 1 << 5;
    /// The row carries `changed_at` (column 8).
    pub const HAS_CHANGED_AT: u16 = 1 << 6;
    /// The value was rounded to the series' scale.
    pub const ROUNDED: u16 = 1 << 7;
    /// Bits 8 to 15: undefined in v1 and critical.
    pub const UNDEFINED: u16 = 0xFF00;
}

/// A value type (A.5 `value_type`, A.6 field 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ValueType {
    /// IEEE-754 `f64`.
    F64 = 1,
    /// Scaled `i64`: value = integer × 10^−scale.
    Decimal = 2,
    /// A named state, by index.
    State = 3,
    /// A boolean.
    Bool = 4,
    /// UTF-8 text, at most 255 bytes.
    Text = 5,
}

impl ValueType {
    /// The type of code `c`.
    pub fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            1 => Self::F64,
            2 => Self::Decimal,
            3 => Self::State,
            4 => Self::Bool,
            5 => Self::Text,
            _ => return None,
        })
    }

    /// Its code.
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// The adapter-supplied quality (A.5): stored as supplied, also on a row without a value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Quality {
    /// Good.
    #[default]
    Good = 0,
    /// Estimated.
    Estimated = 1,
    /// Suspect.
    Suspect = 2,
    /// Bad.
    Bad = 3,
}

impl Quality {
    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => Self::Good,
            1 => Self::Estimated,
            2 => Self::Suspect,
            3 => Self::Bad,
            _ => return None,
        })
    }
}

/// The flags of a row other than `no_value` and `has_changed_at`, which follow from [`Value::Absent`] and
/// [`FactRow::changed_at`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FactFlags {
    /// Bit 0.
    pub future: bool,
    /// Bit 1.
    pub late: bool,
    /// Bit 2.
    pub revise: bool,
    /// Bit 3.
    pub host_time: bool,
    /// Bit 4.
    pub unsynced_clock: bool,
    /// Bit 7.
    pub rounded: bool,
}

/// A row's value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Value {
    /// An `f64`, as its bit pattern.
    F64(u64),
    /// A decimal: `units` × 10^−`scale`, at the scale it was stored at (0 to 18).
    Decimal {
        /// The scaled integer.
        units: i64,
        /// The scale.
        scale: u8,
    },
    /// A named state, by index.
    State(u16),
    /// A boolean.
    Bool(bool),
    /// Text, at most 255 bytes.
    Text(String),
    /// No value (flag bit 5): the row keeps the series' value type, and a decimal row its scale (0 otherwise).
    Absent {
        /// The series' value type.
        value_type: ValueType,
        /// The scale of a decimal row; 0 for other types.
        scale: u8,
    },
}

impl Value {
    /// The value type.
    pub fn value_type(&self) -> ValueType {
        match self {
            Self::F64(_) => ValueType::F64,
            Self::Decimal { .. } => ValueType::Decimal,
            Self::State(_) => ValueType::State,
            Self::Bool(_) => ValueType::Bool,
            Self::Text(_) => ValueType::Text,
            Self::Absent { value_type, .. } => *value_type,
        }
    }

    fn scale(&self) -> u8 {
        match self {
            Self::Decimal { scale, .. } | Self::Absent { scale, .. } => *scale,
            _ => 0,
        }
    }
}

/// One fact row.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FactRow {
    /// The series.
    pub series_id: u32,
    /// Event time.
    pub t: i64,
    /// The adapter-supplied quality.
    pub quality: Quality,
    /// Flags.
    pub flags: FactFlags,
    /// The value.
    pub value: Value,
    /// `changed_at` of a row of a versioned series (flag bit 6).
    pub changed_at: Option<u64>,
}

impl FactRow {
    /// The row's share of the plain-encoded size that closes a block (A.4): 33 bytes plus its text.
    pub fn plain_len(&self) -> u64 {
        PLAIN_ROW_LEN
            + if let Value::Text(s) = &self.value {
                s.len() as u64
            } else {
                0
            }
    }

    fn flag_bits(&self) -> u16 {
        let f = &self.flags;
        let mut b = 0;
        for (on, bit) in [
            (f.future, flag::FUTURE),
            (f.late, flag::LATE),
            (f.revise, flag::REVISE),
            (f.host_time, flag::HOST_TIME),
            (f.unsynced_clock, flag::UNSYNCED_CLOCK),
            (matches!(self.value, Value::Absent { .. }), flag::NO_VALUE),
            (self.changed_at.is_some(), flag::HAS_CHANGED_AT),
            (f.rounded, flag::ROUNDED),
        ] {
            if on {
                b |= bit;
            }
        }
        b
    }
}

/// A facts block body.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FactsBlock {
    /// Sequence of the first row.
    pub first_sequence: u64,
    /// The batch's receive time.
    pub received_at: i64,
    /// The rows, 1 to 65,536.
    pub rows: Vec<FactRow>,
}

fn pad8(b: &mut Vec<u8>) {
    b.resize(b.len().div_ceil(8) * 8, 0);
}

/// A u8 or u16 column in its canonical encoding: run-length exactly when smaller than plain (Q-106 (c)).
fn small_column<T: rle::Fixed>(values: &[T]) -> (u8, Vec<u8>) {
    let r = rle::encode(values);
    if r.len() < values.len() * T::WIDTH {
        (encoding::RUN_LENGTH, r)
    } else {
        let mut p = Vec::with_capacity(values.len() * T::WIDTH);
        values.iter().for_each(|v| v.put(&mut p));
        (encoding::PLAIN, p)
    }
}

impl FactsBlock {
    /// The canonical body.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let n = self.rows.len();
        let count = u32::try_from(n).ok().filter(|c| (1..=MAX_ROWS).contains(c));
        let count = count.ok_or(EncodeError::Content(ContentError::RowCount(
            n.min(u32::MAX as usize) as u32,
        )))?;
        let mut heap = Vec::new();
        let mut values = Vec::with_capacity(n * 8);
        for (i, r) in self.rows.iter().enumerate() {
            let bad = |column| {
                EncodeError::Content(ContentError::Value {
                    column,
                    row: i as u32,
                })
            };
            let v: u64 = match &r.value {
                Value::F64(bits) => *bits,
                Value::Decimal { units, scale } => {
                    if *scale > MAX_SCALE {
                        return Err(bad(column::SCALE));
                    }
                    *units as u64
                }
                Value::State(s) => u64::from(*s),
                Value::Bool(b) => u64::from(*b),
                Value::Text(s) => {
                    if s.len() > MAX_TEXT_LEN {
                        return Err(EncodeError::Content(ContentError::Text { row: i as u32 }));
                    }
                    let off = heap.len() as u64;
                    heap.extend_from_slice(s.as_bytes());
                    off | (s.len() as u64) << 32
                }
                Value::Absent { value_type, scale } => {
                    let allowed = if *value_type == ValueType::Decimal {
                        MAX_SCALE
                    } else {
                        0
                    };
                    if *scale > allowed {
                        return Err(bad(column::SCALE));
                    }
                    0
                }
            };
            values.extend_from_slice(&v.to_le_bytes());
        }
        let quality: Vec<u8> = self.rows.iter().map(|r| r.quality as u8).collect();
        let flags: Vec<u16> = self.rows.iter().map(FactRow::flag_bits).collect();
        let vtypes: Vec<u8> = self
            .rows
            .iter()
            .map(|r| r.value.value_type().code())
            .collect();
        let scales: Vec<u8> = self.rows.iter().map(|r| r.value.scale()).collect();
        let any_text = vtypes.contains(&ValueType::Text.code());
        let any_changed = self.rows.iter().any(|r| r.changed_at.is_some());
        let any_decimal = vtypes.contains(&ValueType::Decimal.code());

        let mut columns: Vec<(u8, u8, Vec<u8>)> = Vec::new();
        columns.push((
            column::SERIES_ID,
            encoding::PLAIN,
            self.rows
                .iter()
                .flat_map(|r| r.series_id.to_le_bytes())
                .collect(),
        ));
        columns.push((
            column::T,
            encoding::PLAIN,
            self.rows.iter().flat_map(|r| r.t.to_le_bytes()).collect(),
        ));
        if quality.iter().any(|&q| q != 0) {
            let (e, p) = small_column(&quality);
            columns.push((column::QUALITY, e, p));
        }
        if flags.iter().any(|&f| f != 0) {
            let (e, p) = small_column(&flags);
            columns.push((column::FLAGS, e, p));
        }
        let (e, p) = small_column(&vtypes);
        columns.push((column::VALUE_TYPE, e, p));
        columns.push((column::VALUE, encoding::PLAIN, values));
        if any_text {
            columns.push((column::TEXT_HEAP, encoding::PLAIN, heap));
        }
        if any_changed {
            let c = self
                .rows
                .iter()
                .flat_map(|r| r.changed_at.unwrap_or(0).to_le_bytes())
                .collect();
            columns.push((column::CHANGED_AT, encoding::PLAIN, c));
        }
        if any_decimal {
            let (e, p) = small_column(&scales);
            columns.push((column::SCALE, e, p));
        }

        let mut body = vec![0u8; DATA_PREFIX_LEN];
        DataPrefix {
            first_sequence: self.first_sequence,
            received_at: self.received_at,
            count,
        }
        .write(&mut body);
        body[20] = columns.len() as u8;
        body[21] = DIR_ENTRY_LEN;
        let dir = body.len();
        body.resize(dir + usize::from(DIR_ENTRY_LEN) * columns.len(), 0);
        for (i, (id, enc, payload)) in columns.iter().enumerate() {
            pad8(&mut body);
            let e = dir + usize::from(DIR_ENTRY_LEN) * i;
            body[e] = *id;
            body[e + 1] = *enc;
            let offset = body.len() as u32;
            put(&mut body, e + 4, &offset.to_le_bytes());
            put(&mut body, e + 8, &(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
        }
        Ok(body)
    }

    /// Decodes a body, applying every check of A.5. An unknown ignorable column is skipped.
    pub fn decode(body: &[u8]) -> Result<Self, BodyError> {
        decode(body)
    }

    /// Decodes a body and also requires it to be exactly what [`FactsBlock::encode`] writes for its content.
    pub fn decode_canonical(body: &[u8]) -> Result<Self, BodyError> {
        let b = decode(body)?;
        match b.encode() {
            Ok(again) if again == body => Ok(b),
            _ => Err(BodyError::NonCanonical),
        }
    }
}

struct Col<'a> {
    encoding: u8,
    payload: &'a [u8],
}

fn decode(body: &[u8]) -> Result<FactsBlock, BodyError> {
    use ContentError as E;
    let p = DataPrefix::decode(block_type::FACTS, body).map_err(|_| {
        if body.len() < DATA_PREFIX_LEN {
            E::Prefix
        } else {
            E::RowCount(u32_at(body, 16))
        }
    })?;
    let rows = p.count;
    let n = rows as usize;
    let column_count = usize::from(body[20]);
    let des = body[21];
    if des < DIR_ENTRY_LEN {
        return Err(E::DirEntrySize(des).into());
    }
    let dir_end = DATA_PREFIX_LEN + column_count * usize::from(des);
    if dir_end > body.len() {
        return Err(E::Directory.into());
    }
    let mut cols: [Option<Col<'_>>; 10] = Default::default();
    let mut ranges: Vec<(usize, usize, u8)> = Vec::with_capacity(column_count);
    let mut last_id: Option<u8> = None;
    for i in 0..column_count {
        let e = &body[DATA_PREFIX_LEN + i * usize::from(des)..];
        let (id, enc) = (e[0], e[1]);
        let (offset, length) = (u32_at(e, 4) as usize, u32_at(e, 8) as usize);
        if last_id.is_some_and(|l| id <= l) {
            return Err(E::ColumnOrder { column: id }.into());
        }
        last_id = Some(id);
        let end = offset.checked_add(length).filter(|&end| end <= body.len());
        let Some(end) = end.filter(|_| offset >= dir_end) else {
            return Err(E::PayloadOutside { column: id }.into());
        };
        ranges.push((offset, end, id));
        match id {
            1..=9 => {
                cols[usize::from(id)] = Some(Col {
                    encoding: enc,
                    payload: &body[offset..end],
                })
            }
            0x80.. => {}
            _ => return Err(BodyError::Unsupported(Element::Column(id))),
        }
    }
    ranges.sort_unstable();
    for w in ranges.windows(2) {
        if w[1].0 < w[0].1 {
            return Err(E::Overlap { column: w[1].2 }.into());
        }
    }
    for id in [
        column::SERIES_ID,
        column::T,
        column::VALUE_TYPE,
        column::VALUE,
    ] {
        if cols[usize::from(id)].is_none() {
            return Err(E::MissingColumn { column: id }.into());
        }
    }
    // Fixed-width plain columns, and u8 or u16 columns that may also be run-length.
    let plain = |id: u8, width: usize| -> Result<Option<&[u8]>, BodyError> {
        let Some(c) = &cols[usize::from(id)] else {
            return Ok(None);
        };
        if c.encoding != encoding::PLAIN {
            return Err(E::Encoding { column: id }.into());
        }
        if c.payload.len() != n * width {
            return Err(E::Length { column: id }.into());
        }
        Ok(Some(c.payload))
    };
    fn small<T: rle::Fixed>(
        c: &Option<Col<'_>>,
        id: u8,
        n: usize,
    ) -> Result<Option<Vec<T>>, BodyError> {
        let Some(c) = c else { return Ok(None) };
        match c.encoding {
            encoding::PLAIN if c.payload.len() == n * T::WIDTH => {
                Ok(Some(c.payload.chunks_exact(T::WIDTH).map(T::get).collect()))
            }
            encoding::PLAIN => Err(ContentError::Length { column: id }.into()),
            encoding::RUN_LENGTH => rle::decode(c.payload, n as u32)
                .map(Some)
                .ok_or(ContentError::RunLength { column: id }.into()),
            _ => Err(ContentError::Encoding { column: id }.into()),
        }
    }
    let series = plain(column::SERIES_ID, 4)?.unwrap_or_default();
    let ts = plain(column::T, 8)?.unwrap_or_default();
    let value = plain(column::VALUE, 8)?.unwrap_or_default();
    let changed = plain(column::CHANGED_AT, 8)?;
    let heap = match &cols[usize::from(column::TEXT_HEAP)] {
        Some(c) if c.encoding != encoding::PLAIN => {
            return Err(E::Encoding {
                column: column::TEXT_HEAP,
            }
            .into());
        }
        Some(c) => Some(c.payload),
        None => None,
    };
    let quality: Option<Vec<u8>> = small(&cols[usize::from(column::QUALITY)], column::QUALITY, n)?;
    let flags: Option<Vec<u16>> = small(&cols[usize::from(column::FLAGS)], column::FLAGS, n)?;
    let vtypes: Vec<u8> = small(
        &cols[usize::from(column::VALUE_TYPE)],
        column::VALUE_TYPE,
        n,
    )?
    .unwrap_or_default();
    let scales: Option<Vec<u8>> = small(&cols[usize::from(column::SCALE)], column::SCALE, n)?;

    let mut out = Vec::with_capacity(n);
    let (mut any_text, mut any_changed, mut any_decimal) = (false, false, false);
    for i in 0..n {
        let row = i as u32;
        let q = quality.as_ref().map_or(0, |q| q[i]);
        let quality = Quality::from_code(q).ok_or(E::Value {
            column: column::QUALITY,
            row,
        })?;
        let f = flags.as_ref().map_or(0, |f| f[i]);
        if f & flag::UNDEFINED != 0 {
            return Err(BodyError::Unsupported(Element::FactFlags(
                f & flag::UNDEFINED,
            )));
        }
        let vt = ValueType::from_code(vtypes[i]).ok_or(E::Value {
            column: column::VALUE_TYPE,
            row,
        })?;
        let raw = u64_at(value, i * 8);
        let scale = scales.as_ref().map_or(0, |s| s[i]);
        if vt == ValueType::Decimal {
            any_decimal = true;
            if scale > MAX_SCALE {
                return Err(E::Value {
                    column: column::SCALE,
                    row,
                }
                .into());
            }
        } else if scale != 0 {
            return Err(E::Presence {
                column: column::SCALE,
            }
            .into());
        }
        any_text |= vt == ValueType::Text;
        let changed_at = if f & flag::HAS_CHANGED_AT != 0 {
            any_changed = true;
            Some(changed.map_or(0, |c| u64_at(c, i * 8)))
        } else {
            if changed.is_some_and(|c| u64_at(c, i * 8) != 0) {
                return Err(E::Presence {
                    column: column::CHANGED_AT,
                }
                .into());
            }
            None
        };
        let value = if f & flag::NO_VALUE != 0 {
            if raw != 0 {
                return Err(E::Value {
                    column: column::VALUE,
                    row,
                }
                .into());
            }
            Value::Absent {
                value_type: vt,
                scale,
            }
        } else {
            match vt {
                ValueType::F64 => Value::F64(raw),
                ValueType::Decimal => Value::Decimal {
                    units: raw as i64,
                    scale,
                },
                ValueType::State if raw <= u64::from(u16::MAX) => Value::State(raw as u16),
                ValueType::Bool if raw <= 1 => Value::Bool(raw == 1),
                ValueType::State | ValueType::Bool => {
                    return Err(E::Value {
                        column: column::VALUE,
                        row,
                    }
                    .into());
                }
                ValueType::Text => {
                    let (off, len) = ((raw & 0xFFFF_FFFF) as usize, (raw >> 32) as usize);
                    let h = heap.unwrap_or_default();
                    let text = off
                        .checked_add(len)
                        .filter(|_| len <= MAX_TEXT_LEN)
                        .and_then(|end| h.get(off..end))
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .ok_or(E::Text { row })?;
                    Value::Text(text.to_owned())
                }
            }
        };
        out.push(FactRow {
            series_id: u32_at(series, i * 4),
            t: i64_at(ts, i * 8),
            quality,
            flags: FactFlags {
                future: f & flag::FUTURE != 0,
                late: f & flag::LATE != 0,
                revise: f & flag::REVISE != 0,
                host_time: f & flag::HOST_TIME != 0,
                unsynced_clock: f & flag::UNSYNCED_CLOCK != 0,
                rounded: f & flag::ROUNDED != 0,
            },
            value,
            changed_at,
        });
    }
    for (present, needed, id) in [
        (heap.is_some(), any_text, column::TEXT_HEAP),
        (changed.is_some(), any_changed, column::CHANGED_AT),
        (scales.is_some(), any_decimal, column::SCALE),
    ] {
        if present != needed {
            return Err(E::Presence { column: id }.into());
        }
    }
    Ok(FactsBlock {
        first_sequence: p.first_sequence,
        received_at: p.received_at,
        rows: out,
    })
}
