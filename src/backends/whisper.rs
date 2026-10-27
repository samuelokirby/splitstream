//! Local Whisper transcription backend via whisper-rs (Metal).
//!
//! Each channel (mic, sys) gets its own WhisperContext so Metal/GPU calls can
//! run concurrently. Each thread accumulates audio until `window_samples` is
//! reached, runs full inference, drains segments, and repeats.
//!
//! Both contexts are loaded on the calling task (inside `spawn_blocking`)
//! before any thread starts, so a bad model path surfaces as
//! [`SplitStreamError::ModelLoad`] instead of killing a detached worker.

use std::mem;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use log::error;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::error::{ErrorReporter, SplitStreamError};
use crate::transcript::{AudioSource, Transcript, samples_to_duration};

/// Sender half of the public transcript channel (bounded, drop-oldest).
type TranscriptTx = tokio::sync::broadcast::Sender<Transcript>;

const BACKEND: &str = "whisper";

/// Returns true for Whisper's standard non-speech hallucinations.
fn is_artifact(text: &str) -> bool {
    if text.starts_with('[') && text.ends_with(']') {
        return true;
    }
    matches!(
        text,
        "you"
            | "You"
            | "Thank you."
            | "Thanks."
            | "Thanks for watching."
            | "Thank you for watching."
    )
}

fn load_context(model_path: &Path) -> Result<WhisperContext, SplitStreamError> {
    if !model_path.exists() {
        return Err(SplitStreamError::ModelNotFound {
            path: model_path.to_path_buf(),
        });
    }
    WhisperContext::new_with_params(
        &model_path.to_string_lossy(),
        WhisperContextParameters::default(),
    )
    .map_err(|e| SplitStreamError::ModelLoad {
        path: model_path.to_path_buf(),
        source: Box::new(e),
    })
}

fn spawn_channel(
    source: AudioSource,
    ctx: WhisperContext,
    model_path: PathBuf,
    window_samples: usize,
    transcript_tx: TranscriptTx,
    errors: ErrorReporter,
) -> Result<Sender<Vec<f32>>, SplitStreamError> {
    let (tx, rx) = channel::<Vec<f32>>();
    let label = match source {
        AudioSource::Mic => "mic",
        AudioSource::Sys => "sys",
    };

    thread::Builder::new()
        .name(format!("whisper-{label}"))
        .spawn(move || {
            let mut state = match ctx.create_state() {
                Ok(s) => s,
                Err(e) => {
                    errors.report(SplitStreamError::ModelLoad {
                        path: model_path,
                        source: Box::new(e),
                    });
                    return;
                }
            };
            let mut buf = Vec::<f32>::new();
            // Stream position, in 16kHz samples consumed by this thread. The
            // engine sends one 320-sample frame per 20ms tick, zero filled when
            // the source is silent, so this tracks the stream exactly.
            let mut samples_seen: u64 = 0;

            while let Ok(first) = rx.recv() {
                buf.extend(first);
                while let Ok(more) = rx.try_recv() {
                    buf.extend(more);
                }

                if buf.len() < window_samples {
                    continue;
                }

                let chunk = mem::take(&mut buf);
                let chunk_start = samples_to_duration(samples_seen);
                samples_seen += chunk.len() as u64;
                let chunk_end = samples_to_duration(samples_seen);

                let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                params.set_language(Some("en"));
                params.set_print_special(false);
                params.set_print_progress(false);
                params.set_print_realtime(false);
                params.set_print_timestamps(false);

                if let Err(e) = state.full(params, &chunk) {
                    error!("Whisper inference error ({label}): {e}");
                    continue;
                }

                let n = state.full_n_segments();
                for i in 0..n {
                    if let Some(seg) = state.get_segment(i) {
                        if let Ok(text) = seg.to_str() {
                            let text = text.trim();
                            if !text.is_empty() && !is_artifact(text) {
                                // whisper.cpp segment timestamps are relative to
                                // the window, not the stream; attributing the
                                // whole window is coarse but never wrong.
                                let _ = transcript_tx.send(Transcript::new(
                                    source,
                                    text.to_string(),
                                    true,
                                    chunk_start,
                                    chunk_end,
                                    None,
                                ));
                            }
                        }
                    }
                }
            }
        })
        .map_err(|e| SplitStreamError::BackendStartFailed {
            backend: BACKEND,
            source: Box::new(e),
        })?;

    Ok(tx)
}

/// Loads both Whisper contexts, then spawns two independent inference threads
/// (mic + sys). Returns `(mic_sender, sys_sender)` — drop both to shut the
/// threads down.
pub(crate) async fn spawn(
    model_path: &Path,
    window_samples: usize,
    transcript_tx: TranscriptTx,
    errors: ErrorReporter,
) -> Result<(Sender<Vec<f32>>, Sender<Vec<f32>>), SplitStreamError> {
    let path: PathBuf = model_path.to_path_buf();

    // Model load is slow and blocking; do it on the blocking pool but on the
    // caller's timeline, so failures are returned rather than thrown away on a
    // detached worker thread.
    let load_path = path.clone();
    let (mic_ctx, sys_ctx) = tokio::task::spawn_blocking(move || {
        let mic = load_context(&load_path)?;
        let sys = load_context(&load_path)?;
        Ok::<_, SplitStreamError>((mic, sys))
    })
    .await
    .map_err(|e| SplitStreamError::BackendStartFailed {
        backend: BACKEND,
        source: Box::new(e),
    })??;

    let mic_tx = spawn_channel(
        AudioSource::Mic,
        mic_ctx,
        path.clone(),
        window_samples,
        transcript_tx.clone(),
        errors.clone(),
    )?;
    let sys_tx = spawn_channel(
        AudioSource::Sys,
        sys_ctx,
        path,
        window_samples,
        transcript_tx,
        errors,
    )?;
    Ok((mic_tx, sys_tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_annotations_are_artifacts() {
        // Whisper emits these for non-speech audio.
        assert!(is_artifact("[BLANK_AUDIO]"));
        assert!(is_artifact("[Music]"));
        assert!(is_artifact("[ Silence ]"));
    }

    #[test]
    fn the_known_hallucination_phrases_are_artifacts() {
        for phrase in [
            "you",
            "You",
            "Thank you.",
            "Thanks.",
            "Thanks for watching.",
            "Thank you for watching.",
        ] {
            assert!(is_artifact(phrase), "{phrase}");
        }
    }

    #[test]
    fn real_speech_is_not_an_artifact() {
        assert!(!is_artifact("Thank you for joining the call."));
        assert!(!is_artifact("thanks for watching"));
        assert!(!is_artifact("you know"));
        assert!(!is_artifact("hello world"));
    }

    #[test]
    fn a_lone_bracket_is_not_an_artifact() {
        // `starts_with('[') && ends_with(']')` must not fire on a one-character
        // string that satisfies both at once.
        assert!(!is_artifact("["));
        assert!(!is_artifact("]"));
    }

    #[test]
    fn the_empty_string_is_not_an_artifact() {
        assert!(!is_artifact(""));
    }

    #[test]
    fn a_missing_model_is_reported_as_model_not_found_not_a_load_failure() {
        let path = Path::new("/nonexistent/splitstream/ggml-small.en.bin");
        match load_context(path) {
            Err(SplitStreamError::ModelNotFound { path: p }) => assert_eq!(p, path),
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("a nonexistent model must not load"),
        }
    }
}
