//! libhackrf bindings (runtime dlopen) — port of `urh.dev.native.lib.hackrf`
//! plus the `HackRF.py` / `Rad1o.py` wrapper semantics.
//!
//! The shared library is loaded with [`libloading`] when [`HackRfDevice::open`]
//! is called, so the crate compiles on machines without the HackRF SDK;
//! a missing library surfaces as [`DeviceError::LibraryUnavailable`].
//!
//! The rad1o uses the very same libhackrf (see `Rad1o.py`); the only
//! differences mirrored here are the display name ("rad1o") and that the
//! rad1o has no bias tee / antenna-enable command.

use std::ffi::c_char;
use std::ffi::CStr;
use std::ffi::CString;
use std::fmt;
use std::os::raw::c_int;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libloading::Library;
use libloading::Symbol;

use crate::devices::{Command, Device, DeviceConfig, DeviceError, IqChunk};

/// Library file names tried (in order) when dlopening libhackrf.
pub const LIBRARY_NAMES: &[&str] = &[
    "libhackrf.0.dylib",
    "libhackrf.dylib",
    "libhackrf.so.0",
    "libhackrf.so",
    "hackrf.dll",
    "hackrf.0.dll",
];

/// Sleep before stopping rx/tx so the streaming thread observes the stop
/// flag (mirrors `TIMEOUT = 0.2` in `hackrf.pyx`).
const STOP_TIMEOUT: Duration = Duration::from_millis(200);

/// Poll interval while waiting for TX to finish.
const TX_POLL_INTERVAL: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// C API declarations (chackrf.pxd)
// ---------------------------------------------------------------------------

/// `typedef struct hackrf_device hackrf_device;` — opaque.
#[repr(C)]
struct HackRfDeviceOpaque {
    _private: [u8; 0],
}

/// `hackrf_transfer` (chackrf.pxd).
#[repr(C)]
struct HackRfTransfer {
    device: *mut HackRfDeviceOpaque,
    buffer: *mut u8,
    buffer_length: c_int,
    valid_length: c_int,
    rx_ctx: *mut c_void,
    tx_ctx: *mut c_void,
}

/// `hackrf_device_list_t` (a.k.a. `struct hackrf_device_list`).
#[repr(C)]
struct HackRfDeviceList {
    serial_numbers: *mut *mut c_char,
    usb_board_ids: *mut u16,
    usb_device_index: *mut c_int,
    devicecount: c_int,
    usb_devices: *mut *mut c_void,
    usb_devicecount: c_int,
}

/// `typedef int (*hackrf_sample_block_cb_fn)(hackrf_transfer* transfer)`.
type HackrfSampleBlockCb = unsafe extern "C" fn(*mut HackRfTransfer) -> c_int;

// libhackrf function types (all return hackrf_error codes, 0 = HACKRF_SUCCESS).
type FnHackrfInit = unsafe extern "C" fn() -> c_int;
type FnHackrfExit = unsafe extern "C" fn() -> c_int;
type FnHackrfOpen = unsafe extern "C" fn(*mut *mut HackRfDeviceOpaque) -> c_int;
type FnHackrfOpenBySerial =
    unsafe extern "C" fn(*const c_char, *mut *mut HackRfDeviceOpaque) -> c_int;
type FnHackrfClose = unsafe extern "C" fn(*mut HackRfDeviceOpaque) -> c_int;
type FnHackrfStartRx = unsafe extern "C" fn(
    *mut HackRfDeviceOpaque,
    HackrfSampleBlockCb,
    *mut c_void,
) -> c_int;
type FnHackrfStopRx = unsafe extern "C" fn(*mut HackRfDeviceOpaque) -> c_int;
type FnHackrfStartTx = unsafe extern "C" fn(
    *mut HackRfDeviceOpaque,
    HackrfSampleBlockCb,
    *mut c_void,
) -> c_int;
type FnHackrfStopTx = unsafe extern "C" fn(*mut HackRfDeviceOpaque) -> c_int;
type FnHackrfIsStreaming = unsafe extern "C" fn(*mut HackRfDeviceOpaque) -> c_int;
type FnHackrfSetFreq = unsafe extern "C" fn(*mut HackRfDeviceOpaque, u64) -> c_int;
type FnHackrfSetSampleRate = unsafe extern "C" fn(*mut HackRfDeviceOpaque, f64) -> c_int;
type FnHackrfSetBasebandFilterBandwidth =
    unsafe extern "C" fn(*mut HackRfDeviceOpaque, u32) -> c_int;
