//! A differential test of the bounded scan (`format.md` A, common rules: scan bound; Q-110, Q-873) on crafted and
//! damaged segments: small ones against the reference scan, which checksums every body directly and reads every
//! boundary in full, and ones longer than the rings of the chain ends and the valid headers against the walked chains;
//! a torn tail's commit blocks against a plain search, and the header checksums of a torn tail of failing commit-like
//! steps.

mod support;

use kvik_log::FormatError;
use kvik_log::batch::MAX_BATCH_DATA;
use kvik_log::block::{BlockHeader, FLAG_SKIPPABLE, block_type};
use kvik_log::body::{COMMIT_LEN, Commit};
use kvik_log::content::BatchContent;
use kvik_log::crc::crc32c;
use kvik_log::header::HEADER_LEN;
use kvik_log::scan::{
    Found, ScanLimit, ScanReport, ScanStats, Scanner, Tail, legacy, scan_with_stats,
};
use kvik_log::segment::SegmentImage;
use proptest::prelude::*;
use support::*;

/// A generator of crafted bytes: xorshift, and the highest sequence a crafted batch may take.
struct R(u64, u64);

impl R {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const TYPES: [u8; 9] = [
    block_type::FACTS,
    block_type::ENTRIES,
    0x50,
    0x50,
    0x30,
    block_type::COMMIT,
    block_type::SEGMENT_INDEX,
    block_type::TRAILER,
    0,
];

fn header(t: u8, flags: u8, len: u32, crc: u32) -> [u8; 16] {
    let mut h = [0u8; 16];
    h[0] = t;
    h[1] = flags;
    h[4..8].copy_from_slice(&len.to_le_bytes());
    h[8..12].copy_from_slice(&crc.to_le_bytes());
    let c = crc32c(&h[..12]);
    h[12..16].copy_from_slice(&c.to_le_bytes());
    h
}

fn some_len(r: &mut R) -> u32 {
    match r.below(10) {
        0 => 0,
        1 | 2 => 8 * r.below(40) as u32,
        3 => r.below(300) as u32,
        4 => r.below(1 << 16) as u32,
        5 => r.below(MAX_BATCH_DATA) as u32,
        6 => (MAX_BATCH_DATA as i64 - 16 + 8 * (r.below(5) as i64 - 2)) as u32,
        7 => 48,
        8 => r.next() as u32,
        _ => 8 * r.below(6) as u32,
    }
}

/// Crafted bytes of about `n` bytes for the offset `abs`: headers, chains, commit blocks naming starts, framed
/// candidates, complete batches of any sequence, runs of overlapping valid headers, words and zeros.
fn junk(r: &mut R, n: usize, abs: u64, dense_max: u32) -> Vec<u8> {
    let mut f: Vec<u8> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    while f.len() < n {
        let p = f.len();
        match r.below(16) {
            0..=3 => {
                starts.push(p);
                let t = TYPES[r.below(9) as usize];
                let flags = r.below(2) as u8;
                let len = if t == block_type::COMMIT && r.chance(70) {
                    48
                } else {
                    some_len(r)
                };
                f.extend_from_slice(&header(t, flags, len, r.next() as u32));
            }
            4 | 5 => {
                // A chain of blocks with small bodies, some with a body checksum that holds.
                for _ in 0..1 + r.below(6) {
                    starts.push(f.len());
                    let body: Vec<u8> = (0..r.below(40)).map(|_| r.next() as u8).collect();
                    let crc = if r.chance(70) { crc32c(&body) } else { 7 };
                    let t = [0x50, 0x50, block_type::FACTS, 0x30][r.below(4) as usize];
                    f.extend_from_slice(&header(t, r.below(2) as u8, body.len() as u32, crc));
                    f.extend_from_slice(&body);
                    f.resize(f.len().next_multiple_of(8), 0);
                }
            }
            6..=8 => {
                let bb = match r.below(8) {
                    0 => 8 * r.below(64),
                    1 => r.next(),
                    2 => MAX_BATCH_DATA + 8 * r.below(3) - 8,
                    _ if !starts.is_empty() => {
                        let s = starts
                            [starts.len() - 1 - r.below(starts.len().min(24) as u64) as usize];
                        (p - s) as u64 + r.below(20) / 19
                    }
                    _ => 16,
                };
                let c = Commit {
                    generation: if r.chance(90) { 0 } else { 1 },
                    block_count: 1 + r.below(4) as u32,
                    first_sequence: 1 + r.below(50),
                    last_sequence: 60 + r.below(50),
                    commit_time: 0,
                    batch_bytes: bb,
                    batch_crc32c: r.next() as u32,
                }
                .encode();
                let crc = if r.chance(75) { crc32c(&c) } else { 1 };
                let len = if r.chance(92) { 48 } else { 40 };
                starts.push(p);
                f.extend_from_slice(&header(block_type::COMMIT, 0, len, crc));
                f.extend_from_slice(&c);
            }
            9 => {
                // A candidate whose framing and batch checksum hold, with a wrong body checksum or an old sequence.
                let k = 1 + r.below(4) as usize;
                let wrong = r.below(k as u64 + 1) as usize;
                let blocks: Vec<(u8, u8, Vec<u8>)> = (0..k)
                    .map(|i| {
                        if i == 0 && r.chance(50) {
                            (
                                block_type::FACTS,
                                0,
                                facts_body(1 + r.below(400), T0, &[(1, T0, 0.5)]),
                            )
                        } else {
                            (0x50, FLAG_SKIPPABLE, vec![i as u8; r.below(64) as usize])
                        }
                    })
                    .collect();
                let s = 1 + r.below(r.1);
                let mut b = raw_batch(&ID, abs + p as u64, &blocks, s, s, T0);
                if wrong < k {
                    let mut at = 0;
                    for blk in blocks.iter().take(wrong) {
                        at += 16 + blk.2.len().next_multiple_of(8);
                    }
                    // The batch checksum covers each header's body checksum, so the body is broken, or the commit.
                    if blocks[wrong].2.is_empty() {
                        let last = b.len() - 20;
                        b[last] ^= 1;
                    } else {
                        b[at + 16] ^= 1;
                    }
                }
                starts.push(p);
                f.extend_from_slice(&b);
            }
            10 => {
                // Valid headers at every 8 bytes: each header's checksum is the next one's length.
                let m = 2 + r.below(40) as usize;
                f.resize(p + 8 * m + 16, 0);
                f[p] = TYPES[r.below(5) as usize];
                f[p + 4..p + 8].copy_from_slice(&(8 * r.below(64) as u32).to_le_bytes());
                for i in 0..m {
                    let q = p + 8 * i;
                    starts.push(q);
                    loop {
                        let mut w = (r.next() as u32).to_le_bytes();
                        if r.chance(60) {
                            w[0] = TYPES[r.below(8) as usize];
                            w[1] = r.below(2) as u8;
                        }
                        f[q + 8..q + 12].copy_from_slice(&w);
                        let c = crc32c(&f[q..q + 12]);
                        if c <= dense_max {
                            f[q + 12..q + 16].copy_from_slice(&c.to_le_bytes());
                            break;
                        }
                    }
                }
            }
            11 => f.extend_from_slice(&r.next().to_le_bytes()),
            12 => f.resize(p + 8 * (1 + r.below(8) as usize), 0),
            13 => f.resize(p + 8 * (1 + r.below(3) as usize), 0xFF),
            14 => {
                let rows: Vec<_> = (0..1 + r.below(4))
                    .map(|i| (1, T0 + i as i64, 1.0))
                    .collect();
                let s = 1 + r.below(r.1);
                let b = facts_batch(s, T0, &rows)
                    .encode(&ID, abs + p as u64, T0)
                    .unwrap();
                starts.push(p);
                f.extend_from_slice(&b);
            }
            _ => {
                // A header whose body ends at an earlier or later start.
                starts.push(p);
                let steps = 2 + r.below(200) as u32;
                let t = TYPES[r.below(5) as usize];
                f.extend_from_slice(&header(
                    t,
                    1,
                    (8 * steps - 16).wrapping_sub(r.below(8) as u32),
                    3,
                ));
            }
        }
        f.resize(f.len().next_multiple_of(8), 0);
    }
    f
}

fn build(r: &mut R, target: usize, large: bool) -> Vec<u8> {
    let mut s = SegmentImage::create_ahead(ID, T0, 4_096).unwrap();
    s.take_into_use(1).unwrap();
    let mut f = s.into_bytes();
    f.truncate(HEADER_LEN);
    if r.chance(8) {
        f[r.below(HEADER_LEN as u64) as usize] ^= 4;
    }
    let dense_max = if large { 4 << 20 } else { 1 << 20 };
    let mut seq = 1u64;
    let valid = 20 + r.below(70);
    while f.len() < target {
        let at = f.len();
        if r.chance(valid) {
            let kind = r.below(if large { 12 } else { 8 });
            let mut b = match kind {
                0..=3 => {
                    let n = 1 + r.below(5);
                    let rows: Vec<_> = (0..n).map(|i| (1, T0 + i as i64, 2.0)).collect();
                    let b = facts_batch(seq, T0, &rows)
                        .encode(&ID, at as u64, T0)
                        .unwrap();
                    seq += n;
                    b
                }
                4 => {
                    let b = entries_batch(seq, T0, &[alias(1, "a")])
                        .encode(&ID, at as u64, T0)
                        .unwrap();
                    seq += 1;
                    b
                }
                5..=7 => {
                    let mut blocks =
                        vec![(block_type::FACTS, 0, facts_body(seq, T0, &[(1, T0, 0.0)]))];
                    let mut inner = at as u64 + 16 + blocks[0].2.len().next_multiple_of(8) as u64;
                    for _ in 0..r.below(6) {
                        let n = r.below(600) as usize;
                        blocks.push((0x50, FLAG_SKIPPABLE, junk(r, n, inner + 16, dense_max)));
                        inner += 16 + blocks.last().unwrap().2.len().next_multiple_of(8) as u64;
                    }
                    if kind == 7 && r.chance(15) {
                        blocks.push((0x30, 0, vec![1; 12]));
                    }
                    let b = raw_batch(&ID, at as u64, &blocks, seq, seq, T0);
                    seq += 1;
                    b
                }
                _ => {
                    let first = facts_body(seq, T0, &[(1, T0, 0.0)]);
                    let head = 16 + first.len().next_multiple_of(8) + 16;
                    let size = match r.below(5) {
                        0 => MAX_BATCH_DATA as usize - head - 8 * r.below(3) as usize,
                        1 => MAX_BATCH_DATA as usize - head + 8,
                        _ => (1 << 16) + r.below(7 << 20) as usize,
                    };
                    let mut filler = if r.chance(50) {
                        vec![0x11u8; size]
                    } else {
                        let mut j = junk(r, size.min(200_000), at as u64 + head as u64, dense_max);
                        j.resize(size, 0x22);
                        j
                    };
                    filler.truncate(size);
                    let blocks = vec![
                        (block_type::FACTS, 0, first),
                        (0x50, FLAG_SKIPPABLE, filler),
                    ];
                    let b = raw_batch(&ID, at as u64, &blocks, seq, seq, T0);
                    seq += 1;
                    b
                }
            };
            if r.chance(12) {
                let i = match r.below(4) {
                    0 => 12 + r.below(4) as usize,
                    1 => r.below(16) as usize,
                    2 => b.len() - 1 - r.below(64) as usize,
                    _ => r.below(b.len() as u64) as usize,
                };
                b[i] ^= 1 << r.below(8);
            }
            f.extend_from_slice(&b);
        } else {
            let n = if large && r.chance(10) {
                r.below(300_000)
            } else {
                r.below(1_500)
            } as usize;
            let j = junk(r, n, at as u64, dense_max);
            f.extend_from_slice(&j);
        }
        if r.chance(2) {
            seq += r.below(3);
        }
    }
    match r.below(6) {
        0 => {}
        1 => f.truncate(f.len() - r.below(100) as usize),
        _ => f.resize(f.len() + 8 * r.below(600) as usize, 0),
    }
    let ring = 16usize << 20;
    if large {
        // A copy a ring above its origin, with headers of the copy broken: a header remembered as valid a ring below
        // must not be taken for the one read here.
        for _ in 0..1 + r.below(4) {
            if f.len() <= ring + 4_096 {
                break;
            }
            let to = (ring + r.below((f.len() - ring - 2_048) as u64) as usize) & !7;
            let n = ((1 + r.below(1 << 19)) as usize).min(f.len() - to) & !7;
            f.copy_within(to - ring..to - ring + n, to);
            let mut q = to;
            while q + 16 <= to + n {
                if BlockHeader::decode(f[q..q + 16].try_into().unwrap()).is_some() && r.chance(60) {
                    let i = [0, 1, 4, 12, 13, 15][r.below(6) as usize];
                    f[q + i] ^= 1 << r.below(8);
                }
                q += 8;
            }
        }
    }
    for _ in 0..r.below(4) {
        let len = f.len();
        match r.below(6) {
            0 => f[r.below(len as u64) as usize] ^= 1 << r.below(8),
            1 => {
                let at = r.below(len as u64) as usize & !7;
                let w = r.next().to_le_bytes();
                let n = (len - at).min(8);
                f[at..at + n].copy_from_slice(&w[..n]);
            }
            2 => {
                let at = r.below(len as u64) as usize;
                let end = (at + r.below(70_000) as usize).min(len);
                f[at..end].fill(0);
            }
            3 | 4 => {
                let (from, to) = (
                    r.below(len as u64) as usize & !7,
                    r.below(len as u64) as usize & !7,
                );
                let n = (r.below(if large { 1 << 20 } else { 4_096 }) as usize)
                    .min(len - from)
                    .min(len - to);
                f.copy_within(from..from + n, to);
            }
            _ => f.truncate(len - r.below(200.min(len as u64 / 2)) as usize),
        }
    }
    f
}

/// The checksum-valid commit blocks from `start`, at 8-byte steps below `end`, wholly below `limit`, by a plain search.
fn plain_commits(f: &[u8], start: u64, end: u64, limit: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut o = start as usize;
    while (o as u64) < end && o + 64 <= limit as usize {
        let h = &f[o..o + 16];
        let ok = h[0] == block_type::COMMIT
            && u32::from_le_bytes(h[4..8].try_into().unwrap()) == 48
            && u32::from_le_bytes(h[12..16].try_into().unwrap()) == crc32c(&h[..12])
            && u32::from_le_bytes(h[8..12].try_into().unwrap()) == crc32c(&f[o + 16..o + 64])
            && Commit::decode(&f[o + 16..o + 64]).is_ok();
        if ok {
            out.push(o as u64);
            o += 64;
        } else {
            o += 8;
        }
    }
    out
}

fn summary(r: &Result<ScanReport<'_>, FormatError>) -> String {
    match r {
        Err(e) => format!("Err {e:?}"),
        Ok(r) => format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}",
            r.header,
            r.batches
                .iter()
                .map(|b| (b.offset, b.commit, b.blocks.len()))
                .collect::<Vec<_>>(),
            r.regions,
            r.tail,
            r.seal
        ),
    }
}

