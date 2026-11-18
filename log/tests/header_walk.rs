//! Crafted block headers against the time of the header walk (`format.md` A, common rules: scan bound; Q-873): a valid
//! header at every 8 bytes whose body checksum does not hold, and commit blocks naming such headers as batch starts.

mod support;

use std::time::Instant;

use kvik_log::block::{BlockHeader, FLAG_SKIPPABLE, block_type, encode_block};
use kvik_log::body::{COMMIT_LEN, Commit};
use kvik_log::crc::crc32c;
use kvik_log::header::{FileHeader, HEADER_LEN, kind};
use kvik_log::manifest::read_manifest_with_stats;
use kvik_log::scan::{ScanLimit, ScanStats, Tail, legacy, scan_with_stats};
use support::*;

/// 8 MiB: a batch's data and a segment window.
const W: u64 = 8 << 20;

/// A generator of the free bytes of each header.
struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }
}

/// The low word of the header at `p`, whose first 8 bytes are set, chosen so that the header checksum, which is the
/// next header's `body_len`, lies in `lens`: with `t`, the next header's type is `t`.
fn chain_word(f: &mut [u8], p: usize, t: Option<u8>, lens: (u32, u32), x: &mut Lcg) -> u32 {
    loop {
        let mut w = (x.next() >> 8).to_le_bytes();
        if let Some(t) = t {
            w = [t, 0, w[0], w[1]];
        }
        f[p + 8..p + 12].copy_from_slice(&w);
        let c = crc32c(&f[p..p + 12]);
        if f[p + 8] != 0 && (lens.0..=lens.1).contains(&c) {
            f[p + 12..p + 16].copy_from_slice(&c.to_le_bytes());
            return c;
        }
    }
}

/// From `s`, a valid block header at every 8 bytes up to 2 MiB before the end, each claiming a
/// body of at least 4 KiB that fits and whose checksum does not hold; every type `t` if given, else any.
fn headers_everywhere(mut f: Vec<u8>, len: usize, t: Option<u8>) -> Vec<u8> {
    let s = f.len().next_multiple_of(8);
    f.resize(len, 0);
    f[s] = t.unwrap_or(block_type::FACTS);
    f[s + 4..s + 8].copy_from_slice(&4096u32.to_le_bytes());
    let mut x = Lcg(1);
    let mut p = s;
    while p + (2 << 20) < len {
        let room = ((len - p - 24) as u32).min(W as u32);
        chain_word(&mut f, p, t, (4096, room), &mut x);
        p += 8;
    }
    f
}

/// From `s`, periods of 64 bytes: a commit block header at 8, whose `batch_bytes` (at 56) names the start of a data
/// header up to a batch before it; data headers at 16 to 40, each claiming a body that runs anywhere up to a batch on;
/// a data header with an empty body at 56, which leads to the next commit; and bytes that are no header at 0 and 48.
/// Every start a commit names is a data header whose chain leaves its block, so each candidate follows its chain.
fn named_walks(mut f: Vec<u8>, len: usize) -> Vec<u8> {
    let s = f.len().next_multiple_of(64);
    f.resize(len, 0);
    let mut x = Lcg(7);
    let mut b = s;
    let lens = (16, W as u32 - 32);
    while b + 256 < len {
        let commit = b + 8;
        f[commit] = block_type::COMMIT;
        f[commit + 4..commit + 8].copy_from_slice(&(COMMIT_LEN as u32).to_le_bytes());
        for p in [commit, b + 16, b + 24, b + 32] {
            chain_word(&mut f, p, Some(block_type::FACTS), lens, &mut x);
        }
        // The data header at 40 is valid; its checksum is the length of the header at 48, which is not one.
        f[b + 48..b + 52].copy_from_slice(&x.next().to_le_bytes());
        let c = crc32c(&f[b + 40..b + 52]);
        f[b + 52..b + 56].copy_from_slice(&c.to_le_bytes());
        // A start in an earlier period, at most a batch back, whose distance gives the header at 56 a data type.
        let periods = ((b - s) as u64 / 64).min((W - 128) / 64);
        let named = (0..64).find_map(|_| {
            let back = 64 * (1 + u64::from(x.next()) % periods.max(1));
            let o = (commit as u64 - back) + 8 * (1 + u64::from(x.next() % 4));
            let bb = commit as u64 - o;
            (periods > 0 && (1..=0x7F).contains(&(bb & 0xFF))).then_some(o)
        });
        let named = named.unwrap_or(commit as u64 - 0x48);
        f[b + 56..b + 64].copy_from_slice(&(commit as u64 - named).to_le_bytes());
        // The header at 56 has an empty body: its checksum is the next period's first 4 bytes after 4 free ones.
        f[b + 64..b + 68].copy_from_slice(&x.next().to_le_bytes());
        let c = crc32c(&f[b + 56..b + 68]);
        f[b + 68..b + 72].copy_from_slice(&c.to_le_bytes());
        b += 64;
    }
    f
}

