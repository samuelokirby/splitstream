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
        // Convert f32 to i16 (scale from -1.0..1.0 to i16 range)
        let rec_i16: Vec<i16> = rec_buffer.iter().map(|&x| (x * 32767.0) as i16).collect();
        let echo_i16: Vec<i16> = echo_buffer.iter().map(|&x| (x * 32767.0) as i16).collect();
        let mut out_i16: Vec<i16> = vec![0; out_buffer.len()];

        // Call the original i16 method
        self.aec.cancel_echo(&rec_i16, &echo_i16, &mut out_i16);

        // Convert back to f32
        for (i, &val) in out_i16.iter().enumerate() {
            out_buffer[i] = val as f32 / 32767.0;
        }
    }

    pub fn cancel_speaker_echo(
        &mut self,
        mut capture_frame: Vec<f32>,
        render_frame: Vec<f32>,
    ) -> Vec<f32> {
        // Call the new f32 method directly (no need for manual conversions here)
        // Clone capture_frame so we don't have an immutable and mutable borrow at the same time.
        let rec_clone = capture_frame.clone();
        self.cancel_echo_f32(&rec_clone, &render_frame, &mut capture_frame);
        capture_frame // Return the modified capture_frame
    }
}
