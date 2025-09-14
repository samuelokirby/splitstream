use log::{debug, warn};

use aec_rs::*;

pub struct EchoCanceler {
    aec: Aec,
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
        Self { aec }
    }

    pub fn cancel_echo_f32(&self, rec_buffer: &[f32], echo_buffer: &[f32], out_buffer: &mut [f32]) {
        // Debug: Log buffer lengths
        debug!(
            "Debug: rec_buffer len={}, echo_buffer len={}, out_buffer len={}",
            rec_buffer.len(),
            echo_buffer.len(),
            out_buffer.len()
        );

        // Convert f32 to i16 (scale from -1.0..1.0 to i16 range, with clamping)
        let rec_i16: Vec<i16> = rec_buffer
            .iter()
            .map(|&x| {
                let scaled = (x * 32767.0).clamp(-32768.0, 32767.0);
                scaled as i16
            })
            .collect();
        let echo_i16: Vec<i16> = echo_buffer
            .iter()
            .map(|&x| {
                let scaled = (x * 32767.0).clamp(-32768.0, 32767.0);
                scaled as i16
            })
            .collect();
        let mut out_i16: Vec<i16> = vec![0; out_buffer.len()];
        if echo_i16.iter().all(|&x| x == 0) {
            warn!("Warning: echo_buffer is all zeros—no echo signal to cancel!");
        }
        // Debug: Log first few samples before AEC
        debug!(
            "Debug: First 5 rec_i16: {:?}",
            &rec_i16[..5.min(rec_i16.len())]
        );
        debug!(
            "Debug: First 5 echo_i16: {:?}",
            &echo_i16[..5.min(echo_i16.len())]
        );

        // Debug: Log RMS before AEC
        let rec_rms = (rec_i16.iter().map(|&x| (x as f32).powi(2)).sum::<f32>()
            / rec_i16.len() as f32)
            .sqrt();
        let echo_rms = (echo_i16.iter().map(|&x| (x as f32).powi(2)).sum::<f32>()
            / echo_i16.len() as f32)
            .sqrt();
        debug!("Debug: rec_i16 RMS={}, echo_i16 RMS={}", rec_rms, echo_rms);

        // Call the original i16 method
        self.aec.cancel_echo(&rec_i16, &echo_i16, &mut out_i16);

        // Debug: Log RMS after AEC
        let out_rms = (out_i16.iter().map(|&x| (x as f32).powi(2)).sum::<f32>()
            / out_i16.len() as f32)
            .sqrt();
        debug!("Debug: out_i16 RMS after AEC={}", out_rms);

        // Debug: Log first few samples after AEC
        debug!(
            "Debug: First 5 out_i16 after AEC: {:?}",
            &out_i16[..5.min(out_i16.len())]
        );

        // Convert back to f32 (use 32768.0 for symmetry)
        for (i, &val) in out_i16.iter().enumerate() {
            out_buffer[i] = val as f32 / 32768.0;
        }

        // Debug: Log first few samples after conversion
        debug!(
            "Debug: First 5 out_buffer after f32 conversion: {:?}",
            &out_buffer[..5.min(out_buffer.len())]
        );
    }

    pub fn cancel_speaker_echo(
        &mut self,
        mut capture_frame: Vec<f32>,
        render_frame: Vec<f32>,
    ) -> Vec<f32> {
        // Debug: Log frame lengths
        debug!(
            "Debug: capture_frame len={}, render_frame len={}",
            capture_frame.len(),
            render_frame.len()
        );

        // Call the new f32 method directly (no need for manual conversions here)
        // Clone capture_frame so we don't have an immutable and mutable borrow at the same time.
        let rec_clone = capture_frame.clone();
        self.cancel_echo_f32(&rec_clone, &render_frame, &mut capture_frame);

        // Debug: Log first few samples of result
        debug!(
            "Debug: First 5 capture_frame after cancellation: {:?}",
            &capture_frame[..5.min(capture_frame.len())]
        );

        capture_frame // Return the modified capture_frame
    }
}
