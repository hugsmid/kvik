use super::*;
use crate::block::{BLOCK_READS, FLAG_SKIPPABLE, encode_block};

/// A segment holding `n` checksum-valid header-only blocks and no commit block: a torn batch whose body
/// holds block images, as crafted text values could.
fn chain(n: usize) -> (Vec<u8>, SegmentIdentity) {
    let id = SegmentIdentity {
        store_id: [3; 16],
        file_id: 1,
        generation: 0,
    };
    let mut h = FileHeader::new(kind::LOG_SEGMENT, 0, 1, 0, id.store_id);
    h.base_sequence = 1;
    let mut b = h.encode().to_vec();
    for _ in 0..n {
        encode_block(0x50, FLAG_SKIPPABLE, &[], &mut b).unwrap();
    }
    b.resize(b.len() + 256, 0);
    (b, id)
}

#[test]
fn resynchronising_over_a_chain_of_block_images_takes_linear_block_reads() {
    for n in [1_000, 2_000, 4_000, 16_000] {
        let (bytes, id) = chain(n);
        BLOCK_READS.with(|c| c.set(0));
        let r = scan(&bytes, &id, ScanLimit::End).unwrap();
        let reads = BLOCK_READS.with(|c| c.get());
        assert!(r.batches.is_empty());
        assert!(matches!(r.tail, Some(Tail::Torn { start: 64, .. })));
        // The boundary read, the resynchronisation and the tail's commit search each read every header a
        // bounded number of times.
        assert!(
            reads <= 8 * n as u64 + 64,
            "{reads} block reads for {n} blocks"
        );
    }
}

/// Back-to-back checksum-valid block headers that each claim a 1 MiB body with a wrong checksum.
fn long_claims(headers_len: usize) -> (Vec<u8>, SegmentIdentity) {
    let (mut b, id) = chain(0);
    b.truncate(HEADER_LEN);
    let claim = crate::block::BlockHeader {
        block_type: 0x50,
        flags: FLAG_SKIPPABLE,
        body_len: 1 << 20,
        body_crc32c: 1,
    };
    while b.len() < HEADER_LEN + headers_len {
        b.extend_from_slice(&claim.encode());
    }
    b.resize(b.len() + (1 << 20) + 64, 0);
    (b, id)
}

#[test]
fn resynchronising_over_long_claimed_bodies_hashes_linear_bytes() {
    // Only the first claim, read at a block boundary, has its body checksummed; resynchronisation judges the
    // rest on their headers.
    for headers_len in [128 << 10, 512 << 10, 1 << 20] {
        let (bytes, id) = long_claims(headers_len);
        crate::crc::BYTES_HASHED.with(|c| c.set(0));
        let r = scan(&bytes, &id, ScanLimit::End).unwrap();
        let hashed = crate::crc::BYTES_HASHED.with(|c| c.get());
        assert!(r.batches.is_empty());
        // A bounded number of 12-byte header checksums per 16-byte header, plus the one body read in full.
        let bound = 6 * headers_len as u64 + (2 << 20);
        assert!(
            hashed <= bound,
            "{hashed} bytes hashed for {headers_len} bytes of headers"
        );
    }
}

