//! Builder for configuring and starting a splitstream session.

#[cfg(any(feature = "whisper", feature = "parakeet"))]
use std::path::Path;
#[cfg(any(feature = "whisper", feature = "parakeet"))]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::SplitStreamError;
use crate::handle::{Controls, HandleInner, SplitStreamHandle};
use crate::transcript::{Transcript, TranscriptStream};

/// Default capacity of the transcript channel, in transcript events.
///
/// At a realistic 1–2 segments per second per channel this is roughly ten
/// minutes of backlog before the drop-oldest policy engages — long enough that
/// a briefly blocked consumer loses nothing, short enough to bound memory.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

// ---------------------------------------------------------------------------
// Backend configuration types (public)
// ---------------------------------------------------------------------------

/// Configuration for the Deepgram streaming backend.
///
/// # Example
///
/// ```
/// # use splitstream::DeepgramConfig;
/// let cfg = DeepgramConfig::new(std::env::var("DEEPGRAM_API_KEY").unwrap_or_default())
///     .model("nova-3");
/// assert_eq!(cfg.model, "nova-3");
/// // `Debug` never prints the key itself:
/// assert!(!format!("{cfg:?}").contains("DEEPGRAM"));
/// ```
#[cfg(feature = "deepgram")]
#[cfg_attr(docsrs, doc(cfg(feature = "deepgram")))]
#[derive(Clone)]
#[non_exhaustive]
pub struct DeepgramConfig {
    /// Deepgram API key, sent as `Authorization: Token <key>`.
    ///
    /// Redacted by the [`Debug`] impl — it is a credential, and config structs
    /// end up in logs.
    pub api_key: String,
    /// Deepgram model name (e.g. `"nova-3"`).
    pub model: String,
}

#[cfg(feature = "deepgram")]
impl DeepgramConfig {
    /// A configuration using the `nova-3` model.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: "nova-3".to_string(),
        }
    }

    /// Select a different Deepgram model.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

#[cfg(feature = "deepgram")]
impl Default for DeepgramConfig {
    /// An empty API key and the `nova-3` model. The key must be set before
    /// [`SplitStreamBuilder::start`] or start-up fails.
    fn default() -> Self {
        Self::new(String::new())
    }
}

#[cfg(feature = "deepgram")]
impl std::fmt::Debug for DeepgramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepgramConfig")
            .field(
                "api_key",
                if self.api_key.is_empty() {
                    &"<unset>"
                } else {
                    &"<redacted>"
                },
            )
            .field("model", &self.model)
            .finish()
    }
}

/// Configuration for the local Whisper backend.
#[cfg(feature = "whisper")]
#[cfg_attr(docsrs, doc(cfg(feature = "whisper")))]
#[doc(hidden)]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WhisperConfig {
    /// Path to a whisper.cpp `ggml-*.bin` model file.
    pub model_path: PathBuf,
    /// Inference window in seconds. Larger = more context, higher latency.
    pub window_seconds: u32,
}

#[cfg(feature = "whisper")]
#[doc(hidden)]
impl WhisperConfig {
    /// A configuration with a 4-second inference window.
    pub fn new(model_path: impl AsRef<Path>) -> Self {
        Self {
            model_path: model_path.as_ref().to_path_buf(),
            window_seconds: 4,
        }
    }

    /// Set the inference window, in seconds.
    pub fn window_seconds(mut self, seconds: u32) -> Self {
        self.window_seconds = seconds;
        self
    }
}

#[cfg(feature = "whisper")]
impl Default for WhisperConfig {
    fn default() -> Self {
        Self::new(PathBuf::new())
    }
}

/// Configuration for the local Parakeet EOU backend.
///
/// # Example
///
/// ```
/// # use splitstream::ParakeetConfig;
/// let cfg = ParakeetConfig::new(std::path::Path::new("./models/parakeet-eou"));
/// assert!(cfg.model_dir.ends_with("parakeet-eou"));
/// ```
#[cfg(feature = "parakeet")]
#[cfg_attr(docsrs, doc(cfg(feature = "parakeet")))]
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ParakeetConfig {
    /// Directory holding `encoder.onnx`, `decoder_joint.onnx` and
    /// `tokenizer.json`.
    pub model_dir: PathBuf,
}

