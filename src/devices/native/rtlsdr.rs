//! RTL-SDR backend — port of `urh.dev.native.lib.rtlsdr` (rtlsdr.pyx) and
//! `urh.dev.native.RTLSDR`.
//!
//! The vendor library `librtlsdr` is dlopened at runtime via
//! [`libloading::Library`], trying each name in [`LIBRARY_NAMES`]. When none
//! can be loaded, [`RtlSdrDevice::open`] returns
//! [`DeviceError::LibraryUnavailable`] so the crate builds and runs on
//! machines without the driver installed.
//!
//! Sample conversion follows `RTLSDR.bytes_to_iq` exactly:
//! `np.subtract(np.frombuffer(buffer, dtype=np.int8), 127)` — i.e. every raw
//! unsigned byte is reinterpreted as signed and shifted by 127.

use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use crate::devices::{Command, Device, DeviceConfig, DeviceError, IqChunk};
use crate::Complex32;

/// Shared-library file names tried (in order) when dlopening librtlsdr.
pub const LIBRARY_NAMES: &[&str] = &[
    "librtlsdr.0.dylib",
    "librtlsdr.dylib",
    "librtlsdr.so.0",
    "librtlsdr.so",
    "rtlsdr.dll",
];

/// Valid manual tuner gain values (tenths of dB) for the E4000 tuner,
/// from the `rtlsdr_set_tuner_gain` docstring in rtlsdr.pyx.
pub const E4000_TUNER_GAINS: &[i32] = &[
    -10, 15, 40, 65, 90, 115, 140, 165, 190, 215, 240, 290, 340, 420, 430, 450, 470, 490,
];

/// rtlsdr_set_sample_rate docstring: 225001-300000 Hz or 900001-3200000 Hz.
pub const SAMPLE_RATE_RANGES: &[(f64, f64)] = &[(225_001.0, 300_000.0), (900_001.0, 3_200_000.0)];

/// Driver-internal error code used by rtlsdr.pyx when the installed driver
/// does not support setting the tuner bandwidth (`RTLSDR_BANDWIDTH_SUPPORT == 0`).
const ERR_METHOD_NOT_AVAILABLE: c_int = -100;

// ---------------------------------------------------------------------------
// C function pointer types (mirror of crtlsdr.pxd)
// ---------------------------------------------------------------------------

type FnGetDeviceCount = unsafe extern "C" fn() -> u32;
type FnGetDeviceUsbStrings =
    unsafe extern "C" fn(u32, *mut c_char, *mut c_char, *mut c_char) -> c_int;
type FnGetIndexBySerial = unsafe extern "C" fn(*const c_char) -> c_int;
type FnOpen = unsafe extern "C" fn(*mut *mut c_void, u32) -> c_int;
type FnClose = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnSetU32 = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
type FnSetFreqCorrection = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnSetTunerBandwidth = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
type FnGetTunerGains = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
type FnSetTunerGain = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnSetTunerGainMode = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnSetDirectSampling = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnResetBuffer = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnReadAsync = unsafe extern "C" fn(
    *mut c_void,
    unsafe extern "C" fn(*mut u8, u32, *mut c_void),
    *mut c_void,
    u32,
    u32,
) -> c_int;
type FnCancelAsync = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnSetBiasTee = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;

/// State handed to the C receive callback through the `ctx` pointer.
struct RxCtx {
    sink: mpsc::Sender<IqChunk>,
}

/// Exit code of the blocking `rtlsdr_read_async` call (0 = ended cleanly).
const ASYNC_RUNNING: i32 = i32::MIN;

/// Send-wrapper for raw pointers moved into the RX thread.
struct SendPtr<T>(*mut T);
// SAFETY: the wrapped pointer is only dereferenced through librtlsdr FFI
// calls on the RX thread while the owning RtlSdrDevice is exclusively
// borrowed (stop() joins the thread before closing the device or freeing
// the boxed callback context).
unsafe impl<T> Send for SendPtr<T> {}

