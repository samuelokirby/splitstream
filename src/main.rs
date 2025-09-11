use chrono::Local;
use colored::*;
use fixed_resample::rubato::{Resampler, SincFixedOut, SincInterpolationParameters};
use log::debug;
use opus::{Decoder, Encoder};
use ringbuf::traits::Consumer;
use std::{
    io::{self},
    time::Duration,
};
use tokio::sync::mpsc;

use crate::{
    coreaudio_listener::{AudioPropertyChange, CoreAudioListener},
    transcript_msg::TranscriptMessage,
    websocket_client::WebSocketClient,
};

pub mod audio_input_buffers;
pub mod coreaudio_listener;
pub mod macos_device;
pub mod transcript_msg;
pub mod websocket_client;

#[tokio::main]
async fn main() {
    // Print startup message for console
    print_splitstream_demo_msg();
    // Start by initializing a new aggregate device based on the user's default devices
    let mut dev = macos_device::OSXInputDevice::new().unwrap();

    // Create the channels for Opus packets to be transmitted at the end of the loop
    let (opus_packet_tx, opus_packet_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // Create the channel for CoreAudio events to trigger resampler updates on device change (death, sample rate change)
    let (ca_tx, mut ca_rx) = mpsc::unbounded_channel::<AudioPropertyChange>();

    // Spawn CoreAudio listener task early to cover HFP bluetooth 16khz downsampling
    create_coreaudio_listener_task(ca_tx);

    // Initialize `in_sample_rate` which matches the system's OUTPUT device sample rate.
    let in_sample_rate = dev.nominal_sample_rate;

    // Start capturing audio from the aggregate device and get the ring buffer consumers
    let (mut mic_consumer, mut sys_consumer) = dev.start_capture().unwrap();
    // Build the resampler to convert from `in_sample_rate` to 16kHz for Opus encoding and transmission
    let mut resampler = build_resampler(in_sample_rate);
    // Allocate internal input buffers for the resampler
    let mut input_buffers = Resampler::input_buffer_allocate(&mut resampler, false);

    let access_token = "your_access_token".to_string();
    let ws_url = "wss://secretary-backend-649765884774.us-east4.run.app/audio/stream".to_string(); // "ws://localhost:8080/audio/stream".to_string
    // Create the WebSocketClient which opens a WS(S) connection to the backend and sends Opus frames
    let ws_client: WebSocketClient = WebSocketClient::new(access_token, ws_url);

    // Add channels for transcripts (to receive from the WebSocket server)
    let (transcript_tx, transcript_rx) = mpsc::unbounded_channel::<String>();

    // Spawn a task to handle WebSocket transmission (sends audio frames as they arrive)
    // combines everything to be able to use transmit_audio_frames
    create_websocket_task(ws_client, opus_packet_rx, transcript_tx);
    // Spawn a task to handle incoming transcripts (prints back to stdout as they arrive)
    create_transcript_stdout_task(transcript_rx);

    println!("Starting in 3...");
    std::thread::sleep(Duration::from_secs(1));
    println!("2...");
    std::thread::sleep(Duration::from_secs(1));
    println!("1...");
    std::thread::sleep(Duration::from_secs(1));

    let mut opus_packets = Vec::new();
    let confirm_msg = format!("✅ Recording. Press Ctrl+C to stop.")
        .green()
        .bold();
    println!("{}", confirm_msg);

    let mut encoder = build_encoder(16_000); // Encode at 16kHz
    loop {
        // Check for test messages
        if let Ok(event) = ca_rx.try_recv() {
            match event {
                AudioPropertyChange::ActualSampleRate { hz } => {
                    debug!("Nominal sample rate changed to {}", hz);
                    change_resampler_in_rate(&mut resampler, hz).unwrap();
                }
                _ => {}
            }
            debug!("CoreAudio Event: {:?}", event);
            if let Err(e) = dev.stop_capture() {
                eprintln!("Failed to stop capture: {e:?}");
            }
            std::thread::sleep(Duration::from_millis(500));
            match macos_device::OSXInputDevice::new() {
                Ok(new_dev) => {
                    println!(
                        "Switching clock device to new {}hz device",
                        new_dev.nominal_sample_rate
                    );
                    dev = new_dev;
                    match dev.start_capture() {
                        Ok((new_mic_consumer, new_sys_consumer)) => {
                            mic_consumer = new_mic_consumer;
                            sys_consumer = new_sys_consumer;
                            // Optional: clear any leftover buffered samples so timing stays aligned
                            input_buffers[0].clear();
                            input_buffers[1].clear();
                            let hz = dev.nominal_sample_rate;
                            change_resampler_in_rate(&mut resampler, hz).unwrap();
                        }
                        Err(e) => eprintln!("Failed to start capture: {e:?}"),
                    }
                }
                Err(e) => eprintln!("Failed to create new input device: {e:?}"),
            }
        }
        // 1. Read from aggregate device ring buffers
        let mut mic_buffer = [0.0_f32; 512];
        let mut sys_buffer = [0.0_f32; 512];
        let mic_read = mic_consumer.pop_slice(&mut mic_buffer);
        let sys_read = sys_consumer.pop_slice(&mut sys_buffer);

        if mic_read > 0 || sys_read > 0 {
            // println!(
            //     "{:3} 🎙️ + {:3} 📣 = {:4} total",
            //     mic_read,
            //     sys_read,
            //     mic_read + sys_read
            // );
            if mic_read > 0 {
                input_buffers[0].extend_from_slice(&mic_buffer[..mic_read]);
            }
            if sys_read > 0 {
                input_buffers[1].extend_from_slice(&sys_buffer[..sys_read]);
            }
        }

        let required_input = Resampler::input_frames_next(&resampler);
        if input_buffers[0].len() >= required_input && input_buffers[1].len() >= required_input {
            let wave_in = [
                &input_buffers[0][..required_input],
                &input_buffers[1][..required_input],
            ];
            let mut out_mic = [0.0_f32; 320]; // only need chunk_size
            let mut out_sys = [0.0_f32; 320];
            let mut wave_out = [&mut out_mic[..], &mut out_sys[..]];
            let active = [true, true];

            match Resampler::process_into_buffer(
                &mut resampler,
                &wave_in,
                &mut wave_out,
                Some(&active),
            ) {
                Ok((_used, produced)) => {
                    // Remove consumed input
                    input_buffers[0].drain(0..required_input);
                    input_buffers[1].drain(0..required_input);

                    // Interleave produced samples
                    let frame_len = produced; // 320
                    let mut interleaved = Vec::<f32>::with_capacity(frame_len * 2);
                    for i in 0..frame_len {
                        interleaved.push(out_mic[i]);
                        interleaved.push(out_sys[i]);
                    }

                    // Encode (Opus expects interleaved stereo)
                    let mut encoded = vec![0u8; 400]; // enough for 20ms @ low bitrate
                    let packet_len = encoder
                        .encode_float(&interleaved, &mut encoded)
                        .expect("Opus encode failed");
                    encoded.truncate(packet_len);
                    // println!("Encoded frame size: {}", packet_len);

                    opus_packets.push(encoded.clone());
                    let _ = opus_packet_tx.send(encoded);
                }
                Err(e) => {
                    eprintln!("Resample error: {e:?}");
                    // If recoverable, consider dropping some input or clearing buffers
                    input_buffers[0].clear();
                    input_buffers[1].clear();
                }
            }
        }
    }

    // After the loop, write raw Opus packets to file
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

fn decode_and_write_wav(opus_packets: &[Vec<u8>], output_path: &str) -> io::Result<()> {
    let sample_rate = 16_000;
    let channels = opus::Channels::Stereo;
    let mut decoder = Decoder::new(sample_rate, channels).expect("Failed to create Opus decoder");

    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(output_path, spec).unwrap();

    let mut decoded_buf = vec![0i16; 960 * 2]; // 960 samples = 20ms @ 48k, scaled to 16k later

    for packet in opus_packets {
        // Decode into PCM i16 samples
        match decoder.decode(packet, &mut decoded_buf, false) {
            Ok(num_samples) => {
                for sample in &decoded_buf[..num_samples * 2] {
                    let _ = writer.write_sample(*sample);
                }
            }
            Err(err) => eprintln!("Decode error: {:?}", err),
        }
    }

    let _ = writer.finalize();
    Ok(())
}

fn print_splitstream_demo_msg() {
    let banner = "
███████╗██████╗ ██╗     ██╗████████╗███████╗████████╗██████╗ ███████╗ █████╗ ███╗   ███╗
██╔════╝██╔══██╗██║     ██║╚══██╔══╝██╔════╝╚══██╔══╝██╔══██╗██╔════╝██╔══██╗████╗ ████║
███████╗██████╔╝██║     ██║   ██║   ███████╗   ██║   ██████╔╝█████╗  ███████║██╔████╔██║
╚════██║██╔═══╝ ██║     ██║   ██║   ╚════██║   ██║   ██╔══██╗██╔══╝  ██╔══██║██║╚██╔╝██║
███████║██║     ███████╗██║   ██║   ███████║   ██║   ██║  ██║███████╗██║  ██║██║ ╚═╝ ██║
╚══════╝╚═╝     ╚══════╝╚═╝   ╚═╝   ╚══════╝   ╚═╝   ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝

    ";
    println!("\n\n{}", banner.bright_blue());
    println!("\t\tDemo");
    println!("\t\t© Secretary Corporation 2024-2025, all rights reserved.")
}

fn change_resampler_in_rate(
    resampler: &mut SincFixedOut<f32>,
    new_in_rate: u32,
) -> Result<(), fixed_resample::rubato::ResampleError> {
    let new_ratio = 16_000.0 / new_in_rate as f64;
    Resampler::set_resample_ratio(resampler, new_ratio, false)
}

fn _frame_size(sample_rate: f64) -> usize {
    const FRAME_MS: f64 = 20.0;
    ((sample_rate * FRAME_MS) / 1000.0).round() as usize
}

/// Spawns the CoreAudio listener tokio task that monitors for device changes from `coreaudio_listener`.
/// Sends events to the provided an UnboundedSender with AudioPropertyChange.
fn create_websocket_task(
    ws_client: WebSocketClient,
    opus_packet_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    transcript_tx: mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        if let Err(e) = ws_client
            .transmit_audio_frames(opus_packet_rx, transcript_tx)
            .await
        {
            eprintln!("WebSocket transmission error: {}", e);
        }
    });
}

