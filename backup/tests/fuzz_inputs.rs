//! The fuzz entry point (`fuzz/`) run on stable over random bytes, the golden objects and mutations of both and of
//! freshly sealed objects, so the code path of the fuzz target is exercised in CI.

use std::fs;
use std::path::PathBuf;

use kvik_backup::envelope::fuzz::{KEY, OBJECT_KEY, STORE_ID, open_all};
use kvik_backup::envelope::{CHUNK, ChunkedXChaCha, Envelope};
use proptest::prelude::*;

fn golden() -> Vec<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "kve"))
        .collect();
    files.sort();
    files.into_iter().map(|p| fs::read(p).unwrap()).collect()
}

fn sealed(len: usize, fill: u8) -> Vec<u8> {
    ChunkedXChaCha::new(KEY, STORE_ID)
        .seal(OBJECT_KEY, &vec![fill; len])
        .unwrap()
}

#[test]
fn every_golden_object_and_every_prefix_of_one() {
    for g in golden() {
        for n in 0..=g.len() {
            open_all(&g[..n]);
        }
    }
}

#[test]
fn objects_sealed_under_the_fuzz_key_open_and_every_prefix_is_refused() {
    for len in [0, 1, CHUNK, CHUNK + 1] {
        let o = sealed(len, 7);
        open_all(&o);
        for n in [0, 63, 64, 80, o.len() - 1] {
            open_all(&o[..n.min(o.len())]);
        }
    }
}

proptest! {
    #[test]
    fn random_bytes(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        open_all(&data);
    }

    #[test]
    fn mutated_golden_objects(i in any::<prop::sample::Index>(), edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6)) {
        let g = golden();
        let mut b = g[i.index(g.len())].clone();
        for (at, v) in edits {
            let n = b.len();
            b[at.index(n)] = v;
        }
        open_all(&b);
    }

    #[test]
    fn mutated_objects_under_the_fuzz_key(len in 0..3 * CHUNK, fill in any::<u8>(), edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..4), cut in any::<prop::sample::Index>()) {
        let mut b = sealed(len, fill);
        for (at, v) in edits {
            let n = b.len();
            b[at.index(n)] = v;
        }
        let n = b.len();
        b.truncate(cut.index(n + 1));
        open_all(&b);
    }
}
