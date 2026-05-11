//! Core audio capture and routing loop.
//!
//! Drains mic and sys ring buffers every 20ms, resamples each to 16kHz/320-sample
//! frames, optionally applies AEC, then routes frames to the active backend.
//! Mid-stream sample-rate adaptation is handled here: mic via device-config
//! polling (stream rebuild), sys via drain-count estimation (resampler rebuild).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use fixed_resample::rubato::Resampler;
use log::info;
use ringbuf::traits::Consumer;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{MissedTickBehavior, interval};

use crate::audio::{mic_capture, resampler, sys_capture};
use crate::audio::resampler::{FINAL_FRAME_SIZE, OUT_SAMPLE_RATE};
use crate::backends::{self, BackendActor};
use crate::builder::BackendSpec;
use crate::echo_cancellation::EchoCanceler;
use crate::handle::Controls;
use crate::transcript::Transcript;
use crate::SplitStreamError;

// EchoCanceler wraps a raw pointer to the Speex AEC state. The pointer is
// owned exclusively by the engine task and never shared — safe to declare Send.
struct SendAec(EchoCanceler);
unsafe impl Send for SendAec {}

#[cfg(feature = "whisper")]
const WHISPER_BATCH_SAMPLES: usize = OUT_SAMPLE_RATE as usize / 2; // 8_000 — 0.5s
#[cfg(feature = "parakeet")]
const PARAKEET_BATCH_SAMPLES: usize = 2_560; // 160ms, exact ParakeetEOU chunk size

