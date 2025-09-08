// src/macos_device.rs
use ca::aggregate_device_keys as agg_keys;
use ca::sub_device_keys as sub_keys;
use cidre::core_audio::DeviceIoProc;
use cidre::{arc, av, cat, cf, core_audio as ca, ns, os};
use log::debug;
use log::info;
use log::warn;
use ringbuf::HeapCons;
use ringbuf::{HeapProd, traits::Producer};
use tokio_util::sync::CancellationToken;

use crate::audio_input_buffers::AudioInputBuffers;

/// OSXIoProcContext is used to pass context to the CoreAudio IOProc.
struct OSXIoProcContext {
    format: arc::R<av::AudioFormat>,
    // Producers for microphone and system audio buffers
    mic_producer: HeapProd<f32>,
    sys_producer: HeapProd<f32>,
}

pub struct OSXInputDevice {
    pub nominal_sample_rate: u32,
    is_capturing: bool,
    agg_desc: arc::Retained<cf::DictionaryOf<cf::String, cf::Type>>,
    _agg_device: Option<ca::AggregateDevice>,
    pub tap: ca::TapGuard,
    started_device: Option<ca::hardware::StartedDevice<ca::AggregateDevice>>,
    ctx: Option<Box<OSXIoProcContext>>, // Store context to keep it alive
    _proc_id: Option<DeviceIoProc>,
    _cancel_token: Option<CancellationToken>,
}

////////
/// InputDevice trait implementation for OSXInputDevice
////////
impl OSXInputDevice {
    /// new creates a new OSXInputDevice instance.
    /// It sets up the aggregate device description but does not start capturing audio.
    pub fn new() -> Result<Self, String> {
        debug!("Starting OSXInputDevice::new()");
        let (agg_desc, tap) = Self::build_agg_description_and_tap()?;
        debug!("✅ Created aggregate device descriptor");

        Ok(OSXInputDevice {
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

    /// start_capture starts audio capture using the provided AudioInputBuffers.
    /// It sets up the Core Audio IOProc to push audio samples into the provided buffers.
    /// Here, we need to start the agg_device, get the tap, and setup the IOProc.
    pub fn start_capture(&mut self) -> Result<(HeapCons<f32>, HeapCons<f32>), String> {
        if self.is_capturing {
            warn!("start_capture called while already capturing");
            return Err("Already capturing".to_string());
        }
        // Rebuild the aggregate description and tap for each start
        let (agg_desc, tap) = Self::build_agg_description_and_tap()?;
        self.agg_desc = agg_desc;
        self.tap = tap;

        let asbd = self
            .tap
            .asbd()
            .map_err(|e| format!("Failed to get ASBD: {:?}", e))?;

        let layout = av::AudioChannelLayout::with_layout_tag(cat::AudioChannelLayoutTag::STEREO);

        let format = av::AudioFormat::with_common_format_sample_rate_interleaved_channel_layout(
            av::AudioCommonFormat::PcmF32,
            asbd.sample_rate as f64,
            false, // non-interleaved → planar (2 buffers)
            &layout,
        );

        let buffers = AudioInputBuffers::new(8192);
        let ((mic_producer, sys_producer), (mic_consumer, sys_consumer)) =
            buffers.into_buffer_splits();

        let mut ctx = Box::new(OSXIoProcContext {
            format,
            mic_producer,
            sys_producer,
        });

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
                if let Some(pcm_buf) =
                    av::AudioPcmBuf::with_buf_list_no_copy(&ctx.format, input_data, None)
                {
                    if let Some(samples) = pcm_buf.data_f32_at(buffer_idx) {
                        if buffer_idx == 0 {
                            // Microphone samples
                            let _ = ctx.mic_producer.push_slice(samples);
                            // println!("* 🔴🎤 Pushed {} mic samples", samples.len());
                        } else if buffer_idx == 1 {
                            // System audio samples
                            let _ = ctx.sys_producer.push_slice(samples);
                            // println!("* 🔵🔊 Pushed {} system audio samples", samples.len());
                        }
                    }
                }
            }
            os::Status::NO_ERR
        }

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

    pub fn stop_capture(&mut self) -> Result<(), String> {
        if !self.is_capturing {
            warn!("stop_capture called while not capturing");
            return Err("Not currently capturing".to_string());
        }

        debug!("Stopping capture...");

        if let Some(started_device) = self.started_device.take() {
            println!("Devices before dropping...");
            drop(started_device);
            debug!("Core Audio device stopped");
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
    fn build_agg_description_and_tap() -> Result<
        (
            arc::Retained<cf::DictionaryOf<cf::String, cf::Type>>,
            ca::TapGuard,
        ),
        String,
    > {
        let input_device =
            ca::System::default_input_device().expect("Failed to get default input device");
        debug!("✅ Got input device");

        let input_uid = input_device
            .uid()
            .map_err(|e| format!("Failed to get UID: {:?}", e))?;
        debug!("✅ Got input device UID: {:?}", input_uid);

        let mic_sub_device =
            cf::DictionaryOf::with_keys_values(&[sub_keys::uid()], &[input_uid.as_type_ref()]);
        debug!("✅ Created mic sub-device dictionary");

        let tap_desc = ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
        debug!("✅ Created tap descriptor");

        let tap = tap_desc
            .create_process_tap()
            .map_err(|e| format!("Failed to create tap: {:?}", e))?;
        debug!("✅ Created process tap");
        info!("Tap Sample Rate: {:?}", tap.asbd().unwrap().sample_rate);

        let tap_uid = tap
            .uid()
            .map_err(|e| format!("Failed to get tap UID: {:?}", e))?;
        debug!("✅ Got tap UID: {:?}", tap_uid);

        let sub_tap =
            cf::DictionaryOf::with_keys_values(&[sub_keys::uid()], &[tap_uid.as_type_ref()]);
        debug!("✅ Created sub-tap dictionary");

        let agg_desc = cf::DictionaryOf::with_keys_values(
            &[
                agg_keys::is_private(),
                agg_keys::is_stacked(),
                agg_keys::tap_auto_start(),
                agg_keys::name(),
                agg_keys::main_sub_device(),
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
