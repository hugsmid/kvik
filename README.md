# kvik

kvik is a planned embedded time-series store for device data, in Rust: a durable ordered log and an analytical
store in one library, with no database service.

**Status: design stage. The engine does not exist yet.** This repository currently contains:

| Path | Contents |
|---|---|
| [`docs/spec.md`](docs/spec.md) | The specification: the rules an implementation must follow |
| [`docs/design.md`](docs/design.md) | Design direction, performance budgets, build order |
| [`model/`](model/) | `kvik-model`: a slow, exact, in-memory reference model of the specified semantics, with property tests |
| [`model/OPEN-QUESTIONS.md`](model/OPEN-QUESTIONS.md) | Open questions in the specification, and where the model still differs from it |

The design goals are that a reading is durable before anything reacts to it, that late and corrected data are
recorded rather than dropped, and that every derived structure can be rebuilt from the recorded facts. None of
this is implemented outside the reference model yet. The engine will be built step by step against the model.

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## Licence
Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT) at your
option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
work, as defined in the Apache-2.0 licence, shall be dual licensed as above, without any additional terms or
conditions.
