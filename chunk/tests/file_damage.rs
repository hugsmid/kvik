//! A compacted segment's reader against damage (`docs/format-compacted.md` B.1 A reader's checks, B.3): a valid
//! block from another place or an older file refused by its parent's checksum, damage confined to the reads that
//! reach the damaged block, truncation, refused headers, a layout that disagrees, and a damaged chunk index object.

mod compacted;

use std::collections::BTreeSet;

use compacted::*;
use kvik_chunk::file::{
    CompactedFile, Fault, FileElement, FileError, IndexObject, Layout, Level, TimedFact,
    block_type, extract,
};
use kvik_log::entries::Entry;
use kvik_log::header::FileHeader;

fn open(bytes: &[u8]) -> CompactedFile<&[u8]> {
    CompactedFile::open(bytes, &id(40), None).unwrap()
}

fn fault(r: Result<impl std::fmt::Debug, FileError>) -> (u64, Fault) {
    match r {
        Err(FileError::Invalid { offset, fault }) => (offset, fault),
        other => panic!("expected an invalid file, got {other:?}"),
    }
}

/// Copies the block at `at` from `from` into `into`.
fn splice(into: &[u8], from: &[u8], at: usize, len: usize) -> Vec<u8> {
    let mut b = into.to_vec();
    b[at..at + len].copy_from_slice(&from[at..at + len]);
    b
}

fn block_len(bytes: &[u8], at: usize) -> usize {
    blocks(bytes).iter().find(|b| b.0 == at).unwrap().2
}

fn read_series(f: &CompactedFile<&[u8]>, id: u32) -> Result<Vec<TimedFact>, FileError> {
    let mut out = Vec::new();
    for e in f.series(id)? {
        out.extend(f.read_chunk(&e)?);
    }
    Ok(out)
}

#[test]
fn a_valid_block_from_another_place_or_an_older_file_is_refused() {
    let bytes = sample(1).write().0;
    let older = sample(2).write().0;
    assert_eq!(bytes.len(), older.len());
    let f = open(&bytes);

    let e1 = f.series(1).unwrap()[0];
    let e2 = f.series(2).unwrap()[0];
    let d1 = f.directory(&e1).unwrap();
    let data = (e1.chunk_offset + u64::from(d1.blocks[0].offset)) as usize;
    let len = block_len(&bytes, data);
    let b = splice(&bytes, &older, data, len);
    assert_eq!(
        fault(open(&b).read_chunk(&e1)),
        (data as u64, Fault::Binding),
        "a data block of an older file"
    );

    let d2 = f.directory(&e2).unwrap();
    let other = (e2.chunk_offset + u64::from(d2.blocks[0].offset)) as usize;
    assert_eq!(block_len(&bytes, other), len);
    let mut moved = bytes.clone();
    moved[data..data + len].copy_from_slice(&bytes[other..other + len]);
    assert_eq!(
        fault(open(&moved).read_chunk(&e1)),
        (data as u64, Fault::Binding),
        "a data block moved from another chunk"
    );

    let dir = (e1.chunk_offset + u64::from(e1.chunk_len - e1.dir_len)) as usize;
    let b = splice(&bytes, &older, dir, e1.dir_len as usize);
    assert_eq!(
        fault(open(&b).read_chunk(&e1)),
        (dir as u64, Fault::Binding)
    );

    let t = *f.trailer();
    let index = t.index_offset as usize;
    let b = splice(&bytes, &older, index, block_len(&bytes, index));
    assert_eq!(fault(open(&b).series(1)), (index as u64, Fault::Binding));

    let root = t.index_root_offset as usize;
    let b = splice(&bytes, &older, root, block_len(&bytes, root));
    assert_eq!(
        fault(CompactedFile::open(b.as_slice(), &id(40), None)),
        (root as u64, Fault::Binding)
    );

    let times = f.times_directory().to_vec();
    assert_eq!(times.len(), 2);
    let (g1, g2) = (times[0].offset as usize, times[1].offset as usize);
    let len = block_len(&bytes, g1);
    assert_eq!(block_len(&bytes, g2), len);
    let mut moved = bytes.clone();
    moved[g2..g2 + len].copy_from_slice(&bytes[g1..g1 + len]);
    assert_eq!(
        fault(open(&moved).batch((2, 5))),
        (g2 as u64, Fault::Binding)
    );
    assert!(open(&moved).batch((1, 5)).is_ok());

    let mut renamed = sample(1);
    for (_, e) in &mut renamed.entries {
        if let Entry::Alias { name, .. } = e {
            *name = name.replace("alias", "other");
        }
    }
    let renamed = renamed.write().0;
    let entries = t.entries_offset as usize;
    let b = splice(
        &bytes,
        &renamed,
        entries,
        (t.times_offset - t.entries_offset) as usize,
    );
    assert_eq!(fault(open(&b).entries()), (entries as u64, Fault::Binding));

    let mut run = sample(1);
    run.description.run_id = 41;
    let run = run.write().0;
    let b = splice(&bytes, &run, 64, block_len(&bytes, 64));
    assert_eq!(
        fault(CompactedFile::open(b.as_slice(), &id(40), None)),
        (64, Fault::Binding)
    );
}

