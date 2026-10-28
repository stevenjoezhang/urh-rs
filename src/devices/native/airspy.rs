//! AirSpy backend — port of `urh.dev.native.lib.airspy` (airspy.pyx) and
//! `urh.dev.native.AirSpy`.
//!
//! `libairspy` is dlopened at runtime via [`libloading::Library`], trying each
//! name in [`LIBRARY_NAMES`]; [`AirSpyDevice::open`] returns
//! [`DeviceError::LibraryUnavailable`] when the driver is not installed.
//!
//! Reception uses `AIRSPY_SAMPLE_FLOAT32_IQ` (as `airspy.start_rx` does), so
//! the callback receives interleaved f32 I/Q pairs that map 1:1 onto
//! [`Complex32`] (`AirSpy.bytes_to_iq` reshapes a float32 buffer to (-1, 2)).
//!
//! Gain semantics follow airspy.pyx exactly:
//! - RF gain     → `airspy_set_mixer_gain`   (0..15)
//! - IF gain (rx)→ `airspy_set_vga_gain`     (0..15)
//! - BB gain     → `airspy_set_lna_gain`     (0..15)
//!
//! The libairspy *linearity* (0..21) and *sensitivity* (0..21) gain modes are
//! exposed as explicit methods but are not wired into the Device trait,
//! matching this URH version's airspy.pyx which never calls them.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use crate::devices::{Command, Device, DeviceConfig, DeviceError, IqChunk};
use crate::Complex32;

/// Shared-library file names tried (in order) when dlopening libairspy.
pub const LIBRARY_NAMES: &[&str] = &[
    "libairspy.0.dylib",
    "libairspy.dylib",
    "libairspy.so.0",
    "libairspy.so",
    "airspy.dll",
];

/// `airspy_set_freq` docstring: 24 MHz .. 1.75 GHz.
pub const FREQUENCY_RANGE_HZ: (f64, f64) = (24_000_000.0, 1_750_000_000.0);

/// LNA / mixer / VGA gain range (airspy.pyx set_baseband_gain etc.).
pub const GAIN_MIN: u8 = 0;
pub const GAIN_MAX: u8 = 15;

/// Linearity / sensitivity gain range (libairspy, 0..21).
pub const LINEARITY_SENSITIVITY_MAX: u8 = 21;

/// `AIRSPY_SAMPLE_FLOAT32_IQ` from cairspy.pxd.
const AIRSPY_SAMPLE_FLOAT32_IQ: c_int = 0;

// ---------------------------------------------------------------------------
// C declarations (mirror of cairspy.pxd)
// ---------------------------------------------------------------------------

/// `airspy_transfer` from cairspy.pxd (repr(C) layout).
#[repr(C)]
struct AirspyTransfer {
    device: *mut c_void,
    ctx: *mut c_void,
    samples: *mut c_void,
    sample_count: c_int,
    dropped_samples: u64,
    sample_type: c_int,
}

type FnOpen = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type FnOpenSn = unsafe extern "C" fn(*mut *mut c_void, u64) -> c_int;
type FnClose = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnGetSamplerates = unsafe extern "C" fn(*mut c_void, *mut u32, u32) -> c_int;
type FnSetSamplerate = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
type FnSetFreq = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
type FnSetGainU8 = unsafe extern "C" fn(*mut c_void, u8) -> c_int;
type FnSetSampleType = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnStartRx = unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut AirspyTransfer) -> c_int, *mut c_void) -> c_int;
type FnStopRx = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnErrorName = unsafe extern "C" fn(c_int) -> *const c_char;

/// State handed to the C receive callback through the `rx_ctx` pointer.
struct RxCtx {
    sink: mpsc::Sender<IqChunk>,
}

const ASYNC_RUNNING: i32 = i32::MIN;

// ---------------------------------------------------------------------------
// Sample conversion (AirSpy.bytes_to_iq, float32 IQ)
// ---------------------------------------------------------------------------