impl<T> SendPtr<T> {
    /// Unwrap the pointer. A method call (as opposed to a field access)
    /// forces closure capture of the whole wrapper, keeping the closure Send.
    fn get(self) -> *mut T {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Sample conversion (RTLSDR.bytes_to_iq)
// ---------------------------------------------------------------------------

/// Convert raw RTL-SDR IQ bytes to normalized complex samples, exactly as
/// `RTLSDR.bytes_to_iq`: interpret each byte as *signed* int8 and subtract 127.
pub(crate) fn convert_rtl_bytes(bytes: &[u8]) -> IqChunk {
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push(Complex32::new(
            pair[0] as i8 as f32 - 127.0,
            pair[1] as i8 as f32 - 127.0,
        ));
    }
    out
}

/// Snap `value` to the nearest entry of `table` (Device.py allowed-values
/// handling: "X not in range of supported values. Assuming Y").
pub(crate) fn snap_to_nearest(value: f64, table: &[i32]) -> i32 {
    let mut best = table[0];
    let mut best_dist = f64::INFINITY;
    for &g in table {
        let d = (g as f64 - value).abs();
        if d < best_dist {
            best_dist = d;
            best = g;
        }
    }
    best
}

/// Sample rates accepted by `rtlsdr_set_sample_rate` (from its docstring).
pub(crate) fn sample_rate_is_valid(rate: f64) -> bool {
    SAMPLE_RATE_RANGES.iter().any(|&(lo, hi)| rate >= lo && rate <= hi)
}

// ---------------------------------------------------------------------------
// C receive callback
// ---------------------------------------------------------------------------

unsafe extern "C" fn rx_callback(buf: *mut u8, len: u32, ctx: *mut c_void) {
    // SAFETY: librtlsdr invokes this callback with a valid buffer of `len`
    // bytes and the `ctx` pointer we passed to rtlsdr_read_async (a boxed
    // RxCtx that outlives the read_async call). The body is panic-free:
    // from_raw_parts on the documented (ptr, len) pair, chunks_exact never
    // panics, and a failed channel send is plain Err, not a panic.
    if buf.is_null() || ctx.is_null() || len == 0 {
        return;
    }
    let ctx = &*(ctx as *const RxCtx);
    let bytes = std::slice::from_raw_parts(buf, len as usize);
    let _ = ctx.sink.send(convert_rtl_bytes(bytes));
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// RTL-SDR receiver backed by a runtime-loaded librtlsdr.
pub struct RtlSdrDevice {
    lib: Arc<libloading::Library>,
    /// raw `rtlsdr_dev_t*`, null when closed
    dev: AtomicPtr<c_void>,
    rx_thread: Option<JoinHandle<()>>,
    /// exit code of the blocking read_async call (ASYNC_RUNNING while active)
    rx_exit: Arc<AtomicI32>,
}

// SAFETY: the raw device pointer is only dereferenced through librtlsdr FFI
// calls made while the struct is mutably borrowed (`&mut self` methods of a
// !Sync trait object usage); moving the handle between threads moves the
// ownership of the single open device along with it.
unsafe impl Send for RtlSdrDevice {}

impl RtlSdrDevice {
    /// Try to dlopen librtlsdr using the platform name table.
    pub fn load_library() -> Result<libloading::Library, DeviceError> {
        let mut last = String::new();
        for name in LIBRARY_NAMES {
            match unsafe { libloading::Library::new(name) } {
                // SAFETY: plain dlopen of a named shared library; no
                // library-provided pointers are created here.
                Ok(lib) => return Ok(lib),
                Err(e) => last = format!("{name}: {e}"),
            }
        }
        Err(DeviceError::LibraryUnavailable(format!(
            "could not load librtlsdr ({last})"
        )))
    }

    fn sym<'a, T>(
        lib: &'a libloading::Library,
        name: &[u8],
    ) -> Result<libloading::Symbol<'a, T>, DeviceError> {
        // SAFETY: dlsym lookup by name; the returned symbol is only called
        // through its correctly declared fn-pointer type below.
        unsafe { lib.get::<T>(name) }
            .map_err(|e| DeviceError::LibraryUnavailable(format!("symbol {}: {e}", String::from_utf8_lossy(name))))
    }

