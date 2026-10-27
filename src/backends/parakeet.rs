//! ParakeetEOU streaming transcription backend via parakeet-rs.
//!
//! `ParakeetEOU::transcribe` expects 160ms chunks (2560 samples at 16kHz).
//! The model maintains a 4-second rolling audio buffer internally and needs
//! ~1 second (≈ 6.25 chunks) of warmup before producing output.
//!
//! Two inference threads share one `ParakeetEOUHandle` (an `Arc<Mutex<_>>` over
//! the ONNX sessions)
//! so mic and sys inference serialize through one lock rather than both hitting
//! the ORT global environment lock independently.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::Duration;

use crate::error::{ErrorReporter, SplitStreamError};
use crate::transcript::{AudioSource, Transcript, samples_to_duration};
use log::{debug, error};
use parakeet_rs::{ExecutionConfig, ParakeetEOU, ParakeetEOUHandle};

/// Sender half of the public transcript channel (bounded, drop-oldest).
type TranscriptTx = tokio::sync::broadcast::Sender<Transcript>;

const BACKEND: &str = "parakeet";

// Print buffered text after this many consecutive empty iterations (with text
// waiting). 3 × 160ms ≈ 480ms — catches end-of-phrase pauses quickly.
const FLUSH_SILENCE_CHUNKS: usize = 3;

// Print after this many words even during continuous speech.
const MAX_WORDS_BEFORE_FLUSH: usize = 8;

// Hard-reset the model stream state after this many consecutive empty
// iterations. Must be > warmup period (~6.25 chunks). 12 × 160ms = 1.92s.
const RESET_SILENCE_CHUNKS: usize = 12;

// A reset before the model has warmed up restarts warmup, which is a reset
// loop, so the silence flush has to fire well before the reset — and the reset
// has to outlast the ~1s (6.25-chunk) warmup. Checked at compile time because
// these are the kind of constants somebody tunes without re-reading the
// comment above them.
const _: () = assert!(FLUSH_SILENCE_CHUNKS < RESET_SILENCE_CHUNKS);
const _: () = assert!(RESET_SILENCE_CHUNKS > 7);
const _: () = assert!(MAX_WORDS_BEFORE_FLUSH > 0);

fn ends_sentence(text: &str) -> bool {
    matches!(text.trim_end().chars().last(), Some('.' | '?' | '!'))
}

fn fix_sentencepiece(text: &str) -> String {
    text.replace('\u{2581}', " ")
}

/// Emit whatever is buffered as one segment spanning `start..end`.
///
/// `start` is the stream offset of the chunk that produced the first token of
/// this segment; `end` is the offset of the last chunk fed to the model. Both
/// come from the thread's own sample count, which tracks the stream exactly:
/// the engine sends every backend one 320-sample frame per 20ms tick, zero
/// filled when the source is silent.
fn flush(
    buf: &mut String,
    source: AudioSource,
    tx: &TranscriptTx,
    start: &mut Option<Duration>,
    end: Duration,
) {
    let text = fix_sentencepiece(buf)
        .replace("[EOU]", "")
        .trim()
        .to_string();
    let seg_start = start.take().unwrap_or(end);
    if !text.is_empty() {
        let _ = tx.send(Transcript::new(source, text, true, seg_start, end, None));
    }
    buf.clear();
}

fn spawn_channel(
    source: AudioSource,
    handle: ParakeetEOUHandle,
    transcript_tx: TranscriptTx,
    errors: ErrorReporter,
) -> Result<Sender<Vec<f32>>, SplitStreamError> {
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
            // Stream position, in 16kHz samples consumed by this thread.
            let mut samples_seen: u64 = 0;
            let mut seg_start: Option<Duration> = None;
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
                            let chunk_start = samples_to_duration(samples_seen);
                            samples_seen += c.len() as u64;
                            match parakeet.transcribe(c, false) {
                                Ok(text) if !text.trim().is_empty() => {
                                    if text_buf.is_empty() {
                                        seg_start = Some(chunk_start);
                                    }
                                    text_buf.push_str(&text);
                                    got_text = true;
                                }
                                Ok(_) => {}
                                Err(e) => error!("Parakeet inference error ({label}): {e}"),
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
                                flush(
                                    &mut text_buf,
                                    source,
                                    &transcript_tx,
                                    &mut seg_start,
                                    samples_to_duration(samples_seen),
                                );
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
                                if let Ok(t) = parakeet.transcribe(&silence, false)
                                    && !t.trim().is_empty()
                                {
                                    text_buf.push_str(&t);
                                }
                            }
                            flush(
                                &mut text_buf,
                                source,
                                &transcript_tx,
                                &mut seg_start,
                                samples_to_duration(samples_seen),
                            );
                            parakeet = ParakeetEOU::from_shared(&handle);
                            warmed_up = false;
                            debug!("[{label}] reset done, warming up again");
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        debug!("[{label}] recv timeout — flushing and resetting model");
                        let silence = vec![0.0f32; 2560];
                        for _ in 0..3 {
                            if let Ok(t) = parakeet.transcribe(&silence, false)
                                && !t.trim().is_empty()
                            {
                                text_buf.push_str(&t);
                            }
                        }
                        flush(
                            &mut text_buf,
                            source,
                            &transcript_tx,
                            &mut seg_start,
                            samples_to_duration(samples_seen),
                        );
                        parakeet = ParakeetEOU::from_shared(&handle);
                        warmed_up = false;
                        empty_streak = 0;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }

            flush(
                &mut text_buf,
                source,
                &transcript_tx,
                &mut seg_start,
                samples_to_duration(samples_seen),
            );
            // Only reached when the engine drops its sender (shutdown) — or if
            // the engine task died, which the consumer should hear about.
            errors.report_expected_stop(SplitStreamError::BackendStopped {
                backend: BACKEND,
                reason: format!("{label} inference thread exited"),
            });
        })
        .map_err(|e| SplitStreamError::BackendStartFailed {
            backend: BACKEND,
            source: Box::new(e),
        })?;

    Ok(tx)
}

