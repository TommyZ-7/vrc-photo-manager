# Fuzz targets

Standalone [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) workspace for
the parsers that consume untrusted input. It is deliberately **not** part of the
main Cargo workspace: libFuzzer requires nightly, while normal builds and CI use
stable Rust. The root `Cargo.toml` excludes `fuzz/` and this directory declares
its own workspace.

## Targets

| Target | Input | Coverage |
| --- | --- | --- |
| `ipc_jsonl` | arbitrary bytes | `vrc_photon_ipc::framing::read_message` (1 MiB cap, blank lines, JSON decoding into `RequestEnvelope`) |
| `ai_protocol_jsonl` | arbitrary bytes | `vrc_photon_ai_protocol::read_message` (1 MiB cap, UTF-8 and JSON validation into `SidecarMessage`) |
| `zip_eocd` | arbitrary bytes | `vrc_photon_ai_host::install::scan_zip_entry_count` (ZIP32/ZIP64 EOCD entry-count gate), `manifest::parse_manifest`, and `zip::ZipArchive::new` |

`scan_zip_entry_count` is compiled only with the `fuzz-support` feature, which
this crate enables on its `vrc-photon-ai-host` dependency.

## Running

```bash
rustup toolchain install nightly
cargo install cargo-fuzz --locked

cargo +nightly fuzz run ipc_jsonl
cargo +nightly fuzz run ai_protocol_jsonl
cargo +nightly fuzz run zip_eocd
```

The fuzzers are intended to run on Linux or macOS. Windows CI does not run
them; if you are on Windows, use WSL. Pass `-- -max_total_time=60` to bound a
run, for example:

```bash
cargo +nightly fuzz run ipc_jsonl -- -max_total_time=60
```

Corpus files are written to `fuzz/corpus/<target>/` and crashes to
`fuzz/artifacts/<target>/`. Both directories are ignored. Reproduce a crash
with:

```bash
cargo +nightly fuzz run zip_eocd fuzz/artifacts/zip_eocd/crash-<hash>
```

## Relation to CI

CI does not run these fuzzers: they require nightly plus extended run time.
The regular `cargo test` suite contains `proptest` property tests for both JSONL
framings (`crates/ipc/tests/framing_proptest.rs` and
`crates/ai-protocol/tests/framing_proptest.rs`) that act as deterministic
regression coverage. Add a reproducer as a unit test whenever a fuzzer finds a
defect.
