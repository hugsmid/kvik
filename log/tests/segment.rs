//! Reading log segments: complete batches, the mark, invalid regions, the tail, the seal and version refusals.

mod support;

use kvik_log::batch::{BatchBuilder, SegmentIdentity};
use kvik_log::block::{FLAG_SKIPPABLE, block_type};
use kvik_log::body::{INDEX_HAS_ENTRIES, IndexRecord};
use kvik_log::header::FileHeader;
use kvik_log::scan::{HeaderState, InvalidRegion, LossCause, ScanLimit, SealState, Tail, scan};
use kvik_log::segment::{OpenAction, OpenState, SegmentImage, classify_open};
use kvik_log::{Element, FormatError};
use support::*;

fn ends(bytes: &[u8], offsets: &[u64]) -> Vec<u64> {
    let r = scan(bytes, &ID, ScanLimit::End).unwrap();
    assert_eq!(
        r.batches.iter().map(|b| b.offset).collect::<Vec<_>>(),
        offsets
    );
    r.batches.iter().map(|b| b.end()).collect()
}

#[test]
fn a_clean_segment_reads_every_batch() {
    let (s, offsets) = segment(ID, 2048, 1, &three_batches());
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert!(matches!(r.header, HeaderState::Valid(h) if h.base_sequence == 1));
    let seqs: Vec<_> = r
        .batches
        .iter()
        .map(|b| (b.commit.first_sequence, b.commit.last_sequence))
        .collect();
    assert_eq!(seqs, [(1, 1), (2, 3), (4, 5)]);
    assert_eq!(
        r.batches.iter().map(|b| b.offset).collect::<Vec<_>>(),
        offsets
    );
    assert_eq!(r.batches[2].blocks.len(), 2);
    assert!(r.regions.is_empty());
    assert_eq!(
        r.tail,
        Some(Tail::Clean {
            offset: s.write_offset()
        })
    );
    assert_eq!(r.seal, None);
    assert!(r.in_use());
}

#[test]
fn the_batch_encoder_follows_the_tables() {
    // The crate's encoder against one written straight from A.2 and A.7.
    for (batch, t) in three_batches() {
        for offset in [64u64, 1024, 1 << 40] {
            let blocks: Vec<_> = batch_blocks(&batch);
            let (first, last) = batch.sequences().unwrap();
            assert_eq!(
                batch.encode(&ID, offset, t).unwrap(),
                raw_batch(&ID, offset, &blocks, first, last, t)
            );
        }
    }
    // The commit header sits at `o + batch_bytes`; the batch checksum covers the offset.
    let (s, offsets) = segment(ID, 2048, 1, &three_batches());
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    for (b, o) in r.batches.iter().zip(offsets) {
        let at = (o + b.commit.batch_bytes) as usize;
        assert_eq!(s.bytes()[at], 0x0F);
        assert_eq!(
            &s.bytes()[at + 16 + 32..at + 16 + 40],
            &b.commit.batch_bytes.to_le_bytes()
        );
    }
}

fn batch_blocks(batch: &BatchBuilder) -> Vec<(u8, u8, Vec<u8>)> {
    // Decode the builder's own output to get its blocks back.
    let bytes = batch.encode(&ID, 64, 0).unwrap();
    let mut image = vec![0u8; 64];
    image.extend_from_slice(&bytes);
    let r = scan(&image, &ID, ScanLimit::End).unwrap();
    r.batches[0]
        .blocks
        .iter()
        .map(|b| {
            (
                b.block.header.block_type,
                b.block.header.flags,
                b.block.body.to_vec(),
            )
        })
        .collect()
}

#[test]
fn reserved_header_bytes_are_not_tested() {
    // A later minor version may use the reserved bytes of a block header; `batch_crc32c` covers them as written.
    let blocks = vec![(block_type::FACTS, 0, facts_body(1, T0, &[(1, T0, 1.0)]))];
    let mut s = SegmentImage::create_ahead(ID, T0, 1024).unwrap();
    s.take_into_use(1).unwrap();
    let batch = raw_batch_reserved(&ID, 64, &blocks, 1, 1, T0, 0xA55A);
    put(s.bytes_mut(), 64, &batch);
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.batches.len(), 1);
    assert_eq!(
        r.batches[0].blocks[0].block.header_bytes[2..4],
        [0x5A, 0xA5]
    );
    assert_eq!(
        r.tail,
        Some(Tail::Clean {
            offset: 64 + batch.len() as u64
        })
    );
}

