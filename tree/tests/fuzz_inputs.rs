//! The fuzz entry point (`fuzz/`) run on stable over random bytes, the golden files, generated nodes and mutations
//! of both, so the code path of the fuzz target is exercised in CI.

mod support;

use std::fs;
use std::path::PathBuf;

use kvik_tree::fuzz::decode_node;
use kvik_tree::{encode, fold};
use proptest::prelude::*;
use support::*;

fn golden() -> Vec<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "node"))
        .collect();
    files.sort();
    files.into_iter().map(|p| fs::read(p).unwrap()).collect()
}

fn mutate(mut data: Vec<u8>, edits: &[(prop::sample::Index, u8)], cut: prop::sample::Index) {
    for (at, byte) in edits {
        if !data.is_empty() {
            let i = at.index(data.len());
            data[i] = *byte;
        }
    }
    decode_node(&data);
    decode_node(&data[..cut.index(data.len() + 1)]);
}

#[test]
fn every_golden_file_and_every_prefix_of_one() {
    let g = golden();
    assert!(g.len() >= 11);
    for file in &g {
        for k in 0..=file.len() {
            decode_node(&file[..k]);
        }
    }
}

proptest! {
    #[test]
    fn random_bytes(data in prop::collection::vec(any::<u8>(), 0..512)) {
        decode_node(&data);
    }

    #[test]
    fn mutated_golden_files(
        which in any::<prop::sample::Index>(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
        cut in any::<prop::sample::Index>(),
    ) {
        let g = golden();
        mutate(g[which.index(g.len())].clone(), &edits, cut);
    }

    #[test]
    fn mutated_generated_nodes(
        c in case(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
        cut in any::<prop::sample::Index>(),
    ) {
        let bytes = encode(&fold(&c.p, c.start, c.end, &c.readings)).unwrap();
        decode_node(&bytes);
        mutate(bytes, &edits, cut);
    }
}
