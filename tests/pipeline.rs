//! End-to-end pipeline tests. **All of these are `#[ignore]`d.**
//!
//! They need things CI cannot provide: real Core Audio devices, a granted
//! microphone/audio-capture TCC consent, and an unpacked model on disk. Run
//! them by hand on a development Mac:
//!
//! ```bash
//! # once: fetch the model
//! cargo run --features cli --bin download-parakeet
//!
//! cargo test --test pipeline -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **They run fine with system output muted**, so you can do this in a quiet
//! room or a meeting. A process tap reads each application's output stream
//! before it reaches the output device, so hardware volume and mute do not
//! affect what the tap captures — verified by running the suite muted. Every
//! test here mutes the microphone and asserts only on [`AudioSource::Sys`], so
//! nothing depends on sound actually reaching the air.
//!
//! `SPLITSTREAM_MODEL_DIR` overrides where the model is looked for.
//!
//! `speaks_system_audio_back_as_a_transcript` drives real audio through the
//! whole pipeline — Core Audio process tap → ring buffer → resampler → AEC →
//! backend — by having macOS speak a known sentence through the default output
//! device and asserting the words come back out of the transcript stream. That
//! is the only test in the repository that exercises the pipeline as a whole.

#![cfg(feature = "parakeet")]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use splitstream::{AudioSource, SplitStreamBuilder, Transcript};

/// Where the unpacked Parakeet EOU model lives.
fn model_dir() -> PathBuf {
    std::env::var_os("SPLITSTREAM_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./models/parakeet-eou"))
}

fn require_model() -> PathBuf {
    let dir = model_dir();
    assert!(
        dir.exists(),
        "model directory {} not found — run \
         `cargo run --features cli --bin download-parakeet`, or set \
         SPLITSTREAM_MODEL_DIR",
        dir.display()
    );
    dir
}

/// Words the speech synthesiser says and the transcript should contain.
const SPOKEN: &str = "the quick brown fox jumps over the lazy dog";

/// Speak `SPOKEN` through the default output device, so system-audio capture
/// has something real to record.
///
/// macOS only: `say` ships with the OS. The Windows equivalent would be a
/// PowerShell `System.Speech` call, which is left for whoever runs these on a
/// Windows box with a real audio device.
#[cfg(target_os = "macos")]
fn speak() -> std::io::Result<std::process::Child> {
    Command::new("say").arg("-r").arg("160").arg(SPOKEN).spawn()
}

/// Collect transcripts until `predicate` is satisfied or `timeout` elapses.
async fn collect_until(
    stream: &mut splitstream::TranscriptStream,
    timeout: Duration,
    mut predicate: impl FnMut(&Transcript) -> bool,
) -> (bool, Vec<Transcript>) {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, stream.recv()).await {
            Ok(Some(t)) => {
                println!(
                    "[{:>6.2}s–{:>6.2}s] {} {}",
                    t.start.as_secs_f32(),
                    t.end.as_secs_f32(),
                    t.source,
                    t.text
                );
                let hit = predicate(&t);
                seen.push(t);
                if hit {
                    return (true, seen);
                }
            }
            // The stream closed: the engine stopped.
            Ok(None) => break,
            Err(_) => break,
        }
    }
    (false, seen)
}

/// The full pipeline, end to end, with real audio.
///
/// macOS only, because it drives the speakers with `say`.
#[cfg(target_os = "macos")]
#[ignore = "needs audio hardware, TCC consent, speakers, and a downloaded model"]
#[tokio::test]
async fn speaks_system_audio_back_as_a_transcript() {
    let dir = require_model();

    let (handle, mut transcripts) = SplitStreamBuilder::new()
        .with_parakeet(&dir)
        .echo_cancellation(true)
        // Only the system-audio path is under test; a live mic would add
        // whatever the room is doing.
        .mic_muted(true)
        .start()
        .await
        .expect("start");

    if let Some(mut errors) = handle.errors() {
        tokio::spawn(async move {
            while let Some(e) = errors.recv().await {
                eprintln!("runtime error: {e}");
            }
        });
    }

    // Let the tap settle and the model warm up (~1s) before making noise.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut child = speak().expect("`say` is part of macOS");

    // Any of these words coming back proves audio traversed the whole chain.
    let (found, seen) = collect_until(&mut transcripts, Duration::from_secs(30), |t| {
        matches!(t.source, AudioSource::Sys)
            && ["quick", "brown", "fox", "lazy", "dog"]
                .iter()
                .any(|w| t.text.to_lowercase().contains(w))
    })
    .await;

    let _ = child.wait();
    handle.shutdown();

    assert!(
        found,
        "nothing recognisable came back from the system-audio tap. \
         Check the output volume is up and that audio-capture consent is granted. \
         Saw {} segment(s): {:?}",
        seen.len(),
        seen.iter().map(|t| &t.text).collect::<Vec<_>>()
    );

    // Timestamps must form a usable timeline, not all-zero placeholders.
    for t in &seen {
        assert!(t.end >= t.start, "inverted span: {t:?}");
        assert_eq!(t.duration(), t.end - t.start);
    }
    assert!(
        seen.iter().any(|t| t.end > Duration::ZERO),
        "every segment is at offset zero — the sample clock is not advancing"
    );
}

