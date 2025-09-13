use webrtc_audio_processing::*;

struct EchoCanceler {
    ap: Processor,
}

impl EchoCanceler {
    pub fn new() -> Self {
        let config = InitializationConfig {
            num_capture_channels: 2, // Stereo mic input
            num_render_channels: 2,  // Stereo speaker output
            ..InitializationConfig::default()
        };

        let mut ap = Processor::new(&config).unwrap();

        let config = Config {
            echo_cancellation: Some(EchoCancellation {
                suppression_level: EchoCancellationSuppressionLevel::Moderate,
                enable_delay_agnostic: false,
                enable_extended_filter: false,
                stream_delay_ms: None,
            }),
            ..Config::default()
        };
        ap.set_config(config);
        Self { ap }
    }

    // cancel_echo takes in a capture_frame (from the mic) and a render_frame
    // (what is being sent to the speakers), and processes them to reduce echo.
    //
    // # Arguments
    // * `capture_frame` - A vector of f32 samples from the microphone input.
    // * `render_frame` - A vector of f32 samples that are being sent to the speakers.
    // # Returns
    // * `Result<(), Error>` - Ok if processing was successful, Err otherwise
    pub fn cancel_echo(
        &mut self,
        capture_frame: Vec<f32>,
        render_frame: Vec<f32>,
    ) -> Result<(), Error> {
        let ap = self.ap;
        // mic = capture, speaker = render
        // The render_frame is what is sent to the speakers, and
        // capture_frame is audio captured from a microphone.
        let mut render_frame_output = render_frame.clone();
        ap.process_render_frame(&mut render_frame_output).unwrap();

        assert_eq!(
            render_frame, render_frame_output,
            "render_frame should not be modified."
        );

        let mut capture_frame_output = capture_frame.clone();
        ap.process_capture_frame(&mut capture_frame_output).unwrap();

        assert_ne!(
            capture_frame, capture_frame_output,
            "Echo cancellation should have modified capture_frame."
        );

        // capture_frame_output is now ready to send to a remote peer.
        println!("Successfully processed a render and capture frame through WebRTC!");
    }
}

fn main() {
    let (render_frame, capture_frame) = sample_stereo_frames();

    let mut render_frame_output = render_frame.clone();
    ap.process_render_frame(&mut render_frame_output).unwrap();

    assert_eq!(
        render_frame, render_frame_output,
        "render_frame should not be modified."
    );

    let mut capture_frame_output = capture_frame.clone();
    ap.process_capture_frame(&mut capture_frame_output).unwrap();

    assert_ne!(
        capture_frame, capture_frame_output,
        "Echo cancellation should have modified capture_frame."
    );

    // capture_frame_output is now ready to send to a remote peer.
    println!("Successfully processed a render and capture frame through WebRTC!");
}