/// From `s`, groups of `n` skippable block headers at 16-byte steps, each claiming a body with a wrong checksum that
/// runs to its own commit block, the commits after the group in reverse: every header is a candidate whose framing and
/// `batch_crc32c` hold, read in full and rejected on its body checksum, so it checks no body.
fn framed_candidates(mut f: Vec<u8>, len: usize, n: usize) -> Vec<u8> {
    let mut base = f.len().next_multiple_of(16);
    f.resize(len, 0);
    while base + 80 * n <= len {
        let e = base + 16 * n;
        for i in 0..n {
            let (o, ci) = (base + 16 * i, e + 64 * (n - 1 - i));
            let h = BlockHeader {
                block_type: 0x90,
                flags: FLAG_SKIPPABLE,
                body_len: (ci - o - 16) as u32,
                body_crc32c: 0xDEAD_BEEF,
            }
            .encode();
            f[o..o + 16].copy_from_slice(&h);
            let mut d = ID.store_id.to_vec();
            d.extend_from_slice(&ID.file_id.to_le_bytes());
            d.extend_from_slice(&(o as u64).to_le_bytes());
            d.extend_from_slice(&h[..12]);
            let commit = Commit {
                generation: ID.generation,
                block_count: 1,
                first_sequence: 1,
                last_sequence: 1,
                commit_time: 0,
                batch_bytes: (ci - o) as u64,
                batch_crc32c: crc32c(&d),
            };
            let mut out = Vec::new();
            encode_block(block_type::COMMIT, 0, &commit.encode(), &mut out).unwrap();
            f[ci..ci + 64].copy_from_slice(&out);
        }
        base += 80 * n;
    }
    f
}

/// From `s`, candidates of `n` empty skippable blocks and a commit block whose framing and `batch_crc32c` hold, the
/// last block's body checksum wrong: each is read in full to its last block and checks no body.
fn skippable_chains(mut f: Vec<u8>, len: usize, n: usize) -> Vec<u8> {
    let mut o = f.len().next_multiple_of(16);
    f.resize(len, 0);
    while o + 16 * n + 64 <= len {
        let mut d = ID.store_id.to_vec();
        d.extend_from_slice(&ID.file_id.to_le_bytes());
        d.extend_from_slice(&(o as u64).to_le_bytes());
        for i in 0..n {
            let h = BlockHeader {
                block_type: 0x90,
                flags: FLAG_SKIPPABLE,
                body_len: 0,
                body_crc32c: u32::from(i + 1 == n),
            }
            .encode();
            f[o + 16 * i..o + 16 * i + 16].copy_from_slice(&h);
            d.extend_from_slice(&h[..12]);
        }
        let ci = o + 16 * n;
        let commit = Commit {
            generation: ID.generation,
            block_count: n as u32,
            first_sequence: 1,
            last_sequence: 1,
            commit_time: 0,
            batch_bytes: (ci - o) as u64,
            batch_crc32c: crc32c(&d),
        };
        let mut out = Vec::new();
        encode_block(block_type::COMMIT, 0, &commit.encode(), &mut out).unwrap();
        f[ci..ci + 64].copy_from_slice(&out);
        o = ci + 64;
    }
    f
}

