use fixed_resample::rubato::{SincFixedOut, SincInterpolationParameters};
use opus::Encoder;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Split},
};

pub mod audio_input_buffers;
pub mod macos_device;

fn main() {
    let mut dev = macos_device::OSXInputDevice::new().unwrap();
    println!(
        "Aggregate device initialized with {}hz nominal sample rate",
        dev.nominal_sample_rate
    );
    let (mut mic_consumer, mut sys_consumer) = dev.start_capture().unwrap();

    loop {
        // 1. Read from aggregate device ring buffers
        let mut mic_buffer = [0.0_f32; 512];
        let mut sys_buffer = [0.0_f32; 512];
        let mic_read = mic_consumer.pop_slice(&mut mic_buffer);
        let sys_read = sys_consumer.pop_slice(&mut sys_buffer);
        if mic_read > 0 || sys_read > 0 {
            println!(
                "{:3} 🎙️ + {:3} 📣 = {:4} total",
                mic_read,
                sys_read,
                mic_read + sys_read
            );
        }
        // 2. Accumulate into another ring buffer for resampling
        let (mic_resample_prod, mic_resample_cons) = HeapRb::<f32>::new(2048).split();
        let (sys_resample_prod, sys_resample_cons) = HeapRb::<f32>::new(2048).split();
    }
}

fn build_resampler(in_sample_rate: u32, out_sample_rate: u32) -> SincFixedOut<f32> {
    let resample_ratio = out_sample_rate as f64 / in_sample_rate as f64; // e.g. 48000 / 44100 = 1.088435
    let max_resample_ratio_relative = 3.1; // Allow for some variance in sample rate

    let parameters = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        oversampling_factor: 128,
        interpolation: fixed_resample::rubato::SincInterpolationType::Linear,
        window: fixed_resample::rubato::WindowFunction::BlackmanHarris2,
    };

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

fn build_encoder(in_sample_rate: u32) -> Encoder {
    let application = opus::Application::Audio;
    let mut encoder = Encoder::new(in_sample_rate, opus::Channels::Stereo, application)
        .expect("Failed to create Opus encoder");
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(32_000));
    encoder
}

fn frame_size(sample_rate: f64) -> usize {
    const FRAME_MS: f64 = 20.0;
    ((sample_rate * FRAME_MS) / 1000.0).round() as usize
}
