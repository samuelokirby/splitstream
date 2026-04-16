use colored::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;
use fixed_resample::rubato::{Resampler, SincFixedOut, SincInterpolationParameters};
use log::{info, trace};
use opus::Encoder;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Producer, Split};
use std::{collections::HashSet, time::Duration};
use tokio::{sync::mpsc, time::{sleep, interval, MissedTickBehavior}};

use crate::{
    echo_cancellation::EchoCanceler,
    settings::Settings,
    sys_audio_tap::SysAudioTap,
    transcript_msg::TranscriptMessage,
    websocket_client::WebSocketClient,
};

pub mod echo_cancellation;
pub mod local_transcriber;
pub mod settings;
pub mod sys_audio_tap;
pub mod transcript_msg;
pub mod websocket_client;

enum TranscriptionBackend {
    Deepgram {
        opus_packet_tx: mpsc::UnboundedSender<Vec<u8>>,
        encoder: Encoder,
    },
    Whisper {
        mic_pcm_tx: std::sync::mpsc::Sender<Vec<f32>>,
        sys_pcm_tx: std::sync::mpsc::Sender<Vec<f32>>,
    },
}

// 0.5 seconds of 16kHz mono PCM per batch sent to the Whisper thread.
// The transcriber accumulates 4 of these (2s) before running inference.
const WHISPER_BATCH_SAMPLES: usize = OUT_SAMPLE_RATE as usize / 2; // 8_000

const OUT_SAMPLE_RATE: u32 = 16_000;
const FINAL_FRAME_SIZE: usize = 320; // 20ms @ 16kHz

