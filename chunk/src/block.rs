//! A block body: up to 4,096 rows of one chunk, column by column (`docs/format-chunk.md` C.2, C.9, C.10).

use crate::bytes::{Reader, put_leb128};
use crate::error::{DecodeError, Element, EncodeError};
use crate::float::{
    best_decimal, decode_decimal, decode_xor, decode_xor_zstd, decompress_xor, encode_decimal,
    encode_xor, encode_xor_zstd,
};
use crate::ints::{
    Fixed, decode_delta, decode_plain, decode_run_length, delta_len, encode_delta, encode_plain,
    encode_run_length,
};
use crate::{MAX_ROWS, MAX_TEXT_LEN, column, encoding, flag, value_type};

/// The rows of one block as equal-length columns, laid out as a facts block (`format.md` A.5) without `series_id`;
/// `heap` holds the texts in row order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Block {
    /// Event time, UTC nanoseconds.
    pub t: Vec<i64>,
    /// Position generation.
    pub generation: Vec<u32>,
    /// Position sequence.
    pub sequence: Vec<u64>,
    /// Adapter-supplied quality, 0–3.
    pub quality: Vec<u8>,
    /// Fact flags; bits 8–15 must be zero.
    pub flags: Vec<u16>,
    /// Value type, 1–5.
    pub value_type: Vec<u8>,
    /// Value word: `f64` bits, decimal integer, state index, bool or text length; 0 for a row without a value.
    pub value: Vec<u64>,
    /// Text bytes.
    pub heap: Vec<u8>,
    /// `changed_at`; 0 for a row without flag bit 6.
    pub changed_at: Vec<u64>,
    /// Stored decimal scale, 0–18; 0 for a row whose value type is not 2.
    pub scale: Vec<u8>,
}

/// One column of a body: id, encoding, payload.
type Column = (u8, u8, Vec<u8>);

/// The smaller of plain and run-length for a fixed-width column; plain wins a tie (as `format.md` A.5).
fn small<T: Fixed>(id: u8, values: &[T]) -> Column {
    let rle = encode_run_length(values);
    if rle.len() < values.len() * T::WIDTH {
        (id, encoding::RUN_LENGTH, rle)
    } else {
        (id, encoding::PLAIN, encode_plain(values))
    }
}

/// The smaller of plain and delta for a 64-bit column; plain wins a tie.
fn wide(id: u8, values: &[i64]) -> Column {
    if delta_len(values) < values.len() * 8 {
        (id, encoding::DELTA, encode_delta(values))
    } else {
        (id, encoding::PLAIN, encode_plain(values))
    }
}

/// The shortest value payload outside the XOR class (C.9): plain, run-length, delta and, when every value is an
/// `f64`, decimal at its best exponent; a tie goes to the lower encoding id. Also whether the XOR class (encodings
/// 4 and 5) is a candidate: every value an `f64` and no decimal candidate, or one with more than one exception per
/// eight values.
fn other_candidates(words: &[u64], all_f64: bool) -> ((u8, Vec<u8>), bool) {
    fn offer(best: &mut (u8, Vec<u8>), enc: u8, payload: Vec<u8>) {
        if payload.len() < best.1.len() {
            *best = (enc, payload);
        }
    }
    let mut best = (encoding::PLAIN, encode_plain(words));
    offer(&mut best, encoding::RUN_LENGTH, encode_run_length(words));
    let ints: Vec<i64> = words.iter().map(|&w| w as i64).collect();
    if delta_len(&ints) < best.1.len() {
        offer(&mut best, encoding::DELTA, encode_delta(&ints));
    }
    if !(all_f64 && !words.is_empty()) {
        return (best, false);
    }
    let decimal = best_decimal(words);
    if let Some((e, _, len)) = decimal
        && len < best.1.len()
    {
        offer(&mut best, encoding::DECIMAL, encode_decimal(words, e));
    }
    let xor = decimal.is_none_or(|(_, exceptions, _)| exceptions * 8 > words.len());
    (best, xor)
}

/// The canonical value choice up to the XOR class, independent of the zstd build (C.9, Q-150).
enum ValueChoice {
    /// The XOR class, holding this encoding 4 stream.
    Xor(Vec<u8>),
    /// Another encoding and its payload.
    Other(u8, Vec<u8>),
}

/// The XOR class when it is a candidate and its encoding 4 payload is strictly shorter than [`other_candidates`];
/// compressed lengths never enter (C.9).
fn value_choice(words: &[u64], all_f64: bool) -> ValueChoice {
    let ((enc, payload), xor_class) = other_candidates(words, all_f64);
    if xor_class {
        let xor = encode_xor(words);
        if xor.len() < payload.len() {
            return ValueChoice::Xor(xor);
        }
    }
    ValueChoice::Other(enc, payload)
}

