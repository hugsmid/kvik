//! The checks of `format.md` A.5 on facts bodies: every refusal, non-canonical bodies and what the encoder
//! refuses.

mod support;

use kvik_log::facts::{FactFlags, FactRow, FactsBlock, Quality, Value, ValueType};
use kvik_log::{BodyError, ContentError, Element, EncodeError};
use support::checks::*;
use support::*;

fn set(c: &mut [(u8, u8, Vec<u8>)], id: u8, enc: u8, payload: Vec<u8>) {
    let e = c.iter_mut().find(|e| e.0 == id).unwrap();
    e.1 = enc;
    e.2 = payload;
}

fn invalid(e: ContentError) -> Result<FactsBlock, BodyError> {
    Err(BodyError::Invalid(e))
}

#[test]
fn facts_checks() {
    use ContentError as E;
    assert!(with(2, |_| {}).is_ok());
    assert_eq!(
        FactsBlock::decode(&facts_raw(0, &plain_columns(0))),
        invalid(E::RowCount(0))
    );
    assert_eq!(
        FactsBlock::decode(&facts_raw(65_537, &[])),
        invalid(E::RowCount(65_537))
    );
    assert_eq!(FactsBlock::decode(&[0; 23]), invalid(E::Prefix));
    let mut b = facts_raw(2, &plain_columns(2));
    b[21] = 11;
    assert_eq!(FactsBlock::decode(&b), invalid(E::DirEntrySize(11)));
    let mut b = facts_raw(2, &plain_columns(2));
    b[20] = 200;
    assert_eq!(FactsBlock::decode(&b), invalid(E::Directory));
    for id in [1, 2, 5, 6] {
        assert_eq!(
            with(2, |c| c.retain(|e| e.0 != id)),
            invalid(E::MissingColumn { column: id }),
            "{id}"
        );
    }
    assert_eq!(
        with(2, |c| c.insert(1, (1, 0, vec![0; 8]))),
        invalid(E::ColumnOrder { column: 1 })
    );
    assert_eq!(
        with(2, |c| c.swap(0, 1)),
        invalid(E::ColumnOrder { column: 1 })
    );
    assert_eq!(
        with(2, |c| c.push((0x10, 0, vec![]))),
        Err(BodyError::Unsupported(Element::Column(0x10)))
    );
    assert!(with(2, |c| c.push((0x90, 0, vec![1, 2, 3]))).is_ok());
    assert_eq!(
        with(2, |c| set(c, 1, 1, vec![2, 0, 0, 0, 0])),
        invalid(E::Encoding { column: 1 })
    );
    assert_eq!(
        with(2, |c| set(c, 5, 2, vec![1, 1])),
        invalid(E::Encoding { column: 5 })
    );
    assert_eq!(
        with(2, |c| set(c, 2, 0, vec![0; 15])),
        invalid(E::Length { column: 2 })
    );
    assert_eq!(
        with(3, |c| set(c, 5, 1, vec![2, 1, 1, 1])),
        invalid(E::RunLength { column: 5 })
    );
    assert!(with(3, |c| set(c, 5, 1, vec![3, 1])).is_ok());
    assert_eq!(
        with(2, |c| insert(c, 3, 0, vec![0, 4])),
        invalid(E::Value { column: 3, row: 1 })
    );
    assert_eq!(
        with(2, |c| set(c, 5, 0, vec![1, 6])),
        invalid(E::Value { column: 5, row: 1 })
    );
    assert_eq!(
        with(2, |c| set(c, 5, 0, vec![0, 1])),
        invalid(E::Value { column: 5, row: 0 })
    );
    assert_eq!(
        with(2, |c| insert(c, 4, 0, vec![0, 0, 0, 1])),
        Err(BodyError::Unsupported(Element::FactFlags(0x0100)))
    );
    // Bool and state values with non-zero upper bytes; a no-value row with a value.
    let v = |x: u64| {
        [0u64, x]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<u8>>()
    };
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![4, 4]);
            set(c, 6, 0, v(2))
        }),
        invalid(E::Value { column: 6, row: 1 })
    );
    assert!(
        with(2, |c| {
            set(c, 5, 0, vec![3, 3]);
            set(c, 6, 0, v(65_535))
        })
        .is_ok()
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![3, 3]);
            set(c, 6, 0, v(65_536))
        }),
        invalid(E::Value { column: 6, row: 1 })
    );
    assert_eq!(
        with(2, |c| {
            insert(c, 4, 0, vec![0, 0, 0x20, 0]);
            set(c, 6, 0, v(5))
        }),
        invalid(E::Value { column: 6, row: 1 })
    );
    // Text: heap presence, bounds, UTF-8, length.
    let text = |off: u64, len: u64| v(off | len << 32);
    assert_eq!(
        with(2, |c| set(c, 5, 0, vec![5, 5])),
        invalid(E::Presence { column: 7 })
    );
    assert_eq!(
        with(2, |c| insert(c, 7, 0, vec![])),
        invalid(E::Presence { column: 7 })
    );
    assert!(
        with(2, |c| {
            set(c, 5, 0, vec![1, 5]);
            set(c, 6, 0, text(0, 2));
            insert(c, 7, 0, b"hi".to_vec())
        })
        .is_ok()
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![1, 5]);
            set(c, 6, 0, text(1, 2));
            insert(c, 7, 0, b"hi".to_vec())
        }),
        invalid(E::Text { row: 1 })
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![1, 5]);
            set(c, 6, 0, text(0, 2));
            insert(c, 7, 0, vec![0xC3, 0x28])
        }),
        invalid(E::Text { row: 1 })
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![1, 5]);
            set(c, 6, 0, text(0, 256));
            insert(c, 7, 0, vec![b'a'; 256])
        }),
        invalid(E::Text { row: 1 })
    );
    // changed_at: column 8 exactly when a row has flag bit 6, zero on the others.
    assert_eq!(
        with(2, |c| insert(c, 8, 0, vec![0; 16])),
        invalid(E::Presence { column: 8 })
    );
    assert_eq!(
        with(2, |c| insert(c, 4, 0, vec![0x40, 0, 0, 0])),
        invalid(E::Presence { column: 8 })
    );
    assert_eq!(
        with(2, |c| {
            insert(c, 4, 0, vec![0x40, 0, 0, 0]);
            insert(
                c,
                8,
                0,
                [9u8; 8].into_iter().chain(1u64.to_le_bytes()).collect(),
            )
        }),
        invalid(E::Presence { column: 8 })
    );
    assert!(
        with(2, |c| {
            insert(c, 4, 0, vec![0x40, 0, 0, 0]);
            insert(c, 8, 0, vec![9; 8].into_iter().chain([0; 8]).collect())
        })
        .is_ok()
    );
    // scale: column 9 exactly when a row is decimal, 0 to 18, zero on the others.
    assert_eq!(
        with(2, |c| set(c, 5, 0, vec![2, 1])),
        invalid(E::Presence { column: 9 })
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![2, 1]);
            insert(c, 9, 0, vec![19, 0])
        }),
        invalid(E::Value { column: 9, row: 0 })
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![2, 1]);
            insert(c, 9, 0, vec![4, 4])
        }),
        invalid(E::Presence { column: 9 })
    );
    assert!(
        with(2, |c| {
            set(c, 5, 0, vec![2, 1]);
            insert(c, 9, 0, vec![18, 0])
        })
        .is_ok()
    );
    // A decimal row holding `i64::MIN` is out of range (`spec.md` §2.1); `i64::MIN + 1` is not.
    for (units, ok) in [(i64::MIN, false), (i64::MIN + 1, true), (i64::MAX, true)] {
        let r = with(1, |c| {
            set(c, 5, 0, vec![2]);
            set(c, 6, 0, units.to_le_bytes().to_vec());
            insert(c, 9, 0, vec![4])
        });
        if ok {
            assert!(r.is_ok(), "{units}");
        } else {
            assert_eq!(r, invalid(E::Value { column: 6, row: 0 }));
        }
    }
    // Payloads outside the body, over the directory, or overlapping.
    let mut b = facts_raw(2, &plain_columns(2));
    b[24 + 4..24 + 8].copy_from_slice(&10_000u32.to_le_bytes());
    assert_eq!(
        FactsBlock::decode(&b),
        invalid(E::PayloadOutside { column: 1 })
    );
    let mut b = facts_raw(2, &plain_columns(2));
    b[24 + 4..24 + 8].copy_from_slice(&30u32.to_le_bytes());
    assert_eq!(
        FactsBlock::decode(&b),
        invalid(E::PayloadOutside { column: 1 })
    );
    let mut b = facts_raw(2, &plain_columns(2));
    let t_offset = u32::from_le_bytes(b[24 + 12 + 4..24 + 12 + 8].try_into().unwrap());
    b[24 + 4..24 + 8].copy_from_slice(&(t_offset - 4).to_le_bytes());
    assert_eq!(FactsBlock::decode(&b), invalid(E::Overlap { column: 2 }));
}

