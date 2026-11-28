# Golden stored nodes, format v1

Stored nodes (`docs/format-tree.md`) that guard the layout against accidental change (`docs/format.md` A.3). They are
hand-built, deterministically, by [`../golden.rs`](../golden.rs) from synthetic readings (no recorded data). The test
checks that each file is byte for byte what the encoder writes now and decodes to the node it was built from.
Regenerate only after a deliberate format change: `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-tree --test golden`.

| File | Shows |
|---|---|
| `gauge-empty.node` | The empty gauge node of a minute |
| `gauge.node` | Six gauge readings with S = 20 s: −0.0, a subnormal and 1e300, so the sum and integral take long mantissas |
| `gauge-one.node` | One reading: no last, min equal to max, no coverage |
| `gauge-quarter.node` | A 15-minute node, the left-to-right merge of fifteen 1-minute nodes of 12 readings each |
| `counter.node` | A counter with a reset, a regression and a step longer than S |
| `counter-wrap.node` | A counter wrapping at 65,536 |
| `state.node` | Named states by index, 65,535 among them, with transitions and a time map |
| `state-bool.node` | A boolean state |
| `text.node` | An empty text and one with multi-byte UTF-8 |
| `interval.node` | Three 5-minute interval readings in a 15-minute node |
| `unknown-form.node` | `gauge.node` with form `0x11`: a version refusal |

## Tree parts and roots

Built by [`../golden_files.rs`](../golden_files.rs) from the synthetic parts in [`../files/mod.rs`](../files/mod.rs)
(`docs/format-tree.md` T.8 to T.12). Regenerate with `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-tree --test golden_files`.

| File | Shows |
|---|---|
| `part-day.kvt` | A merged day part: quarter-hours of a gauge with a gap, stale entries with and without a node, an empty marker and an `as_of` of an older generation; counter roll-ups at 2 hours and a day; a state series |
| `part-empty.kvt` | A delta part with no entries: no nodes or directory blocks, an empty directory root |
| `part-newer-major.kvt` | `part-empty.kvt` with format major 2: a version refusal |
| `root.kvr` | A root naming three parts, covered ranges at three levels, a series floor, an open floor and two retired parts |
