//! LimeSDR backend — runtime dlopen binding to libLimeSuite.
//!
//! Port of `urh.dev.native.lib.limesdr` (limesdr.pyx / climesdr.pxd) and the
//! `LimeSDR.py` wrapper. The Python layer uses the synchronous-ish stream API
//! (`LMS_RecvStream`/`LMS_SendStream` with a 10 ms / 500 ms timeout), so the
//! Rust port runs blocking receives on a background thread.
//!
//! Sample conversion: LimeSuite streams are configured with `LMS_FMT_F32`, so
//! `bytes_to_iq` is the identity on interleaved f32 — the Rust port maps the
//! interleaved floats directly to `Complex32`.

use std::ffi::c_void;
use std::ffi::CStr;
use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::devices::{Command, Device, DeviceConfig, DeviceError, IqChunk};
use crate::Complex32;

/// Shared-library file names tried (in order) at open time.
pub const LIBRARY_NAMES: &[&str] = &[
    "libLimeSuite.23.11-1.dylib",
    "libLimeSuite.dylib",
    "libLimeSuite.so",
    "LimeSuite.dll",
];

// ---------------------------------------------------------------------------
// libLimeSuite C API (subset declared in climesdr.pxd)
// ---------------------------------------------------------------------------

/// `typedef void lms_device_t` — opaque.
#[repr(C)]
struct LmsDevice {
    _private: [u8; 0],
}

/// `typedef char lms_info_str_t[256]`.
type LmsInfoStr = [c_char; 256];

/// `typedef char lms_name_t[16]`.
type LmsName = [c_char; 16];

/// `struct lms_stream_meta_t`.
#[repr(C)]
#[derive(Default)]
struct LmsStreamMeta {
    timestamp: u64,
    wait_for_timestamp: bool,
    flush_partial_packet: bool,
}

/// `struct lms_stream_t` (stream handle filled in by LMS_SetupStream).
#[repr(C)]
struct LmsStream {
    handle: usize,
    is_tx: bool,
    channel: u32,
    fifo_size: u32,
    throughput_vs_latency: f32,
    /// `lms_stream_t::LMS_FMT_F32` = 0
    data_fmt: u32,
}

type CInt = std::os::raw::c_int;

type FnGetDeviceList = unsafe extern "C" fn(*mut LmsInfoStr) -> CInt;

type FnOpen = unsafe extern "C" fn(*mut *mut LmsDevice, *const c_char, *mut c_void) -> i32;
type FnClose = unsafe extern "C" fn(*mut LmsDevice) -> i32;
type FnInit = unsafe extern "C" fn(*mut LmsDevice) -> i32;
type FnEnableChannel =
    unsafe extern "C" fn(*mut LmsDevice, bool, usize, bool) -> i32;
type FnSetSampleRate = unsafe extern "C" fn(*mut LmsDevice, f64, usize) -> i32;
type FnSetLofFrequency = unsafe extern "C" fn(*mut LmsDevice, bool, usize, f64) -> i32;
type FnSetNormalizedGain = unsafe extern "C" fn(*mut LmsDevice, bool, usize, f64) -> i32;
type FnSetLpfBw = unsafe extern "C" fn(*mut LmsDevice, bool, usize, f64) -> i32;
type FnGetAntennaList = unsafe extern "C" fn(*mut LmsDevice, bool, usize, *mut LmsName) -> i32;
type FnSetAntenna = unsafe extern "C" fn(*mut LmsDevice, bool, usize, usize) -> i32;
type FnSetupStream = unsafe extern "C" fn(*mut LmsDevice, *mut LmsStream) -> i32;
type FnDestroyStream = unsafe extern "C" fn(*mut LmsDevice, *mut LmsStream);
type FnStartStream = unsafe extern "C" fn(*mut LmsStream) -> i32;
type FnStopStream = unsafe extern "C" fn(*mut LmsStream) -> i32;
type FnRecvStream =
    unsafe extern "C" fn(*mut LmsStream, *mut c_void, usize, *mut LmsStreamMeta, u32) -> i32;
type FnSendStream =
    unsafe extern "C" fn(*mut LmsStream, *const c_void, usize, *const LmsStreamMeta, u32) -> i32;
type FnGetLastErrorMessage = unsafe extern "C" fn() -> *const c_char;