/// `n` nested batch images that each pass the header checks: image `i` is one skippable block header at
/// `base + 16 i` whose body (with a wrong checksum) runs to its own commit block, the commits laid out in reverse
/// after the last header, so every image passes [`framing_holds`] and is read in full.
fn nested(n: u64) -> (Vec<u8>, SegmentIdentity) {
    let id = SegmentIdentity {
        store_id: [7; 16],
        file_id: 3,
        generation: 1,
    };
    let base = HEADER_LEN as u64 + 8;
    let e = base + 16 * n;
    let mut f = vec![0u8; (e + 64 * n) as usize];
    f[HEADER_LEN..HEADER_LEN + 8].fill(0xFF);
    for i in 0..n {
        let o = base + 16 * i;
        let ci = e + 64 * (n - 1 - i);
        let h = crate::block::BlockHeader {
            block_type: 0x90,
            flags: FLAG_SKIPPABLE,
            body_len: (ci - o - 16) as u32,
            body_crc32c: 0xDEAD_BEEF,
        }
        .encode();
        f[o as usize..o as usize + 16].copy_from_slice(&h);
        let mut d = Vec::new();
        d.extend_from_slice(&id.store_id);
        d.extend_from_slice(&id.file_id.to_le_bytes());
        d.extend_from_slice(&o.to_le_bytes());
        d.extend_from_slice(&h[..12]);
        let commit = Commit {
            generation: id.generation,
            block_count: 1,
            first_sequence: 1,
            last_sequence: 1,
            commit_time: 0,
            batch_bytes: ci - o,
            batch_crc32c: crc32c(&d),
        };
        let mut out = Vec::new();
        encode_block(block_type::COMMIT, 0, &commit.encode(), &mut out).unwrap();
        f[ci as usize..ci as usize + 64].copy_from_slice(&out);
    }
    (f, id)
}

#[test]
fn nested_batch_images_checksum_at_most_twice_the_window() {
    for n in [250u64, 500, 1_000] {
        let (bytes, id) = nested(n);
        let len = bytes.len() as u64;
        let (old, old_stats) = legacy::scan(&bytes, &id, ScanLimit::End);
        let (new, stats) = scan_with_stats(&bytes, &id, ScanLimit::End);
        assert_eq!(new, old);
        assert!(new.unwrap().batches.is_empty());
        // Every image passes the header checks and is read in full: the construction reaches the old cost.
        let images: u64 = (0..n).map(|i| 80 * (n - i)).sum();
        assert!(old_stats.bytes_checksummed >= images - 16 * n);
        assert!(
            !old_stats.within_bound(len),
            "{old_stats:?} for {len} bytes"
        );
        assert!(
            stats.bytes_checksummed <= 2 * len,
            "{stats:?} for {n} nested images in {len} bytes"
        );
        assert!(stats.within_bound(len), "{stats:?} for {len} bytes");
    }
}

/// An alias entry batch (A.6) of sequence `seq` at file offset `at`.
fn alias_batch(id: &SegmentIdentity, seq: u64, at: u64) -> Vec<u8> {
    let e = crate::entries::Entry::Alias {
        id: 1,
        name: "a".into(),
    };
    let body = crate::entries::EntriesBlock {
        first_sequence: seq,
        received_at: 0,
        entries: vec![e],
    };
    let mut builder = crate::batch::BatchBuilder::new();
    builder
        .push(block_type::ENTRIES, body.encode().unwrap())
        .unwrap();
    builder.encode(id, at, 0).unwrap()
}

/// `k` checksum-valid skippable blocks in a chain, each body exactly one valid batch, then a damaged word: every
/// batch is kept, and each kept batch ends where the next block of the chain starts.
fn chained_images(k: u64) -> (Vec<u8>, SegmentIdentity) {
    let (mut b, id) = chain(0);
    b.truncate(HEADER_LEN);
    for j in 0..k {
        let at = b.len() as u64;
        let batch = alias_batch(&id, j + 1, at + BLOCK_HEADER_LEN as u64);
        encode_block(0x50, FLAG_SKIPPABLE, &batch, &mut b).unwrap();
    }
    b.extend_from_slice(&[0xFF; 8]);
    b.resize(b.len() + 64, 0);
    (b, id)
}

#[test]
fn boundary_reads_along_one_chain_judge_each_block_once() {
    for k in [500u64, 1_000, 2_000] {
        let (bytes, id) = chained_images(k);
        let len = bytes.len() as u64;
        let (old, old_stats) = legacy::scan(&bytes, &id, ScanLimit::End);
        let (new, stats) = scan_with_stats(&bytes, &id, ScanLimit::End);
        assert_eq!(new, old);
        assert_eq!(new.unwrap().batches.len() as u64, k);
        // Each boundary read walked the rest of the chain: the construction reaches the old cost.
        assert!(old_stats.headers_visited >= k * k / 2);
        assert!(
            !old_stats.within_bound(len),
            "{old_stats:?} for {len} bytes"
        );
        assert!(
            stats.within_bound(len),
            "{stats:?} for {k} blocks in {len} bytes"
        );
    }
}

