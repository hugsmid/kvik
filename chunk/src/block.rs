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

/// The rows of one block, as columns of equal length (`row_count`), except `heap`.
///
/// The representation follows a facts block (`format.md` A.5) without `series_id` and with the position:
/// - `value` holds, per row, the `f64` bits, the decimal integer (two's complement), the state index, 0 or 1 for a
///   bool, or the length of a text; 0 for a row flagged `no_value`.
/// - `heap` holds the texts of the rows of value type 5 that have a value, in row order, none shared; a text's
///   offset is the sum of the lengths before it.
/// - `changed_at` is 0 for a row without flag bit 6; `scale` is 0 for a row whose value type is not 2.
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
    /// Value word.
    pub value: Vec<u64>,
    /// Text bytes.
    pub heap: Vec<u8>,
    /// `changed_at`.
    pub changed_at: Vec<u64>,
    /// Stored decimal scale, 0–18.
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

/// The value column (C.9): the shortest payload among plain, run-length and delta, and, when every value is an
/// `f64`, decimal at its best exponent and, if that has no candidate or more than one exception per eight values,
/// XOR and compressed XOR. A tie goes to the lower encoding id.
fn value_column(words: &[u64], all_f64: bool) -> Column {
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
    if all_f64 && !words.is_empty() {
        let decimal = best_decimal(words);
        if let Some((e, _, len)) = decimal
            && len < best.1.len()
        {
            offer(&mut best, encoding::DECIMAL, encode_decimal(words, e));
        }
        if decimal.is_none_or(|(_, exceptions, _)| exceptions * 8 > words.len()) {
            let xor = encode_xor(words);
            let zstd = encode_xor_zstd(&xor);
            offer(&mut best, encoding::XOR, xor);
            if let Some(z) = zstd {
                offer(&mut best, encoding::XOR_ZSTD, z);
            }
        }
    }
    (column::VALUE, best.0, best.1)
}

impl Block {
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
        let has_value = |i: usize| self.flags[i] & flag::NO_VALUE == 0;
        let mut cols: Vec<Column> = Vec::with_capacity(10);
        cols.push(wide(column::T, &self.t));
        if self.quality.iter().any(|&q| q != 0) {
            cols.push(small(column::QUALITY, &self.quality));
        }
        if self.flags.iter().any(|&f| f != 0) {
            cols.push(small(column::FLAGS, &self.flags));
        }
        cols.push(small(column::VALUE_TYPE, &self.value_type));
        let words: Vec<u64> = (0..n)
            .filter(|&i| has_value(i))
            .map(|i| self.value[i])
            .collect();
        let all_f64 = (0..n)
            .filter(|&i| has_value(i))
            .all(|i| self.value_type[i] == value_type::F64);
        cols.push(value_column(&words, all_f64));
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

    /// Decodes a body and also requires it to be what [`Block::encode`] writes for its content. A compressed XOR
    /// payload is compared by the stream it decompresses to, not by its compressed bytes (Q-150).
    pub fn decode_canonical(body: &[u8]) -> Result<Self, DecodeError> {
        let b = decode(body)?;
        let again = b.encode().map_err(|_| DecodeError::NonCanonical)?;
        if again == body || same_modulo_compression(body, &again, &b) {
            Ok(b)
        } else {
            Err(DecodeError::NonCanonical)
        }
    }
}

