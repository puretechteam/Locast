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

## transfer_frame

`fuzz_targets/transfer_frame.rs` feeds arbitrary bytes to
`transfer::wire::codec::decode` and `decode_stream`, the
length-prefixed JSON codec every inbound WebRTC DataChannel message
from a remote peer goes through. Each run tries the raw bytes (to fuzz
the length-prefix checks) and a re-framed copy (first four bytes
dropped, correct prefix added) so that mutated JSON bodies reach
`serde_json` and the frame validator instead of dying on a stale
prefix. On `Ok` the target asserts: `decode` consumed `4 + prefix`
bytes (at least 4, at most the input length); `decode_stream`
consumed the whole buffer and its first frame equals `decode`'s; and
every accepted frame re-encodes with `codec::encode` and decodes back
to an equal frame. `validate` is `pub(crate)`, so the round trip is
how the target re-checks it.

## manifest_verify

`fuzz_targets/manifest_verify.rs` feeds arbitrary bytes to
`serde_json::from_slice::<MediaManifest>` (`locast-manifest`), the
same JSON form the server and viewers deserialize from the message
envelope, and on `Ok` runs `verify_manifest` and the canonical
`serialize`. It asserts: nothing panics; `verify_manifest` agrees with
itself across two calls (an arbitrary input may legitimately verify if
a seed is signed, so `Ok` is not forbidden); `serialize` is
deterministic and its output is a fixed point (parsing the canonical
bytes and serializing again gives the same bytes); and signing the
parsed manifest with the public RFC 8032 test seed always verifies
and leaves the canonical bytes unchanged. The key and signature
decoders are private to `signing.rs` and are reached through
`verify_manifest`.

## protocol_url

`fuzz_targets/protocol_url.rs` feeds `library::protocol::LocastUrl::parse`
and `parse_single_range`, the parsers behind every `locast://` webview
request. Input layout: 8 bytes little-endian u64 right-shifted by
the low 6 bits of the last of them (the `total_size`, so 0, 1, small
and near-`u64::MAX` sizes are all common), then the rest decoded with
`String::from_utf8_lossy`, used as both the URL and the `Range`
header. Lossy decoding is deliberate: a single broken UTF-8 byte keeps
a mostly-intact string instead of discarding the input. Asserted:
`Ok((start, end))` satisfies `start <= end < total_size`; an accepted
URL has only non-empty segments with no `..`, `/`, `\` or NUL; and
`encode_segment` followed by `parse` returns the same URL.

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
- `corpus/transfer_frame/*.bin` are frames in the wire format
  (4-byte big-endian length, then the JSON body): hello, offer,
  request, and a two-frame stream. The JSON mirrors what
  `codec::encode` emits (struct field order, no whitespace).
- `corpus/manifest_verify/*.json` are manifests derived from the
  `shared/manifest/tests/signing_golden.json` fixture: unsigned,
  validly signed (golden RFC 8032 vector), a flipped-bit signature,
  and one with decomposed (NFD) unicode in a filename.
- `corpus/protocol_url/*.bin` are an 8-byte little-endian size header
  (4096) followed by one URL or `Range` string: a valid media URL,
  open-ended and suffix ranges, and a percent-encoded `..` URL.
- libFuzzer writes new hash-named entries (no extension) into the
  same directory during a run. Those are gitignored, as are
  `artifacts/`, `coverage/`, and `target/`.

## CI smoke

The fuzz crate has its own `[workspace]`, is not a member of the root
workspace, and is therefore excluded from `cargo test --workspace`,
`cargo check --workspace` and `cargo clippy --workspace`. Nothing in
the normal test run builds or exercises it. To check that every
target still compiles and survives a fixed number of executions, run
from `apps/client/src-tauri`:

```sh
cargo +nightly fuzz build
cargo +nightly fuzz run wire_decode -- -runs=2000 -seed=1
cargo +nightly fuzz run path_validator -- -runs=2000 -seed=1
cargo +nightly fuzz run subtitle_srt -- -runs=2000 -seed=1 -max_len=65536
cargo +nightly fuzz run subtitle_vtt -- -runs=2000 -seed=1 -max_len=65536
cargo +nightly fuzz run subtitle_ass -- -runs=2000 -seed=1 -max_len=65536
cargo +nightly fuzz run transfer_frame -- -runs=2000 -seed=1 -max_len=65536
cargo +nightly fuzz run manifest_verify -- -runs=2000 -seed=1 -max_len=65536
cargo +nightly fuzz run protocol_url -- -runs=2000 -seed=1 -max_len=1024
```

On Windows add `--no-include-main-msvc` after `run` / `build` and put
the MSVC `bin\HostX64\x64` directory on `PATH` (see Running).

`-runs=2000` bounds the work by execution count instead of wall-clock
time, and `-seed=1` fixes libFuzzer's random number generator. The
run is repeatable in practice on one machine and toolchain, but not
guaranteed bit-for-bit: the starting corpus (hash-named files left in
`corpus/<target>/` by earlier local runs change what gets mutated),
the compiler and libFuzzer version, and libFuzzer's own timing-based
heuristics can all shift which inputs are tried. On a clean CI
checkout only the committed seeds exist. The smoke run is a cheap
regression gate, not a substitute for a real campaign. A crash
exits non-zero and writes the reproducer under `artifacts/<target>/`.

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
cargo +nightly fuzz run transfer_frame -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run manifest_verify -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run protocol_url -- -max_total_time=60 -max_len=1024
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

The same `check` / `run --no-include-main-msvc` pair applies to
`transfer_frame`, `manifest_verify` and `protocol_url`, with the
`-max_len` values from the Linux list above.

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
