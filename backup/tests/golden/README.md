# Golden files, envelopes (format v1)

Sealed objects that guard the envelope layout of `docs/format.md` A.17 against accidental change. They are
hand-built, deterministically, by `golden_objects_are_unchanged_and_open` in [`../envelope.rs`](../envelope.rs)
through `ChunkedXChaCha`, under the test key `42 42 … 42` and store id `03 03 … 03` (except `fuzz-seed.kve`); none is a real key.

The test checks that each file equals what sealing produces now and opens to its plaintext. Objects of one full
chunk (65,536 bytes) and of two chunks and one byte are pinned by their SHA-256 in the same test rather than
committed. Regenerate only after a deliberate format change:
`KVIK_BLESS_GOLDEN=1 cargo test -p kvik-backup --test envelope`.

| File | Object key | Plaintext |
|---|---|---|
| `empty.kve` | `<sid>/STORE` | empty: one empty chunk, 80 bytes |
| `short.kve` | `<sid>/claims/00000001` | bytes 0 to 255, then ASCII `kvik`: one short final chunk |
| `fuzz-seed.kve` | the fuzz target's object key | bytes 0 to 99, sealed under the fuzz target's key and store id (`kvik_backup::envelope::fuzz`), so a fuzz corpus starts from an object that opens |
