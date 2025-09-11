use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct TranscriptMessage {
    pub text: String,
    pub channel: String,
    #[serde(rename = "final")]
    pub is_final: bool,
}
