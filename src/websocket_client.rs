use futures_util::{SinkExt, StreamExt};
use http::header::AUTHORIZATION;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, http::HeaderValue},
};
use tungstenite::client::IntoClientRequest;

pub struct WebSocketClient {
    access_token: String,
    ws_url: String,
}

impl WebSocketClient {
    pub fn new(access_token: String, ws_url: String) -> Self {
        Self {
            access_token,
            ws_url,
        }
    }
    pub async fn transmit_audio_frames(
        &self,
        mut audio_receiver: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        transcript_tx: UnboundedSender<String>, // <— add this
    ) -> Result<(), String> {
        let request = self.build_wss_request().unwrap();
        let (ws_stream, _) = connect_async(request)
            .await
            .map_err(|e| format!("Websocket connection failed: {}", e))?;

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        // Task A: pump outgoing audio frames
        let send_task = tokio::spawn(async move {
            while let Some(encoded_frame) = audio_receiver.recv().await {
                if encoded_frame.is_empty() {
                    continue;
                }
                if let Err(e) = ws_sender.send(Message::Binary(encoded_frame.into())).await {
                    eprintln!("Failed to send audio frame: {e}");
                    break;
                }
            }
        });

        // Task B: read server messages (transcripts) and forward to channel
        let recv_task = tokio::spawn(async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(t)) => {
                        // If your backend sends raw transcript text:
                        let _ = transcript_tx.send(t.to_string());
                    }
                    Ok(Message::Binary(b)) => {
                        // If your backend sends JSON in binary, try parse it:
                        if let Ok(text) = String::from_utf8(b.to_vec()) {
                            let _ = transcript_tx.send(text);
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        if let Some(cf) = frame {
                            eprintln!("WS closed: code={}, reason={}", cf.code, cf.reason);
                        }
                        break;
                    }
                    Ok(_) => {} // Ping/Pong/Frame types you don't care about
                    Err(e) => {
                        eprintln!("WS receive error: {e}");
                        break;
                    }
                }
            }
        });

        // Wait for either task to finish
        let _ = tokio::try_join!(send_task, recv_task);
        Ok(())
    }

    /// Builds an authenticated WebSocket request with the provided access token and URL.
    fn build_wss_request(&self) -> Result<http::Request<()>, http::Error> {
        let mut request = self.ws_url.clone().into_client_request().unwrap();
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.access_token)).unwrap(),
        );
        Ok(request)
    }
}
