// src/local_transcriber.rs
//! Local Whisper transcription via two independent WhisperBackend instances.
//!
//! Each channel (mic, sys) owns its own model instance and inference thread so
//! Metal/GPU calls run concurrently — neither channel blocks the other.
//! Each thread accumulates its own window; when full it calls `transcribe_full`
//! then drains any batches that queued during inference to keep TTT bounded.

use scribble::{Backend, Opts, OutputType, Segment, SegmentEncoder, WhisperBackend};
use std::mem;
use std::sync::mpsc::{channel, Sender};
use std::thread;

type ScribbleResult<T> = scribble::Result<T>;

// RMS gate applied to both channels. Whisper hallucinates ("You", "[BLANK_AUDIO]", etc.)
// on quiet windows even with VAD enabled. ~0.003 ≈ -50 dBFS passes normal speech.
const ENERGY_THRESHOLD: f32 = 0.003;

struct LabeledPrinter {
    label: &'static str,
}

impl SegmentEncoder for LabeledPrinter {
    fn write_segment(&mut self, seg: &Segment) -> ScribbleResult<()> {
        let text = seg.text.trim();
        if !text.is_empty() && !is_whisper_artifact(text) {
            println!("{} {}", self.label, text);
        }
        Ok(())
    }
    fn close(&mut self) -> ScribbleResult<()> {
        Ok(())
    }
}

/// Returns true for Whisper's standard non-speech tokens and known hallucinations.
fn is_whisper_artifact(text: &str) -> bool {
    // Bracketed tokens: [BLANK_AUDIO], [MUSIC], [NOISE], [APPLAUSE], etc.
    if text.starts_with('[') && text.ends_with(']') {
        return true;
    }
    // Whisper commonly outputs these single tokens on quiet audio
    matches!(
        text,
        "you" | "You" | "Thank you." | "Thanks." | "Thanks for watching."
            | "Thank you for watching."
    )
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean_sq = samples.iter().map(|&s| s * s).sum::<f32>() / samples.len() as f32;
    mean_sq.sqrt()
}

/// Spawns one dedicated inference thread for a single audio channel.
/// Returns the sender end of its input queue.
fn spawn_channel(
    label: &'static str,
    model_path: String,
    vad_model_path: String,
    window_samples: usize,
) -> Sender<Vec<f32>> {
    let (tx, rx) = channel::<Vec<f32>>();

    thread::Builder::new()
        .name(format!("whisper-{label}"))
        .spawn(move || {
            let backend =
                WhisperBackend::new([model_path.as_str()], vad_model_path.as_str())
                    .expect("failed to load Whisper model");

            let opts = Opts {
                model_key: None,
                enable_translate_to_english: false,
                enable_voice_activity_detection: true,
                language: Some("en".to_string()),
                output_type: OutputType::Vtt,
                incremental_min_window_seconds: 1,
            };

            let mut buf = Vec::<f32>::new();

            while let Ok(samples) = rx.recv() {
                buf.extend(samples);
                if buf.len() >= window_samples {
                    let chunk = mem::take(&mut buf);
                    if rms(&chunk) > ENERGY_THRESHOLD {
                        backend
                            .transcribe_full(&opts, &mut LabeledPrinter { label }, &chunk)
                            .unwrap_or_else(|e| eprintln!("[{label}] whisper error: {e}"));
                    }
                    // Discard batches that queued during inference so the next
                    // window reflects current audio rather than a growing backlog.
                    while let Ok(stale) = rx.try_recv() {
                        buf.clear();
                        buf.extend(stale);
                    }
                }
            }
        })
        .expect("failed to spawn whisper thread");

    tx
}

/// Spawns two independent Whisper inference threads, one per channel.
/// Returns `(mic_sender, sys_sender)` — drop both to shut the threads down.
pub fn spawn(
    model_path: &str,
    vad_model_path: &str,
    window_samples: usize,
) -> (Sender<Vec<f32>>, Sender<Vec<f32>>) {
    let mic_tx = spawn_channel(
        "🎙️",
        model_path.to_string(),
        vad_model_path.to_string(),
        window_samples,
    );
    let sys_tx = spawn_channel(
        "🔊",
        model_path.to_string(),
        vad_model_path.to_string(),
        window_samples,
    );
    (mic_tx, sys_tx)
}
