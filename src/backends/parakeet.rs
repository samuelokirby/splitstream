//! ParakeetEOU streaming transcription backend via parakeet-rs.
//!
//! `ParakeetEOU::transcribe` expects 160ms chunks (2560 samples at 16kHz).
//! The model maintains a 4-second rolling audio buffer internally and needs
//! ~1 second (≈ 6.25 chunks) of warmup before producing output.
//!
//! Two inference threads share one `ParakeetEOUHandle` (Arc<Mutex<OnnxSessions>>)
//! so mic and sys inference serialize through one lock rather than both hitting
//! the ORT global environment lock independently.

use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use log::debug;
use parakeet_rs::{ExecutionConfig, ParakeetEOU, ParakeetEOUHandle};
use tokio::sync::mpsc::UnboundedSender;

use crate::transcript::{AudioSource, Transcript};
use crate::SplitStreamError;

// Print buffered text after this many consecutive empty iterations (with text
// waiting). 3 × 160ms ≈ 480ms — catches end-of-phrase pauses quickly.
const FLUSH_SILENCE_CHUNKS: usize = 3;

// Print after this many words even during continuous speech.
const MAX_WORDS_BEFORE_FLUSH: usize = 8;

// Hard-reset the model stream state after this many consecutive empty
// iterations. Must be > warmup period (~6.25 chunks). 12 × 160ms = 1.92s.
const RESET_SILENCE_CHUNKS: usize = 12;

fn ends_sentence(text: &str) -> bool {
    matches!(text.trim_end().chars().last(), Some('.' | '?' | '!'))
}

fn fix_sentencepiece(text: &str) -> String {
    text.replace('\u{2581}', " ")
}

fn flush(buf: &mut String, source: AudioSource, tx: &UnboundedSender<Transcript>) {
    let text = fix_sentencepiece(buf).replace("[EOU]", "").trim().to_string();
    if !text.is_empty() {
        let _ = tx.send(Transcript { source, text, is_final: true });
    }
    buf.clear();
}

fn spawn_channel(
    source: AudioSource,
    handle: ParakeetEOUHandle,
    transcript_tx: UnboundedSender<Transcript>,
) -> Sender<Vec<f32>> {
    let (tx, rx) = channel::<Vec<f32>>();
    let label = match source {
        AudioSource::Mic => "mic",
        AudioSource::Sys => "sys",
    };

    thread::Builder::new()
        .name(format!("parakeet-{label}"))
        .spawn(move || {
            let mut parakeet = ParakeetEOU::from_shared(&handle);
            let mut text_buf = String::new();
            let mut empty_streak = 0usize;
            // Guard: model must produce at least one token before a
            // silence-triggered reset is allowed. Without this the reset fires
            // during the ~1s startup warmup (empty returns × 6), which restarts
            // warmup and creates a reset loop.
            let mut warmed_up = false;

            debug!("[{label}] started, warming up");

            loop {
                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(chunk) => {
                        // Collect ALL queued chunks to prevent gaps in the 4s rolling buffer.
                        let mut pending = vec![chunk];
                        while let Ok(more) = rx.try_recv() {
                            pending.push(more);
                        }

                        let mut got_text = false;
                        for c in &pending {
                            match parakeet.transcribe(c, false) {
                                Ok(text) if !text.trim().is_empty() => {
                                    text_buf.push_str(&text);
                                    got_text = true;
                                }
                                Ok(_) => {}
                                Err(e) => eprintln!("Parakeet error ({label}): {e}"),
                            }
                        }

                        if got_text && !warmed_up {
                            warmed_up = true;
                            debug!("[{label}] warmup complete");
                        }

                        empty_streak = if got_text { 0 } else { empty_streak + 1 };

                        debug!(
                            "[{label}] streak={empty_streak} warmed={warmed_up} \
                             buf={} got={got_text}",
                            text_buf.len()
                        );

                        // --- Print threshold ---
                        if !text_buf.is_empty() {
                            let decoded = fix_sentencepiece(&text_buf).replace("[EOU]", "");
                            let words = decoded.split_whitespace().count();
                            if ends_sentence(&decoded)
                                || words >= MAX_WORDS_BEFORE_FLUSH
                                || empty_streak >= FLUSH_SILENCE_CHUNKS
                            {
                                flush(&mut text_buf, source, &transcript_tx);
                                empty_streak = 0;
                            }
                        }

                        // --- Reset threshold (warmup-gated) ---
                        // Fires exactly once per silence event (== not >=).
                        // Speech resuming sets got_text=true which resets empty_streak to 0.
                        if empty_streak == RESET_SILENCE_CHUNKS && warmed_up {
                            debug!("[{label}] silence — flushing and resetting model");
                            let silence = vec![0.0f32; 2560];
                            for _ in 0..3 {
                                if let Ok(t) = parakeet.transcribe(&silence, false) {
                                    if !t.trim().is_empty() {
                                        text_buf.push_str(&t);
                                    }
                                }
                            }
                            flush(&mut text_buf, source, &transcript_tx);
                            parakeet = ParakeetEOU::from_shared(&handle);
                            warmed_up = false;
                            debug!("[{label}] reset done, warming up again");
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        debug!("[{label}] recv timeout — flushing and resetting model");
                        let silence = vec![0.0f32; 2560];
                        for _ in 0..3 {
                            if let Ok(t) = parakeet.transcribe(&silence, false) {
                                if !t.trim().is_empty() {
                                    text_buf.push_str(&t);
                                }
                            }
                        }
                        flush(&mut text_buf, source, &transcript_tx);
                        parakeet = ParakeetEOU::from_shared(&handle);
                        warmed_up = false;
                        empty_streak = 0;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }

            flush(&mut text_buf, source, &transcript_tx);
        })
        .expect("failed to spawn parakeet thread");

    tx
}

/// Load the model and spawn mic + sys inference threads.
pub(crate) async fn spawn(
    model_dir: &str,
    transcript_tx: UnboundedSender<Transcript>,
) -> Result<(Sender<Vec<f32>>, Sender<Vec<f32>>), SplitStreamError> {
    let md = model_dir.to_string();
    let handle = tokio::task::spawn_blocking(move || {
        let cfg = ExecutionConfig::new().with_intra_threads(4);
        ParakeetEOUHandle::load(&md, Some(cfg))
            .map_err(|e| SplitStreamError::ModelLoad(e.to_string()))
    })
    .await
    .map_err(|e| SplitStreamError::ModelLoad(e.to_string()))??;

    let mic_tx = spawn_channel(AudioSource::Mic, handle.clone(), transcript_tx.clone());
    let sys_tx = spawn_channel(AudioSource::Sys, handle, transcript_tx);
    Ok((mic_tx, sys_tx))
}
