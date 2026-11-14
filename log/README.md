# kvik-log

The on-disk log format of kvik, as specified in [`docs/format.md`](../docs/format.md) Part A: the file header,
block framing and CRC32C checksums, batches and their commit blocks, the segment index, trailer and end marker, the
durable mark's slots and segment file names.

- `scan` reads a log segment up to the durable mark (a reader process) or to its end (recovery, sealed segments),
  resynchronises past invalid regions (A.2), and reports complete batches, invalid regions with their cause, the
  tail and the seal state. Damage is never an error; a version refusal is.
- `segment` produces every state of a segment's life cycle as bytes (created ahead and zero-filled, in use, sealed,
  zero-filled from a truncation offset) and classifies the states recovery may find (A.4).
- `facts`, `entries` and `content` encode and decode arrival batches as typed values (A.5, A.6, A.7): canonical
  encoders, decoders that apply every check, and canonical decoders that also require the canonical bytes. The
  reader applies the body checks to every batch it accepts.
- `store` creates and opens a store directory (A.12): the store descriptor, the lock and the layout.
- `writer` is the commit path (`spec.md` §3): group commit bounded by `min_sync_interval`, one sync per batch, the
  durable mark with its timed and forced syncs, positions, segments created ahead of use, rolled and sealed, and a
  read-only store after any write or sync failure. A synchronous core makes every decision from explicit times;
  `Log` runs it on the `kvik-writer` and `kvik-maint` threads. Opening a log that already holds data is recovery
  (roadmap 4.5).
- `fs` is the file-system seam every file operation goes through (A.13), with an in-memory implementation for tests
  that injects faults, logs operations and keeps a durable image of each file.

Tests: unit tests, behaviour tests (`tests/segment.rs`), batch codec tests (`tests/batches.rs`), a differential test
against the reference model (`tests/model_differential.rs`), golden files (`tests/golden/`, see its README), property
tests (`tests/properties.rs`), the commit path driven deterministically (`tests/writer.rs`) and on its threads, with a
smoke test on the real file system (`tests/threads.rs`) and the fuzz entry point run over generated inputs (`tests/fuzz_inputs.rs`). The
fuzz targets are in [`fuzz/`](fuzz/).