type FnHackrfSetAmpEnable = unsafe extern "C" fn(*mut HackRfDeviceOpaque, u8) -> c_int;
type FnHackrfSetAntennaEnable = unsafe extern "C" fn(*mut HackRfDeviceOpaque, u8) -> c_int;
type FnHackrfSetLnaGain = unsafe extern "C" fn(*mut HackRfDeviceOpaque, u32) -> c_int;
type FnHackrfSetVgaGain = unsafe extern "C" fn(*mut HackRfDeviceOpaque, u32) -> c_int;
type FnHackrfSetTxvgaGain = unsafe extern "C" fn(*mut HackRfDeviceOpaque, u32) -> c_int;
type FnHackrfDeviceList = unsafe extern "C" fn() -> *mut HackRfDeviceList;
type FnHackrfDeviceListFree = unsafe extern "C" fn(*mut HackRfDeviceList);

// ---------------------------------------------------------------------------
// Gain tables (chackrf.pxd comments / hackrf.pyx docstrings)
// ---------------------------------------------------------------------------

/// LNA (RX IF) gain: 0–40 dB in 8 dB steps.
pub const LNA_GAINS: &[u32] = &[0, 8, 16, 24, 32, 40];
/// VGA (RX baseband) gain: 0–62 dB in 2 dB steps.
pub const VGA_GAINS: &[u32] = &[0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40, 42, 44, 46, 48, 50, 52, 54, 56, 58, 60, 62];
/// TX VGA gain: 0–47 dB in 1 dB steps.
pub const TXVGA_MAX: u32 = 47;

/// Snap `value` to the nearest entry of `table`; `None` when out of range.
/// Ties resolve to the lower entry (like Python's `min(key=abs)` which keeps
/// the first minimum).
fn nearest_gain(table: &[u32], value: f64) -> Option<u32> {
    if !value.is_finite() || value < 0.0 || value > f64::from(table[table.len() - 1]) {
        return None;
    }
    let mut best = table[0];
    let mut best_dist = f64::INFINITY;
    for &g in table {
        let d = (f64::from(g) - value).abs();
        if d < best_dist {
            best_dist = d;
            best = g;
        }
    }
    Some(best)
}

/// Nearest legal LNA gain (0–40, 8 dB steps), `None` when out of range.
pub fn lna_gain_value(value: f64) -> Option<u32> {
    nearest_gain(LNA_GAINS, value)
}

/// Nearest legal VGA gain (0–62, 2 dB steps), `None` when out of range.
pub fn vga_gain_value(value: f64) -> Option<u32> {
    nearest_gain(VGA_GAINS, value)
}

/// Nearest legal TX VGA gain (0–47, 1 dB steps), `None` when out of range.
pub fn txvga_gain_value(value: f64) -> Option<u32> {
    if !value.is_finite() || value < 0.0 || value > f64::from(TXVGA_MAX) {
        return None;
    }
    Some(value.round() as u32)
}

// ---------------------------------------------------------------------------
// Streaming state shared with the C callbacks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    Rx,
    Tx,
}

struct RxShared {
    /// 0 = keep streaming (HACKRF_SUCCESS), -1 = stop (RUNNING in hackrf.pyx).
    running: AtomicI32,
    sink: Mutex<std::sync::mpsc::Sender<IqChunk>>,
}

struct TxShared {
    running: AtomicI32,
    /// Interleaved IQ bytes (i8 values stored as u8 for the C buffer).
    data: Mutex<Vec<u8>>,
    pos: AtomicUsize,
}

