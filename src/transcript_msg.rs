// src/transcript_msg.rs
//! Deepgram streaming transcription response types.
//! Reference: <https://developers.deepgram.com/reference/listen-live>
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Alternative {
    pub transcript: String,
    #[serde(default)]
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
    /// Offset in seconds of this segment from the start of the stream.
    #[serde(default)]
    pub start: f64,
    /// Length of this segment in seconds.
    #[serde(default)]
    pub duration: f64,
    /// Present (and an object) only on `Results` frames — see
    /// [`lenient_channel`].
    #[serde(default, deserialize_with = "lenient_channel")]
    pub channel: Option<Channel>,
}

/// Deepgram overloads the `channel` key across message types: `Results` frames
/// carry an object with `alternatives`, while `SpeechStarted` and
/// `UtteranceEnd` carry a `[channel_number, total_channels]` **array**.
///
/// Deserializing it strictly makes every lifecycle frame fail to parse, which
/// the forwarding task logs as `Deepgram parse error` once per utterance.
/// Anything that is not the transcript shape is simply "no channel".
fn lenient_channel<'de, D>(deserializer: D) -> Result<Option<Channel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<Channel>(value).ok())
}

impl TranscriptMessage {
    /// Returns the top alternative transcript text, if present.
    pub fn transcript(&self) -> Option<&str> {
        self.channel
            .as_ref()
            .and_then(|c| c.alternatives.first())
            .map(|a| a.transcript.as_str())
    }

    /// Returns the top alternative's confidence, clamped to `0.0..=1.0`.
    pub fn confidence(&self) -> Option<f32> {
        self.channel
            .as_ref()
            .and_then(|c| c.alternatives.first())
            .map(|a| (a.confidence as f32).clamp(0.0, 1.0))
    }

    /// Start and end offsets of this segment from the start of the stream.
    ///
    /// Deepgram's numbers are taken as untrusted input: `Duration::from_secs_f64`
    /// panics on a negative, NaN, or overflowing value, and this runs on a task
    /// fed straight from the network. Anything unrepresentable collapses to a
    /// zero-length span at the origin rather than taking the process down.
    pub fn span(&self) -> (std::time::Duration, std::time::Duration) {
        let start = seconds_to_duration(self.start);
        let end = start.saturating_add(seconds_to_duration(self.duration));
        (start, end)
    }

    /// Returns the 0-based channel number (0 = mic, 1 = sys).
    pub fn channel_num(&self) -> Option<u32> {
        self.channel_index
            .as_ref()
            .and_then(|ci| ci.first().copied())
    }
}

