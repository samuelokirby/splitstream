use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// A handle returned from [`crate::SplitStreamBuilder::start`] for
/// mid-stream control and clean shutdown.
pub struct SplitStreamHandle {
    pub(crate) controls: Arc<Controls>,
    pub(crate) shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl SplitStreamHandle {
    /// Mute or unmute the microphone channel. Takes effect on the next 20ms tick.
    pub fn set_mic_muted(&self, muted: bool) {
        self.controls.mic_muted.store(muted, Ordering::Relaxed);
    }

    /// Mute or unmute the system audio channel. Takes effect on the next 20ms tick.
    pub fn set_sys_muted(&self, muted: bool) {
        self.controls.sys_muted.store(muted, Ordering::Relaxed);
    }

    /// Enable or disable acoustic echo cancellation. Takes effect on the next 20ms tick.
    pub fn set_echo_cancellation(&self, enabled: bool) {
        self.controls.echo_cancellation.store(enabled, Ordering::Relaxed);
    }

    /// Shut down the audio engine. Drops all backend senders so inference
    /// threads exit naturally via their `Disconnected` arms.
    pub fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}