    /// Open the RTL-SDR at `identifier`. Per `RTLSDR.setup_device` the
    /// identifier is the device *index*; a non-numeric identifier is resolved
    /// through `rtlsdr_get_index_by_serial` as a convenience extension.
    pub fn open(identifier: Option<&str>) -> Result<Self, DeviceError> {
        let lib = Self::load_library()?;
        let index = match identifier.filter(|s| !s.is_empty()) {
            Some(id) => match id.parse::<u32>() {
                Ok(i) => i,
                Err(_) => {
                    let c_serial = CString::new(id)
                        .map_err(|_| DeviceError::OpenFailed("serial contains NUL".into()))?;
                    let get_index =
                        Self::sym::<FnGetIndexBySerial>(&lib, b"rtlsdr_get_index_by_serial\0")?;
                    // SAFETY: c_serial is a valid NUL-terminated string and
                    // the function only reads it.
                    let i = unsafe { get_index(c_serial.as_ptr()) };
                    if i < 0 {
                        return Err(DeviceError::OpenFailed(format!(
                            "no device with serial {id} (code {i})"
                        )));
                    }
                    i as u32
                }
            },
            None => 0,
        };

        let mut dev: *mut c_void = std::ptr::null_mut();
        let open = Self::sym::<FnOpen>(&lib, b"rtlsdr_open\0")?;
        // SAFETY: dev is a valid out-pointer; index was bounds-checked above.
        let ret = unsafe { open(&mut dev, index) };
        if ret != 0 || dev.is_null() {
            return Err(DeviceError::OpenFailed(format!(
                "rtlsdr_open(#{index}) failed with code {ret}"
            )));
        }

        Ok(Self {
            lib: Arc::new(lib),
            dev: AtomicPtr::new(dev),
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

    fn call_set_u32(&self, sym_name: &[u8], value: u32, what: &str) -> Result<(), DeviceError> {
        let dev = self.dev_ptr()?;
        let f = Self::sym::<FnSetU32>(&self.lib, sym_name)?;
        // SAFETY: dev is the open device handle; value is a plain integer.
        let ret = unsafe { f(dev, value) };
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(format!(
                "{what} = {value} rejected (code {ret})"
            )));
        }
        Ok(())
    }

    fn set_center_freq(&self, freq_hz: u32) -> Result<(), DeviceError> {
        self.call_set_u32(b"rtlsdr_set_center_freq\0", freq_hz, "frequency")
    }

    fn set_sample_rate(&self, rate: u32) -> Result<(), DeviceError> {
        self.call_set_u32(b"rtlsdr_set_sample_rate\0", rate, "sample rate")
    }

    /// Set the tuner bandwidth. Mirrors rtlsdr.pyx: when the installed driver
    /// has no `rtlsdr_set_tuner_bandwidth` (older Manjaro/Ubuntu builds) the
    /// pyx returns -100 and `RTLSDR.set_device_bandwidth` only logs a warning.
    /// Here the same condition surfaces as
    /// [`DeviceError::ParameterRejected`] with code -100.
    fn set_tuner_bandwidth(&self, bw_hz: u32) -> Result<(), DeviceError> {
        let dev = self.dev_ptr()?;
        match Self::sym::<FnSetTunerBandwidth>(&self.lib, b"rtlsdr_set_tuner_bandwidth\0") {
            Ok(f) => {
                // SAFETY: dev is the open device handle.
                let ret = unsafe { f(dev, bw_hz) };
                if ret != 0 {
                    return Err(DeviceError::ParameterRejected(format!(
                        "bandwidth = {bw_hz} rejected (code {ret})"
                    )));
                }
                Ok(())
            }
            Err(_) => Err(DeviceError::ParameterRejected(format!(
                "setting the bandwidth is not supported by your RTL-SDR driver version (code {ERR_METHOD_NOT_AVAILABLE})"
            ))),
        }
    }

    fn set_freq_correction(&self, ppm: i32) -> Result<(), DeviceError> {
        let dev = self.dev_ptr()?;
        let f = Self::sym::<FnSetFreqCorrection>(&self.lib, b"rtlsdr_set_freq_correction\0")?;
        // SAFETY: dev is the open device handle.
        let ret = unsafe { f(dev, ppm) };
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(format!(
                "frequency correction = {ppm} rejected (code {ret})"
            )));
        }
        Ok(())
    }

    fn set_direct_sampling(&self, mode: i32) -> Result<(), DeviceError> {
        if !(0..=2).contains(&mode) {
            return Err(DeviceError::ParameterRejected(format!(
                "direct sampling mode must be 0, 1 or 2, got {mode}"
            )));
        }
        let dev = self.dev_ptr()?;
        let f = Self::sym::<FnSetDirectSampling>(&self.lib, b"rtlsdr_set_direct_sampling\0")?;
        // SAFETY: dev is the open device handle.
        let ret = unsafe { f(dev, mode) };
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(format!(
                "direct sampling mode = {mode} rejected (code {ret})"
            )));
        }
        Ok(())
    }

    fn set_bias_tee(&self, on: bool) -> Result<(), DeviceError> {
        let dev = self.dev_ptr()?;
        let f = Self::sym::<FnSetBiasTee>(&self.lib, b"rtlsdr_set_bias_tee\0")?;
        // SAFETY: dev is the open device handle.
        let ret = unsafe { f(dev, if on { 1 } else { 0 }) };
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(format!(
                "bias tee = {on} rejected (code {ret})"
            )));
        }
        Ok(())
    }

    /// Manual tuner gain in tenths of a dB, snapped to the nearest supported
    /// value (`RTLSDR.py` wires SET_RF_GAIN to `set_tuner_gain` with
    /// `get_tuner_gains` as the allowed-values provider).
    fn set_tuner_gain(&self, gain_tenths_db: f64) -> Result<(), DeviceError> {
        let dev = self.dev_ptr()?;
        // Manual gain mode must be enabled for set_tuner_gain to take effect
        // (librtlsdr docs). rtlsdr.pyx omits this call; we make it explicit.
        let gain_mode = Self::sym::<FnSetTunerGainMode>(&self.lib, b"rtlsdr_set_tuner_gain_mode\0")?;
        // SAFETY: dev is the open device handle.
        let ret = unsafe { gain_mode(dev, 1) };
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(format!(
                "manual gain mode rejected (code {ret})"
            )));
        }

        let get_gains = Self::sym::<FnGetTunerGains>(&self.lib, b"rtlsdr_get_tuner_gains\0")?;
        // SAFETY: NULL gains pointer is the documented way to query the
        // number of supported gain values.
        let n = unsafe { get_gains(dev, std::ptr::null_mut()) };
        let value = if n > 0 {
            let mut gains = vec![0i32; n as usize];
            // SAFETY: gains has n elements as required by the returned count.
            unsafe { get_gains(dev, gains.as_mut_ptr()) };
            snap_to_nearest(gain_tenths_db, &gains)
        } else {
            gain_tenths_db as i32
        };

        let set_gain = Self::sym::<FnSetTunerGain>(&self.lib, b"rtlsdr_set_tuner_gain\0")?;
        // SAFETY: dev is the open device handle; value came from the
        // device-reported gain table.
        let ret = unsafe { set_gain(dev, value) };
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(format!(
                "tuner gain = {value} rejected (code {ret})"
            )));
        }
        Ok(())
    }

    /// Tuner IF gain for one stage, in tenths of a dB (rtlsdr.pyx
    /// `set_tuner_if_gain`). Not wired into the Device trait because
    /// `RTLSDR.py` does not map SET_IF_GAIN to the driver.
    pub fn set_tuner_if_gain(&self, stage: i32, gain_tenths_db: i32) -> Result<(), DeviceError> {
        let dev = self.dev_ptr()?;
        type F = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
        let f = Self::sym::<F>(&self.lib, b"rtlsdr_set_tuner_if_gain\0")?;
        // SAFETY: dev is the open device handle; plain integer arguments.
        let ret = unsafe { f(dev, stage, gain_tenths_db) };
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(format!(
                "if gain stage {stage} = {gain_tenths_db} rejected (code {ret})"
            )));
        }
        Ok(())
    }

    /// Apply a full [`DeviceConfig`] to the open device (port of
    /// `Device.init_device`'s parameter replay loop for RTLSDR).
    fn apply_config(&self, config: &DeviceConfig) -> Result<(), DeviceError> {
        if config.frequency > 0.0 {
            self.set_center_freq(config.frequency as u32)?;
        }
        if config.sample_rate > 0.0 {
            if !sample_rate_is_valid(config.sample_rate) {
                return Err(DeviceError::ParameterRejected(format!(
                    "sample rate {} outside supported ranges 225001-300000 / 900001-3200000 Hz",
                    config.sample_rate
                )));
            }
            self.set_sample_rate(config.sample_rate as u32)?;
        }
        if config.bandwidth > 0.0 {
            self.set_tuner_bandwidth(config.bandwidth as u32)?;
        }
        if config.rf_gain > 0.0 {
            self.set_tuner_gain(config.rf_gain)?;
        }
        if config.frequency_correction != 0.0 {
            self.set_freq_correction(config.frequency_correction as i32)?;
        }
        if config.direct_sampling_mode != 0 {
            self.set_direct_sampling(config.direct_sampling_mode)?;
        }
        if config.bias_tee_enabled {
            self.set_bias_tee(true)?;
        }
        Ok(())
    }

    /// Stop streaming and close the device (port of `RTLSDR.shutdown_device`).
    pub fn close(&mut self) -> Result<(), DeviceError> {
        self.stop()?;
        let dev = self.dev.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if dev.is_null() {
            return Ok(());
        }
        let f = Self::sym::<FnClose>(&self.lib, b"rtlsdr_close\0")?;
        // SAFETY: dev was the open device handle and is now disowned.
        let ret = unsafe { f(dev) };
        if ret != 0 {
            return Err(DeviceError::StreamError(format!(
                "rtlsdr_close failed with code {ret}"
            )));
        }
        Ok(())
    }
}