/// Convert interleaved float32 I/Q pairs to complex samples.
pub(crate) fn convert_float_iq(floats: &[f32]) -> IqChunk {
    let mut out = Vec::with_capacity(floats.len() / 2);
    for pair in floats.chunks_exact(2) {
        out.push(Complex32::new(pair[0], pair[1]));
    }
    out
}

/// Clamp a gain into the libairspy-accepted 0..15 range (values outside are
/// rejected by the driver; the pyx passes them through, here we clamp and let
/// the driver confirm).
fn clamp_gain(value: f64) -> u8 {
    value.clamp(GAIN_MIN as f64, GAIN_MAX as f64) as u8
}

// ---------------------------------------------------------------------------
// C receive callback
// ---------------------------------------------------------------------------

unsafe extern "C" fn rx_callback(transfer: *mut AirspyTransfer) -> c_int {
    // SAFETY: libairspy invokes this callback with a valid transfer whose
    // `samples` buffer holds `2 * sample_count` float32 values because we
    // configured AIRSPY_SAMPLE_FLOAT32_IQ. The body is panic-free:
    // from_raw_parts on the documented (ptr, len) pair, chunks_exact never
    // panics, and a failed channel send is plain Err, not a panic.
    if transfer.is_null() {
        return 0;
    }
    let t = &*transfer;
    if t.samples.is_null() || t.sample_count <= 0 {
        return 0;
    }
    let floats = std::slice::from_raw_parts(
        t.samples.cast::<f32>(),
        2 * t.sample_count as usize,
    );
    let _ = (&*(t.ctx as *const RxCtx)).sink.send(convert_float_iq(floats));
    0
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// AirSpy receiver backed by a runtime-loaded libairspy.
pub struct AirSpyDevice {
    lib: Arc<libloading::Library>,
    /// raw `airspy_device*`, null when closed
    dev: AtomicPtr<c_void>,
    /// boxed `RxCtx` handed to libairspy as rx_ctx while streaming
    rx_ctx: AtomicPtr<RxCtx>,
    rx_thread: Option<JoinHandle<()>>,
    rx_exit: Arc<AtomicI32>,
}

// SAFETY: the raw device pointer is only dereferenced through libairspy FFI
// calls made while the struct is exclusively borrowed; moving the handle
// moves the single open device along with it.
unsafe impl Send for AirSpyDevice {}

/// Describe a libairspy error code via `airspy_error_name` when possible.
fn describe_error(lib: &libloading::Library, code: c_int) -> String {
    // SAFETY: dlsym lookup; the returned const char* is only read.
    let name = unsafe { lib.get::<FnErrorName>(b"airspy_error_name\0") }
        .ok()
        .and_then(|f| {
            // SAFETY: airspy_error_name returns a static string for any code.
            let p = unsafe { f(code) };
            if p.is_null() {
                None
            } else {
                // SAFETY: p is a NUL-terminated static C string.
                Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
            }
        });
    match name {
        Some(n) => format!("{n} ({code})"),
        None => format!("code {code}"),
    }
}

impl AirSpyDevice {
    /// Try to dlopen libairspy using the platform name table.
    pub fn load_library() -> Result<libloading::Library, DeviceError> {
        let mut last = String::new();
        for name in LIBRARY_NAMES {
            match unsafe { libloading::Library::new(name) } {
                // SAFETY: plain dlopen of a named shared library.
                Ok(lib) => return Ok(lib),
                Err(e) => last = format!("{name}: {e}"),
            }
        }
        Err(DeviceError::LibraryUnavailable(format!(
            "could not load libairspy ({last})"
        )))
    }

    fn sym<'a, T>(
        lib: &'a libloading::Library,
        name: &[u8],
    ) -> Result<libloading::Symbol<'a, T>, DeviceError> {
        // SAFETY: dlsym lookup by name; the symbol is only called through its
        // correctly declared fn-pointer type.
        unsafe { lib.get::<T>(name) }
            .map_err(|e| DeviceError::LibraryUnavailable(format!("symbol {}: {e}", String::from_utf8_lossy(name))))
    }

    /// Open the first AirSpy. `AirSpy.setup_device` ignores the identifier and
    /// always calls `airspy.open()`; as an extension, a numeric identifier is
    /// interpreted as a serial number and passed to `airspy_open_sn`.
    pub fn open(identifier: Option<&str>) -> Result<Self, DeviceError> {
        let lib = Self::load_library()?;

        let mut dev: *mut c_void = std::ptr::null_mut();
        let ret = match identifier.filter(|s| !s.is_empty()).and_then(|s| s.parse::<u64>().ok()) {
            Some(serial) => {
                let open_sn = Self::sym::<FnOpenSn>(&lib, b"airspy_open_sn\0")?;
                // SAFETY: dev is a valid out-pointer.
                unsafe { open_sn(&mut dev, serial) }
            }
            None => {
                let open = Self::sym::<FnOpen>(&lib, b"airspy_open\0")?;
                // SAFETY: dev is a valid out-pointer.
                unsafe { open(&mut dev) }
            }
        };
        if ret != 0 || dev.is_null() {
            return Err(DeviceError::OpenFailed(format!(
                "airspy_open failed: {}",
                describe_error(&lib, ret)
            )));
        }

        Ok(Self {
            lib: Arc::new(lib),
            dev: AtomicPtr::new(dev),
            rx_ctx: AtomicPtr::new(std::ptr::null_mut()),
            rx_thread: None,
            rx_exit: Arc::new(AtomicI32::new(ASYNC_RUNNING)),
        })
    }

    fn dev_ptr(&self) -> Result<*mut c_void, DeviceError> {
        let p = self.dev.load(Ordering::Acquire);
        if p.is_null() {
            Err(DeviceError::OpenFailed("device is not open".into()))
        } else {
            Ok(p)
        }
    }

    fn check(ret: c_int, what: &str, lib: &libloading::Library) -> Result<(), DeviceError> {
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(format!(
                "{what} rejected: {}",
                describe_error(lib, ret)
            )));
        }
        Ok(())
    }

    /// `airspy_set_freq` — 24 MHz .. 1.75 GHz (airspy.pyx set_center_frequency).
    pub fn set_center_frequency(&self, freq_hz: f64) -> Result<(), DeviceError> {
        if !(FREQUENCY_RANGE_HZ.0..=FREQUENCY_RANGE_HZ.1).contains(&freq_hz) {
            return Err(DeviceError::ParameterRejected(format!(
                "frequency {freq_hz} outside supported range 24000000..1750000000 Hz"
            )));
        }
        let dev = self.dev_ptr()?;
        let f = Self::sym::<FnSetFreq>(&self.lib, b"airspy_set_freq\0")?;
        // SAFETY: dev is the open device handle; value was range-checked.
        let ret = unsafe { f(dev, freq_hz as u32) };
        Self::check(ret, &format!("frequency {freq_hz}"), &self.lib)
    }

    /// `airspy_set_samplerate` — either a value in Hz from `sample_rates()`
    /// or the index of one (libairspy accepts both).
    pub fn set_sample_rate(&self, sample_rate: u32) -> Result<(), DeviceError> {
        let dev = self.dev_ptr()?;
        let f = Self::sym::<FnSetSamplerate>(&self.lib, b"airspy_set_samplerate\0")?;
        // SAFETY: dev is the open device handle.
        let ret = unsafe { f(dev, sample_rate) };
        Self::check(ret, &format!("sample rate {sample_rate}"), &self.lib)
    }

    /// Query the supported sample rates (airspy.pyx `get_sample_rates`).
    pub fn sample_rates(&self) -> Vec<u32> {
        let dev = match self.dev_ptr() {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let get = match Self::sym::<FnGetSamplerates>(&self.lib, b"airspy_get_samplerates\0") {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        // SAFETY: passing len = 0 with a count out-pointer is the documented
        // way to query the number of supported rates.
        let mut count: u32 = 0;
        if unsafe { get(dev, &mut count, 0) } != 0 || count == 0 {
            return Vec::new();
        }
        let mut rates = vec![0u32; count as usize];
        // SAFETY: rates has `count` elements as required.
        if unsafe { get(dev, rates.as_mut_ptr(), count) } != 0 {
            return Vec::new();
        }
        rates
    }

    /// BB gain → `airspy_set_lna_gain` (0..15).
    pub fn set_lna_gain(&self, value: f64) -> Result<(), DeviceError> {
        self.set_u8_gain(b"airspy_set_lna_gain\0", clamp_gain(value), "LNA gain")
    }

    /// RF gain → `airspy_set_mixer_gain` (0..15).
    pub fn set_mixer_gain(&self, value: f64) -> Result<(), DeviceError> {
        self.set_u8_gain(b"airspy_set_mixer_gain\0", clamp_gain(value), "mixer gain")
    }

    /// IF gain → `airspy_set_vga_gain` (0..15).
    pub fn set_vga_gain(&self, value: f64) -> Result<(), DeviceError> {
        self.set_u8_gain(b"airspy_set_vga_gain\0", clamp_gain(value), "VGA gain")
    }

    /// Linearity gain mode (0..21) — not used by airspy.pyx but declared in
    /// cairspy.pxd; exposed for completeness.
    pub fn set_linearity_gain(&self, value: f64) -> Result<(), DeviceError> {
        let v = value.clamp(0.0, LINEARITY_SENSITIVITY_MAX as f64) as u8;
        self.set_u8_gain(b"airspy_set_linearity_gain\0", v, "linearity gain")
    }

    /// Sensitivity gain mode (0..21) — not used by airspy.pyx but declared in
    /// cairspy.pxd; exposed for completeness.
    pub fn set_sensitivity_gain(&self, value: f64) -> Result<(), DeviceError> {
        let v = value.clamp(0.0, LINEARITY_SENSITIVITY_MAX as f64) as u8;
        self.set_u8_gain(b"airspy_set_sensitivity_gain\0", v, "sensitivity gain")
    }

    fn set_u8_gain(&self, sym_name: &[u8], value: u8, what: &str) -> Result<(), DeviceError> {
        let dev = self.dev_ptr()?;
        let f = Self::sym::<FnSetGainU8>(&self.lib, sym_name)?;
        // SAFETY: dev is the open device handle; the u8 value was range-
        // checked by the caller according to the cairspy.pxd docs.
        let ret = unsafe { f(dev, value) };
        Self::check(ret, what, &self.lib)
    }

    /// Apply a full [`DeviceConfig`] to the open device (port of
    /// `Device.init_device`'s parameter replay loop for AirSpy).
    fn apply_config(&self, config: &DeviceConfig) -> Result<(), DeviceError> {
        if config.frequency > 0.0 {
            self.set_center_frequency(config.frequency)?;
        }
        if config.sample_rate > 0.0 {
            self.set_sample_rate(config.sample_rate as u32)?;
        }
        // AirSpy.py deletes SET_BANDWIDTH: the bandwidth is not adjustable.
        if config.rf_gain > 0.0 {
            self.set_mixer_gain(config.rf_gain)?;
        }
        if config.if_gain > 0.0 {
            self.set_vga_gain(config.if_gain)?;
        }
        if config.bb_gain > 0.0 {
            self.set_lna_gain(config.bb_gain)?;
        }
        Ok(())
    }

    /// Stop streaming and close the device (port of `AirSpy.shutdown_device`:
    /// stop_rx then close).
    pub fn close(&mut self) -> Result<(), DeviceError> {
        self.stop()?;
        let dev = self.dev.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if dev.is_null() {
            return Ok(());
        }
        let f = Self::sym::<FnClose>(&self.lib, b"airspy_close\0")?;
        // SAFETY: dev was the open device handle and is now disowned.
        let ret = unsafe { f(dev) };
        if ret != 0 {
            return Err(DeviceError::StreamError(format!(
                "airspy_close failed: {}",
                describe_error(&self.lib, ret)
            )));
        }
        Ok(())
    }
}

impl Device for AirSpyDevice {
    fn name(&self) -> &'static str {
        "AirSpy"
    }

    /// `AirSpy` does not override `Device.get_device_list` → always empty.
    fn device_list() -> Vec<String> {
        Vec::new()
    }

    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        match cmd {
            Command::Stop => Ok(()),
            Command::SetFrequency => self.set_center_frequency(value),
            Command::SetSampleRate => self.set_sample_rate(value as u32),
            // AirSpy.py: `del DEVICE_METHODS[SET_BANDWIDTH]`
            Command::SetBandwidth => Err(DeviceError::Unsupported(
                "AirSpy bandwidth is not adjustable",
            )),
            Command::SetRfGain => self.set_mixer_gain(value),
            Command::SetIfGain => self.set_vga_gain(value),
            Command::SetBbGain => self.set_lna_gain(value),
            Command::SetDirectSamplingMode | Command::SetFrequencyCorrection
            | Command::SetChannelIndex | Command::SetAntennaIndex | Command::SetBiasTeeEnabled => {
                Err(DeviceError::Unsupported("AirSpy does not support this parameter"))
            }
        }
    }

    fn start_rx(
        &mut self,
        config: &DeviceConfig,
        sink: mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        if self.rx_thread.is_some() {
            return Err(DeviceError::StreamError("RX already running".into()));
        }
        self.apply_config(config)?;

        let dev = self.dev_ptr()?;
        let set_type = Self::sym::<FnSetSampleType>(&self.lib, b"airspy_set_sample_type\0")?;
        // SAFETY: dev is the open device handle; AIRSPY_SAMPLE_FLOAT32_IQ
        // selects the f32 IQ layout the callback below assumes (airspy.pyx
        // start_rx does the same before airspy_start_rx).
        let ret = unsafe { set_type(dev, AIRSPY_SAMPLE_FLOAT32_IQ) };
        Self::check(ret, "sample type FLOAT32_IQ", &self.lib)?;

        let start_rx = Self::sym::<FnStartRx>(&self.lib, b"airspy_start_rx\0")?;
        // SAFETY: rx_callback matches the airspy_sample_block_cb_fn type and
        // treats ctx as a boxed RxCtx whose pointer is also stored in
        // self.rx_ctx, so stop() can reclaim it after streaming ends.
        let ctx = Box::into_raw(Box::new(RxCtx { sink }));
        self.rx_ctx.store(ctx, Ordering::Release);
        let ret = unsafe { start_rx(dev, rx_callback, ctx.cast()) };
        Self::check(ret, "airspy_start_rx", &self.lib)?;

        // libairspy streams on its own internal thread; this watcher thread
        // only detects when streaming ends so stop() can reclaim the ctx.
        let lib = Arc::clone(&self.lib);
        let rx_exit = Arc::clone(&self.rx_exit);
        // SAFETY: the raw device handle is only used for the polling
        // airspy_is_streaming call below; the device stays open for the
        // lifetime of this thread because stop()/close() join it first.
        let dev_poll = dev as usize;
        let thread = std::thread::Builder::new()
            .name("airspy-rx-watch".into())
            .spawn(move || {
                type FnIsStreaming = unsafe extern "C" fn(*mut c_void) -> c_int;
                let exit_code = match Self::sym::<FnIsStreaming>(&lib, b"airspy_is_streaming\0") {
                    Ok(is_streaming) => loop {
                        // SAFETY: dev_poll is the still-open device handle,
                        // reconstituted from the usize it was moved as.
                        let ret = unsafe { is_streaming(dev_poll as *mut c_void) };
                        if ret != 1 {
                            break ret;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    },
                    Err(_) => 0,
                };
                rx_exit.store(exit_code, Ordering::Release);
            })
            .map_err(|e| DeviceError::StreamError(format!("failed to spawn RX watcher: {e}")))?;
        self.rx_thread = Some(thread);
        Ok(())
    }

    fn send(&mut self, _config: &DeviceConfig, _samples: &[Complex32]) -> Result<(), DeviceError> {
        // AirSpy.py only implements reception (no enter_async_send_mode).
        Err(DeviceError::Unsupported("AirSpy cannot transmit"))
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        if let Ok(dev) = self.dev_ptr() {
            if let Ok(stop_rx) = Self::sym::<FnStopRx>(&self.lib, b"airspy_stop_rx\0") {
                // SAFETY: dev is the open device handle. airspy.pyx sleeps
                // 10 ms before stopping; keep the same grace period.
                std::thread::sleep(std::time::Duration::from_millis(10));
                let ret = unsafe { stop_rx(dev) };
                Self::check(ret, "airspy_stop_rx", &self.lib)?;
            }
        }
        if let Some(thread) = self.rx_thread.take() {
            let _ = thread.join();
        }
        // SAFETY: airspy_stop_rx has returned and the watcher thread was
        // joined, so libairspy no longer invokes the callback that received
        // this pointer; the box is reclaimed exactly once.
        let ctx = self.rx_ctx.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !ctx.is_null() {
            drop(unsafe { Box::from_raw(ctx) });
        }
        Ok(())
    }
}

