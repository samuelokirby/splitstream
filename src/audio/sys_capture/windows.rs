//! Windows system-audio capture: WASAPI loopback, through cpal.
//!
//! Windows has no equivalent of the macOS process tap, but it does not need
//! one: WASAPI can hand back whatever a render device is playing. cpal reaches
//! that by a quiet convention — opening an **output** device for *input* sets
//! `AUDCLNT_STREAMFLAGS_LOOPBACK` for you — so capturing the speakers is
//! `default_output_device()` followed by `build_input_stream`, and splitstream
//! needs no Windows-specific dependency at all.
//!
//! Two consequences are worth knowing:
//!
//! * **Loopback needs no permission.** Unlike the macOS tap, which is gated
//!   behind TCC consent, nothing prompts the user and nothing can refuse. Only
//!   the microphone is gated on Windows, under Settings › Privacy & security ›
//!   Microphone.
//! * **The capture format is the render device's mix format**, whatever the
//!   user's hardware negotiated. That is `f32` on most machines but routinely
//!   `i16`, and routinely wider than stereo, so the sample format and channel
//!   count are both read from the device rather than assumed.
//!
//! The data callback runs on an OS audio thread, so it must not allocate, lock,
//! or format strings. The downmix scratch is allocated once at setup and
//! dropped samples are counted into an atomic the engine reports.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, warn};
use ringbuf::traits::{Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use super::{RING_CAPACITY, SCRATCH_FRAMES, downmix_into};
use crate::error::SplitStreamError;

/// Owns the loopback stream and reports what it captured.
///
/// Mirrors the macOS [`SysAudioTap`](super::macos::SysAudioTap) surface — a
/// `sample_rate`, a `dropped_samples` counter, and a consumer of mono `f32` —
/// so the engine does not know which platform it is running on.
pub struct SysAudioTap {
    /// The render device's mix sample rate, read from the device rather than
    /// guessed. The engine resamples from this to 16kHz.
    pub sample_rate: u32,
    /// Running count of samples dropped because the ring buffer was full.
    /// Written by the audio callback, read by the engine.
    pub dropped_samples: Arc<AtomicU64>,
    /// Dropping this stops capture, so it is owned here even though nothing
    /// reads it.
    _stream: cpal::Stream,
}

fn capture_error<E>(stage: &'static str, source: E) -> SplitStreamError
where
    E: std::error::Error + Send + Sync + 'static,
{
    SplitStreamError::SysCaptureFailed {
        stage,
        source: Box::new(source),
    }
}

impl SysAudioTap {
    /// Starts loopback capture on the default output device.
    ///
    /// Returns `(tap, sys_consumer)` where `sys_consumer` yields mono `f32`
    /// samples at `tap.sample_rate`.
    pub fn new() -> Result<(Self, HeapCons<f32>), SplitStreamError> {
        let host = cpal::default_host();

        // The loopback convention: this is an *output* device, opened for input
        // below, which is what makes WASAPI give us what is being played.
        let device = host
            .default_output_device()
            .ok_or(SplitStreamError::NoDefaultOutputDevice)?;

        let supported = device
            .default_output_config()
            .map_err(|e| capture_error("query the output device's mix format", e))?;

        let sample_rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        info!(
            "Sys audio loopback: {}hz, {} channel(s), {:?}",
            sample_rate, channels, format
        );

        let (producer, consumer) = HeapRb::<f32>::new(RING_CAPACITY).split();
        let dropped = Arc::new(AtomicU64::new(0));

        let stream = build_loopback_stream(
            &device,
            &config,
            format,
            channels,
            producer,
            Arc::clone(&dropped),
        )?;

        stream
            .play()
            .map_err(|e| capture_error("start the loopback stream", e))?;

        Ok((
            Self {
                sample_rate,
                dropped_samples: dropped,
                _stream: stream,
            },
            consumer,
        ))
    }
}

/// Build the stream for whichever mix format the device reported.
///
/// Every arm is the same code over a different sample type; the format is a
/// runtime value, so this is where it becomes a type.
fn build_loopback_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    producer: HeapProd<f32>,
    dropped: Arc<AtomicU64>,
) -> Result<cpal::Stream, SplitStreamError> {
    use cpal::SampleFormat as F;
    match format {
        F::F32 => build::<f32>(device, config, channels, producer, dropped),
        F::F64 => build::<f64>(device, config, channels, producer, dropped),
        F::I16 => build::<i16>(device, config, channels, producer, dropped),
        F::I32 => build::<i32>(device, config, channels, producer, dropped),
        F::I64 => build::<i64>(device, config, channels, producer, dropped),
        F::I8 => build::<i8>(device, config, channels, producer, dropped),
        F::U8 => build::<u8>(device, config, channels, producer, dropped),
        F::U16 => build::<u16>(device, config, channels, producer, dropped),
        F::U32 => build::<u32>(device, config, channels, producer, dropped),
        F::U64 => build::<u64>(device, config, channels, producer, dropped),
        // `SampleFormat` is `#[non_exhaustive]`, and packed 24-bit formats have
        // no plain Rust primitive to convert through.
        other => Err(SplitStreamError::SysCaptureFailed {
            stage: "negotiate a supported mix format",
            source: format!(
                "the default output device reports {other:?} samples, which \
                 splitstream cannot convert; select a device with an f32 or \
                 i16 mix format in Sound Control Panel › Advanced"
            )
            .into(),
        }),
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mut producer: HeapProd<f32>,
    dropped: Arc<AtomicU64>,
) -> Result<cpal::Stream, SplitStreamError>
where
    T: cpal::SizedSample + 'static,
    f32: cpal::FromSample<T>,
{
    // Allocated once, here, so the callback never touches the allocator.
    let mut scratch = vec![0.0f32; SCRATCH_FRAMES];

    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let frames = downmix_into(data, channels, &mut scratch);
                let pushed = producer.push_slice(&scratch[..frames]);
                if pushed < frames {
                    dropped.fetch_add((frames - pushed) as u64, Ordering::Relaxed);
                }
            },
            // Logging from the error callback is safe: cpal calls it from its
            // own thread, not from the audio callback.
            |e| warn!("Sys audio loopback stream error: {e}"),
            None,
        )
        .map_err(|e| capture_error("build the loopback stream", e))
}
