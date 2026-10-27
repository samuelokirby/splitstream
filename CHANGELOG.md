# Changelog

All notable changes to `splitstream` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> ### ⚠️ Licensing history — read this first
>
> `splitstream` has changed license twice:
>
> | Versions | License |
> |---|---|
> | 0.1.0 – 0.1.1 | MIT |
> | 0.1.2 | AGPL-3.0-only |
> | 0.1.3 – 0.1.9 | AGPL-3.0-only **or** a paid commercial license (dual) |
> | **0.2.0 and later** | **MIT** |
>
> **0.2.0 returns the project to MIT, permanently.** The AGPL experiment and the
> commercial-license offering are both retired: `LICENSE-COMMERCIAL` is deleted, no
> CLA is required, and there is nothing to buy. If you evaluated `splitstream`
> between 0.1.2 and 0.1.9 and rejected it on license grounds, that objection is gone.
>
> Versions 0.1.2 through 0.1.9 remain published under the terms they shipped with;
> the relicense is not retroactive. Depend on 0.2.0 or later for MIT terms.

## [0.2.0] — Unreleased

The release that makes `splitstream` adoptable: **MIT again**, and honest about
what macOS will and will not let it do.

### Relicensed to MIT

- **AGPL-3.0-only (dual commercial) → MIT.** One `LICENSE` file, `license = "MIT"`
  in `Cargo.toml`, `LICENSE-COMMERCIAL` deleted, no CLA for contributors, nothing
  to buy. See the licensing-history note at the top of this file — 0.2.0 returns
  the project to the terms it launched with, permanently.
- The README's licensing section is now a three-line FAQ instead of a dual-license
  explanation, because under MIT the questions answer themselves.

### macOS permissions and signing are documented

Nothing in the repository previously mentioned TCC, `Info.plist` usage-description
keys, entitlements, or code signing — while both the microphone and the Core Audio
process tap require user consent, and a bare `cargo run` binary is not a signed
`.app`. Evaluators hit an empty transcript stream with no explanation.

- New **Permissions & Signing** section in the README: which prompts fire and when,
  the required `Info.plist` keys, the entitlements the audio tap needs, `cargo run`
  versus a bundled `.app`, and `tccutil reset` for a clean development state.
- A table of **what each failure looks like at the API level**, naming the exact
  error variant for every case — including the one failure the crate cannot detect
  (a missing `NSAudioCaptureUsageDescription`, where the tap silently produces
  silence).
- New `examples/bundled_app/`: a working `Info.plist`, entitlements file, and build
  script that produce a signed `.app` — the known-good artifact to copy.
- Microphone denial is now **detected**, not guessed. cpal cannot report it (with
  the microphone denied, building and starting the stream still succeeds and the
  callback simply receives silence forever), so the TCC authorization status is read
  directly and surfaces as `PermissionDenied { what: Microphone }`.

### Changed (breaking)

- **`Transcript` carries timing and confidence.** New `start: Duration`,
  `end: Duration` (offsets from the start of capture, shared across both sources)
  and `confidence: Option<f32>`, plus a `duration()` helper. Without these, the
  stated use case — aligning, seeking, and diarizing a meeting — was impossible.
- **The transcript channel is bounded with a documented drop-oldest policy**
  (`DEFAULT_CHANNEL_CAPACITY`, builder-configurable via `channel_capacity`). The
  previous unbounded channel grew without limit behind a slow consumer.
- **`start()` now returns a `TranscriptStream`**, which implements
  `futures_core::Stream<Item = Transcript>` as well as `recv()`, reports how many
  events the overflow policy discarded via `dropped()`, and still exposes the raw
  receiver underneath.
- **`SplitStreamHandle` is `Clone` and `shutdown` takes `&self`.** It previously
  consumed the handle and held a non-`Clone` `oneshot::Sender`, so the common
  "ctrl-c task plus a mute toggle" shape forced you to give up mute control.