/// Muting system audio must actually stop transcripts from that source.
#[ignore = "needs audio hardware, TCC consent, speakers, and a downloaded model"]
#[tokio::test]
async fn muting_system_audio_stops_system_transcripts() {
    let dir = require_model();

    let (handle, mut transcripts) = SplitStreamBuilder::new()
        .with_parakeet(&dir)
        .mic_muted(true)
        .sys_muted(true) // compliance mode from the start
        .start()
        .await
        .expect("start");

    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut child = speak().expect("`say` is part of macOS");

    let (found, seen) = collect_until(&mut transcripts, Duration::from_secs(20), |t| {
        matches!(t.source, AudioSource::Sys) && !t.text.trim().is_empty()
    })
    .await;

    let _ = child.wait();
    handle.shutdown();

    assert!(
        !found,
        "compliance mode leaked system audio: {:?}",
        seen.iter().map(|t| &t.text).collect::<Vec<_>>()
    );
}

/// A cloned handle must be able to stop the session, and the transcript stream
/// must then end rather than hang.
#[ignore = "needs audio hardware, TCC consent, and a downloaded model"]
#[tokio::test]
async fn a_cloned_handle_shuts_the_session_down() {
    let dir = require_model();

    let (handle, mut transcripts) = SplitStreamBuilder::new()
        .with_parakeet(&dir)
        .start()
        .await
        .expect("start");

    let stopper = handle.clone();
    stopper.set_mic_muted(true);
    assert!(handle.mic_muted(), "controls are shared between clones");

    tokio::time::sleep(Duration::from_secs(1)).await;
    stopper.shutdown();

    // The engine drops the sender, so the stream terminates.
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while transcripts.recv().await.is_some() {}
    })
    .await;
    assert!(
        ended.is_ok(),
        "the transcript stream did not end after shutdown"
    );
}

/// Dropping every handle must stop the engine, not strand it.
///
/// Regression test for the 0.2.0 handle rework: 0.1.x held a `oneshot::Sender`
/// whose drop woke the engine, and making the handle cloneable removed that
/// guarantee. A stranded engine keeps the Core Audio process tap, the aggregate
/// device and the mic stream alive for the rest of the process — with the macOS
/// recording indicator lit — and nothing can stop it, because the shutdown
/// signal is only reachable through a handle that no longer exists.
///
/// The observable consequence is that the transcript stream closes: the engine
/// owns the broadcast sender, so `recv()` returns `None` once the task exits.
#[tokio::test]
#[ignore = "needs Core Audio devices, TCC consent and the model"]
async fn dropping_every_handle_stops_the_engine() {
    let model = require_model();

    let (handle, mut transcripts) = SplitStreamBuilder::new()
        .with_parakeet(&model)
        .mic_muted(true)
        .start()
        .await
        .expect("engine should start");

    let clone = handle.clone();
    drop(handle);

    // One clone is still alive, so the engine must still be running.
    assert!(
        tokio::time::timeout(Duration::from_millis(500), transcripts.recv())
            .await
            .is_err(),
        "the stream must stay open while a handle clone survives"
    );

    drop(clone);

    // Now nothing holds the engine open: the sender drops and the stream ends.
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while transcripts.recv().await.is_some() {}
    })
    .await;

    assert!(
        closed.is_ok(),
        "dropping the last handle must shut the engine down and close the \
         transcript stream; it is still running"
    );
}
