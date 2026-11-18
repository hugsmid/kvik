//! Golden compacted segments and a chunk index object (`tests/golden/compacted/README.md`, `docs/format-compacted.md`
//! B.3): each file equals what the writer produces now and reads back to the content it was built from.
//! Regenerate only after a deliberate format change:
//! `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-chunk --test file_golden`.

mod compacted;
mod golden_content;

use std::fs;
use std::path::PathBuf;

use compacted::*;
use golden_content::*;
use kvik_chunk::file::{
    Batch, Bounds, CanonicalError, CompactedFile, Fact, FileElement, FileError, Ignored,
    IndexObject, Level, check_canonical, extract,
};
use kvik_chunk::{Element, encoding};
use sha2::{Digest, Sha256};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/compacted")
}

fn read(name: &str) -> Vec<u8> {
    fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn kvi(bytes: &[u8]) -> Vec<u8> {
    let file = CompactedFile::open(bytes, &id(40), None).unwrap();
    extract(&file, Sha256::digest(bytes).into()).unwrap()
}

#[test]
fn golden_files() {
    let bless = std::env::var_os("KVIK_BLESS_GOLDEN").is_some();
    let all: Vec<(&str, Content)> = generated()
        .into_iter()
        .chain(hand_built())
        .chain([("xor-zstd.kvc", xor_zstd())])
        .collect();
    if bless {
        fs::create_dir_all(dir()).unwrap();
        for (name, c) in &all {
            fs::write(dir().join(name), c.write().0).unwrap();
        }
        fs::write(dir().join("l0-current.kvi"), kvi(&read("l0-current.kvc"))).unwrap();
    }
    for (name, c) in generated() {
        let file = read(name);
        assert_eq!(
            check_round_trip(&c),
            file,
            "{name} is what the writer writes now"
        );
        check_canonical(&file, &c.id, &|s| c.info(s)).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    for (name, c) in hand_built() {
        assert_eq!(c.write().0, read(name), "{name}");
    }
    let l0 = read("l0-current.kvc");
    assert_eq!(read("l0-current.kvi"), kvi(&l0));
    let o = IndexObject::open(read("l0-current.kvi"), &id(40)).unwrap();
    assert_eq!(o.sha256(), <[u8; 32]>::from(Sha256::digest(&l0)));
    assert_eq!(
        o.layout(),
        &CompactedFile::open(l0.as_slice(), &id(40), None)
            .unwrap()
            .layout()
    );
}

#[test]
fn the_compressed_xor_golden_reads_back_and_is_canonical_whatever_the_zstd_build() {
    let c = xor_zstd();
    let file = read("xor-zstd.kvc");
    check_canonical(&file, &c.id, &|s| c.info(s)).unwrap();
    let f = CompactedFile::open(file.as_slice(), &id(40), None).unwrap();
    let e = f.series(1).unwrap()[0];
    let d = f.directory(&e).unwrap();
    let rows: Vec<Fact> = f
        .read_chunk(&e)
        .unwrap()
        .into_iter()
        .map(|r| r.fact)
        .collect();
    assert_eq!(rows, c.series[0].facts);
    assert_eq!(value_encoding(&f, &e, &d, 0), encoding::XOR_ZSTD);
}

fn value_encoding(
    f: &CompactedFile<&[u8]>,
    e: &kvik_chunk::file::IndexEntry,
    d: &kvik_chunk::file::Directory,
    i: usize,
) -> u8 {
    let at = (e.chunk_offset + u64::from(d.blocks[i].offset)) as usize + 16;
    let bytes: &[u8] = f.source();
    let body = &bytes[at..at + d.blocks[i].body_len as usize];
    let mut p = 3;
    loop {
        let (id, enc) = (body[p], body[p + 1]);
        p += 2;
        while body[p] & 0x80 != 0 {
            p += 1;
        }
        p += 1;
        if id == kvik_chunk::column::VALUE {
            return enc;
        }
    }
}

fn open(name: &str) -> (Vec<u8>, Vec<kvik_chunk::file::IndexEntry>) {
    let bytes = read(name);
    let index = CompactedFile::open(bytes.as_slice(), &id(40), None)
        .unwrap()
        .index()
        .unwrap();
    (bytes, index)
}

#[test]
fn golden_files_show_what_the_readme_says() {
    let (bytes, index) = open("l0-current.kvc");
    let f = CompactedFile::open(bytes.as_slice(), &id(40), None).unwrap();
    let mut encodings: Vec<u8> = index
        .iter()
        .map(|e| value_encoding(&f, e, &f.directory(e).unwrap(), 0))
        .collect();
    encodings.sort_unstable();
    encodings.dedup();
    assert_eq!(encodings, [0, 1, 2, 3, 4]);

    let levels = [
        ("hour.kvc", Level::Hour),
        ("day.kvc", Level::Day),
        ("week.kvc", Level::Week),
        ("sparse.kvc", Level::Sparse),
    ];
    for (name, level) in levels {
        let b = read(name);
        assert_eq!(
            CompactedFile::open(b.as_slice(), &id(40), None)
                .unwrap()
                .description()
                .level,
            level
        );
    }
    let b = read("week-l0.kvc");
    let d = CompactedFile::open(b.as_slice(), &id(40), None)
        .unwrap()
        .description()
        .clone();
    assert_eq!((d.level, d.period_len), (Level::L0, 7 * DAY));
    let b = read("split-part.kvc");
    let d = CompactedFile::open(b.as_slice(), &id(40), None)
        .unwrap()
        .description()
        .clone();
    assert_eq!(d.name_range, Some(("l/a".into(), "l/b".into())));

    let b = read("entries-two-generations.kvc");
    let rows = CompactedFile::open(b.as_slice(), &id(40), None)
        .unwrap()
        .entries()
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.position.0)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2
    );
    let b = read("entries-only.kvc");
    let f = CompactedFile::open(b.as_slice(), &id(40), None).unwrap();
    assert_eq!(
        (
            f.trailer().chunk_count,
            f.trailer().fact_count,
            f.entries().unwrap().len()
        ),
        (0, 0, 8)
    );
    let b = read("recovery-entry.kvc");
    let rows = CompactedFile::open(b.as_slice(), &id(40), None)
        .unwrap()
        .entries()
        .unwrap();
    assert!(matches!(rows[..], [ref r] if r.entry.is_recovery()));

    let (_, index) = open("set-aside.kvc");
    assert_eq!(
        index.iter().map(|e| e.set_aside()).collect::<Vec<_>>(),
        [false, true]
    );

    let (_, index) = open("decimals.kvc");
    assert_eq!(
        index[0].bounds(),
        Some(Bounds::Decimal {
            scale: 3,
            min: 12_326,
            max: 12_430
        })
    );
    assert_eq!(index[1].bounds(), None);

    let (_, index) = open("nan.kvc");
    assert!(index.iter().all(|e| e.has_nan()));
    assert_eq!(
        index[0].bounds().and_then(Bounds::f64_range),
        Some((-2.5, 6.5))
    );
    assert_eq!(index[1].bounds(), None);

    let (_, index) = open("text-and-orphan.kvc");
    assert!(index.iter().all(|e| e.bounds().is_none() && !e.has_nan()));
    assert_eq!(
        index.iter().map(|e| e.series_id).collect::<Vec<_>>(),
        [1, 900]
    );

    let (_, index) = open("state-bool.kvc");
    assert_eq!(index[0].bounds(), Some(Bounds::State { min: 2, max: 9 }));
    assert_eq!(index[1].bounds(), Some(Bounds::Bool { min: 1, max: 1 }));

    let (_, index) = open("interval-reach.kvc");
    let reach: Vec<i64> = index.iter().map(|e| e.reach - e.max_t).collect();
    assert_eq!(reach, [900 * SECOND, 50 * 3_600 * SECOND, 33 * DAY]);

    let (bytes, index) = open("long-series.kvc");
    let f = CompactedFile::open(bytes.as_slice(), &id(40), None).unwrap();
    assert_eq!(index.len(), 2);
    assert_eq!(
        (
            index[0].row_count,
            f.directory(&index[0]).unwrap().blocks.len()
        ),
        (262_144, 64)
    );
    assert_eq!(index[1].row_count, 1);

    let b = read("many-chunks.kvc");
    let f = CompactedFile::open(b.as_slice(), &id(40), None).unwrap();
    assert_eq!(
        f.root().iter().map(|r| r.entry_count).collect::<Vec<_>>(),
        [1024, 1]
    );
    assert_eq!(f.series(1025).unwrap().len(), 1);

    let b = read("many-batches.kvc");
    let f = CompactedFile::open(b.as_slice(), &id(40), None).unwrap();
    assert_eq!(f.times_directory().len(), 2);
    assert_eq!(f.batches().unwrap().len(), 4097);
    assert_eq!(
        f.batch((1, 4097)).unwrap(),
        Batch {
            generation: 1,
            first_sequence: 4097,
            last_sequence: 4097,
            received_at: T0 + 4096 * SECOND,
            commit_time: T0 + 4096 * SECOND + 2_000_000
        }
    );
}