#[cfg(feature = "parakeet")]
impl ParakeetConfig {
    /// A configuration pointing at an unpacked Parakeet EOU model directory.
    pub fn new(model_dir: impl AsRef<Path>) -> Self {
        Self {
            model_dir: model_dir.as_ref().to_path_buf(),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal backend spec (not public — builder encapsulates it)
// ---------------------------------------------------------------------------

pub(crate) enum BackendSpec {
    #[cfg(feature = "deepgram")]
    Deepgram(DeepgramConfig),
    #[cfg(feature = "whisper")]
    Whisper(WhisperConfig),
    #[cfg(feature = "parakeet")]
    Parakeet(ParakeetConfig),
}

impl BackendSpec {
    /// Reject configurations that are certain to fail, before any audio device
    /// is opened or any model is read.
    #[cfg_attr(
        not(any(feature = "deepgram", feature = "whisper", feature = "parakeet")),
        allow(clippy::unused_self)
    )]
    fn validate(&self) -> Result<(), SplitStreamError> {
        match self {
            #[cfg(not(any(feature = "deepgram", feature = "whisper", feature = "parakeet")))]
            _ => Ok(()),
            #[cfg(feature = "deepgram")]
            BackendSpec::Deepgram(cfg) => {
                if cfg.api_key.trim().is_empty() {
                    return Err(SplitStreamError::BackendStartFailed {
                        backend: "deepgram",
                        source: "no API key — pass one to with_deepgram(), e.g. from \
                                 the DEEPGRAM_API_KEY environment variable"
                            .into(),
                    });
                }
                Ok(())
            }
            #[cfg(feature = "whisper")]
            BackendSpec::Whisper(cfg) => require_model(&cfg.model_path),
            #[cfg(feature = "parakeet")]
            BackendSpec::Parakeet(cfg) => require_model(&cfg.model_dir),
        }
    }
}

/// A missing model is the most common first-run failure, and used to surface as
/// a backend panic or an opaque loader error deep inside ONNX Runtime.
#[cfg(any(feature = "whisper", feature = "parakeet"))]
fn require_model(path: &Path) -> Result<(), SplitStreamError> {
    if path.exists() {
        Ok(())
    } else {
        Err(SplitStreamError::ModelNotFound {
            path: path.to_path_buf(),
        })
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for a splitstream session. Call [`start`](SplitStreamBuilder::start)
/// to begin capture and receive transcripts.
///
/// # Example
///
// Compiled only when a backend this example can call is present; `cargo test
// --no-default-features` would otherwise fail on a method that does not exist.
#[cfg_attr(feature = "parakeet", doc = "```no_run")]
#[cfg_attr(not(feature = "parakeet"), doc = "```ignore")]
/// use splitstream::{SplitStreamBuilder, SplitStreamError};
///
/// #[tokio::main]
/// async fn main() -> Result<(), SplitStreamError> {
///     let (handle, mut transcripts) = SplitStreamBuilder::new()
///         .with_parakeet("./models/parakeet-eou")
///         .echo_cancellation(true)
///         .start()
///         .await?;
///
///     let stopper = handle.clone();
///     tokio::spawn(async move {
///         tokio::signal::ctrl_c().await.ok();
///         stopper.shutdown();
///     });
///
///     while let Some(t) = transcripts.recv().await {
///         println!("[{}] {}", t.source, t.text);
///     }
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct SplitStreamBuilder {
    backend: Option<BackendSpec>,
    mic_muted: bool,
    sys_muted: bool,
    echo_cancellation: bool,
    channel_capacity: usize,
}

impl std::fmt::Debug for BackendSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(not(any(feature = "deepgram", feature = "whisper", feature = "parakeet")))]
            _ => f.write_str("None"),
            #[cfg(feature = "deepgram")]
            BackendSpec::Deepgram(cfg) => f.debug_tuple("Deepgram").field(cfg).finish(),
            #[cfg(feature = "whisper")]
            BackendSpec::Whisper(cfg) => f.debug_tuple("Whisper").field(cfg).finish(),
            #[cfg(feature = "parakeet")]
            BackendSpec::Parakeet(cfg) => f.debug_tuple("Parakeet").field(cfg).finish(),
        }
    }
}

impl SplitStreamBuilder {
    /// A builder with no backend, nothing muted, and echo cancellation off.
    pub fn new() -> Self {
        Self {
            backend: None,
            mic_muted: false,
            sys_muted: false,
            echo_cancellation: false,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
        }
    }

    // --- Backend selection ---

