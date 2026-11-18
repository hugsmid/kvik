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
- `writer` is the commit path (`spec.md` §3): group commit on a sync budget of four batch syncs refilled at one
  per interval, which follows the sync's duration between `sync_interval_floor` and `min_sync_interval`, one sync
  per batch, the durable mark with its timed and forced syncs, positions, segments created ahead of use, rolled
  and sealed, and a read-only store after any write or sync failure. A synchronous core makes every decision from
  explicit times; `Log` runs it on the `kvik-writer` and `kvik-maint` threads.
- `writer::recovery` is the start (`spec.md` §3 Recovery, Order of a start): rule 1 truncation after the mark on the
  same boot, rule 2 losses recorded in the manifest, rule 3 recovery entries (written or adopted), the last batch
  written again when its sync may have failed, segments sealed again, and the mark. `manifest` reads and appends the
  records a start needs (A.14).
- `reader` is a reader process (`spec.md` §7 Readers): no lock, no thread; it reads the mark by path at every
  refresh, takes batches below the mark's offset only, follows segment rolls, seals, writer restarts and recovery
  entries, and voids what it took when a start truncated it. `lease` is the lease file it renews and the writer's
  view of the lease files: first observation, expiry, void leases, and epochs above every lease at a start (A.15).
- `registry` is the registry file (A.16): the registry entries of the kept log with their positions and an id floor,
  written whole under a new name; a start counts its ids toward the id floor, its `through` toward F and its
  definitions' intervals toward the segment index's reach.
- `fs` is the file-system seam every file operation goes through (A.13), with an in-memory implementation for tests
  that injects faults, logs operations and keeps a durable image of each file.

Tests: unit tests, behaviour tests (`tests/segment.rs`), batch codec tests (`tests/batches.rs`,
`tests/facts_checks.rs`, `tests/entries_checks.rs`, `tests/batch_assembly.rs`), differential tests against the
reference model, of the codecs (`tests/model_differential.rs`) and of the log on generated histories with
crashes, power losses, remounts, failed syncs and reader refreshes (`tests/model_histories.rs`, with a long variant run
by `--ignored`), golden files (`tests/golden/`, see its README), property
tests (`tests/properties.rs`), the commit path driven deterministically (`tests/writer.rs`) and on its threads, with a
smoke test on the real file system (`tests/threads.rs`), recovery and a kill loop (`tests/recovery.rs`, with a long
variant run by `--ignored`), the reader process and leases, on threads and on both file systems, with a property test
interleaving writer steps, crashes and refreshes (`tests/reader.rs`) and the fuzz entry point run over generated inputs (`tests/fuzz_inputs.rs`). The
fuzz targets are in [`fuzz/`](fuzz/).

Benchmark: `examples/commit_latency.rs` measures commit latency and throughput on a real file system (roadmap 4.8,
[results](../docs/measurements/4.8-commit-latency.md)): `cargo run --release -p kvik-log --example commit_latency --
<dir>`.