#[test]
fn a_reader_reads_only_below_the_mark() {
    let (s, offsets) = segment(ID, 2048, 1, &three_batches());
    let e = ends(s.bytes(), &offsets);
    for (k, &mark) in [64].iter().chain(&e).enumerate() {
        let r = scan(s.bytes(), &ID, ScanLimit::Mark(mark)).unwrap();
        assert_eq!(r.batches.len(), k);
        assert!(r.regions.is_empty() && r.tail.is_none() && r.seal.is_none());
    }
    // A mark inside a batch: the batch is above the mark and never accepted; below the mark, what is not a complete
    // batch is corruption.
    let r = scan(s.bytes(), &ID, ScanLimit::Mark(e[1] + 8)).unwrap();
    assert_eq!(r.batches.len(), 2);
    assert_eq!(
        r.regions,
        [InvalidRegion {
            start: e[1],
            end: e[1] + 8,
            last_sequence_before: Some(3),
            first_sequence_after: None,
            cause: LossCause::Corruption,
        }]
    );
    // A mark beyond the end of the file.
    let r = scan(&s.bytes()[..e[2] as usize], &ID, ScanLimit::Mark(e[2] + 64)).unwrap();
    assert_eq!(r.batches.len(), 3);
    assert_eq!(r.regions.len(), 1);
}

#[test]
fn corruption_in_the_middle_keeps_the_batches_after_it() {
    let (mut s, offsets) = segment(ID, 2048, 1, &three_batches());
    let at = offsets[1] as usize + 40;
    s.bytes_mut()[at] ^= 0x10;
    for limit in [ScanLimit::End, ScanLimit::Mark(s.write_offset())] {
        let r = scan(s.bytes(), &ID, limit).unwrap();
        assert_eq!(
            r.batches.iter().map(|b| b.offset).collect::<Vec<_>>(),
            [offsets[0], offsets[2]]
        );
        assert_eq!(
            r.regions,
            [InvalidRegion {
                start: offsets[1],
                end: offsets[2],
                last_sequence_before: Some(1),
                first_sequence_after: Some(4),
                cause: LossCause::Corruption,
            }]
        );
    }
}

#[test]
fn a_structural_failure_in_a_checksum_valid_block_is_invalid_content() {
    let (mut s, offsets) = segment(ID, 2048, 1, &three_batches());
    // Batch 2 again, with `row_count` 0 and every checksum recomputed.
    let mut body = facts_body(2, T0, &[(1, T0, 1.0)]);
    body[16..20].copy_from_slice(&0u32.to_le_bytes());
    let bad = raw_batch(&ID, offsets[1], &[(block_type::FACTS, 0, body)], 2, 1, T0);
    assert!(bad.len() as u64 <= offsets[2] - offsets[1]);
    let mut fill = bad.clone();
    fill.resize((offsets[2] - offsets[1]) as usize, 0);
    put(s.bytes_mut(), offsets[1], &fill);
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.batches.len(), 2);
    assert_eq!(r.regions[0].cause, LossCause::InvalidContent);
    assert_eq!(
        (r.regions[0].start, r.regions[0].end),
        (offsets[1], offsets[2])
    );
}

#[test]
fn a_torn_tail_is_reported_with_its_readable_commits() {
    let (s, offsets) = segment(ID, 2048, 1, &three_batches());
    let e = ends(s.bytes(), &offsets);
    // The third batch's first block survives, its commit does not.
    let mut torn = s.clone();
    torn.zero_fill_from(offsets[2] + 64).unwrap();
    let r = scan(torn.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.batches.len(), 2);
    assert!(r.regions.is_empty());
    let Some(Tail::Torn {
        start,
        end,
        cause,
        commits,
    }) = r.tail
    else {
        panic!("{:?}", r.tail)
    };
    assert_eq!((start, cause), (offsets[2], LossCause::Corruption));
    assert!(end > start && end <= offsets[2] + 64);
    assert!(commits.is_empty());
    // The first block of the third batch is lost but its commit survives: one readable commit block.
    let mut torn = s.clone();
    torn.bytes_mut()[offsets[2] as usize + 20] ^= 1;
    let r = scan(torn.bytes(), &ID, ScanLimit::End).unwrap();
    let Some(Tail::Torn { end, commits, .. }) = r.tail else {
        panic!()
    };
    let last_non_zero = torn.bytes().iter().rposition(|&x| x != 0).unwrap() as u64;
    assert_eq!(end, last_non_zero + 1);
    assert!(end > e[2] - 8 - 8 && end <= e[2]);
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].offset, e[2] - 64);
    assert_eq!(commits[0].commit.first_sequence, 4);
}

