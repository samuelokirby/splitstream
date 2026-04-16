use config::Config;
use serde::Deserialize;

fn default_transcription_backend() -> String {
    "deepgram".to_string()
}

fn default_whisper_window_seconds() -> u32 {
    4
}

/// Settings represents the application settings loaded from settings.toml
#[derive(Deserialize)]
pub struct Settings {
    pub compliance_mode_on_start: bool,
    pub echo_cancellation: bool,
    /// "deepgram" or "whisper"
    #[serde(default = "default_transcription_backend")]
    pub transcription_backend: String,
    #[serde(default)]
    pub whisper_model_path: String,
    #[serde(default)]
    pub whisper_vad_model_path: String,
    #[serde(default = "default_whisper_window_seconds")]
    pub whisper_window_seconds: u32,
}

impl Settings {
    /// Load settings from settings.toml by using config's builder pattern and deserializing into the struct
    pub fn new() -> Self {
        let settings = Config::builder()
            .add_source(config::File::with_name("./settings.toml"))
            .build()
            .unwrap();

        // Deserialize the entire config into the struct
        settings.try_deserialize().unwrap()
    }
}
