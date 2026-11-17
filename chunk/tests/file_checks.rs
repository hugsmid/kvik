//! One case per reader check that sits behind the checksum chain (`docs/format-compacted.md` B.1 A reader's
//! checks, B.2 Checks): a body is edited and the file re-sealed, so it is checksum-valid and inconsistent, as only a
//! writer's mistake or a crafted object makes it, and the named check must refuse it.

mod compacted;

use std::cell::RefCell;
use std::io;

use compacted::*;
use kvik_chunk::file::reseal::{edit_blocks, reseal_index_object, reseal_segment};
use kvik_chunk::file::{
    Batch, Class, CompactedFile, Fact, Fault, FileError, IndexObject, Reach, ReadAt, Series,
    block_type, extract,
};
use kvik_chunk::value_type;

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    b[at..at + v.len()].copy_from_slice(v);
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// `bytes` with the `n`-th block of type `ty` edited and the segment re-sealed.
fn edit(bytes: &[u8], ty: u8, n: usize, f: impl Fn(&mut [u8])) -> Vec<u8> {
    edit_blocks(
        bytes,
        ty,
        |_, k, body| {
            if k == n {
                f(body)
            }
        },
        reseal_segment,
    )
}

fn edit_object(bytes: &[u8], ty: u8, f: impl Fn(&mut [u8])) -> Vec<u8> {
    edit_blocks(bytes, ty, |_, _, body| f(body), reseal_index_object)
}

fn fault<T>(r: Result<T, FileError>) -> Fault {
    match r {
        Err(FileError::Invalid { fault, .. }) => fault,
        Err(e) => panic!("expected an invalid file, got {e}"),
        Ok(_) => panic!("expected an invalid file, it was accepted"),
    }
}

fn open(b: &[u8]) -> Result<CompactedFile<&[u8]>, FileError> {
    CompactedFile::open(b, &id(40), None)
}

fn read_series(f: &CompactedFile<&[u8]>, s: u32) -> Result<usize, FileError> {
    let mut n = 0;
    for e in f.series(s)? {
        n += f.read_chunk(&e)?.len();
    }
    Ok(n)
}

fn base() -> Vec<u8> {
    let b = sample(1).write().0;
    assert_eq!(reseal_segment(&b), b);
    b
}

fn trailer(b: &[u8]) -> usize {
    u64_at(b, b.len() - 16) as usize + 16
}

#[test]
fn open_refuses_a_directory_that_disagrees_with_the_trailers_count() {
    let b = edit(&base(), block_type::TRAILER, 0, |t| {
        let n = u32_at(t, 76);
        put(t, 76, &(n + 1).to_le_bytes());
    });
    assert_eq!(fault(open(&b)), Fault::Mismatch("commit-time directory"));
}

#[test]
fn open_refuses_a_directory_that_does_not_cover_the_trailers_range() {
    let b = edit(&base(), block_type::TRAILER, 0, |t| {
        put(t, 32, &999u64.to_le_bytes());
    });
    assert_eq!(fault(open(&b)), Fault::Mismatch("commit-time directory"));
}

#[test]
fn open_refuses_field_11_without_an_entries_section() {
    let b = edit(&base(), block_type::TRAILER, 0, |t| {
        put(t, 80, &0u64.to_le_bytes());
    });
    assert_eq!(fault(open(&b)), Fault::Mismatch("description flags"));
}

#[test]
fn open_refuses_a_file_naming_itself_as_an_input() {
    let b = edit(&base(), block_type::DESCRIPTION, 0, |d| {
        d[76] = 2;
        put(d, 80, &40u64.to_le_bytes());
    });
    assert_eq!(fault(open(&b)), Fault::Body("description inputs"));
}

#[test]
fn open_refuses_a_root_at_or_above_the_trailer() {
    let raw = base();
    let at = trailer(&raw) as u64 - 16;
    let b = edit(&raw, block_type::TRAILER, 0, |t| {
        put(t, 112, &at.to_le_bytes())
    });
    assert_eq!(fault(open(&b)), Fault::Mismatch("trailer offsets"));
}