#[test]
fn a_resumed_scan_continues_where_the_last_stopped() {
    let (mut bytes, id) = chain(0);
    bytes.truncate(64);
    // A batch of one alias entry (A.6) at sequence `first`.
    let alias = |first: u64, at: u64| {
        let e = crate::entries::Entry::Alias {
            id: 1,
            name: "a".into(),
        };
        let body = crate::entries::EntriesBlock {
            first_sequence: first,
            received_at: 0,
            entries: vec![e],
        };
        let mut builder = crate::batch::BatchBuilder::new();
        builder
            .push(block_type::ENTRIES, body.encode().unwrap())
            .unwrap();
        builder.encode(&id, at, 0).unwrap()
    };
    bytes.extend_from_slice(&alias(1, 64));
    let at = bytes.len() as u64;
    bytes.extend_from_slice(&alias(2, at));
    let mut s = Scanner::new(&bytes, &id, ScanLimit::Mark(at)).unwrap();
    assert!(matches!(s.next(), Some(Ok(Found::Batch(_)))));
    assert!(s.next().is_none());
    let point = s.resume_point();
    assert_eq!(
        point,
        Resume {
            offset: at,
            last_sequence: Some(1)
        }
    );
    let mut s = Scanner::resume(&bytes, &id, ScanLimit::Mark(bytes.len() as u64), point).unwrap();
    assert!(
        matches!(s.next(), Some(Ok(Found::Batch(b))) if b.offset == at && b.commit.first_sequence == 2)
    );
    assert!(s.next().is_none());
    // Resume points off a block boundary or above the limit are refused.
    for bad in [at + 4, 8, bytes.len() as u64 + 8] {
        let r = Scanner::resume(
            &bytes,
            &id,
            ScanLimit::Mark(bytes.len() as u64),
            Resume {
                offset: bad,
                last_sequence: None,
            },
        );
        assert_eq!(r.err(), Some(FormatError::ScanPoint { offset: bad }));
    }
    assert_eq!(
        Scanner::new(&bytes, &id, ScanLimit::Mark(at + 4)).err(),
        Some(FormatError::ScanPoint { offset: at + 4 })
    );
    // A resumed scan still requires the sequence to rise.
    let mut s = Scanner::resume(
        &bytes,
        &id,
        ScanLimit::Mark(bytes.len() as u64),
        Resume {
            offset: at,
            last_sequence: Some(2),
        },
    )
    .unwrap();
    assert!(matches!(s.next(), Some(Ok(Found::Region(_)))));
}

#[test]
fn walked_chains_longer_than_a_batch_remember_at_most_a_batch_of_block_starts() {
    let (mut bytes, id) = chain(0);
    bytes.truncate(HEADER_LEN);
    bytes.extend_from_slice(&[0xFF; 8]);
    let body = [0u8; 112];
    while bytes.len() < 3 * crate::batch::MAX_BATCH_DATA as usize {
        encode_block(0x50, FLAG_SKIPPABLE, &body, &mut bytes).unwrap();
    }
    let len = bytes.len() as u64;
    let mut reads = Reads::new(&bytes);
    reads.use_table();
    let mut chains = Chains::walked();
    let found = chains
        .resync(
            &mut reads,
            &id,
            HEADER_LEN as u64,
            len,
            1,
            None,
            &Walks::default(),
        )
        .unwrap();
    assert!(found.is_none());
    let bound = ((crate::batch::MAX_BATCH_DATA + (1 << 16)) / 128 + 16) as usize;
    assert!(
        chains.peak <= bound,
        "{} block starts remembered, bound {bound}",
        chains.peak
    );
}

