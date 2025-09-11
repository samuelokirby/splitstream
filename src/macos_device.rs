// src/macos_device.rs
//! OSXInputDevice handles audio capture on macOS using Core Audio.
//! It creates an aggregate device that combines the default microphone
//! input and a system audio tap, allowing simultaneous capture of both
//! audio sources.
//! It heavily uses the `cidre` crate for Core Audio bindings and `ringbuf` for lock-free ring buffers.

use ca::aggregate_device_keys as agg_keys;
use ca::sub_device_keys as sub_keys;
use cidre::core_audio::DeviceIoProc;
use cidre::{arc, av, cat, cf, core_audio as ca, ns, os};
use log::debug;
use log::info;
use log::trace;
use log::warn;
use ringbuf::HeapCons;
use ringbuf::{HeapProd, traits::Producer};
use tokio_util::sync::CancellationToken;

use crate::audio_input_buffers::AudioInputBuffers;

/// OSXIoProcContext is the struct passed to the CoreAudio IOProc.
/// Context is the word of choice here because I've just seen it
/// used in cidre's documentation.
struct OSXIoProcContext {
    format: arc::R<av::AudioFormat>,
    // Producers for microphone and system audio buffers
    mic_producer: HeapProd<f32>,
    sys_producer: HeapProd<f32>,
}

pub struct OSXInputDevice {
    // Sample rate of the device
    pub nominal_sample_rate: u32,
    // Whether capturing is active
    is_capturing: bool,
    // Aggregate device description, used to create the aggregate device
    agg_desc: arc::Retained<cf::DictionaryOf<cf::String, cf::Type>>,
    // The actual aggregate device, not sure why it's needed to be stored
    _agg_device: Option<ca::AggregateDevice>,
    // The system audio tap
    pub tap: ca::TapGuard,
    // The started aggregate device.
    started_device: Option<ca::hardware::StartedDevice<ca::AggregateDevice>>,
    // Context for the IOProc, must be kept alive while capturing
    ctx: Option<Box<OSXIoProcContext>>, // Store context to keep it alive
    _proc_id: Option<DeviceIoProc>,
    _cancel_token: Option<CancellationToken>,
}

impl OSXInputDevice {
    /// new creates a new OSXInputDevice instance.
    /// It creates the aggregate device description and system audio tap
    /// for later use in start_capture.
    /// Returns:
    /// * `Result<Self, String>`: Ok with the new instance, Err with error message on failure
    pub fn new() -> Result<Self, String> {
        debug!("Creating new OSXInputDevice...");
        // Even though we rebuild these in start_capture,
        // I chose to keep this here because it might give us insight
        // into the specifics of the input device and tap in places I'm not thinking of.
        // This might not be necessary. TODO
        let (agg_desc, tap) = Self::build_agg_description_and_tap()?;
        debug!("✅ Created new OSXInputDevice aggregate description and audio tap");

        Ok(OSXInputDevice {
            // We take the first sample rate from the tap's ASBD
            // Note: in HFP mode, this can change quickly to a different sample rate
            // depending on whether the user is speaking or not.
            nominal_sample_rate: tap.asbd().unwrap().sample_rate as u32,
            is_capturing: false,
            agg_desc,
            _agg_device: None,
            tap,
            started_device: None,
            ctx: None,
            _proc_id: None,
            _cancel_token: None,
        })
    }

