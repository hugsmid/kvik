//! Crafted files against the budget of failed full reads (`format.md` A, common rules: scan bound; Q-110): nested
//! facts images sharing their columns, and manifest records sharing one field list.

mod support;

use std::time::{Duration, Instant};

use kvik_log::block::{BlockHeader, block_type};
use kvik_log::body::Commit;
use kvik_log::crc::{crc32c, crc32c_shift};
use kvik_log::header::{FileHeader, HEADER_LEN, kind};
use kvik_log::manifest::{
    ManifestError, RECORD_EMPTY, RECORD_LOST_RANGE, encode_record, read_manifest_with_stats,
    read_records_with_stats,
};
use kvik_log::scan::{FAILED_READ_BUDGET as BUDGET, ScanLimit, Tail, legacy, scan_with_stats};
use support::*;

/// 8 MiB: the window of a log segment.
const W: u64 = 8 << 20;
/// 16 MiB: the window of the manifest.
const MW: u64 = 16 << 20;
/// Header, prefix and the five-entry directory of a crafted facts image.
const IMAGE: u64 = 104;
/// A smoke ceiling for one crafted file on a slow shared machine; the machine-independent pin is the body bytes that
/// failed reads check per window.
const CEILING: Duration = Duration::from_secs(5);
/// Rows of a full crafted facts image.
const ROWS: u64 = 65_536;

/// The budget, plus the failed read that reached it (two batches in a segment, a body in the manifest).
fn within_budget(peak: u64) -> bool {
    (BUDGET..=BUDGET + 2 * W).contains(&peak)
}

