//! cpal microphone capture with mid-stream sample-rate adaptation.
//!
//! When macOS changes a device's sample rate, CoreAudio invalidates the cpal
//! stream and callbacks stop firing. `maybe_rebuild` polls the device config
//! every second and rebuilds the entire stream (new ring buffer + new cpal
//! stream) when the rate changes.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;
use log::info;
use ringbuf::traits::{Producer, Split};
use ringbuf::{HeapCons, HeapRb};

pub(crate) struct MicStream {
    pub sample_rate: u32,
    pub stream: cpal::Stream,
    pub consumer: HeapCons<f32>,
}

/// Set up the default input device and return a live mic stream.
pub(crate) fn setup() -> Result<MicStream, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "no default input device".to_string())?;

    let cfg = device
        .default_input_config()
        .map_err(|e| e.to_string())?;
    let sample_rate = cfg.sample_rate();
    let channels = cfg.channels() as usize;
    let stream_config: StreamConfig = cfg.config();

    let rb = HeapRb::<f32>::new(8192);
    let (mut prod, cons) = rb.split();

    let stream = device
        .build_input_stream(
            &stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if channels == 1 {
                    prod.push_slice(data);
                } else {
                    let mono: Vec<f32> = data
                        .chunks(channels)
                        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                        .collect();
                    prod.push_slice(&mono);
                }
            },
            |err| eprintln!("Mic stream error: {err}"),
            None,
        )
        .map_err(|e| e.to_string())?;

    stream.play().map_err(|e| e.to_string())?;

    Ok(MicStream { sample_rate, stream, consumer: cons })
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
    if current_rate == mic.sample_rate {
        return false;
    }

    let channels = cfg.channels() as usize;
    let stream_config: StreamConfig = cfg.config();
    let rb = HeapRb::<f32>::new(8192);
    let (mut prod, cons) = rb.split();

    match device.build_input_stream(
        &stream_config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            if channels == 1 {
                prod.push_slice(data);
            } else {
                let mono: Vec<f32> = data
                    .chunks(channels)
                    .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                    .collect();
                prod.push_slice(&mono);
            }
        },
        |err| eprintln!("Mic stream error: {err}"),
        None,
    ) {
        Ok(stream) => {
            info!(
                "Mic rate change: {}Hz → {}Hz, rebuilding stream",
                mic.sample_rate, current_rate
            );
            mic.stream = stream;
            mic.consumer = cons;
            mic.sample_rate = current_rate;
            mic.stream.play().expect("failed to start mic stream");
            true
        }
        Err(e) => {
            eprintln!("Failed to rebuild mic stream: {e}");
            false
        }
    }
}
