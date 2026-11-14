//! Property tests of the segment container: round trip, single-bit corruption, truncation and the mark.

mod support;

use kvik_log::batch::{BatchBuilder, SegmentIdentity};
use kvik_log::block::{FLAG_SKIPPABLE, block_type};
use kvik_log::body::{DataPrefix, IndexRecord};
use kvik_log::scan::{ScanLimit, ScanReport, SealState, Tail, scan};
use kvik_log::segment::SegmentImage;
use proptest::prelude::*;
use support::*;

/// One data block: facts or entries with `count` sequences and an opaque payload after the prefix, or an unknown
/// skippable block.
#[derive(Clone, Debug)]
enum Block {
    Data {
        facts: bool,
        count: u32,
        payload: Vec<u8>,
    },
    Skippable {
        payload: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
struct Plan {
    id: SegmentIdentity,
    base: u64,
    /// Per batch: the gap before it, its receive time, its commit time and its blocks.
    batches: Vec<(u64, i64, i64, Vec<Block>)>,
    zeros: u64,
    seal: Option<bool>,
}

fn block() -> impl Strategy<Value = Block> {
    prop_oneof![
        8 => (any::<bool>(), 1u32..=4, prop::collection::vec(any::<u8>(), 0..48))
            .prop_map(|(facts, count, payload)| Block::Data { facts, count, payload }),
        1 => prop::collection::vec(any::<u8>(), 0..24).prop_map(|payload| Block::Skippable { payload }),
    ]
}

fn plan() -> impl Strategy<Value = Plan> {
    let id = (any::<[u8; 16]>(), any::<u64>(), any::<u32>()).prop_map(
        |(store_id, file_id, generation)| SegmentIdentity {
            store_id,
            file_id,
            generation,
        },
    );
    // Every batch has a data block first, so its sequences are known.
    let batch = (
        prop_oneof![4 => Just(0u64), 1 => 1u64..5],
        any::<i64>(),
        any::<i64>(),
        block(),
        prop::collection::vec(block(), 0..3),
    )
        .prop_map(|(gap, r, c, first, rest)| {
            let first = match first {
                Block::Skippable { payload } => Block::Data {
                    facts: true,
                    count: 1,
                    payload,
                },
                b => b,
            };
            (gap, r, c, std::iter::once(first).chain(rest).collect())
        });
    (
        id,
        1u64..1_000_000,
        prop::collection::vec(batch, 1..6),
        0u64..32,
        prop::option::of(any::<bool>()),
    )
        .prop_map(|(id, base, batches, zeros, seal)| Plan {
            id,
            base,
            batches,
            zeros,
            seal,
        })
}

/// What a scan should return for each batch: offset, end, and the bytes of its blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Expected {
    offset: u64,
    end: u64,
    blocks: Vec<(u8, u8, Vec<u8>)>,
    first: u64,
    last: u64,
    commit_time: i64,
}

fn build(p: &Plan) -> (Vec<u8>, Vec<Expected>) {
    let mut builders = Vec::new();
    let mut next = p.base;
    for (gap, received_at, commit_time, blocks) in &p.batches {
        next += gap;
        let mut b = BatchBuilder::new();
        for blk in blocks {
            match blk {
                Block::Data {
                    facts,
                    count,
                    payload,
                } => {
                    let mut body = vec![0u8; 24];
                    DataPrefix {
                        first_sequence: next,
                        received_at: *received_at,
                        count: *count,
                    }
                    .write(&mut body);
                    body.extend_from_slice(payload);
                    let t = if *facts {
                        block_type::FACTS
                    } else {
                        block_type::ENTRIES
                    };
                    b.push(t, body).unwrap();
                    next += u64::from(*count);
                }
                Block::Skippable { payload } => {
                    b.push_unknown(0x60, FLAG_SKIPPABLE, payload.clone())
                        .unwrap();
                }
            }
        }
        builders.push((b, *commit_time));
    }
    let size: u64 = builders
        .iter()
        .map(|(b, _)| b.encode(&p.id, 64, 0).unwrap().len() as u64)
        .sum();
    let len = 64 + size + 8 * p.zeros;
    let mut s = SegmentImage::create_ahead(p.id, 0, len).unwrap();
    s.take_into_use(p.base).unwrap();
    let mut expected = Vec::new();
    for (b, t) in &builders {
        let offset = s.append(b, *t).unwrap();
        let (first, last) = b.sequences().unwrap();
        let bytes = b.encode(&p.id, offset, *t).unwrap();
        let blocks = read_back(&bytes);
        expected.push(Expected {
            offset,
            end: offset + bytes.len() as u64,
            blocks,
            first,
            last,
            commit_time: *t,
        });
    }
    if let Some(set_len) = p.seal {
        let end = s
            .write_seal_blocks(vec![IndexRecord {
                series_id: 1,
                fact_count: 1,
                min_t: 0,
                max_t: 0,
            }])
            .unwrap();
        if set_len {
            s.set_len(end);
        }
    }
    (s.into_bytes(), expected)
}

/// The blocks `(type, flags, body)` of the encoded batch `bytes`, split by hand from the block headers.
fn read_back(bytes: &[u8]) -> Vec<(u8, u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while bytes[at] != block_type::COMMIT {
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        out.push((
            bytes[at],
            bytes[at + 1],
            bytes[at + 16..at + 16 + len].to_vec(),
        ));
        at += 16 + len.div_ceil(8) * 8;
    }
    out
}

/// The zero padding after each block body of a batch.
fn padding(e: &Expected) -> Vec<std::ops::Range<u64>> {
    let mut at = e.offset;
    let mut out = Vec::new();
    for (_, _, body) in &e.blocks {
        let body_end = at + 16 + body.len() as u64;
        let next = at + 16 + (body.len() as u64).div_ceil(8) * 8;
        out.push(body_end..next);
        at = next;
    }
    out
}

fn got(r: &ScanReport<'_>) -> Vec<Expected> {
    r.batches
        .iter()
        .map(|b| Expected {
            offset: b.offset,
            end: b.end(),
            blocks: b
                .blocks
                .iter()
                .map(|d| {
                    (
                        d.block.header.block_type,
                        d.block.header.flags,
                        d.block.body.to_vec(),
                    )
                })
                .collect(),
            first: b.commit.first_sequence,
            last: b.commit.last_sequence,
            commit_time: b.commit.commit_time,
        })
        .collect()
}

fn all_zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

proptest! {
    #[test]
    fn encode_then_decode_round_trips(p in plan()) {
        let (bytes, expected) = build(&p);
        let r = scan(&bytes, &p.id, ScanLimit::End).unwrap();
        prop_assert_eq!(got(&r), expected.clone());
        prop_assert!(r.regions.is_empty());
        match p.seal {
            None => prop_assert_eq!(r.tail, Some(Tail::Clean { offset: expected.last().unwrap().end })),
            Some(_) => {
                let seal = r.seal.unwrap();
                let state = if seal.end == bytes.len() as u64 { SealState::Sealed } else { SealState::LengthPending };
                prop_assert_eq!(seal.state, state);
                // The seal blocks take 144 bytes (18 words): they fit in the zeros or grow the file.
                prop_assert_eq!(seal.end == bytes.len() as u64, p.seal == Some(true) || p.zeros <= 18);
                prop_assert!(seal.file_crc_matches(&bytes));
                prop_assert_eq!(seal.trailer.batch_count, expected.len() as u64);
                prop_assert_eq!(seal.trailer.last_sequence, expected.last().unwrap().last);
                prop_assert!(r.tail.is_none());
            }
        }
        // Up to a mark at each batch boundary, exactly the batches below it.
        for k in 0..=expected.len() {
            let mark = if k == 0 { 64 } else { expected[k - 1].end };
            let r = scan(&bytes, &p.id, ScanLimit::Mark(mark)).unwrap();
            prop_assert_eq!(got(&r), expected[..k].to_vec());
            prop_assert!(r.regions.is_empty());
        }
    }

    #[test]
    fn a_single_bit_flip_is_detected_or_passed(p in plan(), bit in any::<prop::sample::Index>()) {
        let (mut bytes, expected) = build(&p);
        let bit = bit.index(bytes.len() * 8);
        let byte = (bit / 8) as u64;
        bytes[bit / 8] ^= 1 << (bit % 8);
        let mark = expected.last().unwrap().end;
        for limit in [ScanLimit::End, ScanLimit::Mark(mark)] {
            // Checksums are checked before any field is interpreted, so a flip is never a version refusal.
            let r = scan(&bytes, &p.id, limit).unwrap();
            let got = got(&r);
            // Nothing is accepted that is not an original batch, byte for byte.
            for g in &got {
                prop_assert!(expected.contains(g), "accepted a batch that was not written: {:?}", g);
            }
            // Every batch the flip did not touch is still found.
            for e in expected.iter().filter(|e| !(e.offset..e.end).contains(&byte)) {
                prop_assert!(got.contains(e), "lost an untouched batch at {}", e.offset);
            }
            // The batch the flip touched is reported as damage, unless the flip is in the zero padding after a
            // block body: padding is not covered by the batch's checksums, so such a flip is accepted by design
            // (`format.md` A, common rules; a sealed file's `file_crc32c` covers it).
            if let Some(e) = expected.iter().find(|e| (e.offset..e.end).contains(&byte)) {
                if got.contains(e) {
                    prop_assert!(padding(e).iter().any(|p| p.contains(&byte)), "flip at {} accepted", byte);
                } else {
                    let in_region = r.regions.iter().any(|g| g.start <= byte && byte < g.end);
                    let in_tail = matches!(r.tail, Some(Tail::Torn { start, .. }) if start <= byte);
                    prop_assert!(in_region || in_tail, "damage at {} not reported", byte);
                }
            }
        }
    }

    #[test]
    fn truncation_leaves_the_complete_batches_before_it(p in plan(), cut in any::<prop::sample::Index>()) {
        let (bytes, expected) = build(&p);
        let k = cut.index(bytes.len() + 1);
        // Cut short, and zero-filled from `k`.
        let short = &bytes[..k];
        let mut zeroed = bytes.clone();
        zeroed[k..].fill(0);
        for image in [short, &zeroed[..]] {
            let r = scan(image, &p.id, ScanLimit::End).unwrap();
            // The batches whose bytes are all still there and unchanged.
            let kept: Vec<_> = expected
                .iter()
                .filter(|e| image.get(e.offset as usize..e.end as usize) == bytes.get(e.offset as usize..e.end as usize))
                .cloned()
                .collect();
            if let Some(seal) = &r.seal {
                // Cut at or after the end marker: still sealed.
                prop_assert!(image.len() as u64 >= seal.end);
                prop_assert_eq!(got(&r), expected.clone());
                continue;
            }
            prop_assert_eq!(got(&r), kept.clone());
            prop_assert!(r.regions.is_empty());
            let p0 = kept.last().map_or(64, |e| e.end);
            let rest = image.get(p0 as usize..).unwrap_or(&[]);
            match r.tail.unwrap() {
                Tail::Clean { offset } => {
                    prop_assert!(all_zero(rest));
                    prop_assert_eq!(offset, p0.min(image.len() as u64));
                }
                Tail::Torn { start, end, .. } => {
                    prop_assert!(!all_zero(rest));
                    prop_assert_eq!(start, p0);
                    prop_assert_eq!(end, image.iter().rposition(|&x| x != 0).unwrap() as u64 + 1);
                }
            }
        }
    }

    #[test]
    fn a_reader_takes_exactly_the_batches_below_the_mark(p in plan(), at in any::<prop::sample::Index>()) {
        let (bytes, expected) = build(&p);
        let top = expected.last().unwrap().end;
        let mark = 64 + 8 * at.index(((top - 64) / 8 + 1) as usize) as u64;
        let r = scan(&bytes, &p.id, ScanLimit::Mark(mark)).unwrap();
        let below: Vec<_> = expected.iter().filter(|e| e.end <= mark).cloned().collect();
        prop_assert_eq!(got(&r), below.clone());
        let p0 = below.last().map_or(64, |e| e.end);
        if p0 == mark {
            prop_assert!(r.regions.is_empty());
        } else {
            prop_assert_eq!(r.regions.len(), 1);
            prop_assert_eq!((r.regions[0].start, r.regions[0].end), (p0, mark));
        }
    }
}

/// Every single-bit flip and every truncation offset of the three-batch segment of the golden files.
#[test]
fn every_bit_flip_and_every_cut_of_a_small_segment() {
    let (s, offsets) = segment(ID, 1_024, 1, &three_batches());
    let bytes = s.bytes();
    let originals = got(&scan(bytes, &ID, ScanLimit::End).unwrap());
    assert_eq!(originals.len(), offsets.len());
    for bit in 0..bytes.len() * 8 {
        let mut b = bytes.to_vec();
        b[bit / 8] ^= 1 << (bit % 8);
        let g = got(&scan(&b, &ID, ScanLimit::End).unwrap());
        assert!(g.iter().all(|x| originals.contains(x)), "bit {bit}");
        let byte = (bit / 8) as u64;
        assert!(
            originals
                .iter()
                .filter(|e| !(e.offset..e.end).contains(&byte))
                .all(|e| g.contains(e)),
            "bit {bit}"
        );
    }
    for k in 0..=bytes.len() {
        let g = got(&scan(&bytes[..k], &ID, ScanLimit::End).unwrap());
        let expected: Vec<_> = originals
            .iter()
            .filter(|e| e.end as usize <= k)
            .cloned()
            .collect();
        assert_eq!(g, expected, "cut at {k}");
    }
}
