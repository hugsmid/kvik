# Golden files, format v1

Small v1 files that guard the format against accidental change (`docs/format.md` A.3, A.11). They are hand-built,
deterministically, by `generate` in [`../golden.rs`](../golden.rs) through the crate's writer and the hand-laid facts
and entries bodies of [`../support/mod.rs`](../support/mod.rs) (A.5 and A.6, plain encodings, readings in as few
blocks as A.4 allows). The bodies are provisional until roadmap 4.3 adds the batch encoders and checks that they write
the same bytes. Damage is applied by the generator as described below. Every segment has store id
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
| `torn-slot.mark` | A mark whose slot 1 (counter 8) is torn; slot 0 (counter 7) names the end of the second batch of `in-use.open` |

Still to be added by their roadmap items (A.11): recovery entries, a gap before a recovery entry and an adopted
recovery entry (4.5); unknown ignorable and critical columns, entry types and fields (4.3); a stale mark below
acknowledged batches (4.5); the store descriptor and manifests (by the items that write them).