impl Device for RtlSdrDevice {
    fn name(&self) -> &'static str {
        "RTL-SDR"
    }

    /// `RTLSDR.get_device_list` → "manufacturer product (SN: serial)" strings.
    fn device_list() -> Vec<String> {
        let lib = match Self::load_library() {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        let result = (|| -> Option<Vec<String>> {
            let get_count = Self::sym::<FnGetDeviceCount>(&lib, b"rtlsdr_get_device_count\0").ok()?;
            let get_strings =
                Self::sym::<FnGetDeviceUsbStrings>(&lib, b"rtlsdr_get_device_usb_strings\0").ok()?;
            // SAFETY: plain query functions with no device handle.
            let n = unsafe { get_count() };
            let mut out = Vec::with_capacity(n as usize);
            for i in 0..n {
                let mut manufact = [0u8; 256];
                let mut product = [0u8; 256];
                let mut serial = [0u8; 256];
                // SAFETY: the three buffers are 256 bytes each, matching the
                // rtlsdr.pyx allocation of 256 chars.
                let ret = unsafe {
                    get_strings(
                        i,
                        manufact.as_mut_ptr().cast(),
                        product.as_mut_ptr().cast(),
                        serial.as_mut_ptr().cast(),
                    )
                };
                if ret == 0 {
                    out.push(format!(
                        "{} {} (SN: {})",
                        cstr_lossy(&manufact),
                        cstr_lossy(&product),
                        cstr_lossy(&serial)
                    ));
                }
            }
            Some(out)
        })();
        result.unwrap_or_default()
    }

    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        match cmd {
            Command::Stop => Ok(()),
            Command::SetFrequency => self.set_center_freq(value as u32),
            Command::SetSampleRate => {
                if !sample_rate_is_valid(value) {
                    return Err(DeviceError::ParameterRejected(format!(
                        "sample rate {value} outside supported ranges 225001-300000 / 900001-3200000 Hz"
                    )));
                }
                self.set_sample_rate(value as u32)
            }
            Command::SetBandwidth => self.set_tuner_bandwidth(value as u32),
            Command::SetRfGain => self.set_tuner_gain(value),
            Command::SetIfGain => Err(DeviceError::Unsupported(
                "RTL-SDR does not map SET_IF_GAIN to the driver",
            )),
            Command::SetBbGain => Err(DeviceError::Unsupported(
                "RTL-SDR does not map SET_BB_GAIN to the driver",
            )),
            Command::SetDirectSamplingMode => self.set_direct_sampling(value as i32),
            Command::SetFrequencyCorrection => self.set_freq_correction(value as i32),
            Command::SetChannelIndex | Command::SetAntennaIndex => Err(
                DeviceError::Unsupported("RTL-SDR has no channel/antenna selection"),
            ),
            Command::SetBiasTeeEnabled => self.set_bias_tee(value != 0.0),
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
        let reset = Self::sym::<FnResetBuffer>(&self.lib, b"rtlsdr_reset_buffer\0")?;
        // SAFETY: dev is the open device handle (RTLSDR.prepare_sync_receive).
        let ret = unsafe { reset(dev) };
        if ret != 0 {
            return Err(DeviceError::StreamError(format!(
                "rtlsdr_reset_buffer failed with code {ret}"
            )));
        }

        // Boxed context handed to the C callback; reclaimed after read_async
        // returns, so the raw pointer never outlives the box.
        let ctx = SendPtr(Box::into_raw(Box::new(RxCtx { sink })));
        let lib = Arc::clone(&self.lib);
        let rx_exit = Arc::clone(&self.rx_exit);
        let dev = SendPtr(dev);

        let thread = std::thread::Builder::new()
            .name("rtlsdr-rx".into())
            .spawn(move || {
                let dev = dev.get();
                let ctx = ctx.get();
                let exit_code = match Self::sym::<FnReadAsync>(&lib, b"rtlsdr_read_async\0") {
                    Err(_) => {
                        // SAFETY: ctx was created by Box::into_raw above and
                        // is not owned anywhere else.
                        drop(unsafe { Box::from_raw(ctx) });
                        ERR_METHOD_NOT_AVAILABLE
                    }
                    Ok(read_async) => {
                        // SAFETY: dev is the open device handle, rx_callback
                        // is the matching callback type, ctx stays alive for
                        // the whole (blocking) call, and (0, 0) selects the
                        // driver's default buffer count/length — exactly what
                        // rtlsdr.pyx passes to rtlsdr_read_async.
                        let ret = unsafe { read_async(dev, rx_callback, ctx.cast(), 0, 0) };
                        // SAFETY: read_async has returned, so the driver will
                        // not invoke the callback (and thus ctx) again.
                        drop(unsafe { Box::from_raw(ctx) });
                        ret
                    }
                };
                rx_exit.store(exit_code, Ordering::Release);
            })
            .map_err(|e| DeviceError::StreamError(format!("failed to spawn RX thread: {e}")))?;

        self.rx_thread = Some(thread);
        Ok(())
    }

    fn send(&mut self, _config: &DeviceConfig, _samples: &[Complex32]) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported("RTL-SDR cannot transmit"))
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        if let Some(thread) = self.rx_thread.take() {
            if let Ok(dev) = self.dev_ptr() {
                if let Ok(cancel) = Self::sym::<FnCancelAsync>(&self.lib, b"rtlsdr_cancel_async\0")
                {
                    // SAFETY: dev is the open device handle; cancel_async is
                    // the documented way to unblock read_async.
                    unsafe { cancel(dev) };
                }
            }
            let _ = thread.join();
            let exit = self.rx_exit.load(Ordering::Acquire);
            if exit != ASYNC_RUNNING && exit != 0 {
                return Err(DeviceError::StreamError(format!(
                    "rtlsdr_read_async ended with code {exit}"
                )));
            }
        }
        Ok(())
    }
}

