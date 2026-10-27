//! Error types for splitstream.
//!
//! [`SplitStreamError`] is returned by the builder's `start()` and is also the
//! payload of the runtime error channel returned by
//! [`SplitStreamHandle::errors`](crate::SplitStreamHandle::errors).
//!
//! All variants that wrap another failure use `#[source]`, so callers can walk
//! the chain with [`std::error::Error::source`].

use std::path::PathBuf;

/// Which macOS privacy permission was refused.
///
/// Both permissions are requested by the OS (TCC) the first time the process
/// touches the corresponding hardware, and both require a signed bundle with
/// the matching `Info.plist` usage-description key to prompt at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PermissionKind {
    /// `NSMicrophoneUsageDescription` — microphone input.
    Microphone,
    /// `NSAudioCaptureUsageDescription` — Core Audio process tap (system audio).
    SystemAudioCapture,
}

impl std::fmt::Display for PermissionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PermissionKind::Microphone => f.write_str("microphone"),
            PermissionKind::SystemAudioCapture => f.write_str("system audio capture"),
        }
    }
}

/// Boxed underlying cause, used by the variants that wrap a foreign error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Errors produced by splitstream, both at start-up and at runtime.
///
/// This enum is `#[non_exhaustive]`: matching on it must include a `_` arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SplitStreamError {
    /// No backend was selected on the builder.
    #[error("no backend configured — call with_deepgram or with_parakeet on the builder")]
    NoBackend,

    /// splitstream was compiled with no transcription backend feature at all,
    /// so no `with_*` method exists to call.
    #[error(
        "splitstream was built without a transcription backend — enable the \
         `parakeet` (default) or `deepgram` cargo feature"
    )]
    NoBackendFeature,

    /// cpal reported no default input device, so microphone capture is impossible.
    #[error("no default microphone device found")]
    NoMicDevice,

    /// Core Audio reported no default input device. The aggregate device that
    /// wraps the process tap uses the default input as its clock source, so
    /// system-audio capture cannot start without one.
    ///
    /// macOS only.
    #[error("no default input device available as the aggregate device clock source")]
    NoDefaultInputDevice,

    /// There is no default output (render) device to capture from.
    ///
    /// Windows captures system audio as WASAPI loopback on the device audio is
    /// *playing* to, so with no output device there is nothing to record.
    #[error("no default output device found to capture system audio from")]
    NoDefaultOutputDevice,

    /// macOS refused access to the microphone or to the system-audio tap.
    #[error(
        "permission denied for {what} — grant access in \
         System Settings › Privacy & Security, and see the Permissions & Signing \
         section of the splitstream README"
    )]
    PermissionDenied {
        /// The permission that was refused.
        what: PermissionKind,
    },

    /// The running macOS release predates the Core Audio process-tap API.
    ///
    /// macOS only. Windows support targets Windows 10 and later, which is the
    /// tested baseline rather than a version this checks at runtime.
    ///
    /// `AudioHardwareCreateProcessTap` is `API_AVAILABLE(macos(14.2))`, so
    /// system-audio capture cannot work below that release.
    #[error(
        "macOS {found} is too old — splitstream requires macOS {} or later",
        MIN_MACOS
    )]
    UnsupportedOsVersion {
        /// The detected OS version, e.g. `"14.0.1"`.
        found: String,
    },

    /// A Core Audio call in the tap/aggregate-device setup sequence failed.
    ///
    /// macOS only.
    ///
    /// `status` is the raw `OSStatus`; Core Audio statuses are usually
    /// four-character codes (e.g. `'nope'` = 1852796517).
    #[error("system audio tap failed at {stage}: OSStatus {status}")]
    TapCreationFailed {
        /// The setup step that failed, e.g. `"create process tap"`.
        stage: &'static str,
        /// The raw `OSStatus` returned by Core Audio.
        status: i32,
    },

    /// System-audio capture could not be started.
    ///
    /// Windows only: the macOS path reports
    /// [`TapCreationFailed`](Self::TapCreationFailed) instead, because Core
    /// Audio gives a numeric `OSStatus` where cpal gives a typed error.
    #[error("system audio capture failed at {stage}")]
    SysCaptureFailed {
        /// The setup step that failed, e.g. `"build loopback stream"`.
        stage: &'static str,
        /// The underlying cpal error.
        #[source]
        source: BoxError,
    },

    /// Microphone capture could not be started (cpal device/stream error).
    #[error("microphone capture failed at {stage}")]
    MicCaptureFailed {
        /// The setup step that failed, e.g. `"build input stream"`.
        stage: &'static str,
        /// The underlying cpal error.
        #[source]
        source: BoxError,
    },

    /// The resampler could not be constructed for the given input rate.
    #[error("failed to build a {in_rate}Hz → 16000Hz resampler")]
    ResamplerInit {
        /// The input sample rate that was rejected.
        in_rate: u32,
        /// The underlying rubato error.
        #[source]
        source: BoxError,
    },

    /// The configured model path does not exist on disk.
    #[error(
        "model not found at {} — run \
         `cargo install splitstream --features cli --bin download-parakeet` \
         to fetch it",
        .path.display()
    )]
    ModelNotFound {
        /// The path that was checked.
        path: PathBuf,
    },

    /// The model exists but could not be loaded.
    #[error("failed to load model at {}", .path.display())]
    ModelLoad {
        /// The model path that failed to load.
        path: PathBuf,
        /// The underlying loader error.
        #[source]
        source: BoxError,
    },

    /// A backend could not be started (thread spawn, invalid credentials,
    /// malformed endpoint, codec init).
    #[error("failed to start the {backend} backend")]
    BackendStartFailed {
        /// Backend name: `"deepgram"`, `"whisper"` or `"parakeet"`.
        backend: &'static str,
        /// The underlying failure.
        #[source]
        source: BoxError,
    },

    /// A backend that started successfully has since stopped (network drop,
    /// rejected API key, inference thread exit). Delivered on the runtime
    /// error channel, never returned from `start()`.
    #[error("the {backend} backend stopped: {reason}")]
    BackendStopped {
        /// Backend name.
        backend: &'static str,
        /// Human-readable reason.
        reason: String,
    },
}

