# kvik-tree

Summary-tree nodes of the kvik storage engine (`docs/spec.md` §5, roadmap 7.1a): the per-node fold over readings, the
associative merge, the window edge credit, and the canonical byte layout of a stored node, laid out in
[`docs/format-tree.md`](../docs/format-tree.md). Part of the kvik storage engine; not published on its own.

- Sums, integrals and increases are exact rationals (Q-180); a stored node holds them as dyadic numbers, so a node
  and every aggregate taken from it equal the reference model's whatever the bracketing.
- `encode` writes the one byte sequence of a node and refuses window evaluations; `decode` applies the checks of
  T.6, never panics and never allocates from an unchecked length.

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

Tests: merge laws and agreement with the model (`tests/properties.rs`), layout checks (`tests/layout.rs`), golden
nodes (`tests/golden/`), the fuzz entry over random and mutated inputs (`tests/fuzz_inputs.rs`), and a cargo-fuzz
target in [`fuzz/`](fuzz/README.md).