#[tokio::main]
async fn main() {
    // Load .env so DEEPGRAM_API_KEY is available via std::env::var
    dotenvy::dotenv().ok();

    // Initialize settings from settings.toml
    let settings = Settings::new();
    let sys_muted = settings.compliance_mode_on_start;
    let echo_cancellation_on = settings.echo_cancellation;
    print_splitstream_demo_msg();

    // --- Mic capture via cpal ---
    let host = cpal::default_host();
    let mic_device = host.default_input_device().expect("no default input device");

    let mic_supported_config = mic_device
        .default_input_config()
        .expect("no default input config");
    let mic_sample_rate = mic_supported_config.sample_rate();
    let mic_channels = mic_supported_config.channels() as usize;
    let mic_stream_config: StreamConfig = mic_supported_config.config();

    let mic_rb = HeapRb::<f32>::new(8192);
    let (mut mic_prod, mut mic_cons) = mic_rb.split();

    let mic_stream = mic_device
        .build_input_stream(
            &mic_stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if mic_channels == 1 {
                    trace!("* 🔴🎤 Pushed {} mic samples", data.len());
                    mic_prod.push_slice(data);
                } else {
                    let mono: Vec<f32> = data
                        .chunks(mic_channels)
                        .map(|frame| frame.iter().sum::<f32>() / mic_channels as f32)
                        .collect();
                    trace!("* 🔴🎤 Pushed {} mic samples (mixed to mono)", mono.len());
                    mic_prod.push_slice(&mono);
                }
            },
            |err| eprintln!("Mic stream error: {err}"),
            None,
        )
        .expect("failed to build mic input stream");

    mic_stream.play().expect("failed to start mic stream");

    // --- Sys audio via cidre process tap ---
    // SysAudioTap creates a global CoreAudio process tap with the mic as the aggregate
    // clock source. This avoids the sample rate negotiation timeout that cpal's loopback
    // hits on Bluetooth/AirPlay/virtual output devices.
    // The tap's ASBD gives the authoritative sample rate — no guessing.
    let (_sys_tap, mut sys_cons) =
        SysAudioTap::new().expect("failed to create sys audio tap");
    let sys_sample_rate = _sys_tap.sample_rate;

    info!(
        "Mic: {}ch @ {}hz | Sys: @ {}hz (from tap ASBD)",
        mic_channels, mic_sample_rate, sys_sample_rate
    );

    // --- One single-channel resampler per source, each → 16kHz ---
    let mut mic_resampler = build_resampler(mic_sample_rate);
    let mut sys_resampler = build_resampler(sys_sample_rate);

    let mut mic_input_buf: Vec<f32> = Vec::new();
    let mut sys_input_buf: Vec<f32> = Vec::new();

    let mut aec = EchoCanceler::new();

    let mut transcription_backend = match settings.transcription_backend.as_str() {
        "whisper" => {
            info!("Transcription backend: local Whisper");
            let (mic_pcm_tx, sys_pcm_tx) = local_transcriber::spawn(
                &settings.whisper_model_path,
                &settings.whisper_vad_model_path,
                settings.whisper_window_seconds as usize * 16_000,
            );
            TranscriptionBackend::Whisper { mic_pcm_tx, sys_pcm_tx }
        }
        _ => {
            info!("Transcription backend: Deepgram");
            let access_token = std::env::var("DEEPGRAM_API_KEY")
                .expect("DEEPGRAM_API_KEY not set — add it to .env or the environment");
            let ws_url = "wss://api.deepgram.com/v1/listen?encoding=opus&sample_rate=16000&channels=2&multichannel=true&model=nova-3".to_string();
            let ws_client = WebSocketClient::new(access_token, ws_url);

            let (opus_packet_tx, opus_packet_rx) = mpsc::unbounded_channel::<Vec<u8>>();
            let (transcript_tx, transcript_rx) = mpsc::unbounded_channel::<String>();

            create_websocket_task(ws_client, opus_packet_rx, transcript_tx);
            create_transcript_stdout_task(transcript_rx);

            // Give Deepgram's WebSocket time to connect before sending audio.
            println!("Starting in 3...");
            sleep(Duration::from_secs(1)).await;
            println!("2...");
            sleep(Duration::from_secs(1)).await;
            println!("1...");
            sleep(Duration::from_secs(1)).await;

            TranscriptionBackend::Deepgram {
                opus_packet_tx,
                encoder: build_encoder(OUT_SAMPLE_RATE),
            }
        }
    };

    let confirm_msg = "✅ Recording. Press Ctrl+C to stop.".green().bold();
    println!("{}", confirm_msg);

    // Audio accumulators for the Whisper path — filled each tick and flushed
    // every WHISPER_BATCH_SAMPLES so we send 1s chunks instead of 20ms chunks.
    let mut whisper_mic_buf: Vec<f32> = Vec::new();
    let mut whisper_sys_buf: Vec<f32> = Vec::new();

    // Drive encoding from a 20ms interval timer — matching the Opus frame size exactly.
    // This gives a stable 50 Hz packet rate, yields to the Tokio runtime on every tick
    // so the WebSocket and transcript tasks get CPU time, and eliminates the busy-spin.
    // Skip missed ticks rather than bursting to catch up.
    let mut frame_tick = interval(Duration::from_millis(20));
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Drain any audio that accumulated during the countdown before we start.
    {
        let mut tmp = vec![0.0f32; 8192];
        mic_cons.pop_slice(&mut tmp);
        sys_cons.pop_slice(&mut tmp);
    }

    // Track whether we've warned about AEC being skipped so we don't spam logs.
    let mut aec_skip_warned = false;

    loop {
        frame_tick.tick().await;

        // Drain all samples that have arrived since the last tick into the
        // accumulation buffers.
        let mut tmp = vec![0.0f32; 4096];
        let n = mic_cons.pop_slice(&mut tmp);
        if n > 0 {
            trace!("{:3} 🎙️ mic samples drained", n);
            mic_input_buf.extend_from_slice(&tmp[..n]);
        }
        let n = sys_cons.pop_slice(&mut tmp);
        if n > 0 {
            trace!("{:3} 📣 sys samples drained", n);
            sys_input_buf.extend_from_slice(&tmp[..n]);
        }

        // Cap each buffer to at most 2 frames to prevent latency accumulating if
        // a source temporarily runs ahead (e.g. startup burst).
        let mic_required = Resampler::input_frames_next(&mic_resampler);
        let sys_required = Resampler::input_frames_next(&sys_resampler);
        if mic_input_buf.len() > mic_required * 2 {
            mic_input_buf.drain(0..mic_input_buf.len() - mic_required * 2);
        }
        if sys_input_buf.len() > sys_required * 2 {
            sys_input_buf.drain(0..sys_input_buf.len() - sys_required * 2);
        }

        // Resample each channel; use silence if not enough samples have arrived yet.
        let mut out_mic = [0.0f32; FINAL_FRAME_SIZE];
        if mic_input_buf.len() >= mic_required {
            let wave_in = [&mic_input_buf[..mic_required]];
            let mut wave_out = [&mut out_mic[..]];
            match Resampler::process_into_buffer(
                &mut mic_resampler,
                &wave_in,
                &mut wave_out,
                Some(&[true]),
            ) {
                Ok(_) => mic_input_buf.drain(0..mic_required),
                Err(e) => { eprintln!("Mic resample error: {e:?}"); mic_input_buf.drain(0..mic_input_buf.len()) }
            };
        }

        let mut out_sys = [0.0f32; FINAL_FRAME_SIZE];
        if sys_input_buf.len() >= sys_required {
            let wave_in = [&sys_input_buf[..sys_required]];
            let mut wave_out = [&mut out_sys[..]];
            match Resampler::process_into_buffer(
                &mut sys_resampler,
                &wave_in,
                &mut wave_out,
                Some(&[true]),
            ) {
                Ok(_) => sys_input_buf.drain(0..sys_required),
                Err(e) => { eprintln!("Sys resample error: {e:?}"); sys_input_buf.drain(0..sys_input_buf.len()) }
            };
        }

        if sys_muted {
            mute_buffer(&mut out_sys);
        }

        let mut capture_frame: Vec<f32> = out_mic.to_vec();
        let render_frame: Vec<f32> = out_sys.to_vec();

        if mic_sample_rate >= OUT_SAMPLE_RATE && echo_cancellation_on {
            let orig = capture_frame.clone();
            capture_frame = aec.cancel_speaker_echo(capture_frame, render_frame).unwrap_or(orig);
        } else if !aec_skip_warned {
            info!(
                "Skipping AEC: mic rate {}hz, echo cancellation {}",
                mic_sample_rate,
                if echo_cancellation_on { "on" } else { "off" }
            );
            aec_skip_warned = true;
        }

        out_mic.copy_from_slice(&capture_frame);

        match &mut transcription_backend {
            TranscriptionBackend::Deepgram { encoder, opus_packet_tx } => {
                // Interleave mic (ch0) and sys (ch1) into a stereo Opus frame.
                let mut interleaved = Vec::<f32>::with_capacity(FINAL_FRAME_SIZE * 2);
                for i in 0..FINAL_FRAME_SIZE {
                    interleaved.push(out_mic[i]);
                    interleaved.push(out_sys[i]);
                }
                let mut encoded = vec![0u8; 400];
                match encoder.encode_float(&interleaved, &mut encoded) {
                    Ok(len) => { encoded.truncate(len); let _ = opus_packet_tx.send(encoded); }
                    Err(e) => eprintln!("Opus encode error: {e:?}"),
                }
            }
            TranscriptionBackend::Whisper { mic_pcm_tx, sys_pcm_tx } => {
                whisper_mic_buf.extend_from_slice(&out_mic);
                whisper_sys_buf.extend_from_slice(&out_sys);
                if whisper_mic_buf.len() >= WHISPER_BATCH_SAMPLES {
                    mic_pcm_tx.send(std::mem::take(&mut whisper_mic_buf)).ok();
                    sys_pcm_tx.send(std::mem::take(&mut whisper_sys_buf)).ok();
                }
            }
        }
    }
}