/// The blocks a read of series `id` reaches: index blocks, its chunks' blocks, the commit-times blocks of its
/// positions.
fn reached_by_series(
    f: &CompactedFile<&[u8]>,
    all: &[(usize, u8, usize)],
    id: u32,
) -> BTreeSet<usize> {
    let t = f.trailer();
    let mut out: BTreeSet<usize> = f
        .root()
        .iter()
        .filter(|r| r.first_series_id <= id && id <= r.last_series_id)
        .map(|r| (t.index_offset + r.offset) as usize)
        .collect();
    for e in f.series(id).unwrap() {
        let (a, z) = (
            e.chunk_offset as usize,
            (e.chunk_offset + u64::from(e.chunk_len)) as usize,
        );
        out.extend(all.iter().filter(|b| a <= b.0 && b.0 < z).map(|b| b.0));
        for r in f.read_chunk(&e).unwrap() {
            out.extend(times_block_of(f, r.fact.position));
        }
    }
    out
}

fn times_block_of(f: &CompactedFile<&[u8]>, p: (u32, u64)) -> Option<usize> {
    f.times_directory()
        .iter()
        .find(|r| r.generation == p.0 && r.first_sequence <= p.1 && p.1 <= r.last_sequence)
        .map(|r| r.offset as usize)
}

