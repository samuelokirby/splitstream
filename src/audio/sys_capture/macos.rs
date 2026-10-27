//! macOS system-audio capture: a CoreAudio process tap, via cidre.
//!
//! Creates a global process tap and wraps it in an aggregate device that uses
//! the default input device (mic) as the clock source. This avoids the sample
//! rate negotiation timeout that plagues virtual/Bluetooth/AirPlay output devices
//! when using cpal's loopback approach. The tap's ASBD gives the authoritative
//! sample rate; no guessing required.
//!
//! Only sys audio (the last buffer in the IOProc) is pushed to the ring buffer.
//! Mic audio (buffer 0) is ignored — cpal handles mic capture separately.
//!
//! The IOProc runs on a real-time CoreAudio thread: it must not allocate, must
//! not lock, must not format strings, and must never unwind across the FFI
//! boundary. Diagnostics are recorded into atomics and reported by the engine.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ca::aggregate_device_keys as agg_keys;
use ca::sub_device_keys as sub_keys;
use cidre::{cat, cf, core_audio as ca, ns, os};
use log::info;
use ringbuf::traits::{Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use super::{RING_CAPACITY, SCRATCH_FRAMES, downmix_stereo_into};
use crate::error::{MIN_MACOS_VERSION, SplitStreamError, tap_status_error};

struct IoProcCtx {
    sys_producer: HeapProd<f32>,
    /// Preallocated stereo→mono downmix scratch. Allocated once at setup so the
    /// real-time callback never touches the allocator.
    mono_scratch: Vec<f32>,
    /// Samples the ring buffer refused because the engine had not drained it.
    dropped: Arc<AtomicU64>,
}

/// Owns the CoreAudio tap + aggregate device lifecycle.
/// Drop order matters: `started_device` must be dropped before `_ctx`
/// to ensure the IOProc stops running before its context is freed.
pub struct SysAudioTap {
    /// Authoritative sample rate from the tap's ASBD.
    pub sample_rate: u32,
    /// Running count of samples dropped because the ring buffer was full.
    /// Written by the real-time callback, read by the engine.
    pub dropped_samples: Arc<AtomicU64>,
    _tap: ca::TapGuard,
    started_device: Option<ca::hardware::StartedDevice<ca::AggregateDevice>>,
    _ctx: Option<Box<IoProcCtx>>,
}

/// Returns `Err(UnsupportedOsVersion)` when the running macOS predates the
/// Core Audio process-tap API.
///
/// The floor is macOS 14.2, taken from the SDK annotation on
/// `AudioHardwareCreateProcessTap` (`CoreAudio/AudioHardwareTapping.h`:
/// `API_AVAILABLE(macos(14.2))`). The `initMonoGlobalTapButExcludeProcesses:`
/// initializer this module uses carries no availability annotation narrower
/// than its class, so the mono global tap does not raise the floor.
fn check_os_version() -> Result<(), SplitStreamError> {
    let v = ns::ProcessInfo::current().os_version();
    let (major, minor) = (v.major as i64, v.minor as i64);
    if (major, minor) < MIN_MACOS_VERSION {
        return Err(SplitStreamError::UnsupportedOsVersion {
            found: format!("{}.{}.{}", v.major, v.minor, v.patch),
        });
    }
    Ok(())
}

/// Index of the buffer carrying system-tap audio.
///
/// The aggregate device supplies one `AudioBuffer` per sub-device in
/// declaration order: buffer 0 is the microphone (cpal handles that
/// separately) and the **last** buffer is the tap. With a single sub-device
/// the tap is buffer 0, so "last" is the only rule that holds in both shapes.
fn tap_buffer_index(n_bufs: usize) -> usize {
    n_bufs.saturating_sub(1)
}

/// Whole frames in a buffer holding `data_bytes` of interleaved `f32` across
/// `channels`.
///
/// Returns 0 for a zero-channel buffer — CoreAudio has been observed to hand
/// out empty buffers during device reconfiguration, and dividing by that
/// channel count on the real-time thread would abort the process.
fn frame_count(data_bytes: usize, channels: usize) -> usize {
    let total = data_bytes / std::mem::size_of::<f32>();
    total.checked_div(channels).unwrap_or(0)
}

impl SysAudioTap {
    /// Creates the tap and starts capture.
    /// Returns `(tap, sys_consumer)` where `sys_consumer` yields mono f32 samples
    /// at `tap.sample_rate`.
    pub fn new() -> Result<(Self, HeapCons<f32>), SplitStreamError> {
        check_os_version()?;

        // Get the default input device — used as the aggregate clock source so we
        // never need to renegotiate the output device's hardware sample rate.
        let input_device = ca::System::default_input_device()
            .map_err(|_| SplitStreamError::NoDefaultInputDevice)?;
        let input_uid = input_device
            .uid()
            .map_err(|e| tap_status_error("input device UID", e.0.get()))?;

        let mic_sub =
            cf::DictionaryOf::with_keys_values(&[sub_keys::uid()], &[input_uid.as_type_ref()]);

        // Global tap — captures all processes' output.
        let tap_desc = ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
        let tap = tap_desc
            .create_process_tap()
            .map_err(|e| tap_status_error("create process tap", e.0.get()))?;

        // Authoritative sample rate — no guessing.
        let asbd = tap
            .asbd()
            .map_err(|e| tap_status_error("tap ASBD", e.0.get()))?;
        let sample_rate = asbd.sample_rate as u32;
        info!("Sys audio tap sample rate: {}hz", sample_rate);

        let tap_uid = tap
            .uid()
            .map_err(|e| tap_status_error("tap UID", e.0.get()))?;
        let sub_tap =
            cf::DictionaryOf::with_keys_values(&[sub_keys::uid()], &[tap_uid.as_type_ref()]);

        // Aggregate device: mic is main sub-device (clock), tap is the sys audio source.
        let agg_desc = cf::DictionaryOf::with_keys_values(
            &[
                agg_keys::is_private(),
                agg_keys::is_stacked(),
                agg_keys::tap_auto_start(),
                agg_keys::name(),
                agg_keys::main_sub_device(),
                agg_keys::uid(),
                agg_keys::sub_device_list(),
                agg_keys::tap_list(),
            ],
            &[
                cf::Boolean::value_true().as_type_ref(),
                cf::Boolean::value_false(),
                cf::Boolean::value_false(),
                cf::str!(c"splitstream-sys-tap"),
                &input_uid,
                &cf::Uuid::new().to_cf_string(),
                &cf::ArrayOf::from_slice(&[mic_sub.as_ref(), sub_tap.as_ref()]),
                &cf::ArrayOf::from_slice(&[sub_tap.as_ref()]),
            ],
        );

        let sys_rb = HeapRb::<f32>::new(RING_CAPACITY);
        let (sys_producer, sys_consumer) = sys_rb.split();

        let dropped = Arc::new(AtomicU64::new(0));
        let mut ctx = Box::new(IoProcCtx {
            sys_producer,
            mono_scratch: vec![0.0; SCRATCH_FRAMES],
            dropped: Arc::clone(&dropped),
        });

        // IOProc: the aggregate device provides one AudioBuffer per sub-device.
        // Buffer 0 = mic (ignored — cpal handles mic).
        // Last buffer = sys tap audio (mono f32 from the process tap).
        //
        // Real-time thread: no allocation, no logging, no panics. Returning
        // `NO_ERR` on a bad context is the only correct action — unwinding
        // across this `extern "C"` boundary is undefined behaviour.
        extern "C" fn proc(
            _device: ca::Device,
            _now: &cat::AudioTimeStamp,
            input_data: &cat::AudioBufList<2>,
            _input_time: &cat::AudioTimeStamp,
            _output_data: &mut cat::AudioBufList<2>,
            _output_time: &cat::AudioTimeStamp,
            ctx: Option<&mut IoProcCtx>,
        ) -> os::Status {
            let Some(ctx) = ctx else {
                return os::Status::NO_ERR;
            };
            // Split the borrow so the producer and the scratch buffer can be
            // held mutably/immutably at the same time.
            let IoProcCtx {
                sys_producer,
                mono_scratch,
                dropped,
            } = &mut *ctx;
            let last_idx = tap_buffer_index(input_data.buffers.len());
            for (i, buf) in input_data.buffers.iter().enumerate() {
                // Always process only the last buffer (sys tap).
                // When there are 2 buffers: buffer 0 = mic (skip), buffer 1 = tap (process).
                // When there is 1 buffer: buffer 0 = tap (process).
                if i != last_idx {
                    continue;
                }
                let ch = buf.number_channels as usize;
                let total = buf.data_bytes_size as usize / std::mem::size_of::<f32>();
                let frames = frame_count(buf.data_bytes_size as usize, ch);
                if frames == 0 || buf.data.is_null() {
                    continue;
                }
                let raw = unsafe { std::slice::from_raw_parts(buf.data as *const f32, total) };
                let (pushed, offered) = if ch == 2 {
                    // Stereo → mix to mono, into the preallocated scratch.
                    let frames = downmix_stereo_into(raw, frames, mono_scratch);
                    let n = sys_producer.push_slice(&mono_scratch[..frames]);
                    (n, frames)
                } else {
                    let n = sys_producer.push_slice(raw);
                    (n, raw.len())
                };
                if pushed < offered {
                    dropped.fetch_add((offered - pushed) as u64, Ordering::Relaxed);
                }
            }
            os::Status::NO_ERR
        }

        let agg_device = ca::AggregateDevice::with_desc(&agg_desc)
            .map_err(|e| tap_status_error("create aggregate device", e.0.get()))?;

        let proc_id = agg_device
            .create_io_proc_id(proc, Some(&mut ctx))
            .map_err(|e| tap_status_error("create IOProc", e.0.get()))?;

        let started_device = ca::device_start(agg_device, Some(proc_id))
            .map_err(|e| tap_status_error("start device", e.0.get()))?;

        Ok((
            SysAudioTap {
                sample_rate,
                dropped_samples: dropped,
                _tap: tap,
                started_device: Some(started_device),
                _ctx: Some(ctx),
            },
            sys_consumer,
        ))
    }
}

impl Drop for SysAudioTap {
    fn drop(&mut self) {
        // Stop the device first so the IOProc can't run after ctx is freed.
        self.started_device.take();
        self._ctx.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- buffer selection ---

    #[test]
    fn two_buffers_selects_the_tap_not_the_mic() {
        // mic = 0, tap = 1
        assert_eq!(tap_buffer_index(2), 1);
    }

    #[test]
    fn one_buffer_selects_buffer_zero() {
        assert_eq!(tap_buffer_index(1), 0);
    }

    #[test]
    fn zero_buffers_does_not_underflow() {
        // `n_bufs - 1` on an empty buffer list would wrap to usize::MAX and
        // index out of bounds on the real-time thread.
        assert_eq!(tap_buffer_index(0), 0);
    }

    // --- frame arithmetic ---

    #[test]
    fn frame_count_divides_bytes_by_channels() {
        // 512 bytes = 128 f32 = 64 stereo frames
        assert_eq!(frame_count(512, 2), 64);
        assert_eq!(frame_count(512, 1), 128);
    }

    #[test]
    fn frame_count_of_zero_channels_is_zero_not_a_divide_by_zero() {
        assert_eq!(frame_count(512, 0), 0);
    }

    #[test]
    fn frame_count_of_an_empty_buffer_is_zero() {
        assert_eq!(frame_count(0, 2), 0);
    }

    #[test]
    fn frame_count_ignores_a_partial_trailing_frame() {
        // 12 bytes = 3 f32 = 1 whole stereo frame
        assert_eq!(frame_count(12, 2), 1);
    }
}