/// From `s`, complete batches of a facts block and `n` empty skippable blocks, read by boundary reads after damage.
fn skippable_batches(mut f: Vec<u8>, len: usize, n: usize) -> Vec<u8> {
    let mut seq = 1;
    loop {
        let mut b = facts_batch(seq, T0, &[(1, T0, 1.0)]);
        for _ in 0..n {
            b.push_unknown(0x90, FLAG_SKIPPABLE, Vec::new()).unwrap();
        }
        let at = f.len().next_multiple_of(8);
        let bytes = b.encode(&ID, at as u64, T0).unwrap();
        if at + bytes.len() > len {
            break;
        }
        f.resize(at, 0);
        f.extend_from_slice(&bytes);
        seq += 1;
    }
    f.resize(len, 0);
    f
}

/// From `s`, kept batches of one entries block each, a definition of 65,536 state names of two 4-byte characters: the
/// dearest body to check by the byte (Q-870), read after damage.
fn dearest_entries_batches(mut f: Vec<u8>, len: usize) -> Vec<u8> {
    for id in 1u32.. {
        let mut d = definition(id, "s", "");
        d.1[3].1 = vec![3];
        let at = d.1.iter().position(|(f, _)| *f == 14).unwrap();
        let names = (0..65_536u32).map(|k| {
            let c = char::from_u32(0x10000 + k).unwrap();
            (13u16, format!("{c}{c}").into_bytes())
        });
        d.1.splice(at..at, names);
        let at = f.len().next_multiple_of(8);
        let bytes = entries_batch(u64::from(id), T0, &[d])
            .encode(&ID, at as u64, T0)
            .unwrap();
        if at + bytes.len() > len {
            break;
        }
        f.resize(at, 0);
        f.extend_from_slice(&bytes);
    }
    f.resize(len, 0);
    f
}

/// A skippable block with `body`, its body checksum `crc` or the right one.
fn skippable(body: &[u8], crc: Option<u32>) -> Vec<u8> {
    let mut out = BlockHeader {
        block_type: 0x90,
        flags: FLAG_SKIPPABLE,
        body_len: body.len() as u32,
        body_crc32c: crc.unwrap_or_else(|| crc32c(body)),
    }
    .encode()
    .to_vec();
    out.extend_from_slice(body);
    out.resize(out.len().next_multiple_of(8), 0);
    out
}

/// An 8-byte body whose skippable block reads as a plausible header at each of its three 8-byte steps.
fn plausible_body() -> [u8; 8] {
    for x in 1u32.. {
        let mut body = [0u8; 8];
        body[..4].copy_from_slice(&[0x90, 1, (x >> 8) as u8, x as u8]);
        body[4..].copy_from_slice(&(64u32 + 8 * (x >> 16)).to_le_bytes());
        let b = skippable(&body, None);
        let (t, len) = (b[8], u32::from_le_bytes(b[12..16].try_into().unwrap()));
        let seal_or_commit = [
            0,
            block_type::COMMIT,
            block_type::SEGMENT_INDEX,
            block_type::TRAILER,
        ];
        if !seal_or_commit.contains(&t) && u64::from(len) < W - 64 {
            return body;
        }
    }
    unreachable!()
}

/// A batch at the end of `f` of sequence `seq`: a facts block of one row and `n` skippable blocks of `body`.
fn push_batch(f: &mut Vec<u8>, seq: u64, n: usize, body: &[u8]) {
    let mut blocks = vec![(block_type::FACTS, 0, facts_body(seq, T0, &[(1, T0, 1.0)]))];
    blocks.extend((0..n).map(|_| (0x90, FLAG_SKIPPABLE, body.to_vec())));
    let at = f.len() as u64;
    f.extend_from_slice(&raw_batch(&ID, at, &blocks, seq, seq, T0));
}

fn push_one_row(f: &mut Vec<u8>, seq: u64) {
    let at = f.len() as u64;
    let b = facts_batch(seq, T0, &[(1, T0, 1.0)])
        .encode(&ID, at, T0)
        .unwrap();
    f.extend_from_slice(&b);
}

