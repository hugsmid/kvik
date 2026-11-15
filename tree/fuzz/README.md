# kvik-tree fuzz target

A [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) workspace of its own, excluded from the main workspace, so
stable CI never builds it. It needs a nightly toolchain:

```sh
cargo install cargo-fuzz
cd tree/fuzz
mkdir -p corpus/node && cp ../tests/golden/*.node corpus/node/
cargo +nightly fuzz run node
```

| Target | Feeds arbitrary bytes to |
|---|---|
| `node` | `kvik_tree::fuzz::decode_node`: the stored-node decoder; whatever decodes must encode again to the same bytes |

Its `Cargo.lock` is committed, so `libfuzzer-sys` is pinned.

The same entry function runs on stable in `tests/fuzz_inputs.rs` over random bytes, generated nodes and mutated
golden files, so the code path is exercised in CI. The libFuzzer macro needs `unsafe` attributes, so this crate does
not forbid unsafe code; the library it calls does.
