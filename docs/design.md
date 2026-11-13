# kvik — design direction

kvik is planned as an embedded Rust engine for device time series: a durable, ordered log and an analytical
store in one library, with no database service. The engine does not exist yet; this note gives the intended
direction and budgets. The rules are in [`spec.md`](spec.md).

## Requirements
- **Solid first.** Nothing a consumer reacted to may differ from what is on disk. Every block is checksummed. A
  crash at any point loses nothing that was acknowledged.
- **Fast reaction.** A reading should reach in-process consumers within a few milliseconds of arrival,
  including the sync.
- **Granularity.** Per-second data, with drill-down from years to seconds at interactive speed.
- **Late and corrected data.** Never dropped; summaries recompute; earlier values are kept as revisions.
- **Readable at rest.** A second process must be able to read the files while the engine writes. Parquet export for any
  other tool.
- **Site-neutral.** Calendar buckets use a configured time zone; nothing is hard-coded to a place.

## Intended shape
1. **Log.** Readings are validated, stamped and appended to the log in batches; a batch is synced before
   anything else sees it (group commit when busy, no added delay when idle).
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
Encodings: delta-of-delta timestamps, ALP for decimal values with an XOR fallback, run-length for quality and
rarely changing values. Safeguards for owning a format: a version in every header, golden files of every version
read in CI, a fuzzed reader, and a differential check against Parquet throughout development. If the format does
not clearly beat Parquet on size, compacted data stays in Parquet.

## Budgets (to be measured)
| Budget | Target |
|---|---|
| Reading accepted → delivered to an in-process consumer (includes sync) | p50 ≤ 2 ms, p99 ≤ 10 ms on an SSD |
| Funnel routing and condition, per reading | ≤ 1 µs |
| Chart query at any zoom level | ≤ 50 ms |
| Restart to serving | ≤ 1 s |
| Size on disk, per-second numeric series | clearly smaller than Parquet with ALP |

## Build order
0. Reference model of the semantics (`model/`): the oracle for everything after it.
1. Log: segment container, arrival batches, sync, recovery, positions, second-process reader.
2. Funnels and live state; delivery benchmark.
3. Compacted segments, index, encodings; differential check against Parquet; Parquet export.
4. Summary tree on disk, late-data updates, revisions, calendar levels.
5. Query integration (a DataFusion table provider) and tier selection.
6. Scale test with billions of rows; decide whether compacted data stays in the own format.

Each step is tested against the reference model.
