use std::{fs::File, time::Duration};

use fixed_resample::rubato::{Resampler, SincFixedOut, SincInterpolationParameters};
use opus::Encoder;
use ringbuf::{
    HeapRb, SharedRb,
    traits::{Consumer, Observer, Producer, Split},
};

use crate::websocket_client::WebSocketClient;

pub mod audio_input_buffers;
pub mod macos_device;
pub mod websocket_client;

fn main() {
    let mut dev = macos_device::OSXInputDevice::new().unwrap();
    println!(
        "Aggregate device initialized with {}hz nominal sample rate",
        dev.nominal_sample_rate
    );
    let in_sample_rate = dev.nominal_sample_rate | 16_000;
    let (mut mic_consumer, mut sys_consumer) = dev.start_capture().unwrap();
    let mut resampler = build_resampler(in_sample_rate);
    let mut input_buffers = resampler.input_buffer_allocate(false);

    println!("Resampler channels: {}", resampler.nbr_channels());
    println!(
        "Next input frame size: {}",
        Resampler::input_frames_next(&resampler)
    );
    println!("Starting in three seconds...");
    std::thread::sleep(Duration::from_secs(3));

    let rb = HeapRb::<f32>::new(4096);
    let (mut resample_prod, mut resample_cons) = rb.split();
    let mut encoder = build_encoder(16_000); // Encode at 16kHz
    let ws_client: WebSocketClient = WebSocketClient::new(
        "your_access_token".to_string(),
        "ws://localhost:8080/audio/stream".to_string(),
    );
    loop {
        // 1. Read from aggregate device ring buffers
        let mut mic_buffer = [0.0_f32; 512];
        let mut sys_buffer = [0.0_f32; 512];
        let mic_read = mic_consumer.pop_slice(&mut mic_buffer);
        let sys_read = sys_consumer.pop_slice(&mut sys_buffer);
        if mic_read > 0 || sys_read > 0 {
            println!(
                "{:3} 🎙️ + {:3} 📣 = {:4} total",
                mic_read,
                sys_read,
                mic_read + sys_read
            );
            input_buffers[0].extend_from_slice(&mic_buffer[..mic_read]);
            input_buffers[1].extend_from_slice(&sys_buffer[..sys_read]);
        }
        if mic_read > 0 {
            input_buffers[0].extend_from_slice(&mic_buffer[..mic_read]);
        }
        if sys_read > 0 {
            input_buffers[1].extend_from_slice(&sys_buffer[..sys_read]);
        }
        let required_input = Resampler::input_frames_next(&resampler);
        if input_buffers[0].len() >= required_input && input_buffers[1].len() >= required_input {
            // inputs: one slice per channel
            let wave_in = [
                &input_buffers[0][..required_input],
                &input_buffers[1][..required_input],
            ];

            // outputs: one mutable slice per channel
            let mut out_mic = [0.0_f32; 512];
            let mut out_sys = [0.0_f32; 512];
            let mut wave_out = [&mut out_mic[..], &mut out_sys[..]];

            // mask: both channels active
            let active = [true, true];

            let result = Resampler::process_into_buffer(
                &mut resampler,
                &wave_in,
                &mut wave_out,
                Some(&active),
            );
            println!("Resample result: {:?}", result);

            // Remove processed data
            input_buffers[0].drain(0..required_input);
            input_buffers[1].drain(0..required_input);

            if let Ok(_) = result {
                // Push resampled data to ring buffer
                for &sample in out_mic.iter() {
                    resample_prod.push_slice(&[sample]);
                }
                for &sample in out_sys.iter() {
                    resample_prod.push_slice(&[sample]);
                }

                // Encode when enough data for a frame (e.g., 320 samples at 16kHz for ~20ms)
                let frame_sz = frame_size(16_000.0);
                if resample_cons.occupied_len() >= frame_sz * 2 {
                    // Stereo
                    let mut frame = vec![0.0_f32; frame_sz * 2];
                    // Pop all required samples into the frame buffer
                    let popped = resample_cons.pop_slice(&mut frame[..]);
                    if popped == frame.len() {
                        let mut encoded = vec![0u8; 1024];
                        let len = encoder.encode_float(&frame, &mut encoded).unwrap();
                        println!("Encoded frame size: {}", len);
                        // TODO: Send or save encoded[..len]
                    }
                }
            }
        }
    }
}

fn build_resampler(in_sample_rate: u32) -> SincFixedOut<f32> {
    let out_sample_rate = 16_000;
    let resample_ratio = out_sample_rate as f64 / in_sample_rate as f64; // e.g. 48000 / 44100 = 1.088435
    let max_resample_ratio_relative = 3.1; // Allow for some variance in sample rate

    let parameters = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        oversampling_factor: 128,
        interpolation: fixed_resample::rubato::SincInterpolationType::Linear,
        window: fixed_resample::rubato::WindowFunction::BlackmanHarris2,
    };

    let resampler = SincFixedOut::<f32>::new(
        resample_ratio,
        max_resample_ratio_relative,
        parameters,
        320,
        2,
    )
    .expect("Failed to create resampler");

    resampler
}

fn build_encoder(in_sample_rate: u32) -> Encoder {
    let application = opus::Application::Audio;
    let mut encoder = Encoder::new(in_sample_rate, opus::Channels::Stereo, application)
        .expect("Failed to create Opus encoder");
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(32_000));
    encoder
}

fn frame_size(sample_rate: f64) -> usize {
    const FRAME_MS: f64 = 20.0;
    ((sample_rate * FRAME_MS) / 1000.0).round() as usize
}