/// Load the model and spawn mic + sys inference threads.
pub(crate) async fn spawn(
    model_dir: &Path,
    transcript_tx: TranscriptTx,
    errors: ErrorReporter,
) -> Result<(Sender<Vec<f32>>, Sender<Vec<f32>>), SplitStreamError> {
    let path: PathBuf = model_dir.to_path_buf();
    if !path.exists() {
        return Err(SplitStreamError::ModelNotFound { path });
    }

    let load_path = path.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let cfg = ExecutionConfig::new().with_intra_threads(4);
        ParakeetEOUHandle::load(&load_path, Some(cfg)).map_err(|e| SplitStreamError::ModelLoad {
            path: load_path.clone(),
            source: Box::new(e),
        })
    })
    .await
    .map_err(|e| SplitStreamError::BackendStartFailed {
        backend: BACKEND,
        source: Box::new(e),
    })??;

    let mic_tx = spawn_channel(
        AudioSource::Mic,
        handle.clone(),
        transcript_tx.clone(),
        errors.clone(),
    )?;
    let sys_tx = spawn_channel(AudioSource::Sys, handle, transcript_tx, errors)?;
    Ok((mic_tx, sys_tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- sentence boundary detection (drives the flush decision) ---

    #[test]
    fn terminal_punctuation_ends_a_sentence() {
        assert!(ends_sentence("hello."));
        assert!(ends_sentence("hello?"));
        assert!(ends_sentence("hello!"));
    }

    #[test]
    fn trailing_whitespace_does_not_hide_the_punctuation() {
        // Parakeet emits SentencePiece boundary markers that become trailing
        // spaces, so the check has to look past them.
        assert!(ends_sentence("hello.  "));
        assert!(ends_sentence("hello!\n"));
    }

    #[test]
    fn non_terminal_text_does_not_end_a_sentence() {
        assert!(!ends_sentence("hello"));
        assert!(!ends_sentence("hello,"));
        assert!(!ends_sentence("mid-sentence "));
    }

    #[test]
    fn empty_and_whitespace_only_text_never_ends_a_sentence() {
        assert!(!ends_sentence(""));
        assert!(!ends_sentence("   "));
    }

    // --- SentencePiece marker handling ---

    #[test]
    fn word_boundary_markers_become_spaces() {
        assert_eq!(
            fix_sentencepiece("\u{2581}hello\u{2581}world"),
            " hello world"
        );
    }

    #[test]
    fn text_without_markers_is_unchanged() {
        assert_eq!(fix_sentencepiece("hello world"), "hello world");
        assert_eq!(fix_sentencepiece(""), "");
    }

    #[test]
    fn markers_are_replaced_everywhere_not_just_once() {
        let raw = "\u{2581}a\u{2581}b\u{2581}c";
        assert_eq!(fix_sentencepiece(raw), " a b c");
        assert!(!fix_sentencepiece(raw).contains('\u{2581}'));
    }

    #[test]
    fn multibyte_text_survives_the_replacement() {
        // The marker is 3 bytes in UTF-8; a byte-wise replacement would split
        // neighbouring multibyte characters.
        assert_eq!(
            fix_sentencepiece("\u{2581}café\u{2581}naïve"),
            " café naïve"
        );
    }

    // --- flush ---

    #[test]
    fn flush_emits_a_cleaned_segment_with_its_span() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<Transcript>(8);
        let mut buf = String::from("\u{2581}hello\u{2581}world[EOU]");
        let mut start = Some(Duration::from_millis(500));

        flush(
            &mut buf,
            AudioSource::Mic,
            &tx,
            &mut start,
            Duration::from_millis(1_300),
        );

        let t = rx.try_recv().expect("a transcript was sent");
        assert_eq!(t.text, "hello world");
        assert!(t.is_final);
        assert_eq!(t.start, Duration::from_millis(500));
        assert_eq!(t.end, Duration::from_millis(1_300));
        assert!(buf.is_empty(), "the buffer must be cleared");
        assert!(start.is_none(), "the segment start must be consumed");
    }

    #[test]
    fn flush_of_an_empty_buffer_sends_nothing() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<Transcript>(8);
        let mut buf = String::from("   [EOU] ");
        let mut start = None;
        flush(
            &mut buf,
            AudioSource::Sys,
            &tx,
            &mut start,
            Duration::from_secs(1),
        );
        assert!(rx.try_recv().is_err());
        assert!(buf.is_empty());
    }

    #[test]
    fn flush_without_a_recorded_start_collapses_the_span() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<Transcript>(8);
        let mut buf = String::from("hi");
        let mut start = None;
        let end = Duration::from_millis(800);
        flush(&mut buf, AudioSource::Sys, &tx, &mut start, end);
        let t = rx.try_recv().unwrap();
        assert_eq!(t.start, end);
        assert_eq!(t.end, end);
    }
}
