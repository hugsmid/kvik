# Golden files, format v1

Small v1 files that guard the format against accidental change (`docs/format.md` A.3, A.11). They are hand-built,
deterministically, by `generate` in [`../golden.rs`](../golden.rs) through the crate's writer and the facts and entries
bodies hand-laid from A.5 and A.6 in [`../support/mod.rs`](../support/mod.rs) (plain encodings, readings in as few
blocks as A.4 allows). Since roadmap 4.3 a test checks that these are exactly the bodies the crate's encoders write, and
that every batch of the files without damage is canonical: the bodies are re-encoded identically. Damage is applied
by the generator as described below. Every segment has store id
`01 02 … 10`, file id 1 and generation 0; segments are shorter than a real `segment_size`.

The tests check that each file equals what the generator produces now, decodes to what it shows, and, for the files
without damage, that decoding and encoding again reproduces it byte for byte. Regenerate only after a deliberate
format change: `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-log --test golden`.

| File | Shows |
|---|---|
| `created-ahead.open` | A segment created ahead of use: header with `base_sequence = 0`, zero-filled |
| `in-use.open` | Three batches (a series definition; two readings; an alias and a reading, in two blocks), then zeros |
| `torn-tail.open` | `in-use.open` with the third batch torn: only its first 64 bytes remain |
| `corrupt-middle.open` | `in-use.open` with one bit of the second batch's facts body flipped; the third batch is kept |
| `gap.open` | Two batches with a gap in sequences (1, then 10–11) |
| `sealed.kvl` | `in-use.open` sealed: segment index, trailer and end marker; the file ends at the end marker |
| `seal-length-pending.open` | Seal step 1 done, step 2 (setting the length) not: zeros follow the end marker |
| `seal-torn.open` | Seal blocks torn before the end marker: not sealed |
| `unknown-skippable.open` | A batch holding a block of unknown type `0x50` with `skippable` set: skipped and counted |
| `unknown-critical.open` | A batch holding a block of unknown type `0x51` without `skippable`: a version refusal |
| `unknown-ignorable-content.open` | A facts block with an unknown ignorable column `0x90`, and an entries block with an entry of unknown ignorable type `0x90` and an alias with an unknown ignorable field `0x8001`: all skipped, the ignored entry still takes its sequence |
| `unknown-critical-column.open` | A facts block with an unknown critical column `0x10`: a version refusal |
| `unknown-critical-entry.open` | An entry of unknown critical type `0x10`: a version refusal |
| `unknown-critical-field.open` | An alias with an unknown critical field `0x0100`: a version refusal |
| `torn-slot.mark` | A mark whose slot 1 (counter 8) is torn; slot 0 (counter 7) names the end of the second batch of `in-use.open` |

Still to be added by their roadmap items (A.11): recovery entries, a gap before a recovery entry and an adopted
recovery entry (4.5); a stale mark below
acknowledged batches (4.5); the store descriptor and manifests (by the items that write them).