/// The minimum macOS release splitstream supports.
///
/// Derived from the SDK annotation on `AudioHardwareCreateProcessTap`
/// (`AudioHardwareTapping.h`: `API_AVAILABLE(macos(14.2))`). The
/// `initMonoGlobalTapButExcludeProcesses:` initializer used by the system-audio
/// tap carries no narrower annotation than its class.
pub const MIN_MACOS: &str = "14.2";

/// Minimum supported macOS as `(major, minor)`.
#[cfg(target_os = "macos")]
pub(crate) const MIN_MACOS_VERSION: (i64, i64) = (14, 2);

// ---------------------------------------------------------------------------
// Runtime error plumbing
//
// Failures after `start()` returned cannot be returned to the caller, so the
// engine and every backend hold a cloned `ErrorReporter`. It always logs, and
// additionally forwards to the receiver handed out by
// `SplitStreamHandle::errors`.
// ---------------------------------------------------------------------------

/// How many runtime errors are buffered for a consumer that has not drained
/// them yet.
///
/// The channel is bounded because the receiver stays alive inside the handle
/// until [`SplitStreamHandle::errors`](crate::SplitStreamHandle::errors) is
/// called, and most callers never call it — an unbounded queue there would
/// grow for the whole session with nobody to empty it.
pub const ERROR_CHANNEL_CAPACITY: usize = 64;

/// Signals the outcome of engine bring-up back to
/// [`SplitStreamBuilder::start`](crate::SplitStreamBuilder::start), so a
/// failure opening the devices or starting the backend is *returned* rather
/// than only reported on the error channel.
pub(crate) type ReadySender = tokio::sync::oneshot::Sender<Result<(), SplitStreamError>>;