    /// Use Deepgram cloud transcription with a Bearer API key (requires the
    /// `deepgram` feature).
    ///
    /// # Example
    ///
    /// ```
    /// # use splitstream::SplitStreamBuilder;
    /// let builder = SplitStreamBuilder::new().with_deepgram("dg_...");
    /// ```
    #[cfg(feature = "deepgram")]
    #[cfg_attr(docsrs, doc(cfg(feature = "deepgram")))]
    pub fn with_deepgram(mut self, api_key: impl Into<String>) -> Self {
        self.backend = Some(BackendSpec::Deepgram(DeepgramConfig::new(api_key)));
        self
    }

    /// Use Deepgram with full configuration control (requires the `deepgram`
    /// feature).
    #[cfg(feature = "deepgram")]
    #[cfg_attr(docsrs, doc(cfg(feature = "deepgram")))]
    pub fn with_deepgram_config(mut self, config: DeepgramConfig) -> Self {
        self.backend = Some(BackendSpec::Deepgram(config));
        self
    }

    /// Use local Whisper transcription (requires the `whisper` feature).
    #[cfg(feature = "whisper")]
    #[cfg_attr(docsrs, doc(cfg(feature = "whisper")))]
    #[doc(hidden)]
    pub fn with_whisper(mut self, model_path: impl AsRef<Path>) -> Self {
        self.backend = Some(BackendSpec::Whisper(WhisperConfig::new(model_path)));
        self
    }

    /// Use local Whisper with full configuration control (requires the
    /// `whisper` feature).
    #[cfg(feature = "whisper")]
    #[cfg_attr(docsrs, doc(cfg(feature = "whisper")))]
    #[doc(hidden)]
    pub fn with_whisper_config(mut self, config: WhisperConfig) -> Self {
        self.backend = Some(BackendSpec::Whisper(config));
        self
    }

    /// Use local Parakeet EOU transcription (requires the `parakeet` feature).
    ///
    /// The argument is the directory holding the unpacked ONNX model.
    ///
    /// # Example
    ///
    /// ```
    /// # use splitstream::SplitStreamBuilder;
    /// # use std::path::PathBuf;
    /// let dir: PathBuf = std::env::current_dir().unwrap().join("models/parakeet-eou");
    /// let builder = SplitStreamBuilder::new().with_parakeet(dir);
    /// ```
    #[cfg(feature = "parakeet")]
    #[cfg_attr(docsrs, doc(cfg(feature = "parakeet")))]
    pub fn with_parakeet(mut self, model_dir: impl AsRef<Path>) -> Self {
        self.backend = Some(BackendSpec::Parakeet(ParakeetConfig::new(model_dir)));
        self
    }

    /// Use local Parakeet with full configuration control (requires the
    /// `parakeet` feature).
    #[cfg(feature = "parakeet")]
    #[cfg_attr(docsrs, doc(cfg(feature = "parakeet")))]
    pub fn with_parakeet_config(mut self, config: ParakeetConfig) -> Self {
        self.backend = Some(BackendSpec::Parakeet(config));
        self
    }

    // --- Initial state ---

    /// Start with the microphone muted. Can be toggled via
    /// [`SplitStreamHandle::set_mic_muted`].
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

