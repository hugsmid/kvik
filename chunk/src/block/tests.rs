//! Unit tests of block bodies: layout, row limits, directory, value ranges and canonical checks.

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

/// 2,000 `f64` values from five non-decimal readings: the writer picks compressed XOR.
fn ninths() -> Block {
    let mut b = sample(2_000);
    for (i, v) in b.value.iter_mut().enumerate() {
        *v = (f64::from((i % 5) as u32) / 9.0 + 1.0).to_bits();
    }
    b
}

/// `body` with its value column replaced.
fn with_value(body: &[u8], enc: u8, payload: &[u8]) -> Vec<u8> {
    let mut d = directory(body).unwrap();
    let v = d.iter().position(|c| c.0 == column::VALUE).unwrap();
    d[v] = (column::VALUE, enc, payload);
    rebuild(u16::from_le_bytes([body[0], body[1]]), &d)
}

#[test]
fn xor_class_is_one_canonical_class() {
    let b = ninths();
    let body = b.encode().unwrap();
    let d = directory(&body).unwrap();
    let value = d.iter().find(|c| c.0 == column::VALUE).unwrap();
    assert_eq!(value.1, encoding::XOR_ZSTD);
    let (words, _) = b.value_words();
    let xor = encode_xor(&words);
    // Other compression levels are other valid frames of the same stream: canonical.
    for level in [1, 9, 19] {
        let mut p = (xor.len() as u32).to_le_bytes().to_vec();
        p.extend_from_slice(&zstd::bulk::compress(&xor, level).unwrap());
        let other = with_value(&body, encoding::XOR_ZSTD, &p);
        assert_eq!(
            Block::decode_canonical(&other).as_ref(),
            Ok(&b),
            "level {level}"
        );
    }
    // A trailing skippable frame is not exactly one frame: invalid.
    let mut p = value.2.to_vec();
    p.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18, 0, 0, 0, 0]);
    assert_eq!(
        Block::decode(&with_value(&body, encoding::XOR_ZSTD, &p)),
        Err(DecodeError::Column {
            column: column::VALUE,
            error: ColumnError::Frame
        })
    );
    // The same stream uncompressed is in the class, but here not shorter than every other candidate.
    let raw = with_value(&body, encoding::XOR, &xor);
    assert_eq!(Block::decode(&raw).as_ref(), Ok(&b));
    let (other, class) = other_candidates(&words, true);
    assert!(class);
    assert_eq!(
        Block::decode_canonical(&raw).is_ok(),
        xor.len() < other.1.len()
    );
    // A stream that is valid but not the canonical XOR stream is not canonical.
    let mut w = crate::bytes::BitWriter::default();
    w.write(words[0], 64);
    for pair in words.windows(2) {
        let x = pair[0] ^ pair[1];
        if x == 0 {
            w.bit(false);
        } else {
            // Always a new window, never a reused one.
            let (l, t) = (x.leading_zeros().min(31), x.trailing_zeros());
            w.write(0b11, 2);
            w.write(u64::from(l), 5);
            w.write(u64::from(63 - l - t), 6);
            w.write(x >> t, 64 - l - t);
        }
    }
    let odd = w.finish();
    assert_ne!(odd, xor);
    let mut p = (odd.len() as u32).to_le_bytes().to_vec();
    p.extend_from_slice(&zstd::bulk::compress(&odd, 3).unwrap());
    let body_odd = with_value(&body, encoding::XOR_ZSTD, &p);
    assert_eq!(Block::decode(&body_odd).as_ref(), Ok(&b));
    assert_eq!(
        Block::decode_canonical(&body_odd),
        Err(DecodeError::NonCanonical)
    );
}

#[test]
fn f64_encodings_are_refused_for_other_value_types() {
    let vals: Vec<u64> = vec![5, 6];
    let mixed = plain_body(&[1, 3], &vals, &[]);
    for enc in [encoding::DECIMAL, encoding::XOR, encoding::XOR_ZSTD] {
        let payload = match enc {
            encoding::DECIMAL => crate::float::encode_decimal(&vals, 0),
            encoding::XOR => encode_xor(&vals),
            _ => encode_xor_zstd(&encode_xor(&vals)).unwrap(),
        };
        assert_eq!(
            Block::decode(&with_value(&mixed, enc, &payload)),
            Err(DecodeError::Encoding {
                column: column::VALUE
            }),
            "{enc}"
        );
        // Allowed when every row with a value is an f64.
        let f64s = plain_body(&[1, 1], &vals, &[]);
        assert!(
            Block::decode(&with_value(&f64s, enc, &payload)).is_ok(),
            "{enc}"
        );
    }
    // Not with no value word at all.
    let none = plain_body(
        &[1],
        &[],
        &[(column::FLAGS, flag::NO_VALUE.to_le_bytes().to_vec())],
    );
    assert!(Block::decode(&none).is_ok());
    assert_eq!(
        Block::decode(&with_value(&none, encoding::XOR, &[])),
        Err(DecodeError::Encoding {
            column: column::VALUE
        })
    );
}

#[test]
fn a_truncated_directory_is_a_directory_error() {
    let body = sample(4).encode().unwrap();
    assert_eq!(Block::decode(&body[..1]), Err(DecodeError::Directory));
    assert_eq!(Block::decode(&body[..2]), Err(DecodeError::Directory));
    assert_eq!(Block::decode(&body[..4]), Err(DecodeError::Directory));
}
