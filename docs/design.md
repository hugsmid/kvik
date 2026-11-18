# kvik — design direction

kvik is planned as an embedded Rust engine for device time series: a durable, ordered log and an analytical
store in one library, with no database service. The engine does not exist yet; this note gives the intended
direction and budgets. The rules are in [`spec.md`](spec.md).

## Requirements
- **Solid first.** Nothing a consumer reacted to may differ from what is on disk. Every block is checksummed. On
  storage that honours sync, a crash at any point loses nothing that was acknowledged; on storage that does not,
  a loss is detected and reported (`possible_loss`, `data_lost`), never silent. One limit is accepted: a store whose
  file system is remounted within one boot can lose the one batch committed since the last mark sync, reported as
  `possible_loss` and delivered as a reset point, unless the mark is synced at every commit (spec §3 Recovery rule 1).
- **Fast reaction.** A reading should reach in-process consumers within a few milliseconds of arrival,
  including the sync.
- **Granularity.** Per-second data, with drill-down from years to seconds at interactive speed.
- **Late and corrected data.** Never dropped; summaries recompute; earlier values are kept as revisions.
- **Readable at rest.** A second process must be able to read the files while the engine writes. Parquet export for any
  other tool.
- **Site-neutral.** Calendar buckets use a configured time zone; nothing is hard-coded to a place.

## Intended shape
1. **Log.** Readings are validated, stamped and appended to the log in batches; a batch is synced before
   anything else sees it (group commit; a batch with nothing in flight is synced at once while the sync budget
   holds a sync, and batch syncs run at most one per interval, at least 2 ms, plus a burst of four), except a
   batch of series at the faster durability level, seen once written, its sync starting within `max_unsynced`
   (spec §2.1 `durability`).
2. **Delivery.** Only after its acknowledgement (the sync, or the write at the faster level), funnels deliver to
   consumers and live state updates. A funnel is a route
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
unbatched small writers multiply, so batch syncs draw on a sync budget (spec §3 Group commit, Q-980): four syncs,
refilled at one per interval, eight times the smoothed sync time between `sync_interval_floor` (2 ms) and
`min_sync_interval` (10 ms). It caps host writes for every load, staggered writers included, at 500 batch syncs a
second on a fast device and 100 on one whose sync takes 1.25 ms or more. Under the budget's rate a batch with
nothing in flight is synced at once, so a lone writer or staggered callers wait only for their own sync; above it,
readings queue and the sync at each refill covers all of them, so a reading waits for up to one interval and then
its sync. The p50 ≤ 5 ms budget therefore holds under the budget's rate wherever the sync itself is well inside
5 ms, and above that rate only while half the interval plus the sync stays under 5 ms: on the NVMe target (sync
about 1.1 ms, interval about 8.8 ms) a load above about 110 batch syncs a second, such as one writer that waits
for each acknowledgement, lies outside the p50 budget (Q-983). The p99 holds while an interval plus two syncs stays
under 20 ms. A caller who needs lower latency at high rates lowers the floor or sets `min_sync_interval` to 0,
which disables the budget, at the cost in host writes of spec §13.

**Size.** [Roadmap 2.1](measurements/2.1-chunk-encodings.md) measured the own encoding 8–14 % smaller than Parquet
with ALP for decimal `f64` data, about 1 % for the same series declared as scaled `i64`, and larger than Parquet's
dictionary on low-cardinality converted values; an `f64` dictionary for such blocks is worth a look. Gate 10.3
confirms the format against Parquet.

**Crafted files.** The restart budget covers files the engine wrote, damaged or not; an undamaged segment is read in one
pass of direct checksums (Q-231). A file crafted by a process with write access to the store has its cost bounded by
`format.md` A, common rules (scan bound; Q-110): about 2L bytes checksummed and 16 block headers per 8 bytes for a file
of L bytes, and failed full reads checking at most 16 MiB of body per window (8 MiB in a segment, 16 MiB in the
manifest), past which the rest of the window is passed as damaged and reported with the damage around it; damage does
not reach that budget. Each kept body is checked once, and the checks keep nothing and cost a bounded time per byte
(Q-651). Measured in a release build on one shared 4-core cloud machine, against the bound (264 MiB of bodies for a 64
MiB segment): the body checks of a crafted segment cost at most about 0.8 s at 64 MiB and 0.22 s at 16 MiB, and those of
a crafted manifest about 0.09 s per 16 MiB at a start and 0.15 s at a refresh, within the 1 s restart budget. The header
walk is bounded in reads per byte (Q-873): once damage is found a header checksum is taken at most twice per 8-byte step
(once by the chain ends and once by a torn tail's commit search), and a block's header is read at most eight times, each
block judged once by the boundary reads and each step's chain end kept in a ring of 4-byte entries. Time is measured,
not bounded: a header read costs about 20 to 31 ns, and the dearest crafted 64 MiB segments built cost 0.25 to 0.61 s
(best of five, the worst up to 40 % above; a boundary read failing at the end of a chain of 500,000 empty blocks that
the candidate then keeps is the dearest, 2.0 s before for the failing chain alone), the same blocks undamaged 0.25 to
0.37 s; a crafted manifest's header walk 0.19 to 0.30 s per 16 MiB (1.2 to 1.3 s before). What a start is promised
against the budget is this measured figure per file at the default `segment_size` of 64 MiB, not a bound in seconds
(Q-970): a larger segment costs about `segment_size / 64 MiB` times as much, the 1 GiB maximum some 10 s, and a start
pays for each crafted file it reads; a store with a larger `segment_size` warns when it is given a bucket, which others
may be able to write. Files built to the body bound
measured 0.22 s (nested facts images over every window of a 64 MiB segment; 0.90 s before), 0.10 s (64 MiB of batches
whose rows share one text value, after damage; 1.85 s before) and 0.08 s (a 16 MiB manifest window of rejected records;
0.15 s before) at a start. Every entry type, small and large, costs at most about 2.9 ns a byte to check, a
definition of 65,536 state names of two 4-byte characters being the dearest (definitions of short names 5.2 to 5.7 ns
before; small definitions and routing entries 3 to 5.5 ns before, about 1 ns now), so nested entries bodies over the
bound cost at most about 0.8 s at 64 MiB (Q-870).

**Memory.** Sealing uses memory independent of `segment_size`: the sealer reads the segment 1 MiB at a time to
compute its checksum (`format.md` A.9).

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
