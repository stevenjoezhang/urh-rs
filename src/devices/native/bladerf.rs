//! BladeRF backend — runtime dlopen binding to libbladeRF.
//!
//! Port of `urh.dev.native.lib.bladerf` (bladerf.pyx / cbladerf.pxd) and the
//! `BladeRF.py` wrapper. The Python layer uses the *synchronous* stream API
//! (`bladerf_sync_config` + `bladerf_sync_rx`/`bladerf_sync_tx`), so the Rust
//! port runs the blocking reads on a background thread.
//!
//! Sample conversion (mirrors `BladeRF.bytes_to_iq` / `iq_to_bytes`):
//! the device delivers SC16 Q11 (`int16`), URH shifts left by 4 on RX and
//! right by 4 on TX; the Rust port applies the same shift and normalizes to
//! `Complex32` by dividing by 32768.0.

use std::ffi::c_void;
use std::ffi::CStr;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::devices::{Command, Device, DeviceConfig, DeviceError, IqChunk};
use crate::Complex32;

/// Shared-library file names tried (in order) at open time.
pub const LIBRARY_NAMES: &[&str] = &[
    "libbladeRF.2.dylib",
    "libbladeRF.dylib",
    "libbladeRF.so.2",
    "libbladeRF.so",
    "bladeRF.dll",
];

// ---------------------------------------------------------------------------
// libbladeRF C API (subset declared in cbladerf.pxd)
// ---------------------------------------------------------------------------

/// `struct bladerf` — opaque.
#[repr(C)]
struct BladerfDevice {
    _private: [u8; 0],
}

/// `struct bladerf_devinfo` (relevant part: the 33-byte serial).
#[repr(C)]
struct BladerfDevinfo {
    backend: c_int,
    serial: [c_char; 33],
    usb_bus: u8,
    usb_addr: u8,
    instance: c_uint,
}

/// `struct bladerf_version`.
#[repr(C)]
struct BladerfVersion {
    major: u16,
    minor: u16,
    patch: u16,
    describe: *const c_char,
}

type FnGetDeviceList = unsafe extern "C" fn(*mut *mut BladerfDevinfo) -> c_int;
type FnFreeDeviceList = unsafe extern "C" fn(*mut BladerfDevinfo);
type FnOpen = unsafe extern "C" fn(*mut *mut BladerfDevice, *const c_char) -> c_int;
type FnClose = unsafe extern "C" fn(*mut BladerfDevice);
type FnEnableModule = unsafe extern "C" fn(*mut BladerfDevice, c_int, bool) -> c_int;
type FnSetGainMode = unsafe extern "C" fn(*mut BladerfDevice, c_int, c_int) -> c_int;
type FnSetGain = unsafe extern "C" fn(*mut BladerfDevice, c_int, c_int) -> c_int;
type FnSetSampleRate =
    unsafe extern "C" fn(*mut BladerfDevice, c_int, c_uint, *mut c_uint) -> c_int;
type FnSetBandwidth =
    unsafe extern "C" fn(*mut BladerfDevice, c_int, c_uint, *mut c_uint) -> c_int;
type FnSetFrequency = unsafe extern "C" fn(*mut BladerfDevice, c_int, u64) -> c_int;
type FnSetBiasTee = unsafe extern "C" fn(*mut BladerfDevice, c_int, bool) -> c_int;
type FnSyncConfig = unsafe extern "C" fn(
    *mut BladerfDevice,
    c_int, // channel layout (bladerf_channel_layout)
    c_int, // format (BLADERF_FORMAT_SC16_Q11 = 0)
    c_uint,
    c_uint,
    c_uint,
    c_uint,
) -> c_int;
type FnSyncRx =
    unsafe extern "C" fn(*mut BladerfDevice, *mut c_void, c_uint, *mut c_void, c_uint) -> c_int;
type FnSyncTx =
    unsafe extern "C" fn(*mut BladerfDevice, *const c_void, c_uint, *mut c_void, c_uint) -> c_int;
type FnVersion = unsafe extern "C" fn(*mut BladerfVersion);

/// `BLADERF_FORMAT_SC16_Q11`.
const BLADERF_FORMAT_SC16_Q11: c_int = 0;
/// `bladerf_channel_layout` values (libbladeRF 2.x).
const BLADERF_RX_X1: c_int = 0;
const BLADERF_TX_X1: c_int = 1;
const BLADERF_RX_X2: c_int = 2;
const BLADERF_TX_X2: c_int = 3;
/// `BLADERF_GAIN_MGC` (manual gain control, libbladeRF >= 1.91).
const BLADERF_GAIN_MGC: c_int = 1;

