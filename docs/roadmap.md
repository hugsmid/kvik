# Roadmap

Each item is one pull request. An item is done when its tests pass on Linux, macOS and Windows and its box is
ticked in the same pull request. A **gate** is a point where work stops until the design has been looked at
again; nothing after a gate starts before it is passed. Items that do not depend on an open gate's subject may proceed in parallel; the gate still has to pass before anything that depends on it is ticked.

## 1. Specification settled
- [x] 1.1 Apply the outstanding rulings to `docs/spec.md`
- [x] 1.2 Model: durable mark, boot identity, recovery, generations and restore
- [x] 1.3 Model: delivery — latch, resync, catch-up after overflow, run-time routes, timer deliveries
- [x] 1.4 Model: files — atomic manifest records, reader leases, tiers and eviction, duplicate rule with revise, versioned series
- [x] 1.5 Model: data rules — lateness, non-finite values, retirement time, record-on-change keep-alive, density rule, counters, rate
- [x] 1.6 Model: calendar results as window evaluations; time-zone edge cases in several zones
- [x] 1.7 Every invariant has a property test and every edge-case row has a test; coverage table in `model/README.md`
- [x] 1.8 **Gate** — open questions settled and the specification tagged v0.2 (passed by the owner's decision with
  the questions listed under "Open at v0.2" in `model/OPEN-QUESTIONS.md` deferred to the roadmap items named there)

## 2. Measurements before building
- [x] 2.1 Spike: candidate chunk encodings against the Parquet reader — size and scan speed on synthetic per-second data; results in `docs/measurements/`
- [x] 2.2 Spike: commit and sync latency, group commit, behaviour on sync failure and a full disk; results in `docs/measurements/`
- [x] 2.3 **Gate** — measurements looked at (passed: the own chunk format is kept, with per-value decimal exceptions and a
  compressed XOR fallback, no larger than Parquet with ALP; delivery budgets set per platform, with Linux measured in
  4.8; syncs bounded by `min_sync_interval`; the mark synced on the writer thread)

## 3. Format and interface
- [x] 3.1 `docs/format.md` checked for implementability; problems listed
- [x] 3.2 `docs/api.md` checked for implementability; problems listed
- [x] 3.3 **Gate** — format and interface accepted (passed: format Part A and the interface are implementable; Part B
  outline fixed by 2.1; remaining items deferred to named 4.x–8.x items in `docs/format-check.md` and
  `docs/api-check.md`)

## 4. Log
- [x] 4.1 Crate skeleton: errors, configuration, injectable clock and file-system seams (started early, before gate 3.3,
  on a reference-model backend so that an application can integrate; aligned with the gate 3.3 interface)
- [x] 4.2 Segment container: header, blocks, checksums; reader up to the durable mark; golden files; fuzz target
- [x] 4.3 Arrival batches: encode, decode, round-trip property tests
- [x] 4.4 Commit path: group commit, sync, durable mark, positions (the Windows name-durability check of `format.md`
  A.13 moved to 4.8 and then to gate 4.9; the 5 % low disk mark waits for a file-system size in the seam, Q-115 (e))
- [x] 4.5 Recovery: boot identity rule, torn tail; kill-loop harness; injected sync errors and full disk (points left
  to later items in Q-116)