#[test]
fn a_flipped_bit_in_any_block_fails_only_the_reads_that_reach_it() {
    let bytes = sample(1).write().0;
    let f = open(&bytes);
    let all = blocks(&bytes);
    let t = *f.trailer();
    let series: Vec<u32> = vec![1, 2, 3];
    let want: Vec<Vec<TimedFact>> = series
        .iter()
        .map(|&s| read_series(&f, s).unwrap())
        .collect();
    let want_entries = f.entries().unwrap();
    let reached: Vec<BTreeSet<usize>> = series
        .iter()
        .map(|&s| reached_by_series(&f, &all, s))
        .collect();
    let mut entries_reach: BTreeSet<usize> = all
        .iter()
        .filter(|b| t.entries_offset as usize <= b.0 && b.0 < t.times_offset as usize)
        .map(|b| b.0)
        .collect();
    entries_reach.extend(
        want_entries
            .iter()
            .filter_map(|e| times_block_of(&f, e.position)),
    );
    let at_open = [
        block_type::DESCRIPTION,
        block_type::TIMES_DIRECTORY,
        block_type::ROOT,
        block_type::TRAILER,
    ];
    for &(at, ty, len) in &all {
        for byte in [at + 5, at + 16 + (len - 16) / 2] {
            let mut damaged = bytes.clone();
            damaged[byte] ^= 0x10;
            let opened = CompactedFile::open(damaged.as_slice(), &id(40), None);
            if at_open.contains(&ty) {
                assert!(
                    matches!(opened, Err(FileError::Invalid { .. })),
                    "block {ty:#x} at {at}"
                );
                continue;
            }
            let d = opened.unwrap_or_else(|e| panic!("block {ty:#x} at {at}: {e}"));
            for (i, &s) in series.iter().enumerate() {
                match read_series(&d, s) {
                    Ok(rows) => {
                        assert_eq!(rows, want[i]);
                        assert!(
                            !reached[i].contains(&at),
                            "damage at {at} not seen by series {s}"
                        );
                    }
                    Err(FileError::Invalid { .. }) => {
                        assert!(
                            reached[i].contains(&at),
                            "series {s} failed for damage at {at}"
                        )
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            match d.entries() {
                Ok(rows) => {
                    assert_eq!(rows, want_entries);
                    assert!(!entries_reach.contains(&at));
                }
                Err(FileError::Invalid { .. }) => assert!(entries_reach.contains(&at)),
                Err(e) => panic!("{e}"),
            }
        }
    }
}

#[test]
fn a_truncated_or_extended_file_is_invalid() {
    let bytes = sample(1).write().0;
    for n in 0..bytes.len() {
        let r = CompactedFile::open(&bytes[..n], &id(40), None);
        assert!(
            matches!(r, Err(FileError::Invalid { .. })),
            "truncated at {n}"
        );
    }
    let mut longer = bytes.clone();
    longer.extend_from_slice(&[0; 8]);
    assert!(matches!(
        CompactedFile::open(longer.as_slice(), &id(40), None),
        Err(FileError::Invalid { .. })
    ));
}

#[test]
fn a_header_of_another_kind_store_name_or_a_newer_major_version_is_refused() {
    let bytes = sample(1).write().0;
    let refused =
        |i: kvik_chunk::file::Identity| match CompactedFile::open(bytes.as_slice(), &i, None) {
            Err(FileError::Refused(field)) => field,
            other => panic!("{:?}", other.map(|_| ())),
        };
    assert_eq!(
        refused(kvik_chunk::file::Identity {
            store_id: [8; 16],
            ..id(40)
        }),
        "store_id"
    );
    assert_eq!(refused(id(41)), "file_id");
    assert_eq!(
        refused(kvik_chunk::file::Identity {
            generation: 2,
            ..id(40)
        }),
        "generation"
    );
    let mut h = FileHeader::decode(&bytes).unwrap();
    for (kind, major) in [(14, 1), (2, 2)] {
        h.kind = kind;
        h.format_major = major;
        let mut b = bytes.clone();
        b[..64].copy_from_slice(&h.encode());
        let r = CompactedFile::open(b.as_slice(), &id(40), None);
        match (kind, r) {
            (14, Err(FileError::Refused("kind"))) => {}
            (
                2,
                Err(FileError::Unsupported {
                    element: FileElement::FormatMajor(2),
                    ..
                }),
            ) => {}
            (_, other) => panic!("{:?}", other.map(|_| ())),
        }
    }
}

#[test]
fn a_file_that_disagrees_with_its_recorded_layout_is_refused_naming_the_field() {
    let (bytes, written) = sample(1).write();
    let l = written.layout;
    let open = |l: &Layout| match CompactedFile::open(bytes.as_slice(), &id(40), Some(l)) {
        Err(FileError::Layout(field)) => field,
        other => panic!("{:?}", other.map(|_| ())),
    };
    assert!(CompactedFile::open(bytes.as_slice(), &id(40), Some(&l)).is_ok());
    let cases: Vec<(Layout, &str)> = vec![
        (
            Layout {
                level: Level::Day,
                ..l.clone()
            },
            "level",
        ),
        (
            Layout {
                period_len: l.period_len + 1,
                ..l.clone()
            },
            "period",
        ),
        (
            Layout {
                run_count: 3,
                ..l.clone()
            },
            "run",
        ),
        (
            Layout {
                inputs: vec![],
                ..l.clone()
            },
            "inputs",
        ),
        (
            Layout {
                range: (l.range.0, (2, 1_000)),
                ..l.clone()
            },
            "position range",
        ),
        (
            Layout {
                min_t: l.min_t - 1,
                ..l.clone()
            },
            "min_t",
        ),
        (
            Layout {
                max_reach: l.max_reach + 1,
                ..l.clone()
            },
            "max_reach",
        ),
        (
            Layout {
                last_commit_time: 0,
                ..l.clone()
            },
            "commit times",
        ),
        (
            Layout {
                fact_count: l.fact_count - 1,
                ..l.clone()
            },
            "fact_count",
        ),
        (
            Layout {
                entry_count: l.entry_count + 1,
                ..l.clone()
            },
            "entry_count",
        ),
        (
            Layout {
                has_entries: false,
                ..l.clone()
            },
            "flags",
        ),
        (
            Layout {
                name_range: Some(("a".into(), "b".into())),
                ..l.clone()
            },
            "name range",
        ),
    ];
    for (layout, field) in cases {
        assert_eq!(open(&layout), field);
    }
}

#[test]
fn a_chunk_index_object_with_any_bit_flipped_is_invalid() {
    let bytes = sample(1).write().0;
    let object = extract(&open(&bytes), [3; 32]).unwrap();
    assert!(IndexObject::open(object.as_slice(), &id(40)).is_ok());
    for i in 0..object.len() * 8 {
        let mut d = object.clone();
        d[i / 8] ^= 1 << (i % 8);
        assert!(
            matches!(
                IndexObject::open(d.as_slice(), &id(40)),
                Err(FileError::Invalid { .. })
            ),
            "bit {i}"
        );
    }
    for n in 0..object.len() {
        assert!(
            IndexObject::open(&object[..n], &id(40)).is_err(),
            "truncated at {n}"
        );
    }
}

#[test]
fn an_undefined_directory_flag_is_refused_where_the_index_bit_is_defined() {
    let mut c = sample(1);
    c.tweaks.chunk_flags = BIT_1;
    let bytes = c.write().0;
    let f = open(&bytes);
    let e = f.series(1).unwrap()[0];
    assert!(e.has_nan(), "bit 1 is the index's has_nan");
    match f.read_chunk(&e) {
        Err(FileError::Unsupported { element, .. }) => {
            assert_eq!(element, FileElement::DirectoryFlags(BIT_1))
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}

/// Undefined in a directory, `has_nan` in an index entry.
const BIT_1: u16 = 1 << 1;

#[test]
fn a_directory_whose_block_records_do_not_reach_its_end_is_refused() {
    let mut c = Content::new(kvik_chunk::file::Class::Current);
    c.batches = batches(1, 1, 3, 4096);
    let values = (0..4100u64)
        .map(|i| f64::from_bits(noise(i, 1)))
        .collect::<Vec<_>>();
    let facts = values
        .iter()
        .enumerate()
        .map(|(i, &v)| f64_fact(1, (1, 1 + i as u64), T0 + i as i64 * SECOND, v))
        .collect();
    c.series = vec![kvik_chunk::file::Series {
        series_id: 1,
        name: Some("d/a".into()),
        reach: kvik_chunk::file::Reach::Point,
        facts,
        set_aside: vec![],
    }];
    c.tweaks.unrecorded_last_block = true;
    let bytes = c.settle().write().0;
    let f = open(&bytes);
    let e = f.series(1).unwrap()[0];
    assert_eq!(
        e.row_count, 4096,
        "the second block is written but not recorded"
    );
    let dir = e.chunk_offset + u64::from(e.chunk_len - e.dir_len);
    assert_eq!(
        fault(f.read_chunk(&e)),
        (dir, Fault::Mismatch("block records"))
    );
}

#[test]
fn a_position_no_batch_row_holds_is_invalid_and_never_takes_a_neighbour_row() {
    let mut c = sample(1);
    let mut rows = c.used_batches();
    // Generation 1's second batch (sequences 5 to 8) loses sequence 5, which series 1 holds.
    rows[1].first_sequence = 6;
    c.tweaks.times_rows = Some(rows);
    let bytes = c.write().0;
    let f = open(&bytes);
    assert!(matches!(fault(f.batch((1, 5))), (_, Fault::NoBatch(1, 5))));
    assert!(matches!(
        fault(read_series(&f, 1)),
        (_, Fault::NoBatch(1, 5))
    ));
    assert!(read_series(&f, 2).is_ok());
}

#[test]
fn the_writer_refuses_a_file_named_as_its_own_input() {
    let c = sample(1);
    let mut d = c.description.clone();
    d.inputs = vec![kvik_chunk::file::Input {
        kind: 2,
        file_id: c.id.file_id,
    }];
    let r = kvik_chunk::file::FileWriter::new(Vec::new(), c.id, 0, &d, c.batches.clone());
    assert!(matches!(r, Err(kvik_chunk::file::WriteError::Content(_))));
}