#[test]
fn a_gap_between_batches_is_accepted() {
    let r0 = T0;
    let batches = vec![
        (facts_batch(1, r0, &[(1, T0, 1.0)]), r0),
        (facts_batch(10, r0, &[(1, T0, 2.0)]), r0),
    ];
    let (s, _) = segment(ID, 1024, 1, &batches);
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.batches.len(), 2);
    assert_eq!(r.batches[1].commit.first_sequence, 10);
    assert!(r.regions.is_empty());
}

#[test]
fn sequences_must_rise() {
    // A batch whose first sequence is not above the previous batch's last, or below `base_sequence`, is never
    // accepted, although its checksums hold.
    let mut s = SegmentImage::create_ahead(ID, T0, 1024).unwrap();
    s.take_into_use(5).unwrap();
    let at = s.write_offset();
    put(
        s.bytes_mut(),
        at,
        &facts_batch(4, T0, &[(1, T0, 1.0)])
            .encode(&ID, at, T0)
            .unwrap(),
    );
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert!(r.batches.is_empty());
    assert!(matches!(r.tail, Some(Tail::Torn { .. })));
    let (mut s, offsets) = segment(
        ID,
        1024,
        1,
        &[(facts_batch(1, T0, &[(1, T0, 1.0), (1, T0 + 1, 2.0)]), T0)],
    );
    let at = s.write_offset();
    put(
        s.bytes_mut(),
        at,
        &facts_batch(2, T0, &[(1, T0, 3.0)])
            .encode(&ID, at, T0)
            .unwrap(),
    );
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(
        r.batches.iter().map(|b| b.offset).collect::<Vec<_>>(),
        offsets
    );
    // A recovery batch may start higher than `base_sequence` (Q-105 (d)).
    let mut s = SegmentImage::create_ahead(ID, T0, 1024).unwrap();
    s.take_into_use(5).unwrap();
    let at = s.write_offset();
    put(
        s.bytes_mut(),
        at,
        &entries_batch(9, T0, &[alias(1, "a")])
            .encode(&ID, at, T0)
            .unwrap(),
    );
    assert_eq!(
        scan(s.bytes(), &ID, ScanLimit::End).unwrap().batches.len(),
        1
    );
}

#[test]
fn a_batch_copied_to_another_offset_or_file_is_not_accepted() {
    let (s, offsets) = segment(ID, 2048, 1, &three_batches());
    let e = ends(s.bytes(), &offsets);
    let second = s.bytes()[offsets[1] as usize..e[1] as usize].to_vec();
    // The same bytes 8 bytes later, after the first batch: `batch_crc32c` covers the offset.
    let mut moved = SegmentImage::create_ahead(ID, T0, 2048).unwrap();
    moved.take_into_use(1).unwrap();
    put(moved.bytes_mut(), 64, &s.bytes()[64..e[0] as usize]);
    put(moved.bytes_mut(), e[0] + 8, &second);
    let r = scan(moved.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.batches.len(), 1);
    assert!(matches!(r.tail, Some(Tail::Torn { .. })));
    // The same file read as another file id or generation: no batch is complete.
    let other = SegmentIdentity { file_id: 2, ..ID };
    let mut bytes = s.bytes().to_vec();
    bytes[..64].fill(0);
    assert!(
        scan(&bytes, &other, ScanLimit::End)
            .unwrap()
            .batches
            .is_empty()
    );
    let other = SegmentIdentity {
        generation: 1,
        ..ID
    };
    assert!(
        scan(&bytes, &other, ScanLimit::End)
            .unwrap()
            .batches
            .is_empty()
    );
}

#[test]
fn an_invalid_header_does_not_hide_the_batches() {
    let (mut s, offsets) = segment(ID, 2048, 1, &three_batches());
    s.bytes_mut()[3] = b'X';
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.header, HeaderState::Invalid);
    assert_eq!(r.batches.len(), offsets.len());
    assert_eq!(
        classify_open(&r, true, 2048, 2048),
        OpenAction {
            rewrite_header: true,
            state: OpenState::LastInUse
        }
    );
}

