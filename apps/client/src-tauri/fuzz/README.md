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

## subtitle_srt, subtitle_vtt, subtitle_ass

`fuzz_targets/subtitle_srt.rs`, `subtitle_vtt.rs` and
`subtitle_ass.rs` feed arbitrary raw bytes (no lossy conversion, so
BOMs, UTF-16 and invalid UTF-8 reach the decoder) to
`media::subtitles::parse_srt`, `parse_vtt` and `parse_ass`, the
hand-written parsers for untrusted subtitle files. Any input may be
rejected. On `Err` the target formats the error and calls `code()`,
which must not panic. On `Ok` the target asserts the cue invariants:

- at most `MAX_CUES` cues;
- cues sorted by `start_ms`;
- `start_ms <= end_ms <= MAX_TIMESTAMP_MS`;
- cue text is non-empty and at most `MAX_CUE_TEXT_BYTES` bytes.

Each corpus directory holds a valid minimal file (`valid_basic.*`),
`mal_bom_negative_timestamp.*` (UTF-8 BOM plus a negative
timestamp in the format's own syntax), `mal_4gb_size.*` (a
4294967296 value in numeric fields and a `4GB` string), and one
format-specific edge seed (`multi_line_tags.srt`,
`note_style_blocks.vtt`, `dialogue_commas_overrides.ass`).

## Corpus

- `corpus/wire_decode/*.msgpack` are committed seeds. Their bytes are
  pinned by unit tests in `src/net/wire.rs`, so do not edit them by
  hand.
- `corpus/path_validator/*.txt` are committed seeds: one accepted
  path and a few rejects. They have no trailing newline (a newline is
  a control character and would be rejected before the interesting
  checks).
- `corpus/subtitle_srt/*.srt`, `corpus/subtitle_vtt/*.vtt` and
  `corpus/subtitle_ass/*.ass` are committed seeds (valid, malicious
  and edge files, each under 1 KB). They use LF line endings only,
  because the repo `.gitattributes` normalizes text files to LF and
  would silently alter CRLF bytes; CRLF, CR-only and UTF-16 inputs
  are covered by inline unit tests in `src/media/subtitles/`. The
  per-seed expected outcome is pinned by
  `src/media/subtitles/corpus_pin.rs`.
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
cargo +nightly fuzz run subtitle_srt -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run subtitle_vtt -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run subtitle_ass -- -max_total_time=60 -max_len=65536
```

Windows (MSVC):

```powershell
$msvc = "<VS install>\VC\Tools\MSVC\<ver>\bin\HostX64\x64"
$env:PATH = "$msvc;$env:PATH"
cargo +nightly fuzz check --no-include-main-msvc wire_decode
cargo +nightly fuzz run --no-include-main-msvc wire_decode -- -max_total_time=60
cargo +nightly fuzz check --no-include-main-msvc path_validator
cargo +nightly fuzz run --no-include-main-msvc path_validator -- -max_total_time=60
cargo +nightly fuzz check --no-include-main-msvc subtitle_srt
cargo +nightly fuzz run --no-include-main-msvc subtitle_srt -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz check --no-include-main-msvc subtitle_vtt
cargo +nightly fuzz run --no-include-main-msvc subtitle_vtt -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz check --no-include-main-msvc subtitle_ass
cargo +nightly fuzz run --no-include-main-msvc subtitle_ass -- -max_total_time=60 -max_len=65536
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
