use chrono::Local;
use colored::*;
use fixed_resample::rubato::{Resampler, SincFixedOut, SincInterpolationParameters};
use log::{debug, info, trace};
use opus::{Decoder, Encoder};
use ringbuf::traits::Consumer;
use std::{
    io::{self},
    time::Duration,
};
use tokio::{sync::mpsc, task::yield_now, time::sleep};

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

const OUT_SAMPLE_RATE: u32 = 16_000;

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
    sleep(Duration::from_secs(1)).await;
    println!("2...");
    sleep(Duration::from_secs(1)).await;
    println!("1...");
    sleep(Duration::from_secs(1)).await;

    let mut opus_packets = Vec::new();
    let confirm_msg = format!("✅ Recording. Press Ctrl+C to stop.")
        .green()
        .bold();
    println!("{}", confirm_msg);

    let mut encoder = build_encoder(OUT_SAMPLE_RATE); // Encode at 16kHz
    loop {
        // This is here to avoid busy-waiting if there's nothing to do.
        let mut will_busyspin = true;
        // Start by checking for any CoreAudio events to handle device updates

        // If there's an event...
        if let Ok(event) = ca_rx.try_recv() {
            // Handle events depending on the type of AudioPropertyChange
            match event {
                // If the sample rate changed, update the resampler and don't
                // rebuild the device.
                AudioPropertyChange::ActualSampleRate { hz } => {
                    debug!("Actual sample rate changed to {}", hz);
                    change_resampler_in_rate(&mut resampler, hz).unwrap();
                }
                AudioPropertyChange::NominalSampleRate { hz } => {
                    debug!("Nominal sample rate changed to {}", hz);
                    change_resampler_in_rate(&mut resampler, hz).unwrap();
                }
                // But if the device died or the default input/output device changed,
                // we rebuild the aggregate device, so we stop capture.
                AudioPropertyChange::DeviceIsAlive
                | AudioPropertyChange::HardwareDefaultInputDevice { .. }
                | AudioPropertyChange::HardwareDefaultOutputDevice { .. } => {
                    debug!("CoreAudio device change detected: {:?}", event);
                    if let Err(e) = dev.stop_capture() {
                        eprintln!("Failed to stop capture: {e:?}");
                    }
                    debug!("CoreAudio Event: {:?}", event);
                }
                _ => {}
            }

            // Give the system a moment to stabilize
            sleep(Duration::from_millis(500)).await;
            // Right now, we create a new aggregate device on any of the above events.
            // (In the future, we could be smarter and just update the existing device
            // if the input/output devices are still alive.)
            match macos_device::OSXInputDevice::new() {
                Ok(new_dev) => {
                    info!("Creating new aggregate device after CoreAudio event");
                    info!(
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

        // By now, we have a valid mic_consumer and sys_consumer from the current aggregate device.
        // We can read from their ring buffers, resample, encode, and send.

        // 1. Read from aggregate device's ring buffers
        let mut mic_buffer = [0.0_f32; 512];
        let mut sys_buffer = [0.0_f32; 512];
        let mic_read = mic_consumer.pop_slice(&mut mic_buffer);
        let sys_read = sys_consumer.pop_slice(&mut sys_buffer);

        // If either channel has data, append samples to per-channel input buffers
        if mic_read > 0 || sys_read > 0 {
            trace!(
                "{:3} 🎙️ + {:3} 📣 = {:4} total",
                mic_read,
                sys_read,
                mic_read + sys_read
            );
            // Append to the mic channel (0) and sys channel (1) input buffers
            if mic_read > 0 {
                input_buffers[0].extend_from_slice(&mic_buffer[..mic_read]);
            }
            if sys_read > 0 {
                input_buffers[1].extend_from_slice(&sys_buffer[..sys_read]);
            }
            will_busyspin = false; // We did work, so don't busyspin
        }

        // 2. Resample if we have enough input samples for the next output chunk

        // We determine if we have enough input samples by using the resampler's
        // `input_frames_next` method, which tells us how many input frames are needed
        // to produce the next fixed output chunk (320 frames at 16kHz) for a single
        // channel (but we need it for both channels).
        let required_input = Resampler::input_frames_next(&resampler); // I think this is always 320

        // If we have enough input samples for both channels, we can resample
        if input_buffers[0].len() >= required_input && input_buffers[1].len() >= required_input {
            let wave_in = [
                &input_buffers[0][..required_input],
                &input_buffers[1][..required_input],
            ];

            // out_mic and out_sys are empty arrays that will be filled later by the resampler's
            // audio output for each channel. It starts with zeroed buffers.
            let mut out_mic = [0.0_f32; 320];
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
                    // Remove consumed input, only removes enough to fill a 20ms 16khz frame (320 samples)
                    input_buffers[0].drain(0..required_input);
                    input_buffers[1].drain(0..required_input);

                    // Interleave produced samples

                    // Produced is usually 320, but the reason why we don't assume that is
                    // is because the rubato resampler can produce variable output sizes
                    // This is handled for us through rubato's `input_buffer_allocate`.
                    let frame_len = produced;
                    let mut interleaved = Vec::<f32>::with_capacity(frame_len * 2);
                    for i in 0..frame_len {
                        interleaved.push(out_mic[i]);
                        interleaved.push(out_sys[i]);
                    }

                    // At this point, we have 2x 320 samples of 16khz interleaved float PCM audio
                    // We can now encode this with Opus and send it to the WebSocket server
                    let mut encoded = vec![0u8; 400]; // enough for 20ms @ low bitrate (this is an AI comment idk why its 400 but it works)
                    let packet_len = encoder
                        .encode_float(&interleaved, &mut encoded)
                        .expect("Opus encode failed");
                    encoded.truncate(packet_len);
                    // println!("Encoded frame size: {}", packet_len);

                    // Store the Opus packet for later writing to a file
                    // We don't do that right now but we could in the future.
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
        // If normally we would busyspin, yield to the tokio scheduler instead.
        if will_busyspin {
            // yield_now is a tokio method that yields to the scheduler
            // allowing other tasks to run. This prevents busy-waiting
            // and reduces CPU usage when there's nothing to do.
            yield_now().await;
        }
    }
}

/// Builds and returns a SincFixedOut resampler to convert from `in_sample_rate` to `OUT_SAMPLE_RATE`.
/// We chose FixedOut because we want a fixed output size for Opus encoding.
/// Configured for fixed output of 320 frames (20ms @ 16kHz).
///
/// # Arguments
/// * `in_sample_rate` - The input sample rate (e.g. the system's output device sample rate)
/// # Returns
/// * `SincFixedOut<f32>` - The configured resampler instance
fn build_resampler(in_sample_rate: u32) -> SincFixedOut<f32> {
    let out_sample_rate = OUT_SAMPLE_RATE; // always 16,000hz for Opus and transmission
    let resample_ratio = out_sample_rate as f64 / in_sample_rate as f64; // example: 48000 / 44100 = 1.088435
    let max_resample_ratio_relative = 3.1; // Allow for some variance in sample rate

    let parameters = SincInterpolationParameters {
        sinc_len: 256,            // docs says 256 is a good starting point
        f_cutoff: 0.95,           // docs say start at 0.95 and adjust if needed
        oversampling_factor: 128, // docs say to start at 128
        interpolation: fixed_resample::rubato::SincInterpolationType::Nearest, // Nearest is fastest
        window: fixed_resample::rubato::WindowFunction::Hann, // Hann is fastest.
    };

    // Create the resampler instance using the parameters we made above.
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

/// Builds and returns an Opus encoder configured for the given input sample rate.
/// We use stereo channels and a bitrate of 32kbps. Why 32kbps? Idk. lmao
///
/// # Arguments
/// * `in_sample_rate` - The input sample rate (e.g. 16000 for Opus)
/// # Returns
/// * `Encoder` - The configured Opus encoder instance
fn build_encoder(in_sample_rate: u32) -> Encoder {
    let application = opus::Application::LowDelay;
    let mut encoder = Encoder::new(in_sample_rate, opus::Channels::Stereo, application)
        .expect("Failed to create Opus encoder");
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(32_000));
    encoder
}

/// Changes the input sample rate of the given resampler to `new_in_rate`.
/// This updates the resample ratio accordingly.
/// Remember, we are always resampling to `OUT_SAMPLE_RATE` (16kHz).
/// # Arguments
/// * `resampler` - The resampler instance to update
/// * `new_in_rate` - The new input sample rate (e.g. the system's output device sample rate)
/// # Returns
/// * `Result<(), fixed_resample::rubato::ResampleError>` - Ok if successful, Err if failed
fn change_resampler_in_rate(
    resampler: &mut SincFixedOut<f32>,
    new_in_rate: u32,
) -> Result<(), fixed_resample::rubato::ResampleError> {
    let new_ratio = OUT_SAMPLE_RATE as f64 / new_in_rate as f64;
    Resampler::set_resample_ratio(resampler, new_ratio, false)
}

/// Prints the SplitStream demo banner and copyright message to the console.
/// Uses colored crate for styling.
/// No arguments.
/// No return value.
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

fn _frame_size(sample_rate: f64) -> usize {
    const FRAME_MS: f64 = 20.0;
    ((sample_rate * FRAME_MS) / 1000.0).round() as usize
}

/// Spawns the CoreAudio listener tokio task that monitors for device changes from `coreaudio_listener`.
/// Sends events to the provided an UnboundedSender with AudioPropertyChange.
///
/// # Arguments
/// * `ws_client` - The WebSocketClient instance to use for transmission
/// * `opus_packet_rx` - The UnboundedReceiver to receive Opus packets for transmission
/// * `transcript_tx` - The UnboundedSender to send transcripts back to the main task
/// No return value.
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
///
/// # Arguments
/// * `ca_tx` - The UnboundedSender to send AudioPropertyChange events to the main task
/// No return value.
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
                    sleep(Duration::from_millis(500)).await;
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
///
/// # Arguments
/// * `transcript_rx` - The UnboundedReceiver to receive transcript messages as JSON strings
/// No return value.
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
        let mut enc = build_encoder(OUT_SAMPLE_RATE);
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