/// `BLADERF_CHANNEL_RX(ch)` = `-(1 + 2*ch)` (libbladeRF 2.x channel encoding).
pub const fn blade_channel_rx(ch: i32) -> c_int {
    -1 - 2 * ch
}

/// `BLADERF_CHANNEL_TX(ch)` = `1 + 2*ch`.
pub const fn blade_channel_tx(ch: i32) -> c_int {
    1 + 2 * ch
}

/// Channel layout for a stream: X1 (SISO) for channel 0, X2 (MIMO) otherwise
/// (port of `get_current_channel_layout`).
fn channel_layout(channel: i32, is_tx: bool) -> c_int {
    match (is_tx, channel) {
        (true, 0) => BLADERF_TX_X1,
        (true, _) => BLADERF_TX_X2,
        (false, 0) => BLADERF_RX_X1,
        (false, _) => BLADERF_RX_X2,
    }
}

/// Raw device pointer wrapper that can be moved onto the RX thread.
struct DevPtr(*mut BladerfDevice);
// SAFETY: the raw pointer is only dereferenced on the RX thread while the
/// owning BladeRfDevice keeps the device open; the thread is joined in
/// stop()/Drop before the device is closed or the library is unloaded.
unsafe impl Send for DevPtr {}

impl DevPtr {
    /// Accessor so closures capture the whole (Send) wrapper instead of the
    /// raw pointer field (Rust 2021 disjoint captures would grab `self.0`).
    fn ptr(&self) -> *mut BladerfDevice {
        self.0
    }
}

/// Sync stream configuration used by `prepare_sync` in bladerf.pyx.
const SYNC_NUM_BUFFERS: c_uint = 32;
const SYNC_BUFFER_SIZE: c_uint = 2048;
const SYNC_NUM_TRANSFERS: c_uint = 16;
const SYNC_TIMEOUT_MS: c_uint = 100;

/// `SYNC_RX_CHUNK_SIZE` / `SYNC_TX_CHUNK_SIZE` from BladeRF.py.
pub const SYNC_CHUNK_SIZE: usize = 16384;