/// A scan of `f` that keeps the content its checks decode, as recovery and the readers run it (Q-990): its findings,
/// with each batch's content checked against [`BatchContent::decode`], and its counts.
fn kept(f: &[u8], limit: ScanLimit) -> (String, ScanStats) {
    let Ok(s) = Scanner::new(f, &ID, limit) else {
        return (String::new(), ScanStats::default());
    };
    let mut s = s.keeping_content();
    let mut found = Vec::new();
    while let Some(x) = s.next() {
        if let Ok(Found::Batch(b)) = &x {
            assert_eq!(
                s.take_content().ok_or(()),
                BatchContent::decode(b).map_err(|_| ()),
                "{limit:?}: kept content at {}",
                b.offset
            );
        }
        found.push(format!("{x:?}"));
    }
    (format!("{found:?}"), s.stats())
}

/// The scan of `f` to its end and to `mark` keeps the bound and gives the walked chains' report and charges, and the
/// reference scan's report when `reference`, and below 1 MiB that of chain ends found in small blocks; a scan keeping
/// content finds and counts what one without does; a torn tail's commit blocks are a plain search's.
fn check(f: &[u8], mark: u64, reference: bool) {
    let len = f.len() as u64;
    for limit in [ScanLimit::End, ScanLimit::Mark(mark)] {
        let (new, stats) = scan_with_stats(f, &ID, limit);
        assert!(stats.within_bound(len), "{limit:?}: {stats:?}");
        let mut plain = Scanner::new(f, &ID, limit).ok();
        let plain_found = plain.as_mut().map_or(String::new(), |s| {
            format!(
                "{:?}",
                s.by_ref().map(|x| format!("{x:?}")).collect::<Vec<_>>()
            )
        });
        let plain_stats = plain.map_or(ScanStats::default(), |s| s.stats());
        assert_eq!(
            kept(f, limit),
            (plain_found, plain_stats),
            "{limit:?}: kept"
        );
        let new_s = summary(&new);
        let (walked, walked_stats) = legacy::scan_with_walked_links(f, &ID, limit);
        assert_eq!(new_s, summary(&walked), "{limit:?}: walked");
        assert_eq!(
            stats.failed_bytes_peak, walked_stats.failed_bytes_peak,
            "{limit:?}: the walked chains' charges"
        );
        if reference {
            assert_eq!(
                new_s,
                summary(&legacy::scan(f, &ID, limit).0),
                "{limit:?}: reference"
            );
        }
        if len < 1 << 20 {
            for span in [8, 40, 4_096] {
                let s = summary(&legacy::scan_with_span(f, &ID, limit, span).0);
                assert_eq!(new_s, s, "{limit:?}: span {span}");
            }
        }
        if let Ok(r) = &new
            && let Some(Tail::Torn {
                start,
                end,
                commits,
                ..
            }) = &r.tail
        {
            let got: Vec<u64> = commits.iter().map(|c| c.offset).collect();
            assert_eq!(
                got,
                plain_commits(f, *start, *end, len),
                "{limit:?}: tail commits"
            );
        }
    }
    if reference {
        let ignoring = summary(&legacy::scan_ignoring_seal(f, &ID, false).0);
        assert_eq!(
            ignoring,
            summary(&legacy::scan_ignoring_seal(f, &ID, true).0),
            "ignoring the seal"
        );
    }
}

