use aec_rs::*;
use log::{debug, warn};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, EchoCancelError>;

#[derive(Debug, Error)]
pub enum EchoCancelError {
    #[error("buffer length mismatch: rec={rec}, echo={echo}, out={out}")]
    BufferLengthMismatch { rec: usize, echo: usize, out: usize },

    #[error("invalid frame size: expected {expected}, got {got}")]
    InvalidFrameSize { expected: usize, got: usize },

    #[error("input contains non-finite values (NaN or Inf)")]
    NonFiniteInput,

    #[error("empty frame")]
    EmptyFrame,
}

/// Speex-backed acoustic echo canceler.
///
/// All conversion buffers are preallocated: the engine calls this 50× per
/// second on the audio tick, so the hot path must not allocate.
pub struct EchoCanceler {
    aec: Aec,
    config: aec_rs::AecConfig,
    // Bypass AEC if render frame is silent (copy input to output)
    bypass_on_silent_render: bool,
    // Preallocated scratch — never resized on the hot path.
    rec_f32: Vec<f32>,
    rec_i16: Vec<i16>,
    echo_i16: Vec<i16>,
    out_i16: Vec<i16>,
}

impl EchoCanceler {
    pub fn new() -> Self {
        let config = aec_rs::AecConfig {
            sample_rate: 16000,      // 16Khz (1s)
            filter_length: 3200,     // 0.2s
            frame_size: 320,         // 0.02s
            enable_preprocess: true, // Denoise as well
        };
        let aec = aec_rs::Aec::new(&config);
        let frame = config.frame_size;
        Self {
            aec,
            config,
            bypass_on_silent_render: true,
            rec_f32: vec![0.0; frame],
            rec_i16: vec![0; frame],
            echo_i16: vec![0; frame],
            out_i16: vec![0; frame],
        }
    }

    /// Cancel `echo_buffer` out of `rec_buffer`, writing the result into
    /// `out_buffer`. All three must be exactly `config.frame_size` long.
    ///
    /// `out_buffer` is left untouched when an error is returned.
    pub fn cancel_echo_f32(
        &mut self,
        rec_buffer: &[f32],
        echo_buffer: &[f32],
        out_buffer: &mut [f32],
    ) -> Result<()> {
        let rec_len = rec_buffer.len();
        if rec_len == 0 {
            return Err(EchoCancelError::EmptyFrame);
        }
        if echo_buffer.len() != rec_len || out_buffer.len() != rec_len {
            return Err(EchoCancelError::BufferLengthMismatch {
                rec: rec_len,
                echo: echo_buffer.len(),
                out: out_buffer.len(),
            });
        }
        let expected = self.config.frame_size;
        if rec_len != expected {
            return Err(EchoCancelError::InvalidFrameSize {
                expected,
                got: rec_len,
            });
        }
        if rec_buffer.iter().any(|x| !x.is_finite()) || echo_buffer.iter().any(|x| !x.is_finite()) {
            return Err(EchoCancelError::NonFiniteInput);
        }

        // Optional: warn on clipping
        let clipped_rec = rec_buffer
            .iter()
            .filter(|&&x| !(-1.0..=1.0).contains(&x))
            .count();
        let clipped_echo = echo_buffer
            .iter()
            .filter(|&&x| !(-1.0..=1.0).contains(&x))
            .count();
        if clipped_rec > 0 || clipped_echo > 0 {
            warn!(
                "Clipping detected: rec {}/{} samples, echo {}/{} samples",
                clipped_rec, rec_len, clipped_echo, rec_len
            );
        }

        // Split the borrow so the Speex state and the scratch buffers can be
        // used together without cloning anything.
        let Self {
            aec,
            bypass_on_silent_render,
            rec_i16,
            echo_i16,
            out_i16,
            ..
        } = self;

        // Scratch is sized for `frame_size` at construction; `rec_len` is
        // already checked to equal it, so these are no-ops in steady state.
        debug_assert_eq!(rec_i16.len(), rec_len);

        for (dst, &x) in rec_i16.iter_mut().zip(rec_buffer) {
            *dst = (x * 32767.0).clamp(-32768.0, 32767.0) as i16;
        }
        for (dst, &x) in echo_i16.iter_mut().zip(echo_buffer) {
            *dst = (x * 32767.0).clamp(-32768.0, 32767.0) as i16;
        }

        let silent_render = echo_i16.iter().all(|&x| x == 0);
        // Fast-path bypass for silent render if enabled
        if *bypass_on_silent_render && silent_render {
            debug!("Render frame is silent; bypassing AEC and copying input to output");
            out_buffer.copy_from_slice(rec_buffer);
            return Ok(());
        } else if silent_render {
            warn!("echo_buffer is all zeros — no echo signal to cancel");
        }

        aec.cancel_echo(rec_i16, echo_i16, out_i16);

        for (dst, &val) in out_buffer.iter_mut().zip(out_i16.iter()) {
            *dst = val as f32 / 32768.0;
        }

        Ok(())
    }

