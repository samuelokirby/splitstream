//! The control handle returned by [`SplitStreamBuilder::start`].
//!
//! [`SplitStreamBuilder`]: crate::SplitStreamBuilder

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::error::ErrorReceiver;

/// Shared atomic controls read each tick by the audio engine.
pub(crate) struct Controls {
    pub mic_muted: AtomicBool,
    pub sys_muted: AtomicBool,
    pub echo_cancellation: AtomicBool,
}

impl Controls {
    pub(crate) fn new(mic_muted: bool, sys_muted: bool, echo_cancellation: bool) -> Arc<Self> {
        Arc::new(Self {
            mic_muted: AtomicBool::new(mic_muted),
            sys_muted: AtomicBool::new(sys_muted),
            echo_cancellation: AtomicBool::new(echo_cancellation),
        })
    }
}

/// Everything a handle owns, behind one `Arc` so the handle can be cloned.
pub(crate) struct HandleInner {
    pub(crate) controls: Arc<Controls>,
    pub(crate) shutdown: Arc<Notify>,
    /// Taken by the first caller of [`SplitStreamHandle::errors`].
    pub(crate) errors: Mutex<Option<ErrorReceiver>>,
}

/// Dropping the last [`SplitStreamHandle`] shuts the engine down.
///
/// Every handle is a clone of one `Arc<HandleInner>`, so this runs exactly once
/// — when the final clone goes away — which is why the notify lives here rather
/// than in `Drop for SplitStreamHandle`, where two threads racing to drop the
/// last two clones could each observe a strong count of 2 and neither would
/// fire.
///
/// Without this, dropping every handle would strand the engine: the process
/// tap, the aggregate device and the mic stream would stay live for the rest of
/// the process with no remaining way to stop them, because the `Notify` is only
/// reachable through a handle. Version 0.1.x got this for free — the handle
/// owned a `oneshot::Sender` whose drop woke the engine's receiver — and making
/// the handle cloneable removed that guarantee.
impl Drop for HandleInner {
    fn drop(&mut self) {
        self.shutdown.notify_one();
    }
}

/// A cheap, cloneable handle to a running splitstream session: mute toggles,
/// echo-cancellation toggle, the runtime error channel, and shutdown.
///
/// Every method takes `&self`, so the handle can be cloned into a UI task, a
/// signal handler, and a hotkey listener at the same time. Shutting down from
/// any clone stops the one shared engine; further calls are no-ops.
///
/// The engine keeps running while at least one clone is alive. It stops when
/// [`shutdown`](Self::shutdown) is called, or when the last clone is dropped —
/// so a handle that goes out of scope never leaves the audio tap running.
///
/// # Example
///
// Compiled only when a backend this example can call is compiled in;
// `cargo test --no-default-features` would otherwise fail on a method
// that does not exist.
#[cfg_attr(feature = "parakeet", doc = "```no_run")]
#[cfg_attr(not(feature = "parakeet"), doc = "```ignore")]
/// # async fn run() -> Result<(), splitstream::SplitStreamError> {
/// let (handle, mut transcripts) = splitstream::SplitStreamBuilder::new()
///     .with_parakeet("./models/parakeet-eou")
///     .start()
///     .await?;
///
/// // A clone can shut the session down from a signal handler...
/// let stopper = handle.clone();
/// tokio::spawn(async move {
///     tokio::signal::ctrl_c().await.ok();
///     stopper.shutdown();
/// });
///
/// // ...while the original still toggles compliance mode.
/// handle.set_sys_muted(true);
///
/// while let Some(t) = transcripts.recv().await {
///     println!("[{}] {}", t.source, t.text);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SplitStreamHandle {
    pub(crate) inner: Arc<HandleInner>,
}

impl SplitStreamHandle {
    /// Mute or unmute the microphone channel. Takes effect on the next 20ms tick.
    ///
    /// Muting replaces the microphone frame with silence *after* echo
    /// cancellation, so the filter keeps adapting while you are muted and does
    /// not have to reconverge when you unmute.
    ///
    /// # Example
    ///
    /// ```
    /// # fn f(handle: &splitstream::SplitStreamHandle) {
    /// handle.set_mic_muted(true);
    /// assert!(handle.mic_muted());
    /// # }
    /// ```
    pub fn set_mic_muted(&self, muted: bool) {
        self.inner
            .controls
            .mic_muted
            .store(muted, Ordering::Relaxed);
    }

    /// Whether the microphone channel is currently muted.
    pub fn mic_muted(&self) -> bool {
        self.inner.controls.mic_muted.load(Ordering::Relaxed)
    }

    /// Mute or unmute the system audio channel — "compliance mode", for when
    /// the far side has not consented to being recorded. Takes effect on the
    /// next 20ms tick.
    ///
    /// Muting replaces the system frame with silence *before* echo
    /// cancellation, so no trace of the far side reaches the backend.
    ///
    /// # Example
    ///
    /// ```
    /// # fn f(handle: &splitstream::SplitStreamHandle) {
    /// handle.set_sys_muted(true);
    /// assert!(handle.sys_muted());
    /// # }
    /// ```
    pub fn set_sys_muted(&self, muted: bool) {
        self.inner
            .controls
            .sys_muted
            .store(muted, Ordering::Relaxed);
    }

    /// Whether the system audio channel is currently muted.
    pub fn sys_muted(&self) -> bool {
        self.inner.controls.sys_muted.load(Ordering::Relaxed)
    }