- **Model paths are `PathBuf`.** `WhisperConfig::model_path` and
  `ParakeetConfig::model_dir` were `String` with `impl Into<String>` setters, which
  forced callers holding a `PathBuf` through `to_string_lossy()`. Setters now take
  `impl AsRef<Path>`.
- **`Transcript`, `AudioSource`, `SplitStreamError`, `PermissionKind` and every
  config struct are `#[non_exhaustive]`**, so later releases can add fields and
  variants without another breaking change. Matches on them need a `_` arm.
- **`pub mod audio` and `pub mod backends` are now private.** Their contents were
  all `pub(crate)`; they rendered on docs.rs as two empty modules.
- **The whisper backend is `#[doc(hidden)]`.** The code stays and the feature still
  builds, but it is a work in progress and is no longer advertised in the docs, the
  README table, or `settings.toml`.
- **Binary-only dependencies moved behind a new `cli` feature.** `colored`, `config`,
  `dotenvy`, `reqwest`, and `sha2` are optional and enabled only by `cli`; both
  binaries declare `required-features = ["cli"]`. Install the demo with
  `cargo install splitstream --features cli`.
- **Declared MSRV: Rust 1.85**, the floor for `edition = "2024"`, now enforced in CI.

### Added

- **Windows 10+ support.** splitstream now builds and runs on Windows as well as
  macOS, and the engine above the capture layer is the same code on both.
  System audio comes from WASAPI loopback, which cpal reaches by opening the
  default *output* device for input, so this needed **no new dependency** — only
  `cidre` and Metal moved behind `[target.'cfg(target_os = "macos")']`.
  `src/audio/sys_capture/` is now a module per platform behind one interface.
  What differs is the permission story: Windows does not gate loopback at all,
  so nothing prompts and `PermissionDenied { SystemAudioCapture }` cannot occur
  there, while a refused microphone surfaces as `MicCaptureFailed` because
  Windows exposes no status to read ahead of time. `download-parakeet` and the
  parakeet example now resolve the per-user app-data folder per platform
  (`%LOCALAPPDATA%` on Windows). Building for anything other than macOS or
  Windows is still a `compile_error!`, and CI now proves that stays true.
- **An `aec` feature (on by default)** gating acoustic echo cancellation and its
  `aec-rs` dependency. `aec-rs-sys` runs cmake and bindgen over a vendored
  speexdsp from its `build.rs`, which is why docs.rs has never built this crate:
  the builder cross-compiles to Apple targets from Linux, and that C build cannot
  work there. It is now off the mandatory path, and `[package.metadata.docs.rs]`
  builds with `no-default-features`. Turning it off costs echo cancellation — the
  microphone then picks up the speakers, so the far end is transcribed twice — so
  it stays on by default and nothing changes for existing users. The
  `echo_cancellation` toggles remain present and observable either way.
- **`SplitStreamHandle::errors()`** — a receiver for failures that happen *after*
  `start()` returned. Previously a denied permission, a rejected Deepgram key, and a
  failed model load were indistinguishable: `start()` returned `Ok`, the handle
  stayed alive, and no transcript ever arrived.
- **A structured error type.** `SplitStreamError` moved out of `lib.rs` into
  `error.rs` and grew from four stringly-typed variants to thirteen that carry what
  the caller needs: `PermissionDenied { what }`, `UnsupportedOsVersion { found }`,
  `TapCreationFailed { stage, status }` (the real `OSStatus`, previously formatted
  away), `NoMicDevice`, `NoDefaultInputDevice`, `ModelNotFound { path }`,
  `ModelLoad { path, source }`, `ResamplerInit { in_rate, source }`,
  `BackendStartFailed { backend, source }`, `BackendStopped { backend, reason }`,
  `MicCaptureFailed { stage, source }`, `NoBackend`, and `NoBackendFeature`. Every
  wrapping variant uses `#[source]`, so `std::error::Error::source` walks the chain.
