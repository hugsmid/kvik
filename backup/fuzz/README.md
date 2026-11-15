# kvik-backup fuzz target

A [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) workspace of its own, so stable CI never builds it. It
needs a nightly toolchain:

```sh
cargo install cargo-fuzz
cd backup/fuzz
mkdir -p corpus/envelope && cp ../tests/golden/*.kve corpus/envelope/
cargo +nightly fuzz run envelope -- -malloc_limit_mb=64
```

| Target | Feeds arbitrary bytes to |
|---|---|
| `envelope` | `kvik_backup::envelope::fuzz::open_all`: `ChunkedXChaCha::open`, `check_header` and `open_span` over several ranges, on the input as it is and with its header repaired for the fuzz key (so the size checks and the chunks are reached); an object that opens must be what sealing its plaintext gives, and each range must equal the slice; then the input sealed as a plaintext must open to itself |

`-malloc_limit_mb` makes an allocation sized by a declared length, not by the input, fail the run. The same entry
function runs on stable in `tests/fuzz_inputs.rs` over random bytes and mutated golden and generated objects. The
libFuzzer macro needs `unsafe` attributes, so this crate does not forbid unsafe code; the library it calls does.
Its `Cargo.lock` is committed, so `libfuzzer-sys` is pinned.