/// A damaged file whose headers at `x` the chain ends read and find valid, and a copy of those bytes exactly a ring
/// (16 MiB) above with a bit of each header broken, in the torn tail: none of the copies is a header.
fn lap_file(r: &mut R) -> Vec<u8> {
    let ring = 16usize << 20;
    let mut s = SegmentImage::create_ahead(ID, T0, 4_096).unwrap();
    s.take_into_use(1).unwrap();
    let mut f = s.into_bytes();
    f.truncate(HEADER_LEN);
    f.extend_from_slice(&[0xFF; 8]);
    let n = r.below(100_000) as usize;
    let pre = junk(r, n, f.len() as u64, 1 << 20);
    f.extend_from_slice(&pre);
    let x = f.len();
    let n = 2_000 + r.below(60_000) as usize;
    let a = junk(r, n, x as u64, 1 << 20);
    f.extend_from_slice(&a);
    let cycle = junk(r, 200_000, f.len() as u64, 1 << 20);
    while f.len() < x + ring {
        let n = cycle.len().min(x + ring - f.len());
        f.extend_from_slice(&cycle[..n]);
    }
    let mut copy = a.clone();
    let mut q = 0;
    while q + 16 <= copy.len() {
        if BlockHeader::decode(copy[q..q + 16].try_into().unwrap()).is_some() && !r.chance(10) {
            let i = if r.chance(70) {
                12 + r.below(4) as usize
            } else {
                [0, 1, 4, 5][r.below(4) as usize]
            };
            copy[q + i] ^= 1 << r.below(8);
            q += 16;
        } else {
            q += 8;
        }
    }
    f.extend_from_slice(&copy);
    if r.chance(50) {
        f.resize(f.len() + 8 * r.below(600) as usize, 0);
    }
    f
}

