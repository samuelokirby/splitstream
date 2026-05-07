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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_from_results_message() {
        let json = r#"{
            "type": "Results",
            "channel_index": [0, 2],
            "is_final": true,
            "channel": {
                "alternatives": [{"transcript": "hello world", "confidence": 0.99}]
            }
        }"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "Results");
        assert!(msg.is_final);
        assert_eq!(msg.transcript(), Some("hello world"));
        assert_eq!(msg.channel_num(), Some(0));
    }

    #[test]
    fn transcript_returns_none_without_channel() {
        let json = r#"{"type": "Results", "is_final": false}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert!(msg.transcript().is_none());
        assert!(msg.channel_num().is_none());
    }

    #[test]
    fn transcript_returns_none_for_empty_alternatives() {
        let json = r#"{
            "type": "Results",
            "channel_index": [1, 2],
            "is_final": true,
            "channel": {"alternatives": []}
        }"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert!(msg.transcript().is_none());
        assert_eq!(msg.channel_num(), Some(1));
    }

    #[test]
    fn channel_num_returns_none_for_empty_index() {
        let json = r#"{"type": "Results", "channel_index": [], "is_final": false}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert!(msg.channel_num().is_none());
    }

    #[test]
    fn non_results_message_has_no_channel() {
        let json = r#"{"type": "Metadata", "is_final": false}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "Metadata");
        assert!(msg.channel.is_none());
    }
}