/// libbladeRF error names for the codes URH surfaces (subset of
/// `bladerf.h` error codes, as logged by the Python layer).
fn error_string(code: c_int) -> String {
    match code {
        0 => "BLADERF_ERR_UNEXPECTED(0)".into(),
        -1 => "BLADERF_ERR_UNEXPECTED".into(),
        -2 => "BLADERF_ERR_RANGE".into(),
        -3 => "BLADERF_ERR_INVAL".into(),
        -4 => "BLADERF_ERR_MEM".into(),
        -5 => "BLADERF_ERR_IO".into(),
        -6 => "BLADERF_ERR_TIMEOUT".into(),
        -7 => "BLADERF_ERR_NO_FILE".into(),
        -8 => "BLADERF_ERR_UPDATE_FPGA".into(),
        -9 => "BLADERF_ERR_UPDATE_FW".into(),
        -10 => "BLADERF_ERR_TIME_PAST".into(),
        -11 => "BLADERF_ERR_QUEUE_FULL".into(),
        -12 => "BLADERF_ERR_QUEUE_EMPTY".into(),
        -13 => "BLADERF_ERR_NOT_SUPPORTED".into(),
        other => format!("bladerf error {other}"),
    }
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// A BladeRF device opened through libbladeRF (RX or TX).
pub struct BladeRfDevice {
    lib: libloading::Library,
    dev: *mut BladerfDevice,
    /// channel index (URH `SET_CHANNEL_INDEX`), 0 or 1
    channel: i32,
    is_tx: bool,
    /// background RX thread handle
    rx_thread: Option<std::thread::JoinHandle<()>>,
    rx_running: Option<Arc<AtomicBool>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl BladeRfDevice {
    /// dlopen libbladeRF and open a device. `identifier` is the device
    /// serial; `None` opens the first available device (port of
    /// `bladerf.open`, which builds `*:serial=<serial>`).
    pub fn open(identifier: Option<&str>) -> Result<Self, DeviceError> {
        let lib = load_library()?;
        let dev = unsafe {
            let open: libloading::Symbol<FnOpen> = lib
                .get(b"bladerf_open\0")
                .map_err(|e| DeviceError::OpenFailed(format!("bladerf_open: {e}")))?;
            let mut dev: *mut BladerfDevice = std::ptr::null_mut();
            let arg = match identifier {
                Some(serial) => Some(CString::new(format!("*:serial={serial}"))
                    .map_err(|e| DeviceError::OpenFailed(e.to_string()))?),
                None => None,
            };
            // SAFETY: `open` is a valid symbol of libbladeRF; `dev` is a valid
            // out-pointer and `arg` (when present) is a live NUL-terminated
            // C string. The returned pointer is stored and only passed back
            // to libbladeRF functions.
            let ret = open(&mut dev, arg.as_deref().map_or(std::ptr::null(), |s| s.as_ptr()));
            if ret != 0 {
                return Err(DeviceError::OpenFailed(format!(
                    "bladerf_open: {}",
                    error_string(ret)
                )));
            }
            dev
        };
        Ok(Self {
            lib,
            dev,
            channel: 0,
            is_tx: false,
            rx_thread: None,
            rx_running: None,
            last_error: Arc::new(Mutex::new(None)),
        })
    }

    /// libbladeRF API version as "major.minor" (port of `get_api_version`).
    pub fn api_version(&self) -> Result<(u16, u16), DeviceError> {
        unsafe {
            let version: libloading::Symbol<FnVersion> = self
                .lib
                .get(b"bladerf_version\0")
                .map_err(|e| DeviceError::OpenFailed(format!("bladerf_version: {e}")))?;
            let mut v = BladerfVersion {
                major: 0,
                minor: 0,
                patch: 0,
                describe: std::ptr::null(),
            };
            // SAFETY: valid symbol, `v` is a valid out-pointer.
            version(&mut v);
            Ok((v.major, v.minor))
        }
    }

    fn current_channel(&self) -> c_int {
        if self.is_tx {
            blade_channel_tx(self.channel)
        } else {
            blade_channel_rx(self.channel)
        }
    }

    fn enable_module(&self, enable: bool) -> Result<(), DeviceError> {
        unsafe {
            let f: libloading::Symbol<FnEnableModule> = self
                .lib
                .get(b"bladerf_enable_module\0")
                .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
            // SAFETY: valid symbol; `self.dev` is an open device pointer.
            let ret = f(self.dev, self.current_channel(), enable);
            if ret != 0 {
                return Err(DeviceError::OpenFailed(format!(
                    "bladerf_enable_module: {}",
                    error_string(ret)
                )));
            }
            Ok(())
        }
    }

    /// Port of `prepare_sync`: enable module + `bladerf_sync_config` with the
    /// parameters from bladerf.pyx (32 buffers, 2048 samples, 16 transfers).
    fn prepare_sync(&mut self) -> Result<(), DeviceError> {
        self.enable_module(true)?;
        unsafe {
            let f: libloading::Symbol<FnSyncConfig> = self
                .lib
                .get(b"bladerf_sync_config\0")
                .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
            // SAFETY: valid symbol; `self.dev` is an open device pointer.
            let ret = f(
                self.dev,
                channel_layout(self.channel, self.is_tx),
                BLADERF_FORMAT_SC16_Q11,
                SYNC_NUM_BUFFERS,
                SYNC_BUFFER_SIZE,
                SYNC_NUM_TRANSFERS,
                SYNC_TIMEOUT_MS,
            );
            if ret != 0 {
                return Err(DeviceError::OpenFailed(format!(
                    "bladerf_sync_config: {}",
                    error_string(ret)
                )));
            }
            Ok(())
        }
    }

    /// Kill the RX thread (if any) and wait for it.
    fn join_rx_thread(&mut self) {
        if let Some(running) = self.rx_running.take() {
            running.store(false, Ordering::SeqCst);
        }
        if let Some(t) = self.rx_thread.take() {
            let _ = t.join();
        }
    }
}

impl Device for BladeRfDevice {
    fn name(&self) -> &'static str {
        "BladeRF"
    }

    /// Serial numbers of attached devices (port of `get_device_list`).
    fn device_list() -> Vec<String> {
        let lib = match load_library() {
            Ok(l) => l,
            Err(_) => return vec![],
        };
        unsafe {
            let get_list: libloading::Symbol<FnGetDeviceList> =
                match lib.get(b"bladerf_get_device_list\0") {
                    Ok(f) => f,
                    Err(_) => return vec![],
                };
            let free_list: libloading::Symbol<FnFreeDeviceList> =
                match lib.get(b"bladerf_free_device_list\0") {
                    Ok(f) => f,
                    Err(_) => return vec![],
                };
            let mut list: *mut BladerfDevinfo = std::ptr::null_mut();
            // SAFETY: valid symbols; `list` is a valid out-pointer that
            // libbladeRF allocates and that we free below.
            let n = get_list(&mut list);
            if n <= 0 || list.is_null() {
                return vec![];
            }
            let mut result = Vec::new();
            for i in 0..n as usize {
                let info = &*list.add(i);
                if let Ok(serial) = CStr::from_ptr(info.serial.as_ptr()).to_str() {
                    result.push(serial.to_string());
                }
            }
            // SAFETY: `list` was allocated by bladerf_get_device_list above.
            free_list(list);
            result
        }
    }

    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        let ret = unsafe {
            match cmd {
                Command::SetFrequency => {
                    let f: libloading::Symbol<FnSetFrequency> = self
                        .lib
                        .get(b"bladerf_set_frequency\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer; the frequency
                    // is passed by value.
                    f(self.dev, self.current_channel(), value as u64)
                }
                Command::SetSampleRate => {
                    let f: libloading::Symbol<FnSetSampleRate> = self
                        .lib
                        .get(b"bladerf_set_sample_rate\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer; `actual` may
                    // be NULL per the API.
                    f(self.dev, self.current_channel(), value as c_uint, std::ptr::null_mut())
                }
                Command::SetBandwidth => {
                    let f: libloading::Symbol<FnSetBandwidth> = self
                        .lib
                        .get(b"bladerf_set_bandwidth\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer.
                    f(self.dev, self.current_channel(), value as c_uint, std::ptr::null_mut())
                }
                Command::SetRfGain => {
                    // RX: switch to manual gain first (port of `set_gain`).
                    if !self.is_tx {
                        let f: libloading::Symbol<FnSetGainMode> = self
                            .lib
                            .get(b"bladerf_set_gain_mode\0")
                            .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                        // SAFETY: valid symbol, open device pointer.
                        f(self.dev, self.current_channel(), BLADERF_GAIN_MGC);
                    }
                    let f: libloading::Symbol<FnSetGain> = self
                        .lib
                        .get(b"bladerf_set_gain\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer.
                    f(self.dev, self.current_channel(), value as c_int)
                }
                Command::SetChannelIndex => {
                    self.channel = value as i32;
                    return Ok(());
                }
                Command::SetBiasTeeEnabled => {
                    let f: libloading::Symbol<FnSetBiasTee> = match self.lib.get(b"bladerf_set_bias_tee\0")
                    {
                        Ok(f) => f,
                        // bladerf.pyx returns -1 when BLADERF_API_VERSION < 2
                        Err(_) => {
                            return Err(DeviceError::Unsupported(
                                "bias tee requires libbladeRF API >= 2.0",
                            ))
                        }
                    };
                    // SAFETY: valid symbol, open device pointer.
                    f(self.dev, self.current_channel(), value != 0.0)
                }
                _ => {
                    return Err(DeviceError::Unsupported(
                        "command not supported by BladeRF backend",
                    ))
                }
            }
        };
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(error_string(ret)));
        }
        Ok(())
    }

    fn start_rx(
        &mut self,
        config: &DeviceConfig,
        sink: std::sync::mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        if self.rx_thread.is_some() {
            return Err(DeviceError::StreamError("RX already running".into()));
        }
        self.is_tx = false;
        self.channel = config.channel_index;
        // Port of Device.init_device: apply parameters before streaming.
        self.set_parameter(Command::SetFrequency, config.frequency)?;
        self.set_parameter(Command::SetSampleRate, config.sample_rate)?;
        self.set_parameter(Command::SetBandwidth, config.bandwidth)?;
        if config.rf_gain > 0.0 {
            self.set_parameter(Command::SetRfGain, config.rf_gain)?;
        }
        self.prepare_sync()?;

        // SYNC_RX_CHUNK_SIZE scales with the sample rate
        // (adapt_num_read_samples_to_sample_rate in BladeRF.py).
        let chunk = (SYNC_CHUNK_SIZE as f64 * (config.sample_rate / 1e6).max(1.0)) as usize;
        let chunk = chunk.max(SYNC_CHUNK_SIZE);

        let running = Arc::new(AtomicBool::new(true));
        let last_error = Arc::clone(&self.last_error);
        let dev = DevPtr(self.dev);
        // Resolve the symbol now and pass the raw function pointer to the
        // thread, so the thread does not need to borrow `self.lib`. The
        // library itself stays owned by `self`; the thread is joined in
        // stop()/Drop before the library is ever dropped.
        let sync_rx: FnSyncRx = unsafe {
            let f: libloading::Symbol<FnSyncRx> = self
                .lib
                .get(b"bladerf_sync_rx\0")
                .map_err(|e| DeviceError::OpenFailed(format!("bladerf_sync_rx: {e}")))?;
            // SAFETY: copy of the loaded symbol's fn pointer (fn pointers are
            // Copy); the signature matches libbladeRF's declaration. The
            // underlying dlopen handle is kept alive by `self.lib`, and the
            // thread using this pointer is joined before `self` is dropped.
            *f
        };

        let running_thread = Arc::clone(&running);
        let thread = std::thread::Builder::new()
            .name("bladerf-rx".into())
            .spawn(move || {
                let mut buf = vec![0i16; 2 * chunk];
                while running_thread.load(Ordering::SeqCst) {
                    // SAFETY: `dev` is an open device prepared for sync RX and
                    // `sync_rx` is a valid libbladeRF symbol; `buf` provides
                    // `chunk` sample slots, metadata may be NULL, timeout 100ms
                    // like receive_sync in bladerf.pyx. The thread is joined in
                    // stop()/Drop before the library or device is released.
                    let ret = unsafe {
                        sync_rx(
                            dev.ptr(),
                            buf.as_mut_ptr() as *mut c_void,
                            chunk as c_uint,
                            std::ptr::null_mut(),
                            SYNC_TIMEOUT_MS,
                        )
                    };
                    if ret != 0 {
                        *last_error.lock().unwrap() = Some(error_string(ret));
                        break;
                    }
                    // SC16 Q11 -> Q15 shift (bytes_to_iq: int16 << 4), then
                    // normalize to [-1, 1).
                    let chunk_iq: Vec<Complex32> = buf
                        .chunks_exact(2)
                        .map(|p| {
                            Complex32::new(
                                ((p[0] << 4) as f32) / 32768.0,
                                ((p[1] << 4) as f32) / 32768.0,
                            )
                        })
                        .collect();
                    if sink.send(chunk_iq).is_err() {
                        break; // receiver dropped
                    }
                }
                running_thread.store(false, Ordering::SeqCst);
            })
            .map_err(|e| DeviceError::StreamError(e.to_string()))?;

        self.rx_running = Some(running);
        self.rx_thread = Some(thread);
        Ok(())
    }

    fn send(&mut self, config: &DeviceConfig, samples: &[Complex32]) -> Result<(), DeviceError> {
        self.join_rx_thread();
        self.is_tx = true;
        self.channel = config.channel_index;
        self.set_parameter(Command::SetFrequency, config.frequency)?;
        self.set_parameter(Command::SetSampleRate, config.sample_rate)?;
        self.set_parameter(Command::SetBandwidth, config.bandwidth)?;
        if config.rf_gain > 0.0 {
            self.set_parameter(Command::SetRfGain, config.rf_gain)?;
        }
        self.prepare_sync()?;

        // iq_to_bytes: complex -> int16 with >> 4 (Q15 -> SC16 Q11).
        let mut buf = Vec::with_capacity(2 * samples.len());
        for s in samples {
            buf.push(((s.re * 32768.0) as i16) >> 4);
            buf.push(((s.im * 32768.0) as i16) >> 4);
        }
        unsafe {
            let f: libloading::Symbol<FnSyncTx> = self
                .lib
                .get(b"bladerf_sync_tx\0")
                .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
            // SAFETY: valid symbol, open device prepared for sync TX; `buf`
            // holds `samples.len()` interleaved SC16 Q11 samples; timeout 100ms
            // mirrors send_sync in bladerf.pyx.
            let ret = f(
                self.dev,
                buf.as_ptr() as *const c_void,
                samples.len() as c_uint,
                std::ptr::null_mut(),
                SYNC_TIMEOUT_MS,
            );
            if ret != 0 {
                return Err(DeviceError::StreamError(error_string(ret)));
            }
        }
        // Port of shutdown_device: disable module then close happens in stop().
        self.enable_module(false)?;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.join_rx_thread();
        if !self.dev.is_null() {
            unsafe {
                // SAFETY: disable the module first (otherwise libbladeRF warns
                // when closing during RX/TX — see close() in bladerf.pyx).
                if let Ok(f) = self
                    .lib
                    .get::<FnEnableModule>(b"bladerf_enable_module\0")
                {
                    let _ = f(self.dev, self.current_channel(), false);
                }
                let close: libloading::Symbol<FnClose> = self
                    .lib
                    .get(b"bladerf_close\0")
                    .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                // SAFETY: valid symbol; `self.dev` is the pointer returned by
                // bladerf_open and is not used afterwards.
                close(self.dev);
                self.dev = std::ptr::null_mut();
            }
        }
        Ok(())
    }
}

impl Drop for BladeRfDevice {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn load_library() -> Result<libloading::Library, DeviceError> {
    for name in LIBRARY_NAMES {
        // SAFETY: loading a shared library by name is the intended use of
        // libloading; all further calls into it go through checked symbols.
        if let Ok(lib) = crate::devices::native::try_open(name) {
            return Ok(lib);
        }
    }
    Err(DeviceError::LibraryUnavailable(format!(
        "none of {LIBRARY_NAMES:?} could be loaded"
    )))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_names_table_is_wellformed() {
        assert_eq!(LIBRARY_NAMES[0], "libbladeRF.2.dylib");
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".so.2")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dll")));
        assert!(LIBRARY_NAMES.iter().all(|n| n.contains("bladeRF")));
    }

    fn lib_present() -> bool {
        LIBRARY_NAMES.iter().any(|n| crate::devices::native::try_open(n).is_ok())
    }

    #[test]
    fn open_without_library_returns_library_unavailable() {
        if lib_present() {
            return; // lib present: real hardware test territory
        }
        match BladeRfDevice::open(None) {
            Err(DeviceError::LibraryUnavailable(_)) => {}
            Err(e) => panic!("expected LibraryUnavailable, got {e:?}"),
            Ok(_) => panic!("unexpectedly opened a device"),
        }
    }

    #[test]
    fn device_list_without_library_is_empty_not_panic() {
        assert!(BladeRfDevice::device_list().is_empty());
    }

    /// Channel encoding from libbladeRF.h: RX channels are negative,
    /// TX channels positive, spaced by 2.
    #[test]
    fn channel_macros_match_libbladerf() {
        assert_eq!(blade_channel_rx(0), -1);
        assert_eq!(blade_channel_rx(1), -3);
        assert_eq!(blade_channel_tx(0), 1);
        assert_eq!(blade_channel_tx(1), 3);
    }

    /// Channel layout selection (get_current_channel_layout).
    #[test]
    fn channel_layout_selection() {
        assert_eq!(channel_layout(0, false), BLADERF_RX_X1);
        assert_eq!(channel_layout(1, false), BLADERF_RX_X2);
        assert_eq!(channel_layout(0, true), BLADERF_TX_X1);
        assert_eq!(channel_layout(1, true), BLADERF_TX_X2);
    }

    /// Sync configuration constants from prepare_sync in bladerf.pyx.
    #[test]
    fn sync_config_constants_match_pyx() {
        assert_eq!(SYNC_NUM_BUFFERS, 32);
        assert_eq!(SYNC_BUFFER_SIZE, 2048);
        assert_eq!(SYNC_NUM_TRANSFERS, 16);
        assert_eq!(SYNC_TIMEOUT_MS, 100);
        assert_eq!(SYNC_CHUNK_SIZE, 16384);
    }

    #[test]
    fn error_string_maps_known_codes() {
        assert!(error_string(-2).contains("RANGE"));
        assert!(error_string(-6).contains("TIMEOUT"));
        assert!(error_string(-42).contains("42"));
    }

    #[test]
    #[ignore = "requires libbladeRF and a connected BladeRF"]
    fn hardware_open_and_rx() {
        let mut dev = BladeRfDevice::open(None).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        dev.start_rx(
            &DeviceConfig {
                frequency: 433.92e6,
                sample_rate: 1e6,
                bandwidth: 1.5e6,
                ..Default::default()
            },
            tx,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        dev.stop().unwrap();
        assert!(!rx.recv_timeout(std::time::Duration::from_millis(100)).unwrap().is_empty());
    }
}
