//! Golden tree parts and roots (`tests/golden/README.md`): each file is what the encoder writes now for a fixed part
//! or root, and decodes to it. Regenerate only after a deliberate format change:
//! `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-tree --test golden_files`.

mod files;

use std::fs;
use std::path::PathBuf;

use files::*;
use kvik_tree::part::Part;
use kvik_tree::root::Root;
use kvik_tree::{FileError, Refusal};

fn small_day_part() -> Part {
    let mut p = day_part();
    p.runs.remove(0);
    p
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn check(name: &str, bytes: Vec<u8>) -> Vec<u8> {
    let path = dir().join(name);
    if std::env::var_os("KVIK_BLESS_GOLDEN").is_some() {
        fs::write(&path, &bytes).unwrap();
    }
    let file = fs::read(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(file, bytes, "{name}: the encoder writes other bytes");
    file
}

#[test]
fn golden_parts_are_what_the_encoder_writes_and_decode_to_their_parts() {
    for (name, part) in [
        ("part-day.kvt", small_day_part()),
        ("part-empty.kvt", empty_part()),
    ] {
        let file = check(name, part.encode().unwrap());
        assert_eq!(Part::decode(&file).unwrap(), part, "{name}");
    }
}

#[test]
fn golden_root_is_what_the_encoder_writes_and_decodes_to_its_root() {
    let file = check("root.kvr", sample_root().encode().unwrap());
    assert_eq!(Root::decode(&file).unwrap(), sample_root());
}

#[test]
fn a_newer_major_version_is_a_version_refusal() {
    let mut b = empty_part().encode().unwrap();
    b[5] = 2;
    kvik_tree::fuzz::reframe(&mut b);
    let file = check("part-newer-major.kvt", b);
    assert_eq!(
        Part::decode(&file),
        Err(FileError {
            at: 5,
            refusal: Refusal::NewerMajor
        })
    );
    assert!(Refusal::NewerMajor.is_version());
}