#[test]
fn open_refuses_section_offsets_that_do_not_rise_or_a_mandatory_one_of_zero() {
    let raw = base();
    let t = trailer(&raw);
    let dir = u64_at(&raw, t + 96);
    let b = edit(&raw, block_type::TRAILER, 0, |t| {
        put(t, 88, &dir.to_le_bytes())
    });
    assert_eq!(fault(open(&b)), Fault::Body("trailer offsets"));
    for at in [88, 96, 112] {
        let b = edit(&raw, block_type::TRAILER, 0, |t| {
            put(t, at, &0u64.to_le_bytes())
        });
        assert_eq!(
            fault(open(&b)),
            Fault::Body("trailer offsets"),
            "offset at {at}"
        );
    }
}

#[test]
fn open_refuses_a_root_whose_counts_disagree_with_the_trailer_or_overrun_the_index() {
    let b = edit(&base(), block_type::TRAILER, 0, |t| {
        put(t, 72, &2u32.to_le_bytes())
    });
    assert_eq!(fault(open(&b)), Fault::Mismatch("index root"));
    let b = edit(&base(), block_type::TRAILER, 0, |t| {
        put(t, 72, &4u32.to_le_bytes())
    });
    let b = edit(&b, block_type::ROOT, 0, |r| {
        put(r, 8 + 24, &4u32.to_le_bytes())
    });
    assert_eq!(fault(open(&b)), Fault::Mismatch("index root"));
}

#[test]
fn open_refuses_positions_per_generation_that_disagree_with_the_trailer() {
    let b = edit(&base(), block_type::DESCRIPTION, 0, |d| {
        assert_eq!(u16::from_le_bytes([d[88], d[89]]), 10);
        let n = u64_at(d, 100);
        put(d, 100, &(n + 1).to_le_bytes());
    });
    assert_eq!(fault(open(&b)), Fault::Mismatch("description positions"));
}

#[test]
fn an_index_block_that_disagrees_with_its_root_record_is_refused() {
    let b = edit(&base(), block_type::ROOT, 0, |r| {
        let t = u64_at(r, 16);
        put(r, 16, &(t + 1).to_le_bytes());
    });
    assert_eq!(
        fault(open(&b).unwrap().index()),
        Fault::Mismatch("index block")
    );
}

#[test]
fn an_index_block_out_of_order_is_refused() {
    let b = edit(&base(), block_type::INDEX, 0, |ib| {
        let (a, z) = ib[8..8 + 192].split_at_mut(96);
        a.swap_with_slice(z);
    });
    assert_eq!(
        fault(open(&b).unwrap().index()),
        Fault::Body("chunk index order")
    );
}

fn index_edit(at: usize, n: usize, v: &[u8]) -> Fault {
    let raw = base();
    let b = edit(&raw, block_type::INDEX, 0, |ib| put(ib, 8 + 96 * n + at, v));
    fault(open(&b).unwrap().index())
}

#[test]
fn an_index_entry_outside_the_chunk_section_or_the_trailers_ranges_is_refused() {
    let raw = base();
    let times = u64_at(&raw, trailer(&raw) + 88);
    assert_eq!(
        index_edit(56, 0, &times.to_le_bytes()),
        Fault::Mismatch("chunk index entry")
    );
    let reach = u64_at(&raw, trailer(&raw) + 64);
    assert_eq!(
        index_edit(24, 0, &(reach + 1).to_le_bytes()),
        Fault::Mismatch("chunk index entry")
    );
    let first = open(&raw).unwrap().index().unwrap()[0].flags;
    assert_eq!(
        index_edit(4, 0, &(first | 4).to_le_bytes()),
        Fault::Mismatch("chunk index entry")
    );
}

#[test]
fn an_index_entry_with_inconsistent_statistics_is_refused() {
    assert_eq!(
        index_edit(4, 2, &3u16.to_le_bytes()),
        Fault::Body("chunk index entry")
    );
    assert_eq!(
        index_edit(80, 2, &i64::MAX.to_le_bytes()),
        Fault::Body("chunk index entry")
    );
    assert_eq!(
        index_edit(24, 0, &0i64.to_le_bytes()),
        Fault::Body("chunk index entry")
    );
}

fn dir_fault(f: impl Fn(&mut [u8])) -> Fault {
    let b = edit(&base(), block_type::DIRECTORY, 0, f);
    let file = open(&b).unwrap();
    fault(read_series(&file, 1))
}

