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

pub struct EchoCanceler {
    aec: Aec,
    config: aec_rs::AecConfig,
    // Bypass AEC if render frame is silent (copy input to output)
    bypass_on_silent_render: bool,
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
        Self {
            aec,
            config,
            bypass_on_silent_render: true,
        }
    }

    pub fn cancel_echo_f32(
        &self,
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
        let expected = self.config.frame_size as usize;
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
        let clipped_rec = rec_buffer.iter().filter(|&&x| x < -1.0 || x > 1.0).count();
        let clipped_echo = echo_buffer.iter().filter(|&&x| x < -1.0 || x > 1.0).count();
        if clipped_rec > 0 || clipped_echo > 0 {
            warn!(
                "Clipping detected: rec {}/{} samples, echo {}/{} samples",
                clipped_rec, rec_len, clipped_echo, rec_len
            );
        }

        debug!(
            "Debug: rec_buffer len={}, echo_buffer len={}, out_buffer len={}",
            rec_buffer.len(),
            echo_buffer.len(),
            out_buffer.len()
        );

        let rec_i16: Vec<i16> = rec_buffer
            .iter()
            .map(|&x| (x * 32767.0).clamp(-32768.0, 32767.0) as i16)
            .collect();
        let echo_i16: Vec<i16> = echo_buffer
            .iter()
            .map(|&x| (x * 32767.0).clamp(-32768.0, 32767.0) as i16)
            .collect();

        // Fast-path bypass for silent render if enabled
        if self.bypass_on_silent_render && echo_i16.iter().all(|&x| x == 0) {
            debug!("Render frame is silent; bypassing AEC and copying input to output");
            out_buffer.copy_from_slice(rec_buffer);
            return Ok(());
        } else if echo_i16.iter().all(|&x| x == 0) {
            warn!("Warning: echo_buffer is all zeros—no echo signal to cancel!");
        }

        debug!(
            "Debug: First 5 rec_i16: {:?}",
            &rec_i16[..5.min(rec_i16.len())]
        );
        debug!(
            "Debug: First 5 echo_i16: {:?}",
            &echo_i16[..5.min(echo_i16.len())]
        );

        let rec_rms = (rec_i16.iter().map(|&x| (x as f32).powi(2)).sum::<f32>()
            / rec_i16.len() as f32)
            .sqrt();
        let echo_rms = (echo_i16.iter().map(|&x| (x as f32).powi(2)).sum::<f32>()
            / echo_i16.len() as f32)
            .sqrt();
        debug!("Debug: rec_i16 RMS={}, echo_i16 RMS={}", rec_rms, echo_rms);

        // Make sure out_i16 matches input length
        let mut out_i16: Vec<i16> = vec![0; rec_i16.len()];

        self.aec.cancel_echo(&rec_i16, &echo_i16, &mut out_i16);

        let out_rms = (out_i16.iter().map(|&x| (x as f32).powi(2)).sum::<f32>()
            / out_i16.len() as f32)
            .sqrt();
        debug!("Debug: out_i16 RMS after AEC={}", out_rms);

        debug!(
            "Debug: First 5 out_i16 after AEC: {:?}",
            &out_i16[..5.min(out_i16.len())]
        );

        for (i, &val) in out_i16.iter().enumerate() {
            out_buffer[i] = val as f32 / 32768.0;
        }

        debug!(
            "Debug: First 5 out_buffer after f32 conversion: {:?}",
            &out_buffer[..5.min(out_buffer.len())]
        );

        Ok(())
    }

    pub fn cancel_speaker_echo(
        &mut self,
        mut capture_frame: Vec<f32>,
        render_frame: Vec<f32>,
    ) -> Result<Vec<f32>> {
        debug!(
            "Debug: capture_frame len={}, render_frame len={}",
            capture_frame.len(),
            render_frame.len()
        );

        let rec_clone = capture_frame.clone();
        self.cancel_echo_f32(&rec_clone, &render_frame, &mut capture_frame)?;

        debug!(
            "Debug: First 5 capture_frame after cancellation: {:?}",
            &capture_frame[..5.min(capture_frame.len())]
        );

        Ok(capture_frame)
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
        let ec = aec();
        let mut out = vec![];
        assert!(matches!(
            ec.cancel_echo_f32(&[], &[], &mut out),
            Err(EchoCancelError::EmptyFrame)
        ));
    }

    #[test]
    fn mismatched_lengths_returns_error() {
        let ec = aec();
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
        let ec = aec();
        let buf = silent(100);
        let mut out = silent(100);
        assert!(matches!(
            ec.cancel_echo_f32(&buf, &buf, &mut out),
            Err(EchoCancelError::InvalidFrameSize { .. })
        ));
    }

    #[test]
    fn nan_in_rec_returns_error() {
        let ec = aec();
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
        let ec = aec();
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
        let ec = aec();
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
        let ec = aec();
        let rec = silent(320);
        let mut echo = silent(320);
        echo[0] = 0.1; // non-zero so AEC actually runs
        let mut out = silent(320);
        assert!(ec.cancel_echo_f32(&rec, &echo, &mut out).is_ok());
    }
}
