# Golden compacted segments and chunk index object, format v1

Whole files (`docs/format-compacted.md`) that guard the layout against accidental change (`docs/format.md` A.3). They
are hand-built, deterministically, by [`../../file_golden.rs`](../../file_golden.rs) from synthetic facts (no
recorded data). The test checks that each canonical file is byte for byte what the writer writes now, reads back to
the content it was built from (facts with positions and batch times, entries, statistics, batch rows, layout), passes
the canonical checks and extracts to a chunk index object that reads back. Regenerate only after a deliberate format
change: `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-chunk --test file_golden`.

| File | Shows |
|---|---|
| `l0-current.kvc` | An L0 current file: bool, decimal, state, `f64` and text series, value encodings plain, run-length, delta, scaled decimal and XOR, a text row without a value |
| `l0-current.kvi` | The chunk index object of `l0-current.kvc` (kind 14), with its length and SHA-256 |
| `hour.kvc`, `day.kvc`, `week.kvc`, `sparse.kvc` | One file of each merge level, with compacted inputs; the week and 13-week periods start on their UTC boundaries |
| `week-l0.kvc` | An L0 file whose period is a week |
| `split-part.kvc` | A day file that is a split part, with its first and last series name |
| `revision.kvc` | A revision file: three facts per `t` |
| `entries-two-generations.kvc` | Facts and entries in generations 1 and 2: two generation markers |
| `entries-only.kvc` | Entries and no chunk: no index block, `index_offset` 0 |
| `recovery-entry.kvc` | A recovery entry alone in its batch and its block |
| `set-aside.kvc` | A revision file with a set-aside chunk beside the series' other chunk |
| `decimals.kvc` | A decimal chunk across a scale raise (bounds at scale 3) and one whose rescaled bounds overflow (no bounds) |
| `nan.kvc` | A chunk with one NaN word (bounds and `has_nan`) and one of NaN words only (`has_nan`, no bounds) |
| `text-and-orphan.kvc` | A text chunk and an orphaned chunk (no name) of five value types: no bounds |
| `state-bool.kvc` | State and bool bounds |
| `interval-reach.kvc` | The reach of fixed, `day` and `month` interval series |
| `long-series.kvc` | One series of 262,145 rows: a full chunk of 64 blocks and a chunk of one row |
| `many-chunks.kvc` | 1,025 chunks: two index blocks |
| `many-batches.kvc` | 4,097 batches: two commit-times blocks |
| `xor-zstd.kvc` | An `f64` chunk in compressed XOR (encoding 5): compared by what it reads back to and by the canonical checks, since compressed bytes are a writer's choice (`docs/format-chunk.md` C.8) |
| `longer-trailer.kvc` | A trailer with 8 bytes beyond v1's: read, not canonical |
| `unknown-ignorable.kvc` | An ignorable description field `0x8001`, commit-times column `0x90` and data column `0x90`: read, not canonical |
| `unknown-critical-data.kvc` | A critical data column `0x12`: a version refusal |
| `unknown-critical-times.kvc` | A critical commit-times column `0x05`: a version refusal |
| `unknown-critical-description.kvc` | A critical description field 13: a version refusal at open |
| `undefined-flag.kvc` | Chunk flag bit 4 in the directory and index entry: a version refusal |