    /// start_capture starts audio capture by starting the aggregate device,
    /// getting the tap, and setting up the IOProc.
    /// It sets up the Core Audio IOProc to push audio samples into AudioInputBuffers.
    /// It returns the consumers for microphone and system audio buffers.
    /// Returns:
    /// * `Result<(HeapCons<f32>, HeapCons<f32>), String>`: Ok with the consumers, Err with error message on failure
    pub fn start_capture(&mut self) -> Result<(HeapCons<f32>, HeapCons<f32>), String> {
        // If we are already capturing, return an error
        // We don't panic cause idk.
        if self.is_capturing {
            warn!("start_capture called while already capturing");
            return Err("Already capturing".to_string());
        }
        // Rebuild the aggregate description and tap for each start
        let (agg_desc, tap) = Self::build_agg_description_and_tap()?;
        self.agg_desc = agg_desc;
        self.tap = tap;

        // Get the Stream Basic Description (ASBD) from the tap
        // This describes the audio format (sample rate, channels, etc.)
        let asbd = self
            .tap
            .asbd()
            .map_err(|e| format!("Failed to get ASBD: {:?}", e))?;

        // Here, we create a layout to be used in format below that specifies stereo.
        let layout = av::AudioChannelLayout::with_layout_tag(cat::AudioChannelLayoutTag::STEREO);

        // Here, we create a format that sets up the following expectations:
        // - We will receive audio as PCM Float32 samples (wav)
        // - The sample rate is whatever the tap's ASBD says it is
        // - The audio is non-interleaved (planar), meaning each channel has its own buffer
        //   (this is important for us to separate mic and system audio)
        // - The channel layout is stereo (2 channels)
        let format = av::AudioFormat::with_common_format_sample_rate_interleaved_channel_layout(
            av::AudioCommonFormat::PcmF32,
            asbd.sample_rate as f64,
            false, // non-interleaved → planar (2 buffers)
            &layout,
        );

        // We instantiate ring buffers for mic and system audio using our AudioInputBuffers struct.
        // Each buffer is 8192 samples in size, which should be plenty for our needs according to AI.
        let buffers = AudioInputBuffers::new(8192);
        // We split the buffers into their producers and consumers, leaving two consumers and two producers
        // each for mic and system audio respectively.
        let ((mic_producer, sys_producer), (mic_consumer, sys_consumer)) =
            buffers.into_buffer_splits();

        // Now, we build the context that will be passed to the IOProc.
        // This context holds the audio format and the producers for mic and system audio
        // that can be used in the callback.
        let mut ctx = Box::new(OSXIoProcContext {
            format,
            mic_producer,
            sys_producer,
        });

        /// This is the actual Core Audio IOProc callback function.
        /// It is called by Core Audio whenever there is new audio data.
        /// It receives audio buffers for both mic and system audio from our context,
        /// and pushes the samples into the respective ring buffer producers.
        ///
        /// Arguments:
        /// * `_device`: The Core Audio device (not used here)
        /// * `_now`: The current audio timestamp (not used here)
        /// * `input_data`: The input audio buffer list containing audio samples
        /// * `_input_time`: The input audio timestamp (not used here)
        /// * `_output_data`: The output audio buffer list (not used here)
        /// * `_output_time`: The output audio timestamp (not used here)
        /// * `ctx`: The context passed to the IOProc, containing format and producers
        /// Returns:
        /// * `os::Status`: The status of the IOProc, NO_ERR on success
        extern "C" fn proc(
            _device: ca::Device,
            _now: &cat::AudioTimeStamp,
            input_data: &cat::AudioBufList<2>,
            _input_time: &cat::AudioTimeStamp,
            _output_data: &mut cat::AudioBufList<2>,
            _output_time: &cat::AudioTimeStamp,
            ctx: Option<&mut OSXIoProcContext>,
        ) -> os::Status {
            // Unwrap the MacOS context safely
            let ctx = ctx.unwrap();
            let buffers = input_data.buffers;
            // Process each buffer separately
            for (buffer_idx, _) in buffers.iter().enumerate() {
                // We start using some heavy Core Audio and cidre APIs here.
                // We create an AudioPcmBuf from the buffer list without copying data.
                // This gives us access to the raw PCM audio samples.
                if let Some(pcm_buf) =
                    av::AudioPcmBuf::with_buf_list_no_copy(&ctx.format, input_data, None)
                {
                    // When we have samples, we split them based on buffer index:
                    // - Buffer 0 is microphone audio
                    // - Buffer 1 is system audio
                    // Then, we push their samples into the respective ring buffer producers.
                    if let Some(samples) = pcm_buf.data_f32_at(buffer_idx) {
                        if buffer_idx == 0 {
                            // Microphone samples
                            let _ = ctx.mic_producer.push_slice(samples);
                            trace!("* 🔴🎤 Pushed {} mic samples", samples.len());
                        } else if buffer_idx == 1 {
                            // System audio samples
                            let _ = ctx.sys_producer.push_slice(samples);
                            trace!("* 🔵🔊 Pushed {} system audio samples", samples.len());
                        }
                    }
                }
            }
            os::Status::NO_ERR // some weird Core Audio status code meaning success
        }

        // The rest of this function is mostly boilerplate to create and start the aggregate device.
        let agg_device = ca::AggregateDevice::with_desc(&self.agg_desc)
            .map_err(|e| format!("Failed to create aggregate device: {:?}", e))?;
        let _ = agg_device.set_vad_enabled(true);
        // Debug prints with proper error handling
        match agg_device.input_asbd() {
            Ok(asbd) => debug!("Input ASBD: {:#?}", asbd.channels_per_frame),
            Err(e) => debug!("Failed to get Input ASBD: {:?}", e),
        }
        match agg_device.input_stream_cfg() {
            Ok(cfg) => debug!("Input Stream Configuration: {:#?}", cfg),
            Err(e) => debug!("Failed to get Input Stream Configuration: {:?}", e),
        }
        let proc_id = agg_device
            .create_io_proc_id(proc, Some(&mut ctx))
            .map_err(|e| format!("Failed to create IOProc: {:?}", e))?;

        let started_device = ca::device_start(agg_device, Some(proc_id))
            .map_err(|e| format!("Failed to start device: {:?}", e))?;

        self.ctx = Some(ctx); // Store context to keep it alive, otherwise we get a dangling pointer in the IOProc
        // self.proc_id = Some(proc_id);
        self.started_device = Some(started_device);
        self.is_capturing = true;

        Ok((mic_consumer, sys_consumer))
    }

