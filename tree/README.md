# kvik-tree

Summary-tree nodes of the kvik storage engine (`docs/spec.md` §5, roadmap 7.1a): the per-node fold over readings, the
associative merge, the window edge credit, and the canonical byte layout of a stored node, laid out in
[`docs/format-tree.md`](../docs/format-tree.md). Part of the kvik storage engine; not published on its own.

- Sums, integrals and increases are exact (Q-180): a dyadic mantissa and binary exponent, which folding and merging
  never leave, so no gcd runs; a rational only after a window's pro rata division. A node and every aggregate taken
  from it equal the reference model's whatever the bracketing.
- `encode` writes the one byte sequence of a node and refuses window evaluations; `decode` applies the checks of
  T.6, never panics and never allocates from an unchecked length.
- `part::Part` and `root::Root` encode and decode tree parts and the tree root (roadmap 7.1b, T.8 to T.12) over byte
  buffers; a decode accepts only canonical bytes and names each refusal (`Refusal`). The stored levels are one table,
  `LEVELS`; files carry the table they were written under.
- `set` forms the visible state of a root over its parts (floors, `covered`), the `cover` of a span, the part merge
  with its roll-ups, the root that publishes it, and the merge planner: pure functions, no file access.

```rust
use kvik_tree::{Kind, Params, Value, decode, encode, fold, merge};

const S: i64 = 1_000_000_000;
let p = Params::new(Kind::Counter, Some(90 * S));
let r: Vec<(i64, Value)> = [(0, 10.0), (30, 12.5), (60, 0.5), (90, 2.0)]
    .into_iter()
    .map(|(t, v)| (t * S, Value::F64(v)))
    .collect();
let whole = fold(&p, 0, 120 * S, &r);
let parts = merge(&p, &fold(&p, 0, 45 * S, &r[..2]), &fold(&p, 45 * S, 120 * S, &r[2..]));
assert_eq!(encode(&parts).unwrap(), encode(&whole).unwrap());
assert_eq!(decode(&encode(&whole).unwrap()).unwrap(), whole);
```

Tests: merge laws and agreement with the model (`tests/properties.rs`), layout checks (`tests/layout.rs`, one vector
per T.6 check in `tests/checks.rs`), node rules (`tests/nodes.rs`), golden
nodes (`tests/golden/`), the fuzz entry over random and mutated inputs (`tests/fuzz_inputs.rs`), and a cargo-fuzz
target in [`fuzz/`](fuzz/README.md); for parts and roots, round trips (`tests/parts.rs`), one vector per refusal
(`tests/refusals.rs`) and golden files (`tests/golden_files.rs`); for the visible state, merges and covers, a map
oracle and direct folds (`tests/set.rs`) and the planner rule by rule (`tests/plan.rs`). Fold time, node and entry sizes and the cost of a
decode and merge: `cargo run --release -p kvik-tree --example fold_cost` (`docs/measurements/tree-parts.md`).