/// The directory of a body that has decoded: (id, encoding, payload) per column.
fn directory(body: &[u8]) -> Option<Vec<(u8, u8, &[u8])>> {
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

/// Whether two bodies of the same rows differ only in the compressed bytes of an encoding 5 value column that
/// decompresses to the same XOR stream.
fn same_modulo_compression(a: &[u8], b: &[u8], block: &Block) -> bool {
    let (Some(da), Some(db)) = (directory(a), directory(b)) else {
        return false;
    };
    let n = block
        .flags
        .iter()
        .filter(|&&f| f & flag::NO_VALUE == 0)
        .count();
    a.get(..2) == b.get(..2)
        && da.len() == db.len()
        && da.iter().zip(&db).all(|(x, y)| {
            x.0 == y.0
                && x.1 == y.1
                && (x.2 == y.2
                    || (x.0 == column::VALUE
                        && x.1 == encoding::XOR_ZSTD
                        && matches!(
                            (decompress_xor(x.2, n), decompress_xor(y.2, n)),
                            (Ok(p), Ok(q)) if p == q
                        )))
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
mod tests {
    use super::*;
    use crate::error::ColumnError;

    fn sample(n: usize) -> Block {
        Block {
            t: (0..n as i64)
                .map(|i| 1_767_225_600_000_000_000 + i * 1_000_000_000)
                .collect(),
            generation: vec![1; n],
            sequence: (0..n as u64).map(|i| 100 + 3 * i + i % 2).collect(),
            quality: vec![0; n],
            flags: vec![0; n],
            value_type: vec![value_type::F64; n],
            value: (0..n)
                .map(|i| (20.0 + (i % 50) as f64 / 100.0).to_bits())
                .collect(),
            heap: vec![],
            changed_at: vec![0; n],
            scale: vec![0; n],
        }
    }

    #[test]
    fn round_trips_at_frame_and_block_edges() {
        for n in [1usize, 2, 127, 128, 129, 4095, 4096] {
            let b = sample(n);
            let body = b.encode().unwrap();
            assert_eq!(Block::decode_canonical(&body).unwrap(), b, "{n}");
        }
    }

    #[test]
    fn every_row_without_a_value() {
        let mut b = sample(300);
        b.flags = vec![flag::NO_VALUE; 300];
        b.quality = vec![3; 300];
        b.value = vec![0; 300];
        let body = b.encode().unwrap();
        assert_eq!(Block::decode_canonical(&body).unwrap(), b);
    }

    #[test]
    fn row_count_limits() {
        assert_eq!(Block::default().encode(), Err(EncodeError::RowCount(0)));
        assert_eq!(sample(4097).encode(), Err(EncodeError::RowCount(4097)));
        let mut body = sample(3).encode().unwrap();
        body[..2].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(Block::decode(&body), Err(DecodeError::RowCount(0)));
        body[..2].copy_from_slice(&4097u16.to_le_bytes());
        assert_eq!(Block::decode(&body), Err(DecodeError::RowCount(4097)));
    }

    #[test]
    fn truncation_at_every_offset_is_rejected() {
        let body = sample(200).encode().unwrap();
        for k in 0..body.len() {
            assert!(Block::decode(&body[..k]).is_err(), "{k}");
        }
        let mut long = body.clone();
        long.push(0);
        assert_eq!(Block::decode(&long), Err(DecodeError::BodyLength));
    }

    #[test]
    fn undefined_flags_are_a_version_refusal() {
        let mut b = sample(2);
        b.flags[1] = 0x0100;
        assert!(matches!(
            b.encode(),
            Err(EncodeError::Value { column: 4, row: 1 })
        ));
        // Hand-built: flags column plain with bit 8 set in row 1.
        b.flags[1] = 0x0001;
        let body = b.encode().unwrap();
        let mut d = directory(&body).unwrap();
        let flags = d.iter().position(|c| c.0 == column::FLAGS).unwrap();
        let mut payload = d[flags].2.to_vec();
        assert_eq!(d[flags].1, encoding::PLAIN);
        payload[3] = 0x01;
        d[flags].2 = &payload;
        let body = rebuild(2, &d);
        assert_eq!(
            Block::decode(&body),
            Err(DecodeError::Unsupported(Element::FactFlags(0x0100)))
        );
    }

    /// A body from a directory, in the given order.
    pub(crate) fn rebuild(rows: u16, cols: &[(u8, u8, &[u8])]) -> Vec<u8> {
        let mut out = rows.to_le_bytes().to_vec();
        out.push(cols.len() as u8);
        for (id, enc, p) in cols {
            out.push(*id);
            out.push(*enc);
            put_leb128(&mut out, p.len() as u64);
        }
        for (_, _, p) in cols {
            out.extend_from_slice(p);
        }
        out
    }

    /// A body of `vt.len()` rows with plain columns and the given extra columns.
    fn plain_body(vt: &[u8], values: &[u64], extra: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let n = vt.len();
        let t = encode_plain(&(0..n as i64).collect::<Vec<_>>());
        let gens = encode_plain(&vec![1u32; n]);
        let seq = encode_plain(&(1..=n as u64).collect::<Vec<_>>());
        let vals = encode_plain(values);
        let mut cols: Vec<(u8, u8, &[u8])> = vec![
            (column::T, encoding::PLAIN, &t),
            (column::VALUE_TYPE, encoding::PLAIN, vt),
            (column::VALUE, encoding::PLAIN, &vals),
            (column::GENERATION, encoding::PLAIN, &gens),
            (column::SEQUENCE, encoding::PLAIN, &seq),
        ];
        for (id, p) in extra {
            cols.push((*id, encoding::PLAIN, p));
        }
        cols.sort_by_key(|c| c.0);
        rebuild(n as u16, &cols)
    }

    #[test]
    fn value_ranges_are_checked() {
        let v = |column, row| Err(DecodeError::Value { column, row });
        assert!(Block::decode(&plain_body(&[1, 1], &[5, 6], &[])).is_ok());
        assert_eq!(
            Block::decode(&plain_body(
                &[1, 1],
                &[5, 6],
                &[(column::QUALITY, vec![0, 4])]
            )),
            v(column::QUALITY, 1)
        );
        assert_eq!(
            Block::decode(&plain_body(&[1, 6], &[5, 6], &[])),
            v(column::VALUE_TYPE, 1)
        );
        assert_eq!(
            Block::decode(&plain_body(&[0, 1], &[5, 6], &[])),
            v(column::VALUE_TYPE, 0)
        );
        let scale = |s: u8| (column::SCALE, vec![s]);
        assert!(Block::decode(&plain_body(&[2], &[5], &[scale(18)])).is_ok());
        assert_eq!(
            Block::decode(&plain_body(&[2], &[5], &[scale(19)])),
            v(column::SCALE, 0)
        );
        assert_eq!(
            Block::decode(&plain_body(&[2], &[i64::MIN as u64], &[scale(2)])),
            v(column::VALUE, 0)
        );
        assert!(Block::decode(&plain_body(&[3], &[65_535], &[])).is_ok());
        assert_eq!(
            Block::decode(&plain_body(&[3], &[65_536], &[])),
            v(column::VALUE, 0)
        );
        assert_eq!(
            Block::decode(&plain_body(&[4], &[2], &[])),
            v(column::VALUE, 0)
        );
        let heap = |h: &[u8]| (column::TEXT_HEAP, h.to_vec());
        assert!(Block::decode(&plain_body(&[5, 5], &[2, 0], &[heap(b"ok")])).is_ok());
        assert_eq!(
            Block::decode(&plain_body(&[5], &[256], &[heap(&[b'a'; 256])])),
            v(column::VALUE, 0)
        );
        assert_eq!(
            Block::decode(&plain_body(&[5], &[2], &[heap(&[0xC3, 0x28])])),
            Err(DecodeError::Text { row: 0 })
        );
        assert_eq!(
            Block::decode(&plain_body(&[5], &[2], &[heap(b"abc")])),
            Err(DecodeError::Text { row: 1 })
        );
        assert_eq!(
            Block::decode(&plain_body(&[5], &[3], &[heap(b"ab")])),
            Err(DecodeError::Text { row: 0 })
        );
        // Presence: text rows without a heap, a heap without text rows, a changed_at column without flag bit 6.
        assert_eq!(
            Block::decode(&plain_body(&[5], &[0], &[])),
            Err(DecodeError::Presence {
                column: column::TEXT_HEAP
            })
        );
        assert_eq!(
            Block::decode(&plain_body(&[1], &[0], &[heap(b"")])),
            Err(DecodeError::Presence {
                column: column::TEXT_HEAP
            })
        );
        assert_eq!(
            Block::decode(&plain_body(&[1], &[0], &[(column::CHANGED_AT, vec![0; 8])])),
            Err(DecodeError::Presence {
                column: column::CHANGED_AT
            })
        );
        // A plain body is valid but not canonical where run-length or delta is shorter.
        let body = plain_body(&[1; 40], &[7; 40], &[]);
        assert!(Block::decode(&body).is_ok());
        assert_eq!(
            Block::decode_canonical(&body),
            Err(DecodeError::NonCanonical)
        );
    }

    #[test]
    fn non_minimal_delta_is_valid_but_not_canonical() {
        let b = sample(3);
        let body = b.encode().unwrap();
        let mut d = directory(&body).unwrap();
        assert_eq!(d[0], (column::T, encoding::DELTA, d[0].2));
        // t on a 1 s grid: first value, then one frame with min 10^9 and width 0; widen it to 1 bit (2 zero bits).
        let mut t = d[0].2.to_vec();
        let w = t.len() - 1;
        assert_eq!(t[w], 0);
        t[w] = 1;
        t.push(0);
        d[0].2 = &t;
        let wide = rebuild(3, &d);
        assert_eq!(Block::decode(&wide), Ok(b));
        assert_eq!(
            Block::decode_canonical(&wide),
            Err(DecodeError::NonCanonical)
        );
    }

    #[test]
    fn directory_checks() {
        let body = sample(5).encode().unwrap();
        let d = directory(&body).unwrap();
        // Unknown critical column.
        let mut x = d.clone();
        x.push((0x12, 0, &[]));
        assert_eq!(
            Block::decode(&rebuild(5, &x)),
            Err(DecodeError::Unsupported(Element::Column(0x12)))
        );
        // Unknown ignorable column: skipped, but not canonical.
        let mut x = d.clone();
        x.push((0x90, 7, &[1, 2, 3]));
        let b = Block::decode(&rebuild(5, &x)).unwrap();
        assert_eq!(b, sample(5));
        assert_eq!(
            Block::decode_canonical(&rebuild(5, &x)),
            Err(DecodeError::NonCanonical)
        );
        // Out of order.
        let mut x = d.clone();
        x.swap(0, 1);
        assert!(matches!(
            Block::decode(&rebuild(5, &x)),
            Err(DecodeError::ColumnOrder { .. })
        ));
        // Missing mandatory column.
        let x: Vec<_> = d
            .iter()
            .copied()
            .filter(|c| c.0 != column::SEQUENCE)
            .collect();
        assert_eq!(
            Block::decode(&rebuild(5, &x)),
            Err(DecodeError::MissingColumn {
                column: column::SEQUENCE
            })
        );
        // Encoding not allowed.
        let mut x = d.clone();
        x[0].1 = encoding::XOR;
        assert_eq!(
            Block::decode(&rebuild(5, &x)),
            Err(DecodeError::Encoding { column: column::T })
        );
        // Presence: a scale column without decimal rows.
        let mut x = d.clone();
        let at = x.iter().position(|c| c.0 > column::SCALE).unwrap();
        x.insert(at, (column::SCALE, 0, &[]));
        assert_eq!(
            Block::decode(&rebuild(5, &x)),
            Err(DecodeError::Presence {
                column: column::SCALE
            })
        );
        // A plain column of the wrong length.
        let mut x = d.clone();
        let vt = x.iter().position(|c| c.0 == column::VALUE_TYPE).unwrap();
        let short = [1u8; 4];
        x[vt] = (column::VALUE_TYPE, encoding::PLAIN, &short);
        assert_eq!(
            Block::decode(&rebuild(5, &x)),
            Err(DecodeError::Column {
                column: column::VALUE_TYPE,
                error: ColumnError::Length
            })
        );
    }
}
