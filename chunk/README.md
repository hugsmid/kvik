# kvik-chunk

Column encodings and block bodies of kvik's compacted chunks, laid out in
[`docs/format-chunk.md`](../docs/format-chunk.md) (roadmap 6.1), and the compacted segment and chunk index object
around them, laid out in [`docs/format-compacted.md`](../docs/format-compacted.md) (roadmap 6.2a1). Part of the kvik
storage engine; not published on its own.

- `t`, positions and `changed_at`: delta, frame-of-reference bit-packed per 128 values.
- `f64` values: scaled decimal with per-value exceptions, else XOR, compressed with zstd where that is smaller; every
  bit pattern round-trips.
- Decimal, state and bool values and text lengths: delta, run-length or plain; quality, flags, value type, scale and
  generation: run-length or plain.

`Block::encode` writes the one canonical body for given rows; `Block::decode` applies the checks of C.11 and never
panics or allocates from an unchecked length; `Block::decode_canonical` also requires the canonical body. The column
codecs are public in `ints` and `float`.

```rust
use kvik_chunk::{Block, value_type};

let block = Block {
    t: vec![1_000_000_000, 2_000_000_000],
    generation: vec![1, 1],
    sequence: vec![7, 9],
    quality: vec![0, 0],
    flags: vec![0, 0],
    value_type: vec![value_type::F64; 2],
    value: vec![21.37f64.to_bits(), 21.4f64.to_bits()],
    heap: vec![],
    changed_at: vec![0, 0],
    scale: vec![0, 0],
};
let body = block.encode().unwrap();
assert_eq!(Block::decode_canonical(&body).unwrap(), block);
```

The `file` module writes a compacted segment one series at a time (`FileWriter`), reads it by position with every
block bound to its parent's checksum (`CompactedFile`), extracts and reads its chunk index object (`extract`,
`IndexObject`) and applies the canonical checks (`check_canonical`); the module's documentation has an example.

Tests: property tests (`tests/properties.rs`, `tests/file_properties.rs`), damage and binding
(`tests/file_damage.rs`), golden bodies and files (`tests/golden/`, `tests/golden/compacted/`), the fuzz entries over
random and mutated inputs (`tests/fuzz_inputs.rs`), and cargo-fuzz targets in [`fuzz/`](fuzz/README.md). The size check against roadmap
2.1 is `cargo run --release -p encodings --example chunk_sizes` ([`docs/measurements/6.1-encodings.md`](../docs/measurements/6.1-encodings.md)).
