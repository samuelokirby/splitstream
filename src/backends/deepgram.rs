//! Deepgram streaming transcription backend.
//!
//! Connects to the Deepgram WebSocket API, streams stereo Opus audio, and
//! forwards parsed transcript events to the caller's transcript channel.
//!
//! The request (URL + `Authorization` header) is built **before** any task is
//! spawned: both are derived from user-supplied strings, so a key containing a
//! newline or a non-ASCII byte must be a returned error, not a panic on a
//! detached task.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::{SinkExt, StreamExt};
use http::header::AUTHORIZATION;
use log::{error, info, warn};
use opus::Encoder;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, http::HeaderValue},
};
use tungstenite::client::IntoClientRequest;

use crate::error::{ErrorReporter, SplitStreamError};
use crate::transcript::{AudioSource, Transcript};

/// Sender half of the public transcript channel (bounded, drop-oldest).
type TranscriptTx = tokio::sync::broadcast::Sender<Transcript>;
use crate::transcript_msg::TranscriptMessage;

pub(crate) const OUT_SAMPLE_RATE: u32 = crate::audio::resampler::OUT_SAMPLE_RATE;

const BACKEND: &str = "deepgram";

pub(crate) struct DeepgramActor {
    pub opus_packet_tx: UnboundedSender<Vec<u8>>,
    pub encoder: Encoder,
    /// Cleared when the WebSocket task exits for any reason.
    ///
    /// `opus_packet_tx` is unbounded and nothing reads it once that task is
    /// gone, so the engine must stop encoding into it — otherwise a rejected
    /// API key turns into ~50 leaked packets per second for the life of the
    /// process.
    pub alive: Arc<AtomicBool>,
}

/// Validates the endpoint and credentials, then spawns the WebSocket
/// send/recv tasks and the transcript forwarding task.
pub(crate) fn spawn(
    api_key: String,
    ws_url: String,
    transcript_tx: TranscriptTx,
    errors: ErrorReporter,
) -> Result<DeepgramActor, SplitStreamError> {
    // Build the request up front: both of these are fallible on user input.
    let mut request =
        ws_url
            .into_client_request()
            .map_err(|e| SplitStreamError::BackendStartFailed {
                backend: BACKEND,
                source: Box::new(e),
            })?;
    let auth = HeaderValue::from_str(&format!("Token {api_key}")).map_err(|e| {
        SplitStreamError::BackendStartFailed {
            backend: BACKEND,
            source: Box::new(e),
        }
    })?;
    request.headers_mut().insert(AUTHORIZATION, auth);

    let encoder = build_encoder(OUT_SAMPLE_RATE)?;

    let (opus_packet_tx, opus_packet_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<String>();

    let alive = Arc::new(AtomicBool::new(true));

    // WebSocket send + receive tasks
    tokio::spawn(transmit(
        request,
        opus_packet_rx,
        raw_tx,
        Arc::clone(&alive),
        errors,
    ));

    // Forward parsed Deepgram events to the caller's transcript channel
    tokio::spawn(forward_transcripts(raw_rx, transcript_tx));

    Ok(DeepgramActor {
        opus_packet_tx,
        encoder,
        alive,
    })
}

async fn transmit(
    request: tungstenite::handshake::client::Request,
    mut audio_rx: UnboundedReceiver<Vec<u8>>,
    raw_tx: UnboundedSender<String>,
    alive: Arc<AtomicBool>,
    errors: ErrorReporter,
) {
    // Whatever happens below, the engine must learn that this backend is gone.
    struct AliveGuard(Arc<AtomicBool>);
    impl Drop for AliveGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    let _guard = AliveGuard(alive);

    let (ws_stream, _) = match connect_async(request).await {
        Ok(v) => v,
        Err(e) => {
            errors.report(SplitStreamError::BackendStartFailed {
                backend: BACKEND,
                source: Box::new(e),
            });
            return;
        }
    };

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    let send_task = tokio::spawn(async move {
        while let Some(frame) = audio_rx.recv().await {
            if frame.is_empty() {
                continue;
            }
            if let Err(e) = ws_sender.send(Message::Binary(frame.into())).await {
                error!("Deepgram WS send error: {e}");
                break;
            }
        }
        // Audio channel closed (shutdown) — send a graceful WebSocket close frame
        // so Deepgram doesn't time out and log an error on their end.
        let _ = ws_sender.send(Message::Close(None)).await;
    });

    let recv_errors = errors.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(t)) => {
                    info!("Deepgram: {}", t);
                    let _ = raw_tx.send(t.to_string());
                }
                Ok(Message::Binary(b)) => {
                    if let Ok(text) = String::from_utf8(b.to_vec()) {
                        let _ = raw_tx.send(text);
                    }
                }
                Ok(Message::Close(frame)) => {
                    if let Some(cf) = frame {
                        info!("Deepgram WS closed: code={}, reason={}", cf.code, cf.reason);
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    recv_errors.report_expected_stop(SplitStreamError::BackendStopped {
                        backend: BACKEND,
                        reason: format!("websocket receive error: {e}"),
                    });
                    break;
                }
            }
        }
    });

    let _ = tokio::try_join!(send_task, recv_task);
    errors.report_expected_stop(SplitStreamError::BackendStopped {
        backend: BACKEND,
        reason: "websocket session ended".to_string(),
    });
}