/// A chain of `n` skippable blocks of `body`, the last with a wrong body checksum, and a commit block naming the
/// chain's start whose framing and `batch_crc32c` hold.
fn push_broken_chain(f: &mut Vec<u8>, seq: u64, n: usize, body: &[u8]) {
    let o = f.len();
    let mut d = ID.store_id.to_vec();
    d.extend_from_slice(&ID.file_id.to_le_bytes());
    d.extend_from_slice(&(o as u64).to_le_bytes());
    for i in 0..n {
        let crc = (i + 1 == n).then(|| crc32c(body) ^ 1);
        let b = skippable(body, crc);
        d.extend_from_slice(&b[..12]);
        f.extend_from_slice(&b);
    }
    let commit = Commit {
        generation: ID.generation,
        block_count: n as u32,
        first_sequence: seq,
        last_sequence: seq,
        commit_time: 0,
        batch_bytes: (f.len() - o) as u64,
        batch_crc32c: crc32c(&d),
    };
    encode_block(block_type::COMMIT, 0, &commit.encode(), f).unwrap();
}

/// After damage, units of a kept one-row batch, one skippable block and a chain of `n` blocks of `body`: the boundary
/// read after the kept batch walks the chain and fails, and the chain's start is a candidate. When `kept`, the chain
/// is a batch (a facts block first), which the candidate keeps; else its last block's body checksum is wrong.
fn walked_then_read(len: usize, n: usize, body: &[u8], kept: bool) -> Vec<u8> {
    let mut f = damaged_start();
    let mut seq = 1;
    while f.len() + 256 + (n + 1) * (16 + body.len().next_multiple_of(8)) + 64 <= len {
        push_one_row(&mut f, seq);
        f.extend_from_slice(&skippable(body, None));
        if kept {
            push_batch(&mut f, seq + 1, n, body);
        } else {
            push_broken_chain(&mut f, seq + 1, n, body);
        }
        seq += 2;
    }
    f.resize(len, 0);
    f
}

/// Units of damage, a kept one-row batch when `at_boundary`, and a batch of a facts block and `n` blocks of `body`:
/// the large batch is read at a boundary, or found by resynchronisation, inside the bytes the chain ends read.
fn kept_after_damage(len: usize, n: usize, body: &[u8], at_boundary: bool) -> Vec<u8> {
    let mut f = damaged_start();
    let mut seq = 1;
    while f.len() + 512 + n * (16 + body.len().next_multiple_of(8)) <= len {
        if at_boundary {
            push_one_row(&mut f, seq);
            seq += 1;
        }
        push_batch(&mut f, seq, n, body);
        seq += 1;
        f.extend_from_slice(&[0xFF; 8]);
    }
    f.resize(len, 0);
    f
}

/// Undamaged batches of a facts block and `n` empty skippable blocks: what reporting the blocks costs.
fn undamaged_batches(len: usize, n: usize) -> Vec<u8> {
    let mut f = vec![0u8; HEADER_LEN];
    let mut seq = 1;
    while f.len() + 512 + 16 * n <= len {
        push_batch(&mut f, seq, n, &[]);
        seq += 1;
    }
    f.resize(len, 0);
    f
}

/// After damage, units of a damaged word and a kept batch of one entries block, a definition of 65,536 state names
/// of four ASCII letters placed so that every 8-byte step of the names is a plausible header: each body is checked
/// and each of its steps takes a header checksum.
fn plausible_entries_after_damage(len: usize) -> Vec<u8> {
    let mut f = damaged_start();
    let letters: Vec<u8> = (b'a'..=b'z').chain(b'A'..=b'Z').collect();
    for id in 1u32.. {
        let mut d = definition(id, "sssss", "");
        d.1[3].1 = vec![3];
        let at = d.1.iter().position(|(f, _)| *f == 14).unwrap();
        let names = (0..65_536usize).map(|k| {
            let c = |i: u32| letters[k / letters.len().pow(i) % letters.len()];
            (13u16, vec![c(0), c(1), c(2), c(3)])
        });
        d.1.splice(at..at, names);
        let at = f.len();
        let bytes = entries_batch(u64::from(id), T0, &[d])
            .encode(&ID, at as u64, T0)
            .unwrap();
        if at + bytes.len() + 8 > len {
            break;
        }
        f.extend_from_slice(&bytes);
        f.extend_from_slice(&[0xFF; 8]);
    }
    f.resize(len, 0);
    f
}