#[test]
fn headers_that_do_not_belong_are_refused() {
    let (s, _) = segment(ID, 1024, 1, &three_batches()[..1]);
    let with = |f: &dyn Fn(&mut FileHeader)| {
        let mut h = FileHeader::decode(s.bytes()).unwrap();
        f(&mut h);
        let mut b = s.bytes().to_vec();
        b[..64].copy_from_slice(&h.encode());
        scan(&b, &ID, ScanLimit::End).map(|r| r.batches.len())
    };
    assert_eq!(with(&|_| {}), Ok(1));
    assert_eq!(
        with(&|h| h.format_major = 2),
        Err(FormatError::Unsupported {
            offset: 0,
            element: Element::FormatMajor(2)
        })
    );
    // A newer minor version is read: compatibility is decided per element.
    assert_eq!(with(&|h| h.format_minor = 7), Ok(1));
    assert_eq!(
        with(&|h| h.kind = 2),
        Err(FormatError::WrongKind {
            found: 2,
            expected: 1
        })
    );
    assert_eq!(
        with(&|h| h.store_id[0] ^= 1),
        Err(FormatError::StoreIdMismatch)
    );
    assert_eq!(
        with(&|h| h.file_id = 9),
        Err(FormatError::HeaderMismatch {
            field: "file_id",
            in_header: 9,
            expected: 1
        })
    );
    assert!(matches!(
        with(&|h| h.generation = 3),
        Err(FormatError::HeaderMismatch {
            field: "generation",
            ..
        })
    ));
}

#[test]
fn an_unknown_skippable_block_is_skipped_and_counted() {
    let mut b = facts_batch(1, T0, &[(1, T0, 1.0)]);
    b.push_unknown(0x50, FLAG_SKIPPABLE, vec![0xAB; 5]).unwrap();
    b.push(block_type::FACTS, facts_body(2, T0, &[(1, T0 + 1, 2.0)]))
        .unwrap();
    let (s, _) = segment(ID, 1024, 1, &[(b, T0)]);
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.batches.len(), 1);
    let batch = &r.batches[0];
    assert_eq!(batch.commit.block_count, 3);
    assert_eq!(
        (batch.commit.first_sequence, batch.commit.last_sequence),
        (1, 2)
    );
    assert_eq!(batch.blocks[1].prefix, None);
    assert_eq!(batch.blocks[1].block.body, &[0xAB; 5]);
}

#[test]
fn an_unknown_skippable_block_between_batches_is_an_invalid_region() {
    // Q-105 (b): with no commit block of its own it is taken as the start of the next batch, which then fails.
    let (mut s, offsets) = segment(ID, 2048, 1, &three_batches()[..1]);
    let at = s.write_offset();
    let mut stray = Vec::new();
    kvik_log::block::encode_block(0x50, FLAG_SKIPPABLE, b"stray", &mut stray).unwrap();
    put(s.bytes_mut(), at, &stray);
    let next = at + stray.len() as u64;
    put(
        s.bytes_mut(),
        next,
        &facts_batch(2, T0, &[(1, T0, 1.0)])
            .encode(&ID, next, T0)
            .unwrap(),
    );
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(
        r.batches.iter().map(|b| b.offset).collect::<Vec<_>>(),
        [offsets[0], next]
    );
    assert_eq!(
        (r.regions[0].start, r.regions[0].end, r.regions[0].cause),
        (at, next, LossCause::Corruption)
    );
}

#[test]
fn an_unknown_critical_block_is_a_version_refusal() {
    let mut b = facts_batch(2, T0, &[(1, T0, 1.0)]);
    b.push_unknown(0x51, 0, vec![1, 2, 3]).unwrap();
    let batches = vec![three_batches().remove(0), (b, T0)];
    let (s, offsets) = segment(ID, 1024, 1, &batches);
    let unknown_at =
        offsets[1] + 16 + facts_body(2, T0, &[(1, T0, 1.0)]).len().div_ceil(8) as u64 * 8;
    let refused = Err(FormatError::Unsupported {
        offset: unknown_at,
        element: Element::BlockType(0x51),
    });
    for limit in [ScanLimit::End, ScanLimit::Mark(s.write_offset())] {
        assert_eq!(
            scan(s.bytes(), &ID, limit).map(|r| r.batches.len()),
            refused
        );
    }
    // Above the mark it is never looked at.
    assert_eq!(
        scan(s.bytes(), &ID, ScanLimit::Mark(offsets[1])).map(|r| r.batches.len()),
        Ok(1)
    );
    // Found while resynchronising after damage: refused only when its batch checks out (Q-105 (c)).
    let mut damaged = s.clone();
    damaged.bytes_mut()[70] ^= 1;
    assert_eq!(
        scan(damaged.bytes(), &ID, ScanLimit::End).map(|r| r.batches.len()),
        refused
    );
    let mut damaged = s.clone();
    damaged.bytes_mut()[70] ^= 1;
    let commit_crc_at = s.write_offset() as usize - 8;
    damaged.bytes_mut()[commit_crc_at] ^= 1;
    let r = scan(damaged.bytes(), &ID, ScanLimit::End).unwrap();
    assert!(r.batches.is_empty());
    assert!(matches!(r.tail, Some(Tail::Torn { .. })));
}

