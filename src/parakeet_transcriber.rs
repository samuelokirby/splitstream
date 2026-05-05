//! ParakeetEOU streaming transcription via parakeet-rs.
//!
//! `ParakeetEOU::transcribe` expects 160ms chunks (2560 samples at 16kHz).
//! The model maintains a 4-second rolling audio buffer internally and needs
//! ~1 second (≈ 6.25 chunks) of warmup before producing output.
//!
//! Two independent handles load separate ONNX sessions so mic and sys
//! inference run concurrently without lock contention.

use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use log::debug;
use parakeet_rs::{ExecutionConfig, ParakeetEOU, ParakeetEOUHandle};

// Print buffered text after this many consecutive empty iterations (with text
// waiting). 3 × 160ms ≈ 480ms — catches end-of-phrase pauses quickly.
const FLUSH_SILENCE_CHUNKS: usize = 3;

// Print after this many words even during continuous speech, so long sentences
// don't pile up in the buffer.
const MAX_WORDS_BEFORE_FLUSH: usize = 8;

// Hard-reset the model stream state after this many consecutive empty
// iterations. Must be > warmup period (~6.25 chunks) so the reset never fires
// during warmup. 12 × 160ms = 1.92s comfortably clears warmup and natural
// between-sentence pauses.
const RESET_SILENCE_CHUNKS: usize = 12;

fn ends_sentence(text: &str) -> bool {
    matches!(text.trim_end().chars().last(), Some('.' | '?' | '!'))
}

/// Replace SentencePiece word-boundary markers with spaces.
fn fix_sentencepiece(text: &str) -> String {
    text.replace('\u{2581}', " ")
}

fn flush(buf: &mut String, label: &'static str) {
    let text = fix_sentencepiece(buf).replace("[EOU]", "").trim().to_string();
    if !text.is_empty() {
        println!("{} {}", label, text);
    }
    buf.clear();
}

fn spawn_channel(label: &'static str, handle: ParakeetEOUHandle) -> Sender<Vec<f32>> {
    let (tx, rx) = channel::<Vec<f32>>();

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
                        // Collect ALL queued chunks and process every one in
                        // order. Never skip — the model's internal 4s rolling
                        // buffer assumes a contiguous audio stream; any gap
                        // corrupts it and causes drift.
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

                        // --- Print threshold (fast) ---
                        // Flush to screen after a sentence ends, enough words
                        // accumulate, or a short pause with buffered text.
                        if !text_buf.is_empty() {
                            let decoded = fix_sentencepiece(&text_buf).replace("[EOU]", "");
                            let words = decoded.split_whitespace().count();
                            if ends_sentence(&decoded)
                                || words >= MAX_WORDS_BEFORE_FLUSH
                                || empty_streak >= FLUSH_SILENCE_CHUNKS
                            {
                                flush(&mut text_buf, label);
                                empty_streak = 0;
                            }
                        }

                        // --- Reset threshold (slow, warmup-gated) ---
                        // Hard-reset stream state once per silence event, but
                        // only after warmup has completed. empty_streak is NOT
                        // reset here — it stays above RESET_SILENCE_CHUNKS so
                        // this fires exactly once (== not >=). Speech resuming
                        // sets got_text=true which resets it to 0.
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
                            flush(&mut text_buf, label);
                            parakeet = ParakeetEOU::from_shared(&handle);
                            warmed_up = false;
                            debug!("[{label}] reset done, warming up again");
                            // empty_streak intentionally left > RESET_SILENCE_CHUNKS
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        // No chunks for 2s — audio capture likely stopped.
                        debug!("[{label}] recv timeout — flushing and resetting model");
                        let silence = vec![0.0f32; 2560];
                        for _ in 0..3 {
                            if let Ok(t) = parakeet.transcribe(&silence, false) {
                                if !t.trim().is_empty() {
                                    text_buf.push_str(&t);
                                }
                            }
                        }
                        flush(&mut text_buf, label);
                        parakeet = ParakeetEOU::from_shared(&handle);
                        warmed_up = false;
                        empty_streak = 0;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }

            flush(&mut text_buf, label);
        })
        .expect("failed to spawn parakeet thread");

    tx
}

pub async fn spawn(model_dir: &str) -> (Sender<Vec<f32>>, Sender<Vec<f32>>) {
    let md = model_dir.to_string();

    // Load ONE handle and clone it for both channels. Each channel gets its
    // own ParakeetEOU instance (independent LSTM state, audio buffer, warmup
    // tracking) but they share the underlying Arc<Mutex<OnnxSessions>>.
    //
    // Why shared rather than two independent handles:
    // ort 2.0.0-rc.12 serialises concurrent session.run() calls through an
    // ORT-global environment lock. Two independent handles both hit that lock,
    // so when sys inference holds it, mic inference blocks completely — mic
    // appears to stop whenever system audio plays.
    //
    // With one shared handle the Arc<Mutex<>> makes the serialisation
    // explicit and bounded: each inference call is ~5–10 ms, chunks arrive
    // every 160 ms, so both channels process their audio in time.
    // Use 4 intra-op threads (default) since only one channel runs at a time.
    let handle = tokio::task::spawn_blocking(move || {
        let cfg = ExecutionConfig::new().with_intra_threads(4);
        ParakeetEOUHandle::load(&md, Some(cfg)).expect("failed to load Parakeet model")
    })
    .await
    .expect("model load thread panicked");

    let mic_tx = spawn_channel("🎙️", handle.clone());
    let sys_tx = spawn_channel("🔊", handle);
    (mic_tx, sys_tx)
}
