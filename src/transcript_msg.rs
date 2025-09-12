// src/transcript_msg.rs
//! TranscriptMessage represents a message from the transcription service.
use serde::Deserialize;
#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Microphone,
    System,
}

#[derive(Debug, Deserialize)]
pub struct TranscriptMessage {
    pub text: String,     // transcribed text
    pub channel: Channel, // channel, either "mic" or "system"
    #[serde(rename = "final")]
    pub is_final: bool, // whether the transcription is interim or final
}
