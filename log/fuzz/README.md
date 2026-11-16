# kvik-log fuzz targets

A [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) workspace of its own, excluded from the main workspace, so
stable CI never builds it. It needs a nightly toolchain:

```sh
cargo install cargo-fuzz
cd log/fuzz
mkdir -p corpus/segment && cp ../tests/golden/*.open ../tests/golden/*.kvl ../tests/golden/*.mark corpus/segment/
cargo +nightly fuzz run segment
mkdir -p corpus/derived && cp ../tests/golden-derived/*.root ../tests/golden-derived/*.grp \
    ../tests/golden-derived/*.sdir ../tests/golden-derived/*.stat corpus/derived/
cargo +nightly fuzz run derived
```

| Target | Feeds arbitrary bytes to |
|---|---|
| `segment` | `kvik_log::fuzz::decode_all`: the segment reader to the end and up to several marks, the content of every batch it accepts, the file header, the mark slots and each body decoder (facts and entries included, plain and canonical); it also checks that batches and regions are ordered, disjoint and below the limit |
| `derived` | `kvik_log::fuzz::decode_derived`: the log directory's root and group, the series directory and the status file (`format.md` A.18); whatever decodes must encode to bytes that decode to the same value |

Its `Cargo.lock` is committed, so `libfuzzer-sys` is pinned.

The same entry functions run on stable in `tests/fuzz_inputs.rs` over random bytes and mutated golden files, so the
code path is exercised in CI. The libFuzzer macro needs `unsafe` attributes, so this crate does not forbid unsafe
code; the library it calls does.