/// A batch image at the end of `f` whose framing and `batch_crc32c` hold: `blocks` of a type, flags, body and body
/// checksum (the right one if `None`), and a commit block of sequences `seq`.
fn push_framed(f: &mut Vec<u8>, blocks: &[(u8, u8, Vec<u8>, Option<u32>)], seq: u64) {
    let o = f.len();
    let mut d = ID.store_id.to_vec();
    d.extend_from_slice(&ID.file_id.to_le_bytes());
    d.extend_from_slice(&(o as u64).to_le_bytes());
    for (t, flags, body, crc) in blocks {
        let h = BlockHeader {
            block_type: *t,
            flags: *flags,
            body_len: body.len() as u32,
            body_crc32c: crc.unwrap_or_else(|| crc32c(body)),
        }
        .encode();
        d.extend_from_slice(&h[..12]);
        f.extend_from_slice(&h);
        f.extend_from_slice(body);
        f.resize(f.len().next_multiple_of(8), 0);
    }
    let commit = Commit {
        generation: ID.generation,
        block_count: blocks.len() as u32,
        first_sequence: seq,
        last_sequence: seq,
        commit_time: 0,
        batch_bytes: (f.len() - o) as u64,
        batch_crc32c: crc32c(&d),
    };
    encode_block(block_type::COMMIT, 0, &commit.encode(), f).unwrap();
}

/// After damage, units in windows of their own: a kept one-row batch, a skippable block, and a batch image of a facts
/// block and `n` skippable blocks of 8 bytes whose framing holds, which the boundary read after the kept batch walks and
/// rejects and whose first block is then a candidate. `stop` 0: a last skippable block with a wrong body checksum;
/// 1: a last facts block whose body fails its checks; 2: no last block, and a sequence below the kept one.
fn charged_candidates(len: usize, n: usize, stop: u8) -> Vec<u8> {
    let mut f = damaged_start();
    let mut seq = 2;
    while f.len() + 2 * W as usize <= len {
        f.resize(f.len().next_multiple_of(W as usize) + 4_096, 0xFF);
        push_one_row(&mut f, seq);
        f.extend_from_slice(&skippable(&[], None));
        let mut blocks = vec![(
            block_type::FACTS,
            0,
            facts_body(seq + 1, T0, &[(1, T0, 1.0)]),
            None,
        )];
        blocks.extend((0..n).map(|_| (0x90, FLAG_SKIPPABLE, vec![7; 8], None)));
        match stop {
            0 => blocks.push((0x90, FLAG_SKIPPABLE, vec![7; 8], Some(1))),
            1 => {
                let mut body = facts_body(seq + 1, T0, &[(1, T0, 1.0)]);
                body[16] = 2;
                blocks.push((block_type::FACTS, 0, body, None));
            }
            _ => {}
        }
        push_framed(&mut f, &blocks, if stop == 2 { 1 } else { seq + 1 });
        seq += 2;
    }
    f.resize(len, 0);
    f
}

/// After damage, commit blocks packed over 512 KiB and, 16 MiB above them (a ring of chain ends), empty skippable
/// blocks over as much, in a torn tail: a commit bit kept from a ring below would send the tail's commit search to
/// read the header of every fourth skippable block again.
fn commits_a_ring_below_blocks() -> (Vec<u8>, u64) {
    let x = 1usize << 20;
    let mut f = damaged_start();
    f.resize(x, 0);
    let mut commits = 0;
    while f.len() < x + (512 << 10) {
        let c = Commit {
            generation: ID.generation,
            block_count: 1,
            first_sequence: 1,
            last_sequence: 1,
            commit_time: 0,
            batch_bytes: 0,
            batch_crc32c: 0,
        };
        encode_block(block_type::COMMIT, 0, &c.encode(), &mut f).unwrap();
        commits += 1;
    }
    f.resize(x + (16 << 20), 0);
    while f.len() < x + (16 << 20) + (512 << 10) {
        f.extend_from_slice(&skippable(&[], None));
    }
    (f, commits)
}