#[test]
fn non_canonical_facts_bodies_pass_the_checks_but_not_the_canonical_decoder() {
    let canonical = facts_raw(3, &plain_columns(3));
    // value_type of three equal rows is canonical in run-length, not plain.
    assert!(FactsBlock::decode(&canonical).is_ok());
    assert_eq!(
        FactsBlock::decode_canonical(&canonical),
        Err(BodyError::NonCanonical)
    );
    let mut c = plain_columns(3);
    set(&mut c, 5, 1, vec![3, 1]);
    let good = facts_raw(3, &c);
    assert!(FactsBlock::decode_canonical(&good).is_ok());
    // Non-zero padding between payloads; bytes after the last payload; an all-good quality column; an ignorable column.
    let mut b = good.clone();
    let pad_at = 24 + 12 * 4 + 12;
    b[pad_at] = 1;
    assert!(FactsBlock::decode(&b).is_ok());
    assert_eq!(
        FactsBlock::decode_canonical(&b),
        Err(BodyError::NonCanonical)
    );
    let mut b = good.clone();
    b.push(0);
    assert_eq!(
        FactsBlock::decode_canonical(&b),
        Err(BodyError::NonCanonical)
    );
    let mut c2 = c.clone();
    insert(&mut c2, 3, 0, vec![0; 3]);
    assert_eq!(
        FactsBlock::decode_canonical(&facts_raw(3, &c2)),
        Err(BodyError::NonCanonical)
    );
    let mut c3 = c.clone();
    c3.push((0x90, 0, vec![]));
    assert_eq!(
        FactsBlock::decode_canonical(&facts_raw(3, &c3)),
        Err(BodyError::NonCanonical)
    );
}

