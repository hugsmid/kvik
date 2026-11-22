//! Golden files of the derived files (`format.md` A.18), committed under `tests/golden-derived/` (see its README).
//! Each body is laid out here field by field from the A.18 tables, independently of the encoders; the tests check
//! that the committed bytes equal it, that each file decodes to what it shows and that encoding that again gives the
//! same bytes. To regenerate after a deliberate format change:
//! `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-log --test directory_golden`.

use std::fs;
use std::path::PathBuf;

use kvik_log::block::encode_block;
use kvik_log::directory::logdir::{Group, GroupEntry, LostEntry, Root, SegmentRecord};
use kvik_log::directory::series::{
    Candidate, CandidateValue, Pending, SeriesDirectory, SeriesEntry, StartCounts,
};
use kvik_log::directory::status::{StatusFile, counter, flag, health, reason};
use kvik_log::directory::{Frame, block_type};
use kvik_log::facts::Quality;
use kvik_log::header::{FileHeader, kind};
use kvik_log::mark::ResetStamp;
use kvik_log::writer::Position;

const STORE: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
const T0: i64 = 1_767_225_600_000_000_000;
const S: i64 = 1_000_000_000;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden-derived")
}

/// Little-endian fields, in order.
#[derive(Default)]
struct Body(Vec<u8>);

