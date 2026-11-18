//! Scans that keep the content their checks decode, against scans that do not (Q-990).

use super::*;

#[test]
fn a_recovery_entry_not_alone_is_passed_over_whether_or_not_content_is_kept() {
    let (mut bytes, id) = chain(0);
    bytes.truncate(64);
    let batch = |first: u64, at: u64, entries: Vec<crate::entries::Entry>| {
        let body = crate::entries::EntriesBlock {
            first_sequence: first,
            received_at: 0,
            entries,
        };
        let mut builder = crate::batch::BatchBuilder::new();
        builder
            .push(block_type::ENTRIES, body.encode().unwrap())
            .unwrap();
        builder.encode(&id, at, 0).unwrap()
    };
    let alias = || crate::entries::Entry::Alias {
        id: 1,
        name: "a".into(),
    };
    let recovery = crate::entries::Entry::Recovery(crate::entries::Recovery {
        after: 0,
        stamp: None,
        boot_id: [0; 16],
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: crate::entries::MarkState::Readable,
        id_floor: 0,
        consumers: Vec::new(),
    });
    bytes.extend_from_slice(&batch(1, 64, vec![recovery, alias()]));
    let at = bytes.len() as u64;
    bytes.extend_from_slice(&batch(3, at, vec![alias()]));
    let limit = ScanLimit::Mark(bytes.len() as u64);
    let plain: Vec<_> = Scanner::new(&bytes, &id, limit).unwrap().collect();
    let kept: Vec<_> = Scanner::new(&bytes, &id, limit)
        .unwrap()
        .keeping_content()
        .collect();
    assert_eq!(plain, kept);
    assert!(
        matches!(&plain[..], [Ok(Found::Region(r)), Ok(Found::Batch(b))] if r.cause == LossCause::InvalidContent && b.offset == at)
    );
}

#[test]
fn batches_kept_after_damage_keep_their_content_with_each_body_checked_once() {
    let (mut bytes, id) = chain(0);
    bytes.truncate(HEADER_LEN);
    bytes.extend_from_slice(&[0xFF; 8]);
    let k = 50u64;
    let mut bodies = 0u64;
    for j in 0..k {
        let at = bytes.len() as u64;
        let batch = alias_batch(&id, j + 1, at);
        bodies += (batch.len() - 2 * BLOCK_HEADER_LEN - crate::body::COMMIT_LEN) as u64;
        bytes.extend_from_slice(&batch);
    }
    bytes.resize(bytes.len() + 64, 0);
    let limit = ScanLimit::End;
    let mut plain = Scanner::new(&bytes, &id, limit).unwrap();
    let found: Vec<_> = plain.by_ref().collect();
    let mut kept = Scanner::new(&bytes, &id, limit).unwrap().keeping_content();
    let mut batches = 0;
    let mut again = Vec::new();
    while let Some(f) = kept.next() {
        if let Ok(Found::Batch(b)) = &f {
            assert_eq!(kept.take_content(), Some(BatchContent::decode(b).unwrap()));
            batches += 1;
        }
        again.push(f);
    }
    assert_eq!(found, again);
    assert_eq!(batches, k);
    assert_eq!(kept.stats(), plain.stats());
    assert_eq!(kept.stats().bytes_body_checked, bodies);
    assert!(kept.stats().within_bound(bytes.len() as u64));
}

/// A facts batch (A.5) of one boolean reading at sequence `seq`, file offset `at`, committed under `id`.
fn facts_batch(id: &SegmentIdentity, seq: u64, at: u64) -> Vec<u8> {
    let body = crate::facts::FactsBlock {
        first_sequence: seq,
        received_at: 0,
        rows: vec![crate::facts::FactRow {
            series_id: 1,
            t: 0,
            quality: crate::facts::Quality::default(),
            flags: crate::facts::FactFlags::default(),
            value: crate::facts::Value::Bool(true),
            changed_at: None,
        }],
    };
    let mut builder = crate::batch::BatchBuilder::new();
    builder
        .push(block_type::FACTS, body.encode().unwrap())
        .unwrap();
    builder.encode(id, at, 0).unwrap()
}

#[test]
fn a_boundary_read_that_keeps_no_batch_decodes_no_body_for_its_content() {
    let (mut bytes, id) = chain(0);
    bytes.truncate(HEADER_LEN);
    bytes.extend_from_slice(&[0xFF; 8]);
    let other = SegmentIdentity {
        generation: 1,
        ..id
    };
    // Found by resynchronisation, refused by its commit's generation at a boundary, found again, then read at a
    // boundary and kept.
    for (seq, of) in [(1, &id), (2, &other), (3, &id), (4, &id)] {
        let at = bytes.len() as u64;
        bytes.extend_from_slice(&facts_batch(of, seq, at));
    }
    bytes.resize(bytes.len() + 64, 0);
    let r = scan(&bytes, &id, ScanLimit::End).unwrap();
    let kept: Vec<_> = r.batches.iter().map(|b| b.commit.first_sequence).collect();
    assert_eq!(kept, [1, 3, 4]);
    crate::facts::FACTS_DECODES.with(|c| c.set(0));
    let (again, contents) = scan_to_end(&bytes, &id, nonzero_end(&bytes)).unwrap();
    assert_eq!(again, r);
    assert!(contents.iter().all(Option::is_some));
    assert_eq!(crate::facts::FACTS_DECODES.with(|c| c.get()), 3);
}

#[test]
fn a_candidate_a_failed_boundary_read_walked_keeps_its_content() {
    let (mut bytes, id) = chain(0);
    bytes.truncate(HEADER_LEN);
    bytes.extend_from_slice(&[0xFF; 8]);
    let at = bytes.len() as u64;
    bytes.extend_from_slice(&facts_batch(&id, 1, at));
    // A boundary read after the first batch walks the skippable block and the second batch to its commit, which does
    // not name the block's offset; resynchronisation then takes the second batch from that walk (Q-873).
    encode_block(0x50, FLAG_SKIPPABLE, &[], &mut bytes).unwrap();
    let at = bytes.len() as u64;
    bytes.extend_from_slice(&facts_batch(&id, 2, at));
    bytes.resize(bytes.len() + 64, 0);
    let mut plain = Scanner::new(&bytes, &id, ScanLimit::End).unwrap();
    let found: Vec<_> = plain.by_ref().collect();
    let mut kept = Scanner::new(&bytes, &id, ScanLimit::End)
        .unwrap()
        .keeping_content();
    let mut again = Vec::new();
    let mut contents = Vec::new();
    while let Some(f) = kept.next() {
        if let Ok(Found::Batch(b)) = &f {
            assert_eq!(kept.take_content(), Some(BatchContent::decode(b).unwrap()));
            contents.push(b.commit.first_sequence);
        }
        again.push(f);
    }
    assert_eq!(found, again);
    assert_eq!(contents, [1, 2]);
    assert_eq!(kept.stats(), plain.stats());
}
