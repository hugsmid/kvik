//! The fuzz entry point (`fuzz/`) run on stable over random bytes, the golden files and mutations of both, so the
//! code path of the fuzz target is exercised in CI.

mod support;

use std::fs;
use std::path::PathBuf;

use kvik_files::fuzz::decode_all;
use proptest::prelude::*;
use support::*;

fn golden() -> Vec<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "manifest"))
        .collect();
    files.sort();
    files.into_iter().map(|p| fs::read(p).unwrap()).collect()
}

#[test]
fn every_golden_file_and_every_prefix_of_one() {
    for g in golden() {
        for n in 0..=g.len() {
            decode_all(&g[..n]);
        }
    }
}

proptest! {
    #[test]
    fn random_bytes(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        decode_all(&data);
    }

    #[test]
    fn mutated_golden_files(i in any::<prop::sample::Index>(), edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6)) {
        let g = golden();
        let mut b = g[i.index(g.len())].clone();
        for (at, v) in edits {
            let n = b.len();
            b[at.index(n)] = v;
        }
        decode_all(&b);
    }

    #[test]
    fn mutated_generated_manifests(h in history(), edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..4), cut in any::<prop::sample::Index>()) {
        let mut b = manifest_bytes(&h.records);
        for (at, v) in edits {
            let n = b.len();
            b[at.index(n)] = v;
        }
        let n = b.len();
        b.truncate(64 + cut.index(n - 63));
        decode_all(&b);
    }
}