/// The 8-byte steps whose header the chain ends checksum: a type and length that could be part of a batch.
fn plausible_steps(f: &[u8]) -> u64 {
    let mut n = 0;
    for p in (HEADER_LEN..f.len().saturating_sub(15)).step_by(8) {
        let len = u64::from(u32::from_le_bytes(f[p + 4..p + 8].try_into().unwrap()));
        n += u64::from(match f[p] {
            0 | block_type::SEGMENT_INDEX | block_type::TRAILER => false,
            block_type::COMMIT => len == COMMIT_LEN as u64,
            _ => 16 + len.next_multiple_of(8) <= W,
        });
    }
    n
}

fn damaged_start() -> Vec<u8> {
    let mut f = vec![0u8; HEADER_LEN + 8];
    f[HEADER_LEN..].fill(0xFF);
    f
}

/// CPU seconds this thread has used, where the system reports them.
fn cpu() -> f64 {
    std::fs::read_to_string("/proc/thread-self/stat")
        .ok()
        .and_then(|s| {
            let f: Vec<&str> = s.rsplit(')').next()?.split_whitespace().collect();
            Some((f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / 100.0)
        })
        .unwrap_or(0.0)
}

fn timed(name: &str, f: &[u8]) -> ScanStats {
    let (mut best, mut worst, mut best_cpu) = (f64::INFINITY, 0f64, f64::INFINITY);
    let mut stats = ScanStats::default();
    for _ in 0..5 {
        let (t, c) = (Instant::now(), cpu());
        let (r, s) = scan_with_stats(f, &ID, ScanLimit::End);
        let took = t.elapsed().as_secs_f64();
        (best, worst) = (best.min(took), worst.max(took));
        best_cpu = best_cpu.min(cpu() - c);
        r.unwrap();
        stats = s;
    }
    assert!(stats.within_bound(f.len() as u64), "{stats:?}");
    println!(
        "{name}, {} MiB: {best:.3} s, worst {worst:.3} s ({best_cpu:.2} s of CPU), {stats:?}",
        f.len() >> 20
    );
    stats
}

/// The scan agrees with the walked chains, which walk each candidate's chain, and reads each step's header
/// once ahead of the candidates and once more where it starts a commit block, for the tail's commit search.
fn agrees_and_reads_each_header_once(f: &[u8], commits: u64) {
    let len = f.len() as u64;
    for limit in [ScanLimit::End, ScanLimit::Mark(len & !7)] {
        let (r, stats) = scan_with_stats(f, &ID, limit);
        let (walked, _) = legacy::scan_with_walked_links(f, &ID, limit);
        assert_eq!(r, walked, "{limit:?}");
        assert!(stats.within_bound(len), "{stats:?}");
        assert!(
            stats.headers_visited <= len / 8 + 2 * commits + 64,
            "{limit:?}: {stats:?} for {len} bytes"
        );
    }
}

#[test]
fn a_header_at_every_step_has_each_header_read_once() {
    for t in [None, Some(block_type::FACTS), Some(block_type::COMMIT)] {
        let f = headers_everywhere(damaged_start(), 3 << 20, t);
        let valid = (HEADER_LEN..f.len() - 16)
            .step_by(8)
            .filter(|&p| BlockHeader::decode(f[p..p + 16].try_into().unwrap()).is_some())
            .count();
        assert!(valid > 100_000, "{valid}");
        agrees_and_reads_each_header_once(&f, 0);
    }
}

/// Header reads per plausible step on each shape: the chain ends' one, and per block a boundary read's two, a
/// candidate's framing, a read in full of a batch kept or of blocks no boundary read walked.
#[test]
fn files_longer_than_the_ring_take_one_header_checksum_and_a_few_reads_a_plausible_step() {
    let len = 24usize << 20;
    let wide = plausible_body();
    let shapes = [
        (skippable_chains(damaged_start(), len, 4_096), 4),
        (framed_candidates(damaged_start(), len, 1_000), 3),
        (walked_then_read(len, 100_000, &[], false), 4),
        (walked_then_read(len, 100_000, &[], true), 5),
        (walked_then_read(len, 60_000, &wide, false), 2),
        (kept_after_damage(len, 100_000, &[], true), 5),
        (kept_after_damage(len, 100_000, &[], false), 4),
        (skippable_batches(damaged_start(), len, 100_000), 5),
        (headers_everywhere(damaged_start(), 3 << 20, None), 2),
        (named_walks(damaged_start(), 2 << 20), 2),
    ];
    for (f, reads) in &shapes {
        let (r, stats) = scan_with_stats(f, &ID, ScanLimit::End);
        let (walked, _) = legacy::scan_with_walked_links(f, &ID, ScanLimit::End);
        assert_eq!(r, walked);
        assert!(stats.within_bound(f.len() as u64), "{stats:?}");
        let plausible = plausible_steps(f);
        assert!(
            (plausible.saturating_sub(128)..=plausible + 64).contains(&stats.header_checksums),
            "{stats:?}: {plausible} plausible steps"
        );
        assert!(
            stats.headers_visited <= reads * plausible + 64,
            "{stats:?}: {plausible} plausible steps"
        );
    }
}

#[test]
fn commits_naming_data_headers_have_each_header_read_once() {
    let f = named_walks(damaged_start(), 2 << 20);
    agrees_and_reads_each_header_once(&f, f.len() as u64 / 64);
}

#[test]
fn framed_candidates_check_no_body_and_agree_with_the_walked_chains() {
    let f = framed_candidates(damaged_start(), 2 << 20, 4_096);
    let (r, stats) = scan_with_stats(&f, &ID, ScanLimit::End);
    let (walked, _) = legacy::scan_with_walked_links(&f, &ID, ScanLimit::End);
    assert_eq!(r, walked);
    assert_eq!(stats.bytes_body_checked, 0);
    assert!(stats.within_bound(f.len() as u64), "{stats:?}");
}

fn timed_manifest(name: &str, m: &[u8]) {
    let (mut best, mut stats) = (f64::INFINITY, ScanStats::default());
    for _ in 0..5 {
        let t = Instant::now();
        let (_, s) = read_manifest_with_stats(m, &STORE);
        best = best.min(t.elapsed().as_secs_f64());
        stats = s;
    }
    assert!(stats.within_manifest_bound(m.len() as u64), "{stats:?}");
    println!(
        "{name}, {} MiB, start's reader: {best:.3} s, {stats:?}",
        m.len() >> 20
    );
}

/// The crafted header walks against the restart budget, timed (Q-873): the best of five, and the CPU time. Run in a
/// release build, one at a time: `cargo test --release -p kvik-log --test header_walk -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement"]
fn crafted_header_walks_timed() {
    for (what, t, len) in [
        ("any type", None, 16usize << 20),
        ("facts type", Some(block_type::FACTS), 16 << 20),
        ("commit type", Some(block_type::COMMIT), 16 << 20),
        ("any type", None, 64 << 20),
    ] {
        let f = headers_everywhere(damaged_start(), len, t);
        timed(&format!("segment, a header every 8 bytes, {what}"), &f);
    }
    let f = dearest_entries_batches(damaged_start(), 64 << 20);
    timed(
        "segment, kept batches of the dearest entries body after damage",
        &f,
    );
    for n in [1_000, 500_000] {
        let f = skippable_batches(damaged_start(), 64 << 20, n);
        timed(
            &format!("segment, kept batches of {n} empty skippable blocks after damage"),
            &f,
        );
    }
    for n in [1_000, 500_000] {
        let f = skippable_chains(damaged_start(), 64 << 20, n);
        timed(
            &format!("segment, chains of {n} empty skippable blocks read in full"),
            &f,
        );
    }
    for n in [1_000, 100_000] {
        let f = framed_candidates(damaged_start(), 64 << 20, n);
        timed(
            &format!("segment, candidates read in full, groups of {n}"),
            &f,
        );
    }
    let f = named_walks(damaged_start(), 64 << 20);
    timed("segment, commits naming data headers", &f);
    let t = Instant::now();
    let (r, _) = legacy::scan_with_walked_links(&f, &ID, ScanLimit::End);
    r.unwrap();
    println!(
        "  the same, by the walked chains: {:.3} s",
        t.elapsed().as_secs_f64()
    );
    let header = FileHeader::new(kind::MANIFEST, 0, 1, 0, STORE)
        .encode()
        .to_vec();
    for t in [None, Some(kvik_log::manifest::RECORD_LOST_RANGE)] {
        let m = headers_everywhere(header.clone(), 16 << 20, t);
        timed_manifest(&format!("manifest, a header every 8 bytes, type {t:?}"), &m);
    }
}

/// Crafted shapes that stack the passes over one file's own bytes (Q-873, Q-970), timed as
/// [`crafted_header_walks_timed`]: a boundary read that walks a chain and fails, then the chain read as a candidate;
/// batches kept after damage, at a boundary or by resynchronisation; a body checked whose every step takes a header
/// checksum; and undamaged batches of the same blocks, what reporting them costs.
#[test]
#[ignore = "a measurement"]
fn stacked_passes_timed() {
    let len = 64 << 20;
    let wide = plausible_body();
    for (what, body, n) in [
        ("empty", &[][..], 500_000),
        ("empty", &[][..], 4_096),
        ("8-byte plausible", &wide[..], 340_000),
    ] {
        for kept in [false, true] {
            let f = walked_then_read(len, n, body, kept);
            let then = if kept { "keeps" } else { "fails at its last" };
            timed(
                &format!(
                    "a boundary read fails on a chain of {n} {what} blocks the candidate then {then}"
                ),
                &f,
            );
        }
        for at_boundary in [true, false] {
            let f = kept_after_damage(len, n, body, at_boundary);
            let how = if at_boundary {
                "at a boundary"
            } else {
                "by resynchronisation"
            };
            timed(
                &format!("damage, then batches of {n} {what} blocks kept {how}"),
                &f,
            );
        }
    }
    let f = plausible_entries_after_damage(len);
    timed(
        "damage, then batches of state names whose every step is a plausible header",
        &f,
    );
    let f = undamaged_batches(len, 500_000);
    timed("undamaged batches of 500000 empty skippable blocks", &f);
}

#[test]
fn a_candidate_on_a_chain_a_boundary_read_rejected_is_charged_what_reading_it_checks() {
    for stop in 0..3 {
        let f = charged_candidates(5 * W as usize, 1_000, stop);
        let (r, stats) = scan_with_stats(&f, &ID, ScanLimit::End);
        let (walked, walked_stats) = legacy::scan_with_walked_links(&f, &ID, ScanLimit::End);
        assert_eq!(r, walked, "stop {stop}");
        assert_eq!(r.unwrap().regions.len(), 3, "stop {stop}");
        assert_eq!(
            stats.failed_bytes_peak, walked_stats.failed_bytes_peak,
            "stop {stop}"
        );
        assert!(stats.failed_bytes_peak > 0, "stop {stop}");
        assert!(
            stats.bytes_body_checked < walked_stats.bytes_body_checked,
            "stop {stop}"
        );
        // Per block: the chain ends' read, the boundary read's two, and the candidate's framing, or for a batch
        // image whose blocks all pass, its read in full.
        let reads = [4, 4, 5][usize::from(stop)];
        assert!(
            stats.headers_visited <= reads * 3 * 1_003 + 200,
            "stop {stop}: {stats:?}"
        );
    }
}

#[test]
fn the_tail_s_commit_search_reads_no_header_again_for_a_commit_a_ring_below() {
    let (f, commits) = commits_a_ring_below_blocks();
    let (r, stats) = scan_with_stats(&f, &ID, ScanLimit::End);
    let r = r.unwrap();
    assert!(matches!(&r.tail, Some(Tail::Torn { commits: c, .. }) if c.len() as u64 == commits));
    let plausible = plausible_steps(&f);
    assert!(
        stats.headers_visited <= plausible + 2 * commits + 256,
        "{stats:?}: {plausible} plausible steps, {commits} commit blocks"
    );
}
