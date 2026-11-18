//! The fuzz entry point (`fuzz/`) run on stable over random bytes, the golden files and mutations of both, so the
//! code path of the fuzz target is exercised in CI; also the stable corpus of `tests/golden`.

mod compacted;
mod support;

use std::fs;
use std::path::PathBuf;

use kvik_chunk::fuzz::{
    decode_all, decode_compacted, decode_compacted_resealed, decode_file_bodies,
    decode_index_object, decode_index_object_resealed,
};
use proptest::prelude::*;
use support::*;

fn golden() -> Vec<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "blk"))
        .collect();
    files.sort();
    files.into_iter().map(|p| fs::read(p).unwrap()).collect()
}

/// The compacted golden files below 64 KiB.
fn golden_files() -> Vec<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/compacted");
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "kvc" || x == "kvi"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| fs::read(p).unwrap())
        .filter(|b| b.len() < 64 << 10)
        .collect()
}

fn decode_any_file(data: &[u8]) {
    decode_compacted(data);
    decode_index_object(data);
    decode_file_bodies(data);
}

#[test]
fn every_compacted_golden_file_and_prefixes_of_one() {
    let g = golden_files();
    assert!(g.len() >= 25);
    for file in &g {
        decode_any_file(file);
        for k in (0..file.len()).step_by(file.len() / 97 + 1) {
            decode_any_file(&file[..k]);
            decode_file_bodies(&file[k..]);
        }
    }
}

#[test]
fn every_golden_file_and_every_prefix_of_one() {
    let g = golden();
    assert!(g.len() >= 14);
    for file in &g {
        decode_all(file);
        for k in 0..file.len().min(600) {
            decode_all(&file[..k]);
        }
    }
}

proptest! {
    #[test]
    fn random_bytes(data in prop::collection::vec(any::<u8>(), 0..2_048)) {
        decode_all(&data);
    }

    #[test]
    fn mutated_golden_files(
        which in any::<prop::sample::Index>(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
        cut in any::<prop::sample::Index>(),
    ) {
        let g = golden();
        let mut data = g[which.index(g.len())].clone();
        for (at, byte) in edits {
            let i = at.index(data.len());
            data[i] = byte;
        }
        decode_all(&data);
        let k = cut.index(data.len() + 1);
        decode_all(&data[..k]);
    }

    #[test]
    fn random_bytes_as_file_bodies(data in prop::collection::vec(any::<u8>(), 0..1_024)) {
        decode_any_file(&data);
    }

    #[test]
    fn generated_files_and_mutations_of_them(
        s in compacted::spec(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..4),
    ) {
        let mut data = compacted::build(&s).0.write().0;
        decode_compacted(&data);
        for (at, byte) in edits {
            let i = at.index(data.len());
            data[i] = byte;
        }
        decode_any_file(&data);
    }

    #[test]
    fn resealed_edits_of_generated_files_never_panic(
        s in compacted::spec(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
    ) {
        let c = compacted::build(&s).0;
        let mut data = c.write().0;
        let object = {
            let f = kvik_chunk::file::CompactedFile::open(data.as_slice(), &c.id, None).unwrap();
            kvik_chunk::file::extract(&f, [5; 32]).unwrap()
        };
        let mut obj = object.clone();
        for (at, byte) in &edits {
            let i = at.index(data.len());
            data[i] = *byte;
            let j = at.index(obj.len());
            obj[j] = *byte;
        }
        decode_compacted_resealed(&data);
        decode_index_object_resealed(&obj);
    }

    #[test]
    fn mutated_compacted_golden_files(
        which in any::<prop::sample::Index>(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
        cut in any::<prop::sample::Index>(),
    ) {
        let g = golden_files();
        let mut data = g[which.index(g.len())].clone();
        for (at, byte) in edits {
            let i = at.index(data.len());
            data[i] = byte;
        }
        decode_any_file(&data);
        decode_compacted_resealed(&data);
        decode_index_object_resealed(&data);
        let k = cut.index(data.len() + 1);
        decode_any_file(&data[..k]);
    }

    #[test]
    fn mutated_generated_blocks(
        b in any_block(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
    ) {
        let mut data = b.encode().unwrap();
        for (at, byte) in edits {
            let i = at.index(data.len());
            data[i] = byte;
        }
        decode_all(&data);
    }
}
