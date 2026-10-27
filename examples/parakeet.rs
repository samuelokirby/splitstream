//! Local transcription with the Parakeet EOU backend — no API key, no network.
//!
//! ```bash
//! cargo install splitstream --features cli --bin download-parakeet
//! download-parakeet                       # prints the directory it wrote to
//! cargo run --example parakeet
//! ```
//!
//! The model is NVIDIA Parakeet-Realtime-EOU-120m-v1 (English, streaming, with
//! end-of-utterance detection), ONNX-exported and ~481 MB on disk. Its weights
//! are under the NVIDIA Open Model License, which permits commercial use — the
//! splitstream crate itself is MIT.
//!
//! Running under `cargo run` means the process is not a signed `.app`, so macOS
//! may refuse the microphone or the system-audio tap outright. See the
//! "Permissions & Signing" section of the README, and `examples/bundled_app/`
//! for a signed bundle that prompts properly.

use std::path::PathBuf;

use splitstream::{AudioSource, SplitStreamBuilder};

#[tokio::main]
async fn main() {
    // `env_logger` is not a dependency of the crate; with any `log` subscriber
    // installed you would see device selection, sample-rate changes, and
    // dropped-audio counters here.
    let model_dir = parakeet_model_dir();
    println!("model: {}", model_dir.display());

    let (handle, mut transcripts) = match SplitStreamBuilder::new()
        .with_parakeet(&model_dir)
        .echo_cancellation(true)
        .start()
        .await
    {
        Ok(pair) => pair,
        Err(e) => {
            // Covers SplitStreamError::ModelNotFound, ::PermissionDenied and
            // ::UnsupportedOsVersion, among others.
            eprintln!("failed to start splitstream: {e}");
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            std::process::exit(1);
        }
    };

    // Failures *after* start-up — a backend thread that dies, a tap that stops
    // delivering — arrive here rather than presenting as silence.
    if let Some(mut errors) = handle.errors() {
        tokio::spawn(async move {
            while let Some(e) = errors.recv().await {
                eprintln!("splitstream runtime error: {e}");
            }
        });
    }

    // The handle is `Clone` and every method takes `&self`, so the Ctrl-C task
    // can own one while the rest of the program keeps its own for mute control.
    let stopper = handle.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        stopper.shutdown();
    });

    while let Some(t) = transcripts.recv().await {
        let label = match t.source {
            AudioSource::Mic => "🎤 mic",
            AudioSource::Sys => "🖥️  sys",
            // `AudioSource` is `#[non_exhaustive]`.
            _ => "?      ",
        };
        println!("[{:>7.2}s] {label}  {}", t.start.as_secs_f32(), t.text);
    }

    let dropped = transcripts.dropped();
    if dropped > 0 {
        eprintln!("note: {dropped} transcripts were dropped by a slow consumer");
    }
}

/// Locate the model directory holding `encoder.onnx`, `decoder_joint.onnx` and
/// `tokenizer.json`, matching what `download-parakeet` writes:
///
/// 1. `$SPLITSTREAM_MODEL_DIR/parakeet-eou`
/// 2. `./models/parakeet-eou`, if it exists (convenient from a git checkout)
/// 3. the platform default: `~/Library/Application Support/splitstream/models/parakeet-eou`
///    on macOS, `%LOCALAPPDATA%\splitstream\models\parakeet-eou` on Windows
///
/// Step 3 matters for the bundled `.app` in `examples/bundled_app/`: a bundle
/// launched from Finder has `/` as its working directory, so a relative path
/// would never resolve.
fn parakeet_model_dir() -> PathBuf {
    if let Some(root) = std::env::var_os("SPLITSTREAM_MODEL_DIR") {
        return PathBuf::from(root).join("parakeet-eou");
    }

    let local = PathBuf::from("./models/parakeet-eou");
    if local.is_dir() {
        return local;
    }

    default_model_dir()
}

/// Where `download-parakeet` writes by default on this platform.
#[cfg(target_os = "macos")]
fn default_model_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Library/Application Support/splitstream/models/parakeet-eou")
}

#[cfg(target_os = "windows")]
fn default_model_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("splitstream\\models\\parakeet-eou")
}