/// The value column (C.9): [`value_choice`], and within the XOR class compressed XOR when strictly shorter.
fn value_column(words: &[u64], all_f64: bool) -> Result<Column, EncodeError> {
    let (enc, payload) = match value_choice(words, all_f64) {
        ValueChoice::Other(enc, payload) => (enc, payload),
        ValueChoice::Xor(xor) => {
            let zstd = encode_xor_zstd(&xor)?;
            if zstd.len() < xor.len() {
                (encoding::XOR_ZSTD, zstd)
            } else {
                (encoding::XOR, xor)
            }
        }
    };
    Ok((column::VALUE, enc, payload))
}

impl Block {
    /// The value words of the rows that have a value, in row order, and whether every one is an `f64`.
    fn value_words(&self) -> (Vec<u64>, bool) {
        let rows = (0..self.len()).filter(|&i| self.flags[i] & flag::NO_VALUE == 0);
        let words = rows.clone().map(|i| self.value[i]).collect();
        let all_f64 = rows.clone().all(|i| self.value_type[i] == value_type::F64);
        (words, all_f64)
    }

    /// The number of rows.
    pub fn len(&self) -> usize {
        self.t.len()
    }

    /// Whether the block has no rows (never true for a decoded block).
    pub fn is_empty(&self) -> bool {
        self.t.is_empty()
    }

    /// The text of each row: `Some` for a row of value type 5 that has a value, else `None`. Valid for a decoded
    /// block or one that [`Block::encode`] accepts.
    pub fn texts(&self) -> Vec<Option<&str>> {
        let mut at = 0usize;
        (0..self.len())
            .map(|i| {
                if self.value_type[i] != value_type::TEXT || self.flags[i] & flag::NO_VALUE != 0 {
                    return None;
                }
                let len = self.value[i] as usize;
                let s = self
                    .heap
                    .get(at..at + len)
                    .and_then(|b| std::str::from_utf8(b).ok());
                at += len;
                s
            })
            .collect()
    }

    fn check(&self) -> Result<(), EncodeError> {
        let n = self.len();
        if n == 0 || n > MAX_ROWS {
            return Err(EncodeError::RowCount(n));
        }
        for (id, len) in [
            (column::GENERATION, self.generation.len()),
            (column::SEQUENCE, self.sequence.len()),
            (column::QUALITY, self.quality.len()),
            (column::FLAGS, self.flags.len()),
            (column::VALUE_TYPE, self.value_type.len()),
            (column::VALUE, self.value.len()),
            (column::CHANGED_AT, self.changed_at.len()),
            (column::SCALE, self.scale.len()),
        ] {
            if len != n {
                return Err(EncodeError::Length { column: id });
            }
        }
        let mut at = 0usize;
        for row in 0..n {
            let bad = |column| Err(EncodeError::Value { column, row });
            let (f, vt, v) = (self.flags[row], self.value_type[row], self.value[row]);
            if self.quality[row] > 3 {
                return bad(column::QUALITY);
            }
            if f & flag::UNDEFINED != 0 {
                return bad(column::FLAGS);
            }
            if !(value_type::F64..=value_type::TEXT).contains(&vt) {
                return bad(column::VALUE_TYPE);
            }
            if f & flag::HAS_CHANGED_AT == 0 && self.changed_at[row] != 0 {
                return bad(column::CHANGED_AT);
            }
            let scale_ok = if vt == value_type::DECIMAL {
                self.scale[row] <= 18
            } else {
                self.scale[row] == 0
            };
            if !scale_ok {
                return bad(column::SCALE);
            }
            let value_ok = if f & flag::NO_VALUE != 0 {
                v == 0
            } else {
                match vt {
                    value_type::DECIMAL => v as i64 != i64::MIN,
                    value_type::STATE => v <= u64::from(u16::MAX),
                    value_type::BOOL => v <= 1,
                    value_type::TEXT => v <= MAX_TEXT_LEN as u64,
                    _ => true,
                }
            };
            if !value_ok {
                return bad(column::VALUE);
            }
            if vt == value_type::TEXT && f & flag::NO_VALUE == 0 {
                let len = v as usize;
                let ok = self
                    .heap
                    .get(at..at + len)
                    .is_some_and(|b| std::str::from_utf8(b).is_ok());
                if !ok {
                    return Err(EncodeError::Text { row });
                }
                at += len;
            }
        }
        if at != self.heap.len() {
            return Err(EncodeError::Text { row: n });
        }
        Ok(())
    }

