# Contributing

kvik is at the design stage: the repository holds the specification and a reference model; the engine does not
exist yet. Changes go through pull requests against `main`.

## Before opening a pull request
```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```
CI checks formatting, lints and documentation on Linux, runs the tests on Linux, macOS and Windows, and runs the
property tests in release mode with many more cases on Linux: 10,000 on every change and 40,000 nightly, split into
the shards listed in `.github/deep-shards.sh`. Each deep job checks that every test is in exactly one shard; tests
of a new invariant go to the last invariants shard unless the table names them, so give a slow group a place there.

## Guidelines
- **Specification first.** A change to behaviour starts as a change to `docs/spec.md`. The reference model then
  follows it, with tests. If you find a gap or contradiction, add it to `model/OPEN-QUESTIONS.md` with a concrete
  scenario rather than choosing a reading silently.
- **Tests.** Every edge-case row and invariant in the specification should have a test in `model/tests/`, named
  after it. Prefer property tests with adversarial inputs (late facts, corrections, crashes at every step, time-zone
  changes).
- **Code.** Rust stable as pinned in `rust-toolchain.toml`, 2024 edition, `#![forbid(unsafe_code)]`. Keep commits
  small and focused; write commit messages in the imperative mood.
- **Writing.** British English; concise.
- **Data.** Fixtures must not contain real device identifiers, locations or personal data.

## Licence
kvik is licensed under the PolyForm Noncommercial License 1.0.0, and the project owner also grants commercial
licences. Contributions are accepted only if you agree that they may be distributed both under the project's licence
and under commercial licences granted by the project owner. You may be asked to sign a contributor licence agreement
before a contribution is merged.
