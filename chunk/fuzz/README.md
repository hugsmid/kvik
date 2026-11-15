# kvik-chunk fuzz target

A [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) workspace of its own, excluded from the main workspace, so
stable CI never builds it. It needs a nightly toolchain:

```sh
cargo install cargo-fuzz
cd chunk/fuzz
mkdir -p corpus/block && cp ../tests/golden/*.blk corpus/block/
cargo +nightly fuzz run block
```

| Target | Feeds arbitrary bytes to |
|---|---|
| `block` | `kvik_chunk::fuzz::decode_all`: the block decoder, plain and canonical, and, with a value count taken from the first two bytes, every column decoder (plain, run-length at each width, delta, decimal, XOR, compressed XOR); whatever decodes must encode again to the same content |

Its `Cargo.lock` is committed, so `libfuzzer-sys` is pinned.

The same entry function runs on stable in `tests/fuzz_inputs.rs` over random bytes, generated blocks and mutated
golden files, so the code path is exercised in CI. The libFuzzer macro needs `unsafe` attributes, so this crate does
not forbid unsafe code; the library it calls does.