#[test]
fn facts_encode_refuses_what_it_cannot_write() {
    let row = |value| FactRow {
        series_id: 1,
        t: 0,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value,
        changed_at: None,
    };
    let block = |rows| FactsBlock {
        first_sequence: 1,
        received_at: 0,
        rows,
    };
    assert_eq!(
        block(vec![]).encode(),
        Err(EncodeError::Content(ContentError::RowCount(0)))
    );
    assert_eq!(
        block(vec![row(Value::Bool(true)); 65_537]).encode(),
        Err(EncodeError::Content(ContentError::RowCount(65_537)))
    );
    assert!(block(vec![row(Value::Bool(true)); 65_536]).encode().is_ok());
    assert_eq!(
        block(vec![row(Value::Text("x".repeat(256)))]).encode(),
        Err(EncodeError::Content(ContentError::Text { row: 0 }))
    );
    assert!(
        block(vec![row(Value::Text("x".repeat(255)))])
            .encode()
            .is_ok()
    );
    assert_eq!(
        block(vec![row(Value::Decimal {
            units: 1,
            scale: 19
        })])
        .encode(),
        Err(EncodeError::Content(ContentError::Value {
            column: 9,
            row: 0
        }))
    );
    assert_eq!(
        block(vec![row(Value::Decimal {
            units: i64::MIN,
            scale: 4
        })])
        .encode(),
        Err(EncodeError::Content(ContentError::Value {
            column: 6,
            row: 0
        }))
    );
    assert_eq!(
        block(vec![row(Value::Absent {
            value_type: ValueType::F64,
            scale: 1
        })])
        .encode(),
        Err(EncodeError::Content(ContentError::Value {
            column: 9,
            row: 0
        }))
    );
}

#[test]
fn a_batch_whose_facts_body_fails_its_checks_is_invalid_content_whether_or_not_content_is_kept() {
    use kvik_log::block::block_type;
    use kvik_log::scan::{Found, LossCause, ScanLimit, Scanner};
    let body = |first: u64, c: &[(u8, u8, Vec<u8>)], rows: u32| {
        let mut b = facts_raw(rows, c);
        b[..8].copy_from_slice(&first.to_le_bytes());
        b
    };
    let mut bad = plain_columns(2);
    set(&mut bad, 5, 0, vec![1, 6]);
    let mut f = vec![0u8; kvik_log::header::HEADER_LEN];
    let bad_at = f.len() as u64;
    f.extend(raw_batch(
        &ID,
        bad_at,
        &[(block_type::FACTS, 0, body(1, &bad, 2))],
        1,
        2,
        0,
    ));
    let good_at = f.len() as u64;
    let good = (block_type::FACTS, 0, body(3, &plain_columns(1), 1));
    f.extend(raw_batch(&ID, good_at, &[good], 3, 3, 0));
    f.resize(f.len() + 64, 0);
    for keep in [false, true] {
        let s = Scanner::new(&f, &ID, ScanLimit::End).unwrap();
        let s = if keep { s.keeping_content() } else { s };
        let found: Vec<_> = s.collect();
        assert!(
            matches!(&found[..], [Ok(Found::Region(r)), Ok(Found::Batch(b))]
                if r.start == bad_at && r.cause == LossCause::InvalidContent && b.offset == good_at),
            "keep {keep}: {found:?}"
        );
    }
}
