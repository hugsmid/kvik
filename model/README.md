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
| Conditions with an injected monotonic clock | `condition` |
| Ingest, duplicates, commit with crash points, durable mark, recovery by boot id, delivery, cursors, catch-up, compaction, manifest, tiers, backup, restore | `engine` |

The model does not yet follow every rule of the specification; [`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md) lists where
it differs, and the open questions in the specification itself.

## Tests

| File | Covers |
|---|---|
| `tests/invariants.rs` | Property tests for specification invariants 1–11, 17 and 18, one test per invariant |
| `tests/edge_cases.rs` | Edge-case rows 1, 2, 2a, 2b, 3–9, 11–14, 17, 17a, 18, 18a, 19, 20, 22–25, 27, 29–32 and 35, one test (or property) each, named after the row |
| `tests/rules.rs` | Other rules, named after their section |
| `tests/open_questions.rs` | One test per open question that the model can show |
| `tests/model.rs` | Model behaviour where it implements a simpler rule than the specification |

Not yet covered: edge-case rows 10, 15, 16, 21, 22a, 26, 28, 33 and 34, and invariants 12–16 (see
`OPEN-QUESTIONS.md`).

```bash
cargo test --release                  # faster
PROPTEST_CASES=10000 cargo test --release
cargo run --example basic
```