/// Convert a JSON-supplied offset in seconds to a `Duration`, mapping every
/// value `Duration` cannot represent — negative, NaN, infinite, or beyond
/// `u64::MAX` seconds — onto zero.
fn seconds_to_duration(seconds: f64) -> std::time::Duration {
    std::time::Duration::try_from_secs_f64(seconds).unwrap_or(std::time::Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
        assert_eq!(msg.confidence(), Some(0.99));
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

    // -----------------------------------------------------------------
    // Error frames
    //
    // Deepgram sends these instead of a Results payload when the request is
    // rejected. They have no `channel`, so every accessor must degrade
    // rather than unwrap.
    // -----------------------------------------------------------------

    #[test]
    fn error_frame_parses_and_yields_no_transcript() {
        let json = r#"{
            "type": "Error",
            "description": "Deepgram did not receive audio data or a text message within the timeout window.",
            "message": "NET-0001",
            "variant": "NET-0001"
        }"#;
        let msg: TranscriptMessage = serde_json::from_str(json).expect("error frames must parse");
        assert_eq!(msg.msg_type, "Error");
        assert!(msg.transcript().is_none());
        assert!(msg.confidence().is_none());
        assert!(msg.channel_num().is_none());
        assert!(!msg.is_final);
        assert_eq!(msg.span(), (Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn unauthorized_error_frame_parses() {
        let json = r#"{"type":"Error","description":"Unauthorized","message":"401"}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "Error");
        assert!(msg.transcript().is_none());
    }

    #[test]
    fn a_frame_with_an_explicit_null_channel_yields_no_transcript() {
        let json = r#"{"type": "Error", "channel": null, "channel_index": null}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert!(msg.transcript().is_none());
        assert!(msg.channel_num().is_none());
    }

    #[test]
    fn a_frame_without_a_type_is_rejected_rather_than_defaulted() {
        // `msg_type` drives the "is this a transcript?" decision, so silently
        // defaulting it to "" would be worse than a parse error the caller logs.
        let json = r#"{"is_final": true}"#;
        assert!(serde_json::from_str::<TranscriptMessage>(json).is_err());
    }

    // -----------------------------------------------------------------
    // Close / lifecycle frames
    // -----------------------------------------------------------------

    #[test]
    fn close_and_lifecycle_frames_parse_with_no_transcript() {
        for json in [
            r#"{"type": "Close"}"#,
            r#"{"type": "SpeechStarted", "channel": [0, 2], "timestamp": 3.5}"#,
            r#"{"type": "UtteranceEnd", "channel": [0, 2], "last_word_end": 4.25}"#,
            r#"{"type": "Finalize"}"#,
        ] {
            let msg: TranscriptMessage =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{json}: {e}"));
            assert!(msg.transcript().is_none(), "{json}");
        }
    }

    #[test]
    fn an_array_shaped_channel_key_does_not_break_parsing() {
        // `SpeechStarted` reuses `channel` for a `[number, total]` pair. A
        // strict `Option<Channel>` rejects the whole frame, which the
        // forwarding task then logs as a parse error once per utterance.
        let json = r#"{"type": "SpeechStarted", "channel": [0, 2], "timestamp": 3.5}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).expect("must parse");
        assert_eq!(msg.msg_type, "SpeechStarted");
        assert!(msg.channel.is_none());
        assert!(msg.transcript().is_none());
    }

    #[test]
    fn the_final_metadata_frame_parses() {
        // Sent right before the socket closes; the display task ignores it,
        // but it must not fail to parse and get logged as an error.
        let json = r#"{
            "type": "Metadata",
            "transaction_key": "deprecated",
            "request_id": "1234",
            "sha256": "abc",
            "created": "2026-01-01T00:00:00.000Z",
            "duration": 12.5,
            "channels": 2,
            "models": ["nova-3"]
        }"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "Metadata");
        assert!(msg.transcript().is_none());
    }

    // -----------------------------------------------------------------
    // Timing — `Duration::from_secs_f64` panics on values JSON can carry.
    // -----------------------------------------------------------------

    #[test]
    fn span_is_start_plus_duration() {
        let json = r#"{"type": "Results", "start": 1.5, "duration": 0.75}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            msg.span(),
            (Duration::from_millis(1_500), Duration::from_millis(2_250))
        );
    }

    #[test]
    fn negative_offsets_collapse_to_zero_instead_of_panicking() {
        let json = r#"{"type": "Results", "start": -1.0, "duration": -2.0}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.span(), (Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn absurd_offsets_collapse_to_zero_instead_of_panicking() {
        // Far beyond `u64::MAX` seconds: `Duration::from_secs_f64` would abort
        // the forwarding task on a payload the process does not control.
        let json = r#"{"type": "Results", "start": 1e30, "duration": 1e30}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.span(), (Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn non_finite_seconds_collapse_to_zero() {
        assert_eq!(seconds_to_duration(f64::NAN), Duration::ZERO);
        assert_eq!(seconds_to_duration(f64::INFINITY), Duration::ZERO);
        assert_eq!(seconds_to_duration(f64::NEG_INFINITY), Duration::ZERO);
        assert_eq!(seconds_to_duration(-0.0), Duration::ZERO);
    }

    #[test]
    fn a_missing_span_defaults_to_zero() {
        let json = r#"{"type": "Results", "channel": {"alternatives": []}}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.span(), (Duration::ZERO, Duration::ZERO));
    }

    // -----------------------------------------------------------------
    // Confidence
    // -----------------------------------------------------------------

    #[test]
    fn confidence_is_clamped_into_zero_to_one() {
        let over = r#"{"type":"Results","channel":{"alternatives":[{"transcript":"a","confidence":1.7}]}}"#;
        let under = r#"{"type":"Results","channel":{"alternatives":[{"transcript":"a","confidence":-0.4}]}}"#;
        let over: TranscriptMessage = serde_json::from_str(over).unwrap();
        let under: TranscriptMessage = serde_json::from_str(under).unwrap();
        assert_eq!(over.confidence(), Some(1.0));
        assert_eq!(under.confidence(), Some(0.0));
    }

    #[test]
    fn a_missing_confidence_defaults_to_zero_not_a_parse_error() {
        let json = r#"{"type":"Results","channel":{"alternatives":[{"transcript":"a"}]}}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.transcript(), Some("a"));
        assert_eq!(msg.confidence(), Some(0.0));
    }

    #[test]
    fn the_first_alternative_wins() {
        let json = r#"{"type":"Results","channel":{"alternatives":[
            {"transcript":"best","confidence":0.9},
            {"transcript":"worse","confidence":0.1}
        ]}}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.transcript(), Some("best"));
        assert_eq!(msg.confidence(), Some(0.9));
    }
}