/// Spawns the CoreAudio listener tokio task that monitors for device changes from `coreaudio_listener`.
/// Sends events to the provided an UnboundedSender with AudioPropertyChange.
fn create_coreaudio_listener_task(ca_tx: mpsc::UnboundedSender<AudioPropertyChange>) {
    tokio::spawn(async move {
        let mut ca_listener = CoreAudioListener::new();
        let mut dev_change_rx = ca_listener.subscribe();
        ca_listener.start();
        // Listen for CoreAudio device change events
        while let Ok(event) = dev_change_rx.recv().await {
            // Send debug message with the CoreAudio event
            debug!(
                "[{}] CoreAudio event \"{:?}\" received.",
                Local::now()
                    .format("%a %-I:%M%p")
                    .to_string()
                    .to_lowercase(),
                event
            );
            // Handle full device swaps
            match event {
                AudioPropertyChange::DeviceIsAlive
                | AudioPropertyChange::HardwareDefaultInputDevice { .. }
                | AudioPropertyChange::HardwareDefaultOutputDevice { .. } => {
                    // Wait a moment for the system to stabilize
                    std::thread::sleep(Duration::from_millis(500));
                    // Rebuild synchronously on the main task
                    ca_listener.rebuild();
                }
                _ => {}
            }
            let _ = ca_tx.send(event);
        }
    });
}

