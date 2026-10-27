//! Core audio capture and routing loop.
//!
//! Drains mic and sys ring buffers every 20ms, resamples each to 16kHz/320-sample
//! frames, optionally applies AEC, then routes frames to the active backend.
//! Mid-stream sample-rate adaptation is handled here: mic via device-config
//! polling (stream rebuild), sys via drain-count estimation (resampler rebuild).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use ringbuf::traits::Consumer;
use rubato::Resampler;
use tokio::sync::Notify;
use tokio::time::{MissedTickBehavior, interval};

use crate::SplitStreamError;
use crate::audio::resampler::{FINAL_FRAME_SIZE, OUT_SAMPLE_RATE};
use crate::audio::{mic_capture, resampler, sys_capture};
#[cfg(any(feature = "deepgram", feature = "whisper", feature = "parakeet"))]
use crate::backends::{self, BackendActor};
use crate::builder::BackendSpec;
#[cfg(feature = "aec")]
use crate::echo_cancellation::EchoCanceler;
use crate::error::{ErrorReporter, ErrorSender, ReadySender};
use crate::handle::Controls;
use crate::transcript::Transcript;

/// Collapses a per-tick failure into at most one log line per interval.
///
/// The resample / AEC / encode paths run 50× per second: logging each failure
/// unthrottled would bury every other line in the consumer's log.
struct Throttled {
    label: &'static str,
    last: Option<Instant>,
    suppressed: u64,
    interval: Duration,
}

impl Throttled {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            last: None,
            suppressed: 0,
            interval: Duration::from_secs(5),
        }
    }

    /// Log `msg` unless one was logged within the last `interval`; suppressed
    /// occurrences are counted and reported with the next line that gets through.
    fn warn(&mut self, msg: std::fmt::Arguments<'_>) {
        let now = Instant::now();
        let due = self
            .last
            .is_none_or(|t| now.duration_since(t) >= self.interval);
        if due {
            if self.suppressed > 0 {
                warn!(
                    "{}: {msg} ({} similar occurrence(s) suppressed)",
                    self.label, self.suppressed
                );
            } else {
                warn!("{}: {msg}", self.label);
            }
            self.last = Some(now);
            self.suppressed = 0;
        } else {
            self.suppressed += 1;
        }
    }
}

/// Log a ring-buffer overrun counter if it grew since the last check.
fn report_drops(label: &str, counter: &AtomicU64, last: &mut u64) {
    let now = counter.load(Ordering::Relaxed);
    if now > *last {
        warn!(
            "{label} ring buffer overrun: {} samples dropped in the last second \
             ({now} total) — the engine is not draining fast enough",
            now - *last
        );
        *last = now;
    }
}

// EchoCanceler wraps a raw pointer to the Speex AEC state. The pointer is
// owned exclusively by the engine task and never shared — safe to declare Send.
struct SendAec(Canceler);
#[cfg(feature = "aec")]
unsafe impl Send for SendAec {}

/// The echo canceller, or a do-nothing stand-in when the `aec` feature is off.
///
/// Aliasing here rather than `cfg`-ing every call site keeps the mute-ordering
/// logic in [`apply_mutes_and_aec`] a single code path, which is the part worth
/// testing. Without the feature the engine never marks the AEC active, so the
/// stand-in's `cancel_speaker_echo` is unreachable in practice.
#[cfg(feature = "aec")]
type Canceler = EchoCanceler;
#[cfg(not(feature = "aec"))]
type Canceler = NoAec;

/// Stand-in for [`EchoCanceler`] when the `aec` feature is off. Passes the
/// microphone through untouched.
#[cfg(not(feature = "aec"))]
pub(crate) struct NoAec;

#[cfg(not(feature = "aec"))]
impl NoAec {
    fn new() -> Self {
        Self
    }

    fn cancel_speaker_echo(&mut self, _mic: &mut [f32], _sys: &[f32]) -> AecResult<()> {
        Ok(())
    }
}

/// `Result` of an echo-cancellation attempt. With the `aec` feature off there
/// is no failure mode, so the error type is uninhabited.
#[cfg(feature = "aec")]
type AecResult<T> = crate::echo_cancellation::Result<T>;
#[cfg(not(feature = "aec"))]
type AecResult<T> = std::result::Result<T, std::convert::Infallible>;

