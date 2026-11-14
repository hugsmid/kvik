# kvik-model

A deliberately simple, slow, in-memory implementation of the semantics in [`docs/spec.md`](../docs/spec.md). No
files, no performance work. It shows that the rules are consistent and is the oracle the engine will be tested
against. Sums and integrals use exact rationals, so tree nodes, merges and windows are compared with direct
computation for exact equality.

| Area | Module |
|---|---|
| Series registry, kinds (interval series with length and phase), aliases, retirement, versioned series, timing, expected interval, record on change, counter rules, lateness bound | `registry` |
| Facts, quality, flags, `t_source`, positions, rank | `types` |
| Current, as-of and history views (direct scans) | `view` |
| Summary nodes, direct computation, `merge`, window edge stretches, density rule, credit limit, counter steps and rate, apportioned interval amounts | `summary` |
| Summary tree (1-minute and 15-minute levels, stored where the density rule keeps them), windows (interval windows included), sliding cache | `tree` |
| Calendar days and months: spans in the site zone (an effective-dated log of zone entries) or a named zone, zone identity after links, the tzdata version, calendar files of stored day and month nodes, calendar windows and results as window evaluations | `calendar` |
| Direct window and calendar oracles | `oracle` |
| Conditions with an injected monotonic clock; condition state, transitions and resume from a latch | `condition` |
| Ingest (lateness, future readings, invalid values, the clock before synchronisation, retirement, interval phase), duplicates, commit with crash points, durable mark, recovery by boot id, delivery (run-time routes, condition funnels and timer deliveries, cursors and latches, catch-up, resync), compaction and compaction of compacted files, atomic manifest records, reader leases, retirement, tiers, backup, tree checkpoints, generations, stepwise restore | `engine` |

The model does not yet follow every rule of the specification; [`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md) lists where
it differs, and the open questions in the specification itself.

## Tests

| File | Covers |
|---|---|
| `tests/invariants.rs` | Property tests for specification invariants 1–22, at least one test per invariant, named after it; invariants 2, 3 and 11 also over a delivery machine with run-time routes, condition funnels and timers; invariants 15 and 19 also across a restore; invariant 5 also for the density rule, interval series and calendar nodes, 7 also for rate and calendar results, 8 also for calendar files after site-zone changes, 17 and 22 also over the local days of a month, 18 also for the credit limit and interval readings, 21 through the engine and over the tree |
| `tests/edge_cases.rs` | Edge-case rows 1, 2, 2a–2d, 3–7, 7a, 8–17, 17a, 18, 18a, 19, 20, 20a, 21, 21a, 22, 22a, 23–27, 29–32, 34–37, 37b and 38–42, one test (or property) each, named after the row (row 20a for a series whose expected interval crosses the threshold is not covered: the model has no parameter changes, Q-60); rows 20–21a in zones with DST in both hemispheres, 30- and 45-minute offsets, a 30-minute DST shift and historical offsets off 15-minute boundaries |
| `tests/rules.rs` | Other rules, named after their section; the `s5_7_…` and `s6_calendar_…` tests cover calendar results, spans pinned by hand, zone identity and calendar windows; the `s7_…` tests cover per-series chunks, atomic manifest records, merging compacted files, the grace period and leases; the `s3_low_mark_…`, `s8_eviction_…`, `s8_retention_…` and `s8_fill_…` tests cover tiers and eviction; the `s2_2_…` and `s2_5_…` tests cover lateness, future readings and the clock, `s5_1_density_rule` and `s6_rate_…` the density rule and rate; the `s2_…`, `s4_outranked_…`, `s5_6_…` and `s8_version_index_…` tests cover the duplicate rule and versioned series (rank, refusals, outranked facts in delivery and the tree, the version index during a restore); the `s4_…` and `s6_…` delivery tests cover run-time routes, `resync` on resume, after a recovery on a new boot, after a restore and in place of a replay that is too far behind, condition deliveries and latches, with properties for delivery identity, resync order and no gap within a session; the `s8_…` tests cover tree checkpoints, generations and restore steps 1–4, including rows 28 and 28b, and row 28a at three points: before the record, during step 1, and between later downloads (a download is atomic in the model) |
| `tests/open_questions.rs` | One test per open question that the model can show |
| `tests/model.rs` | Model behaviour where it implements a simpler rule than the specification |

Not yet covered: edge-case rows 11a and 33 (see `OPEN-QUESTIONS.md`).

```bash
cargo test --release                  # faster
PROPTEST_CASES=10000 cargo test --release
cargo run --example basic
```