/// Sender half of the runtime error channel.
pub type ErrorSender = tokio::sync::mpsc::Sender<SplitStreamError>;

/// Receiver half of the runtime error channel, obtained from
/// [`SplitStreamHandle::errors`](crate::SplitStreamHandle::errors).
///
/// Bounded to [`ERROR_CHANNEL_CAPACITY`]. When it is full the *newest* error is
/// dropped rather than the oldest: the first failure is normally the root cause
/// and everything after it is cascade noise. Nothing is lost either way —
/// every error is logged through the `log` facade before it is ever queued.
pub type ErrorReceiver = tokio::sync::mpsc::Receiver<SplitStreamError>;

/// Cheap, cloneable sink for runtime errors, held by the engine and by every
/// backend task/thread.
///
/// Reporting always logs at `error` level; when a sender has been attached the
/// error is additionally delivered to the consumer. Detached reporters are
/// therefore safe to use everywhere and degrade to logging only.
#[derive(Clone, Default)]
pub(crate) struct ErrorReporter {
    tx: Option<ErrorSender>,
    /// Set once shutdown has been requested. Backends exit by observing their
    /// channel close, which at the exit site is indistinguishable from a crash,
    /// so this flag is what tells the two apart.
    shutting_down: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ErrorReporter {
    /// A reporter that only logs. Kept for callers that have no channel to
    /// attach, including tests.
    #[allow(dead_code)]
    pub(crate) fn detached() -> Self {
        Self {
            tx: None,
            shutting_down: Default::default(),
        }
    }

    /// A reporter that logs and forwards to `tx`.
    pub(crate) fn new(tx: Option<ErrorSender>) -> Self {
        Self {
            tx,
            shutting_down: Default::default(),
        }
    }

    /// Mark the session as shutting down, so the backend exits that follow are
    /// understood as expected rather than reported as failures.
    ///
    /// Shared by every clone of this reporter, which is how the engine reaches
    /// the backends without threading a flag through each `spawn`.
    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Report a backend stopping, unless shutdown was requested — in which case
    /// stopping is the point, and a `BackendStopped` error would be noise a
    /// consumer cannot tell apart from a real mid-session crash.
    // Only the parakeet and deepgram backends report a stop this way; the
    // whisper threads exit without one. So this is unused whenever neither of
    // those two is compiled in — including a whisper-only build.
    #[cfg_attr(not(any(feature = "deepgram", feature = "parakeet")), allow(dead_code))]
    pub(crate) fn report_expected_stop(&self, err: SplitStreamError) {
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            log::debug!("{err} (expected: shutting down)");
            return;
        }
        self.report(err);
    }

    /// Log `err` and forward it to the consumer if one is attached.
    pub(crate) fn report(&self, err: SplitStreamError) {
        log::error!("{err}");
        let mut src = std::error::Error::source(&err);
        while let Some(e) = src {
            log::error!("  caused by: {e}");
            src = e.source();
        }
        if let Some(tx) = &self.tx {
            // `try_send` never blocks, which matters because `report` is called
            // from the real-time-adjacent engine task and from backend threads.
            // A full queue means the consumer is not draining; the error has
            // already been logged above, so dropping it here loses nothing but
            // the programmatic notification.
            if let Err(tokio::sync::mpsc::error::TrySendError::Full(dropped)) = tx.try_send(err) {
                log::debug!(
                    "runtime error channel full ({ERROR_CHANNEL_CAPACITY}); \
                     dropping: {dropped} — call SplitStreamHandle::errors() and \
                     drain it to receive these"
                );
            }
        }
    }
}