impl Body {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i64(&mut self, v: i64) -> &mut Self {
        self.u64(v as u64)
    }
    fn zeros(&mut self, n: usize) -> &mut Self {
        self.0.resize(self.0.len() + n, 0);
        self
    }
    fn pos(&mut self, generation: u32, sequence: u64) -> &mut Self {
        self.u32(generation).u64(sequence)
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
}

fn file(kind: u8, block: u8, file_id: u64, body: &[u8]) -> Vec<u8> {
    let mut out = FileHeader::new(kind, 0, file_id, T0 + 600 * S, STORE)
        .encode()
        .to_vec();
    encode_block(block, 0, body, &mut out).unwrap();
    out
}

fn frame(file_id: u64) -> Frame {
    Frame {
        generation: 0,
        file_id,
        written_at: T0 + 600 * S,
    }
}

fn at(sequence: u64) -> Position {
    Position {
        generation: 0,
        sequence,
    }
}

fn segment(file_id: u64, first: u64, last: u64, has_entries: bool) -> SegmentRecord {
    SegmentRecord {
        file_id,
        generation: 0,
        has_entries,
        first_sequence: first,
        last_sequence: last,
        first_commit_time: T0 + first as i64 * S,
        last_commit_time: T0 + last as i64 * S,
        fact_count: last - first,
        len: 4_096 * file_id,
        file_crc32c: 0xA0B0_C0D0 + file_id as u32,
        sha256: [0x10 + file_id as u8; 32],
    }
}

fn lay_segment(b: &mut Body, s: &SegmentRecord) {
    b.u64(s.file_id)
        .u32(s.generation)
        .u8(u8::from(s.has_entries))
        .zeros(3);
    b.u64(s.first_sequence).u64(s.last_sequence);
    b.i64(s.first_commit_time).i64(s.last_commit_time);
    b.u64(s.fact_count)
        .u64(s.len)
        .u32(s.file_crc32c)
        .zeros(4)
        .bytes(&s.sha256);
}

fn group() -> Group {
    Group {
        number: 0,
        segments: vec![segment(1, 1, 40, true), segment(2, 41, 90, false)],
    }
}

fn group_bytes() -> Vec<u8> {
    let g = group();
    let mut b = Body::default();
    b.u32(0).u32(2).u16(104).zeros(6);
    for s in &g.segments {
        lay_segment(&mut b, s);
    }
    file(kind::LOG_DIRECTORY, block_type::GROUP, 3, &b.0)
}

fn root() -> Root {
    Root {
        epoch: 4,
        manifest_end: 312,
        manifest_crc32c: 0x1234_5678,
        through: at(90),
        segment_count: 2,
        stamp: Some(ResetStamp {
            sequence: 12,
            nonce: 0x5eed,
        }),
        compacted_through: None,
        groups: vec![GroupEntry {
            number: 0,
            segment_count: 2,
            first_file_id: 1,
            last_file_id: 2,
            first: at(1),
            last: at(90),
            first_commit_time: T0 + S,
            last_commit_time: T0 + 90 * S,
            residence: 1,
            body_crc32c: group().entry().body_crc32c,
        }],
        lost: vec![LostEntry {
            first: at(20),
            last: at(22),
            file_id: 1,
            cause: 1,
        }],
        quarantined: vec![],
        restored: vec![],
    }
}

fn root_bytes() -> Vec<u8> {
    let r = root();
    let g = &r.groups[0];
    let mut b = Body::default();
    b.u64(4).u64(312).u32(0).u32(0x1234_5678).u64(90).u64(2);
    b.u64(12).u64(0x5eed);
    b.u32(0).zeros(4).u64(0);
    b.u32(1).u32(1).u32(0).u32(0).u32(0);
    b.u16(72).u16(40).u16(16).u16(0).u16(16).zeros(2);
    b.u32(0).u32(2).u64(1).u64(2).pos(0, 1).pos(0, 90);
    b.i64(g.first_commit_time)
        .i64(g.last_commit_time)
        .u8(1)
        .zeros(3)
        .u32(g.body_crc32c);
    b.pos(0, 20).pos(0, 22).u64(1).u8(1).zeros(7);
    file(kind::LOG_DIRECTORY, block_type::ROOT, 5, &b.0)
}

fn series() -> SeriesDirectory {
    SeriesDirectory {
        through: at(95),
        lost: 1,
        stamp: None,
        judged_at: T0 + 601 * S,
        entries: vec![
            SeriesEntry {
                series_id: 1,
                latest_t: Some(T0 + 590 * S),
                lateness_reference: Some(T0 + 590 * S),
                fact_count: 60,
                orphan_count: 0,
                candidate: Some(Candidate {
                    t: T0 + 590 * S,
                    position: at(94),
                    quality: Quality::Good,
                    value: CandidateValue::F64(21.5f64.to_bits()),
                }),
                candidate_unsure: false,
                pending: None,
                pending_more: false,
                counts: Some(StartCounts {
                    late: 2,
                    future: 0,
                    last_received_at: T0 + 591 * S,
                    last_revisable: false,
                }),
            },
            SeriesEntry {
                series_id: 7,
                latest_t: Some(T0 + 7_200 * S),
                lateness_reference: None,
                fact_count: 3,
                orphan_count: 3,
                candidate: Some(Candidate {
                    t: T0 + 300 * S,
                    position: at(50),
                    quality: Quality::Suspect,
                    value: CandidateValue::Text,
                }),
                candidate_unsure: true,
                pending: Some(Pending {
                    t: T0 + 3_600 * S,
                    position: at(51),
                }),
                pending_more: true,
                counts: None,
            },
        ],
    }
}

fn series_bytes() -> Vec<u8> {
    let mut b = Body::default();
    b.pos(0, 95)
        .u32(1)
        .u64(0)
        .u64(0)
        .i64(T0 + 601 * S)
        .u32(2)
        .u16(120)
        .zeros(2);
    // Series 1: latest, reference, candidate, start counts (bits 0, 1, 2, 6); an f64 of good quality.
    b.u32(1).u16(0b100_0111).u8(1).u8(0);
    b.i64(T0 + 590 * S).i64(T0 + 590 * S).u64(60).u64(0);
    b.i64(T0 + 590 * S)
        .pos(0, 94)
        .u8(0)
        .zeros(3)
        .u64(21.5f64.to_bits());
    b.i64(0).pos(0, 0).zeros(4);
    b.u64(2).u64(0).i64(T0 + 591 * S);
    // Series 7, orphaned: latest, candidate unsure, pending and more (bits 0, 2, 3, 4, 5); text of suspect quality.
    b.u32(7).u16(0b11_1101).u8(5).u8(2);
    b.i64(T0 + 7_200 * S).i64(0).u64(3).u64(3);
    b.i64(T0 + 300 * S).pos(0, 50).u8(0).zeros(3).u64(0);
    b.i64(T0 + 3_600 * S).pos(0, 51).zeros(4);
    b.zeros(24);
    file(kind::SERIES_DIRECTORY, block_type::SERIES, 2, &b.0)
}

fn status() -> StatusFile {
    StatusFile {
        interval_ns: Some(60 * S as u64),
        mark_synced: at(95),
        health: health::DEGRADED,
        flags: flag::PARTIAL,
        pressure: false,
        pressure_since: None,
        slot_count: 0,
        reasons: vec![reason::DISK_LOW],
        counters: vec![
            (counter::QUEUE_DEPTH, 12),
            (counter::COMMIT_P99_NS, 4_000_000),
        ],
    }
}

fn status_bytes() -> Vec<u8> {
    let mut b = Body::default();
    b.u64(60 * S as u64)
        .pos(0, 95)
        .u8(1)
        .u8(0)
        .u8(1)
        .zeros(1)
        .i64(0);
    b.u32(0).u32(1).u32(2).u16(4).u16(16);
    b.u16(3).zeros(2);
    b.u16(1).zeros(6).u64(12);
    b.u16(6).zeros(6).u64(4_000_000);
    file(kind::STATUS, block_type::STATUS, 9, &b.0)
}

fn generate() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("logdir.root", root_bytes()),
        ("logdir.grp", group_bytes()),
        ("series.sdir", series_bytes()),
        ("status.stat", status_bytes()),
    ]
}