#[test]
fn zeros_below_the_mark_are_not_the_end_of_data() {
    // A.4's accepted limit: a power loss during a zero-fill leaves a zeroed hole with kept batches after it.
    let (mut s, offsets) = segment(ID, 2048, 1, &three_batches());
    let e = ends(s.bytes(), &offsets);
    put(
        s.bytes_mut(),
        offsets[1],
        &vec![0; (e[1] - offsets[1]) as usize],
    );
    for limit in [ScanLimit::End, ScanLimit::Mark(e[2])] {
        let r = scan(s.bytes(), &ID, limit).unwrap();
        assert_eq!(
            r.batches.iter().map(|b| b.offset).collect::<Vec<_>>(),
            [offsets[0], offsets[2]]
        );
        assert_eq!(
            (r.regions[0].start, r.regions[0].end),
            (offsets[1], offsets[2])
        );
    }
}

fn sealed(len_after: Option<u64>) -> (SegmentImage, u64) {
    let (mut s, _) = segment(ID, 2048, 1, &three_batches());
    let records = vec![IndexRecord {
        series_id: 1,
        fact_count: 4,
        min_t: T0,
        max_t: T0 + 30 * SECOND,
    }];
    let end = s.write_seal_blocks(records).unwrap();
    s.set_len(len_after.unwrap_or(end));
    (s, end)
}

#[test]
fn seal_states() {
    let (s, end) = sealed(None);
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    let seal = r.seal.as_ref().unwrap();
    assert_eq!((seal.state, seal.end), (SealState::Sealed, end));
    assert_eq!(r.batches.len(), 3);
    assert!(r.regions.is_empty() && r.tail.is_none());
    assert_eq!(seal.trailer.entry_count, 5);
    assert_eq!(
        (
            seal.trailer.first_sequence,
            seal.trailer.last_sequence,
            seal.trailer.batch_count
        ),
        (1, 5, 3)
    );
    let index = seal.index.clone().flatten().unwrap();
    assert_eq!(index.flags, INDEX_HAS_ENTRIES);
    assert!(seal.file_crc_matches(s.bytes()));
    assert_eq!(&s.bytes()[end as usize - 8..], b"KVIKEND\0");
    assert_eq!(
        classify_open(&r, false, end, 4096).state,
        OpenState::SealWritten {
            length_pending: false
        }
    );

    // A crash between seal steps 1 and 2: the file is still longer than the end marker.
    let (s, _) = sealed(Some(2048));
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.seal.as_ref().unwrap().state, SealState::LengthPending);
    assert_eq!(
        classify_open(&r, true, 2048, 2048).state,
        OpenState::SealWritten {
            length_pending: true
        }
    );

    // Seal blocks torn before the end marker: not sealed; what follows the last batch is seal blocks or damage.
    let (mut s, end) = sealed(Some(2048));
    s.zero_fill_from(end - 40).unwrap();
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.seal, None);
    assert_eq!(r.batches.len(), 3);
    assert!(matches!(r.tail, Some(Tail::Torn { .. })));
    assert_eq!(
        classify_open(&r, false, 2048, 2048).state,
        OpenState::AwaitingSeal
    );
    assert_eq!(
        classify_open(&r, true, 2048, 2048).state,
        OpenState::LastInUse
    );

    // Damage in padding or seal blocks: the batches are complete, `file_crc32c` does not match.
    let (mut s, _) = sealed(None);
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    let index_at = r.seal.unwrap().start as usize;
    s.bytes_mut()[index_at + 20] ^= 1;
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    let seal = r.seal.unwrap();
    assert_eq!(seal.index, Some(None));
    assert!(!seal.file_crc_matches(s.bytes()));
    assert_eq!(r.batches.len(), 3);
}

