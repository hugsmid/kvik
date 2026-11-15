//! Summary-tree nodes of the kvik store (`docs/spec.md` §5, roadmap 7.1): the per-node fold over readings, the
//! associative merge, the window edge credit, and the canonical byte layout of a stored node
//! (`docs/format-tree.md`).
//!
//! Sums, integrals and increases are exact (Q-180), so a node, and every aggregate taken from it, equals the
//! reference model's for the same readings whatever the bracketing of the merges. A stored node holds only dyadic
//! numbers (sums of `f64` values and their products with whole nanoseconds), which [`encode`] writes exactly.
//!
//! ```
//! use kvik_tree::{Kind, Params, Value, decode, encode, fold, merge};
//!
//! const S: i64 = 1_000_000_000;
//! let p = Params::new(Kind::Gauge, Some(90 * S));
//! let r: Vec<(i64, Value)> = [(0, 20.5), (30, 21.0), (60, 21.25), (90, 21.5)]
//!     .into_iter()
//!     .map(|(t, v)| (t * S, Value::F64(v)))
//!     .collect();
//! let whole = fold(&p, 0, 120 * S, &r);
//! let parts = merge(&p, &fold(&p, 0, 45 * S, &r[..2]), &fold(&p, 45 * S, 120 * S, &r[2..]));
//! let bytes = encode(&whole).unwrap();
//! assert_eq!(encode(&parts).unwrap(), bytes);
//! assert_eq!(decode(&bytes).unwrap(), whole);
//! ```

#![forbid(unsafe_code)]

mod bytes;
mod exact;
mod fold;
pub mod fuzz;
mod node;
mod window;

pub use bytes::{DecodeError, NotStorable, decode, encode, form};
pub use exact::{Exact, exact, exact_ns};
pub use fold::{fold, merge, merge_all};
pub use node::{Body, CounterRules, Kind, Node, Params, StateKey, Value};
pub use window::{credit_intervals, with_stretches};
