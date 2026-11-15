# Golden block bodies, format v1

Small block bodies (`docs/format-chunk.md`) that guard the encodings against accidental change (`docs/format.md`
A.3). They are hand-built, deterministically, by [`../golden.rs`](../golden.rs) from synthetic rows (no recorded
data). The test checks that each file decodes to the rows it was built from, is canonical, and, except for the
compressed XOR payload (Q-150), is byte for byte what the encoder writes now. Regenerate only after a deliberate
format change: `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-chunk --test golden`.

| File | Shows |
|---|---|
| `decimal.blk` | 300 `f64` readings at 0.01 resolution: scaled decimal (encoding 3) with exceptions for −0.0, 0.1 + 0.2, a NaN with a payload and a signalling NaN |
| `xor.blk` | 40 `f64` values with random signs and 20 random mantissa bits: XOR (encoding 4) |
| `xor-zstd.blk` | 2,000 `f64` values from five non-decimal readings: XOR compressed with zstd (encoding 5) |
| `scaled-i64.blk` | A decimal counter at scale 3, raised to 4 from row 50, two rows flagged `rounded`: value delta, scale run-length |
| `state.blk` | State indexes 0, 3 and 65,535 in long runs: run-length |
| `bool.blk` | A bool switching every 97 rows |
| `text.blk` | Texts, two empty, two with multi-byte UTF-8, and one row without a value |
| `versioned.blk` | Two issues of a 15-minute forecast: flag bit 6 and `changed_at` on every row |
| `gappy.blk` | Host-stamped `t` with nanosecond jitter, `host_time` on every row, a suspect run, rows without a value, a `late` row |
| `mixed-types.blk` | Rows of all five value types in one block: the value column in a general encoding |
| `generations.blk` | Positions in generation 1, then in generation 2 from sequence 1 |
| `full.blk` | 4,096 rows: a full block |
| `unknown-ignorable.blk` | Five rows and an unknown ignorable column `0x90`: read, skipped, not canonical |
| `unknown-critical.blk` | Five rows and an unknown critical column `0x12`: a version refusal |
