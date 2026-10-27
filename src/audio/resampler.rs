use rubato::{
    Resampler, SincFixedOut, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use crate::error::SplitStreamError;

pub(crate) const OUT_SAMPLE_RATE: u32 = 16_000;
pub(crate) const FINAL_FRAME_SIZE: usize = 320; // 20ms @ 16kHz

/// Single-channel SincFixedOut resampler: `in_sample_rate` → 16kHz, 320-sample output.
///
/// Fails for `in_sample_rate == 0`, which the engine's drain-count rate
/// estimation can produce on a stalled device, and for any ratio rubato itself
/// rejects. Callers keep their previous resampler on failure — a stale ratio
/// is recoverable, no resampler at all is not.
///
/// The zero case is checked here rather than left to rubato: `16000 / 0` is
/// `+inf`, which passes rubato's `ratio > 0` validation and yields a resampler
/// that cannot produce output.
pub(crate) fn build_resampler(in_sample_rate: u32) -> Result<SincFixedOut<f32>, SplitStreamError> {
    if in_sample_rate == 0 {
        return Err(SplitStreamError::ResamplerInit {
            in_rate: 0,
            source: "input sample rate is zero".into(),
        });
    }
    let resample_ratio = OUT_SAMPLE_RATE as f64 / in_sample_rate as f64;

    let parameters = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        oversampling_factor: 128,
        interpolation: SincInterpolationType::Nearest,
        window: WindowFunction::Hann,
    };

    SincFixedOut::<f32>::new(resample_ratio, 3.1, parameters, FINAL_FRAME_SIZE, 1).map_err(|e| {
        SplitStreamError::ResamplerInit {
            in_rate: in_sample_rate,
            source: Box::new(e),
        }
    })
}

/// Map an observed sample count (samples drained in 1 second) to the nearest
/// standard audio sample rate. Handles mid-stream Bluetooth codec switches.
pub(crate) fn snap_to_standard_rate(observed: u32) -> u32 {
    // SAFETY(infallible): RATES is a non-empty compile-time constant slice, so
    // `min_by_key` over it always yields `Some`.
    *RATES.iter().min_by_key(|&&r| r.abs_diff(observed)).unwrap()
}

/// Sample rates a consumer audio device plausibly runs at.
const RATES: &[u32] = &[
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000,
];

/// How far an observed rate may sit from a standard rate and still be believed,
/// as a fraction of that rate.
///
/// This only rejects counts that are near-dead — the rate ladder is too dense
/// for a tolerance to separate "48kHz undercounted" from "22.05kHz", since
/// 44.1kHz and 48kHz are themselves only 8.8% apart. Catching an undercount is
/// the engine's job, by requiring the same rate twice running.
const RATE_TOLERANCE: f32 = 0.15;

/// The standard rate `observed` samples-per-second corresponds to, or `None` if
/// it is not close to any of them.
///
/// Everything is transcribed at 16kHz, so this number's only job is to set the
/// resampler's input side — the ratio. That is what makes a wrong answer worse
/// than no answer: the output stays 16kHz either way, but the audio inside it
/// is resampled at the wrong speed, so the model is fed a pitch-shifted stream
/// and returns plausible-looking nonsense rather than silence.
///
/// The engine infers the rate by counting samples drained over a second, which
/// only means something while audio is actually flowing. A partially idle
/// second produces a small count that would otherwise *snap* to a lower
/// standard rate and trigger a rebuild at that wrong ratio.
///
/// This is the normal idle state on Windows, not an edge case: WASAPI loopback
/// delivers nothing at all while the render endpoint is idle, so any second
/// containing both audio and silence undercounts. Rejecting an implausible
/// observation and keeping the current rate is always better than believing it.
pub(crate) fn infer_standard_rate(observed: u32) -> Option<u32> {
    let nearest = snap_to_standard_rate(observed);
    let allowed = (nearest as f32 * RATE_TOLERANCE) as u32;
    (nearest.abs_diff(observed) <= allowed).then_some(nearest)
}

