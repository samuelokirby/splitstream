// src/transcript_msg.rs
//! Deepgram streaming transcription response types.
//! Reference: https://developers.deepgram.com/reference/listen-live
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Alternative {
    pub transcript: String,
    pub confidence: f64,
}

/// Deepgram channel object — contains one or more transcript alternatives.
#[derive(Debug, Deserialize)]
pub struct Channel {
    pub alternatives: Vec<Alternative>,
}

/// Top-level Deepgram streaming message.
/// Only "Results" messages carry transcripts; others (Metadata, SpeechStarted,
/// UtteranceEnd) are silently ignored by the display task.
#[derive(Debug, Deserialize)]
pub struct TranscriptMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// [channel_number, total_channels] — channel 0 = mic, channel 1 = sys
    pub channel_index: Option<Vec<u32>>,
    #[serde(default)]
    pub is_final: bool,
    pub channel: Option<Channel>,
}

impl TranscriptMessage {
    /// Returns the top alternative transcript text, if present.
    pub fn transcript(&self) -> Option<&str> {
        self.channel
            .as_ref()
            .and_then(|c| c.alternatives.first())
            .map(|a| a.transcript.as_str())
    }

    /// Returns the 0-based channel number (0 = mic, 1 = sys).
    pub fn channel_num(&self) -> Option<u32> {
        self.channel_index.as_ref().and_then(|ci| ci.first().copied())
    }
}
