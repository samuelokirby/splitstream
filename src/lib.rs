//! # splitstream
//!
//! Capture microphone and system audio simultaneously on macOS, then transcribe
//! both sides of a conversation in real time.
//!
//! ## Quick start
//!
//! ```ignore
//! let (handle, mut rx) = splitstream::SplitStreamBuilder::new()
//!     .with_parakeet("./models/parakeet-eou")
//!     .echo_cancellation(true)
//!     .start()
//!     .await
//!     .unwrap();
//!
//! while let Some(t) = rx.recv().await {
//!     println!("[{:?}] {}", t.source, t.text);
//! }
//! ```
//!
//! ## Backends
//!
//! | Method | Backend | Cargo feature |
//! |---|---|---|
//! | `with_deepgram(api_key)` | Deepgram cloud (nova-3) | *(always available)* |
//! | `with_whisper(model_path)` | Local whisper-rs (Metal) | `whisper` |
//! | `with_parakeet(model_dir)` | Local parakeet-rs (ONNX) | `parakeet` |

pub mod audio;
pub mod backends;
pub(crate) mod echo_cancellation;
pub(crate) mod engine;
pub(crate) mod transcript_msg;

mod builder;
mod handle;
mod transcript;

pub use builder::{DeepgramConfig, SplitStreamBuilder};
pub use handle::SplitStreamHandle;
pub use transcript::{AudioSource, Transcript};

#[cfg(feature = "whisper")]
pub use builder::WhisperConfig;

#[cfg(feature = "parakeet")]
pub use builder::ParakeetConfig;

/// Errors returned by [`SplitStreamBuilder::start`].
#[derive(Debug, thiserror::Error)]
pub enum SplitStreamError {
    #[error("no backend configured — call with_deepgram, with_whisper, or with_parakeet")]
    NoBackend,

    #[error("no default microphone device found")]
    NoMicDevice,

    #[error("sys audio tap failed: {0}")]
    SysAudioTap(String),

    #[error("model load failed: {0}")]
    ModelLoad(String),
}