- **Builder validation before anything is opened**: a missing model directory
  returns `ModelNotFound` naming the path *and* the command that fixes it; an empty
  Deepgram key is rejected up front; a build with no backend feature returns
  `NoBackendFeature` instead of hitting a zero-arm `match`.
- `MIN_MACOS` is public, and `SysAudioTap::new` checks the running OS version before
  touching Core Audio, so pre-14.2 systems get `UnsupportedOsVersion` rather than an
  opaque tap failure. A `compile_error!` rejects non-macOS targets at build time.
- `cargo install splitstream --features cli --bin download-parakeet` now pins the
  Hugging Face **revision** (not `main`), verifies every file against a hard-coded
  SHA-256 digest, downloads to `<name>.part` and renames only after the digest
  matches, accepts `--dest`, and honours `SPLITSTREAM_MODEL_DIR` (defaulting to
  `~/Library/Application Support/splitstream/models` instead of a CWD-relative path
  that a bundled `.app` cannot use).
- `[package.metadata.docs.rs]`, `#![cfg_attr(docsrs, feature(doc_cfg))]`, and
  `doc(cfg(...))` on every feature-gated public item.
- GitHub Actions CI on `macos-14`: rustfmt, clippy with `-D warnings`, tests,
  doctests, rustdoc, an MSRV job, a six-way feature matrix, and `cargo deny check`.
  A release workflow tags on a version bump and publishes from the tag.
- `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`, `rustfmt.toml`, `clippy.toml`,
  `deny.toml`, and retroactive `v0.1.0` … `v0.1.9` tags.
- An `examples/deepgram.rs` example — Deepgram was a headline backend with none.

### Fixed (post-review)

- **Dropping every `SplitStreamHandle` shuts the engine down again.** 0.1.x got
  this for free — the handle owned a `oneshot::Sender` whose drop woke the
  engine's receiver — and making the handle cloneable silently removed the
  guarantee, since a `Notify` has no equivalent behaviour and is only reachable
  through a live handle. A dropped handle stranded the Core Audio process tap,
  the aggregate device and the mic stream for the rest of the process, with the
  macOS recording indicator lit and no way left to stop them. `Drop for
  HandleInner` now fires the shutdown when the final clone goes away; it lives
  on the inner `Arc` payload rather than on the handle so that two threads
  racing to drop the last two clones cannot both skip it.
- **The runtime error channel is bounded** (`ERROR_CHANNEL_CAPACITY`, 64) and
  `report` uses `try_send`, so it never blocks the engine task or a backend
  thread. Its receiver stays alive inside the handle until
  `SplitStreamHandle::errors()` is called and most callers never call it, so the
  previous unbounded queue grew untouched for the whole session. On overflow the
  *newest* error is dropped: the first failure is normally the root cause and
  the rest is cascade noise, and every error is logged before it is queued.

### Security

- Updated `bytes` 1.10.1 → 1.12.1 (RUSTSEC-2026-0007, integer overflow in
  `BytesMut::reserve`) and `crossbeam-epoch` 0.9.18 → 0.9.21 (RUSTSEC-2026-0204,
  invalid pointer dereference in the `fmt::Pointer` impl). Both arrived
  transitively through `parakeet-rs`, so both affected the default build.
- `cargo deny check` now passes on advisories, bans, licences, and sources. Two
  unmaintained-crate advisories are ignored with documented reasoning in
  `deny.toml` — `paste` (RUSTSEC-2024-0436, compile-time only, via
  tokenizers → parakeet-rs) and `audiopus_sys` (RUSTSEC-2026-0150, `deepgram`
  feature only). Neither has a patched version to move to.

### Fixed

