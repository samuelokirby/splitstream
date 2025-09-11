// src/websocket_client.rs
//! WebSocketClient handles the WebSocket connection to the transcription service.

use futures_util::{SinkExt, StreamExt};
use http::header::AUTHORIZATION;
use log::info;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, http::HeaderValue},
};
use tungstenite::client::IntoClientRequest;

pub struct WebSocketClient {
    access_token: String, // Bearer token for authentication
    ws_url: String,       // WebSocket URL of the transcription service
}

impl WebSocketClient {
    /// Constructor that saves the access token and WebSocket URL for later use in transmit_audio_frames.
    pub fn new(access_token: String, ws_url: String) -> Self {
        Self {
            access_token,
            ws_url,
        }
    }

    /// Main method to transmit audio frames and receive transcripts.
    /// It takes an audio frame receiver and a transcript sender channel.
    /// It spawns two async tasks: one for sending audio frames and another for receiving transcripts.
    /// Received transcripts are forwarded to the provided UnboundedSender<String> back to main.
    ///
    /// Arguments:
    /// * `audio_receiver`: UnboundedReceiver<Vec<u8>> that provides Opus encoded audio frames
    /// * `transcript_tx`: UnboundedSender<String> to send received transcripts back to main
    /// Returns:
    /// * Result<(), String>: Ok on success, Err with error message on failure
    pub async fn transmit_audio_frames(
        &self,
        mut audio_receiver: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        transcript_tx: UnboundedSender<String>,
    ) -> Result<(), String> {
        // Establish a WS(S) connection with authentication
        let request = self.build_wss_request().unwrap();
        let (ws_stream, _) = connect_async(request)
            .await
            .map_err(|e| format!("Websocket connection failed: {}", e))?;

        // ws_sender and ws_receiver are split halves of the WebSocket stream
        // ws_sender is used to send messages to the server
        // ws_receiver is used to receive messages from the server
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        // Task A: First, we spawn a new thread to "pump" outgoing audio frames
        // Pumping means we continuously read from the audio_receiver channel
        // and send each frame as a binary message over the WebSocket connection.
        let send_task = tokio::spawn(async move {
            while let Some(encoded_frame) = audio_receiver.recv().await {
                if encoded_frame.is_empty() {
                    continue;
                }
                if let Err(e) = ws_sender.send(Message::Binary(encoded_frame.into())).await {
                    panic!("Failed to transmit audio frame over websocket: {e}");
                }
            }
        });

        // Task B: Next, we spawn another thread to handle incoming messages
        // from the WebSocket connection. This thread continuously reads messages
        // from ws_receiver. When a text or binary message is received, it is
        // forwarded to the transcript_tx channel back to main.
        let recv_task = tokio::spawn(async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    // If we receive Utf8Bytes in text, convert to String and forward
                    // This is the most common case.
                    Ok(Message::Text(t)) => {
                        info!("Received text message: {}", t);
                        let _ = transcript_tx.send(t.to_string());
                    }
                    // If we receive binary data, turn it into a String if possible and forward
                    Ok(Message::Binary(b)) => {
                        // If your backend sends JSON in binary, try parse it:
                        if let Ok(text) = String::from_utf8(b.to_vec()) {
                            info!("Received binary message: {}", text);
                            let _ = transcript_tx.send(text);
                        }
                    }
                    // Handle close messages by logging and breaking the loop
                    Ok(Message::Close(frame)) => {
                        if let Some(cf) = frame {
                            eprintln!("WS closed: code={}, reason={}", cf.code, cf.reason);
                        }
                        break;
                    }
                    Ok(_) => {} // Ping/Pong/Frame types we don't care about
                    Err(e) => {
                        eprintln!("WS receive error: {e}");
                        break;
                    }
                }
            }
        });

        // Wait for either task to finish before returning
        // TODO: right now, we don't handle errors from either task.
        // This means if one task fails, the other keeps running.
        // In a production system, you'd want to handle this more gracefully.
        let _ = tokio::try_join!(send_task, recv_task);
        Ok(())
    }

    /// Helper method to build an authenticated WebSocket request with the provided access token and URL.
    /// Doing this in Rust is a bit clunky due to the types involved, so we encapsulate it here.
    fn build_wss_request(&self) -> Result<http::Request<()>, http::Error> {
        let mut request = self.ws_url.clone().into_client_request().unwrap();
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.access_token)).unwrap(),
        );
        Ok(request)
    }
}
