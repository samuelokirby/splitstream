// src/local_transcriber.rs
//! Local Whisper transcription via two independent inference threads.
//!
//! Each channel (mic, sys) gets its own WhisperContext so Metal/GPU calls can
//! run concurrently. Each thread drains all available audio, waits until the
//! rolling buffer reaches `window_samples`, runs full inference, then clears
//! the buffer and repeats.

use std::mem;
use std::sync::mpsc::{channel, Sender};
use std::thread;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Returns true for Whisper's standard non-speech hallucinations.
fn is_artifact(text: &str) -> bool {
    if text.starts_with('[') && text.ends_with(']') {
        return true;
    }
    matches!(
        text,
        "you" | "You" | "Thank you." | "Thanks." | "Thanks for watching."
            | "Thank you for watching."
    )
}

fn spawn_channel(
    label: &'static str,
    model_path: String,
    window_samples: usize,
) -> Sender<Vec<f32>> {
    let (tx, rx) = channel::<Vec<f32>>();

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

                // Drain everything else that arrived while we were waiting.
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
                                println!("{} {}", label, text);
                            }
                        }
                    }
                }
            }
        })
        .expect("failed to spawn whisper thread");

    tx
}

/// Spawns two independent Whisper inference threads, one per channel.
/// Returns `(mic_sender, sys_sender)` — drop both to shut the threads down.
pub fn spawn(model_path: &str, window_samples: usize) -> (Sender<Vec<f32>>, Sender<Vec<f32>>) {
    let mic_tx = spawn_channel("🎙️", model_path.to_string(), window_samples);
    let sys_tx = spawn_channel("🔊", model_path.to_string(), window_samples);
    (mic_tx, sys_tx)
}
