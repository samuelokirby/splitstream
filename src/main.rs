use opus::Encoder;
use ringbuf::traits::Consumer;

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
        let mut mic_resample_buffer = [0.0_f32; 512];
        let mut sys_resample_buffer = [0.0_f32; 512];
    }
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