fn read_all(b: &[u8]) -> Result<(), FileError> {
    let f = CompactedFile::open(b, &id(40), None)?;
    for e in f.index()? {
        f.read_chunk(&e)?;
    }
    f.entries()?;
    f.batches()?;
    Ok(())
}

#[test]
fn unknown_elements_are_skipped_or_refused_by_the_critical_rule() {
    let skipped = [
        ("longer-trailer.kvc", Ignored::TRAILER_TAIL),
        (
            "unknown-ignorable.kvc",
            Ignored::DESCRIPTION_FIELD | Ignored::TIMES_COLUMN | Ignored::DATA_COLUMN,
        ),
    ];
    for (name, bits) in skipped {
        let b = read(name);
        read_all(&b).unwrap_or_else(|e| panic!("{name}: {e}"));
        let c = level(Level::L0, T0, DAY);
        match check_canonical(&b, &id(40), &|s| c.info(s)) {
            Err(CanonicalError::Unknown(i)) => assert_eq!(i, Ignored(bits), "{name}"),
            other => panic!("{name}: {other:?}"),
        }
    }
    let b = read("l0-current.kvc");
    let f = CompactedFile::open(b.as_slice(), &id(40), None).unwrap();
    for e in f.index().unwrap() {
        f.read_chunk(&e).unwrap();
    }
    assert!(f.ignored().is_empty(), "a v1 file skips nothing");
    let refused = |name: &str| match read_all(&read(name)) {
        Err(FileError::Unsupported { element, .. }) => element,
        other => panic!("{name}: {other:?}"),
    };
    assert_eq!(
        refused("unknown-critical-data.kvc"),
        FileElement::Data(Element::Column(0x12))
    );
    assert_eq!(
        refused("unknown-critical-times.kvc"),
        FileElement::TimesColumn(5)
    );
    assert_eq!(
        refused("unknown-critical-description.kvc"),
        FileElement::DescriptionField(14)
    );
    assert_eq!(
        refused("undefined-flag.kvc"),
        FileElement::IndexFlags(1 << 4)
    );
}