#[cfg(feature = "whisper")]
const WHISPER_BATCH_SAMPLES: usize = OUT_SAMPLE_RATE as usize / 2; // 8_000 — 0.5s
#[cfg(feature = "parakeet")]
const PARAKEET_BATCH_SAMPLES: usize = 2_560; // 160ms, exact ParakeetEOU chunk size

/// Apply mute state and echo cancellation to one 20 ms frame pair.
///
/// The ordering here is load-bearing and is the reason this is a function
/// rather than four inline statements:
///
/// * **System audio is muted _before_ the AEC runs.** The system frame is the
///   AEC's reference signal. Muting it first means the canceller is told
///   "nothing is playing", takes its silent-render bypass, and passes the mic
///   through untouched — which is what compliance mode has to mean. Muting
///   after would let the filter subtract audio the caller asked never to be
///   captured.
/// * **The microphone is muted _after_ the AEC runs.** The adaptive filter has
///   to keep observing the real captured signal, otherwise it de-converges
///   while the mic is muted and has to re-adapt from scratch on unmute.
///
/// `aec` is `None` when echo cancellation is off or the mic rate is below the
/// AEC's 16 kHz operating rate. On an AEC error `mic` keeps its captured
/// contents (still subject to the mic mute) and the error is returned.
fn apply_mutes_and_aec(
    mic: &mut [f32],
    sys: &mut [f32],
    mic_muted: bool,
    sys_muted: bool,
    aec: Option<&mut Canceler>,
) -> AecResult<()> {
    if sys_muted {
        sys.iter_mut().for_each(|s| *s = 0.0);
    }

    let result = match aec {
        Some(canceler) => canceler.cancel_speaker_echo(mic, sys),
        None => Ok(()),
    };

    if mic_muted {
        mic.iter_mut().for_each(|s| *s = 0.0);
    }

    result
}

/// Hand `emit` every whole `batch`-sized pair of chunks available on the front
/// of both accumulators, leaving the remainder buffered.
///
/// Parakeet's `transcribe` requires *exactly* `PARAKEET_BATCH_SAMPLES` and
/// assumes a contiguous stream, so a partial chunk must be kept rather than
/// padded, truncated or skipped — a gap garbles the model's rolling buffer.
/// Both accumulators are checked because they must stay sample-aligned; the
/// engine extends both by one frame per tick, so in practice they are always
/// the same length.
#[cfg(feature = "parakeet")]
fn drain_batch_pairs(
    mic_buf: &mut Vec<f32>,
    sys_buf: &mut Vec<f32>,
    batch: usize,
    mut emit: impl FnMut(Vec<f32>, Vec<f32>),
) {
    debug_assert!(batch > 0);
    while mic_buf.len() >= batch && sys_buf.len() >= batch {
        emit(mic_buf[..batch].to_vec(), sys_buf[..batch].to_vec());
        mic_buf.drain(0..batch);
        sys_buf.drain(0..batch);
    }
}

