//! Deepgram streaming transcription backend.
//!
//! Connects to the Deepgram WebSocket API, streams stereo Opus audio, and
//! forwards parsed transcript events to the caller's `UnboundedSender<Transcript>`.

use futures_util::{SinkExt, StreamExt};
use http::header::AUTHORIZATION;
use log::info;
use opus::Encoder;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, http::HeaderValue},
};
use tungstenite::client::IntoClientRequest;

use crate::transcript::{AudioSource, Transcript};
use crate::transcript_msg::TranscriptMessage;

pub(crate) const OUT_SAMPLE_RATE: u32 = crate::audio::resampler::OUT_SAMPLE_RATE;

pub(crate) struct DeepgramActor {
    pub opus_packet_tx: UnboundedSender<Vec<u8>>,
    pub encoder: Encoder,
}

/// Spawns the WebSocket send/recv tasks and the transcript forwarding task.
/// Returns the actor used by the engine to send Opus packets.
pub(crate) fn spawn(
    api_key: String,
    ws_url: String,
    transcript_tx: UnboundedSender<Transcript>,
) -> DeepgramActor {
    let (opus_packet_tx, opus_packet_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<String>();

    // WebSocket send + receive tasks
    tokio::spawn(transmit(api_key, ws_url, opus_packet_rx, raw_tx));

    // Forward parsed Deepgram events to the caller's transcript channel
    tokio::spawn(forward_transcripts(raw_rx, transcript_tx));

    DeepgramActor {
        opus_packet_tx,
        encoder: build_encoder(OUT_SAMPLE_RATE),
    }
}

async fn transmit(
    api_key: String,
    ws_url: String,
    mut audio_rx: UnboundedReceiver<Vec<u8>>,
    raw_tx: UnboundedSender<String>,
) {
    let mut request = ws_url.into_client_request().unwrap();
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Token {}", api_key)).unwrap(),
    );

    let (ws_stream, _) = match connect_async(request).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Deepgram WebSocket connection failed: {e}");
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
                eprintln!("Deepgram WS send error: {e}");
                break;
            }
        }
        // Audio channel closed (shutdown) — send a graceful WebSocket close frame
        // so Deepgram doesn't time out and log an error on their end.
        let _ = ws_sender.send(Message::Close(None)).await;
    });

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
                    eprintln!("Deepgram WS receive error: {e}");
                    break;
                }
            }
        }
    });

    let _ = tokio::try_join!(send_task, recv_task);
}

async fn forward_transcripts(
    mut raw_rx: UnboundedReceiver<String>,
    tx: UnboundedSender<Transcript>,
) {
    while let Some(raw) = raw_rx.recv().await {
        let v: TranscriptMessage = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Deepgram parse error: {e} — raw: {raw}");
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

        let _ = tx.send(Transcript { source, text: text.to_string(), is_final: v.is_final });
    }
}

pub(crate) fn build_encoder(sample_rate: u32) -> Encoder {
    let mut enc = Encoder::new(sample_rate, opus::Channels::Stereo, opus::Application::LowDelay)
        .expect("failed to create Opus encoder");
    let _ = enc.set_bitrate(opus::Bitrate::Bits(32_000));
    enc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::resampler::FINAL_FRAME_SIZE;
    use opus::Decoder;

    #[test]
    fn test_encoder_create_and_encode_silence() {
        let mut enc = build_encoder(OUT_SAMPLE_RATE);
        let mut packet = vec![0u8; 400];
        let interleaved = vec![0.0_f32; FINAL_FRAME_SIZE * 2];
        let len = enc.encode_float(&interleaved, &mut packet).expect("encode ok");
        assert!(len > 0);
        packet.truncate(len);

        let mut dec = Decoder::new(16_000, opus::Channels::Stereo).expect("decoder ok");
        let mut pcm = vec![0i16; FINAL_FRAME_SIZE * 2];
        let samples = dec.decode(&packet, &mut pcm, false).expect("decode ok");
        assert_eq!(samples, FINAL_FRAME_SIZE);
    }
}
