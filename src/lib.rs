//! # splitstream
//!
//! Capture microphone and system audio simultaneously on macOS or Windows, then
//! transcribe both sides of a conversation in real time.
//!
//! The two channels stay separate all the way through: the microphone is echo-
//! cancelled against system audio, both are resampled to 16kHz, and every
//! [`Transcript`] is tagged with the [`AudioSource`] it came from and its
//! offset from the start of the stream.
//!
//! ## Quick start
//!
// Compiled only when a backend this example can call is compiled in;
// `cargo test --no-default-features` would otherwise fail on a method
// that does not exist.
#![cfg_attr(feature = "parakeet", doc = "```no_run")]
#![cfg_attr(not(feature = "parakeet"), doc = "```ignore")]
//! use splitstream::{SplitStreamBuilder, SplitStreamError};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), SplitStreamError> {
//!     let (handle, mut transcripts) = SplitStreamBuilder::new()
//!         .with_parakeet("./models/parakeet-eou")
//!         .echo_cancellation(true)
//!         .start()
//!         .await?;
//!
//!     // Runtime failures — a dropped connection, a backend that dies — arrive
//!     // here instead of showing up as silence.
//!     if let Some(mut errors) = handle.errors() {
//!         tokio::spawn(async move {
//!             while let Some(e) = errors.recv().await {
//!                 eprintln!("splitstream: {e}");
//!             }
//!         });
//!     }
//!
//!     // The handle is cloneable and every method takes `&self`.
//!     let stopper = handle.clone();
//!     tokio::spawn(async move {
//!         tokio::signal::ctrl_c().await.ok();
//!         stopper.shutdown();
//!     });
//!
//!     while let Some(t) = transcripts.recv().await {
//!         println!("[{:>6.2}s {}] {}", t.start.as_secs_f32(), t.source, t.text);
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Backends
//!
//! | Method | Backend | Cargo feature |
//! |---|---|---|
//! | [`with_parakeet`](SplitStreamBuilder::with_parakeet) | Local parakeet-rs (ONNX) | `parakeet` (default) |
//! | [`with_deepgram`](SplitStreamBuilder::with_deepgram) | Deepgram cloud (nova-3) | `deepgram` |
//!
//! ## Requirements
//!
//! macOS [`MIN_MACOS`] or later, or Windows 10 or later. Anything else is a
//! `compile_error!`, because system-audio capture has no portable API.
//!
//! The two platforms differ in how they capture and what they ask of the user:
//!
//! | | macOS | Windows |
//! |---|---|---|
//! | System audio | Core Audio process tap in an aggregate device | WASAPI loopback on the default output device |
//! | System-audio consent | Required (TCC), and start-up fails with [`SplitStreamError::PermissionDenied`] without it | Not required; loopback is ungated |
//! | Microphone consent | Required (TCC), read before capture starts | Required, but only observable as a capture failure |
//! | Signing | Consent is granted to a signed bundle, so a loose binary inherits the terminal's grants | No bundle or signature needed to capture |
//!
//! macOS therefore needs the *Permissions & Signing* section of the README
//! before anything will work. On Windows there is nothing to configure.
//!
//! ## Logging
//!
//! splitstream logs through the [`log`](https://docs.rs/log) facade and never
//! prints. Install any subscriber (`env_logger`, `tracing-subscriber`) to see
//! device selection, rate changes, dropped audio, and every runtime error.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub(crate) mod audio;
pub(crate) mod backends;
#[cfg(feature = "aec")]
pub(crate) mod echo_cancellation;
pub(crate) mod engine;
#[cfg(feature = "deepgram")]
pub(crate) mod transcript_msg;

mod builder;
mod error;
mod handle;
mod transcript;

pub use builder::{DEFAULT_CHANNEL_CAPACITY, SplitStreamBuilder};
pub use error::{
    BoxError, ERROR_CHANNEL_CAPACITY, ErrorReceiver, ErrorSender, MIN_MACOS, PermissionKind,
    SplitStreamError,
};
pub use handle::SplitStreamHandle;
pub use transcript::{AudioSource, Transcript, TranscriptStream};

#[cfg(feature = "deepgram")]
#[cfg_attr(docsrs, doc(cfg(feature = "deepgram")))]
pub use builder::DeepgramConfig;

#[cfg(feature = "whisper")]
#[cfg_attr(docsrs, doc(cfg(feature = "whisper")))]
#[doc(hidden)]
pub use builder::WhisperConfig;

#[cfg(feature = "parakeet")]
#[cfg_attr(docsrs, doc(cfg(feature = "parakeet")))]
pub use builder::ParakeetConfig;

/// splitstream only builds on macOS: the capture path is a CoreAudio process
/// tap plus an aggregate device, which have no equivalent elsewhere.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
compile_error!(
    "splitstream supports macOS 14.2+ and Windows 10+ only — both capture \
     system audio through platform APIs that have no equivalent elsewhere"
);