    /// Set the transcript channel capacity, in transcript events.
    ///
    /// The channel is bounded with a drop-oldest policy — see
    /// [`TranscriptStream`]. A capacity of `0` is treated as `1`.
    pub fn channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity.max(1);
        self
    }

    // --- Start ---

    /// Start the audio engine.
    ///
    /// Returns a cloneable [`SplitStreamHandle`] for mid-stream control and a
    /// [`TranscriptStream`] of [`Transcript`] events from the active backend.
    /// Failures that happen later — a rejected API key, a dropped WebSocket —
    /// arrive on [`SplitStreamHandle::errors`] rather than here.
    ///
    /// This waits for the engine to open both devices and start the backend, so
    /// everything below is reported here rather than as a transcript stream
    /// that ends without explanation.
    ///
    /// # Errors
    ///
    /// - [`SplitStreamError::NoBackend`] if no `with_*` backend was selected.
    /// - [`SplitStreamError::ModelNotFound`] if a local backend's model path
    ///   does not exist.
    /// - [`SplitStreamError::PermissionDenied`] if macOS refuses **system
    ///   audio** capture. A refused *microphone* is not fatal: the session runs
    ///   system-audio-only and reports it on
    ///   [`SplitStreamHandle::errors`] instead.
    /// - [`SplitStreamError::UnsupportedOsVersion`] below macOS
    ///   [`MIN_MACOS`](crate::MIN_MACOS).
    /// - [`SplitStreamError::BackendStartFailed`] if the backend itself cannot
    ///   start.
    #[cfg_attr(
        not(any(feature = "deepgram", feature = "whisper", feature = "parakeet")),
        allow(unreachable_code, unused_variables)
    )]
    pub async fn start(self) -> Result<(SplitStreamHandle, TranscriptStream), SplitStreamError> {
        #[cfg(not(any(feature = "deepgram", feature = "whisper", feature = "parakeet")))]
        return Err(SplitStreamError::NoBackendFeature);

        let backend_spec = self.backend.ok_or(SplitStreamError::NoBackend)?;
        backend_spec.validate()?;

        let controls = Controls::new(self.mic_muted, self.sys_muted, self.echo_cancellation);
        let (transcript_tx, transcript_rx) =
            tokio::sync::broadcast::channel::<Transcript>(self.channel_capacity);
        let (error_tx, error_rx) =
            tokio::sync::mpsc::channel::<SplitStreamError>(crate::error::ERROR_CHANNEL_CAPACITY);
        let shutdown = Arc::new(tokio::sync::Notify::new());

        let controls_clone = Arc::clone(&controls);
        let engine_shutdown = Arc::clone(&shutdown);
        let engine_errors = error_tx.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = crate::engine::run_with_errors(
                controls_clone,
                backend_spec,
                transcript_tx,
                engine_shutdown,
                Some(error_tx),
                Some(ready_tx),
            )
            .await;
            // Bring-up failures went back to `start()` through the readiness
            // channel. Anything arriving here happened mid-session, after
            // `start()` had already returned, so the error channel is the only
            // way to tell the caller.
            if let Err(e) = result {
                crate::error::ErrorReporter::new(Some(engine_errors)).report(e);
            }
        });

        // Wait for the engine to open the devices and start the backend, so a
        // denied permission, an unsupported macOS version or a bad model is an
        // `Err` from `start()` rather than a transcript stream that ends with
        // no explanation.
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            // The engine task ended without signalling, which means it panicked.
            Err(_) => {
                return Err(SplitStreamError::BackendStartFailed {
                    backend: "engine",
                    source: "the engine task stopped during start-up".into(),
                });
            }
        }

        let handle = SplitStreamHandle {
            inner: Arc::new(HandleInner {
                controls,
                shutdown,
                errors: Mutex::new(Some(error_rx)),
            }),
        };
        Ok((handle, TranscriptStream::new(transcript_rx)))
    }
}

impl Default for SplitStreamBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SplitStreamError;

    #[tokio::test]
    async fn no_backend_returns_error() {
        let result = SplitStreamBuilder::new().start().await;
        #[cfg(any(feature = "deepgram", feature = "whisper", feature = "parakeet"))]
        assert!(matches!(result, Err(SplitStreamError::NoBackend)));
        #[cfg(not(any(feature = "deepgram", feature = "whisper", feature = "parakeet")))]
        assert!(matches!(result, Err(SplitStreamError::NoBackendFeature)));
    }

    #[cfg(feature = "parakeet")]
    #[tokio::test]
    async fn missing_model_dir_returns_model_not_found() {
        let result = SplitStreamBuilder::new()
            .with_parakeet("/nonexistent/splitstream/models/parakeet-eou")
            .start()
            .await;
        match result {
            Err(SplitStreamError::ModelNotFound { path }) => {
                assert!(path.ends_with("parakeet-eou"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[cfg(feature = "deepgram")]
    #[tokio::test]
    async fn empty_api_key_is_rejected() {
        let result = SplitStreamBuilder::new().with_deepgram("   ").start().await;
        assert!(matches!(
            result,
            Err(SplitStreamError::BackendStartFailed {
                backend: "deepgram",
                ..
            })
        ));
    }

    #[cfg(feature = "deepgram")]
    #[test]
    fn debug_redacts_the_api_key() {
        let cfg = DeepgramConfig::new("dg_super_secret");
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("dg_super_secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn channel_capacity_is_never_zero() {
        assert_eq!(
            SplitStreamBuilder::new()
                .channel_capacity(0)
                .channel_capacity,
            1
        );
    }
}
