//! Executable reference model of the kvik time-series store.
//!
//! A deliberately simple, slow, in-memory implementation of the store's
//! semantics: series and facts, the three views, the duplicate rule, commit and
//! delivery ordering with crash points, the summary tree with merge and carry,
//! windowed operators and conditions, compaction, tiers and restore. It has no
//! files and does no performance work. It exists to show that the rules are
//! consistent and to serve as the oracle a real engine is tested against.
//!
//! Every structure has a naive direct computation next to it:
//! [`view`] computes views by scanning facts, [`summary::direct`] computes a
//! node from its readings, and [`oracle`] computes windows and calendar days
//! straight from the current view.
//!
//! ```
//! use kvik_model::engine::{Config, Model};
//! use kvik_model::types::{Kind, Reading, ValueType, SECOND};
//!
//! let mut m = Model::new(Config::default(), vec![], 1_000 * SECOND);
//! let s = m.define("plant/line-1/temperature", Kind::Gauge, ValueType::Number, "degC", 30 * SECOND).unwrap();
//! m.ingest(vec![Reading::num(s, 990 * SECOND, 100.0), Reading::num(s, 995 * SECOND, 200.0)]).unwrap();
//! let w = m.window(s, 990 * SECOND, 1_000 * SECOND).unwrap();
//! assert_eq!(w.count, 2);
//! assert_eq!(w.coverage, i128::from(10 * SECOND));
//! ```

#![forbid(unsafe_code)]

pub mod condition;
pub mod engine;
pub mod oracle;
pub mod registry;
pub mod summary;
pub mod tree;
pub mod types;
pub mod view;