fn rle_run(n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut run = n;
    loop {
        let b = (run & 0x7F) as u8;
        run >>= 7;
        if run == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
    out.push(5);
    out
}

fn alias_batch(seq: u64, at: u64) -> Vec<u8> {
    entries_batch(seq, T0, &[alias(1, "a")])
        .encode(&ID, at, 0)
        .unwrap()
}

/// Appends `m` nested facts images, image `i` at `base + stride · i`, all pointing at the same columns of `n` text
/// rows of 255 bytes, each running to its own commit block, the commits in reverse after the columns. Each passes
/// its header, framing, checksum and body checks and fails on its sequences. With `between`, a batch of one alias
/// entry follows each image's directory, so each image after the first is met by a boundary read. Returns the
/// sequence after the last alias batch.
fn crafted(f: &mut Vec<u8>, m: u64, n: u64, between: bool, mut seq: u64) -> u64 {
    let stride = IMAGE
        + if between {
            alias_batch(1, 0).len() as u64
        } else {
            0
        };
    let base = f.len() as u64;
    let series = base + stride * m;
    let ts = series + 4 * n;
    let vt = ts + 8 * n;
    let value = vt + 8;
    let heap = value + 8 * n;
    let e = heap + 256;
    f.resize((e + 64 * m) as usize, 0);
    put(f, vt, &rle_run(n));
    for r in 0..n {
        put(f, value + 8 * r, &(255u64 << 32).to_le_bytes());
    }
    put(f, heap, "€".repeat(85).as_bytes());
    if between {
        for i in 0..m {
            let at = base + stride * i + IMAGE;
            put(f, at, &alias_batch(seq, at));
            seq += 1;
        }
    }
    let mut inner: Option<u32> = None;
    for i in (0..m).rev() {
        let o = base + stride * i;
        let body = o + 16;
        let ci = e + 64 * (m - 1 - i);
        put(f, body, &5u64.to_le_bytes());
        put(f, body + 16, &(n as u32).to_le_bytes());
        f[body as usize + 20] = 5;
        f[body as usize + 21] = 12;
        let dir = [
            (1u8, 0u8, series, 4 * n),
            (2, 0, ts, 8 * n),
            (5, 1, vt, rle_run(n).len() as u64),
            (6, 0, value, 8 * n),
            (7, 0, heap, 255),
        ];
        for (k, (id, enc, at, len)) in dir.iter().enumerate() {
            let d = (body + 24 + 12 * k as u64) as usize;
            f[d] = *id;
            f[d + 1] = *enc;
            put(f, d as u64 + 4, &((at - body) as u32).to_le_bytes());
            put(f, d as u64 + 8, &(*len as u32).to_le_bytes());
        }
        // Body i: its own stride up to body i + 1, then body i + 1, then commit block i + 1.
        let crc = match inner {
            None => crc32c(&f[body as usize..ci as usize]),
            Some(c) => {
                let head = crc32c(&f[body as usize..(body + stride) as usize]);
                let with_next = crc32c_shift(head, ci - 64 - (body + stride)) ^ c;
                crc32c_shift(with_next, 64) ^ crc32c(&f[(ci - 64) as usize..ci as usize])
            }
        };
        inner = Some(crc);
        let h = BlockHeader {
            block_type: block_type::FACTS,
            flags: 0,
            body_len: (ci - body) as u32,
            body_crc32c: crc,
        }
        .encode();
        put(f, o, &h);
        let mut d = Vec::new();
        d.extend_from_slice(&ID.store_id);
        d.extend_from_slice(&ID.file_id.to_le_bytes());
        d.extend_from_slice(&o.to_le_bytes());
        d.extend_from_slice(&h[..12]);
        let commit = Commit {
            generation: ID.generation,
            block_count: 1,
            first_sequence: 7,
            last_sequence: 7 + n - 1,
            commit_time: 0,
            batch_bytes: ci - o,
            batch_crc32c: crc32c(&d),
        }
        .encode();
        let ch = BlockHeader {
            block_type: block_type::COMMIT,
            flags: 0,
            body_len: 48,
            body_crc32c: crc32c(&commit),
        }
        .encode();
        put(f, ci, &ch);
        put(f, ci + 16, &commit);
    }
    seq
}

/// An invalid header and a damaged word, so the scan resynchronises from the start.
fn damaged_start() -> Vec<u8> {
    let mut f = vec![0u8; HEADER_LEN + 8];
    f[HEADER_LEN..].fill(0xFF);
    f
}

fn facts_at(f: &mut Vec<u8>, seq: u64) -> u64 {
    let at = f.len() as u64;
    let b = facts_batch(seq, T0, &[(1, T0, 1.0)])
        .encode(&ID, at, 0)
        .unwrap();
    f.extend_from_slice(&b);
    at
}

#[test]
fn a_crafted_window_has_the_rest_passed_as_damaged_once_failed_reads_use_its_budget() {
    let mut f = damaged_start();
    crafted(&mut f, 64, ROWS, false, 1);
    let inside = facts_at(&mut f, 50);
    f.resize(W as usize, 0);
    let after = facts_at(&mut f, 60);
    let len = f.len() as u64;
    let (r, stats) = scan_with_stats(&f, &ID, ScanLimit::End);
    let r = r.unwrap();
    assert!(within_budget(stats.failed_bytes_peak), "{stats:?}");
    assert!(stats.within_bound(len), "{stats:?} for {len} bytes");
    // The batch in the passed part of the window is reported with the region; the next window is read.
    let offsets: Vec<u64> = r.batches.iter().map(|b| b.offset).collect();
    assert_eq!(offsets, [after]);
    assert_eq!(r.regions.len(), 1);
    assert_eq!(
        (r.regions[0].start, r.regions[0].end),
        (HEADER_LEN as u64, after)
    );
    assert_eq!(r.regions[0].first_sequence_after, Some(60));
    // Without the budget every image is read in full and the batch inside the window is kept.
    let (old, old_stats) = legacy::scan(&f, &ID, ScanLimit::End);
    let old: Vec<u64> = old.unwrap().batches.iter().map(|b| b.offset).collect();
    assert_eq!(old, [inside, after]);
    assert!(old_stats.failed_bytes_peak > 4 * BUDGET, "{old_stats:?}");
}

#[test]
fn boundary_reads_that_fail_after_a_body_check_are_charged_to_the_window() {
    let mut f = damaged_start();
    crafted(&mut f, 64, ROWS, true, 1);
    let len = f.len() as u64;
    let (r, stats) = scan_with_stats(&f, &ID, ScanLimit::End);
    let r = r.unwrap();
    assert!(within_budget(stats.failed_bytes_peak), "{stats:?}");
    assert!(stats.within_bound(len), "{stats:?} for {len} bytes");
    // One failed read by resynchronisation, then a boundary read after each kept alias batch until the budget.
    let kept = r.batches.len();
    assert!((1..16).contains(&kept), "{kept} alias batches kept");
    assert!(matches!(r.tail, Some(Tail::Torn { .. })));
    let (old, _) = legacy::scan(&f, &ID, ScanLimit::End);
    assert_eq!(old.unwrap().batches.len(), 64);
}

#[test]
fn full_crafted_windows_stay_within_the_budget_and_a_smoke_ceiling() {
    for between in [false, true] {
        let mut f = damaged_start();
        let m = if between { 20_000 } else { 42_000 };
        let seq = crafted(&mut f, m, ROWS, between, 1);
        assert!(f.len() as u64 <= W);
        f.resize(W as usize, 0);
        crafted(&mut f, m, ROWS, between, seq);
        let len = f.len() as u64;
        let t = Instant::now();
        let (r, stats) = scan_with_stats(&f, &ID, ScanLimit::End);
        let elapsed = t.elapsed();
        r.unwrap();
        assert!(within_budget(stats.failed_bytes_peak), "{stats:?}");
        assert!(stats.within_bound(len), "{stats:?} for {len} bytes");
        assert!(elapsed < CEILING, "{elapsed:?} for {len} bytes");
    }
}

/// A manifest whose first record is damaged, then `m` records of type `t` sharing one list of `g` empty fields of id
/// `field`: record `k` starts inside a field of every record before it. Each has a valid checksum and no field 1.
fn crafted_manifest(m: u64, g: u64, t: u8, field: u16) -> Vec<u8> {
    let f = FileHeader::new(kind::MANIFEST, 0, 1, 0, [9; 16])
        .encode()
        .to_vec();
    crafted_after(f, 1_000, (m, g, t, field), false)
}

/// [`crafted_manifest`]'s damaged record and crafted records after `f`, the crafted epochs from `epoch`; with
/// `overrun` the shared list's last field runs past the bodies, so no record's field list parses.
fn crafted_after(mut f: Vec<u8>, epoch: u64, shape: (u64, u64, u8, u16), overrun: bool) -> Vec<u8> {
    let (m, g, t, field) = shape;
    f.extend_from_slice(&[0xFF; 8]);
    let base = f.len() as u64;
    let e = (base + 48 * m + 4 * g).div_ceil(8) * 8;
    f.resize(e as usize, 0);
    let shared = base + 48 * m - 8;
    for i in 0..(e - shared) / 4 {
        put(&mut f, shared + 4 * i, &field.to_le_bytes());
    }
    if overrun {
        put(&mut f, e - 2, &4u16.to_le_bytes());
    }
    let mut inner: Option<u32> = None;
    for k in (0..m).rev() {
        let p = base + 48 * k;
        let body = p + 16;
        put(&mut f, body, &(epoch + k).to_le_bytes());
        if k > 0 {
            put(&mut f, p - 8, &field.to_le_bytes());
            put(&mut f, p - 6, &44u16.to_le_bytes());
        }
        let crc = match inner {
            None => crc32c(&f[body as usize..e as usize]),
            Some(c) => {
                crc32c_shift(
                    crc32c(&f[body as usize..(body + 48) as usize]),
                    e - body - 48,
                ) ^ c
            }
        };
        inner = Some(crc);
        let h = BlockHeader {
            block_type: t,
            flags: 0,
            body_len: (e - body) as u32,
            body_crc32c: crc,
        }
        .encode();
        put(&mut f, p, &h);
    }
    f
}

#[test]
fn a_crafted_manifest_search_passes_the_rest_of_a_window_once_failed_reads_use_its_budget() {
    let mut f = crafted_manifest(2_000, 100_000, RECORD_LOST_RANGE, 0x8001);
    f.extend(encode_record(RECORD_EMPTY, 1 << 40, 0, 0, None));
    let (r, stats) = read_manifest_with_stats(&f, &[9; 16]);
    assert!(within_budget(stats.failed_bytes_peak), "{stats:?}");
    assert!(stats.within_bound(f.len() as u64), "{stats:?}");
    // The record in the passed part of the window is not found: the file reads as a torn tail (Q-650).
    let r = r.unwrap();
    assert!(r.records.is_empty() && r.torn);
    // A record in the next window is found, so the damaged record and the window are reported corrupt.
    f.resize(MW as usize, 0);
    f.extend(encode_record(RECORD_EMPTY, 1 << 41, 0, 0, None));
    let (r, stats) = read_manifest_with_stats(&f, &[9; 16]);
    assert_eq!(r, Err(ManifestError::Corrupt(HEADER_LEN as u64)));
    assert!(within_budget(stats.failed_bytes_peak), "{stats:?}");
}

/// The failed reads' charges and the search's probes name the same window when a reader parses only a tail from a
/// base that is not a multiple of the window: records whose field list does not parse, and lost ranges that do not
/// decode.
#[test]
fn a_crafted_manifest_tail_read_from_inside_a_window_charges_the_window_it_probes() {
    let mut prefix = FileHeader::new(kind::MANIFEST, 0, 1, 0, [9; 16])
        .encode()
        .to_vec();
    let mut epoch = 0;
    while (prefix.len() as u64) < MW + MW / 2 {
        epoch += 1;
        prefix.extend(encode_record(RECORD_EMPTY, epoch, 0, 0, None));
    }
    let base = prefix.len() as u64;
    assert!(!base.is_multiple_of(MW));
    for overrun in [true, false] {
        let shape = (2_000, 100_000, RECORD_LOST_RANGE, 0x8001);
        let f = crafted_after(prefix.clone(), epoch + 1, shape, overrun);
        let (whole, whole_stats) = read_manifest_with_stats(&f, &[9; 16]);
        let whole = whole.unwrap();
        assert_eq!((whole.records.len() as u64, whole.valid_end), (epoch, base));
        let (tail, stats) = read_records_with_stats(&f[base as usize..], base, epoch);
        assert_eq!(tail, Ok((Vec::new(), base)));
        assert!(
            within_budget(stats.failed_bytes_peak),
            "{overrun}: {stats:?}"
        );
        assert_eq!(stats.failed_bytes_peak, whole_stats.failed_bytes_peak);
    }
}

#[test]
fn a_full_crafted_manifest_window_stays_within_the_budget_and_a_smoke_ceiling() {
    let f = crafted_manifest(20_000, 3_900_000, RECORD_LOST_RANGE, 201);
    let len = f.len() as u64;
    assert!(len <= MW);
    let t = Instant::now();
    let (r, stats) = read_manifest_with_stats(&f, &[9; 16]);
    let elapsed = t.elapsed();
    assert!(r.unwrap().torn);
    assert!(within_budget(stats.failed_bytes_peak), "{stats:?}");
    assert!(stats.within_bound(len), "{stats:?} for {len} bytes");
    assert!(elapsed < CEILING, "{elapsed:?} for {len} bytes");
}
