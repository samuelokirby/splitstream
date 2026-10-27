//! Cloud transcription with Deepgram.
//!
//! ```bash
//! export DEEPGRAM_API_KEY=...
//! cargo run --example deepgram --features deepgram
//! ```
//!
//! Mic and system audio are interleaved into a stereo Opus stream and sent to
//! Deepgram's streaming API over a WebSocket, so each transcript still carries
//! the channel it came from — and, unlike the local backends, a confidence
//! score. The `deepgram` feature pulls in an Opus encoder, which needs `cmake`
//! at build time (`brew install cmake`).
//!
//! As with every splitstream program, macOS decides whether the capture is
//! allowed at all — see "Permissions & Signing" in the README.

use splitstream::{AudioSource, SplitStreamBuilder};

#[tokio::main]
async fn main() {
    let Ok(api_key) = std::env::var("DEEPGRAM_API_KEY") else {
        eprintln!("DEEPGRAM_API_KEY is not set");
        std::process::exit(1);
    };

    let (handle, mut transcripts) = match SplitStreamBuilder::new()
        .with_deepgram(api_key)
        .echo_cancellation(true)
        .start()
        .await
    {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("failed to start splitstream: {e}");
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            std::process::exit(1);
        }
    };

    // A rejected API key or a dropped WebSocket surfaces here. Without this
    // loop the symptom would just be a stream that never yields anything.
    if let Some(mut errors) = handle.errors() {
        tokio::spawn(async move {
            while let Some(e) = errors.recv().await {
                eprintln!("splitstream runtime error: {e}");
            }
        });
    }

    let stopper = handle.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        stopper.shutdown();
    });

    // Deepgram sends interim results before it commits to a transcript; only
    // the final ones are worth persisting.
    while let Some(t) = transcripts.recv().await {
        if !t.is_final {
            continue;
        }
        let label = match t.source {
            AudioSource::Mic => "🎤 mic",
            AudioSource::Sys => "🖥️  sys",
            _ => "?      ",
        };
        let confidence = match t.confidence {
            Some(c) => format!(" ({c:.2})"),
            None => String::new(),
        };
        println!(
            "[{:>7.2}s–{:>7.2}s] {label}  {}{confidence}",
            t.start.as_secs_f32(),
            t.end.as_secs_f32(),
            t.text
        );
    }
}