/// Raw stream pointer wrapper that can be moved onto the RX thread.
struct StreamPtr(*mut LmsStream);
// SAFETY: the raw pointer is only dereferenced on the RX thread while the
/// owning LimeSdrDevice keeps the stream alive; the thread is joined in
/// stop()/Drop before the stream is destroyed or the library is unloaded.
unsafe impl Send for StreamPtr {}

impl StreamPtr {
    /// Accessor so closures capture the whole (Send) wrapper instead of the
    /// raw pointer field (Rust 2021 disjoint captures would grab `self.0`).
    fn ptr(&self) -> *mut LmsStream {
        self.0
    }
}

// ---------------------------------------------------------------------------
// constants from LimeSDR.py
// ---------------------------------------------------------------------------

pub const SYNC_RX_CHUNK_SIZE: usize = 32768;
pub const SYNC_TX_CHUNK_SIZE: usize = 32768;
pub const RECV_FIFO_SIZE: usize = 1_048_576;
pub const SEND_FIFO_SIZE: usize = 8 * SYNC_TX_CHUNK_SIZE;
pub const LIME_TIMEOUT_RECEIVE_MS: u32 = 10;
pub const LIME_TIMEOUT_SEND_MS: u32 = 500;

/// Gain handed to URH is in "percent" (0..100) and normalized to [0, 1]
/// before hitting LMS_SetNormalizedGain (`set_device_gain(gain * 0.01)`).
pub const GAIN_INPUT_MAX: f64 = 100.0;

/// Normalize a URH gain value (0..100) to LimeSuite's [0, 1] range,
/// clamping out-of-range input.
pub fn normalize_gain(gain: f64) -> f64 {
    (gain * 0.01).clamp(0.0, 1.0)
}