pub(crate) async fn run(
    controls: Arc<Controls>,
    backend_spec: BackendSpec,
    transcript_tx: UnboundedSender<Transcript>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), SplitStreamError> {
    // --- Mic capture ---
    let mut mic = mic_capture::setup()
        .map_err(|_| SplitStreamError::NoMicDevice)?;

    // --- Sys capture ---
    let (_sys_tap, mut sys_cons) = sys_capture::SysAudioTap::new()
        .map_err(SplitStreamError::SysAudioTap)?;
    let mut sys_sample_rate = _sys_tap.sample_rate;

    info!("Mic: {}hz | Sys: {}hz", mic.sample_rate, sys_sample_rate);

    let mut mic_resampler = resampler::build_resampler(mic.sample_rate);
    let mut sys_resampler = resampler::build_resampler(sys_sample_rate);
    let mut mic_input_buf: Vec<f32> = Vec::new();
    let mut sys_input_buf: Vec<f32> = Vec::new();
    let mut aec = SendAec(EchoCanceler::new());

    // --- Backend initialization ---
    let mut backend_actor = match backend_spec {
        #[cfg(feature = "deepgram")]
        BackendSpec::Deepgram(cfg) => {
            let ws_url = format!(
                "wss://api.deepgram.com/v1/listen?encoding=opus&sample_rate=16000\
                 &channels=2&multichannel=true&model={}",
                cfg.model
            );
            let b = backends::deepgram::spawn(cfg.api_key, ws_url, transcript_tx.clone());
            BackendActor::Deepgram { opus_packet_tx: b.opus_packet_tx, encoder: b.encoder }
        }

        #[cfg(feature = "whisper")]
        BackendSpec::Whisper(cfg) => {
            let window = cfg.window_seconds as usize * OUT_SAMPLE_RATE as usize;
            let (mic_tx, sys_tx) =
                backends::whisper::spawn(&cfg.model_path, window, transcript_tx.clone());
            BackendActor::Whisper { mic_pcm_tx: mic_tx, sys_pcm_tx: sys_tx }
        }

        #[cfg(feature = "parakeet")]
        BackendSpec::Parakeet(cfg) => {
            let (mic_tx, sys_tx) =
                backends::parakeet::spawn(&cfg.model_dir, transcript_tx.clone()).await?;
            BackendActor::Parakeet { mic_pcm_tx: mic_tx, sys_pcm_tx: sys_tx }
        }
    };

    // --- Local backend accumulators ---
    #[cfg(any(feature = "whisper", feature = "parakeet"))]
    let mut local_mic_buf: Vec<f32> = Vec::new();
    #[cfg(any(feature = "whisper", feature = "parakeet"))]
    let mut local_sys_buf: Vec<f32> = Vec::new();

    // --- 20ms frame tick ---
    let mut frame_tick = interval(Duration::from_millis(20));
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Drain any audio that accumulated during backend init.
    {
        let mut tmp = vec![0.0f32; 8192];
        mic.consumer.pop_slice(&mut tmp);
        sys_cons.pop_slice(&mut tmp);
    }

    // Mid-stream rate adaptation counters (sys uses drain-count estimation).
    let mut sys_rate_acc: u64 = 0;
    let mut rate_ticks: u32 = 0;
    const RATE_CHECK_TICKS: u32 = 50; // 50 × 20ms = 1s

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown_rx => {
                break;
            }

            _ = frame_tick.tick() => {
                // --- Drain ring buffers ---
                let mut tmp = vec![0.0f32; 4096];
                let n = mic.consumer.pop_slice(&mut tmp);
                if n > 0 {
                    mic_input_buf.extend_from_slice(&tmp[..n]);
                }
                let n = sys_cons.pop_slice(&mut tmp);
                if n > 0 {
                    sys_input_buf.extend_from_slice(&tmp[..n]);
                    sys_rate_acc += n as u64;
                }

                // --- Rate adaptation (every 1s) ---
                rate_ticks += 1;
                if rate_ticks >= RATE_CHECK_TICKS {
                    // Mic: poll device config; rebuild stream + resampler if rate changed.
                    if mic_capture::maybe_rebuild(&mut mic) {
                        mic_resampler = resampler::build_resampler(mic.sample_rate);
                        mic_input_buf.clear();
                    }

                    // Sys: drain-count estimation; rebuild resampler if rate changed.
                    if sys_rate_acc > 0 {
                        let inferred = resampler::snap_to_standard_rate(sys_rate_acc as u32);
                        if inferred != sys_sample_rate {
                            info!(
                                "Sys rate change: {}Hz → {}Hz, rebuilding resampler",
                                sys_sample_rate, inferred
                            );
                            sys_sample_rate = inferred;
                            sys_resampler = resampler::build_resampler(sys_sample_rate);
                            sys_input_buf.clear();
                        }
                    }
                    sys_rate_acc = 0;
                    rate_ticks = 0;
                }

                // --- Cap buffers to 2 frames to prevent latency buildup ---
                let mic_required = Resampler::input_frames_next(&mic_resampler);
                let sys_required = Resampler::input_frames_next(&sys_resampler);
                if mic_input_buf.len() > mic_required * 2 {
                    mic_input_buf.drain(0..mic_input_buf.len() - mic_required * 2);
                }
                if sys_input_buf.len() > sys_required * 2 {
                    sys_input_buf.drain(0..sys_input_buf.len() - sys_required * 2);
                }

                // --- Resample mic → 16kHz/320 ---
                let mut out_mic = [0.0f32; FINAL_FRAME_SIZE];
                if mic_input_buf.len() >= mic_required {
                    let wave_in = [&mic_input_buf[..mic_required]];
                    let mut wave_out = [&mut out_mic[..]];
                    match Resampler::process_into_buffer(
                        &mut mic_resampler, &wave_in, &mut wave_out, Some(&[true]),
                    ) {
                        Ok(_) => { mic_input_buf.drain(0..mic_required); }
                        Err(e) => {
                            eprintln!("Mic resample error: {e:?}");
                            mic_input_buf.drain(0..mic_input_buf.len());
                        }
                    }
                }

                // --- Resample sys → 16kHz/320 ---
                let mut out_sys = [0.0f32; FINAL_FRAME_SIZE];
                if sys_input_buf.len() >= sys_required {
                    let wave_in = [&sys_input_buf[..sys_required]];
                    let mut wave_out = [&mut out_sys[..]];
                    match Resampler::process_into_buffer(
                        &mut sys_resampler, &wave_in, &mut wave_out, Some(&[true]),
                    ) {
                        Ok(_) => { sys_input_buf.drain(0..sys_required); }
                        Err(e) => {
                            eprintln!("Sys resample error: {e:?}");
                            sys_input_buf.drain(0..sys_input_buf.len());
                        }
                    }
                }

                // --- Sys mute (applied before AEC so AEC gets silence as reference) ---
                if controls.sys_muted.load(Ordering::Relaxed) {
                    out_sys.iter_mut().for_each(|s| *s = 0.0);
                }

                // --- AEC ---
                let mut capture_frame = out_mic.to_vec();
                let render_frame = out_sys.to_vec();

                if mic.sample_rate >= OUT_SAMPLE_RATE
                    && controls.echo_cancellation.load(Ordering::Relaxed)
                {
                    let orig = capture_frame.clone();
                    capture_frame =
                        aec.0.cancel_speaker_echo(capture_frame, render_frame).unwrap_or(orig);
                }

                out_mic.copy_from_slice(&capture_frame);

                // --- Mic mute (applied after AEC so the filter receives real signal) ---
                if controls.mic_muted.load(Ordering::Relaxed) {
                    out_mic.iter_mut().for_each(|s| *s = 0.0);
                }

                // --- Route to backend ---
                match &mut backend_actor {
                    #[cfg(feature = "deepgram")]
                    BackendActor::Deepgram { encoder, opus_packet_tx } => {
                        let mut interleaved = Vec::<f32>::with_capacity(FINAL_FRAME_SIZE * 2);
                        for i in 0..FINAL_FRAME_SIZE {
                            interleaved.push(out_mic[i]);
                            interleaved.push(out_sys[i]);
                        }
                        let mut encoded = vec![0u8; 400];
                        match encoder.encode_float(&interleaved, &mut encoded) {
                            Ok(len) => {
                                encoded.truncate(len);
                                let _ = opus_packet_tx.send(encoded);
                            }
                            Err(e) => eprintln!("Opus encode error: {e:?}"),
                        }
                    }

                    #[cfg(feature = "whisper")]
                    BackendActor::Whisper { mic_pcm_tx, sys_pcm_tx } => {
                        local_mic_buf.extend_from_slice(&out_mic);
                        local_sys_buf.extend_from_slice(&out_sys);
                        if local_mic_buf.len() >= WHISPER_BATCH_SAMPLES {
                            mic_pcm_tx.send(std::mem::take(&mut local_mic_buf)).ok();
                            sys_pcm_tx.send(std::mem::take(&mut local_sys_buf)).ok();
                        }
                    }

                    #[cfg(feature = "parakeet")]
                    BackendActor::Parakeet { mic_pcm_tx, sys_pcm_tx } => {
                        local_mic_buf.extend_from_slice(&out_mic);
                        local_sys_buf.extend_from_slice(&out_sys);
                        while local_mic_buf.len() >= PARAKEET_BATCH_SAMPLES {
                            mic_pcm_tx
                                .send(local_mic_buf[..PARAKEET_BATCH_SAMPLES].to_vec())
                                .ok();
                            sys_pcm_tx
                                .send(local_sys_buf[..PARAKEET_BATCH_SAMPLES].to_vec())
                                .ok();
                            local_mic_buf.drain(0..PARAKEET_BATCH_SAMPLES);
                            local_sys_buf.drain(0..PARAKEET_BATCH_SAMPLES);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