/// Runs the engine, delivering post-start failures to `error_tx` when supplied.
///
/// Returns when `shutdown` is notified from any [`crate::SplitStreamHandle`].
// With no backend feature compiled in, `BackendSpec` is an uninhabited enum, so
// the backend-selection match diverges and everything after it is dead code.
// `SplitStreamBuilder::start` rejects that configuration with
// `SplitStreamError::NoBackendFeature` before the engine is ever spawned.
#[cfg_attr(
    not(any(feature = "deepgram", feature = "whisper", feature = "parakeet")),
    allow(unreachable_code, unused_variables, unused_mut, clippy::never_loop)
)]
pub(crate) async fn run_with_errors(
    controls: Arc<Controls>,
    backend_spec: BackendSpec,
    transcript_tx: tokio::sync::broadcast::Sender<Transcript>,
    shutdown: Arc<Notify>,
    error_tx: Option<ErrorSender>,
    ready: Option<ReadySender>,
) -> Result<(), SplitStreamError> {
    let errors = ErrorReporter::new(error_tx);
    let mut ready = ready;

    // Hand a start-up failure back to `start()` so it can return `Err`, rather
    // than reporting it on the runtime error channel where a caller who never
    // asked for errors would see only an empty transcript stream. Returns
    // `Ok(())` in that case because the error now belongs to `start()`.
    macro_rules! fail_startup {
        ($e:expr) => {{
            let err = $e;
            match ready.take() {
                Some(tx) => {
                    // `start()` is gone if the send fails; the error channel is
                    // then all that is left to carry the failure.
                    if let Err(Err(e)) = tx.send(Err(err)) {
                        errors.report(e);
                    }
                    return Ok(());
                }
                // Already past bring-up: ordinary failure path.
                None => return Err(err),
            }
        }};
    }

    // --- Mic capture ---
    let mut mic = match mic_capture::setup() {
        Ok(m) => Some(m),
        Err(e) => {
            warn!(
                "No microphone available ({e}). Recording system audio only. \
                 To suppress this warning, call .mic_muted(true) on the builder."
            );
            errors.report(e);
            None
        }
    };

    // --- Sys capture ---
    let (_sys_tap, mut sys_cons) = match sys_capture::SysAudioTap::new() {
        Ok(v) => v,
        Err(e) => fail_startup!(e),
    };
    let mut sys_sample_rate = _sys_tap.sample_rate;

    info!(
        "Mic: {} | Sys: {}hz",
        mic.as_ref()
            .map_or("none".to_string(), |m| format!("{}hz", m.sample_rate)),
        sys_sample_rate
    );

    let mut mic_resampler =
        match resampler::build_resampler(mic.as_ref().map_or(OUT_SAMPLE_RATE, |m| m.sample_rate)) {
            Ok(r) => r,
            Err(e) => fail_startup!(e),
        };
    let mut sys_resampler = match resampler::build_resampler(sys_sample_rate) {
        Ok(r) => r,
        Err(e) => fail_startup!(e),
    };
    let mut mic_input_buf: Vec<f32> = Vec::new();
    let mut sys_input_buf: Vec<f32> = Vec::new();
    let mut aec = SendAec(Canceler::new());

    // --- Backend initialization ---
    let mut backend_actor = match backend_spec {
        #[cfg(feature = "deepgram")]
        BackendSpec::Deepgram(cfg) => {
            let ws_url = format!(
                "wss://api.deepgram.com/v1/listen?encoding=opus&sample_rate=16000\
                 &channels=2&multichannel=true&model={}",
                cfg.model
            );
            let b = match backends::deepgram::spawn(
                cfg.api_key,
                ws_url,
                transcript_tx.clone(),
                errors.clone(),
            ) {
                Ok(b) => b,
                Err(e) => fail_startup!(e),
            };
            BackendActor::Deepgram {
                opus_packet_tx: b.opus_packet_tx,
                encoder: b.encoder,
                alive: b.alive,
            }
        }

        #[cfg(feature = "whisper")]
        BackendSpec::Whisper(cfg) => {
            let window = cfg.window_seconds as usize * OUT_SAMPLE_RATE as usize;
            let (mic_tx, sys_tx) = match backends::whisper::spawn(
                &cfg.model_path,
                window,
                transcript_tx.clone(),
                errors.clone(),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => fail_startup!(e),
            };
            BackendActor::Whisper {
                mic_pcm_tx: mic_tx,
                sys_pcm_tx: sys_tx,
            }
        }

        #[cfg(feature = "parakeet")]
        BackendSpec::Parakeet(cfg) => {
            let (mic_tx, sys_tx) = match backends::parakeet::spawn(
                &cfg.model_dir,
                transcript_tx.clone(),
                errors.clone(),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => fail_startup!(e),
            };
            BackendActor::Parakeet {
                mic_pcm_tx: mic_tx,
                sys_pcm_tx: sys_tx,
            }
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
        if let Some(ref mut m) = mic {
            m.consumer.pop_slice(&mut tmp);
        }
        sys_cons.pop_slice(&mut tmp);
    }

    // Mid-stream rate adaptation counters (sys uses drain-count estimation).
    let mut sys_rate_acc: u64 = 0;
    let mut rate_ticks: u32 = 0;
    const RATE_CHECK_TICKS: u32 = 50; // 50 × 20ms = 1s
    // A rate change is acted on only after two consecutive seconds agree. One
    // second is not evidence: a partially idle second undercounts, and the
    // standard rates are close enough together that an undercount reads as a
    // lower rate rather than as nonsense. A real device change persists.
    let mut sys_rate_candidate: Option<u32> = None;

    // --- Rate-limited diagnostics for the 50Hz paths ---
    let mut mic_resample_log = Throttled::new("Mic resample failed");
    let mut sys_resample_log = Throttled::new("Sys resample failed");
    let mut aec_log = Throttled::new("Echo cancellation failed, passing mic through");
    #[cfg(feature = "deepgram")]
    let mut opus_log = Throttled::new("Opus encode failed");
    let mut mic_dropped_seen: u64 = 0;
    let mut sys_dropped_seen: u64 = 0;
    #[cfg(feature = "deepgram")]
    let mut backend_dead_logged = false;

    // --- Scratch reused by every tick ---
    // Sized to the whole ring, so a tick after a scheduling stall can drain the
    // entire backlog instead of leaving half of it for the next one.
    let mut drain_buf = vec![0.0f32; sys_capture::RING_CAPACITY];
    #[cfg(feature = "deepgram")]
    let mut interleaved = Vec::<f32>::with_capacity(FINAL_FRAME_SIZE * 2);
    #[cfg(feature = "deepgram")]
    let mut encode_buf = vec![0u8; 400];

    // Bring-up finished: devices open, resamplers built, backend running.
    // `start()` returns to its caller at this point.
    if let Some(tx) = ready.take() {
        let _ = tx.send(Ok(()));
    }

    loop {
        tokio::select! {
            biased;

            _ = shutdown.notified() => {
                // Tell the backends their channels are about to close on
                // purpose, so their exits are not reported as failures.
                errors.begin_shutdown();
                break;
            }

            _ = frame_tick.tick() => {
                // --- Drain ring buffers ---
                if let Some(ref mut m) = mic {
                    let n = m.consumer.pop_slice(&mut drain_buf);
                    if n > 0 { mic_input_buf.extend_from_slice(&drain_buf[..n]); }
                }
                let n = sys_cons.pop_slice(&mut drain_buf);
                if n > 0 {
                    sys_input_buf.extend_from_slice(&drain_buf[..n]);
                    sys_rate_acc += n as u64;
                }

                // --- Rate adaptation (every 1s) ---
                rate_ticks += 1;
                if rate_ticks >= RATE_CHECK_TICKS {
                    // Mic: poll device config; rebuild stream + resampler if rate changed.
                    if let Some(ref mut m) = mic {
                        if mic_capture::maybe_rebuild(m) {
                            match resampler::build_resampler(m.sample_rate) {
                                Ok(r) => {
                                    mic_resampler = r;
                                    mic_input_buf.clear();
                                }
                                // Keep the previous resampler: a wrong ratio is
                                // recoverable, no resampler at all is not.
                                Err(e) => errors.report(e),
                            }
                        }
                        report_drops("Mic", &m.dropped_samples, &mut mic_dropped_seen);
                    }
                    report_drops("Sys", &_sys_tap.dropped_samples, &mut sys_dropped_seen);

                    // Sys: drain-count estimation; rebuild resampler if rate changed.
                    // `None` means the count is not close to any real rate —
                    // a stalled or idle second, which is routine for WASAPI
                    // loopback. Keep the rate we have rather than believing it.
                    match resampler::infer_standard_rate(sys_rate_acc as u32) {
                        // Matches what we already use: nothing to do, and no
                        // candidate change is in flight any more.
                        Some(inferred) if inferred == sys_sample_rate => {
                            sys_rate_candidate = None;
                        }
                        Some(inferred) if sys_rate_candidate == Some(inferred) => {
                            info!(
                                "Sys rate change confirmed: {}Hz → {}Hz, rebuilding resampler",
                                sys_sample_rate, inferred
                            );
                            match resampler::build_resampler(inferred) {
                                Ok(r) => {
                                    sys_sample_rate = inferred;
                                    sys_resampler = r;
                                    sys_input_buf.clear();
                                }
                                Err(e) => errors.report(e),
                            }
                            sys_rate_candidate = None;
                        }
                        // First sighting. Wait for a second opinion.
                        Some(inferred) => {
                            debug!(
                                "Sys rate looks like {}Hz, not {}Hz — waiting for \
                                 confirmation before rebuilding",
                                inferred, sys_sample_rate
                            );
                            sys_rate_candidate = Some(inferred);
                        }
                        // Stalled or idle second: routine for WASAPI loopback,
                        // and no evidence about the rate either way.
                        None => sys_rate_candidate = None,
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
                            mic_resample_log.warn(format_args!("{e}"));
                            mic_input_buf.clear();
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
                            sys_resample_log.warn(format_args!("{e}"));
                            sys_input_buf.clear();
                        }
                    }
                }

                // --- Mute + AEC (see `apply_mutes_and_aec` for the ordering) ---
                // In-place: `out_mic` is overwritten on AEC success and left
                // as captured on failure. No per-frame allocation on this path.
                // Without the `aec` feature there is no canceller to run, so the
                // toggle stays observable through the handle but has no effect.
                let aec_active = cfg!(feature = "aec")
                    && mic.as_ref().is_some_and(|m| m.sample_rate >= OUT_SAMPLE_RATE)
                    && controls.echo_cancellation.load(Ordering::Relaxed);
                if let Err(e) = apply_mutes_and_aec(
                    &mut out_mic,
                    &mut out_sys,
                    controls.mic_muted.load(Ordering::Relaxed),
                    controls.sys_muted.load(Ordering::Relaxed),
                    aec_active.then_some(&mut aec.0),
                ) {
                    // A persistent AEC failure used to be completely
                    // invisible here; surface it, throttled.
                    aec_log.warn(format_args!("{e}"));
                }

                // --- Route to backend ---
                match &mut backend_actor {
                    // Unreachable when no backend feature is enabled
                    // (`backend_actor` is uninhabited there); required because
                    // `&mut !` counts as inhabited for exhaustiveness.
                    #[cfg(not(any(
                        feature = "deepgram",
                        feature = "whisper",
                        feature = "parakeet"
                    )))]
                    _ => {}
                    #[cfg(feature = "deepgram")]
                    BackendActor::Deepgram { encoder, opus_packet_tx, alive } => {
                        // The packet channel is unbounded and unread once the
                        // WebSocket task exits — stop feeding it.
                        if !alive.load(Ordering::Relaxed) {
                            if !backend_dead_logged {
                                warn!(
                                    "Deepgram backend has stopped; discarding audio \
                                     instead of queueing it"
                                );
                                backend_dead_logged = true;
                            }
                            continue;
                        }
                        interleaved.clear();
                        for i in 0..FINAL_FRAME_SIZE {
                            interleaved.push(out_mic[i]);
                            interleaved.push(out_sys[i]);
                        }
                        match encoder.encode_float(&interleaved, &mut encode_buf) {
                            // The packet has to be owned to cross the channel,
                            // so this one allocation stays.
                            Ok(len) => {
                                let _ = opus_packet_tx.send(encode_buf[..len].to_vec());
                            }
                            Err(e) => opus_log.warn(format_args!("{e}")),
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
                        drain_batch_pairs(
                            &mut local_mic_buf,
                            &mut local_sys_buf,
                            PARAKEET_BATCH_SAMPLES,
                            |mic_chunk, sys_chunk| {
                                mic_pcm_tx.send(mic_chunk).ok();
                                sys_pcm_tx.send(sys_chunk).ok();
                            },
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = FINAL_FRAME_SIZE;

    /// A frame with a distinctive, non-silent shape so muting is detectable.
    fn tone(amplitude: f32) -> Vec<f32> {
        (0..FRAME)
            .map(|i| amplitude * (i as f32 * 0.05).sin())
            .collect()
    }

    // --- mute ordering ---

    #[test]
    fn nothing_muted_and_no_aec_leaves_both_frames_alone() {
        let mut mic = tone(0.5);
        let mut sys = tone(0.9);
        let (mic0, sys0) = (mic.clone(), sys.clone());
        apply_mutes_and_aec(&mut mic, &mut sys, false, false, None).unwrap();
        assert_eq!(mic, mic0);
        assert_eq!(sys, sys0);
    }

    #[test]
    fn mic_mute_zeroes_only_the_mic() {
        let mut mic = tone(0.5);
        let mut sys = tone(0.9);
        let sys0 = sys.clone();
        apply_mutes_and_aec(&mut mic, &mut sys, true, false, None).unwrap();
        assert!(mic.iter().all(|&s| s == 0.0));
        assert_eq!(sys, sys0);
    }

    #[test]
    fn sys_mute_zeroes_only_the_sys_frame() {
        let mut mic = tone(0.5);
        let mut sys = tone(0.9);
        let mic0 = mic.clone();
        apply_mutes_and_aec(&mut mic, &mut sys, false, true, None).unwrap();
        assert_eq!(mic, mic0);
        assert!(sys.iter().all(|&s| s == 0.0));
    }

    #[cfg(feature = "aec")]
    #[test]
    fn sys_is_muted_before_the_aec_sees_it() {
        // With compliance mode on, the AEC's reference must already be silent.
        // The canceller's silent-render bypass then copies the mic through
        // verbatim, which is the observable proof of the ordering: if the
        // real system frame had reached the filter, the mic would have been
        // altered.
        let mut aec = EchoCanceler::new();
        let mut mic = tone(0.5);
        let mut sys = tone(0.9);
        let mic0 = mic.clone();

        apply_mutes_and_aec(&mut mic, &mut sys, false, true, Some(&mut aec)).unwrap();

        assert!(sys.iter().all(|&s| s == 0.0), "sys frame must be silenced");
        assert_eq!(
            mic, mic0,
            "muted sys means the AEC must pass the mic through"
        );
    }

    #[cfg(feature = "aec")]
    #[test]
    fn a_live_sys_frame_does_reach_the_aec() {
        // The counterpart to the test above: without the sys mute the same
        // input *is* modified, so the previous assertion is not vacuous.
        let mut aec = EchoCanceler::new();
        let mut mic = tone(0.5);
        let mut sys = tone(0.9);
        let mic0 = mic.clone();

        apply_mutes_and_aec(&mut mic, &mut sys, false, false, Some(&mut aec)).unwrap();

        assert_ne!(
            mic, mic0,
            "a non-silent reference must engage the canceller"
        );
    }

    #[cfg(feature = "aec")]
    #[test]
    fn mic_is_muted_after_the_aec_ran() {
        // Muting the mic must not stop the filter from adapting: the AEC is
        // still invoked (so it keeps converging), and only the frame handed to
        // the backend is zeroed.
        let mut aec = EchoCanceler::new();
        let mut mic = tone(0.5);
        let mut sys = tone(0.9);

        apply_mutes_and_aec(&mut mic, &mut sys, true, false, Some(&mut aec)).unwrap();
        assert!(mic.iter().all(|&s| s == 0.0));

        // Feed one more frame with the mic unmuted; the canceller is alive and
        // still altering the signal, i.e. the muted frame did not bypass it.
        let mut mic2 = tone(0.5);
        let mic2_0 = mic2.clone();
        let mut sys2 = tone(0.9);
        apply_mutes_and_aec(&mut mic2, &mut sys2, false, false, Some(&mut aec)).unwrap();
        assert_ne!(mic2, mic2_0);
    }

    #[cfg(feature = "aec")]
    #[test]
    fn both_muted_zeroes_both_frames() {
        let mut aec = EchoCanceler::new();
        let mut mic = tone(0.5);
        let mut sys = tone(0.9);
        apply_mutes_and_aec(&mut mic, &mut sys, true, true, Some(&mut aec)).unwrap();
        assert!(mic.iter().all(|&s| s == 0.0));
        assert!(sys.iter().all(|&s| s == 0.0));
    }

    #[cfg(feature = "aec")]
    #[test]
    fn an_aec_error_still_applies_the_mic_mute() {
        // Wrong frame size → the canceller rejects the frame. The mute must
        // still be honoured: a failing AEC must never leak muted audio.
        let mut aec = EchoCanceler::new();
        let mut mic = vec![0.5f32; 64];
        let mut sys = vec![0.9f32; 64];
        let res = apply_mutes_and_aec(&mut mic, &mut sys, true, false, Some(&mut aec));
        assert!(res.is_err());
        assert!(mic.iter().all(|&s| s == 0.0));
    }

    #[cfg(feature = "aec")]
    #[test]
    fn an_aec_error_leaves_an_unmuted_mic_as_captured() {
        let mut aec = EchoCanceler::new();
        let mut mic = vec![0.5f32; 64];
        let mut sys = vec![0.9f32; 64];
        let res = apply_mutes_and_aec(&mut mic, &mut sys, false, false, Some(&mut aec));
        assert!(res.is_err());
        assert!(mic.iter().all(|&s| s == 0.5));
    }

    // --- parakeet batching ---

    #[cfg(feature = "parakeet")]
    #[test]
    fn batching_emits_nothing_below_one_full_chunk() {
        let mut mic = vec![1.0f32; PARAKEET_BATCH_SAMPLES - 1];
        let mut sys = vec![2.0f32; PARAKEET_BATCH_SAMPLES - 1];
        let mut emitted = 0;
        drain_batch_pairs(&mut mic, &mut sys, PARAKEET_BATCH_SAMPLES, |_, _| {
            emitted += 1
        });
        assert_eq!(emitted, 0);
        assert_eq!(mic.len(), PARAKEET_BATCH_SAMPLES - 1, "input must be kept");
    }

    #[cfg(feature = "parakeet")]
    #[test]
    fn batching_emits_exact_chunks_and_keeps_the_remainder() {
        let extra = 7;
        let mut mic = vec![1.0f32; PARAKEET_BATCH_SAMPLES * 2 + extra];
        let mut sys = vec![2.0f32; PARAKEET_BATCH_SAMPLES * 2 + extra];
        let mut chunks: Vec<(usize, usize)> = Vec::new();
        drain_batch_pairs(&mut mic, &mut sys, PARAKEET_BATCH_SAMPLES, |m, s| {
            chunks.push((m.len(), s.len()))
        });
        assert_eq!(
            chunks,
            vec![
                (PARAKEET_BATCH_SAMPLES, PARAKEET_BATCH_SAMPLES),
                (PARAKEET_BATCH_SAMPLES, PARAKEET_BATCH_SAMPLES),
            ]
        );
        assert_eq!(mic.len(), extra);
        assert_eq!(sys.len(), extra);
    }

    #[cfg(feature = "parakeet")]
    #[test]
    fn batching_is_contiguous_across_calls() {
        // The model's rolling buffer assumes an unbroken stream, so every
        // sample must appear exactly once, in order, across successive ticks.
        let mut mic: Vec<f32> = Vec::new();
        let mut sys: Vec<f32> = Vec::new();
        let mut seen: Vec<f32> = Vec::new();
        let mut next = 0.0f32;

        // 20 ticks of one 320-sample frame each.
        for _ in 0..20 {
            for _ in 0..FRAME {
                mic.push(next);
                sys.push(next);
                next += 1.0;
            }
            drain_batch_pairs(&mut mic, &mut sys, PARAKEET_BATCH_SAMPLES, |m, s| {
                assert_eq!(m, s);
                seen.extend_from_slice(&m);
            });
        }

        let expected: Vec<f32> = (0..seen.len()).map(|i| i as f32).collect();
        assert_eq!(seen, expected);
        // 20 × 320 = 6400 samples → two whole 2560-sample chunks, 1280 left.
        assert_eq!(seen.len(), PARAKEET_BATCH_SAMPLES * 2);
        assert_eq!(mic.len(), 20 * FRAME - PARAKEET_BATCH_SAMPLES * 2);
    }

    #[cfg(feature = "parakeet")]
    #[test]
    fn batching_stops_when_one_side_runs_short() {
        // Desynchronised accumulators must not panic on the slice of the
        // shorter buffer; the pair is simply held back until both catch up.
        let mut mic = vec![1.0f32; PARAKEET_BATCH_SAMPLES * 2];
        let mut sys = vec![2.0f32; PARAKEET_BATCH_SAMPLES];
        let mut emitted = 0;
        drain_batch_pairs(&mut mic, &mut sys, PARAKEET_BATCH_SAMPLES, |_, _| {
            emitted += 1
        });
        assert_eq!(emitted, 1);
        assert_eq!(mic.len(), PARAKEET_BATCH_SAMPLES);
        assert!(sys.is_empty());
    }

    // --- throttled logging ---

    #[test]
    fn throttled_suppresses_within_the_interval() {
        let mut t = Throttled::new("test");
        t.warn(format_args!("first"));
        assert!(t.last.is_some());
        for _ in 0..10 {
            t.warn(format_args!("more"));
        }
        assert_eq!(t.suppressed, 10);
    }

    #[test]
    fn throttled_resets_the_counter_once_a_line_gets_through() {
        let mut t = Throttled::new("test");
        t.interval = Duration::from_millis(0);
        t.warn(format_args!("first"));
        t.warn(format_args!("second"));
        assert_eq!(t.suppressed, 0);
    }

    // --- drop reporting ---

    #[test]
    fn drops_are_reported_only_when_the_counter_grows() {
        let counter = AtomicU64::new(0);
        let mut seen = 0u64;
        report_drops("Mic", &counter, &mut seen);
        assert_eq!(seen, 0);

        counter.store(128, Ordering::Relaxed);
        report_drops("Mic", &counter, &mut seen);
        assert_eq!(seen, 128);

        // No further growth: `seen` must stay put rather than re-reporting.
        report_drops("Mic", &counter, &mut seen);
        assert_eq!(seen, 128);
    }
}