/// Spawns a task to handle incoming transcripts (prints them as they arrive)
/// with color coding for final vs interim and mic vs sys channel.
fn create_transcript_stdout_task(mut transcript_rx: mpsc::UnboundedReceiver<String>) {
    // Spawn a task to handle incoming transcripts (prints them as they arrive)
    tokio::spawn(async move {
        while let Some(transcript) = transcript_rx.recv().await {
            let v: TranscriptMessage =
                serde_json::from_str::<TranscriptMessage>(&transcript).unwrap();

            // Base text
            let mut text = v.text.clone();

            // If final, color it dark green
            if v.is_final {
                text = text.green().bold().to_string();
                // you can also pick a darker RGB shade:
                // text = text.truecolor(0, 100, 0).to_string();
            }

            if v.channel == "microphone" {
                println!("🎙️(mic)\t{}", text.white());
            } else {
                println!("🔊(sys) {}", text.bright_black());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_size_common_rates() {
        assert_eq!(_frame_size(16_000.0), 320);
        assert_eq!(_frame_size(48_000.0), 960);
        assert_eq!(_frame_size(44_100.0), 882);
        assert_eq!(_frame_size(22_050.0), 441);
    }

    #[test]
    fn test_build_resampler_and_required_input_positive() {
        let resampler = build_resampler(48_000);
        let required = Resampler::input_frames_next(&resampler);
        assert!(required > 0);
    }

    #[test]
    fn test_resampler_processes_silence_and_outputs_320() {
        let mut resampler = build_resampler(48_000);
        let required = Resampler::input_frames_next(&resampler);

        // Two channels of silence input
        let ch0 = vec![0.0_f32; required];
        let ch1 = vec![0.0_f32; required];
        let wave_in = [&ch0[..], &ch1[..]];

        // FixedOut is configured for 320 output frames
        let mut out_mic = [0.0_f32; 320];
        let mut out_sys = [0.0_f32; 320];
        let mut wave_out = [&mut out_mic[..], &mut out_sys[..]];
        let active = [true, true];

        let (_used, produced) =
            Resampler::process_into_buffer(&mut resampler, &wave_in, &mut wave_out, Some(&active))
                .expect("resample should succeed");

        assert_eq!(produced, 320);
        assert!(out_mic.iter().all(|v| v.abs() < 1e-6));
        assert!(out_sys.iter().all(|v| v.abs() < 1e-6));
    }

    #[test]
    fn test_change_resampler_in_rate_changes_required_input() {
        let mut resampler = build_resampler(48_000);
        let req_48k = Resampler::input_frames_next(&resampler);

        change_resampler_in_rate(&mut resampler, 44_100).expect("ratio change ok");
        let req_44k = Resampler::input_frames_next(&resampler);

        // For fixed output of 320 frames, higher ratio (16000/44100) requires fewer input samples than 16000/48000.
        assert!(
            req_44k < req_48k,
            "expected required input to decrease after 48k->44.1k change"
        );
    }

    #[test]
    fn test_change_resampler_in_rate_zero_is_error() {
        let mut resampler = build_resampler(48_000);
        let res = change_resampler_in_rate(&mut resampler, 0);
        assert!(res.is_err(), "expected error when new_in_rate is zero");
    }

    #[test]
    fn test_encoder_create_and_encode_silence() {
        // 20ms stereo frame at 16 kHz = 320 frames per ch, interleaved floats
        let mut enc = build_encoder(16_000);
        let mut packet = vec![0u8; 400];

        let mut interleaved = vec![0.0_f32; 320 * 2];
        let len = enc
            .encode_float(&interleaved, &mut packet)
            .expect("encode ok");
        assert!(len > 0, "encoded packet should be non-empty");
        packet.truncate(len);

        // Decode back to PCM i16 to validate basic round-trip
        let mut dec = Decoder::new(16_000, opus::Channels::Stereo).expect("decoder ok");
        let mut pcm = vec![0i16; 320 * 2];
        let samples = dec.decode(&packet, &mut pcm, false).expect("decode ok");
        assert_eq!(
            samples, 320,
            "expected 20ms @ 16kHz = 320 samples per channel"
        );
    }

    #[test]
    fn test_resampler_input_buffer_allocate_and_fill() {
        let mut resampler = build_resampler(48_000);
        let mut bufs = Resampler::input_buffer_allocate(&mut resampler, false);
        assert_eq!(bufs.len(), 2);
        assert!(bufs[0].is_empty() && bufs[1].is_empty());

        // Push some samples and ensure lengths update
        bufs[0].extend_from_slice(&[1.0_f32; 10]);
        bufs[1].extend_from_slice(&[2.0_f32; 5]);
        assert_eq!(bufs[0].len(), 10);
        assert_eq!(bufs[1].len(), 5);
    }
}
