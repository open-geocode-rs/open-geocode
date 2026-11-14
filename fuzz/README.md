# Fuzz targets

`fuzz_parse` feeds the query parser any text the HTTP API accepts, up to 256 characters.
It fails on a panic, or on a parse slower than 150 ms under coverage instrumentation, about 50 ms in a release build.
It builds a small fixture Pack once, since the parser's dictionaries and weights come from the Pack.

Run it locally with a nightly toolchain and `cargo-fuzz`, from this directory:

```sh
cp ../Cargo.lock Cargo.lock
python3 -I seed.py
cargo +nightly fuzz run --sanitizer none fuzz_parse -- -max_len=1024 -timeout=10
```

`seed.py` writes the seed corpus from the string literals in `src/parse/lattice/tests.rs` and the gold rows in `fixtures/parse/gold`.
`.github/workflows/fuzz.yml` runs the target for 20 minutes every night, and for 5 minutes on a pull request that touches the parser or this directory.

A failing input is saved under `artifacts/fuzz_parse/`, and the workflow uploads it.
Fix it with a unit test in `src/parse/lattice/tests.rs` that parses that input, so it stays fixed.
