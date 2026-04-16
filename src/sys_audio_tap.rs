// src/sys_audio_tap.rs
//! Captures system audio output via a CoreAudio process tap using cidre.
//!
//! Creates a global process tap and wraps it in an aggregate device that uses
//! the default input device (mic) as the clock source. This avoids the sample
//! rate negotiation timeout that plagues virtual/Bluetooth/AirPlay output devices
//! when using cpal's loopback approach. The tap's ASBD gives the authoritative
//! sample rate; no guessing required.
//!
//! Only sys audio (the last buffer in the IOProc) is pushed to the ring buffer.
//! Mic audio (buffer 0) is ignored — cpal handles mic capture separately.

use ca::aggregate_device_keys as agg_keys;
use ca::sub_device_keys as sub_keys;
use cidre::{cat, cf, core_audio as ca, ns, os};
use log::{info, trace};
use ringbuf::traits::{Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

struct IoProcCtx {
    sys_producer: HeapProd<f32>,
    has_logged: bool,
}

/// Owns the CoreAudio tap + aggregate device lifecycle.
/// Drop order matters: `started_device` must be dropped before `_ctx`
/// to ensure the IOProc stops running before its context is freed.
pub struct SysAudioTap {
    /// Authoritative sample rate from the tap's ASBD.
    pub sample_rate: u32,
    _tap: ca::TapGuard,
    started_device: Option<ca::hardware::StartedDevice<ca::AggregateDevice>>,
    _ctx: Option<Box<IoProcCtx>>,
}

impl SysAudioTap {
    /// Creates the tap and starts capture.
    /// Returns `(tap, sys_consumer)` where `sys_consumer` yields mono f32 samples
    /// at `tap.sample_rate`.
    pub fn new() -> Result<(Self, HeapCons<f32>), String> {
        // Get the default input device — used as the aggregate clock source so we
        // never need to renegotiate the output device's hardware sample rate.
        let input_device =
            ca::System::default_input_device().expect("no default input device");
        let input_uid = input_device
            .uid()
            .map_err(|e| format!("input device UID: {:?}", e))?;

        let mic_sub = cf::DictionaryOf::with_keys_values(
            &[sub_keys::uid()],
            &[input_uid.as_type_ref()],
        );

        // Global tap — captures all processes' output.
        let tap_desc =
            ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
        let tap = tap_desc
            .create_process_tap()
            .map_err(|e| format!("create process tap: {:?}", e))?;

        // Authoritative sample rate — no guessing.
        let asbd = tap
            .asbd()
            .map_err(|e| format!("tap ASBD: {:?}", e))?;
        let sample_rate = asbd.sample_rate as u32;
        info!("Sys audio tap sample rate: {}hz", sample_rate);

        let tap_uid = tap
            .uid()
            .map_err(|e| format!("tap UID: {:?}", e))?;
        let sub_tap = cf::DictionaryOf::with_keys_values(
            &[sub_keys::uid()],
            &[tap_uid.as_type_ref()],
        );

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

        let sys_rb = HeapRb::<f32>::new(8192);
        let (sys_producer, sys_consumer) = sys_rb.split();

        let mut ctx = Box::new(IoProcCtx { sys_producer, has_logged: false });

        // IOProc: the aggregate device provides one AudioBuffer per sub-device.
        // Buffer 0 = mic (ignored — cpal handles mic).
        // Last buffer = sys tap audio (mono f32 from the process tap).
        // Raw pointer access matches the original working implementation.
        extern "C" fn proc(
            _device: ca::Device,
            _now: &cat::AudioTimeStamp,
            input_data: &cat::AudioBufList<2>,
            _input_time: &cat::AudioTimeStamp,
            _output_data: &mut cat::AudioBufList<2>,
            _output_time: &cat::AudioTimeStamp,
            ctx: Option<&mut IoProcCtx>,
        ) -> os::Status {
            let ctx = ctx.unwrap();
            let n_bufs = input_data.buffers.len();
            if !ctx.has_logged {
                println!("[tap IOProc] buffers in list: {}", n_bufs);
                for (i, b) in input_data.buffers.iter().enumerate() {
                    println!(
                        "  buf[{}] channels={} bytes={} null={}",
                        i, b.number_channels, b.data_bytes_size, b.data.is_null()
                    );
                }
                ctx.has_logged = true;
            }
            let last_idx = n_bufs.saturating_sub(1);
            for (i, buf) in input_data.buffers.iter().enumerate() {
                // Always process only the last buffer (sys tap).
                // When there are 2 buffers: buffer 0 = mic (skip), buffer 1 = tap (process).
                // When there is 1 buffer: buffer 0 = tap (process).
                if i != last_idx {
                    continue;
                }
                let ch = buf.number_channels as usize;
                let total = buf.data_bytes_size as usize / std::mem::size_of::<f32>();
                let frames = if ch > 0 { total / ch } else { 0 };
                if frames == 0 || buf.data.is_null() {
                    continue;
                }
                let raw = unsafe { std::slice::from_raw_parts(buf.data as *const f32, total) };
                if ch == 2 {
                    // Stereo → mix to mono
                    let mono: Vec<f32> = (0..frames)
                        .map(|f| (raw[f * 2] + raw[f * 2 + 1]) / 2.0)
                        .collect();
                    trace!("* 🔵🔊 sys {} samples (stereo→mono)", mono.len());
                    let _ = ctx.sys_producer.push_slice(&mono);
                } else {
                    trace!("* 🔵🔊 sys {} samples", raw.len());
                    let _ = ctx.sys_producer.push_slice(raw);
                }
            }
            os::Status::NO_ERR
        }

        let agg_device = ca::AggregateDevice::with_desc(&agg_desc)
            .map_err(|e| format!("create aggregate device: {:?}", e))?;

        let proc_id = agg_device
            .create_io_proc_id(proc, Some(&mut ctx))
            .map_err(|e| format!("create IOProc: {:?}", e))?;

        let started_device = ca::device_start(agg_device, Some(proc_id))
            .map_err(|e| format!("start device: {:?}", e))?;

        Ok((
            SysAudioTap {
                sample_rate,
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