#[test]
fn a_directory_of_another_series_or_with_other_flags_is_refused() {
    assert_eq!(
        dir_fault(|d| put(d, 0, &9u32.to_le_bytes())),
        Fault::Mismatch("chunk directory")
    );
    assert_eq!(
        dir_fault(|d| put(d, 8, &41u32.to_le_bytes())),
        Fault::Body("chunk directory records")
    );
    assert_eq!(
        dir_fault(|d| put(d, 4, &4u16.to_le_bytes())),
        Fault::Mismatch("chunk flags")
    );
}

#[test]
fn a_directory_block_shorter_than_the_entrys_dir_len_is_refused() {
    let b = edit(&base(), block_type::INDEX, 0, |ib| {
        let len = u32_at(ib, 8 + 64);
        let dir = u32_at(ib, 8 + 68);
        put(ib, 8 + 64, &(len + 8).to_le_bytes());
        put(ib, 8 + 68, &(dir + 8).to_le_bytes());
    });
    assert_eq!(
        fault(read_series(&open(&b).unwrap(), 1)),
        Fault::BlockLength
    );
}

/// A current file with one series of two full blocks of the same length.
fn two_blocks() -> Vec<u8> {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 2, 4096);
    let facts = (0..8192u64)
        .map(|i| {
            f64_fact(
                1,
                (1, 1 + i),
                T0 + i as i64 * SECOND,
                f64::from_bits(noise(i, 3)),
            )
        })
        .collect();
    c.series = vec![Series {
        series_id: 1,
        name: Some("b/a".into()),
        reach: Reach::Point,
        facts,
        set_aside: vec![],
    }];
    c.settle().write().0
}

#[test]
fn a_directory_whose_records_are_permuted_is_refused() {
    let raw = two_blocks();
    let b = edit(&raw, block_type::DIRECTORY, 0, |d| {
        let (a, z) = d[24..24 + 64].split_at_mut(32);
        assert_eq!(a[20..24], z[20..24], "the blocks have one length");
        a.swap_with_slice(z);
    });
    assert_eq!(
        fault(read_series(&open(&b).unwrap(), 1)),
        Fault::Mismatch("block records")
    );
}

#[test]
fn a_data_block_that_disagrees_with_its_record_is_refused() {
    let raw = two_blocks();
    let b = edit(&raw, block_type::DIRECTORY, 0, |d| {
        let len = u32_at(d, 24 + 20);
        let other = if len.is_multiple_of(8) {
            len - 1
        } else {
            len + 1 - (len % 8 == 7) as u32 * 2
        };
        assert_eq!(len.div_ceil(8), other.div_ceil(8));
        put(d, 24 + 20, &other.to_le_bytes());
    });
    assert_eq!(
        fault(read_series(&open(&b).unwrap(), 1)),
        Fault::BlockLength
    );
    let b = edit(&raw, block_type::DIRECTORY, 0, |d| {
        let t = u64_at(d, 24);
        put(d, 24, &(t - 1).to_le_bytes());
    });
    assert_eq!(
        fault(read_series(&open(&b).unwrap(), 1)),
        Fault::Mismatch("data block record")
    );
}

#[test]
fn a_commit_times_block_that_disagrees_with_its_record_is_refused() {
    let b = edit(&base(), block_type::TIMES_DIRECTORY, 0, |d| {
        let t = u64_at(d, 8 + 48 + 24);
        put(d, 8 + 48 + 24, &(t + 1).to_le_bytes());
    });
    let f = open(&b).unwrap();
    assert_eq!(
        fault(f.batch((2, 1))),
        Fault::Mismatch("commit-times record")
    );
    assert!(f.batch((1, 1)).is_ok());
}

/// One series of 9,000 batches of one fact in each of two generations: three commit-times blocks a generation.
fn many() -> (Content, Vec<u8>) {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 9000, 1);
    c.batches
        .extend(batches(2, 1, 9000, 1).into_iter().map(|b| Batch {
            received_at: b.received_at + 10_000 * SECOND,
            commit_time: b.commit_time + 10_000 * SECOND,
            ..b
        }));
    let facts = c
        .batches
        .iter()
        .enumerate()
        .map(|(i, b)| {
            Fact::value(
                1,
                (b.generation, b.first_sequence),
                T0 + i as i64,
                value_type::BOOL,
                1,
            )
        })
        .collect();
    c.series = vec![Series {
        series_id: 1,
        name: Some("m/a".into()),
        reach: Reach::Point,
        facts,
        set_aside: vec![],
    }];
    let c = c.settle();
    let b = c.write().0;
    (c, b)
}