- **Every panic on a path a caller can trigger is gone** — all 31 sites the audit
  found. The worst were
  a `.unwrap()` inside the **real-time Core Audio IOProc**, where unwinding across
  the `extern "C"` boundary is undefined behaviour; `WhisperContext::new(...).expect()`
  on a *spawned thread*, so a bad model path killed a worker with no recoverable
  signal; and `HeaderValue::from_str("Token {api_key}").unwrap()` on a detached
  task — the API key is user-supplied, so a stray newline or non-ASCII byte took
  down a task nobody was watching. Models are now loaded on the calling task before
  any thread is spawned. The `expect`s that remain are provably infallible and each
  carries a comment naming the invariant. `grep -rn 'unwrap()\|expect(' src` outside
  test modules is down to a single provably-infallible `min_by_key` call, and
  `grep -rn 'println!\|eprintln!' src | grep -v src/bin` matches only doc examples.
- **An unbounded memory leak on Deepgram failure.** The Opus packet channel was
  unbounded and, once the WebSocket task exited (bad key, network drop), nothing
  read it while the engine kept encoding ~50 packets a second into it forever. The
  backend now signals its own death and the engine stops feeding it.
- **Silent audio loss.** Ring-buffer overruns in both capture callbacks were
  discarded with `let _ = push_slice(...)`. Each source now counts dropped samples
  into an atomic that the engine reports once a second.
- **Two `println!` calls inside the real-time audio callback** — with format-string
  allocation — are gone, along with all 18 other `eprintln!` sites in the library,
  which are now `log` macros at appropriate levels. Add `env_logger` or
  `tracing-subscriber` to see them; the README lists the useful target filters.
- **Real-time allocation removed** from the hot paths: the stereo downmix in both
  capture callbacks uses a preallocated scratch buffer, and the echo canceller's
  five heap allocations per 20 ms frame are down to zero.
- A persistent AEC failure used to be swallowed by `.unwrap_or(orig)` and was
  completely invisible; it is now reported, rate-limited to one line per five
  seconds so a 50 Hz failure cannot bury the log.
- Deepgram's `SpeechStarted` and `UtteranceEnd` frames reuse the `channel` key for
  an array where `Results` frames use an object, so every one of them failed to
  deserialize and was logged as a parse error. `channel` is now parsed leniently.
- Deepgram timing is treated as untrusted input: a negative, `NaN`, or absurdly
  large `start`/`duration` used to panic `Duration::from_secs_f64` on the forwarding
  task. Unrepresentable spans now collapse to zero.
- `build_resampler` rejects a zero input sample rate, which the engine's drain-count
  estimation can produce on a stalled device. `16000 / 0` is `+inf`, which passes
  rubato's own validation and yields a resampler that can never produce a frame.
- `cargo check --no-default-features --features deepgram` failed to build, because
  `examples/parakeet.rs` had no `[[example]]` entry declaring its required feature.
- README repairs: an unterminated ` ```xml ` fence that swallowed the feature-flag
  table and license section on GitHub; `tokio = "1.52.3"` in both quickstart
  snippets, a version that does not exist, so the copy-paste quickstart failed to
  resolve; a documented `api_key` settings key that `struct Settings` has no field
  for; `cmake` listed as an unconditional requirement when it has been
  deepgram-only since 0.1.5; and an MSRV of 1.80.

### Dependencies

`cargo tree --edges normal | wc -l`, before → after:

| Feature set | 0.1.9 | 0.2.0 |
|---|---:|---:|
| default (`parakeet`) | 507 | 250 |
| `--no-default-features` | 385 | 125 |

The `.crate` tarball drops from **1,086,539 bytes to 107,013 bytes** (a 90.2%
reduction), almost all of it the 1.0 MB concept-art PNG that
`exclude = ["assets/*", "models/*"]` now keeps out.

### Testing

Unit and integration tests go from 23 to 127 (117 unit, 6 in the downloader, 4
integration), plus 16 doctests and 3 `#[ignore]`d end-to-end tests that drive real
audio through the whole pipeline on a development Mac. The new coverage is concentrated where there was none: the
real-time IOProc buffer arithmetic, the mic stream-rebuild decision, the engine's
mute ordering (system audio is muted *before* the AEC so the reference is silent;
the microphone is muted *after* so the adaptive filter keeps converging) and its
Parakeet batching, the backend text helpers, Deepgram error/close/lifecycle frames,
and — new in kind — a behavioural AEC test asserting that ERLE actually improves
over 200 frames of synthetic echo, where the previous tests only checked input
validation.