fn mute_buffer(buffer: &mut [f32]) {
    for sample in buffer.iter_mut() {
        *sample = 0.0;
    }
}

/// Single-channel SincFixedOut resampler: `in_sample_rate` → 16kHz, 320-sample output.
fn build_resampler(in_sample_rate: u32) -> SincFixedOut<f32> {
    let resample_ratio = OUT_SAMPLE_RATE as f64 / in_sample_rate as f64;
    let max_resample_ratio_relative = 3.1;

    let parameters = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        oversampling_factor: 128,
        interpolation: fixed_resample::rubato::SincInterpolationType::Nearest,
        window: fixed_resample::rubato::WindowFunction::Hann,
    };

    SincFixedOut::<f32>::new(
        resample_ratio,
        max_resample_ratio_relative,
        parameters,
        FINAL_FRAME_SIZE,
        1,
    )
    .expect("Failed to create resampler")
}

#[allow(dead_code)]
fn change_resampler_in_rate(
    resampler: &mut SincFixedOut<f32>,
    new_in_rate: u32,
) -> Result<(), fixed_resample::rubato::ResampleError> {
    let new_ratio = OUT_SAMPLE_RATE as f64 / new_in_rate as f64;
    Resampler::set_resample_ratio(resampler, new_ratio, false)
}

