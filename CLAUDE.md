# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build                        # build
cargo run                          # run (reads settings.toml from cwd)
RUST_LOG=info cargo run            # with logging
RUST_LOG=debug cargo run           # verbose (prints every audio tick)
cargo test                         # run all tests
cargo test test_frame_size         # run a single test by name
cargo nextest run                  # faster test runner (cargo-nextest installed)
```

Requires macOS — CoreAudio/cidre tap only runs on macOS. The `cidre` crate is pinned to a specific git rev in `Cargo.toml`.

## Architecture

Splitstream captures mic and system audio simultaneously, applies echo cancellation, and feeds a selectable transcription backend.

**Audio pipeline (single Tokio task, 20ms tick loop in `main.rs`):**
1. **Mic** — captured via `cpal` default input device → pushed to `HeapRb<f32>`
2. **Sys** — captured via `SysAudioTap` (CoreAudio global process tap + aggregate device, `src/sys_audio_tap.rs`) → pushed to a second `HeapRb<f32>`
3. Each tick drains both ring buffers into accumulation vecs, then resamples each to 16kHz/320-sample frames using `SincFixedOut` from `fixed-resample/rubato`
4. **AEC** (`src/echo_cancellation.rs`) — applied to mic using sys as reference, only when mic ≥ 16kHz and `echo_cancellation = true`
5. Resampled frames are forwarded to the active `TranscriptionBackend`

**Transcription backends** (selected by `transcription_backend` in `settings.toml`):

| Value | Backend | File |
|-------|---------|------|
| `"deepgram"` | Cloud: stereo Opus → WebSocket | `src/websocket_client.rs` |
| `"whisper"` | Local: whisper-rs (Metal) | `src/local_transcriber.rs` |
| `"parakeet"` | Local: parakeet-rs ONNX | `src/parakeet_transcriber.rs` |

**Deepgram path:** mic + sys interleaved as stereo → Opus encoded → WebSocket to Deepgram nova-3 API. Requires `DEEPGRAM_API_KEY` in `.env`.

**Whisper path:** 16kHz PCM accumulated in `local_transcriber.rs` to `whisper_window_seconds * 16000` samples, then full inference via `whisper-rs` (each channel loads its own `WhisperContext` for Metal GPU concurrency). Configured by `whisper_model_path` and `whisper_window_seconds`.

**Parakeet path:** 16kHz PCM sent in exact 2560-sample (160ms) chunks to two inference threads sharing one `ParakeetEOUHandle`. Each thread buffers token output and flushes on sentence boundary, word count, or silence timeout. Configured by `parakeet_model_dir`.

**Key constants in `main.rs`:**
- `OUT_SAMPLE_RATE = 16_000`
- `FINAL_FRAME_SIZE = 320` — 20ms @ 16kHz, one frame per tick
- `WHISPER_BATCH_SAMPLES = 8_000` — 0.5s per send; local_transcriber accumulates more before inference
- `PARAKEET_BATCH_SAMPLES = 2_560` — exact 160ms chunk required by ParakeetEOU API

**parakeet-rs API notes:**
- `ParakeetEOU::transcribe(&chunk, reset_on_eou)` — expects exactly 2560 samples; maintains a 4-second rolling buffer internally; needs ~1 second (16000 samples) of warmup before producing output
- Returned tokens contain `▁` (U+2581) SentencePiece word-boundary markers — strip with `text.replace('\u{2581}', " ")`
- `ParakeetEOUHandle` is `Clone` (Arc-wrapped); two threads can share one loaded model
- Do NOT skip/drain chunks — the model's internal buffer assumes a contiguous audio stream; gaps cause garbled output

**Models directory** (not in repo, must be provided):
- `./models/ggml-small.en.bin` — Whisper small English (whisper.cpp format)
- `./models/parakeet-eou/` — Parakeet EOU ONNX model directory

## Configuration

`settings.toml` (loaded from cwd at startup):

```toml
compliance_mode_on_start = false   # if true, sys audio starts muted
echo_cancellation = true
transcription_backend = "parakeet" # "deepgram" | "whisper" | "parakeet"
whisper_model_path = "./models/ggml-small.en.bin"
whisper_window_seconds = 3
parakeet_model_dir = "./models/parakeet-eou"
```

Deepgram API key goes in `.env` as `DEEPGRAM_API_KEY=...`.
