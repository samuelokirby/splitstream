use cidre::core_audio as ca;
use std::ffi::c_void;
use tokio::sync::broadcast::{self, Receiver, Sender};

use coreaudio_sys::{
    AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertyElement, AudioObjectPropertyScope, AudioObjectPropertySelector,
    AudioObjectRemovePropertyListener, OSStatus, kAudioDevicePropertyActualSampleRate,
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyNominalSampleRate,
    kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyElementMaster, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioObjectUnknown,
};
use log::{error, info};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

struct ListenerClientData {
    event_tx: Sender<AudioPropertyChange>,
}

// Send-safe wrapper for raw pointers
struct SendPtr<T>(*mut T);

// SAFETY: This is safe because:
// 1. The ListenerClientData is allocated once and lives for the duration of the listener
// 2. The Core Audio callbacks are synchronized by the system
// 3. We control the lifetime - we create it in register_ca_listeners and free it in teardown_ca_listeners
unsafe impl<T> Send for SendPtr<T> where T: Send {}

impl<T> SendPtr<T> {
    fn new(ptr: *mut T) -> Self {
        SendPtr(ptr)
    }

    fn as_ptr(&self) -> *mut T {
        self.0
    }
}

pub struct CoreAudioListener {
    pub default_input_device: ca::Device,
    pub default_output_device: ca::Device,
    pub is_listening: bool, // used to prevent double registration and teardown
    event_tx: Sender<AudioPropertyChange>,
    listener_client_data: Option<SendPtr<ListenerClientData>>, // Send-safe raw pointer wrapper
}

impl CoreAudioListener {
    pub fn new() -> Self {
        let (event_tx, _event_rx) = broadcast::channel::<AudioPropertyChange>(16);
        let default_output_device = ca::System::default_output_device().unwrap();
        let default_input_device = ca::System::default_input_device().unwrap();
        info!(
            "Default input device: {} (ID {:#?})",
            default_input_device.name().unwrap(),
            default_input_device.0
        );
        Self {
            event_tx,
            default_input_device,
            default_output_device,
            is_listening: false,
            listener_client_data: None,
        }
    }

    pub fn start(&mut self) {
        if self.is_listening {
            error!("Already listening for CoreAudio events");
            return;
        }
        self.register_ca_listeners();
        self.is_listening = true;
    }

    pub fn stop(&mut self) {
        if !self.is_listening {
            error!("Not currently listening for CoreAudio events");
            return;
        }
        self.teardown_ca_listeners();
        self.is_listening = false;
    }

    /// This function needs to be thread-safe as it may be called from multiple threads.
    pub fn rebuild(&mut self) {
        self.stop();
        // Update the default devices
        self.default_output_device = ca::System::default_output_device().unwrap();
        self.default_input_device = ca::System::default_input_device().unwrap();

        self.start();
    }

    pub fn subscribe(&self) -> Receiver<AudioPropertyChange> {
        self.event_tx.subscribe()
    }

    /// Registers listeners for CoreAudio property changes on the specified device ID.
    fn register_ca_listeners(&mut self) {
        if self.default_output_device.0.0 == kAudioObjectUnknown {
            error!("Invalid default output device, cannot register listeners");
            return;
        }

        let mut device_property_addresses: Vec<AudioObjectPropertyAddress> = Vec::new();
        let mut hardware_property_addresses: Vec<AudioObjectPropertyAddress> = Vec::new();
        for &dselector in PROPERTY_SELECTORS {
            let address = get_property_address(
                dselector,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMaster,
            );
            device_property_addresses.push(address);
        }

        for &hselector in HARDWARE_SELECTORS {
            let address = get_property_address(
                hselector,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMaster,
            );
            hardware_property_addresses.push(address);
        }

        // Allocate client_data once and reuse it so teardown can remove the same listener.
        let event_tx_ptr: *mut ListenerClientData = if let Some(ptr) = &self.listener_client_data {
            ptr.as_ptr()
        } else {
            let boxed = Box::new(ListenerClientData {
                event_tx: self.event_tx.clone(),
            });
            let ptr = Box::into_raw(boxed);
            self.listener_client_data = Some(SendPtr::new(ptr));
            ptr
        };

        unsafe {
            for &address in &device_property_addresses {
                AudioObjectAddPropertyListener(
                    self.default_output_device.0.0, // returns the AudioObjectID u32
                    &address,
                    Some(device_changed_listener),
                    event_tx_ptr as *mut _ as *mut c_void,
                );
            }

            for &address in &hardware_property_addresses {
                AudioObjectAddPropertyListener(
                    kAudioObjectSystemObject,
                    &address,
                    Some(device_changed_listener),
                    event_tx_ptr as *mut _ as *mut c_void,
                );
            }
        }
    }