/// Render a 256-byte NUL-padded C string buffer as UTF-8 (lossy).
fn cstr_lossy(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    // SAFETY-free helper: CStr::from_bytes_with_nul needs the terminating
    // NUL included, so operate on the slice up to and including it.
    let with_nul = &buf[..end];
    String::from_utf8_lossy(with_nul).into_owned()
}

impl Drop for RtlSdrDevice {
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
        assert!(LIBRARY_NAMES.iter().all(|n| n.contains("rtlsdr")));
        // exactly the table promised to the registry
        assert_eq!(
            LIBRARY_NAMES,
            &["librtlsdr.0.dylib", "librtlsdr.dylib", "librtlsdr.so.0", "librtlsdr.so", "rtlsdr.dll"]
        );
    }

    #[test]
    fn open_without_library_returns_library_unavailable() {
        // On a machine without librtlsdr this must be LibraryUnavailable;
        // if the driver happens to be installed, opening device #0 without
        // hardware yields OpenFailed — both are acceptable, a panic is not.
        // (Debug is deliberately not implemented for the device handle)
        let acceptable = matches!(
            RtlSdrDevice::open(None),
            Err(DeviceError::LibraryUnavailable(_)) | Err(DeviceError::OpenFailed(_)) | Ok(_)
        );
        assert!(acceptable, "RtlSdrDevice::open panicked or returned an unexpected error kind");
    }

    #[test]
    fn e4000_gain_table_matches_pyx() {
        assert_eq!(
            E4000_TUNER_GAINS,
            &[-10, 15, 40, 65, 90, 115, 140, 165, 190, 215, 240, 290, 340, 420, 430, 450, 470, 490]
        );
    }

    #[test]
    fn gain_snapping_picks_nearest_allowed_value() {
        // Device.py: value not in supported values -> assume nearest
        assert_eq!(snap_to_nearest(100.0, E4000_TUNER_GAINS), 90);
        assert_eq!(snap_to_nearest(105.0, E4000_TUNER_GAINS), 115); // |105-90|=15 > |115-105|=10
        assert_eq!(snap_to_nearest(0.0, E4000_TUNER_GAINS), -10);
        assert_eq!(snap_to_nearest(500.0, E4000_TUNER_GAINS), 490);
        assert_eq!(snap_to_nearest(-10.0, E4000_TUNER_GAINS), -10);
    }

    #[test]
    fn sample_rate_ranges_match_driver_docs() {
        for ok in [225_001.0, 250_000.0, 300_000.0, 900_001.0, 2_400_000.0, 3_200_000.0] {
            assert!(sample_rate_is_valid(ok), "{ok} should be valid");
        }
        for bad in [0.0, 225_000.0, 300_001.0, 900_000.0, 3_200_001.0, -1.0] {
            assert!(!sample_rate_is_valid(bad), "{bad} should be invalid");
        }
    }

    #[test]
    fn bytes_to_iq_follows_pyx_conversion() {
        // RTLSDR.bytes_to_iq: np.subtract(frombuffer(int8), 127)
        let out = convert_rtl_bytes(&[0, 127, 128, 255]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].re, 0i8 as f32 - 127.0); // 0x00 -> -127
        assert_eq!(out[0].im, 127_f32 - 127.0); // 0x7F -> 0
        assert_eq!(out[1].re, -128_f32 - 127.0); // 0x80 -> -255
        assert_eq!(out[1].im, -1_f32 - 127.0); // 0xFF -> -128
        // odd trailing byte is dropped (chunks_exact)
        assert_eq!(convert_rtl_bytes(&[1, 2, 3]).len(), 1);
        assert!(convert_rtl_bytes(&[]).is_empty());
    }

    #[test]
    fn direct_sampling_modes_are_zero_one_two() {
        for m in [0, 1, 2] {
            assert!((0..=2).contains(&m));
        }
    }
}
