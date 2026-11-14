# kvik-model

A deliberately simple, slow, in-memory implementation of the semantics in [`docs/spec.md`](../docs/spec.md). No
files, no performance work. It shows that the rules are consistent and is the oracle the engine will be tested
against. Sums and integrals use exact rationals, so tree nodes, merges and windows are compared with direct
computation for exact equality.

| Area | Module |
|---|---|
| Series registry, kinds, aliases, retirement | `registry` |
| Facts, quality, flags, positions | `types` |
| Current, as-of and history views (direct scans) | `view` |
| Summary nodes, direct computation, `merge`, window edge stretches | `summary` |
| Summary tree (stored 1-minute and 15-minute levels), calendar days, windows, sliding cache | `tree` |
| Direct window and calendar oracles | `oracle` |
| Conditions with an injected monotonic clock; condition state, transitions and resume from a latch | `condition` |
| Ingest, duplicates, commit with crash points, durable mark, recovery by boot id, delivery (run-time routes, condition funnels and timer deliveries, cursors and latches, catch-up, resync), compaction, manifest, tiers, backup, tree checkpoints, generations, stepwise restore | `engine` |

The model does not yet follow every rule of the specification; [`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md) lists where
it differs, and the open questions in the specification itself.

## Tests

| File | Covers |
|---|---|
| `tests/invariants.rs` | Property tests for specification invariants 1–11, 13, 15, 16, 17 and 18, at least one test per invariant, named after it; invariants 2, 3 and 11 also over a delivery machine with run-time routes, condition funnels and timers |
| `tests/edge_cases.rs` | Edge-case rows 1, 2, 2a, 2b, 3–7, 7a, 8, 9, 11–17, 17a, 18, 18a, 19, 20, 22–25, 27, 29–32, 34 and 35, one test (or property) each, named after the row |
| `tests/rules.rs` | Other rules, named after their section; the `s4_…` and `s6_…` delivery tests cover run-time routes, `resync` on resume, after a recovery on a new boot, after a restore and in place of a replay that is too far behind, condition deliveries and latches, with properties for delivery identity, resync order and no gap within a session; the `s8_…` tests cover tree checkpoints, generations and restore steps 1–4, including rows 28 and 28b, and row 28a at three points: before the record, during step 1, and between later downloads (a download is atomic in the model) |
| `tests/open_questions.rs` | One test per open question that the model can show |
| `tests/model.rs` | Model behaviour where it implements a simpler rule than the specification |

Not yet covered: edge-case rows 2c, 2d, 10, 11a, 21, 21a, 22a, 26, 33 and 36–42, and invariants 12, 14 and
19–22 (see `OPEN-QUESTIONS.md`).

```bash
cargo test --release                  # faster
PROPTEST_CASES=10000 cargo test --release
cargo run --example basic
```