    /// Enable or disable acoustic echo cancellation. Takes effect on the next
    /// 20ms tick.
    ///
    /// Echo cancellation is skipped regardless of this flag when the
    /// microphone runs below 16kHz.
    ///
    /// # Example
    ///
    /// ```
    /// # fn f(handle: &splitstream::SplitStreamHandle) {
    /// handle.set_echo_cancellation(false);
    /// # }
    /// ```
    pub fn set_echo_cancellation(&self, enabled: bool) {
        self.inner
            .controls
            .echo_cancellation
            .store(enabled, Ordering::Relaxed);
    }

    /// Whether echo cancellation is currently requested.
    pub fn echo_cancellation(&self) -> bool {
        self.inner
            .controls
            .echo_cancellation
            .load(Ordering::Relaxed)
    }

    /// Take the runtime error channel.
    ///
    /// Failures that happen *after* `start()` returned — the Deepgram key being
    /// rejected, the WebSocket dropping, a microphone that disappears, a
    /// resampler that cannot be rebuilt — are delivered here. Without this,
    /// they look identical to silence.
    ///
    /// There is one receiver per session: the first call returns it and every
    /// later call returns `None`. Errors are always logged through the `log`
    /// crate whether or not anything takes the receiver.
    ///
    /// # Example
    ///
    // Compiled only when a backend this example can call is compiled in;
    // `cargo test --no-default-features` would otherwise fail on a method
    // that does not exist.
    #[cfg_attr(feature = "parakeet", doc = "```no_run")]
    #[cfg_attr(not(feature = "parakeet"), doc = "```ignore")]
    /// # async fn run() -> Result<(), splitstream::SplitStreamError> {
    /// let (handle, mut transcripts) = splitstream::SplitStreamBuilder::new()
    ///     .with_parakeet("./models/parakeet-eou")
    ///     .start()
    ///     .await?;
    ///
    /// let mut errors = handle.errors().expect("first caller takes the receiver");
    /// tokio::spawn(async move {
    ///     while let Some(e) = errors.recv().await {
    ///         eprintln!("splitstream: {e}");
    ///     }
    /// });
    /// # let _ = transcripts.recv().await;
    /// # Ok(())
    /// # }
    /// ```
    pub fn errors(&self) -> Option<ErrorReceiver> {
        self.inner
            .errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Shut down the audio engine.
    ///
    /// Returns immediately; the engine stops on its next tick, drops every
    /// backend sender, and the inference threads exit through their
    /// `Disconnected` arms. The transcript channel then closes, so a
    /// `while let Some(t) = transcripts.recv().await` loop ends on its own.
    ///
    /// Idempotent, and callable from any clone of the handle.
    ///
    /// # Example
    ///
    /// ```
    /// # fn f(handle: &splitstream::SplitStreamHandle) {
    /// handle.shutdown();
    /// handle.shutdown(); // no-op
    /// # }
    /// ```
    pub fn shutdown(&self) {
        self.inner.shutdown.notify_one();
    }
}

impl std::fmt::Debug for SplitStreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplitStreamHandle")
            .field("mic_muted", &self.mic_muted())
            .field("sys_muted", &self.sys_muted())
            .field("echo_cancellation", &self.echo_cancellation())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    fn inner() -> (Arc<HandleInner>, Arc<Notify>) {
        let shutdown = Arc::new(Notify::new());
        let inner = Arc::new(HandleInner {
            controls: Arc::new(Controls {
                mic_muted: AtomicBool::new(false),
                sys_muted: AtomicBool::new(false),
                echo_cancellation: AtomicBool::new(false),
            }),
            shutdown: Arc::clone(&shutdown),
            errors: Mutex::new(None),
        });
        (inner, shutdown)
    }

    /// The regression this exists for: making the handle cloneable replaced a
    /// `oneshot::Sender`, whose drop woke the engine, with a `Notify` that has
    /// no such behaviour. Without `Drop for HandleInner` the engine would run
    /// forever once the last handle went away.
    #[tokio::test]
    async fn dropping_the_last_handle_notifies_shutdown() {
        let (inner, shutdown) = inner();
        let handle = SplitStreamHandle { inner };

        drop(handle);

        // The permit is stored even though nothing was awaiting at notify time,
        // which is what makes this safe against the engine being mid-tick.
        tokio::time::timeout(Duration::from_secs(1), shutdown.notified())
            .await
            .expect("dropping the last handle must shut the engine down");
    }

    #[tokio::test]
    async fn dropping_one_clone_does_not_shut_down() {
        let (inner, shutdown) = inner();
        let handle = SplitStreamHandle { inner };
        let clone = handle.clone();

        drop(clone);

        assert!(
            tokio::time::timeout(Duration::from_millis(100), shutdown.notified())
                .await
                .is_err(),
            "a surviving clone must keep the engine running"
        );

        // ...and the survivor still shuts it down when it goes.
        drop(handle);
        tokio::time::timeout(Duration::from_secs(1), shutdown.notified())
            .await
            .expect("the last surviving clone must shut the engine down");
    }

    #[tokio::test]
    async fn explicit_shutdown_still_works_and_is_idempotent() {
        let (inner, shutdown) = inner();
        let handle = SplitStreamHandle { inner };

        handle.shutdown();
        handle.shutdown(); // must not panic or double-fire into anything
        drop(handle); // Drop also notifies; a stored permit is harmless

        tokio::time::timeout(Duration::from_secs(1), shutdown.notified())
            .await
            .expect("explicit shutdown must notify");
    }

    #[test]
    fn controls_are_shared_between_clones() {
        let (inner, _shutdown) = inner();
        let handle = SplitStreamHandle { inner };
        let clone = handle.clone();

        clone.set_mic_muted(true);
        assert!(handle.mic_muted());
        handle.set_sys_muted(true);
        assert!(clone.sys_muted());
    }
}