#[allow(dead_code)]
pub(crate) fn change_resampler_in_rate(
    resampler: &mut SincFixedOut<f32>,
    new_in_rate: u32,
) -> Result<(), rubato::ResampleError> {
    let new_ratio = OUT_SAMPLE_RATE as f64 / new_in_rate as f64;
    Resampler::set_resample_ratio(resampler, new_ratio, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rubato::Resampler;

    #[test]
    fn test_build_resampler_and_required_input_positive() {
        let resampler = build_resampler(48_000).expect("48kHz resampler is constructible");
        let required = Resampler::input_frames_next(&resampler);
        assert!(required > 0);
    }

    #[test]
    fn test_resampler_processes_silence_and_outputs_320() {
        let mut resampler = build_resampler(48_000).expect("48kHz resampler is constructible");
        let required = Resampler::input_frames_next(&resampler);

        let ch0 = vec![0.0_f32; required];
        let wave_in = [&ch0[..]];
        let mut out = [0.0_f32; FINAL_FRAME_SIZE];
        let mut wave_out = [&mut out[..]];

        let (_used, produced) =
            Resampler::process_into_buffer(&mut resampler, &wave_in, &mut wave_out, Some(&[true]))
                .expect("resample should succeed");

        assert_eq!(produced, FINAL_FRAME_SIZE);
        assert!(out.iter().all(|v| v.abs() < 1e-6));
    }

    #[test]
    fn test_change_resampler_in_rate_changes_required_input() {
        let mut resampler = build_resampler(48_000).expect("48kHz resampler is constructible");
        let req_48k = Resampler::input_frames_next(&resampler);
        change_resampler_in_rate(&mut resampler, 44_100).expect("ratio change ok");
        let req_44k = Resampler::input_frames_next(&resampler);
        assert!(req_44k < req_48k);
    }

    #[test]
    fn test_change_resampler_in_rate_zero_is_error() {
        let mut resampler = build_resampler(48_000).expect("48kHz resampler is constructible");
        let res = change_resampler_in_rate(&mut resampler, 0);
        assert!(res.is_err());
    }

    #[test]
    fn snap_exact_standard_rates() {
        assert_eq!(snap_to_standard_rate(48_000), 48_000);
        assert_eq!(snap_to_standard_rate(44_100), 44_100);
        assert_eq!(snap_to_standard_rate(16_000), 16_000);
        assert_eq!(snap_to_standard_rate(8_000), 8_000);
    }

    #[test]
    fn snap_rounds_to_nearest() {
        // 46000: |46000-44100|=1900 < |46000-48000|=2000 → 44100
        assert_eq!(snap_to_standard_rate(46_000), 44_100);
        // 47000: |47000-48000|=1000 < |47000-44100|=2900 → 48000
        assert_eq!(snap_to_standard_rate(47_000), 48_000);
    }

    #[test]
    fn snap_near_zero_returns_lowest_rate() {
        // `snap_to_standard_rate` is still pure "nearest"; the judgement about
        // whether to believe it lives in `infer_standard_rate`.
        assert_eq!(snap_to_standard_rate(100), 8_000);
    }

    // --- 44.1 kHz fixture ---

    /// One second of a `freq` Hz sine at `rate`.
    fn sine(freq: f32, rate: u32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Drive `resampler` with `input` and collect every full 320-sample frame
    /// it produces.
    fn run(resampler: &mut SincFixedOut<f32>, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        loop {
            let required = Resampler::input_frames_next(resampler);
            if pos + required > input.len() {
                break;
            }
            let wave_in = [&input[pos..pos + required]];
            let mut frame = [0.0f32; FINAL_FRAME_SIZE];
            let mut wave_out = [&mut frame[..]];
            let (_used, produced) =
                Resampler::process_into_buffer(resampler, &wave_in, &mut wave_out, Some(&[true]))
                    .expect("resample should succeed");
            assert_eq!(
                produced, FINAL_FRAME_SIZE,
                "SincFixedOut must fill the frame"
            );
            out.extend_from_slice(&frame);
            pos += required;
        }
        out
    }

    #[test]
    fn resamples_44100_to_16000_at_the_expected_rate() {
        // 44.1kHz is the other rate macOS hands out routinely (Bluetooth,
        // most external interfaces) and the only non-integer ratio involved.
        let mut resampler = build_resampler(44_100).expect("44.1kHz resampler is constructible");
        let input = sine(440.0, 44_100, 44_100); // one second
        let out = run(&mut resampler, &input);

        // One second in should give roughly one second out (16000 samples),
        // rounded down to whole 320-sample frames and minus the filter's
        // startup latency.
        let frames = out.len() / FINAL_FRAME_SIZE;
        assert!(
            (45..=50).contains(&frames),
            "expected ~50 frames from one second of 44.1kHz audio, got {frames}"
        );
    }

    #[test]
    fn resampling_44100_preserves_signal_level() {
        // A resampler that silently produced zeros would pass a length-only
        // assertion, so check the energy survives.
        let mut resampler = build_resampler(44_100).expect("44.1kHz resampler is constructible");
        let input = sine(440.0, 44_100, 44_100);
        let out = run(&mut resampler, &input);

        // Skip the filter's startup ramp before measuring.
        let steady = &out[FINAL_FRAME_SIZE * 2..];
        let rms = (steady.iter().map(|x| (x * x) as f64).sum::<f64>() / steady.len() as f64).sqrt();
        // A 0.5-amplitude sine has an RMS of 0.5/√2 ≈ 0.354.
        assert!(
            (0.30..0.40).contains(&rms),
            "resampled RMS {rms:.4} is not that of the input sine"
        );
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn resamples_16000_to_16000_as_a_passthrough_ratio() {
        let mut resampler = build_resampler(16_000).expect("16kHz resampler is constructible");
        // The first call asks for extra input to prime the sinc filter; the
        // steady state settles on exactly one output frame per input frame.
        assert!(Resampler::input_frames_next(&resampler) >= FINAL_FRAME_SIZE);
        let input = sine(440.0, 16_000, 16_000);
        let out = run(&mut resampler, &input);
        assert!(!out.is_empty());
        assert_eq!(out.len() % FINAL_FRAME_SIZE, 0);
        assert_eq!(Resampler::input_frames_next(&resampler), FINAL_FRAME_SIZE);
    }

    // --- construction failures ---

    #[test]
    fn a_zero_input_rate_is_an_error_not_an_unusable_resampler() {
        // `16000 / 0` is `+inf`, which satisfies rubato's `ratio > 0` check.
        // Without the explicit guard the engine would install a resampler that
        // never yields a frame, and the drain-count estimator can reach zero
        // whenever the device stalls.
        match build_resampler(0) {
            Err(SplitStreamError::ResamplerInit { in_rate, .. }) => assert_eq!(in_rate, 0),
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("a zero input rate must not build a resampler"),
        }
    }

    #[test]
    fn the_resampler_error_names_the_rate_and_chains_its_cause() {
        let Err(err) = build_resampler(0) else {
            panic!("a zero input rate must not build a resampler")
        };
        let rendered = err.to_string();
        assert!(rendered.contains('0'), "{rendered}");
        assert!(rendered.contains("16000"), "{rendered}");
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn every_standard_rate_is_constructible() {
        // `snap_to_standard_rate` can return any of these, and the engine
        // feeds its result straight into `build_resampler`.
        for rate in [
            8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000,
        ] {
            assert!(build_resampler(rate).is_ok(), "{rate}Hz");
        }
    }

    #[test]
    fn a_stalled_drain_count_is_rejected_rather_than_snapped() {
        // The case this guards: a device draining ~100 samples/s is stalled or
        // idle, not running at 8kHz. Believing it would rebuild the resampler
        // at the wrong rate. This is the normal idle state for WASAPI loopback.
        assert_eq!(infer_standard_rate(100), None);
        assert_eq!(infer_standard_rate(0), None);
        assert_eq!(infer_standard_rate(1_000), None);
        // Below the lowest plausible rate minus the tolerance.
        assert_eq!(infer_standard_rate(6_000), None, "under 8kHz - 15%");
    }

    /// The limit of what this function can do alone, recorded so the engine's
    /// confirmation step is not mistaken for belt-and-braces.
    #[test]
    fn an_undercount_can_still_look_like_a_real_rate() {
        // Half a second of 48kHz audio reads as a plausible 22.05kHz, because
        // the standard rates are too closely spaced to tell the two apart.
        assert_eq!(infer_standard_rate(24_000), Some(22_050));
    }

    #[test]
    fn a_real_rate_is_believed_even_with_clock_drift() {
        for rate in [8_000, 16_000, 44_100, 48_000, 96_000] {
            assert_eq!(infer_standard_rate(rate), Some(rate), "exact {rate}");
            // A tick landing a little early or late must not be discarded.
            // 2% is the useful bound: 44.1kHz and 48kHz are 8.8% apart, so a
            // larger drift legitimately rounds to the neighbouring rate.
            assert_eq!(
                infer_standard_rate(rate + rate / 50),
                Some(rate),
                "drifted {rate}"
            );
            assert_eq!(
                infer_standard_rate(rate - rate / 50),
                Some(rate),
                "short {rate}"
            );
        }
    }

    #[test]
    fn every_believed_rate_builds_a_resampler() {
        // Whatever the inference accepts, the engine must be able to act on.
        for observed in [8_000u32, 16_000, 22_050, 44_100, 48_000, 96_000] {
            let rate = infer_standard_rate(observed).expect("a standard rate");
            assert!(build_resampler(rate).is_ok(), "rate {rate}");
        }
    }
}