/// RX callback (`_c_callback_recv` in hackrf.pyx).
///
/// Converts the raw int8 IQ buffer to normalized [`Complex32`] samples
/// (each component divided by 128.0) and forwards them through the channel.
unsafe extern "C" fn rx_callback(transfer: *mut HackRfTransfer) -> c_int {
    // SAFETY: libhackrf guarantees `transfer` points to a valid, fully
    // populated hackrf_transfer for the duration of the callback, and
    // `rx_ctx` is exactly the `RxShared` pointer we passed to
    // hackrf_start_rx (it stays alive in the device struct until stop()).
    let t = &*transfer;
    if t.rx_ctx.is_null() {
        return -1;
    }
    let shared = &*(t.rx_ctx as *const RxShared);

    // Build the byte slice at the FFI boundary; everything below is panic-free
    // (no unwinding may cross the C boundary), but the channel send is
    // additionally guarded with catch_unwind as a belt-and-braces measure.
    let len = if t.valid_length > 0 && !t.buffer.is_null() {
        t.valid_length as usize
    } else {
        0
    };
    // SAFETY: `buffer` is valid for `valid_length` bytes per libhackrf contract.
    let bytes = std::slice::from_raw_parts(t.buffer, len);

    let result = std::panic::catch_unwind(|| {
        let mut chunk: IqChunk = Vec::with_capacity(len / 2);
        for pair in bytes.chunks_exact(2) {
            let re = f32::from(pair[0] as i8) / 128.0;
            let im = f32::from(pair[1] as i8) / 128.0;
            chunk.push(num_complex::Complex32::new(re, im));
        }
        // A closed receiver just means the consumer is gone: stop quietly.
        if let Ok(sink) = shared.sink.lock() {
            let _ = sink.send(chunk);
        }
    });
    match result {
        Ok(()) => shared.running.load(Ordering::Relaxed),
        Err(_) => -1,
    }
}

