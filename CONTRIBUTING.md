# Contributing to splitstream

Thanks for your interest. `splitstream` is a small macOS crate, so the process is light.

## No CLA required

**There is no Contributor License Agreement.** `splitstream` is licensed under
[MIT](LICENSE), and contributions are accepted under the same terms — by opening a pull
request you agree that your contribution is licensed MIT. Nothing is signed, no rights
are assigned, and there is no separate commercial license to worry about.

(The project was AGPL-3.0 with a dual commercial option from 0.1.2 through 0.1.9. That
is over — see [CHANGELOG.md](CHANGELOG.md). If you passed on contributing because of
the license or a CLA, please reconsider.)

## Requirements

- **macOS 14.2 or later.** The CoreAudio global process tap the crate is built on does
  not exist on earlier releases, and it does not exist on any other platform. The crate
  will not compile off macOS by design.
- **Rust 1.85 or later** — the MSRV, set by `edition = "2024"`. Raising it is a
  breaking change and needs a `rust-version` bump plus a CHANGELOG note.
- **`cmake`**, only if you build the `deepgram` feature (it compiles `opus`).
- A microphone and audio output. Many code paths cannot be exercised without them.

## Getting started

```bash
git clone https://github.com/samuelokirby/splitstream
cd splitstream
cargo build
RUST_LOG=info cargo run --features cli --bin splitstream
```

Model weights are not in the repo and are gitignored. Fetch the Parakeet model with:

```bash
cargo run --features cli --bin download-parakeet
```

## Features

| Feature | What it pulls in |
|---|---|
| `parakeet` (default) | Local ONNX transcription via `parakeet-rs` |
| `deepgram` | Cloud transcription: Opus + WebSocket (needs `cmake`) |
| `whisper` | Local `whisper-rs` (Metal). Work in progress, undocumented. |
| `cli` | Dependencies used **only** by the bundled binaries (`colored`, `config`, `dotenvy`, `reqwest`, `sha2`) |

Keep `cli` dependencies out of the library. If a change makes `src/lib.rs` (or anything
it reaches) depend on `colored`, `config`, `dotenvy`, `reqwest`, or `sha2`, that is a design
problem, not a `Cargo.toml` problem.

## Before you open a PR

Everything CI runs, locally:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --all-features --doc
cargo check --all-targets --no-default-features --features deepgram
cargo check --all-targets --no-default-features --features cli
cargo check --all-targets --all-features
cargo deny check
```

House rules that CI cannot check for you:

- **No panics in library code.** `unwrap()`/`expect()` in `src/` outside `#[cfg(test)]`
  must be provably infallible and carry a comment naming the invariant. Everything else
  returns `SplitStreamError`. This matters most on spawned threads and inside the
  real-time CoreAudio IOProc callback, where a panic is unrecoverable and invisible.
- **No allocation or `println!` in the real-time audio callback.** Use preallocated
  scratch buffers; report conditions with atomic counters read from outside the callback.
- **Use `log` macros, never `println!`/`eprintln!`,** anywhere outside `src/bin/`.
- Public API changes need doc comments with a compiling ` ```no_run ` example, and a
  CHANGELOG entry under `Unreleased`.

## Local agent/editor files

`CLAUDE.md`, `.claude/`, and `vendor/` are intentionally gitignored: they are local
tooling state, not project configuration. Leave them out of PRs.

## Commit and PR conventions

Short imperative subject lines (`Fix Deepgram WebSocket close handling`). One logical
change per PR. Version bumps and `Cargo.lock` updates belong to the maintainer's
release commit, not to feature PRs — the release workflow tags on a version bump.

## Reporting problems

Bugs and feature requests go to
[GitHub Issues](https://github.com/samuelokirby/splitstream/issues). Security issues do
**not** — see [SECURITY.md](SECURITY.md).
