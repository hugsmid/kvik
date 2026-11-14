# kvik-log fuzz targets

A [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) workspace of its own, excluded from the main workspace, so
stable CI never builds it. It needs a nightly toolchain:

```sh
cargo install cargo-fuzz
cd log/fuzz
mkdir -p corpus/segment && cp ../tests/golden/*.open ../tests/golden/*.kvl ../tests/golden/*.mark corpus/segment/
cargo +nightly fuzz run segment
```

| Target | Feeds arbitrary bytes to |
|---|---|
| `segment` | `kvik_log::fuzz::decode_all`: the segment reader to the end and up to several marks, the file header, the mark slots and each body decoder; it also checks that batches and regions are ordered, disjoint and below the limit |

Its `Cargo.lock` is committed, so `libfuzzer-sys` is pinned.

The same entry function runs on stable in `tests/fuzz_inputs.rs` over random bytes and mutated golden files, so the
code path is exercised in CI. The libFuzzer macro needs `unsafe` attributes, so this crate does not forbid unsafe
code; the library it calls does.
