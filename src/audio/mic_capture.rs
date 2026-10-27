//! cpal microphone capture with mid-stream sample-rate adaptation.
//!
//! When macOS changes a device's sample rate, CoreAudio invalidates the cpal
//! stream and callbacks stop firing. `maybe_rebuild` polls the device config
//! every second and rebuilds the entire stream (new ring buffer + new cpal
//! stream) when the rate changes.
//!
//! The cpal data callback runs on a real-time thread: it uses a preallocated
//! downmix scratch buffer and records ring-buffer overruns into an atomic
//! rather than logging.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "macos")]
use cidre::av;
use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{error, info};
use ringbuf::traits::{Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

#[cfg(target_os = "macos")]
use crate::error::PermissionKind;
use crate::error::SplitStreamError;

/// Ring buffer capacity, in f32 samples.
const RING_CAPACITY: usize = 8192;

pub(crate) struct MicStream {
    pub sample_rate: u32,
    pub stream: cpal::Stream,
    pub consumer: HeapCons<f32>,
    /// Running count of samples dropped because the ring buffer was full.
    /// Written by the capture callback, read by the engine.
    pub dropped_samples: Arc<AtomicU64>,
}

/// Average `channels` interleaved samples per frame into `scratch`, returning
/// the number of mono frames written.
///
/// Truncates to `scratch.len()` rather than allocating: this runs on the
/// real-time capture thread.
fn downmix_into(data: &[f32], channels: usize, scratch: &mut [f32]) -> usize {
    debug_assert!(channels >= 1);
    let frames = (data.len() / channels).min(scratch.len());
    for (f, slot) in scratch[..frames].iter_mut().enumerate() {
        let frame = &data[f * channels..f * channels + channels];
        *slot = frame.iter().sum::<f32>() / channels as f32;
    }
    frames
}

/// Build the cpal data callback: downmix to mono through a preallocated
/// scratch buffer and record overruns.
fn make_callback(
    channels: usize,
    mut prod: HeapProd<f32>,
    dropped: Arc<AtomicU64>,
) -> impl FnMut(&[f32], &cpal::InputCallbackInfo) + Send + 'static {
    let mut mono_scratch = vec![0.0f32; RING_CAPACITY];
    move |data: &[f32], _: &cpal::InputCallbackInfo| {
        let (pushed, offered) = if channels <= 1 {
            (prod.push_slice(data), data.len())
        } else {
            let frames = downmix_into(data, channels, &mut mono_scratch);
            (prod.push_slice(&mono_scratch[..frames]), frames)
        };
        if pushed < offered {
            dropped.fetch_add((offered - pushed) as u64, Ordering::Relaxed);
        }
    }
}

fn on_stream_error(err: cpal::StreamError) {
    error!("Mic stream error: {err}");
}

/// Map a TCC authorization status onto an error.
///
/// `NotDetermined` is deliberately *not* a failure: the first
/// `build_input_stream` is what triggers the system prompt, so refusing to try
/// would mean the prompt never appears. `Denied` and `Restricted` are both
/// terminal — the process will never receive microphone audio in this state.
#[cfg(target_os = "macos")]
fn permission_error(status: av::AuthorizationStatus) -> Option<SplitStreamError> {
    match status {
        av::AuthorizationStatus::Denied | av::AuthorizationStatus::Restricted => {
            Some(SplitStreamError::PermissionDenied {
                what: PermissionKind::Microphone,
            })
        }
        _ => None,
    }
}