#[test]
fn golden_files_match_the_field_by_field_layout() {
    let bless = std::env::var_os("KVIK_BLESS_GOLDEN").is_some_and(|v| v == "1");
    for (name, bytes) in generate() {
        let path = dir().join(name);
        if bless {
            fs::write(&path, &bytes).unwrap();
        }
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "{name}: the format changed"
        );
    }
}

#[test]
fn golden_files_decode_to_what_they_show_and_encode_again() {
    let read = |n: &str| fs::read(dir().join(n)).unwrap();
    let b = read("logdir.root");
    assert_eq!(Root::decode(&b, &STORE).unwrap(), (frame(5), root()));
    assert_eq!(root().encode(frame(5), STORE).unwrap(), b);
    let b = read("logdir.grp");
    let (f, (g, crc)) = Group::decode(&b, &STORE).unwrap();
    assert_eq!(
        (f, &g, crc),
        (frame(3), &group(), root().groups[0].body_crc32c)
    );
    assert_eq!(group().encode(frame(3), STORE).unwrap(), b);
    let b = read("series.sdir");
    assert_eq!(
        SeriesDirectory::decode(&b, &STORE).unwrap(),
        (frame(2), series())
    );
    assert_eq!(series().encode(frame(2), STORE).unwrap(), b);
    let b = read("status.stat");
    assert_eq!(
        StatusFile::decode(&b, &STORE).unwrap(),
        (frame(9), status())
    );
    assert_eq!(status().encode(frame(9), STORE).unwrap(), b);
}

#[test]
fn a_v1_series_directory_reads_as_before_with_no_start_counts() {
    let b = fs::read(dir().join("series-v1.sdir")).unwrap();
    let (f, d) = SeriesDirectory::decode(&b, &STORE).unwrap();
    let mut v1 = series();
    for e in &mut v1.entries {
        e.counts = None;
    }
    assert_eq!((f, d), (frame(2), v1));
}

#[test]
fn a_longer_record_size_is_read_and_its_tail_ignored() {
    // The status file with 24-byte counter records: a later minor version's longer record (A.8's rule).
    let mut b = Body::default();
    b.u64(0).pos(0, 3).u8(0).u8(0).u8(0).zeros(1).i64(0);
    b.u32(0).u32(0).u32(1).u16(4).u16(24);
    b.u16(1).zeros(6).u64(5).u64(0xffff_ffff);
    let bytes = file(kind::STATUS, block_type::STATUS, 1, &b.0);
    let (_, s) = StatusFile::decode(&bytes, &STORE).unwrap();
    assert_eq!(s.counters, vec![(1, 5)]);
}
