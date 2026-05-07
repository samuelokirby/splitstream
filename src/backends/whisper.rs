//! Local Whisper transcription backend via whisper-rs (Metal).
//!
//! Each channel (mic, sys) gets its own WhisperContext so Metal/GPU calls can
//! run concurrently. Each thread accumulates audio until `window_samples` is
//! reached, runs full inference, drains segments, and repeats.

use std::mem;
use std::sync::mpsc::{channel, Sender};
use std::thread;

use tokio::sync::mpsc::UnboundedSender;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::transcript::{AudioSource, Transcript};

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

fn spawn_channel(
    source: AudioSource,
    model_path: String,
    window_samples: usize,
    transcript_tx: UnboundedSender<Transcript>,
) -> Sender<Vec<f32>> {
    let (tx, rx) = channel::<Vec<f32>>();
    let label = match source {
        AudioSource::Mic => "mic",
        AudioSource::Sys => "sys",
    };

    thread::Builder::new()
        .name(format!("whisper-{label}"))
        .spawn(move || {
            let ctx =
                WhisperContext::new_with_params(&model_path, WhisperContextParameters::default())
                    .expect("failed to load Whisper model");
            let mut state = ctx.create_state().expect("failed to create Whisper state");
            let mut buf = Vec::<f32>::new();

            while let Ok(first) = rx.recv() {
                buf.extend(first);
                while let Ok(more) = rx.try_recv() {
                    buf.extend(more);
                }

                if buf.len() < window_samples {
                    continue;
                }

                let chunk = mem::take(&mut buf);

                let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                params.set_language(Some("en"));
                params.set_print_special(false);
                params.set_print_progress(false);
                params.set_print_realtime(false);
                params.set_print_timestamps(false);

                if let Err(e) = state.full(params, &chunk) {
                    eprintln!("Whisper error ({label}): {e}");
                    continue;
                }

                let n = state.full_n_segments();
                for i in 0..n {
                    if let Some(seg) = state.get_segment(i) {
                        if let Ok(text) = seg.to_str() {
                            let text = text.trim();
                            if !text.is_empty() && !is_artifact(text) {
                                let _ = transcript_tx.send(Transcript {
                                    source,
                                    text: text.to_string(),
                                    is_final: true,
                                });
                            }
                        }
                    }
                }
            }
        })
        .expect("failed to spawn whisper thread");

    tx
}

/// Spawns two independent Whisper inference threads (mic + sys).
/// Returns `(mic_sender, sys_sender)` — drop both to shut the threads down.
pub(crate) fn spawn(
    model_path: &str,
    window_samples: usize,
    transcript_tx: UnboundedSender<Transcript>,
) -> (Sender<Vec<f32>>, Sender<Vec<f32>>) {
    let mic_tx = spawn_channel(
        AudioSource::Mic,
        model_path.to_string(),
        window_samples,
        transcript_tx.clone(),
    );
    let sys_tx = spawn_channel(
        AudioSource::Sys,
        model_path.to_string(),
        window_samples,
        transcript_tx,
    );
    (mic_tx, sys_tx)
}