    /// In-place variant used by the engine: `capture` is overwritten with the
    /// echo-cancelled signal, and left untouched if an error is returned.
    ///
    /// Allocation-free in steady state.
    pub fn cancel_speaker_echo(&mut self, capture: &mut [f32], render: &[f32]) -> Result<()> {
        if self.rec_f32.len() != capture.len() {
            // Only reachable on a frame-size mismatch, which the validation in
            // `cancel_echo_f32` is about to reject anyway.
            self.rec_f32.resize(capture.len(), 0.0);
        }
        let mut rec = std::mem::take(&mut self.rec_f32);
        rec.copy_from_slice(capture);
        let res = self.cancel_echo_f32(&rec, render, capture);
        self.rec_f32 = rec;
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aec() -> EchoCanceler {
        EchoCanceler::new()
    }

    fn silent(n: usize) -> Vec<f32> {
        vec![0.0f32; n]
    }

    #[test]
    fn empty_frame_returns_error() {
        let mut ec = aec();
        let mut out = vec![];
        assert!(matches!(
            ec.cancel_echo_f32(&[], &[], &mut out),
            Err(EchoCancelError::EmptyFrame)
        ));
    }

    #[test]
    fn mismatched_lengths_returns_error() {
        let mut ec = aec();
        let rec = silent(320);
        let echo = silent(319);
        let mut out = silent(320);
        assert!(matches!(
            ec.cancel_echo_f32(&rec, &echo, &mut out),
            Err(EchoCancelError::BufferLengthMismatch { .. })
        ));
    }

    #[test]
    fn wrong_frame_size_returns_error() {
        let mut ec = aec();
        let buf = silent(100);
        let mut out = silent(100);
        assert!(matches!(
            ec.cancel_echo_f32(&buf, &buf, &mut out),
            Err(EchoCancelError::InvalidFrameSize { .. })
        ));
    }

    #[test]
    fn nan_in_rec_returns_error() {
        let mut ec = aec();
        let mut rec = silent(320);
        rec[0] = f32::NAN;
        let echo = silent(320);
        let mut out = silent(320);
        assert!(matches!(
            ec.cancel_echo_f32(&rec, &echo, &mut out),
            Err(EchoCancelError::NonFiniteInput)
        ));
    }

    #[test]
    fn inf_in_echo_returns_error() {
        let mut ec = aec();
        let rec = silent(320);
        let mut echo = silent(320);
        echo[10] = f32::INFINITY;
        let mut out = silent(320);
        assert!(matches!(
            ec.cancel_echo_f32(&rec, &echo, &mut out),
            Err(EchoCancelError::NonFiniteInput)
        ));
    }

    #[test]
    fn silent_render_bypass_copies_input() {
        let mut ec = aec();
        let mut rec = silent(320);
        rec[5] = 0.5;
        rec[100] = -0.3;
        let echo = silent(320); // all-zero render
        let mut out = silent(320);
        ec.cancel_echo_f32(&rec, &echo, &mut out).unwrap();
        assert_eq!(out, rec);
    }

    #[test]
    fn valid_frame_returns_ok() {
        let mut ec = aec();
        let rec = silent(320);
        let mut echo = silent(320);
        echo[0] = 0.1; // non-zero so AEC actually runs
        let mut out = silent(320);
        assert!(ec.cancel_echo_f32(&rec, &echo, &mut out).is_ok());
    }

    // ---------------------------------------------------------------------
    // Behavioural tests
    //
    // The tests above only exercise input validation — they would all still
    // pass if `cancel_echo` were a no-op. These check that the canceller
    // actually cancels.
    // ---------------------------------------------------------------------

    const FRAME: usize = 320;

    /// Deterministic pseudo-random far-end signal (a linear congruential
    /// generator, so the test never depends on an RNG crate or on wall-clock
    /// seeding).
    fn far_end(frames: usize) -> Vec<Vec<f32>> {
        let mut state: u32 = 0x1234_5678;
        (0..frames)
            .map(|_| {
                (0..FRAME)
                    .map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        // Scale into roughly [-0.5, 0.5].
                        ((state >> 8) as f32 / (1u32 << 23) as f32) - 0.5
                    })
                    .collect()
            })
            .collect()
    }

    /// Energy ratio between what went in and what came out, in dB.
    /// Higher is better: 0 dB means the canceller removed nothing.
    fn erle_db(rec: &[f32], out: &[f32]) -> f32 {
        let e_in: f64 = rec.iter().map(|&x| (x as f64) * (x as f64)).sum();
        let e_out: f64 = out.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (10.0 * (e_in / (e_out + 1e-12)).log10()) as f32
    }

    #[test]
    fn erle_improves_as_the_filter_converges() {
        // Mic hears an attenuated, one-sample-delayed copy of what the
        // speakers are playing and nothing else, so a converged canceller
        // should drive the output towards silence.
        const FRAMES: usize = 200;
        let render = far_end(FRAMES);

        let mut ec = aec();
        let mut out = silent(FRAME);
        let mut prev_tail = 0.0f32;
        let mut erle = Vec::with_capacity(FRAMES);

        for echo in &render {
            let mut rec = Vec::with_capacity(FRAME);
            for (i, &x) in echo.iter().enumerate() {
                let delayed = if i == 0 { prev_tail } else { echo[i - 1] };
                rec.push(0.5 * delayed + 0.05 * x);
            }
            prev_tail = echo[FRAME - 1];

            ec.cancel_echo_f32(&rec, echo, &mut out)
                .expect("valid frame");
            erle.push(erle_db(&rec, &out));
        }

        let mean = |s: &[f32]| s.iter().sum::<f32>() / s.len() as f32;
        let early = mean(&erle[..20]);
        let late = mean(&erle[FRAMES - 20..]);

        assert!(
            late > early,
            "AEC did not converge: early ERLE {early:.2} dB, late ERLE {late:.2} dB"
        );
        assert!(
            late > 3.0,
            "converged ERLE is implausibly low: {late:.2} dB (early {early:.2} dB)"
        );
    }

    #[test]
    fn a_converged_canceller_leaves_near_end_speech_audible() {
        // The counterpart to the convergence test: cancelling the echo must
        // not also cancel the person talking.
        const FRAMES: usize = 200;
        let render = far_end(FRAMES);
        let mut ec = aec();
        let mut out = silent(FRAME);

        // Converge on echo alone.
        for echo in &render {
            let rec: Vec<f32> = echo.iter().map(|&x| 0.5 * x).collect();
            ec.cancel_echo_f32(&rec, echo, &mut out).unwrap();
        }

        // Now add a loud near-end tone on top of the same echo.
        let echo = &render[FRAMES - 1];
        let rec: Vec<f32> = echo
            .iter()
            .enumerate()
            .map(|(i, &x)| 0.5 * x + 0.4 * (i as f32 * 0.2).sin())
            .collect();
        ec.cancel_echo_f32(&rec, echo, &mut out).unwrap();

        let energy: f32 = out.iter().map(|x| x * x).sum();
        assert!(
            energy > 1e-3,
            "near-end speech was suppressed along with the echo (energy {energy:e})"
        );
    }

    #[test]
    fn out_of_range_input_is_accepted_and_clamped() {
        // Levels beyond ±1.0 are logged as clipping but must not be rejected,
        // and the i16 round-trip must not wrap a +2.0 sample into a negative
        // one.
        let mut ec = aec();
        let rec: Vec<f32> = (0..FRAME)
            .map(|i| if i % 2 == 0 { 2.0 } else { -3.0 })
            .collect();
        let echo: Vec<f32> = (0..FRAME).map(|i| 0.9 + (i % 3) as f32).collect();
        let mut out = silent(FRAME);

        ec.cancel_echo_f32(&rec, &echo, &mut out)
            .expect("clipping is not an error");
        assert!(
            out.iter()
                .all(|v| v.is_finite() && (-1.01..=1.01).contains(v)),
            "output escaped the representable range"
        );
    }

    #[test]
    fn in_place_variant_matches_the_out_of_place_one() {
        let render = far_end(30);
        let mut a = aec();
        let mut b = aec();
        let mut out = silent(FRAME);

        for echo in &render {
            let rec: Vec<f32> = echo.iter().map(|&x| 0.5 * x).collect();
            a.cancel_echo_f32(&rec, echo, &mut out).unwrap();

            let mut in_place = rec.clone();
            b.cancel_speaker_echo(&mut in_place, echo).unwrap();

            assert_eq!(in_place, out);
        }
    }

    #[test]
    fn in_place_variant_leaves_the_capture_buffer_untouched_on_error() {
        let mut ec = aec();
        let mut capture = vec![0.25f32; 64]; // wrong frame size
        let render = vec![0.5f32; 64];
        assert!(ec.cancel_speaker_echo(&mut capture, &render).is_err());
        assert!(capture.iter().all(|&v| v == 0.25));
    }
}
