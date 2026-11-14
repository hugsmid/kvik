//! The fuzz entry point (`fuzz/`) run on stable over random bytes, the golden files and mutations of both, so the
//! code path of the fuzz target is exercised in CI.

mod support;

use std::fs;
use std::path::PathBuf;

use kvik_log::fuzz::decode_all;
use proptest::prelude::*;
use support::*;

fn golden() -> Vec<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x != "md"))
        .collect();
    files.sort();
    files.into_iter().map(|p| fs::read(p).unwrap()).collect()
}

/// A seed: a golden file, or a small generated segment (sealed or not).
fn seed(i: usize) -> Vec<u8> {
    let g = golden();
    if i % (g.len() + 2) < g.len() {
        return g[i % (g.len() + 2)].clone();
    }
    let (mut s, _) = segment(ID, 1_024, 1, &three_batches());
    if i.is_multiple_of(2) {
        let end = s.write_seal_blocks(vec![]).unwrap();
        s.set_len(end);
    }
    s.into_bytes()
}

#[test]
fn every_golden_file_and_every_prefix_of_one() {
    for g in golden() {
        decode_all(&g);
    }
    let sealed =
        fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/sealed.kvl"))
            .unwrap();
    for k in 0..=sealed.len() {
        decode_all(&sealed[..k]);
    }
}

proptest! {
    #[test]
    fn random_bytes(data in prop::collection::vec(any::<u8>(), 0..4_096)) {
        decode_all(&data);
    }

    #[test]
    fn mutated_seeds(
        which in any::<usize>(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
        cut in prop::option::of(any::<prop::sample::Index>()),
    ) {
        let mut data = seed(which);
        for (at, v) in edits {
            let i = at.index(data.len());
            data[i] = v;
        }
        if let Some(c) = cut {
            data.truncate(c.index(data.len() + 1));
        }
        decode_all(&data);
    }

    #[test]
    fn spliced_seeds(a in any::<usize>(), b in any::<usize>(), at in any::<prop::sample::Index>()) {
        // The start of one file followed by the rest of another: valid structures at unexpected offsets.
        let (x, y) = (seed(a), seed(b));
        let i = at.index(x.len().min(y.len()) + 1);
        let mut data = x[..i].to_vec();
        data.extend_from_slice(&y[i..]);
        decode_all(&data);
    }
}