/// TX callback (`_c_callback_send` in hackrf.pyx): fills the transfer buffer
/// from the remaining samples, zero-pads the rest, and returns -1 once all
/// data was handed over (otherwise stop_tx hangs forever — see hackrf.pyx).
unsafe extern "C" fn tx_callback(transfer: *mut HackRfTransfer) -> c_int {
    // SAFETY: libhackrf guarantees `transfer` is valid for the duration of
    // the callback and `tx_ctx` is the TxShared pointer passed to
    // hackrf_start_tx (kept alive by the device struct until stop()).
    let t = &mut *transfer;
    if t.tx_ctx.is_null() {
        return -1;
    }
    let shared = &*(t.tx_ctx as *const TxShared);

    let valid = if t.valid_length > 0 && !t.buffer.is_null() {
        t.valid_length as usize
    } else {
        0
    };
    // SAFETY: `buffer` is writable for `valid_length` bytes (libhackrf contract).
    let buf = std::slice::from_raw_parts_mut(t.buffer, valid);

    let finished = {
        // lock() only fails on a poisoned mutex, which cannot happen here
        // because this closure never panics; recover the data regardless.
        let data = match shared.data.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let pos = shared.pos.load(Ordering::Relaxed).min(data.len());
        let remaining = (data.len() - pos).min(valid);
        buf[..remaining].copy_from_slice(&data[pos..pos + remaining]);
        for b in &mut buf[remaining..] {
            *b = 0;
        }
        shared.pos.store(pos + remaining, Ordering::Relaxed);
        pos + remaining >= data.len()
    };

    if finished {
        -1
    } else {
        shared.running.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// A HackRF (or rad1o) device opened through libhackrf.
pub struct HackRfDevice {
    lib: Library,
    device: *mut HackRfDeviceOpaque,
    rad1o: bool,
    mode: Mode,
    rx_shared: Option<Arc<RxShared>>,
    tx_shared: Option<Arc<TxShared>>,
    closed: bool,
}

// The raw device pointer is only dereferenced through &mut self methods of
// this struct, so single-threaded ownership is enough; the shared streaming
// state uses atomics/mutexes where the C callback threads touch it.
impl fmt::Debug for HackRfDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HackRfDevice")
            .field("rad1o", &self.rad1o)
            .field("mode", &self.mode)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

fn load_library() -> Result<Library, DeviceError> {
    let mut last = String::new();
    for name in LIBRARY_NAMES {
        // SAFETY: loading a library may run its initializers; libhackrf is a
        // trusted vendor library and loading it is exactly the intended use.
        match unsafe { Library::new(name) } {
            Ok(lib) => return Ok(lib),
            Err(e) => last = format!("{name}: {e}"),
        }
    }
    Err(DeviceError::LibraryUnavailable(format!(
        "none of {LIBRARY_NAMES:?} could be loaded ({last})"
    )))
}

/// Resolve a symbol from the loaded library.
fn sym<'lib, T>(lib: &'lib Library, name: &[u8]) -> Result<Symbol<'lib, T>, DeviceError>
where
    T: Sized,
{
    // SAFETY: resolving a symbol does not run any library code; the caller
    // is responsible for invoking it with a matching signature, which the
    // type aliases above guarantee (they mirror chackrf.pxd).
    unsafe { lib.get::<T>(name) }.map_err(|e| {
        DeviceError::LibraryUnavailable(format!("symbol {}: {e}", String::from_utf8_lossy(name)))
    })
}

fn param_err(cmd: &str, ret: c_int) -> DeviceError {
    DeviceError::ParameterRejected(format!("{cmd} failed with hackrf error {ret}"))
}

impl HackRfDevice {
    /// Open the first available HackRF (or the one with the given serial).
    pub fn open(identifier: Option<&str>) -> Result<Self, DeviceError> {
        Self::open_impl(false, identifier)
    }

    /// Open a rad1o (same libhackrf, different display name, no bias tee).
    pub fn open_rad1o(identifier: Option<&str>) -> Result<Self, DeviceError> {
        Self::open_impl(true, identifier)
    }

    fn open_impl(rad1o: bool, identifier: Option<&str>) -> Result<Self, DeviceError> {
        let lib = load_library()?;
        let init: Symbol<FnHackrfInit> = sym(&lib, b"hackrf_init\0")?;
        // SAFETY: matches the chackrf.pxd declaration `int hackrf_init()`.
        let ret = unsafe { init() };
        if ret != 0 {
            return Err(DeviceError::OpenFailed(format!("hackrf_init: {ret}")));
        }

        let mut device: *mut HackRfDeviceOpaque = std::ptr::null_mut();
        let open_ret = match identifier.filter(|s| !s.is_empty()) {
            Some(serial) => {
                let serial = CString::new(serial).map_err(|_| {
                    DeviceError::ParameterRejected("serial contains NUL byte".into())
                })?;
                let open_by_serial: Symbol<FnHackrfOpenBySerial> =
                    sym(&lib, b"hackrf_open_by_serial\0")?;
                // SAFETY: `device` is a valid out-pointer; `serial` is a
                // NUL-terminated C string kept alive for the call.
                unsafe { open_by_serial(serial.as_ptr(), &mut device) }
            }
            None => {
                let open: Symbol<FnHackrfOpen> = sym(&lib, b"hackrf_open\0")?;
                // SAFETY: `device` is a valid out-pointer.
                unsafe { open(&mut device) }
            }
        };
        if open_ret != 0 || device.is_null() {
            let exit: Symbol<FnHackrfExit> = sym(&lib, b"hackrf_exit\0")?;
            // SAFETY: matches `int hackrf_exit()`; balances the hackrf_init above.
            unsafe { exit() };
            return Err(DeviceError::OpenFailed(format!(
                "hackrf_open: {open_ret}"
            )));
        }

        Ok(Self {
            lib,
            device,
            rad1o,
            mode: Mode::Idle,
            rx_shared: None,
            tx_shared: None,
            closed: false,
        })
    }

    /// Apply all [`DeviceConfig`] parameters in the order `Device.py`'s
    /// `init_device` uses (frequency, sample rate, bandwidth, gains, bias tee).
    fn apply_config(&mut self, config: &DeviceConfig) -> Result<(), DeviceError> {
        self.set_parameter(Command::SetFrequency, config.frequency)?;
        self.set_parameter(Command::SetSampleRate, config.sample_rate)?;
        if config.bandwidth > 0.0 {
            self.set_parameter(Command::SetBandwidth, config.bandwidth)?;
        }
        self.set_parameter(Command::SetRfGain, config.rf_gain)?;
        self.set_parameter(Command::SetIfGain, config.if_gain)?;
        self.set_parameter(Command::SetBbGain, config.bb_gain)?;
        if !self.rad1o {
            let v = if config.bias_tee_enabled { 1.0 } else { 0.0 };
            self.set_parameter(Command::SetBiasTeeEnabled, v)?;
        }
        Ok(())
    }

    /// Stop streaming (rx or tx) but keep the device open.
    fn stop_streaming(&mut self) -> Result<(), DeviceError> {
        match self.mode {
            Mode::Rx => {
                if let Some(shared) = self.rx_shared.take() {
                    shared.running.store(-1, Ordering::Relaxed);
                    std::thread::sleep(STOP_TIMEOUT);
                }
                let stop_rx: Symbol<FnHackrfStopRx> = sym(&self.lib, b"hackrf_stop_rx\0")?;
                // SAFETY: `device` is the open hackrf_device handle.
                let ret = unsafe { stop_rx(self.device) };
                if ret != 0 {
                    return Err(DeviceError::StreamError(format!(
                        "hackrf_stop_rx: {ret}"
                    )));
                }
            }
            Mode::Tx => {
                if let Some(shared) = self.tx_shared.take() {
                    shared.running.store(-1, Ordering::Relaxed);
                }
                let stop_tx: Symbol<FnHackrfStopTx> = sym(&self.lib, b"hackrf_stop_tx\0")?;
                // SAFETY: `device` is the open hackrf_device handle.
                let ret = unsafe { stop_tx(self.device) };
                if ret != 0 {
                    return Err(DeviceError::StreamError(format!(
                        "hackrf_stop_tx: {ret}"
                    )));
                }
            }
            Mode::Idle => {}
        }
        self.mode = Mode::Idle;
        Ok(())
    }

    /// Close the device and release libhackrf (shutdown_device order:
    /// stop mode → close → exit).
    fn shutdown(&mut self) -> Result<(), DeviceError> {
        if self.closed {
            return Ok(());
        }
        self.stop_streaming()?;
        let close: Symbol<FnHackrfClose> = sym(&self.lib, b"hackrf_close\0")?;
        // SAFETY: `device` is the open hackrf_device handle, closed exactly once.
        let ret = unsafe { close(self.device) };
        let exit: Symbol<FnHackrfExit> = sym(&self.lib, b"hackrf_exit\0")?;
        // SAFETY: matches `int hackrf_exit()`; balances the hackrf_init in open.
        unsafe { exit() };
        self.closed = true;
        self.device = std::ptr::null_mut();
        if ret != 0 {
            return Err(DeviceError::StreamError(format!("hackrf_close: {ret}")));
        }
        Ok(())
    }
}

impl Device for HackRfDevice {
    fn name(&self) -> &'static str {
        if self.rad1o {
            "rad1o"
        } else {
            "HackRF"
        }
    }

    fn device_list() -> Vec<String> {
        let lib = match load_library() {
            Ok(lib) => lib,
            Err(_) => return Vec::new(),
        };
        let (init, exit, list, list_free) = (
            sym::<FnHackrfInit>(&lib, b"hackrf_init\0"),
            sym::<FnHackrfExit>(&lib, b"hackrf_exit\0"),
            sym::<FnHackrfDeviceList>(&lib, b"hackrf_device_list\0"),
            sym::<FnHackrfDeviceListFree>(&lib, b"hackrf_device_list_free\0"),
        );
        let (Ok(init), Ok(exit), Ok(list), Ok(list_free)) = (init, exit, list, list_free) else {
            return Vec::new();
        };

        let mut result = Vec::new();
        // SAFETY: hackrf_init/hackrf_exit are refcounted library setup/teardown
        // calls; hackrf_device_list returns a heap-allocated list that must be
        // released with hackrf_device_list_free, done below in all paths.
        unsafe {
            if init() != 0 {
                return Vec::new();
            }
            let device_list = list();
            if !device_list.is_null() {
                let count = (*device_list).devicecount.max(0) as usize;
                let serials = (*device_list).serial_numbers;
                if !serials.is_null() {
                    for i in 0..count {
                        let s = *serials.add(i);
                        if !s.is_null() {
                            result.push(CStr::from_ptr(s).to_string_lossy().into_owned());
                        }
                    }
                }
                list_free(device_list);
            }
            exit();
        }
        result
    }

    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        if self.closed {
            return Err(DeviceError::StreamError("device is closed".into()));
        }
        let dev = self.device;
        let ret = match cmd {
            Command::SetFrequency => {
                let f: Symbol<FnHackrfSetFreq> = sym(&self.lib, b"hackrf_set_freq\0")?;
                // SAFETY: `dev` is the open hackrf_device handle.
                unsafe { f(dev, value.max(0.0) as u64) }
            }
            Command::SetSampleRate => {
                let f: Symbol<FnHackrfSetSampleRate> =
                    sym(&self.lib, b"hackrf_set_sample_rate\0")?;
                // SAFETY: `dev` is the open hackrf_device handle.
                unsafe { f(dev, value) }
            }
            Command::SetBandwidth => {
                let f: Symbol<FnHackrfSetBasebandFilterBandwidth> =
                    sym(&self.lib, b"hackrf_set_baseband_filter_bandwidth\0")?;
                // SAFETY: `dev` is the open hackrf_device handle; libhackrf
                // rounds the bandwidth down to the nearest supported filter.
                unsafe { f(dev, value.max(0.0) as u32) }
            }
            Command::SetRfGain => {
                // RF "gain" is the external amplifier, boolean (set_amp_enable).
                let f: Symbol<FnHackrfSetAmpEnable> = sym(&self.lib, b"hackrf_set_amp_enable\0")?;
                // SAFETY: `dev` is the open hackrf_device handle.
                unsafe { f(dev, u8::from(value > 0.0)) }
            }
            Command::SetIfGain => {
                if self.mode == Mode::Tx {
                    // TX: txvga gain, 0–47 dB in 1 dB steps.
                    let gain = txvga_gain_value(value).ok_or_else(|| {
                        DeviceError::ParameterRejected(format!(
                            "TX IF gain {value} out of range 0..={TXVGA_MAX} dB"
                        ))
                    })?;
                    let f: Symbol<FnHackrfSetTxvgaGain> =
                        sym(&self.lib, b"hackrf_set_txvga_gain\0")?;
                    // SAFETY: `dev` is the open hackrf_device handle.
                    unsafe { f(dev, gain) }
                } else {
                    // RX: LNA gain, 0–40 dB in 8 dB steps (snap to nearest).
                    let gain = lna_gain_value(value).ok_or_else(|| {
                        DeviceError::ParameterRejected(format!(
                            "LNA gain {value} out of range 0..=40 dB"
                        ))
                    })?;
                    let f: Symbol<FnHackrfSetLnaGain> = sym(&self.lib, b"hackrf_set_lna_gain\0")?;
                    // SAFETY: `dev` is the open hackrf_device handle.
                    unsafe { f(dev, gain) }
                }
            }
            Command::SetBbGain => {
                // RX baseband (VGA) gain, 0–62 dB in 2 dB steps (snap to nearest).
                let gain = vga_gain_value(value).ok_or_else(|| {
                    DeviceError::ParameterRejected(format!(
                        "VGA gain {value} out of range 0..=62 dB"
                    ))
                })?;
                let f: Symbol<FnHackrfSetVgaGain> = sym(&self.lib, b"hackrf_set_vga_gain\0")?;
                // SAFETY: `dev` is the open hackrf_device handle.
                unsafe { f(dev, gain) }
            }
            Command::SetBiasTeeEnabled => {
                if self.rad1o {
                    // rad1o has no antenna-enable (bias tee) command.
                    return Err(DeviceError::Unsupported(
                        "rad1o has no bias tee",
                    ));
                }
                let f: Symbol<FnHackrfSetAntennaEnable> =
                    sym(&self.lib, b"hackrf_set_antenna_enable\0")?;
                // SAFETY: `dev` is the open hackrf_device handle.
                unsafe { f(dev, u8::from(value > 0.0)) }
            }
            other => {
                return Err(DeviceError::Unsupported(match other {
                    Command::Stop => "Stop is handled by stop()",
                    Command::SetDirectSamplingMode => "HackRF has no direct sampling mode",
                    Command::SetFrequencyCorrection => "HackRF has no frequency correction",
                    Command::SetChannelIndex => "HackRF has no channel index",
                    Command::SetAntennaIndex => "HackRF has no antenna index",
                    _ => "unsupported command",
                }));
            }
        };
        if ret != 0 {
            return Err(param_err(&cmd_debug(&cmd), ret));
        }
        Ok(())
    }

    fn start_rx(
        &mut self,
        config: &DeviceConfig,
        sink: std::sync::mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        if self.closed {
            return Err(DeviceError::StreamError("device is closed".into()));
        }
        self.stop_streaming()?;
        self.mode = Mode::Rx;
        self.apply_config(config)?;

        let shared = Arc::new(RxShared {
            running: AtomicI32::new(0),
            sink: Mutex::new(sink),
        });
        let start_rx: Symbol<FnHackrfStartRx> = sym(&self.lib, b"hackrf_start_rx\0")?;
        // SAFETY: `dev` is the open hackrf_device handle, `rx_callback` has
        // the hackrf_sample_block_cb_fn signature, and the ctx pointer is the
        // Arc<RxShared> kept alive by `self.rx_shared` until stop().
        let ret = unsafe {
            start_rx(
                self.device,
                rx_callback,
                Arc::as_ptr(&shared) as *mut c_void,
            )
        };
        if ret != 0 {
            self.mode = Mode::Idle;
            return Err(DeviceError::StreamError(format!("hackrf_start_rx: {ret}")));
        }
        self.rx_shared = Some(shared);
        Ok(())
    }

    fn send(&mut self, config: &DeviceConfig, samples: &[Complex32Slice]) -> Result<(), DeviceError> {
        if self.closed {
            return Err(DeviceError::StreamError("device is closed".into()));
        }
        self.stop_streaming()?;
        self.mode = Mode::Tx;
        self.apply_config(config)?;

        // Complex32 → interleaved int8 (inverse of the RX normalization;
        // iq_to_bytes in HackRF.py just flattens an int8 array).
        let mut data = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            data.push(clamp_i8(s.re * 128.0) as u8);
            data.push(clamp_i8(s.im * 128.0) as u8);
        }

        let shared = Arc::new(TxShared {
            running: AtomicI32::new(0),
            data: Mutex::new(data),
            pos: AtomicUsize::new(0),
        });
        let total = samples.len();
        let start_tx: Symbol<FnHackrfStartTx> = sym(&self.lib, b"hackrf_start_tx\0")?;
        // SAFETY: `dev` is the open hackrf_device handle, `tx_callback` has
        // the hackrf_sample_block_cb_fn signature, and the ctx pointer is the
        // Arc<TxShared> kept alive by `self.tx_shared` until stop().
        let ret = unsafe {
            start_tx(
                self.device,
                tx_callback,
                Arc::as_ptr(&shared) as *mut c_void,
            )
        };
        if ret != 0 {
            self.mode = Mode::Idle;
            return Err(DeviceError::StreamError(format!("hackrf_start_tx: {ret}")));
        }
        self.tx_shared = Some(shared);

        // Synchronous send: poll hackrf_is_streaming until the callback has
        // handed over all samples (it returns -1 when finished) or a generous
        // timeout based on the sample rate expires.
        let is_streaming: Symbol<FnHackrfIsStreaming> =
            sym(&self.lib, b"hackrf_is_streaming\0")?;
        let timeout = if config.sample_rate > 0.0 {
            Duration::from_secs_f64((total as f64 / config.sample_rate).max(0.5) * 2.0 + 5.0)
        } else {
            Duration::from_secs(30)
        };
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: `dev` is the open hackrf_device handle.
            let streaming = unsafe { is_streaming(self.device) };
            let done = self
                .tx_shared
                .as_ref()
                .map(|s| s.pos.load(Ordering::Relaxed) >= total * 2)
                .unwrap_or(true);
            if streaming != 1 || done || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(TX_POLL_INTERVAL);
        }

        self.stop_streaming()?;
        // Match the Python shutdown_device sequence after a send.
        self.shutdown()
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.shutdown()
    }
}