    fn teardown_ca_listeners(&mut self) {
        if !self.is_listening {
            error!("Not currently listening for CoreAudio events");
            return;
        }
        let mut device_property_addresses: Vec<AudioObjectPropertyAddress> = Vec::new();
        let mut hardware_property_addresses: Vec<AudioObjectPropertyAddress> = Vec::new();
        for &dselector in PROPERTY_SELECTORS {
            let address = get_property_address(
                dselector,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMaster,
            );
            device_property_addresses.push(address);
        }

        for &hselector in HARDWARE_SELECTORS {
            let address = get_property_address(
                hselector,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMaster,
            );
            hardware_property_addresses.push(address);
        }

        // Use the exact same client_data pointer used during registration.
        let Some(event_tx_ptr_wrapper) = &self.listener_client_data else {
            return; // Nothing to teardown
        };
        let event_tx_ptr = event_tx_ptr_wrapper.as_ptr();

        unsafe {
            for &address in &device_property_addresses {
                AudioObjectRemovePropertyListener(
                    self.default_output_device.0.0, // returns the AudioObjectID u32
                    &address,
                    Some(device_changed_listener),
                    event_tx_ptr as *mut _ as *mut c_void,
                );
            }

            for &address in &hardware_property_addresses {
                AudioObjectRemovePropertyListener(
                    kAudioObjectSystemObject,
                    &address,
                    Some(device_changed_listener),
                    event_tx_ptr as *mut _ as *mut c_void,
                );
            }

            // Free the boxed sender to avoid leaking.
            drop(Box::from_raw(event_tx_ptr));
        }

        self.listener_client_data = None;
    }
}

/// AudioPropertyChange represents a more readable representation of CoreAudio property
/// changes we want to listen for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPropertyChange {
    ActualSampleRate { hz: u32 },  // The actual sample rate in Hz
    NominalSampleRate { hz: u32 }, // The nominal sample rate in Hz
    DeviceIsAlive,
    HardwareDefaultInputDevice { id: u32, hz: u32 },
    HardwareDefaultOutputDevice { id: u32, hz: u32 },
    Unknown,
}

/// SELECTORS are the CoreAudio property selectors we want to listen for.
static PROPERTY_SELECTORS: &[AudioObjectPropertySelector] = &[
    kAudioDevicePropertyActualSampleRate,
    kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyDeviceIsAlive, // TODO this one kills the listener when the device is unplugged, need to re-register on next default device change
];

static HARDWARE_SELECTORS: &[AudioObjectPropertySelector] = &[
    kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice,
];

/// CoreAudio property listener callback.
/// This function is called by CoreAudio when a property change is detected.
/// It identifies the changed property and sends an event through a transmitter...
/// ...the transmitter is passed as client data from register_ca_listeners().
///
/// - `num_addresses`: The number of property addresses that have changed.
/// - `addresses`: A pointer to an array of AudioObjectPropertyAddress structures.
/// - `client_data`: In this case, a pointer to the UnboundedSender<AudioPropertyChange> to send events.
#[allow(non_upper_case_globals)]
extern "C" fn device_changed_listener(
    _: AudioObjectID,
    num_addresses: u32,
    addresses: *const AudioObjectPropertyAddress,
    client_data: *mut c_void,
) -> OSStatus {
    info!("Device change detected: {num_addresses} addresses changed");

    let data = unsafe { &*(client_data as *const ListenerClientData) };
    let event_tx = &data.event_tx;

    let default_output_device = ca::System::default_output_device().unwrap();
    let default_input_device = ca::System::default_input_device().unwrap();

    for i in 0..num_addresses {
        let address = unsafe { *addresses.add(i as usize) };
        let audio_property_changed = match address.mSelector {
            // long but readable
            kAudioDevicePropertyActualSampleRate | kAudioDevicePropertyNominalSampleRate => {
                let new_sample_rate: u32 =
                    default_output_device.actual_sample_rate().unwrap() as u32;
                AudioPropertyChange::ActualSampleRate {
                    hz: new_sample_rate,
                }
            }
            kAudioDevicePropertyDeviceIsAlive => AudioPropertyChange::DeviceIsAlive,
            kAudioHardwarePropertyDefaultInputDevice => {
                AudioPropertyChange::HardwareDefaultInputDevice {
                    id: default_input_device.0.0,
                    hz: default_input_device.actual_sample_rate().unwrap() as u32,
                }
            }
            kAudioHardwarePropertyDefaultOutputDevice => {
                AudioPropertyChange::HardwareDefaultOutputDevice {
                    id: default_output_device.0.0,
                    hz: default_output_device.actual_sample_rate().unwrap() as u32,
                }
            }
            _ => AudioPropertyChange::Unknown,
        };
        let _ = event_tx.send(audio_property_changed.clone());

        info!(
            "  Selector: {:x}, Scope: {:x}, Element: {:x}",
            address.mSelector, address.mScope, address.mElement
        );
    }
    0 // noErr
}

/// Helper function to create an AudioObjectPropertyAddress struct.
fn get_property_address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    element: AudioObjectPropertyElement,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: element,
    }
}
