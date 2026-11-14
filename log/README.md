# kvik-log

The on-disk log format of kvik, as specified in [`docs/format.md`](../docs/format.md) Part A: the file header,
block framing and CRC32C checksums, batches and their commit blocks, the segment index, trailer and end marker, the
durable mark's slots and segment file names.

- `scan` reads a log segment up to the durable mark (a reader process) or to its end (recovery, sealed segments),
  resynchronises past invalid regions (A.2), and reports complete batches, invalid regions with their cause, the
  tail and the seal state. Damage is never an error; a version refusal is.
- `segment` produces every state of a segment's life cycle as bytes (created ahead and zero-filled, in use, sealed,
  zero-filled from a truncation offset) and classifies the states recovery may find (A.4).
- The bodies of facts and entries blocks are opaque here apart from their 24-byte prefix; they are encoded and
  checked by the batch layer (roadmap 4.3).

Tests: unit tests, behaviour tests (`tests/segment.rs`), golden files (`tests/golden/`, see its README), property
tests (`tests/properties.rs`) and the fuzz entry point run over generated inputs (`tests/fuzz_inputs.rs`). The
fuzz targets are in [`fuzz/`](fuzz/).