    /// stop_capture stops audio capture by stopping the aggregate device
    /// and cleaning up resources.
    /// It drops the started device and context to stop the IOProc.
    /// Returns:
    /// * `Result<(), String>`: Ok on success, Err with error message on failure
    pub fn stop_capture(&mut self) -> Result<(), String> {
        // If we are not capturing, return an error
        if !self.is_capturing {
            warn!("stop_capture called while not capturing");
            return Err("Not currently capturing".to_string());
        }

        debug!("Stopping capture...");

        if let Some(started_device) = self.started_device.take() {
            debug!("Dropping CoreAudio device");
            drop(started_device);
        }

        // Drop the context to avoid dangling pointers
        self.ctx.take();

        self.is_capturing = false;
        debug!("Capture stopped");
        Ok(())
    }

    fn _is_capturing(&self) -> bool {
        self.is_capturing
    }

    /// Creates a Core Audio aggregate device description.
    /// This is needed for later as setup to construct the aggregate device.
    /// build_agg_description_and_tap constructs the Core Audio aggregate device
    /// description and creates a system audio tap.
    ///
    /// This function:
    /// * Queries the system default input device (microphone).
    /// * Creates a sub-device dictionary referencing the microphone by UID.
    /// * Builds a TapDesc for a mono global system tap and starts a process tap.
    /// * Creates a sub-device dictionary for the tap by UID.
    /// * Assembles an aggregate device description that includes the mic and tap
    ///   as sub-devices and configures various aggregate device keys.
    ///
    /// Returns:
    /// * Ok((agg_desc, tap)) on success where `agg_desc` is a CF dictionary suitable
    ///   for constructing an AggregateDevice and `tap` is the created TapGuard.
    /// * Err(String) with a human-readable message on any failure.
    fn build_agg_description_and_tap() -> Result<
        (
            arc::Retained<cf::DictionaryOf<cf::String, cf::Type>>,
            ca::TapGuard,
        ),
        String,
    > {
        // Get the system default input device (microphone). This is expected to succeed
        // on machines with an input device. We propagate errors as Strings.
        let input_device =
            ca::System::default_input_device().expect("Failed to get default input device");
        debug!("✅ Got input device");

        // Get the unique identifier (UID) for the input device. This UID will be used
        // to reference the microphone as a sub-device in the aggregate device.
        let input_uid = input_device
            .uid()
            .map_err(|e| format!("Failed to get UID: {:?}", e))?;
        debug!("✅ Got input device UID: {:?}", input_uid);

        // Build a CoreFoundation dictionary representing the microphone sub-device.
        // The aggregate device expects sub-device dictionaries keyed by their UID.
        let mic_sub_device =
            cf::DictionaryOf::with_keys_values(&[sub_keys::uid()], &[input_uid.as_type_ref()]);
        debug!("✅ Created mic sub-device dictionary");

        // Create a TapDesc for a mono global process tap. Here we exclude no processes
        // (empty array) to allow capturing system audio globally for the process.
        let tap_desc = ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
        debug!("✅ Created tap descriptor");

        // Create the process tap from the descriptor. The returned TapGuard keeps the tap alive.
        let tap = tap_desc
            .create_process_tap()
            .map_err(|e| format!("Failed to create tap: {:?}", e))?;
        debug!("✅ Created process tap");
        info!("Tap Sample Rate: {:?}", tap.asbd().unwrap().sample_rate);

        // Get the UID for the created tap so it can be referenced as a sub-device.
        let tap_uid = tap
            .uid()
            .map_err(|e| format!("Failed to get tap UID: {:?}", e))?;
        debug!("✅ Got tap UID: {:?}", tap_uid);

        // Build a CoreFoundation dictionary representing the tap sub-device.
        let sub_tap =
            cf::DictionaryOf::with_keys_values(&[sub_keys::uid()], &[tap_uid.as_type_ref()]);
        debug!("✅ Created sub-tap dictionary");

        // Assemble the aggregate device description dictionary. Keys include:
        // - is_private: keep the device private from the user
        // - is_stacked: no fucking clue
        // - tap_auto_start: determines if the tap starts once both sub-devices are started
        // - name: a human-readable name (kept short/opaque here)
        // - main_sub_device: which sub-device is the main input (mic UID)
        // - uid: a generated UID for the aggregate device itself
        // - sub_device_list: list of sub-device dictionaries (mic + tap)
        // - tap_list: list of taps associated with the aggregate device (tap only)
        //
        // These CF types are provided as references; the aggregate device constructor
        // will consume this description when creating the AggregateDevice.
        let agg_desc = cf::DictionaryOf::with_keys_values(
            &[
                agg_keys::is_private(),
                agg_keys::is_stacked(),
                agg_keys::tap_auto_start(),
                agg_keys::name(),
                agg_keys::main_sub_device(), // TODO: does this need to be the output device?
                agg_keys::uid(),
                agg_keys::sub_device_list(),
                agg_keys::tap_list(),
            ],
            &[
                cf::Boolean::value_true().as_type_ref(),
                cf::Boolean::value_false(),
                cf::Boolean::value_false(),
                cf::str!(c"secretary-agdevice"),
                &input_uid,
                &cf::Uuid::new().to_cf_string(),
                &cf::ArrayOf::from_slice(&[mic_sub_device.as_ref(), sub_tap.as_ref()]),
                &cf::ArrayOf::from_slice(&[sub_tap.as_ref()]),
            ],
        );

        Ok((agg_desc, tap))
    }
}