async fn forward_transcripts(mut raw_rx: UnboundedReceiver<String>, tx: TranscriptTx) {
    while let Some(raw) = raw_rx.recv().await {
        let v: TranscriptMessage = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn!("Deepgram parse error: {e} — raw: {raw}");
                continue;
            }
        };

        if v.msg_type != "Results" {
            continue;
        }
        let Some(text) = v.transcript() else { continue };
        if text.is_empty() {
            continue;
        }

        let source = match v.channel_num() {
            Some(0) => AudioSource::Mic,
            Some(1) => AudioSource::Sys,
            _ => continue,
        };

        let (start, end) = v.span();
        let _ = tx.send(Transcript::new(
            source,
            text.to_string(),
            v.is_final,
            start,
            end,
            v.confidence(),
        ));
    }
}

pub(crate) fn build_encoder(sample_rate: u32) -> Result<Encoder, SplitStreamError> {
    let mut enc = Encoder::new(
        sample_rate,
        opus::Channels::Stereo,
        opus::Application::LowDelay,
    )
    .map_err(|e| SplitStreamError::BackendStartFailed {
        backend: BACKEND,
        source: Box::new(e),
    })?;
    let _ = enc.set_bitrate(opus::Bitrate::Bits(32_000));
    Ok(enc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::resampler::FINAL_FRAME_SIZE;
    use opus::Decoder;

    #[test]
    fn test_encoder_create_and_encode_silence() {
        let mut enc = build_encoder(OUT_SAMPLE_RATE).expect("encoder ok");
        let mut packet = vec![0u8; 400];
        let interleaved = vec![0.0_f32; FINAL_FRAME_SIZE * 2];
        let len = enc
            .encode_float(&interleaved, &mut packet)
            .expect("encode ok");
        assert!(len > 0);
        packet.truncate(len);

        let mut dec = Decoder::new(16_000, opus::Channels::Stereo).expect("decoder ok");
        let mut pcm = vec![0i16; FINAL_FRAME_SIZE * 2];
        let samples = dec.decode(&packet, &mut pcm, false).expect("decode ok");
        assert_eq!(samples, FINAL_FRAME_SIZE);
    }

    #[test]
    fn build_encoder_rejects_invalid_sample_rate() {
        assert!(build_encoder(12_345).is_err());
    }

    // --- transcript forwarding ---
    //
    // `forward_transcripts` consumes text straight off the socket. Everything
    // it can be handed — malformed JSON, lifecycle frames, unknown channel
    // numbers — must be dropped quietly rather than panic a detached task.

    /// Push `frames` through the forwarding task and collect what reaches the
    /// public transcript channel.
    async fn forwarded(frames: &[&str]) -> Vec<Transcript> {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<String>();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<Transcript>(32);
        for f in frames {
            raw_tx.send((*f).to_string()).unwrap();
        }
        drop(raw_tx);
        forward_transcripts(raw_rx, tx).await;

        let mut out = Vec::new();
        while let Ok(t) = rx.try_recv() {
            out.push(t);
        }
        out
    }

    #[tokio::test]
    async fn forwards_results_frames_with_source_span_and_confidence() {
        let out = forwarded(&[
            r#"{"type":"Results","channel_index":[0,2],"is_final":true,"start":1.0,
                "duration":0.5,"channel":{"alternatives":[{"transcript":"hi","confidence":0.8}]}}"#,
            r#"{"type":"Results","channel_index":[1,2],"is_final":false,"start":2.0,
                "duration":1.0,"channel":{"alternatives":[{"transcript":"there","confidence":0.4}]}}"#,
        ])
        .await;

        assert_eq!(out.len(), 2);
        assert!(matches!(out[0].source, AudioSource::Mic));
        assert_eq!(out[0].text, "hi");
        assert!(out[0].is_final);
        assert_eq!(out[0].start, std::time::Duration::from_secs(1));
        assert_eq!(out[0].end, std::time::Duration::from_millis(1_500));
        assert_eq!(out[0].confidence, Some(0.8));
        assert!(matches!(out[1].source, AudioSource::Sys));
        assert!(!out[1].is_final);
    }

    #[tokio::test]
    async fn hostile_and_lifecycle_frames_are_dropped_without_panicking() {
        let out = forwarded(&[
            "not json at all",
            "",
            "null",
            "[]",
            r#"{"type":"Error","description":"Unauthorized","message":"401"}"#,
            r#"{"type":"Metadata","request_id":"x"}"#,
            r#"{"type":"SpeechStarted","channel":[0,2],"timestamp":1.0}"#,
            r#"{"type":"UtteranceEnd","channel":[0,2],"last_word_end":2.0}"#,
            // Results with nothing usable in it
            r#"{"type":"Results","channel_index":[0,2],"channel":{"alternatives":[]}}"#,
            // Empty transcript text — Deepgram sends these constantly
            r#"{"type":"Results","channel_index":[0,2],"channel":{"alternatives":[{"transcript":""}]}}"#,
            // Channel number the stereo stream cannot produce
            r#"{"type":"Results","channel_index":[7,2],"channel":{"alternatives":[{"transcript":"x"}]}}"#,
            // No channel_index at all
            r#"{"type":"Results","channel":{"alternatives":[{"transcript":"x"}]}}"#,
            // Offsets that would panic `Duration::from_secs_f64`
            r#"{"type":"Results","channel_index":[0,2],"start":-1.0,"duration":1e30,
                "channel":{"alternatives":[{"transcript":"ok"}]}}"#,
        ])
        .await;

        // Only the last frame carries usable text.
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].text, "ok");
        assert_eq!(out[0].start, std::time::Duration::ZERO);
    }

    #[tokio::test]
    async fn forwarding_survives_a_closed_transcript_channel() {
        // Every consumer dropped: `send` fails on each frame and the task must
        // still drain to completion instead of erroring out.
        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<String>();
        let (tx, rx) = tokio::sync::broadcast::channel::<Transcript>(4);
        drop(rx);
        raw_tx
            .send(
                r#"{"type":"Results","channel_index":[0,2],
                    "channel":{"alternatives":[{"transcript":"hi"}]}}"#
                    .to_string(),
            )
            .unwrap();
        drop(raw_tx);
        forward_transcripts(raw_rx, tx).await;
    }
}
