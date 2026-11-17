# kvik-chunk fuzz targets

A [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) workspace of its own, excluded from the main workspace, so
stable CI never builds it. It needs a nightly toolchain:

```sh
cargo install cargo-fuzz
cd chunk/fuzz
mkdir -p corpus/block && cp ../tests/golden/*.blk corpus/block/
cargo +nightly fuzz run block
mkdir -p corpus/compacted && cp ../tests/golden/compacted/*.kvc corpus/compacted/
cargo +nightly fuzz run compacted
mkdir -p corpus/chunk_index && cp ../tests/golden/compacted/*.kvi corpus/chunk_index/
cargo +nightly fuzz run chunk_index
cargo +nightly fuzz run file_bodies
```

| Target | Feeds arbitrary bytes to |
|---|---|
| `block` | `kvik_chunk::fuzz::decode_all`: the block decoder, plain and canonical, and, with a value count taken from the first two bytes, every column decoder (plain, run-length at each width, delta, decimal, XOR, compressed XOR); whatever decodes must encode again to the same content |
| `compacted` | `kvik_chunk::fuzz::decode_compacted`: the compacted segment reader on every path (index, chunks, entries, commit times, as-of time), the layout comparison and the canonical checks; a file that opens extracts to a chunk index object with the same layout and index |
| `chunk_index` | `kvik_chunk::fuzz::decode_index_object`: the chunk index object reader and its index |
| `file_bodies` | `kvik_chunk::fuzz::decode_file_bodies`: the description, chunk directory, index block, root, commit-time directory, commit-times, trailer and index object trailer decoders; whatever decodes must encode and decode again to itself |

Its `Cargo.lock` is committed, so `libfuzzer-sys` is pinned.

The same entry functions run on stable in `tests/fuzz_inputs.rs` over random bytes, generated blocks and files, and
mutated golden files, so the code path is exercised in CI. The libFuzzer macro needs `unsafe` attributes, so this crate does
not forbid unsafe code; the library it calls does.
