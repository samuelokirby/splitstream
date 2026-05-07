//! Builder for configuring and starting a splitstream session.

use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::handle::{Controls, SplitStreamHandle};
use crate::transcript::Transcript;
use crate::SplitStreamError;

// ---------------------------------------------------------------------------
// Backend configuration types (public)
// ---------------------------------------------------------------------------

/// Configuration for the Deepgram streaming backend.
pub struct DeepgramConfig {
    pub api_key: String,
    /// Deepgram model name (e.g. `"nova-3"`).
    pub model: String,
}

/// Configuration for the local Whisper backend.
#[cfg(feature = "whisper")]
pub struct WhisperConfig {
    pub model_path: String,
    /// Inference window in seconds. Larger = more context, higher latency.
    pub window_seconds: u32,
}

/// Configuration for the local Parakeet EOU backend.
#[cfg(feature = "parakeet")]
pub struct ParakeetConfig {
    pub model_dir: String,
}

// ---------------------------------------------------------------------------
// Internal backend spec (not public — builder encapsulates it)
// ---------------------------------------------------------------------------

pub(crate) enum BackendSpec {
    Deepgram(DeepgramConfig),
    #[cfg(feature = "whisper")]
    Whisper(WhisperConfig),
    #[cfg(feature = "parakeet")]
    Parakeet(ParakeetConfig),
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for a splitstream session. Call [`start`](SplitStreamBuilder::start)
/// to begin capture and receive transcripts.
///
/// # Example
/// ```ignore
/// let (handle, mut rx) = splitstream::SplitStreamBuilder::new()
///     .with_parakeet("./models/parakeet-eou")
///     .echo_cancellation(true)
///     .start()
///     .await
///     .unwrap();
///
/// while let Some(t) = rx.recv().await {
///     println!("[{:?}] {}", t.source, t.text);
/// }
/// ```
pub struct SplitStreamBuilder {
    backend: Option<BackendSpec>,
    mic_muted: bool,
    sys_muted: bool,
    echo_cancellation: bool,
}

impl SplitStreamBuilder {
    pub fn new() -> Self {
        Self {
            backend: None,
            mic_muted: false,
            sys_muted: false,
            echo_cancellation: false,
        }
    }

    // --- Backend selection ---

    /// Use Deepgram cloud transcription with a Bearer API key.
    pub fn with_deepgram(mut self, api_key: impl Into<String>) -> Self {
        self.backend = Some(BackendSpec::Deepgram(DeepgramConfig {
            api_key: api_key.into(),
            model: "nova-3".to_string(),
        }));
        self
    }

    /// Use Deepgram with full configuration control.
    pub fn with_deepgram_config(mut self, config: DeepgramConfig) -> Self {
        self.backend = Some(BackendSpec::Deepgram(config));
        self
    }

    /// Use local Whisper transcription (requires `whisper` feature).
    #[cfg(feature = "whisper")]
    pub fn with_whisper(mut self, model_path: impl Into<String>) -> Self {
        self.backend = Some(BackendSpec::Whisper(WhisperConfig {
            model_path: model_path.into(),
            window_seconds: 4,
        }));
        self
    }

    /// Use local Whisper with full configuration control (requires `whisper` feature).
    #[cfg(feature = "whisper")]
    pub fn with_whisper_config(mut self, config: WhisperConfig) -> Self {
        self.backend = Some(BackendSpec::Whisper(config));
        self
    }

    /// Use local Parakeet EOU transcription (requires `parakeet` feature).
    #[cfg(feature = "parakeet")]
    pub fn with_parakeet(mut self, model_dir: impl Into<String>) -> Self {
        self.backend = Some(BackendSpec::Parakeet(ParakeetConfig {
            model_dir: model_dir.into(),
        }));
        self
    }

    /// Use local Parakeet with full configuration control (requires `parakeet` feature).
    #[cfg(feature = "parakeet")]
    pub fn with_parakeet_config(mut self, config: ParakeetConfig) -> Self {
        self.backend = Some(BackendSpec::Parakeet(config));
        self
    }

    // --- Initial state ---

    /// Start with the microphone muted. Can be toggled via [`SplitStreamHandle::set_mic_muted`].
    pub fn mic_muted(mut self, muted: bool) -> Self {
        self.mic_muted = muted;
        self
    }

    /// Start with system audio muted (compliance mode). Can be toggled via
    /// [`SplitStreamHandle::set_sys_muted`].
    pub fn sys_muted(mut self, muted: bool) -> Self {
        self.sys_muted = muted;
        self
    }

    /// Enable acoustic echo cancellation. Can be toggled via
    /// [`SplitStreamHandle::set_echo_cancellation`].
    pub fn echo_cancellation(mut self, enabled: bool) -> Self {
        self.echo_cancellation = enabled;
        self
    }

    // --- Start ---

    /// Start the audio engine. Returns a handle for mid-stream control and a
    /// receiver that yields [`Transcript`] events from the active backend.
    pub async fn start(
        self,
    ) -> Result<(SplitStreamHandle, UnboundedReceiver<Transcript>), SplitStreamError> {
        let backend_spec = self.backend.ok_or(SplitStreamError::NoBackend)?;
        let controls = Controls::new(self.mic_muted, self.sys_muted, self.echo_cancellation);
        let (transcript_tx, transcript_rx) = mpsc::unbounded_channel::<Transcript>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let controls_clone = Arc::clone(&controls);
        tokio::spawn(async move {
            if let Err(e) =
                crate::engine::run(controls_clone, backend_spec, transcript_tx, shutdown_rx).await
            {
                eprintln!("Splitstream engine error: {e}");
            }
        });

        let handle = SplitStreamHandle { controls, shutdown_tx };
        Ok((handle, transcript_rx))
    }
}

impl Default for SplitStreamBuilder {
    fn default() -> Self {
        Self::new()
    }
}
