# kvik-tree fuzz targets

A [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) workspace of its own, excluded from the main workspace, so
stable CI never builds it. It needs a nightly toolchain:

```sh
cargo install cargo-fuzz
cd tree/fuzz
mkdir -p corpus/node && cp ../tests/golden/*.node corpus/node/
cargo +nightly fuzz run node
mkdir -p corpus/part corpus/root && cp ../tests/golden/*.kvt corpus/part && cp ../tests/golden/*.kvr corpus/root
cargo +nightly fuzz run part
cargo +nightly fuzz run root
```

| Target | Feeds arbitrary bytes to |
|---|---|
| `node` | `kvik_tree::fuzz::decode_node`: the stored-node decoder; whatever decodes must encode again to the same bytes |
| `part` | `kvik_tree::fuzz::decode_part`, raw and after `reframe` recomputes the checksums: the tree part decoder |
| `root` | `kvik_tree::fuzz::decode_root`, raw and after `reframe`: the tree root decoder |

Its `Cargo.lock` is committed, so `libfuzzer-sys` is pinned.

The same entry functions run on stable in `tests/fuzz_inputs.rs` over random bytes, generated nodes, parts and roots, and mutated
golden files, so the code path is exercised in CI. The libFuzzer macro needs `unsafe` attributes, so this crate does
not forbid unsafe code; the library it calls does.
