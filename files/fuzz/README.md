# kvik-files fuzz target

A [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) workspace of its own, excluded from the main workspace, so
stable CI never builds it. It needs a nightly toolchain:

```sh
cargo install cargo-fuzz
cd files/fuzz
mkdir -p corpus/manifest && cp ../tests/golden/*.manifest corpus/manifest/
cargo +nightly fuzz run manifest
```

| Target | Feeds arbitrary bytes to |
|---|---|
| `manifest` | `kvik_files::fuzz::decode_all`: the manifest reader (valid records on both sides of each corrupt one, the torn tail), every record decoder and the file description; it checks that records and corrupt spans tile the file with rising epochs, and that every valid record encodes canonically and reads back as itself |

The same entry function runs on stable in `tests/fuzz_inputs.rs` over random bytes and mutated golden and generated
manifests, so the code path is exercised in CI. The libFuzzer macro needs `unsafe` attributes, so this crate does not
forbid unsafe code; the library it calls does. Its `Cargo.lock` is committed, so `libfuzzer-sys` is pinned.