#[test]
fn commit_times_that_decrease_are_refused_in_the_directory_and_in_a_block() {
    let (_, raw) = many();
    let b = edit(&raw, block_type::TIMES_DIRECTORY, 0, |d| {
        let last = u64_at(d, 8 + 32);
        put(d, 8 + 48 + 24, &(last - 1).to_le_bytes());
    });
    assert_eq!(
        fault(open(&b)),
        Fault::Body("commit-time directory records")
    );
    let mut c = sample(1);
    let mut rows = c.used_batches();
    rows[2].received_at = rows[1].received_at;
    rows[2].commit_time = rows[1].commit_time - 1;
    c.tweaks.times_rows = Some(rows);
    let b = c.write().0;
    assert_eq!(
        fault(open(&b).unwrap().batch((1, 1))),
        Fault::Body("commit-times block")
    );
}

/// A source that records the offsets it is asked for.
struct Counting<'a> {
    bytes: &'a [u8],
    reads: RefCell<Vec<u64>>,
}

impl ReadAt for Counting<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.reads.borrow_mut().push(offset);
        self.bytes.read_at(offset, len)
    }
}

fn counted(bytes: &[u8]) -> CompactedFile<Counting<'_>> {
    let src = Counting {
        bytes,
        reads: RefCell::new(Vec::new()),
    };
    let f = CompactedFile::open(src, &id(40), None).unwrap();
    f.source().reads.borrow_mut().clear();
    f
}

fn times_reads(f: &CompactedFile<Counting<'_>>) -> usize {
    let t = f.trailer();
    f.source()
        .reads
        .borrow()
        .iter()
        .filter(|&&o| t.times_offset <= o && o < t.times_dir_offset)
        .count()
}

#[test]
fn a_position_outside_every_record_reads_no_commit_times_block() {
    let b = base();
    for p in [(1, 200), (1, 0), (3, 1), (0, 5)] {
        let f = counted(&b);
        assert!(matches!(fault(f.batch(p)), Fault::NoBatch(..)), "{p:?}");
        assert_eq!(times_reads(&f), 0, "{p:?}");
    }
}

#[test]
fn as_of_reads_at_most_one_block_a_generation_and_none_at_a_records_end() {
    let (c, b) = many();
    let oracle = |t: i64| {
        c.batches
            .iter()
            .filter(|x| x.commit_time <= t)
            .map(|x| (x.generation, x.last_sequence))
            .max()
    };
    let f = CompactedFile::open(b.as_slice(), &id(40), None).unwrap();
    assert_eq!(f.times_directory().len(), 6);
    for x in &c.batches {
        for t in [x.commit_time - 1, x.commit_time, x.commit_time + 1] {
            assert_eq!(f.as_of(t).unwrap(), oracle(t), "as of {t}");
        }
    }
    let records = f.times_directory().to_vec();
    for (k, x) in c.batches.iter().enumerate().step_by(311) {
        let f = counted(&b);
        assert_eq!(f.as_of(x.commit_time).unwrap(), oracle(x.commit_time));
        let reads = times_reads(&f);
        let at_end = records.iter().any(|r| r.last_commit_time == x.commit_time);
        assert!(reads <= 2, "batch {k}: {reads} reads");
        if at_end {
            assert!(reads <= 1, "batch {k}");
        }
    }
    for r in &records {
        let f = counted(&b);
        f.as_of(r.last_commit_time).unwrap();
        let other_generation = records.iter().filter(|o| o.generation != r.generation);
        let read_needed = other_generation.clone().any(|o| {
            o.first_commit_time <= r.last_commit_time && r.last_commit_time < o.last_commit_time
        });
        assert_eq!(
            times_reads(&f),
            usize::from(read_needed),
            "at the end of {r:?}"
        );
    }
}

