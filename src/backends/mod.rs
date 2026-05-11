#[cfg(feature = "deepgram")]
pub(crate) mod deepgram;
#[cfg(feature = "parakeet")]
pub(crate) mod parakeet;
#[cfg(feature = "whisper")]
pub(crate) mod whisper;

#[cfg(any(feature = "whisper", feature = "parakeet"))]
use std::sync::mpsc::Sender;
#[cfg(feature = "deepgram")]
use tokio::sync::mpsc::UnboundedSender;

/// Internal enum holding the live state for the active backend.
/// Dropped when the engine shuts down — dropping the senders causes
/// inference threads to exit via their `Disconnected` arms.
pub(crate) enum BackendActor {
    #[cfg(feature = "deepgram")]
    Deepgram {
        opus_packet_tx: UnboundedSender<Vec<u8>>,
        encoder: opus::Encoder,
    },
    #[cfg(feature = "whisper")]
    Whisper {
        mic_pcm_tx: Sender<Vec<f32>>,
        sys_pcm_tx: Sender<Vec<f32>>,
    },
    #[cfg(feature = "parakeet")]
    Parakeet {
        mic_pcm_tx: Sender<Vec<f32>>,
        sys_pcm_tx: Sender<Vec<f32>>,
    },
}