/// A batch of `total` bytes at the end of `f`: a facts block and a skippable block of filler.
fn filler(f: &mut Vec<u8>, seq: u64, total: usize) {
    let first = facts_body(seq, T0, &[(1, T0, 0.0)]);
    let n = total - 64 - 32 - first.len().next_multiple_of(8);
    let blocks = vec![
        (block_type::FACTS, 0, first),
        (0x50, FLAG_SKIPPABLE, vec![0x11; n]),
    ];
    let at = f.len() as u64;
    f.extend_from_slice(&raw_batch(&ID, at, &blocks, seq, seq, T0));
}

/// After damage, a valid header at every 8 bytes from `x`, which the chain ends read; kept batches up to a ring
/// (16 MiB) above one of those headers, with damage before the last of them or none; and there a batch whose first
/// header fails its checksum by one bit: a boundary read, which must not take the header as valid.
fn boundary_a_ring_above(r: &mut R) -> (Vec<u8>, usize) {
    let ring = 16usize << 20;
    let mut s = SegmentImage::create_ahead(ID, T0, 4_096).unwrap();
    s.take_into_use(1).unwrap();
    let mut f = s.into_bytes();
    f.truncate(HEADER_LEN);
    f.extend_from_slice(&[0xFF; 8]);
    let x = f.len();
    let m = 2_048;
    f.resize(x + 8 * m + 16, 0);
    f[x] = 0x50;
    f[x + 1] = 1;
    f[x + 4] = 64;
    for i in 0..m {
        let q = x + 8 * i;
        loop {
            let w = r.next() as u32;
            f[q + 8..q + 12].copy_from_slice(&[0x50, 1, w as u8, (w >> 8) as u8]);
            let c = crc32c(&f[q..q + 12]);
            if c <= 1 << 20 {
                f[q + 12..q + 16].copy_from_slice(&c.to_le_bytes());
                break;
            }
        }
    }
    let t = x + ring + 8 * r.below(m as u64 - 8) as usize;
    let small = facts_batch(3, T0, &[(1, T0, 1.0)]);
    let damage_before = r.chance(50);
    let slack = if damage_before {
        4_096 + small.data_len() as usize + 64
    } else {
        0
    };
    filler(&mut f, 1, 5_600_000);
    filler(&mut f, 2, 5_600_000);
    let rest = t - slack - f.len();
    filler(&mut f, 3, rest);
    let mut seq = 4;
    if damage_before {
        f.resize(f.len() + 4_096, 0xFF);
        let at = f.len() as u64;
        f.extend_from_slice(
            &facts_batch(seq, T0, &[(1, T0, 1.0)])
                .encode(&ID, at, T0)
                .unwrap(),
        );
        seq += 1;
    }
    assert_eq!(f.len(), t);
    for i in 0..4 {
        let at = f.len();
        f.extend_from_slice(
            &facts_batch(seq, T0, &[(1, T0, 1.0)])
                .encode(&ID, at as u64, T0)
                .unwrap(),
        );
        if i == 0 {
            f[at + 12 + r.below(4) as usize] ^= 1 << r.below(8);
        }
        seq += 1;
    }
    f.resize(f.len() + 8 * r.below(64) as usize, 0);
    (f, t)
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn crafted_and_damaged_segments_give_the_reports_of_the_reference_scan(seed in any::<u64>(), kind in 0u64..4) {
        let mut r = R(seed | 1, [1, 5, 400, 2_000][kind as usize]);
        r.next();
        let target = 200 + r.below(if seed % 10 == 0 { 300_000 } else { 20_000 }) as usize;
        let f = build(&mut r, target, false);
        let mark = r.below(f.len() as u64 + 1) & !7;
        check(&f, mark, true);
    }
}