impl std::fmt::Debug for ErrorReporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErrorReporter")
            .field("attached", &self.tx.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Core Audio status mapping
// ---------------------------------------------------------------------------

/// `kAudioHardwareIllegalOperationError` — what Core Audio returns from
/// `AudioHardwareCreateProcessTap` when the process has not been granted
/// audio-capture consent.
#[cfg(target_os = "macos")]
const K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR: i32 = i32::from_be_bytes(*b"nope");

/// Map a Core Audio `OSStatus` from the tap setup sequence onto an error,
/// promoting the known permission-denial status to [`SplitStreamError::PermissionDenied`].
#[cfg(target_os = "macos")]
pub(crate) fn tap_status_error(stage: &'static str, status: i32) -> SplitStreamError {
    if status == K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR {
        SplitStreamError::PermissionDenied {
            what: PermissionKind::SystemAudioCapture,
        }
    } else {
        SplitStreamError::TapCreationFailed { stage, status }
    }
}

#[cfg(test)]
mod channel_tests {
    use super::*;

    fn err(n: usize) -> SplitStreamError {
        SplitStreamError::BackendStopped {
            backend: "test",
            reason: n.to_string(),
        }
    }

    /// The channel must be bounded: its receiver lives inside the handle until
    /// `SplitStreamHandle::errors()` is called, and most callers never call it,
    /// so an unbounded queue would grow untouched for the whole session.
    #[tokio::test]
    async fn a_full_channel_drops_the_newest_and_keeps_the_root_cause() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(ERROR_CHANNEL_CAPACITY);
        let reporter = ErrorReporter::new(Some(tx));

        // Report twice the capacity without ever draining.
        for n in 0..ERROR_CHANNEL_CAPACITY * 2 {
            reporter.report(err(n));
        }

        // The first `CAPACITY` survive, in order: the earliest failure is the
        // one worth keeping, later ones are usually cascade noise.
        for expected in 0..ERROR_CHANNEL_CAPACITY {
            match rx.try_recv() {
                Ok(SplitStreamError::BackendStopped { reason, .. }) => {
                    assert_eq!(reason, expected.to_string());
                }
                other => panic!("expected buffered error {expected}, got {other:?}"),
            }
        }
        assert!(
            rx.try_recv().is_err(),
            "queue must be bounded at {ERROR_CHANNEL_CAPACITY}, not grow past it"
        );
    }

    #[tokio::test]
    async fn reporting_never_blocks_when_nobody_drains() {
        let (tx, _rx) = tokio::sync::mpsc::channel(ERROR_CHANNEL_CAPACITY);
        let reporter = ErrorReporter::new(Some(tx));

        // `report` is called from the engine task and from backend threads;
        // if it ever awaited a full queue it would stall audio capture.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for n in 0..ERROR_CHANNEL_CAPACITY * 10 {
                reporter.report(err(n));
            }
        })
        .await
        .expect("report must never block");
    }

    #[tokio::test]
    async fn a_detached_reporter_only_logs() {
        ErrorReporter::new(None).report(err(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn illegal_operation_status_maps_to_permission_denied() {
        let e = tap_status_error(
            "create process tap",
            K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR,
        );
        assert!(matches!(
            e,
            SplitStreamError::PermissionDenied {
                what: PermissionKind::SystemAudioCapture
            }
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn other_status_keeps_the_raw_code() {
        let e = tap_status_error("tap ASBD", -50);
        match e {
            SplitStreamError::TapCreationFailed { stage, status } => {
                assert_eq!(stage, "tap ASBD");
                assert_eq!(status, -50);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn errors_chain_through_source() {
        let e = SplitStreamError::ModelLoad {
            path: PathBuf::from("/tmp/model"),
            source: "boom".into(),
        };
        assert_eq!(
            std::error::Error::source(&e).map(|s| s.to_string()),
            Some("boom".to_string())
        );
    }

    #[test]
    fn unsupported_os_message_names_the_floor() {
        let e = SplitStreamError::UnsupportedOsVersion {
            found: "14.0".into(),
        };
        assert!(e.to_string().contains("14.2"));
    }
}
