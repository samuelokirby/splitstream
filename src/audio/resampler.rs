use fixed_resample::rubato::{
    Resampler, SincFixedOut, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

pub(crate) const OUT_SAMPLE_RATE: u32 = 16_000;
pub(crate) const FINAL_FRAME_SIZE: usize = 320; // 20ms @ 16kHz

/// Single-channel SincFixedOut resampler: `in_sample_rate` → 16kHz, 320-sample output.
pub(crate) fn build_resampler(in_sample_rate: u32) -> SincFixedOut<f32> {
    let resample_ratio = OUT_SAMPLE_RATE as f64 / in_sample_rate as f64;

    let parameters = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        oversampling_factor: 128,
        interpolation: SincInterpolationType::Nearest,
        window: WindowFunction::Hann,
    };

    SincFixedOut::<f32>::new(resample_ratio, 3.1, parameters, FINAL_FRAME_SIZE, 1)
        .expect("failed to create resampler")
}

/// Map an observed sample count (samples drained in 1 second) to the nearest
/// standard audio sample rate. Handles mid-stream Bluetooth codec switches.
pub(crate) fn snap_to_standard_rate(observed: u32) -> u32 {
    const RATES: &[u32] = &[8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000];
    *RATES.iter().min_by_key(|&&r| r.abs_diff(observed)).unwrap()
}

#[allow(dead_code)]
pub(crate) fn change_resampler_in_rate(
    resampler: &mut SincFixedOut<f32>,
    new_in_rate: u32,
) -> Result<(), fixed_resample::rubato::ResampleError> {
    let new_ratio = OUT_SAMPLE_RATE as f64 / new_in_rate as f64;
    Resampler::set_resample_ratio(resampler, new_ratio, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixed_resample::rubato::Resampler;

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

        let (_used, produced) =
            Resampler::process_into_buffer(&mut resampler, &wave_in, &mut wave_out, Some(&[true]))
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
        assert_eq!(snap_to_standard_rate(100), 8_000);
    }
}