- [x] 4.6 Second-process reader and leases (in `kvik-log`; the interface `Reader`'s `facts` and series lookup moved to 5.1)
- [x] 4.7 Differential tests: engine log against the model on generated histories
- [x] 4.8 Benchmarks: commit latency and throughput, recorded in `docs/measurements/` (recorded on macOS and on the
  Linux NVMe target, `scripts/bench-nvme.sh`, which checks the delivery budget (Q-310); the cloud-runner
  figures of the `bench.yml` workflow are recorded, not durability-representative; Q-52 (b), Q-86 (b) and Q-115 (j)
  moved to gate 4.9)
- [ ] 4.9 **Gate** — log looked at as a whole. Reviewed; its fixes and the questions due here are applied
  (`model/OPEN-QUESTIONS.md`). The owner's run on the Linux NVMe target is recorded
  (`docs/measurements/4.8-commit-latency.md`). Remaining: (1) a run on the target confirming the fix for Q-310
  (p50 above 5 ms at exactly 100 × 1 Hz) and a cold start on the target for Q-116 (n) (warm: 0.39–0.74 s); (2) the owner's decisions
  on Q-111 (call limit in `api.md`), Q-115 (e) (`space()` seam), (j) (Windows wording) and
  (n) (a byte bound on the ingest queue, an ingest wait at 8 blocked seals), Q-116 (k) (full disk after a power
  loss), the Q-52 (b) macOS and Windows wording and the Q-86 (b) `chattr +C` sentence

## 5. Delivery
- [ ] 5.1 Series registry; the `kvik` crate on the durable log: writing, and `facts`, series lookup and `refresh` for
  reader processes
- [x] 5.1a Model: recovery entries, reset points, rows 7b and 7c, invariants 25–27 (the oracle for 5.2 and 5.3;
  moved from before 5.1)
- [x] 5.2 Routing table, conditions, consumer queues, durable cursors
- [ ] 5.3 Catch-up, resync and replay flags; the saved-point check (`Reader::check`, `Start::Position`, row 7b)
- [ ] 5.4 Live state and windowed operators over recent facts
- [ ] 5.5 Differential tests against the model; routing and delivery benchmarks
- [ ] 5.5a Durability levels chosen per source (Q-170): spec text and model; for the faster level, §3 Recovery
  rule 1, principle 3 and invariants 2 and 11 rewritten to keep complete flagged batches beyond the mark (Q-125)
- [ ] 5.5b Durability levels: log writer acknowledges at write; commit-block flag; recovery keeps flagged batches
- [ ] 5.5c Durability levels: definition parameter, `Outcome`, durable-up-to watermark
- [ ] 5.6 **Gate** — delivery looked at as a whole

## 6. Compacted segments
- [x] 6.1 Value and timestamp encodings with property and fuzz tests (block bodies and column encodings in
  `docs/format-chunk.md`; sizes in `docs/measurements/6.1-encodings.md`; the compressed fallback's canonical class settled in Q-150)
- [ ] 6.2 Per-series chunks, index, lossless compaction with verification
- [x] 6.3a Manifest records, the file catalogue and the automatic rebuild (`files/`; record layouts in `docs/format.md`
  A.14; Q-171 and Q-174 settled at its review)
- [ ] 6.3b Atomic publish-and-retire: the store publishes compaction outputs and retires inputs through the manifest,
  and a start calls the rebuild and the copies of a corrupt record (moved from 6.3)
- [ ] 6.4a Parquet export: the export schema (provisional, `docs/export.md`) and the Parquet writer (unticked pending Q-190 to Q-197)
- [ ] 6.4b Differential check of compacted data against the Parquet export (moved from 6.4)
- [ ] 6.5 **Gate** — compaction looked at as a whole (includes: measure a pure-Rust codec, `ruzstd` or `lz4_flex`, on the XOR
  stream; within about 10 % of zstd level 3 on the °F→°C shape it replaces the C library, otherwise zstd stays and CI
  fails when `ZSTD_SYS_USE_PKG_CONFIG` is set; Q-150)

## 7. Summary tree
- [x] 7.1a Summary nodes: the per-node fold, the associative merge and the canonical byte layout (`kvik-tree`,
  `docs/format-tree.md`; exact node fields, signed zeros and distinct state names settled in Q-180 to Q-182)
- [ ] 7.1b Stored levels, tree files, checkpoints, rebuild-equals-live (moved from 7.1)
- [ ] 7.2 Late and corrected data; revisions
- [ ] 7.3 Calendar levels; windowed operators over the tree
- [ ] 7.4 **Gate** — tree looked at as a whole

## 8. Tiers and backup
- [ ] 8.1 Tier policy and verified eviction
- [x] 8.2 Backup sink interface with an S3-compatible implementation, tested against a local fake
- [ ] 8.3 Restore, partial restore and rebuild
- [ ] 8.4 **Gate** — restore passes end to end
- [ ] 8.5 Self-repair: a loss is restored from a verified copy, then offered to the application for re-sending, then
  closed by a record, with health back to OK; manual acknowledgement as a setting; scheduled hash check of local files
  (8.5a spec, format and model; 8.5b scrub, quarantine and fetch-verify-swap; 8.5c the store wired: loss states,
  re-send requests, closing records, alerts; 8.5d the fault matrix against the model)

## 9. Query
- [ ] 9.1 Table provider and tier selection by zoom level
- [ ] 9.2 Query benchmarks against the budgets in `docs/design.md`

## 10. Scale
- [ ] 10.1 Scale test with billions of rows: size, drill-down and delivery latency
- [ ] 10.2 Comparative benchmarks against other tools on the target workloads (plan: [`docs/benchmarks.md`](benchmarks.md)); results in `docs/measurements/`
- [ ] 10.3 **Gate** — confirm the own format against Parquet on size, per-file overhead, recovery from a damaged file, binary size and decode speed at scale; Parquet stays for export