#[test]
fn segments_longer_than_the_rings_give_the_reports_of_the_walked_chains() {
    for case in 0..2u64 {
        let mut r = R(0xD1B5_4A32_D192_ED03 ^ (case + 1), [1, 400][case as usize]);
        r.next();
        let target = (17 << 20) + r.below(20 << 20) as usize;
        let f = build(&mut r, target, true);
        let mark = r.below(f.len() as u64 + 1) & !7;
        check(&f, mark, false);
    }
}

#[test]
fn a_broken_copy_a_ring_above_valid_headers_gives_the_reports_of_the_reference_scan() {
    for case in 0..2u64 {
        let mut r = R(0xA24B_AED4_963E_E407 ^ (case + 1), [1, 5][case as usize]);
        r.next();
        let f = lap_file(&mut r);
        let mark = r.below(f.len() as u64 + 1) & !7;
        check(&f, mark, true);
    }
}

#[test]
fn a_boundary_read_a_ring_above_a_valid_header_gives_the_reports_of_the_reference_scan() {
    for case in 0..4u64 {
        let mut r = R(0x9FB2_1C65_1E98_DF25 ^ (case + 1), 1);
        r.next();
        let (f, t) = boundary_a_ring_above(&mut r);
        check(&f, t as u64, true);
        let rep = scan_with_stats(&f, &ID, ScanLimit::End).0.unwrap();
        assert!(rep.batches.iter().all(|b| b.offset != t as u64));
        assert!(rep.batches.len() >= 6, "{}", rep.batches.len());
    }
}

