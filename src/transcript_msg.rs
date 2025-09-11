// src/transcript_msg.rs
//! TranscriptMessage represents a message from the transcription service.
use serde::Deserialize;
#[derive(Debug, Deserialize)]
pub struct TranscriptMessage {
    pub text: String,    // transcribed text
    pub channel: String, // channel, either "mic" or "system"
    #[serde(rename = "final")]
    pub is_final: bool, // whether the transcription is interim or final
}