fn last_error(lib: &libloading::Library) -> String {
    unsafe {
        let f: libloading::Symbol<FnGetLastErrorMessage> =
            match lib.get(b"LMS_GetLastErrorMessage\0") {
                Ok(f) => f,
                Err(_) => return "unknown LMS error".into(),
            };
        // SAFETY: valid symbol; returns a NUL-terminated string owned by
        // LimeSuite that we only read.
        let msg = f();
        if msg.is_null() {
            "unknown LMS error".into()
        } else {
            CStr::from_ptr(msg).to_string_lossy().into_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// A LimeSDR device opened through libLimeSuite.
pub struct LimeSdrDevice {
    lib: libloading::Library,
    dev: *mut LmsDevice,
    /// URH `SET_CHANNEL_INDEX` (LimeSDR.py applies it when enabling the
    /// channel, before the stream is set up).
    channel: usize,
    is_tx: bool,
    /// live stream (created in start_rx/send, destroyed in stop)
    stream: Option<Box<LmsStream>>,
    rx_thread: Option<std::thread::JoinHandle<()>>,
    rx_running: Option<Arc<AtomicBool>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl LimeSdrDevice {
    /// dlopen libLimeSuite and open a device. `identifier` must be one of
    /// the info strings returned by [`Device::device_list`] (or `None` to
    /// open the first device) — port of `limesdr.open`.
    pub fn open(identifier: Option<&str>) -> Result<Self, DeviceError> {
        let lib = load_library()?;
        unsafe {
            let open: libloading::Symbol<FnOpen> = lib
                .get(b"LMS_Open\0")
                .map_err(|e| DeviceError::OpenFailed(format!("LMS_Open: {e}")))?;
            let mut dev: *mut LmsDevice = std::ptr::null_mut();
            let arg = match identifier {
                Some(info) => Some(
                    CString::new(info).map_err(|e| DeviceError::OpenFailed(e.to_string()))?,
                ),
                None => None,
            };
            // SAFETY: valid symbol; `dev` is a valid out-pointer, `arg` is a
            // live NUL-terminated info string when present; args is NULL per
            // limesdr.pyx.
            let ret = open(&mut dev, arg.as_deref().map_or(std::ptr::null(), |s| s.as_ptr()), std::ptr::null_mut());
            if ret != 0 {
                return Err(DeviceError::OpenFailed(format!(
                    "LMS_Open: {}",
                    last_error(&lib)
                )));
            }
            let mut device = Self {
                lib,
                dev,
                channel: 0,
                is_tx: false,
                stream: None,
                rx_thread: None,
                rx_running: None,
                last_error: Arc::new(Mutex::new(None)),
            };
            // setup_device in LimeSDR.py: disable all channels, then LMS_Init.
            device.enable_all_channels(false)?;
            let init: libloading::Symbol<FnInit> = device
                .lib
                .get(b"LMS_Init\0")
                .map_err(|e| DeviceError::OpenFailed(format!("LMS_Init: {e}")))?;
            // SAFETY: valid symbol, open device pointer.
            let ret = init(device.dev);
            if ret != 0 {
                return Err(DeviceError::OpenFailed(format!(
                    "LMS_Init: {}",
                    last_error(&device.lib)
                )));
            }
            Ok(device)
        }
    }

    /// Port of `enable_all_channels` / `disable_all_channels`.
    fn enable_all_channels(&mut self, enabled: bool) -> Result<(), DeviceError> {
        for is_tx in [false, true] {
            for ch in [0usize, 1] {
                unsafe {
                    let f: libloading::Symbol<FnEnableChannel> = self
                        .lib
                        .get(b"LMS_EnableChannel\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer.
                    let ret = f(self.dev, is_tx, ch, enabled);
                    if ret != 0 {
                        return Err(DeviceError::OpenFailed(format!(
                            "LMS_EnableChannel(tx={is_tx}, ch={ch}): {}",
                            last_error(&self.lib)
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Names of the antennas of the current channel (`get_antenna_list`).
    pub fn antenna_list(&self) -> Vec<String> {
        unsafe {
            let f: libloading::Symbol<FnGetAntennaList> =
                match self.lib.get(b"LMS_GetAntennaList\0") {
                    Ok(f) => f,
                    Err(_) => return vec![],
                };
            let mut list: [LmsName; 16] = std::mem::zeroed();
            // SAFETY: valid symbol, open device pointer; `list` provides 16
            // lms_name_t slots (LimeSDR never exposes more than 10 paths).
            let n = f(self.dev, self.is_tx, self.channel, list.as_mut_ptr());
            if n <= 0 {
                return vec![];
            }
            (0..n as usize)
                .map(|i| {
                    CStr::from_ptr(list[i].as_ptr())
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }
    }

    /// Port of `setup_stream` + `start_stream`.
    fn setup_and_start_stream(&mut self, fifo_size: u32, is_tx: bool) -> Result<(), DeviceError> {
        let mut stream = Box::new(LmsStream {
            handle: 0,
            is_tx,
            channel: self.channel as u32,
            fifo_size,
            throughput_vs_latency: 0.0, // optimize for minimum latency (pyx)
            data_fmt: 0,                // lms_stream_t::LMS_FMT_F32
        });
        unsafe {
            let setup: libloading::Symbol<FnSetupStream> = self
                .lib
                .get(b"LMS_SetupStream\0")
                .map_err(|e| DeviceError::OpenFailed(format!("LMS_SetupStream: {e}")))?;
            // SAFETY: valid symbol, open device pointer; `stream` is a valid
            // lms_stream_t that LMS_SetupStream fills with the handle.
            let ret = setup(self.dev, stream.as_mut());
            if ret != 0 {
                return Err(DeviceError::OpenFailed(format!(
                    "LMS_SetupStream: {}",
                    last_error(&self.lib)
                )));
            }
            let start: libloading::Symbol<FnStartStream> = self
                .lib
                .get(b"LMS_StartStream\0")
                .map_err(|e| DeviceError::OpenFailed(format!("LMS_StartStream: {e}")))?;
            // SAFETY: valid symbol; the stream was set up above.
            let ret = start(stream.as_mut());
            if ret != 0 {
                return Err(DeviceError::OpenFailed(format!(
                    "LMS_StartStream: {}",
                    last_error(&self.lib)
                )));
            }
        }
        self.stream = Some(stream);
        Ok(())
    }

    /// Port of `stop_stream` + `destroy_stream`.
    fn stop_and_destroy_stream(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            unsafe {
                if let Ok(f) = self.lib.get::<FnStopStream>(b"LMS_StopStream\0") {
                    // SAFETY: valid symbol; `stream` was set up by
                    // LMS_SetupStream and is still owned by us.
                    let _ = f(stream.as_mut());
                }
                if let Ok(f) = self.lib.get::<FnDestroyStream>(b"LMS_DestroyStream\0") {
                    // SAFETY: valid symbol, open device pointer, our stream.
                    f(self.dev, stream.as_mut());
                }
            }
        }
    }

    fn join_rx_thread(&mut self) {
        if let Some(running) = self.rx_running.take() {
            running.store(false, Ordering::SeqCst);
        }
        if let Some(t) = self.rx_thread.take() {
            let _ = t.join();
        }
    }
}

impl Device for LimeSdrDevice {
    fn name(&self) -> &'static str {
        "LimeSDR"
    }

    /// Info strings of attached devices (port of `get_device_list`).
    fn device_list() -> Vec<String> {
        let lib = match load_library() {
            Ok(l) => l,
            Err(_) => return vec![],
        };
        unsafe {
            let f: libloading::Symbol<FnGetDeviceList> =
                match lib.get(b"LMS_GetDeviceList\0") {
                    Ok(f) => f,
                    Err(_) => return vec![],
                };
            // limesdr.pyx allocates 256 info strings
            let mut list: Box<[LmsInfoStr]> = vec![[0; 256]; 256].into_boxed_slice();
            // SAFETY: valid symbol; `list` provides 256 lms_info_str_t slots.
            let n = f(list.as_mut_ptr());
            if n <= 0 {
                return vec![];
            }
            (0..n as usize)
                .map(|i| {
                    CStr::from_ptr(list[i].as_ptr())
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }
    }

    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        // NOTE (LimeSDR.py): "Set Antenna needs to be called before other
        // stuff!!!" — the antenna command is applied eagerly here; all other
        // parameters are applied when the channel is (re)configured.
        let ret = unsafe {
            match cmd {
                Command::SetAntennaIndex => {
                    let f: libloading::Symbol<FnSetAntenna> = self
                        .lib
                        .get(b"LMS_SetAntenna\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer.
                    f(self.dev, self.is_tx, self.channel, value as usize)
                }
                Command::SetFrequency => {
                    let f: libloading::Symbol<FnSetLofFrequency> = self
                        .lib
                        .get(b"LMS_SetLOFrequency\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer.
                    f(self.dev, self.is_tx, self.channel, value)
                }
                Command::SetSampleRate => {
                    let f: libloading::Symbol<FnSetSampleRate> = self
                        .lib
                        .get(b"LMS_SetSampleRate\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer; oversample 0
                    // = device default (pyx default).
                    f(self.dev, value, 0)
                }
                Command::SetBandwidth => {
                    let f: libloading::Symbol<FnSetLpfBw> = self
                        .lib
                        .get(b"LMS_SetLPFBW\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer.
                    f(self.dev, self.is_tx, self.channel, value)
                }
                Command::SetRfGain => {
                    let f: libloading::Symbol<FnSetNormalizedGain> = self
                        .lib
                        .get(b"LMS_SetNormalizedGain\0")
                        .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                    // SAFETY: valid symbol, open device pointer.
                    f(self.dev, self.is_tx, self.channel, normalize_gain(value))
                }
                Command::SetChannelIndex => {
                    self.channel = value as usize;
                    return Ok(());
                }
                _ => {
                    return Err(DeviceError::Unsupported(
                        "command not supported by LimeSDR backend",
                    ))
                }
            }
        };
        if ret != 0 {
            return Err(DeviceError::ParameterRejected(format!(
                "LMS error: {}",
                last_error(&self.lib)
            )));
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
        self.channel = config.channel_index as usize;

        // init_device in LimeSDR.py: enable the (rx) channel, then apply the
        // parameters (antenna first!).
        unsafe {
            let f: libloading::Symbol<FnEnableChannel> = self
                .lib
                .get(b"LMS_EnableChannel\0")
                .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
            // SAFETY: valid symbol, open device pointer.
            let ret = f(self.dev, false, self.channel, true);
            if ret != 0 {
                return Err(DeviceError::OpenFailed(format!(
                    "LMS_EnableChannel: {}",
                    last_error(&self.lib)
                )));
            }
        }
        if config.antenna_index >= 0 {
            self.set_parameter(Command::SetAntennaIndex, config.antenna_index as f64)?;
        }
        self.set_parameter(Command::SetFrequency, config.frequency)?;
        self.set_parameter(Command::SetSampleRate, config.sample_rate)?;
        self.set_parameter(Command::SetBandwidth, config.bandwidth)?;
        self.set_parameter(Command::SetRfGain, config.rf_gain)?;

        // prepare_sync_receive: setup + start stream with RECV_FIFO_SIZE.
        self.setup_and_start_stream(RECV_FIFO_SIZE as u32, false)?;

        let chunk = (SYNC_RX_CHUNK_SIZE as f64 * (config.sample_rate / 1e6).max(1.0)) as usize;
        let chunk = chunk.max(SYNC_RX_CHUNK_SIZE);

        let stream_ptr = StreamPtr(
            self.stream
                .as_mut()
                .map(|s| s.as_mut() as *mut LmsStream)
                .expect("stream just created"),
        );
        let running = Arc::new(AtomicBool::new(true));
        let last_error = Arc::clone(&self.last_error);

        // Resolve raw fn pointers so the thread does not borrow self.
        let recv: FnRecvStream = unsafe {
            let f: libloading::Symbol<FnRecvStream> = self
                .lib
                .get(b"LMS_RecvStream\0")
                .map_err(|e| DeviceError::OpenFailed(format!("LMS_RecvStream: {e}")))?;
            // SAFETY: copy of the loaded symbol's fn pointer (fn pointers
            // are Copy); the underlying dlopen handle is kept alive by
            // `self.lib` and the thread is joined before `self` is dropped.
            *f
        };

        let running_thread = Arc::clone(&running);
        let thread = std::thread::Builder::new()
            .name("limesdr-rx".into())
            .spawn(move || {
                let mut buf = vec![0f32; 2 * chunk];
                while running_thread.load(Ordering::SeqCst) {
                    let mut meta = LmsStreamMeta::default();
                    // SAFETY: `stream_ptr` points to the stream owned by the
                    // parent LimeSdrDevice; the thread is joined in stop()/Drop
                    // before the stream or library is released. `buf` holds
                    // `chunk` complex f32 samples; timeout 10 ms per pyx.
                    let n = unsafe {
                        recv(
                            stream_ptr.ptr(),
                            buf.as_mut_ptr() as *mut c_void,
                            chunk,
                            &mut meta,
                            LIME_TIMEOUT_RECEIVE_MS,
                        )
                    };
                    if n > 0 {
                        let chunk_iq: Vec<Complex32> = buf[..2 * n as usize]
                            .chunks_exact(2)
                            .map(|p| Complex32::new(p[0], p[1]))
                            .collect();
                        if sink.send(chunk_iq).is_err() {
                            break; // receiver dropped
                        }
                    } else if n < 0 {
                        *last_error.lock().unwrap() =
                            Some("LMS_RecvStream failed".to_string());
                        break;
                    }
                    // n == 0: timeout, loop again (mirrors recv_stream warning)
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
        self.stop_and_destroy_stream();
        self.is_tx = true;
        self.channel = config.channel_index as usize;

        unsafe {
            let f: libloading::Symbol<FnEnableChannel> = self
                .lib
                .get(b"LMS_EnableChannel\0")
                .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
            // SAFETY: valid symbol, open device pointer.
            let ret = f(self.dev, true, self.channel, true);
            if ret != 0 {
                return Err(DeviceError::OpenFailed(format!(
                    "LMS_EnableChannel: {}",
                    last_error(&self.lib)
                )));
            }
        }
        self.set_parameter(Command::SetFrequency, config.frequency)?;
        self.set_parameter(Command::SetSampleRate, config.sample_rate)?;
        self.set_parameter(Command::SetBandwidth, config.bandwidth)?;
        self.set_parameter(Command::SetRfGain, config.rf_gain)?;

        // prepare_sync_send: SEND_FIFO_SIZE
        self.setup_and_start_stream(SEND_FIFO_SIZE as u32, true)?;

        let mut buf = Vec::with_capacity(2 * samples.len());
        for s in samples {
            buf.push(s.re);
            buf.push(s.im);
        }
        let send_fn: FnSendStream = unsafe {
            let f: libloading::Symbol<FnSendStream> = self
                .lib
                .get(b"LMS_SendStream\0")
                .map_err(|e| DeviceError::OpenFailed(format!("LMS_SendStream: {e}")))?;
            // SAFETY: copy of the loaded symbol's fn pointer (fn pointers
            // are Copy); the signature matches LimeSuite's declaration.
            *f
        };
        let meta = LmsStreamMeta::default();
        let stream_ptr = self
            .stream
            .as_mut()
            .map(|s| s.as_mut() as *mut LmsStream)
            .expect("stream just created");
        // SAFETY: the stream was set up above and is still owned by us; `buf`
        // holds `samples.len()` interleaved f32 samples; timeout 500 ms per
        // LimeSDR.py.
        let ret = unsafe {
            send_fn(
                stream_ptr,
                buf.as_ptr() as *const c_void,
                samples.len(),
                &meta,
                LIME_TIMEOUT_SEND_MS,
            )
        };
        if ret < 0 {
            return Err(DeviceError::StreamError(format!(
                "LMS_SendStream: {}",
                last_error(&self.lib)
            )));
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.join_rx_thread();
        // shutdown_device in LimeSDR.py
        self.stop_and_destroy_stream();
        let _ = self.enable_all_channels(false);
        if !self.dev.is_null() {
            unsafe {
                let close: libloading::Symbol<FnClose> = self
                    .lib
                    .get(b"LMS_Close\0")
                    .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                // SAFETY: valid symbol; `self.dev` came from LMS_Open and is
                // not used afterwards.
                let _ = close(self.dev);
                self.dev = std::ptr::null_mut();
            }
        }
        Ok(())
    }
}

impl Drop for LimeSdrDevice {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn load_library() -> Result<libloading::Library, DeviceError> {
    for name in LIBRARY_NAMES {
        // SAFETY: loading a shared library by name is the intended use of
        // libloading; all further calls go through checked symbols.
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
    use std::time::Duration;

    #[test]
    fn library_names_table_is_wellformed() {
        assert!(LIBRARY_NAMES[0].contains("LimeSuite"));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dylib")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".so")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dll")));
    }

    fn lib_present() -> bool {
        LIBRARY_NAMES.iter().any(|n| crate::devices::native::try_open(n).is_ok())
    }

    #[test]
    fn open_without_library_returns_library_unavailable() {
        if lib_present() {
            return; // lib present: real hardware test territory
        }
        match LimeSdrDevice::open(None) {
            Err(DeviceError::LibraryUnavailable(_)) => {}
            Err(e) => panic!("expected LibraryUnavailable, got {e:?}"),
            Ok(_) => panic!("unexpectedly opened a device"),
        }
    }

    #[test]
    fn device_list_without_library_is_empty_not_panic() {
        assert!(LimeSdrDevice::device_list().is_empty());
    }

    /// URH gain is 0..100 "percent", mapped to [0, 1] normalized gain
    /// (set_device_gain(gain * 0.01) in LimeSDR.py), clamped.
    #[test]
    fn gain_normalization_matches_pyx() {
        assert!((normalize_gain(0.0) - 0.0).abs() < 1e-12);
        assert!((normalize_gain(50.0) - 0.5).abs() < 1e-12);
        assert!((normalize_gain(100.0) - 1.0).abs() < 1e-12);
        // clamping of out-of-range input
        assert!((normalize_gain(150.0) - 1.0).abs() < 1e-12);
        assert!((normalize_gain(-5.0) - 0.0).abs() < 1e-12);
    }

    /// Stream / FIFO constants from LimeSDR.py.
    #[test]
    fn stream_constants_match_pyx() {
        assert_eq!(SYNC_RX_CHUNK_SIZE, 32768);
        assert_eq!(SYNC_TX_CHUNK_SIZE, 32768);
        assert_eq!(RECV_FIFO_SIZE, 1_048_576);
        assert_eq!(SEND_FIFO_SIZE, 8 * 32768);
        assert_eq!(LIME_TIMEOUT_RECEIVE_MS, 10);
        assert_eq!(LIME_TIMEOUT_SEND_MS, 500);
    }

    #[test]
    #[ignore = "requires libLimeSuite and a connected LimeSDR"]
    fn hardware_open_and_rx() {
        let mut dev = LimeSdrDevice::open(None).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        dev.start_rx(
            &DeviceConfig {
                frequency: 433.92e6,
                sample_rate: 1e6,
                bandwidth: 5e6,
                ..Default::default()
            },
            tx,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        dev.stop().unwrap();
        assert!(!rx.recv_timeout(Duration::from_millis(100)).unwrap().is_empty());
    }
}