/// Ask TCC whether this process may record audio.
///
/// cpal cannot answer this: when macOS has denied microphone access, building
/// and playing the stream still succeeds and the callback simply receives
/// silence forever. Checking the authorization status directly is the only way
/// the denial becomes an error instead of an unexplained silent channel.
#[cfg(target_os = "macos")]
fn check_permission() -> Result<(), SplitStreamError> {
    // An ObjC exception here is a diagnostic failure, not a permission
    // failure — fall through and let cpal try.
    let Ok(status) = av::CaptureDevice::authorization_status_for_media_type(av::MediaType::audio())
    else {
        return Ok(());
    };
    match permission_error(status) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Windows has no status to query up front.
///
/// The microphone is gated under Settings › Privacy & security › Microphone,
/// and a refusal surfaces as a cpal build-or-play error, which `setup` already
/// reports as [`SplitStreamError::MicCaptureFailed`]. System-audio loopback is
/// not gated at all. So unlike macOS there is nothing to pre-check, and the
/// prompt-preserving `NotDetermined` distinction does not arise.
#[cfg(not(target_os = "macos"))]
fn check_permission() -> Result<(), SplitStreamError> {
    Ok(())
}

/// Set up the default input device and return a live mic stream.
pub(crate) fn setup() -> Result<MicStream, SplitStreamError> {
    check_permission()?;

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or(SplitStreamError::NoMicDevice)?;

    let cfg = device
        .default_input_config()
        .map_err(|e| SplitStreamError::MicCaptureFailed {
            stage: "query default input config",
            source: Box::new(e),
        })?;
    let sample_rate = cfg.sample_rate();
    let channels = cfg.channels() as usize;
    let stream_config: StreamConfig = cfg.config();

    let rb = HeapRb::<f32>::new(RING_CAPACITY);
    let (prod, cons) = rb.split();
    let dropped = Arc::new(AtomicU64::new(0));

    let stream = device
        .build_input_stream(
            &stream_config,
            make_callback(channels, prod, Arc::clone(&dropped)),
            on_stream_error,
            None,
        )
        .map_err(|e| SplitStreamError::MicCaptureFailed {
            stage: "build input stream",
            source: Box::new(e),
        })?;

    stream
        .play()
        .map_err(|e| SplitStreamError::MicCaptureFailed {
            stage: "start input stream",
            source: Box::new(e),
        })?;

    Ok(MicStream {
        sample_rate,
        stream,
        consumer: cons,
        dropped_samples: dropped,
    })
}

/// Whether the mic stream must be torn down and rebuilt.
///
/// `observed` is the default input device's current sample rate, or `None`
/// when the device has gone away or its configuration could not be read — in
/// which case rebuilding would fail anyway, so the existing stream is kept.
fn should_rebuild(observed: Option<u32>, active: u32) -> bool {
    matches!(observed, Some(rate) if rate != active)
}

/// Poll the default input device config. If the sample rate has changed,
/// rebuild the cpal stream (new ring buffer + new stream) and update `mic` in
/// place. Returns `true` if a rebuild occurred.
pub(crate) fn maybe_rebuild(mic: &mut MicStream) -> bool {
    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        return false;
    };
    let Ok(cfg) = device.default_input_config() else {
        return false;
    };

    let current_rate = cfg.sample_rate();
    if !should_rebuild(Some(current_rate), mic.sample_rate) {
        return false;
    }

    let channels = cfg.channels() as usize;
    let stream_config: StreamConfig = cfg.config();
    let rb = HeapRb::<f32>::new(RING_CAPACITY);
    let (prod, cons) = rb.split();
    let dropped = Arc::clone(&mic.dropped_samples);

    match device.build_input_stream(
        &stream_config,
        make_callback(channels, prod, dropped),
        on_stream_error,
        None,
    ) {
        Ok(stream) => {
            info!(
                "Mic rate change: {}Hz → {}Hz, rebuilding stream",
                mic.sample_rate, current_rate
            );
            // Start the new stream before publishing it, so a failure to start
            // leaves the old (already invalidated) stream in place and reports
            // "no rebuild" rather than panicking on the engine's tick thread.
            if let Err(e) = stream.play() {
                error!("Failed to start rebuilt mic stream: {e}");
                return false;
            }
            mic.stream = stream;
            mic.consumer = cons;
            mic.sample_rate = current_rate;
            true
        }
        Err(e) => {
            error!("Failed to rebuild mic stream: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- downmix ---

    #[test]
    fn downmix_averages_stereo_pairs() {
        let data = [1.0, 0.0, 0.5, 0.5, -1.0, 1.0];
        let mut scratch = [0.0f32; 8];
        assert_eq!(downmix_into(&data, 2, &mut scratch), 3);
        assert_eq!(&scratch[..3], &[0.5, 0.5, 0.0]);
    }

    #[test]
    fn downmix_averages_all_channels() {
        // 4 channels, one frame: mean of 1,2,3,4 = 2.5
        let data = [1.0, 2.0, 3.0, 4.0];
        let mut scratch = [0.0f32; 4];
        assert_eq!(downmix_into(&data, 4, &mut scratch), 1);
        assert_eq!(scratch[0], 2.5);
    }

    #[test]
    fn downmix_truncates_to_scratch_and_never_allocates() {
        let data = vec![1.0f32; 64];
        let mut scratch = [0.0f32; 4];
        assert_eq!(downmix_into(&data, 2, &mut scratch), 4);
        assert!(scratch.iter().all(|&v| v == 1.0));
    }

    #[test]
    fn downmix_of_a_partial_trailing_frame_drops_it() {
        // 5 samples over 2 channels = 2 whole frames, half a frame discarded.
        let data = [1.0, 1.0, 2.0, 2.0, 3.0];
        let mut scratch = [0.0f32; 8];
        assert_eq!(downmix_into(&data, 2, &mut scratch), 2);
        assert_eq!(&scratch[..2], &[1.0, 2.0]);
    }

    #[test]
    fn downmix_of_empty_input_writes_nothing() {
        let mut scratch = [7.0f32; 4];
        assert_eq!(downmix_into(&[], 2, &mut scratch), 0);
        assert_eq!(scratch, [7.0; 4]);
    }

    // --- rebuild decision ---

    #[test]
    fn rebuild_only_when_the_rate_actually_changed() {
        assert!(should_rebuild(Some(44_100), 48_000));
        assert!(!should_rebuild(Some(48_000), 48_000));
    }

    #[test]
    fn no_rebuild_when_the_device_is_unreadable() {
        // Device gone or `default_input_config` failed: rebuilding would fail
        // too, so the (possibly stale) stream is kept rather than dropped.
        assert!(!should_rebuild(None, 48_000));
    }

    // --- permission mapping (macOS TCC only) ---

    #[cfg(target_os = "macos")]
    #[test]
    fn denied_and_restricted_map_to_permission_denied() {
        for status in [
            av::AuthorizationStatus::Denied,
            av::AuthorizationStatus::Restricted,
        ] {
            assert!(matches!(
                permission_error(status),
                Some(SplitStreamError::PermissionDenied {
                    what: PermissionKind::Microphone
                })
            ));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn authorized_and_not_determined_are_not_errors() {
        // NotDetermined must not error: the TCC prompt only appears once the
        // stream is actually built.
        assert!(permission_error(av::AuthorizationStatus::Authorized).is_none());
        assert!(permission_error(av::AuthorizationStatus::NotDetermined).is_none());
    }

    #[test]
    fn checking_permission_does_not_panic() {
        // On Windows this is the trivial `Ok(())`; on macOS it crosses into ObjC.
        // Exercises the real ObjC call; the result depends on the machine's
        // TCC state, so only the "no unwind, no exception" property is asserted.
        let _ = check_permission();
    }
}
