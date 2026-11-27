//! The fuzz entry points (`fuzz/`) run on stable over random bytes, the golden files, generated nodes, parts and
//! roots, and mutations of them, so the code paths of the fuzz targets are exercised in CI.

mod files;
mod support;

use std::fs;
use std::path::PathBuf;

use kvik_tree::fuzz::{decode_node, decode_part, decode_root, reframe};
use kvik_tree::{encode, fold};
use proptest::prelude::*;
use support::*;

fn golden_of(ext: &str) -> Vec<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
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
    let g = golden_of("node");
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
        let g = golden_of("node");
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

fn tree_file(data: &[u8]) {
    decode_part(data);
    decode_root(data);
    let mut framed = data.to_vec();
    reframe(&mut framed);
    decode_part(&framed);
    decode_root(&framed);
}

fn edit(mut data: Vec<u8>, edits: &[(prop::sample::Index, u8)], cut: prop::sample::Index) {
    for (at, byte) in edits {
        let i = at.index(data.len());
        data[i] = *byte;
    }
    tree_file(&data);
    tree_file(&data[..cut.index(data.len() + 1)]);
}

#[test]
fn every_golden_tree_file_and_prefixes_of_each() {
    let g: Vec<_> = golden_of("kvt")
        .into_iter()
        .chain(golden_of("kvr"))
        .collect();
    assert!(g.len() >= 4);
    for file in &g {
        for k in (0..=file.len()).step_by(13) {
            tree_file(&file[..k]);
        }
    }
}

proptest! {
    #[test]
    fn random_bytes_as_tree_files(data in prop::collection::vec(any::<u8>(), 0..512)) {
        tree_file(&data);
    }

    #[test]
    fn mutated_golden_tree_files(
        which in any::<prop::sample::Index>(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
        cut in any::<prop::sample::Index>(),
    ) {
        let g: Vec<_> = golden_of("kvt").into_iter().chain(golden_of("kvr")).collect();
        edit(g[which.index(g.len())].clone(), &edits, cut);
    }

    #[test]
    fn mutated_generated_parts(
        p in files::generated_part(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
        cut in any::<prop::sample::Index>(),
    ) {
        edit(p.encode().unwrap(), &edits, cut);
    }

    #[test]
    fn mutated_generated_roots(
        r in files::generated_root(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
        cut in any::<prop::sample::Index>(),
    ) {
        edit(r.encode().unwrap(), &edits, cut);
    }
}