/// `k` regions, each a damaged word, then a skippable block header whose chain leaps to one shared chain of `n`
/// header-only blocks at the end of the file, then a batch: every region's resynchronisation meets the shared chain.
fn regions_sharing_a_chain(k: u64, n: u64) -> (Vec<u8>, SegmentIdentity) {
    let (mut b, id) = chain(0);
    b.truncate(HEADER_LEN);
    let batch_len = alias_batch(&id, 1, 0).len() as u64;
    let region = 8 + 16 + batch_len;
    let shared = HEADER_LEN as u64 + k * region;
    for j in 0..k {
        let at = b.len() as u64;
        b.extend_from_slice(&[0xFF; 8]);
        let leap = crate::block::BlockHeader {
            block_type: 0x50,
            flags: FLAG_SKIPPABLE,
            body_len: (shared - at - 24) as u32,
            body_crc32c: 0,
        };
        b.extend_from_slice(&leap.encode());
        b.extend_from_slice(&alias_batch(&id, j + 1, at + 24));
    }
    for _ in 0..n {
        encode_block(0x50, FLAG_SKIPPABLE, &[], &mut b).unwrap();
    }
    b.extend_from_slice(&[0xFF; 8]);
    b.resize(b.len() + 64, 0);
    (b, id)
}

#[test]
fn resynchronising_in_many_regions_walks_a_shared_chain_once() {
    for (k, n) in [(500u64, 500u64), (1_000, 1_000), (2_000, 2_000)] {
        let (bytes, id) = regions_sharing_a_chain(k, n);
        let len = bytes.len() as u64;
        let (old, old_stats) = legacy::scan(&bytes, &id, ScanLimit::End);
        let (new, stats) = scan_with_stats(&bytes, &id, ScanLimit::End);
        assert_eq!(new, old);
        let r = new.unwrap();
        assert_eq!(r.batches.len() as u64, k);
        assert_eq!(r.regions.len() as u64, k);
        assert!(old_stats.headers_visited >= k * n);
        assert!(
            !old_stats.within_bound(len),
            "{old_stats:?} for {len} bytes"
        );
        assert!(stats.within_bound(len), "{stats:?} for {len} bytes");
    }
}

/// A damaged word, then a resynchronisation candidate whose one data block (with a wrong body checksum) holds
/// readable commit blocks every 4 KiB and runs to the candidate's own commit block at the end of the file: reading
/// the candidate fills the prefix table to the end before the tail's commit search has passed its start.
fn tail_of_commits(commits: u64) -> (Vec<u8>, SegmentIdentity) {
    let (mut b, id) = chain(0);
    b.truncate(HEADER_LEN);
    b.extend_from_slice(&[0xFF; 8]);
    let o = b.len() as u64;
    let commit_at = o + 16 + 4_096 * commits;
    let h = crate::block::BlockHeader {
        block_type: 0x50,
        flags: FLAG_SKIPPABLE,
        body_len: (commit_at - o - 16) as u32,
        body_crc32c: 0,
    }
    .encode();
    b.extend_from_slice(&h);
    for i in 0..commits {
        let commit = Commit {
            generation: id.generation,
            block_count: 1,
            first_sequence: i + 1,
            last_sequence: i + 1,
            commit_time: 0,
            batch_bytes: 1 << 40,
            batch_crc32c: 0,
        };
        b.resize(b.len() + 4_096 - 64, 0x11);
        encode_block(block_type::COMMIT, 0, &commit.encode(), &mut b).unwrap();
    }
    let mut d = Vec::new();
    d.extend_from_slice(&id.store_id);
    d.extend_from_slice(&id.file_id.to_le_bytes());
    d.extend_from_slice(&o.to_le_bytes());
    d.extend_from_slice(&h[..12]);
    let commit = Commit {
        generation: id.generation,
        block_count: 1,
        first_sequence: 1,
        last_sequence: 1,
        commit_time: 0,
        batch_bytes: commit_at - o,
        batch_crc32c: crc32c(&d),
    };
    encode_block(block_type::COMMIT, 0, &commit.encode(), &mut b).unwrap();
    (b, id)
}

