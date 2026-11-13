# Contributing

kvik is at the design stage: the repository holds the specification and a reference model; the engine does not
exist yet. Changes go through pull requests against `main`.

## Before opening a pull request
```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```
CI checks formatting, lints and documentation on Linux, runs the tests on Linux, macOS and Windows, and runs
longer property tests on Linux on every change and nightly.

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
By contributing you agree that your contributions are dual licensed under MIT OR Apache-2.0, as described in the
README.