    /// The canonical body for these rows (`docs/format-chunk.md` C.10), or the rule the content breaks.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        self.check()?;
        let n = self.len();
        let mut cols: Vec<Column> = Vec::with_capacity(10);
        cols.push(wide(column::T, &self.t));
        if self.quality.iter().any(|&q| q != 0) {
            cols.push(small(column::QUALITY, &self.quality));
        }
        if self.flags.iter().any(|&f| f != 0) {
            cols.push(small(column::FLAGS, &self.flags));
        }
        cols.push(small(column::VALUE_TYPE, &self.value_type));
        let (words, all_f64) = self.value_words();
        cols.push(value_column(&words, all_f64)?);
        if self.value_type.contains(&value_type::TEXT) {
            cols.push((column::TEXT_HEAP, encoding::PLAIN, self.heap.clone()));
        }
        let changed: Vec<i64> = (0..n)
            .filter(|&i| self.flags[i] & flag::HAS_CHANGED_AT != 0)
            .map(|i| self.changed_at[i] as i64)
            .collect();
        if !changed.is_empty() {
            cols.push(wide(column::CHANGED_AT, &changed));
        }
        let scales: Vec<u8> = (0..n)
            .filter(|&i| self.value_type[i] == value_type::DECIMAL)
            .map(|i| self.scale[i])
            .collect();
        if !scales.is_empty() {
            cols.push(small(column::SCALE, &scales));
        }
        cols.push(small(column::GENERATION, &self.generation));
        let seq: Vec<i64> = self.sequence.iter().map(|&s| s as i64).collect();
        cols.push(wide(column::SEQUENCE, &seq));

        let mut out = Vec::with_capacity(3 + cols.iter().map(|c| 5 + c.2.len()).sum::<usize>());
        out.extend_from_slice(&(n as u16).to_le_bytes());
        out.push(cols.len() as u8);
        for (id, enc, payload) in &cols {
            out.push(*id);
            out.push(*enc);
            put_leb128(&mut out, payload.len() as u64);
        }
        for (_, _, payload) in &cols {
            out.extend_from_slice(payload);
        }
        Ok(out)
    }

    /// Decodes a body, applying every check of `docs/format-chunk.md` C.11. An unknown ignorable column is skipped.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        decode(body)
    }

    /// Decodes a body and also requires it to be what a writer on any zstd build writes for its content (C.10): in
    /// the XOR class, encoding 4 or a single frame of encoding 5 shorter than the stream it holds (Q-150).
    pub fn decode_canonical(body: &[u8]) -> Result<Self, DecodeError> {
        let b = decode(body)?;
        let again = b.encode().map_err(|_| DecodeError::NonCanonical)?;
        if again == body || canonical_in_xor_class(body, &again, &b) {
            Ok(b)
        } else {
            Err(DecodeError::NonCanonical)
        }
    }
}

/// The directory of a body that has decoded: (id, encoding, payload) per column.
pub(crate) fn directory(body: &[u8]) -> Option<Vec<(u8, u8, &[u8])>> {
    let mut r = Reader::new(body);
    r.u16().ok()?;
    let count = r.u8().ok()?;
    let mut dir = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let id = r.u8().ok()?;
        let enc = r.u8().ok()?;
        let len = r.leb128(3).ok()? as usize;
        dir.push((id, enc, len));
    }
    let mut cols = Vec::with_capacity(dir.len());
    for (id, enc, len) in dir {
        cols.push((id, enc, r.bytes(len).ok()?));
    }
    Some(cols)
}

/// Whether `body`, which differs from the writer's output `again` for the same rows, differs only in a value
/// column that another zstd build could have written: the XOR class is the canonical choice and the column holds
/// its stream as encoding 4, or as encoding 5 with a payload shorter than that stream (C.10).
fn canonical_in_xor_class(body: &[u8], again: &[u8], block: &Block) -> bool {
    let (Some(da), Some(db)) = (directory(body), directory(again)) else {
        return false;
    };
    if body.get(..2) != again.get(..2) || da.len() != db.len() {
        return false;
    }
    da.iter().zip(&db).all(|(x, y)| {
        if x == y {
            return true;
        }
        if x.0 != y.0 || x.0 != column::VALUE {
            return false;
        }
        let (words, all_f64) = block.value_words();
        let ValueChoice::Xor(stream) = value_choice(&words, all_f64) else {
            return false;
        };
        match x.1 {
            encoding::XOR => x.2 == stream.as_slice(),
            encoding::XOR_ZSTD => {
                x.2.len() < stream.len()
                    && decompress_xor(x.2, words.len()).is_ok_and(|s| s == stream)
            }
            _ => false,
        }
    })
}