/// An invalid header, a damaged word, then a torn tail whose every 8-byte step reads as a commit block header of the
/// commit length and fails its checksum.
fn commit_like_steps(len: usize) -> Vec<u8> {
    let mut f = vec![0u8; HEADER_LEN + 8];
    f[HEADER_LEN..].fill(0xFF);
    while f.len() < len {
        f.extend_from_slice(&[block_type::COMMIT, 0, 0, 0, COMMIT_LEN as u8, 0, 0, 0]);
    }
    f
}

#[test]
fn a_torn_tail_of_failing_commit_like_steps_takes_two_header_checksums_a_step_and_one_to_a_mark() {
    let f = commit_like_steps(1 << 20);
    let steps = (f.len() - HEADER_LEN) as u64 / 8;
    check(&f, f.len() as u64, true);
    let (r, end) = scan_with_stats(&f, &ID, ScanLimit::End);
    assert!(matches!(r.unwrap().tail, Some(Tail::Torn { commits, .. }) if commits.is_empty()));
    let (_, mark) = scan_with_stats(&f, &ID, ScanLimit::Mark(f.len() as u64));
    // The chain ends checksum each step once; the tail's commit search, absent below a mark, once more.
    assert!(
        end.header_checksums <= 2 * steps,
        "{end:?} for {steps} steps"
    );
    assert!(mark.header_checksums <= steps, "{mark:?} for {steps} steps");
    assert_eq!(
        (end.header_checksums, mark.header_checksums),
        (262_120, 131_064)
    );
}
