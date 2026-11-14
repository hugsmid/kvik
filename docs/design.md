# kvik — design direction

kvik is planned as an embedded Rust engine for device time series: a durable, ordered log and an analytical
store in one library, with no database service. The engine does not exist yet; this note gives the intended
direction and budgets. The rules are in [`spec.md`](spec.md).

## Requirements
- **Solid first.** Nothing a consumer reacted to may differ from what is on disk. Every block is checksummed. On
  storage that honours sync, a crash at any point loses nothing that was acknowledged; on storage that does not,
  a loss is detected and reported (`possible_loss`, `data_lost`), never silent. One limit is accepted: a store whose
  file system is remounted within one boot can lose the one batch committed since the last mark sync, unreported,
  unless the mark is synced at every commit (spec §3 Recovery rule 1).
- **Fast reaction.** A reading should reach in-process consumers within a few milliseconds of arrival,
  including the sync.
- **Granularity.** Per-second data, with drill-down from years to seconds at interactive speed.
- **Late and corrected data.** Never dropped; summaries recompute; earlier values are kept as revisions.
- **Readable at rest.** A second process must be able to read the files while the engine writes. Parquet export for any
  other tool.
- **Site-neutral.** Calendar buckets use a configured time zone; nothing is hard-coded to a place.

## Intended shape
1. **Log.** Readings are validated, stamped and appended to the log in batches; a batch is synced before
   anything else sees it (group commit; no added delay after a quiet period; log syncs at least
   `min_sync_interval`, default 10 ms, apart).
2. **Delivery.** Only after the sync, funnels deliver to consumers and live state updates. A funnel is a route
   compiled from configuration: series selection, optional windowed operator, condition, consumer. A consumer
   that falls behind catches up from the log; it never blocks the log or other consumers.
3. **Summary tree.** Per series, summaries over time: stored 1-minute and 15-minute nodes, calendar days and
   months in the site zone. The levels serve as roll-ups, as the path for chart drill-down, and as the basis for
   windowed operators: whole nodes in the middle, raw facts at the edges.
4. **Compaction.** Log batches are rearranged into per-series chunks, losslessly, with revisions kept apart.
5. **Tiers.** Hot local disk, an optional cold path, and a deep S3-compatible copy of every sealed file. Data
   moves between tiers only after a verified copy exists; the engine never deletes a fact.

## Segment format (proposed)
One container: a fixed header, then self-delimiting blocks (type, length, CRC32C), and a trailer written on
seal. Log segments hold arrival batches; compacted segments hold per-series chunks with summaries, then an index.
Encodings are those decided by roadmap 2.1 (`format.md` Part B). Safeguards for owning a format: a version in
every header, golden files of every version read in CI, a fuzzed reader, and a differential check against Parquet
throughout development.

Compacted data uses this own chunk format, chosen because it fits the use: damage is confined to a checksummed
block (no single footer to lose), per-file overhead is low for many small per-series chunks, the dependency
footprint is small for embedded use, and decoding is fast for large scans. Parquet remains the export format.

## Budgets (to be measured)
| Budget | Target |
|---|---|
| Reading accepted → delivered to an in-process consumer (includes sync) | p50 ≤ 5 ms, p99 ≤ 20 ms on Linux with an NVMe SSD; p50 ≤ 2 ms as a stretch goal on drives with power-loss protection |
| Funnel routing and condition, per reading | ≤ 1 µs |
| Chart query at any zoom level, for data on local tiers | ≤ 50 ms; results over detail in deep are flagged rather than fetched (spec §6 Old spans) |
| Restart to serving, after a normal stop or a power loss | ≤ 1 s; a restore, and rebuilding derived files after a recorded loss, are excluded |
| Size on disk, per-second numeric series | no larger than Parquet with ALP on the same data |

**Delivery.** macOS is a development platform: its durable sync (`F_FULLFSYNC`) measured 4–6 ms p50 in roadmap
2.2 and is not held to the budget. Windows is a development platform too: it builds, runs and is tested, runs with
the mark synced at every commit (it has no verified boot identity), and is held neither to these budgets nor to the
durability guarantees of spec §3 Sync primitive. The relaxed figure is enough, as device and human reaction times in automation
are tens to hundreds of milliseconds. Roadmap 2.2 found that each durable sync writes a minimum amount, which
unbatched small writers multiply, so log syncs start at least `min_sync_interval` apart (spec §3 Group commit). The
default of 10 ms adds on average under half the interval, and only when syncs are back to back, inside the p99 budget;
the first reading after a pause is synced at once; and it caps host writes from many staggered writers. The p50
≤ 5 ms budget applies while syncs are not back to back, roughly up to 100 synced batches a second; above that rate
batching adds a wait of up to `min_sync_interval` (about half of it at the median) in exchange for fewer syncs, and
a caller who needs lower latency at high rates sets it lower or to 0.

**Size.** [Roadmap 2.1](measurements/2.1-chunk-encodings.md) measured the own encoding 8–14 % smaller than Parquet
with ALP for decimal `f64` data, about 1 % for the same series declared as scaled `i64`, and larger than Parquet's
dictionary on low-cardinality converted values; an `f64` dictionary for such blocks is worth a look. Gate 10.3
confirms the format against Parquet.

## Build order
0. Reference model of the semantics (`model/`): the oracle for everything after it.
1. Log: segment container, arrival batches, sync, recovery, positions, second-process reader.
2. Funnels and live state; delivery benchmark.
3. Compacted segments, index, encodings; differential check against Parquet; Parquet export.
4. Summary tree on disk, late-data updates, revisions, calendar levels.
5. Query integration (a table provider for a query engine) and tier selection.
6. Scale test with billions of rows; confirm the own format against Parquet on size, per-file overhead, recovery
   from a damaged file, binary size and decode speed at scale; Parquet stays for export.

Each step is tested against the reference model.