fn decode(body: &[u8]) -> Result<Block, DecodeError> {
    use DecodeError as E;
    let mut r = Reader::new(body);
    let rows = r.u16().map_err(|_| E::Directory)?;
    let n = usize::from(rows);
    if n == 0 || n > MAX_ROWS {
        return Err(E::RowCount(rows));
    }
    let count = r.u8().map_err(|_| E::Directory)?;
    let mut dir: Vec<(u8, u8, usize)> = Vec::with_capacity(usize::from(count));
    let mut total = 0usize;
    for _ in 0..count {
        let id = r.u8().map_err(|_| E::Directory)?;
        let enc = r.u8().map_err(|_| E::Directory)?;
        let len = r.leb128(3).map_err(|_| E::Directory)? as usize;
        if dir.last().is_some_and(|&(last, _, _)| id <= last) {
            return Err(E::ColumnOrder { column: id });
        }
        total += len;
        dir.push((id, enc, len));
    }
    if body.len() - r.position() != total {
        return Err(E::BodyLength);
    }
    let mut cols: [Option<(u8, &[u8])>; 12] = Default::default();
    for (id, enc, len) in dir {
        let payload = r.bytes(len).map_err(|_| E::BodyLength)?;
        match id {
            column::T..=column::SEQUENCE => cols[usize::from(id)] = Some((enc, payload)),
            0x80.. => {}
            _ => return Err(E::Unsupported(Element::Column(id))),
        }
    }
    for id in [
        column::T,
        column::VALUE_TYPE,
        column::VALUE,
        column::GENERATION,
        column::SEQUENCE,
    ] {
        if cols[usize::from(id)].is_none() {
            return Err(E::MissingColumn { column: id });
        }
    }
    let col_err = |column| move |error| E::Column { column, error };

    // Fixed-width columns that may be plain or run-length.
    fn small<T: Fixed>(c: Option<(u8, &[u8])>, id: u8, n: usize) -> Result<Option<Vec<T>>, E> {
        let Some((enc, payload)) = c else {
            return Ok(None);
        };
        match enc {
            encoding::PLAIN => decode_plain(payload, n),
            encoding::RUN_LENGTH => decode_run_length(payload, n),
            _ => return Err(E::Encoding { column: id }),
        }
        .map(Some)
        .map_err(|error| E::Column { column: id, error })
    }
    // 64-bit columns that may be plain or delta.
    fn wide(c: Option<(u8, &[u8])>, id: u8, n: usize) -> Result<Option<Vec<i64>>, E> {
        let Some((enc, payload)) = c else {
            return Ok(None);
        };
        match enc {
            encoding::PLAIN => decode_plain(payload, n),
            encoding::DELTA => decode_delta(payload, n),
            _ => return Err(E::Encoding { column: id }),
        }
        .map(Some)
        .map_err(|error| E::Column { column: id, error })
    }

    let flags: Vec<u16> =
        small(cols[usize::from(column::FLAGS)], column::FLAGS, n)?.unwrap_or_else(|| vec![0; n]);
    // Undefined flag bits are a version refusal, tested before any per-row check.
    if let Some(f) = flags.iter().find(|&&f| f & flag::UNDEFINED != 0) {
        return Err(E::Unsupported(Element::FactFlags(f & flag::UNDEFINED)));
    }
    let t = wide(cols[usize::from(column::T)], column::T, n)?.unwrap_or_default();
    let quality: Vec<u8> = small(cols[usize::from(column::QUALITY)], column::QUALITY, n)?
        .unwrap_or_else(|| vec![0; n]);
    let vtypes: Vec<u8> =
        small(cols[usize::from(column::VALUE_TYPE)], column::VALUE_TYPE, n)?.unwrap_or_default();
    let generation: Vec<u32> =
        small(cols[usize::from(column::GENERATION)], column::GENERATION, n)?.unwrap_or_default();
    let sequence: Vec<u64> = wide(cols[usize::from(column::SEQUENCE)], column::SEQUENCE, n)?
        .unwrap_or_default()
        .into_iter()
        .map(|s| s as u64)
        .collect();

    for (row, (&q, &vt)) in quality.iter().zip(&vtypes).enumerate() {
        let row = row as u16;
        if q > 3 {
            return Err(E::Value {
                column: column::QUALITY,
                row,
            });
        }
        if !(value_type::F64..=value_type::TEXT).contains(&vt) {
            return Err(E::Value {
                column: column::VALUE_TYPE,
                row,
            });
        }
    }
    let has_value = |i: usize| flags[i] & flag::NO_VALUE == 0;
    let valued = (0..n).filter(|&i| has_value(i)).count();
    let any_text = vtypes.contains(&value_type::TEXT);
    let changed_rows = flags
        .iter()
        .filter(|&&f| f & flag::HAS_CHANGED_AT != 0)
        .count();
    let decimal_rows = vtypes.iter().filter(|&&v| v == value_type::DECIMAL).count();
    for (present, needed, id) in [
        (
            cols[usize::from(column::TEXT_HEAP)].is_some(),
            any_text,
            column::TEXT_HEAP,
        ),
        (
            cols[usize::from(column::CHANGED_AT)].is_some(),
            changed_rows > 0,
            column::CHANGED_AT,
        ),
        (
            cols[usize::from(column::SCALE)].is_some(),
            decimal_rows > 0,
            column::SCALE,
        ),
    ] {
        if present != needed {
            return Err(E::Presence { column: id });
        }
    }

    let (venc, vpayload) = cols[usize::from(column::VALUE)].unwrap_or_default();
    // Encodings 3–5 hold `f64` values only: at least one row has a value word and every such row has value type 1.
    let f64_only = valued > 0
        && (0..n)
            .filter(|&i| has_value(i))
            .all(|i| vtypes[i] == value_type::F64);
    if matches!(venc, encoding::DECIMAL..=encoding::XOR_ZSTD) && !f64_only {
        return Err(E::Encoding {
            column: column::VALUE,
        });
    }
    let words: Vec<u64> = match venc {
        encoding::PLAIN => decode_plain(vpayload, valued),
        encoding::RUN_LENGTH => decode_run_length(vpayload, valued),
        encoding::DELTA => {
            decode_delta(vpayload, valued).map(|v| v.into_iter().map(|w| w as u64).collect())
        }
        encoding::DECIMAL => decode_decimal(vpayload, valued),
        encoding::XOR => decode_xor(vpayload, valued),
        encoding::XOR_ZSTD => decode_xor_zstd(vpayload, valued),
        _ => {
            return Err(E::Encoding {
                column: column::VALUE,
            });
        }
    }
    .map_err(col_err(column::VALUE))?;
    let heap: Vec<u8> = match cols[usize::from(column::TEXT_HEAP)] {
        Some((encoding::PLAIN, h)) => h.to_vec(),
        Some(_) => {
            return Err(E::Encoding {
                column: column::TEXT_HEAP,
            });
        }
        None => Vec::new(),
    };
    let changed = wide(
        cols[usize::from(column::CHANGED_AT)],
        column::CHANGED_AT,
        changed_rows,
    )?
    .unwrap_or_default();
    let scales: Vec<u8> = small(
        cols[usize::from(column::SCALE)],
        column::SCALE,
        decimal_rows,
    )?
    .unwrap_or_default();

    let mut value = vec![0u64; n];
    let mut changed_at = vec![0u64; n];
    let mut scale = vec![0u8; n];
    let (mut w, mut c, mut s, mut at) = (words.iter(), changed.iter(), scales.iter(), 0usize);
    for i in 0..n {
        let row = i as u16;
        let vt = vtypes[i];
        if flags[i] & flag::HAS_CHANGED_AT != 0 {
            changed_at[i] = c.next().map_or(0, |&x| x as u64);
        }
        if vt == value_type::DECIMAL {
            let sc = s.next().copied().unwrap_or(0);
            if sc > 18 {
                return Err(E::Value {
                    column: column::SCALE,
                    row,
                });
            }
            scale[i] = sc;
        }
        if !has_value(i) {
            continue;
        }
        let v = w.next().copied().unwrap_or(0);
        let ok = match vt {
            value_type::DECIMAL => v as i64 != i64::MIN,
            value_type::STATE => v <= u64::from(u16::MAX),
            value_type::BOOL => v <= 1,
            value_type::TEXT => v <= MAX_TEXT_LEN as u64,
            _ => true,
        };
        if !ok {
            return Err(E::Value {
                column: column::VALUE,
                row,
            });
        }
        if vt == value_type::TEXT {
            let len = v as usize;
            let text_ok = heap
                .get(at..at + len)
                .is_some_and(|b| std::str::from_utf8(b).is_ok());
            if !text_ok {
                return Err(E::Text { row });
            }
            at += len;
        }
        value[i] = v;
    }
    if at != heap.len() {
        return Err(E::Text { row: rows });
    }
    Ok(Block {
        t,
        generation,
        sequence,
        quality,
        flags,
        value_type: vtypes,
        value,
        heap,
        changed_at,
        scale,
    })
}

#[cfg(test)]
mod tests;