#[test]
fn an_entries_block_that_disagrees_with_its_batch_or_order_or_count_is_refused() {
    let b = edit(&base(), block_type::ENTRIES, 0, |e| {
        let t = u64_at(e, 8);
        put(e, 8, &(t + 1).to_le_bytes());
    });
    assert_eq!(
        fault(open(&b).unwrap().entries()),
        Fault::Mismatch("entry receive time")
    );
    let raw = base();
    let b = edit(&raw, block_type::ENTRIES, 1, |e| {
        assert_eq!(u64_at(e, 0), 8);
        put(e, 0, &4u64.to_le_bytes());
    });
    assert_eq!(
        fault(open(&b).unwrap().entries()),
        Fault::Mismatch("entry position")
    );
    let b = edit(&raw, block_type::TRAILER, 0, |t| {
        let n = u64_at(t, 0);
        put(t, 0, &(n + 1).to_le_bytes());
    });
    let b = edit(&b, block_type::DESCRIPTION, 0, |d| {
        let n = u64_at(d, 116);
        put(d, 116, &(n + 1).to_le_bytes());
    });
    assert_eq!(
        fault(open(&b).unwrap().entries()),
        Fault::Mismatch("entry count")
    );
}

fn object() -> Vec<u8> {
    let raw = base();
    let o = extract(&open(&raw).unwrap(), [3; 32]).unwrap();
    assert_eq!(reseal_index_object(&o), o);
    o
}

fn object_fault(b: &[u8]) -> Fault {
    fault(IndexObject::open(b, &id(40)))
}

#[test]
fn a_chunk_index_object_whose_directory_disagrees_with_the_segments_trailer_is_refused() {
    let b = edit_object(&object(), block_type::SEGMENT, |s| {
        let n = u32_at(s, 76);
        put(s, 76, &(n + 5).to_le_bytes());
    });
    assert_eq!(object_fault(&b), Fault::Mismatch("commit-time directory"));
}

#[test]
fn a_chunk_index_object_naming_its_segment_as_an_input_is_refused() {
    let b = edit_object(&object(), block_type::DESCRIPTION, |d| {
        d[76] = 2;
        put(d, 80, &40u64.to_le_bytes());
    });
    assert_eq!(object_fault(&b), Fault::Body("description inputs"));
}

#[test]
fn a_chunk_index_object_whose_segment_record_or_offsets_disagree_is_refused() {
    let o = object();
    let b = edit_object(&o, block_type::SEGMENT, |s| {
        let c = u32_at(s, 132);
        put(s, 132, &(c ^ 1).to_le_bytes());
    });
    assert_eq!(object_fault(&b), Fault::Mismatch("segment record"));
    let b = edit_object(&o, block_type::SEGMENT, |s| {
        let at = s.len() - 40;
        put(s, at, &64u64.to_le_bytes());
    });
    assert_eq!(object_fault(&b), Fault::Mismatch("segment record"));
    let b = edit_object(&o, block_type::TRAILER, |t| {
        let at = u64_at(t, 16);
        put(t, 16, &(at + 8).to_le_bytes());
    });
    assert_eq!(object_fault(&b), Fault::Mismatch("trailer offsets"));
    let b = edit_object(&o, block_type::SEGMENT, |s| {
        let c = u32_at(s, 124);
        put(s, 124, &(c ^ 1).to_le_bytes());
    });
    assert_eq!(object_fault(&b), Fault::Mismatch("trailer offsets"));
}

#[test]
fn every_position_of_a_run_of_ignored_entries_is_checked_against_its_batch() {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 1, 4);
    c.series = vec![Series {
        series_id: 1,
        name: Some("e/a".into()),
        reach: Reach::Point,
        facts: vec![f64_fact(1, (1, 1), T0, 1.0)],
        set_aside: vec![],
    }];
    c.entries = (2..=3)
        .map(|q| {
            (
                (1, q),
                kvik_log::entries::Entry::Retirement { id: 1, t_r: T0 },
            )
        })
        .collect();
    let mut c = c.settle();
    let mut rows = c.batches.clone();
    rows[0].last_sequence = 2;
    rows.push(Batch {
        first_sequence: 3,
        last_sequence: 4,
        received_at: rows[0].received_at + 1,
        commit_time: rows[0].commit_time + 1,
        ..rows[0]
    });
    c.tweaks.times_rows = Some(rows);
    let raw = c.write().0;
    let b = edit(&raw, block_type::ENTRIES, 0, |e| {
        assert_eq!(u32_at(e, 16), 2, "both entries in one block");
        let second = 24 + 8 + (u32_at(e, 28) as usize).div_ceil(8) * 8;
        e[24] = 0x90;
        e[second] = 0x90;
    });
    assert_eq!(
        fault(open(&b).unwrap().entries()),
        Fault::Mismatch("entry receive time")
    );
}