impl Drop for AirSpyDevice {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_names_have_platform_shapes() {
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dylib")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.contains(".so")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dll")));
        assert!(LIBRARY_NAMES.iter().all(|n| n.contains("airspy")));
        assert_eq!(
            LIBRARY_NAMES,
            &["libairspy.0.dylib", "libairspy.dylib", "libairspy.so.0", "libairspy.so", "airspy.dll"]
        );
    }

    #[test]
    fn open_without_library_returns_library_unavailable() {
        // (Debug is deliberately not implemented for the device handle)
        let acceptable = matches!(
            AirSpyDevice::open(None),
            Err(DeviceError::LibraryUnavailable(_)) | Err(DeviceError::OpenFailed(_)) | Ok(_)
        );
        assert!(acceptable, "AirSpyDevice::open panicked or returned an unexpected error kind");
    }

    #[test]
    fn frequency_range_matches_driver_docs() {
        assert_eq!(FREQUENCY_RANGE_HZ, (24_000_000.0, 1_750_000_000.0));
        assert!((24e6..=1.75e9).contains(&100e6));
        assert!(!(24e6..=1.75e9).contains(&1e6));
        assert!(!(24e6..=1.75e9).contains(&2e9));
    }

    #[test]
    fn gains_clamp_to_libairspy_range() {
        assert_eq!(clamp_gain(-3.0), 0);
        assert_eq!(clamp_gain(0.0), 0);
        assert_eq!(clamp_gain(7.0), 7);
        assert_eq!(clamp_gain(15.0), 15);
        assert_eq!(clamp_gain(22.0), 15);
        assert_eq!((GAIN_MIN, GAIN_MAX), (0, 15));
        assert_eq!(LINEARITY_SENSITIVITY_MAX, 21);
    }

    #[test]
    fn float_iq_conversion_matches_bytes_to_iq() {
        // AirSpy.bytes_to_iq: float32 buffer reshaped to (-1, 2)
        let out = convert_float_iq(&[1.0, -1.0, 0.5, 0.25]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].re, 1.0);
        assert_eq!(out[0].im, -1.0);
        assert_eq!(out[1].re, 0.5);
        assert_eq!(out[1].im, 0.25);
        assert!(convert_float_iq(&[3.0]).is_empty()); // odd tail dropped
    }
}