/// Alias used in the `send` signature (the crate-wide complex sample type).
type Complex32Slice = crate::Complex32;

fn clamp_i8(v: f32) -> i8 {
    v.round().clamp(-127.0, 127.0) as i8
}

fn cmd_debug(cmd: &Command) -> String {
    format!("{cmd:?}")
}

impl Drop for HackRfDevice {
    fn drop(&mut self) {
        if !self.closed && !self.device.is_null() {
            let _ = self.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_names_table() {
        assert_eq!(
            LIBRARY_NAMES,
            &[
                "libhackrf.0.dylib",
                "libhackrf.dylib",
                "libhackrf.so.0",
                "libhackrf.so",
                "hackrf.dll",
                "hackrf.0.dll",
            ]
        );
    }

    #[test]
    fn lna_gain_snapping() {
        // exact steps
        for &g in LNA_GAINS {
            assert_eq!(lna_gain_value(f64::from(g)), Some(g));
        }
        // snapped to nearest step (URH "assuming X" behaviour)
        assert_eq!(lna_gain_value(9.0), Some(8));
        assert_eq!(lna_gain_value(33.0), Some(32));
        assert_eq!(lna_gain_value(37.0), Some(40));
        assert_eq!(lna_gain_value(3.0), Some(0)); // tie -> lower
        // out of range
        assert_eq!(lna_gain_value(41.0), None);
        assert_eq!(lna_gain_value(-1.0), None);
        assert_eq!(lna_gain_value(f64::NAN), None);
    }

    #[test]
    fn vga_gain_snapping() {
        assert_eq!(vga_gain_value(0.0), Some(0));
        assert_eq!(vga_gain_value(62.0), Some(62));
        assert_eq!(vga_gain_value(33.0), Some(32)); // tie -> lower
        assert_eq!(vga_gain_value(34.0), Some(34));
        assert_eq!(vga_gain_value(61.0), Some(60)); // tie -> lower
        assert_eq!(vga_gain_value(63.0), None);
        assert_eq!(vga_gain_value(-2.0), None);
    }

    #[test]
    fn txvga_gain_snapping() {
        assert_eq!(txvga_gain_value(0.0), Some(0));
        assert_eq!(txvga_gain_value(47.0), Some(47));
        assert_eq!(txvga_gain_value(12.4), Some(12));
        assert_eq!(txvga_gain_value(12.6), Some(13));
        assert_eq!(txvga_gain_value(48.0), None);
        assert_eq!(txvga_gain_value(-1.0), None);
    }

    #[test]
    fn gain_tables_match_pyx_ranges() {
        // LNA: 0–40 step 8; VGA: 0–62 step 2 (chackrf.pxd comments).
        assert_eq!(LNA_GAINS.first(), Some(&0));
        assert_eq!(LNA_GAINS.last(), Some(&40));
        assert!(LNA_GAINS.windows(2).all(|w| w[1] - w[0] == 8));
        assert_eq!(VGA_GAINS.first(), Some(&0));
        assert_eq!(VGA_GAINS.last(), Some(&62));
        assert!(VGA_GAINS.windows(2).all(|w| w[1] - w[0] == 2));
    }

    #[test]
    fn open_without_library_returns_library_unavailable() {
        if lib_present() {
            // libhackrf is installed on this machine; only check that open
            // does not panic (it may fail later for lack of hardware).
            let _ = HackRfDevice::open(None);
            return;
        }
        match HackRfDevice::open(None) {
            Err(DeviceError::LibraryUnavailable(_)) => {}
            other => panic!("expected LibraryUnavailable, got {other:?}"),
        }
        match HackRfDevice::open_rad1o(Some("0123456789abcdef")) {
            Err(DeviceError::LibraryUnavailable(_)) => {}
            other => panic!("expected LibraryUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn device_list_without_library_is_empty_not_panic() {
        if lib_present() {
            // just must not panic
            let _ = <HackRfDevice as Device>::device_list();
            return;
        }
        assert!(<HackRfDevice as Device>::device_list().is_empty());
    }

    /// True when any of [`LIBRARY_NAMES`] can be dlopened right now.
    fn lib_present() -> bool {
        LIBRARY_NAMES.iter().any(|n| {
            // SAFETY: probe load only; the HackRF vendor library's
            // initializers are trusted, and the Library is dropped at once.
            unsafe { Library::new(n) }.is_ok()
        })
    }

    #[test]
    fn clamp_i8_bounds() {
        assert_eq!(clamp_i8(300.0), 127);
        assert_eq!(clamp_i8(-300.0), -127);
        assert_eq!(clamp_i8(0.4), 0);
        assert_eq!(clamp_i8(0.6), 1);
    }

    /// Real hardware round trip — run manually with a HackRF attached.
    #[test]
    #[ignore = "requires a connected HackRF and libhackrf"]
    fn hardware_rx_smoke() {
        let mut dev = HackRfDevice::open(None).expect("open hackrf");
        let (tx, rx) = std::sync::mpsc::channel();
        let config = DeviceConfig {
            frequency: 100_000_000.0,
            sample_rate: 8_000_000.0,
            bandwidth: 8_000_000.0,
            rf_gain: 0.0,
            if_gain: 16.0,
            bb_gain: 20.0,
            ..Default::default()
        };
        dev.start_rx(&config, tx).expect("start rx");
        std::thread::sleep(Duration::from_millis(500));
        let chunk = rx.recv_timeout(Duration::from_secs(2)).expect("iq chunk");
        assert!(!chunk.is_empty());
        dev.stop().expect("stop");
    }
}
