# Golden files, manifest records (format v1)

Small v1 manifests that guard the record layouts against accidental change (`docs/format.md` A.3, A.14). They are
hand-built, deterministically, by `generate` in [`../records.rs`](../records.rs) through the crate's encoder. Every
file has store id `03 03 … 03`, generation 0, and wall times from a fixed instant. Hashes are placeholders that name
the file (kind, id), not real SHA-256 digests.

The test checks that each file equals what the generator produces now, decodes to what it shows, and, for the file
without damage, that decoding and encoding again reproduces it byte for byte. Regenerate only after a deliberate
format change: `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-files --test records`.

| File | Shows |
|---|---|
| `every-record.manifest` | One record of each laid-out type: empty (epoch 1); sealed log segments 1 and 2; a publish of compacted file 5 that retires compacted file 3; a lost range (identity 4, positions 4–6, corruption); its extension to 7 (cause unknown, field 12); a rebuild record with `file_id` floors for kinds 1 and 2 and one corrupt span (field 7, epochs 2 to 4); an acknowledgement of the loss and of the rebuild record, carried with its old epoch 3 (field 100) |
| `torn-tail.manifest` | `every-record.manifest` with its last record cut after 24 bytes: a torn tail |
| `corrupt-record.manifest` | `every-record.manifest` with one bit of the third record's body checksum flipped and the records after it intact: a corrupt record (presumed epoch 3) |