fn build_encoder(in_sample_rate: u32) -> Encoder {
    let application = opus::Application::LowDelay;
    let mut encoder = Encoder::new(in_sample_rate, opus::Channels::Stereo, application)
        .expect("Failed to create Opus encoder");
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(32_000));
    encoder
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

fn _frame_size(sample_rate: f64) -> usize {
    const FRAME_MS: f64 = 20.0;
    ((sample_rate * FRAME_MS) / 1000.0).round() as usize
}

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

/// Prints Deepgram transcripts. Logs ALL messages at debug level so non-Results
/// responses (errors, metadata, etc.) are visible when RUST_LOG=debug.
fn create_transcript_stdout_task(mut transcript_rx: mpsc::UnboundedReceiver<String>) {
    tokio::spawn(async move {
        while let Some(raw) = transcript_rx.recv().await {
            // Log the raw message so we can debug Deepgram responses
            log::debug!("Deepgram: {raw}");

            let v: TranscriptMessage = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Transcript parse error: {e} — raw: {raw}");
                    continue;
                }
            };

            if v.msg_type != "Results" {
                continue;
            }
            let Some(text) = v.transcript() else { continue };
            if text.is_empty() {
                continue;
            }

            // channel 0 = mic, channel 1 = sys
            let styled = if v.is_final {
                text.green().bold().to_string()
            } else {
                text.to_string()
            };

            match v.channel_num() {
                Some(0) => println!("🎙️(mic)\t{}", styled.white()),
                Some(1) => println!("🔊(sys) {}", styled.bright_black()),
                _ => {}
            }
        }
    });
}

fn _share_more_than_three(a: &str, b: &str) -> bool {
    let wa: HashSet<String> = a
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect();

    let mut count = 0;
    let mut matches: HashSet<String> = HashSet::new();

    for word in b.split_whitespace().map(|w| w.to_ascii_lowercase()) {
        if wa.contains(&word) {
            count += 1;
            matches.insert(word.clone());
            if count > 3 {
                println!("Shared words (>3): {:?}", matches);
                return true;
            }
        }
    }

    println!("Shared words ({}): {:?}", count, matches);
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus::Decoder;

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

        let ch0 = vec![0.0_f32; required];
        let wave_in = [&ch0[..]];

        let mut out = [0.0_f32; FINAL_FRAME_SIZE];
        let mut wave_out = [&mut out[..]];
        let active = [true];

        let (_used, produced) =
            Resampler::process_into_buffer(&mut resampler, &wave_in, &mut wave_out, Some(&active))
                .expect("resample should succeed");

        assert_eq!(produced, FINAL_FRAME_SIZE);
        assert!(out.iter().all(|v| v.abs() < 1e-6));
    }

    #[test]
    fn test_change_resampler_in_rate_changes_required_input() {
        let mut resampler = build_resampler(48_000);
        let req_48k = Resampler::input_frames_next(&resampler);

        change_resampler_in_rate(&mut resampler, 44_100).expect("ratio change ok");
        let req_44k = Resampler::input_frames_next(&resampler);

        assert!(req_44k < req_48k);
    }

    #[test]
    fn test_change_resampler_in_rate_zero_is_error() {
        let mut resampler = build_resampler(48_000);
        let res = change_resampler_in_rate(&mut resampler, 0);
        assert!(res.is_err());
    }

    #[test]
    fn test_encoder_create_and_encode_silence() {
        let mut enc = build_encoder(OUT_SAMPLE_RATE);
        let mut packet = vec![0u8; 400];

        let interleaved = vec![0.0_f32; FINAL_FRAME_SIZE * 2];
        let len = enc
            .encode_float(&interleaved, &mut packet)
            .expect("encode ok");
        assert!(len > 0);
        packet.truncate(len);

        let mut dec = Decoder::new(16_000, opus::Channels::Stereo).expect("decoder ok");
        let mut pcm = vec![0i16; FINAL_FRAME_SIZE * 2];
        let samples = dec.decode(&packet, &mut pcm, false).expect("decode ok");
        assert_eq!(samples, FINAL_FRAME_SIZE);
    }

    #[test]
    fn test_resampler_input_buf_fill() {
        let mut resampler = build_resampler(48_000);
        let required = Resampler::input_frames_next(&resampler);

        let mut buf: Vec<f32> = Vec::new();
        buf.extend_from_slice(&vec![1.0_f32; 10]);
        assert!(buf.len() < required);

        buf.extend_from_slice(&vec![0.0_f32; required]);
        assert!(buf.len() >= required);
    }
}