#[test]
fn unused_segments() {
    let s = SegmentImage::create_ahead(ID, T0, 4096).unwrap();
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert!(!r.in_use());
    assert_eq!(r.tail, Some(Tail::Clean { offset: 64 }));
    assert_eq!(
        classify_open(&r, false, 4096, 4096).state,
        OpenState::Unused { keep: true }
    );
    assert_eq!(
        classify_open(&r, false, 4096, 8192).state,
        OpenState::Unused { keep: false }
    );
    let mut b = s.into_bytes();
    b[0] = 0;
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    assert_eq!(
        classify_open(&r, false, 4096, 4096).state,
        OpenState::Unused { keep: false }
    );
    // A header with `base_sequence = 0` but a complete batch: in use, header rewritten.
    let mut s = SegmentImage::create_ahead(ID, T0, 1024).unwrap();
    put(
        s.bytes_mut(),
        64,
        &facts_batch(1, T0, &[(1, T0, 1.0)])
            .encode(&ID, 64, T0)
            .unwrap(),
    );
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(
        classify_open(&r, false, 1024, 1024),
        OpenAction {
            rewrite_header: true,
            state: OpenState::AwaitingSeal
        }
    );
}

#[test]
fn padding_between_blocks_is_not_checked() {
    let (mut s, offsets) = segment(ID, 2048, 1, &three_batches());
    let before = scan(s.bytes(), &ID, ScanLimit::End)
        .unwrap()
        .batches
        .iter()
        .map(|b| b.end())
        .collect::<Vec<_>>();
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    let blk = r.batches[1].blocks[0].block;
    let pad_from = blk.offset + 16 + u64::from(blk.header.body_len);
    let pad_to = blk.next_offset();
    let bodies: Vec<Vec<u8>> = r
        .batches
        .iter()
        .flat_map(|b| b.blocks.iter().map(|d| d.block.body.to_vec()))
        .collect();
    drop(r);
    if pad_from < pad_to {
        s.bytes_mut()[pad_from as usize] = 0xEE;
    }
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(
        r.batches.iter().map(|b| b.end()).collect::<Vec<_>>(),
        before
    );
    let after: Vec<Vec<u8>> = r
        .batches
        .iter()
        .flat_map(|b| b.blocks.iter().map(|d| d.block.body.to_vec()))
        .collect();
    assert_eq!(bodies, after);
    assert_eq!(offsets.len(), 3);
}

#[test]
fn a_batch_above_8_mib_of_data_blocks_or_without_a_sequence_is_not_complete() {
    // Two facts blocks of 4 MiB + 8 bytes each, with every checksum valid.
    let big = |first: u64| {
        let mut body = facts_body(first, T0, &[(1, T0, 1.0)]);
        body.resize(4 * 1024 * 1024 + 8, 0);
        body
    };
    let blocks = vec![
        (block_type::FACTS, 0, big(1)),
        (block_type::FACTS, 0, big(2)),
    ];
    let batch = raw_batch(&ID, 64, &blocks, 1, 2, T0);
    let mut s = SegmentImage::create_ahead(ID, T0, batch.len() as u64 + 128).unwrap();
    s.take_into_use(1).unwrap();
    put(s.bytes_mut(), 64, &batch);
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert!(r.batches.is_empty());
    // One block of the same size fits.
    let one = raw_batch(&ID, 64, &blocks[..1], 1, 1, T0);
    let mut s = SegmentImage::create_ahead(ID, T0, one.len() as u64 + 128).unwrap();
    s.take_into_use(1).unwrap();
    put(s.bytes_mut(), 64, &one);
    assert_eq!(
        scan(s.bytes(), &ID, ScanLimit::End).unwrap().batches.len(),
        1
    );
    // An entries block without entries, and a batch of skippable blocks only: no sequence taken, not complete.
    let empty = entries_body(1, T0, &[]);
    for blocks in [
        vec![(block_type::ENTRIES, 0, empty)],
        vec![(0x50, FLAG_SKIPPABLE, vec![1])],
    ] {
        let mut s = SegmentImage::create_ahead(ID, T0, 1024).unwrap();
        s.take_into_use(1).unwrap();
        put(s.bytes_mut(), 64, &raw_batch(&ID, 64, &blocks, 1, 0, T0));
        assert!(
            scan(s.bytes(), &ID, ScanLimit::End)
                .unwrap()
                .batches
                .is_empty()
        );
    }
    let mut b = BatchBuilder::new();
    assert!(
        b.push(block_type::ENTRIES, entries_body(1, T0, &[]))
            .is_err()
    );
    b.push_unknown(0x50, FLAG_SKIPPABLE, vec![]).unwrap();
    assert!(b.encode(&ID, 64, T0).is_err());
}
