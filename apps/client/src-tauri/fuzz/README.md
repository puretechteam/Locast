# locast-client fuzz targets

## wire_decode

`fuzz_targets/wire_decode.rs` feeds arbitrary bytes to
`net::wire::decode_and_validate`, the production decoder that
`src/net/signaling.rs` uses for every inbound binary WebSocket
frame. The only property checked is that no input panics or hangs;
`Err` (a `bad_msg` reject) and `Ok` are both expected outcomes.
Accepted envelopes are also re-encoded with `rmp_serde::to_vec_named`.

## path_validator

`fuzz_targets/path_validator.rs` feeds arbitrary bytes (lossily
decoded to a string) to `core::paths::validate_library_path`, the
single path validator `src/library/protocol.rs` uses before serving a
file. It runs against a fixed library root created under the system
temp dir (`locast-fuzz-path-validator/`, holding
`library/ab/cd/Movie.mkv`). Any input may be rejected; an accepted
path must be a regular file under the canonical root and must not
contain `..`, `\`, NUL, or a leading `/`. The crafted-path battery
(traversal, absolute, drive, UNC, device, NUL, symlink, junction) is
in the unit tests of `src/core/paths.rs`.

## Corpus

- `corpus/wire_decode/*.msgpack` are committed seeds. Their bytes are
  pinned by unit tests in `src/net/wire.rs`, so do not edit them by
  hand.
- `corpus/path_validator/*.txt` are committed seeds: one accepted
  path and a few rejects. They have no trailing newline (a newline is
  a control character and would be rejected before the interesting
  checks).
- libFuzzer writes new hash-named entries (no extension) into the
  same directory during a run. Those are gitignored, as are
  `artifacts/`, `coverage/`, and `target/`.

## Requirements

- A nightly toolchain. The repo's `rust-toolchain.toml` pins stable,
  so pass `+nightly` explicitly.
- cargo-fuzz: `cargo install cargo-fuzz`.

## Running

All commands run from `apps/client/src-tauri`.

Linux and macOS:

```sh
cargo +nightly fuzz run wire_decode -- -max_total_time=60
cargo +nightly fuzz run path_validator -- -max_total_time=60
```

Windows (MSVC):

```powershell
$msvc = "<VS install>\VC\Tools\MSVC\<ver>\bin\HostX64\x64"
$env:PATH = "$msvc;$env:PATH"
cargo +nightly fuzz check --no-include-main-msvc wire_decode
cargo +nightly fuzz run --no-include-main-msvc wire_decode -- -max_total_time=60
cargo +nightly fuzz check --no-include-main-msvc path_validator
cargo +nightly fuzz run --no-include-main-msvc path_validator -- -max_total_time=60
```

Notes for Windows:

- The MSVC `bin\HostX64\x64` directory contains
  `clang_rt.asan_dynamic-x86_64.dll`. If it is not on `PATH`, the
  fuzz executable fails to start with exit code `0xc0000135`.
- `--no-include-main-msvc` is required. By default cargo-fuzz adds
  `/include:main` to RUSTFLAGS, which also reaches the `cdylib` crate
  type of `locast-client`, and a DLL cannot resolve `main` (LNK2001).
  `fuzz/build.rs` re-adds `/include:main` for this crate's binaries
  only, so the fuzz binary still gets libFuzzer's `main` and the
  client `cdylib` links.

## Build profile

Use the default cargo-fuzz profile. Do not pass `-O` on its own: it
drops debug assertions in the target, which brings back the
tauri-utils E0063 mismatch described in `Cargo.toml`. If you want
an optimized build, pass `-O` together with `-a`
(`--debug-assertions`).
