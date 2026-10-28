//! Device backends (port of `urh.dev`).
//!
//! - [`virtual_`]: file/spectrum replay as a signal source (VirtualDevice.py)
//! - [`rtltcp`]: RTL-TCP network protocol, pure Rust
//! - [`pcap`]: PCAP / PCAPNG IQ playback
//! - [`native`]: direct vendor-library bindings (libhackrf, librtlsdr, ...)
//!   loaded at runtime via `libloading` — compile everywhere, run where the
//!   SDK is present.

pub mod native;
pub mod pcap;
pub mod rtltcp;
pub mod virtual_;

use std::fmt;

use crate::Complex32;

/// Control commands shared by all devices (port of `Device.Command`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Stop,
    SetFrequency,
    SetSampleRate,
    SetBandwidth,
    SetRfGain,
    SetIfGain,
    SetBbGain,
    SetDirectSamplingMode,
    SetFrequencyCorrection,
    SetChannelIndex,
    SetAntennaIndex,
    SetBiasTeeEnabled,
}

/// Device configuration parameters (subset of `Device.device_parameters`).
#[derive(Debug, Clone, Default)]
pub struct DeviceConfig {
    pub identifier: String,
    pub frequency: f64,
    pub sample_rate: f64,
    pub bandwidth: f64,
    pub rf_gain: f64,
    pub if_gain: f64,
    pub bb_gain: f64,
    pub direct_sampling_mode: i32,
    pub frequency_correction: f64,
    pub channel_index: i32,
    pub antenna_index: i32,
    pub bias_tee_enabled: bool,
}

#[derive(Debug)]
pub enum DeviceError {
    /// vendor library not installed / could not be loaded
    LibraryUnavailable(String),
    /// device open failed (retcode / message from the vendor SDK)
    OpenFailed(String),
    /// parameter rejected by the device
    ParameterRejected(String),
    /// streaming error
    StreamError(String),
    Unsupported(&'static str),
    Io(std::io::Error),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::LibraryUnavailable(s) => write!(f, "library unavailable: {s}"),
            DeviceError::OpenFailed(s) => write!(f, "device open failed: {s}"),
            DeviceError::ParameterRejected(s) => write!(f, "parameter rejected: {s}"),
            DeviceError::StreamError(s) => write!(f, "stream error: {s}"),
            DeviceError::Unsupported(s) => write!(f, "unsupported: {s}"),
            DeviceError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for DeviceError {}

impl From<std::io::Error> for DeviceError {
    fn from(e: std::io::Error) -> Self {
        DeviceError::Io(e)
    }
}

/// A receive stream: normalized complex IQ samples in chunks.
pub type IqChunk = Vec<Complex32>;

/// Synchronous device interface implemented by every backend.
///
/// The Python original drives each backend in a separate process with a
/// command pipe; the Rust port keeps the same control surface but hands the
/// caller a channel of IQ chunks for continuous reception.
pub trait Device {
    /// Human-readable device name, e.g. "HackRF".
    fn name(&self) -> &'static str;

    /// Available device identifiers (USB serials etc.), may be empty.
    fn device_list() -> Vec<String>
    where
        Self: Sized;

    /// Apply a control command to the (open) device.
    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError>;

    /// Open the device for RX and start streaming into `sink`.
    fn start_rx(&mut self, config: &DeviceConfig, sink: std::sync::mpsc::Sender<IqChunk>)
        -> Result<(), DeviceError>;

    /// Open the device for TX and send the given samples (sync mode).
    fn send(&mut self, config: &DeviceConfig, samples: &[Complex32]) -> Result<(), DeviceError>;

    /// Stop streaming and close the device.
    fn stop(&mut self) -> Result<(), DeviceError>;
}