#[test]
fn a_torn_tail_with_readable_commits_checksums_each_byte_once() {
    for commits in [64u64, 256, 1_024] {
        let (bytes, id) = tail_of_commits(commits);
        let len = bytes.len() as u64;
        let (old, _) = legacy::scan(&bytes, &id, ScanLimit::End);
        let (new, stats) = scan_with_stats(&bytes, &id, ScanLimit::End);
        assert_eq!(new, old);
        let r = new.unwrap();
        assert!(
            matches!(&r.tail, Some(Tail::Torn { commits: c, .. }) if c.len() as u64 == commits + 1)
        );
        assert!(stats.bytes_checksummed <= len, "{stats:?} for {len} bytes");
    }
}

#[test]
fn an_undamaged_segment_is_read_in_one_pass_of_direct_reads() {
    let (mut bytes, id) = chain(0);
    bytes.truncate(HEADER_LEN);
    let k = 1_000u64;
    for j in 0..k {
        let at = bytes.len() as u64;
        bytes.extend_from_slice(&alias_batch(&id, j + 1, at));
    }
    bytes.resize(bytes.len() + 4_096, 0);
    let len = bytes.len() as u64;
    let (r, stats) = scan_with_stats(&bytes, &id, ScanLimit::End);
    let r = r.unwrap();
    assert_eq!(r.batches.len() as u64, k);
    assert!(matches!(r.tail, Some(Tail::Clean { .. })));
    // Each of the two blocks of a batch: its header peeked, then read once.
    assert!(stats.headers_visited <= 4 * k + 2, "{stats:?}");
    assert!(stats.bytes_checksummed <= len, "{stats:?} for {len} bytes");
    let bodies: usize = r.batches.iter().map(|b| b.blocks[0].block.body.len()).sum();
    assert_eq!(stats.bytes_body_checked, bodies as u64);
}

#[test]
fn the_bound_holds_body_checks_and_the_failed_read_budget_per_window() {
    let len = 1u64 << 20;
    // The file once (Q-651), the direct read that found damage, and per window the budget and the read that reached
    // it.
    let per_window = FAILED_READ_BUDGET + len;
    let at_bound = ScanStats {
        bytes_body_checked: 2 * len + per_window,
        failed_bytes_peak: per_window,
        ..ScanStats::default()
    };
    assert!(at_bound.within_bound(len));
    let more_body = ScanStats {
        bytes_body_checked: at_bound.bytes_body_checked + 1,
        ..at_bound
    };
    assert!(!more_body.within_bound(len));
    let more_failed = ScanStats {
        failed_bytes_peak: per_window + 1,
        ..at_bound
    };
    assert!(!more_failed.within_bound(len));
    // A manifest's windows are a body: in a 32 MiB file, two windows each of the budget and one body.
    let big = 32u64 << 20;
    let manifest_window = FAILED_READ_BUDGET + u64::from(crate::block::MAX_BODY_LEN);
    let manifest = ScanStats {
        bytes_body_checked: big + 2 * manifest_window,
        failed_bytes_peak: manifest_window,
        ..ScanStats::default()
    };
    assert!(manifest.within_manifest_bound(big));
    assert!(!manifest.within_bound(big));
    assert!(
        !ScanStats {
            bytes_body_checked: manifest.bytes_body_checked + 1,
            ..manifest
        }
        .within_manifest_bound(big)
    );
}

#[test]
fn failed_reads_are_charged_per_window_and_pass_the_rest_of_one_past_its_budget() {
    let w = 1u64 << 20;
    let mut f = FailedReads::new(w);
    f.charge(0, FAILED_READ_BUDGET - 1);
    assert_eq!(f.next_from(8), 8);
    f.charge(8, 1);
    assert_eq!(f.next_from(16), w);
    assert_eq!(f.next_from(w), w);
    f.charge(w, 8);
    assert_eq!((f.next_from(w + 8), f.peak()), (w + 8, FAILED_READ_BUDGET));
    let mut z = FailedReads::new(0);
    z.charge(8, FAILED_READ_BUDGET);
    assert_eq!(z.next_from(16), 16);
}
