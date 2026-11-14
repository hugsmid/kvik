# Roadmap

Each item is one pull request. An item is done when its tests pass on Linux, macOS and Windows and its box is
ticked in the same pull request. A **gate** is a point where work stops until the design has been looked at
again; nothing after a gate starts before it is passed.

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
- [x] 4.1 Crate skeleton: errors, configuration, injectable clock and file-system seams (public API first implemented
  on the reference model; durable engine from 4.2)
- [ ] 4.2 Segment container: header, blocks, checksums; reader up to the durable mark; golden files; fuzz target
- [ ] 4.3 Arrival batches: encode, decode, round-trip property tests
- [ ] 4.4 Commit path: group commit, sync, durable mark, positions
- [ ] 4.5 Recovery: boot identity rule, torn tail; kill-loop harness; injected sync errors and full disk
- [ ] 4.6 Second-process reader and leases
- [ ] 4.7 Differential tests: engine log against the model on generated histories
- [ ] 4.8 Benchmarks: commit latency and throughput, recorded in `docs/measurements/`
- [ ] 4.9 **Gate** — log looked at as a whole

## 5. Delivery
- [ ] 5.1 Series registry
- [ ] 5.2 Routing table, conditions, consumer queues, durable cursors
- [ ] 5.3 Catch-up, resync and replay flags
- [ ] 5.4 Live state and windowed operators over recent facts
- [ ] 5.5 Differential tests against the model; routing and delivery benchmarks
- [ ] 5.6 **Gate** — delivery looked at as a whole

## 6. Compacted segments
- [ ] 6.1 Value and timestamp encodings with property and fuzz tests
- [ ] 6.2 Per-series chunks, index, lossless compaction with verification
- [ ] 6.3 Manifest and atomic publish-and-retire
- [ ] 6.4 Parquet export and a differential check against it
- [ ] 6.5 **Gate** — compaction looked at as a whole

## 7. Summary tree
- [ ] 7.1 Stored levels, checkpoints, rebuild-equals-live
- [ ] 7.2 Late and corrected data; revisions
- [ ] 7.3 Calendar levels; windowed operators over the tree
- [ ] 7.4 **Gate** — tree looked at as a whole

## 8. Tiers and backup
- [ ] 8.1 Tier policy and verified eviction
- [ ] 8.2 Backup sink interface with an S3-compatible implementation, tested against a local fake
- [ ] 8.3 Restore, partial restore and rebuild
- [ ] 8.4 **Gate** — restore passes end to end

## 9. Query
- [ ] 9.1 Table provider and tier selection by zoom level
- [ ] 9.2 Query benchmarks against the budgets in `docs/design.md`

## 10. Scale
- [ ] 10.1 Scale test with billions of rows: size, drill-down and delivery latency
- [ ] 10.2 Comparative benchmarks against other tools on the target workloads (plan: [`docs/benchmarks.md`](benchmarks.md)); results in `docs/measurements/`
- [ ] 10.3 **Gate** — confirm the own format against Parquet on size, per-file overhead, recovery from a damaged file, binary size and decode speed at scale; Parquet stays for export
