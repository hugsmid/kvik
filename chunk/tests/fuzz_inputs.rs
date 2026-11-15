//! The fuzz entry point (`fuzz/`) run on stable over random bytes, the golden files and mutations of both, so the
//! code path of the fuzz target is exercised in CI; also the stable corpus of `tests/golden`.

mod support;

use std::fs;
use std::path::PathBuf;

use kvik_chunk::fuzz::decode_all;
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