## [0.1.9] — 2026-05-12

- Added `cmake` to the documented Minimum Requirements. (`d11cf46`, `388733f`)

## [0.1.8] — 2026-05-12

- Graceful fallback when no microphone device is present: the engine keeps running on
  system audio alone instead of failing to start.
- Fixed the Deepgram WebSocket close handling.
- Removed unused dependencies (57 lines out of `Cargo.lock`). (`a99431a`, `63e70fb`)

## [0.1.7] — 2026-05-12

- Substantial README rewrite. (`71955cd`)

## [0.1.6] — 2026-05-12

- Fixed HTTPS in `download-parakeet` by enabling `rustls-tls` on `reqwest`; downloads
  previously failed outright. (`ba2d333`)

## [0.1.5] — 2026-05-12

- Made the `deepgram` backend optional, removing the `cmake` build requirement from the
  default installation. (`daeb3f5`)
- Repo hygiene: removed accidentally staged embedded git repositories, ignored
  `.claude/` and `vendor/`. (`5cc2987`, `0b02a73`)

## [0.1.4] — 2026-05-12

- `download-parakeet` uses `reqwest` directly instead of shelling out to `curl`.
  (`985539c`)

## [0.1.3] — 2026-05-12

- **License:** added dual licensing — AGPL-3.0-only **or** a paid commercial license
  (`LICENSE-COMMERCIAL`). (`049d81d`, `3e4eca3`)

## [0.1.2] — 2026-05-12

- **License: switched from MIT to AGPL-3.0-only.** (`e3e15c7`, `20c70ab`)
  See the licensing-history note at the top of this file; 0.2.0 reverses this.

## [0.1.1] — 2026-05-12

- Updated the crate description and keywords for crates.io. (`85fb9b2`, `1914e99`)

## [0.1.0] — 2026-05-12

First release on crates.io.

- Simultaneous microphone (`cpal`) and system-audio (CoreAudio global process tap via
  `cidre`) capture on macOS, mixed on a 20 ms tick loop.
- Resampling of both sources to 16 kHz / 320-sample frames (`fixed-resample`/`rubato`),
  with automatic rebuild on input-device rate changes.
- Acoustic echo cancellation of the mic against the system-audio reference (`aec-rs`).
- Three transcription backends behind cargo features: `deepgram` (stereo Opus over
  WebSocket to nova-3), `whisper` (local `whisper-rs`, Metal), and `parakeet`
  (local `parakeet-rs` ONNX, the default).
- Builder API (`SplitStream::builder()`) returning a handle with per-source mute
  ("compliance mode") and shutdown.
- `splitstream` demo binary and `download-parakeet` model downloader.
- Unit tests for the resampler, echo canceller, Deepgram transcript parsing, and builder.
- README, concept art, and `parakeet` example.
- Licensed MIT. (`3b63872`)

[Unreleased]: https://github.com/samuelokirby/splitstream/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/samuelokirby/splitstream/compare/v0.1.9...v0.2.0
[0.1.9]: https://github.com/samuelokirby/splitstream/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/samuelokirby/splitstream/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/samuelokirby/splitstream/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/samuelokirby/splitstream/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/samuelokirby/splitstream/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/samuelokirby/splitstream/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/samuelokirby/splitstream/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/samuelokirby/splitstream/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/samuelokirby/splitstream/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/samuelokirby/splitstream/releases/tag/v0.1.0
