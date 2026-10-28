//! SDRPlay (RSP) backend — runtime dlopen binding to libmirsdrapi-rsp.
//!
//! Port of `urh.dev.native.lib.sdrplay` (sdrplay.pyx / csdrplay.pxd) and the
//! `SDRPlay.py` wrapper. RX-only (the API has no TX). Streaming is callback
//! based (`mir_sdr_StreamInit` with a stream callback); the Rust port
//! forwards the interleaved `int16` xi/xq buffers to the sink as normalized
//! `Complex32` (divided by 32768.0).
//!
//! Parameter reconfiguration goes through `mir_sdr_Reinit` with the same
//! reset handshake as sdrplay.pyx: set `reset_rx`, wait for the callback to
//! acknowledge, reinit, clear the flags.

use std::ffi::c_void;
use std::ffi::CStr;
use std::os::raw::{c_char, c_double, c_int, c_uint};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::devices::{Command, Device, DeviceConfig, DeviceError, IqChunk};
use crate::Complex32;

/// Shared-library file names tried (in order) at open time.
pub const LIBRARY_NAMES: &[&str] = &[
    "libmirsdrapi-rsp.3.dylib",
    "libmirsdrapi-rsp.dylib",
    "libmirsdrapi-rsp.so",
    "mirsdrapi-rsp.dll",
];

// ---------------------------------------------------------------------------
// mirsdrapi-rsp C API (subset declared in csdrplay.pxd)
// ---------------------------------------------------------------------------

/// `struct mir_sdr_DeviceT`.
#[repr(C)]
struct MirSdrDevice {
    ser_no: *mut c_char,
    dev_nm: *mut c_char,
    hw_ver: u8,
    dev_avail: u8,
}

/// `mir_sdr_Bw_MHzT` (values are kHz).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirSdrBw {
    Undefined = 0,
    Bw200 = 200,
    Bw300 = 300,
    Bw600 = 600,
    Bw1536 = 1536,
    Bw5000 = 5000,
    Bw6000 = 6000,
    Bw7000 = 7000,
    Bw8000 = 8000,
}

/// `mir_sdr_If_kHzT`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirSdrIf {
    Undefined = -1,
    Zero = 0,
    Khz450 = 450,
    Khz1620 = 1620,
    Khz2048 = 2048,
}

/// `mir_sdr_ErrT` values.
const MIR_SDR_SUCCESS: c_int = 0;

/// `mir_sdr_ReasonForReinitT`.
pub const CHANGE_NONE: c_int = 0x00;
pub const CHANGE_GR: c_int = 0x01;
pub const CHANGE_FS_FREQ: c_int = 0x02;
pub const CHANGE_RF_FREQ: c_int = 0x04;
pub const CHANGE_BW_TYPE: c_int = 0x08;
pub const CHANGE_IF_TYPE: c_int = 0x10;
pub const CHANGE_LO_MODE: c_int = 0x20;
pub const CHANGE_AM_PORT: c_int = 0x40;

/// `mir_sdr_SetGrModeT`.
const MIR_SDR_USE_SET_GR_ALT_MODE: c_int = 1;
const MIR_SDR_USE_RSP_SET_GR: c_int = 2;

/// `mir_sdr_RSPII_AntennaSelectT`.
const RSPII_ANTENNA_A: c_int = 5;
const RSPII_ANTENNA_B: c_int = 6;

/// `mir_sdr_LoModeT`: LO_Undefined = 0.
const LO_UNDEFINED: c_int = 0;

type StreamCallback = unsafe extern "C" fn(
    xi: *mut i16,
    xq: *mut i16,
    first_sample_num: c_uint,
    gr_changed: c_int,
    rf_changed: c_int,
    fs_changed: c_int,
    num_samples: c_uint,
    reset: c_uint,
    hw_removed: c_uint,
    cb_context: *mut c_void,
);
type GainChangeCallback =
    unsafe extern "C" fn(g_rdb: c_uint, lna_g_rdb: c_uint, cb_context: *mut c_void);

type FnApiVersion = unsafe extern "C" fn(*mut f32) -> c_int;
type FnGetDevices =
    unsafe extern "C" fn(*mut MirSdrDevice, *mut c_uint, c_uint) -> c_int;
type FnSetDeviceIdx = unsafe extern "C" fn(c_uint) -> c_int;
type FnReleaseDeviceIdx = unsafe extern "C" fn() -> c_int;
type FnStreamInit = unsafe extern "C" fn(
    *mut c_int, // gRdB
    c_double,   // fsMHz
    c_double,   // rfMHz
    c_int,      // bwType (mir_sdr_Bw_MHzT)
    c_int,      // ifType (mir_sdr_If_kHzT)
    c_int,      // LNAstate
    *mut c_int, // gRdBsystem
    c_int,      // setGrMode
    *mut c_int, // samplesPerPacket
    StreamCallback,
    GainChangeCallback,
    *mut c_void, // cbContext
) -> c_int;
type FnStreamUninit = unsafe extern "C" fn() -> c_int;
type FnReinit = unsafe extern "C" fn(
    *mut c_int, // gRdB
    c_double,   // fsMHz
    c_double,   // rfMHz
    c_int,      // bwType
    c_int,      // ifType
    c_int,      // loMode
    c_int,      // LNAstate
    *mut c_int, // gRdBsystem
    c_int,      // setGrMode
    *mut c_int, // samplesPerPacket
    c_int,      // reasonForReinit
) -> c_int;
type FnAmPortSelect = unsafe extern "C" fn(c_int) -> c_int;
type FnRspiiAntennaControl = unsafe extern "C" fn(c_int) -> c_int;

/// `mir_sdr_ErrT` names (SDRPlay.py error_codes).
fn error_string(code: c_int) -> String {
    match code {
        0 => "SUCCESS".into(),
        1 => "FAIL".into(),
        2 => "INVALID PARAMETER".into(),
        3 => "OUT OF RANGE".into(),
        4 => "GAIN UPDATE ERROR".into(),
        5 => "RF UPDATE ERROR".into(),
        6 => "FS UPDATE ERROR".into(),
        7 => "HARDWARE ERROR".into(),
        8 => "ALIASING ERROR".into(),
        9 => "ALREADY INITIALIZED".into(),
        10 => "NOT INITIALIZED".into(),
        11 => "NOT ENABLED".into(),
        12 => "HARDWARE VERSION ERROR".into(),
        13 => "OUT OF MEMORY ERROR".into(),
        other => format!("mir_sdr error {other}"),
    }
}

// ---------------------------------------------------------------------------
// parameter tables from sdrplay.pyx
// ---------------------------------------------------------------------------

/// Bandwidths understood by `get_nearest_bandwidth` (Hz).
pub const BANDWIDTHS: &[f64] = &[
    200e3, 300e3, 600e3, 1536e3, 5000e3, 6000e3, 7000e3, 8000e3,
];

/// IF values understood by `get_nearest_if_gain` (kHz).
pub const IF_GAINS: &[f64] = &[0.0, 450.0, 1620.0, 2048.0];

/// Port of `get_nearest_bandwidth`: pick the tabulated bandwidth closest to
/// the requested one and return its `mir_sdr_Bw_MHzT` value.
pub fn nearest_bandwidth(bandwidth: f64) -> MirSdrBw {
    let mut best = BANDWIDTHS[0];
    for &bw in BANDWIDTHS.iter().skip(1) {
        if (bw - bandwidth).abs() < (best - bandwidth).abs() {
            best = bw;
        }
    }
    match best as i64 {
        200_000 => MirSdrBw::Bw200,
        300_000 => MirSdrBw::Bw300,
        600_000 => MirSdrBw::Bw600,
        1_536_000 => MirSdrBw::Bw1536,
        5_000_000 => MirSdrBw::Bw5000,
        6_000_000 => MirSdrBw::Bw6000,
        7_000_000 => MirSdrBw::Bw7000,
        8_000_000 => MirSdrBw::Bw8000,
        _ => MirSdrBw::Bw200,
    }
}

/// Port of `get_nearest_if_gain`: pick the tabulated IF closest to the
/// requested one and return its `mir_sdr_If_kHzT` value.
pub fn nearest_if_gain(if_gain: f64) -> MirSdrIf {
    let mut best = IF_GAINS[0];
    for &g in IF_GAINS.iter().skip(1) {
        if (g - if_gain).abs() < (best - if_gain).abs() {
            best = g;
        }
    }
    match best as i32 {
        0 => MirSdrIf::Zero,
        450 => MirSdrIf::Khz450,
        1620 => MirSdrIf::Khz1620,
        2048 => MirSdrIf::Khz2048,
        _ => MirSdrIf::Zero,
    }
}

/// Port of `calculate_gain_reduction`: URH's gain is clamped to [20, 59] dB
/// and inverted into a gain reduction for the API (highest gain = lowest
/// gain reduction, offset 79).
pub fn gain_reduction(gain: f64) -> c_int {
    79 - gain.clamp(20.0, 59.0) as c_int
}

// ---------------------------------------------------------------------------
// callback state
// ---------------------------------------------------------------------------

/// State shared with the C stream callback. Boxed and passed as `cbContext`
/// while the stream is alive; reclaimed on stop.
struct CbState {
    sink: std::sync::mpsc::Sender<IqChunk>,
    /// reinit handshake flags (reset_rx / reset_rx_request_received)
    reset_rx: AtomicBool,
    reset_rx_request_received: AtomicBool,
}

/// Port of `_rx_stream_callback` (10-arg variant of the API 3 callback).
unsafe extern "C" fn rx_stream_callback(
    xi: *mut i16,
    xq: *mut i16,
    _first_sample_num: c_uint,
    _gr_changed: c_int,
    _rf_changed: c_int,
    _fs_changed: c_int,
    num_samples: c_uint,
    _reset: c_uint,
    _hw_removed: c_uint,
    cb_context: *mut c_void,
) {
    // SAFETY: `cb_context` is the Box<CbState> pointer we passed to
    // mir_sdr_StreamInit; the box lives until StreamUninit returns and the
    // stream callbacks have stopped. This function must not unwind across
    // the FFI boundary: it contains no panicking operations (Vec allocation
    // may abort under OOM, which is the panic=abort behavior we accept).
    let state = &*(cb_context as *const CbState);
    if state.reset_rx.load(Ordering::SeqCst) {
        state.reset_rx_request_received.store(true, Ordering::SeqCst);
        return;
    }
    let n = num_samples as usize;
    let mut chunk = Vec::with_capacity(n);
    // SAFETY: `xi`/`xq` point to `num_samples` valid i16 values owned by the
    // API for the duration of the callback; we copy them out immediately.
    for i in 0..n {
        let re = *xi.add(i) as f32 / 32768.0;
        let im = *xq.add(i) as f32 / 32768.0;
        chunk.push(Complex32::new(re, im));
    }
    // mpsc send never panics; a dropped receiver is not an error here.
    let _ = state.sink.send(chunk);
}

/// Port of `_gain_change_callback` (no-op in sdrplay.pyx).
unsafe extern "C" fn gain_change_callback(
    _g_rdb: c_uint,
    _lna_g_rdb: c_uint,
    _cb_context: *mut c_void,
) {
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// An SDRPlay RSP receiver opened through libmirsdrapi-rsp (RX only).
pub struct SdrPlayDevice {
    lib: libloading::Library,
    /// gr mode depends on the device model (set_gr_mode_for_dev_model):
    /// RSP1 (hwVer 1) uses USE_SET_GR_ALT_MODE, others USE_RSP_SET_GR.
    set_gr_mode: c_int,
    device_index: Option<c_uint>,
    /// callback context while the stream is alive
    cb_state: Option<Box<CbState>>,
}

impl SdrPlayDevice {
    /// dlopen libmirsdrapi-rsp. `identifier` is the device number (index
    /// into the device list) as a decimal string; when given, the device
    /// index is set immediately (port of `set_device_index`).
    pub fn open(identifier: Option<&str>) -> Result<Self, DeviceError> {
        let lib = load_library()?;
        let mut device = Self {
            lib,
            set_gr_mode: MIR_SDR_USE_RSP_SET_GR,
            device_index: None,
            cb_state: None,
        };
        if let Some(id) = identifier {
            let index: c_uint = id
                .trim()
                .parse()
                .map_err(|_| DeviceError::OpenFailed(format!("invalid SDRPlay device number {id:?}")))?;
            device.set_device_index(index)?;
            device.device_index = Some(index);
        }
        Ok(device)
    }

    /// `mir_sdr_ApiVersion` (port of `get_api_version`).
    pub fn api_version(&self) -> Result<f32, DeviceError> {
        unsafe {
            let f: libloading::Symbol<FnApiVersion> = self
                .lib
                .get(b"mir_sdr_ApiVersion\0")
                .map_err(|e| DeviceError::OpenFailed(format!("mir_sdr_ApiVersion: {e}")))?;
            let mut version = 0.0f32;
            // SAFETY: valid symbol; `version` is a valid out-pointer.
            let ret = f(&mut version);
            if ret != MIR_SDR_SUCCESS {
                return Err(DeviceError::OpenFailed(error_string(ret)));
            }
            Ok(version)
        }
    }

    fn set_device_index(&mut self, index: c_uint) -> Result<(), DeviceError> {
        unsafe {
            let f: libloading::Symbol<FnSetDeviceIdx> = self
                .lib
                .get(b"mir_sdr_SetDeviceIdx\0")
                .map_err(|e| DeviceError::OpenFailed(format!("mir_sdr_SetDeviceIdx: {e}")))?;
            // SAFETY: valid symbol; plain integer argument.
            let ret = f(index);
            if ret != MIR_SDR_SUCCESS {
                return Err(DeviceError::OpenFailed(format!(
                    "mir_sdr_SetDeviceIdx: {}",
                    error_string(ret)
                )));
            }
        }
        // gr mode follows the hardware version of the selected device
        // (init_device in SDRPlay.py)
        let devices = Self::raw_devices(&self.lib);
        if let Some(d) = devices.get(index as usize) {
            self.set_gr_mode = if d.hw_version == 1 {
                MIR_SDR_USE_SET_GR_ALT_MODE
            } else {
                MIR_SDR_USE_RSP_SET_GR
            };
        }
        Ok(())
    }

    fn raw_devices(lib: &libloading::Library) -> Vec<MirSdrDeviceFields> {
        unsafe {
            let f: libloading::Symbol<FnGetDevices> = match lib.get(b"mir_sdr_GetDevices\0") {
                Ok(f) => f,
                Err(_) => return vec![],
            };
            // sdrplay.pyx allocates 256 device slots
            let mut devs: Vec<MirSdrDevice> = Vec::with_capacity(256);
            let mut num_devs: c_uint = 0;
            // SAFETY: valid symbol; `devs` provides 256 uninitialized
            // mir_sdr_DeviceT slots, and we only read the first `num_devs`
            // entries which the API fully initializes.
            let ret = f(
                devs.as_mut_ptr(),
                &mut num_devs,
                256,
            );
            if ret != MIR_SDR_SUCCESS {
                return vec![];
            }
            devs.set_len(num_devs as usize);
            devs.into_iter()
                .map(|d| MirSdrDeviceFields {
                    serial: if d.ser_no.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(d.ser_no).to_string_lossy().into_owned()
                    },
                    device_ref: if d.dev_nm.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(d.dev_nm).to_string_lossy().into_owned()
                    },
                    hw_version: d.hw_ver,
                    available: d.dev_avail != 0,
                })
                .collect()
        }
    }

    /// Port of `reinit_stream` including the reset handshake.
    fn reinit(
        &mut self,
        reason: c_int,
        sample_rate: f64,
        frequency: f64,
        bw_type: MirSdrBw,
        if_type: MirSdrIf,
        gain: c_int,
    ) -> Result<(), DeviceError> {
        let state = self
            .cb_state
            .as_deref()
            .ok_or_else(|| DeviceError::StreamError("stream not initialized".into()))?;
        state.reset_rx.store(true, Ordering::SeqCst);
        // wait for the callback to acknowledge (reset_rx_request_received)
        let mut waited = 0u32;
        while !state.reset_rx_request_received.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(10));
            waited += 1;
            if waited > 200 {
                // no callback within 2 s: proceed anyway like a timeout
                break;
            }
        }
        let ret = unsafe {
            let f: libloading::Symbol<FnReinit> = self
                .lib
                .get(b"mir_sdr_Reinit\0")
                .map_err(|e| DeviceError::OpenFailed(format!("mir_sdr_Reinit: {e}")))?;
            let mut g_rdb = gain;
            let mut g_rdb_system: c_int = 0;
            let mut samples_per_packet: c_int = 0;
            // SAFETY: valid symbol; `g_rdb` etc. are valid in/out pointers.
            // bw/if/lo "undefined" sentinel values mirror the pyx defaults.
            let bw = match bw_type {
                MirSdrBw::Undefined => 0,
                other => other as c_int,
            };
            let ift = if_type as c_int;
            f(
                &mut g_rdb,
                sample_rate / 1e6,
                frequency / 1e6,
                bw,
                ift,
                LO_UNDEFINED,
                0, // lna_state
                &mut g_rdb_system,
                self.set_gr_mode,
                &mut samples_per_packet,
                reason,
            )
        };
        state.reset_rx.store(false, Ordering::SeqCst);
        state.reset_rx_request_received.store(false, Ordering::SeqCst);
        if ret != MIR_SDR_SUCCESS {
            return Err(DeviceError::ParameterRejected(error_string(ret)));
        }
        Ok(())
    }
}

/// Plain-data view of `mir_sdr_DeviceT` (port of `get_devices` dicts).
#[derive(Debug, Clone)]
pub struct MirSdrDeviceFields {
    pub serial: String,
    pub device_ref: String,
    pub hw_version: u8,
    pub available: bool,
}

impl Device for SdrPlayDevice {
    fn name(&self) -> &'static str {
        "SDRPlay"
    }

    /// "RSP {hw_version} ({serial})" entries (SDRPlay.get_device_list).
    fn device_list() -> Vec<String> {
        let lib = match load_library() {
            Ok(l) => l,
            Err(_) => return vec![],
        };
        Self::raw_devices(&lib)
            .into_iter()
            .map(|d| format!("RSP {} ({})", d.hw_version, d.serial))
            .collect()
    }

    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        match cmd {
            Command::SetFrequency => {
                self.reinit(CHANGE_RF_FREQ, 0.0, value, MirSdrBw::Undefined, MirSdrIf::Undefined, 0)
            }
            Command::SetSampleRate => {
                self.reinit(CHANGE_FS_FREQ, value, 0.0, MirSdrBw::Undefined, MirSdrIf::Undefined, 0)
            }
            Command::SetBandwidth => {
                self.reinit(CHANGE_BW_TYPE, 0.0, 0.0, nearest_bandwidth(value), MirSdrIf::Undefined, 0)
            }
            Command::SetRfGain => {
                self.reinit(CHANGE_GR, 0.0, 0.0, MirSdrBw::Undefined, MirSdrIf::Undefined, gain_reduction(value))
            }
            Command::SetIfGain => {
                self.reinit(CHANGE_IF_TYPE, 0.0, 0.0, MirSdrBw::Undefined, nearest_if_gain(value), 0)
            }
            Command::SetAntennaIndex => {
                // port of set_antenna (RSP2+ only)
                let antenna = value as i32;
                unsafe {
                    if antenna == 0 || antenna == 1 {
                        let am: libloading::Symbol<FnAmPortSelect> = self
                            .lib
                            .get(b"mir_sdr_AmPortSelect\0")
                            .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                        // SAFETY: valid symbol; plain integer argument.
                        let ret = am(0);
                        if ret != MIR_SDR_SUCCESS {
                            return Err(DeviceError::ParameterRejected(error_string(ret)));
                        }
                        let ctrl: libloading::Symbol<FnRspiiAntennaControl> = self
                            .lib
                            .get(b"mir_sdr_RSPII_AntennaControl\0")
                            .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                        // SAFETY: valid symbol; plain enum argument.
                        let ret = ctrl(if antenna == 0 { RSPII_ANTENNA_A } else { RSPII_ANTENNA_B });
                        if ret != MIR_SDR_SUCCESS {
                            return Err(DeviceError::ParameterRejected(error_string(ret)));
                        }
                        Ok(())
                    } else if antenna == 2 {
                        let am: libloading::Symbol<FnAmPortSelect> = self
                            .lib
                            .get(b"mir_sdr_AmPortSelect\0")
                            .map_err(|e| DeviceError::OpenFailed(e.to_string()))?;
                        // SAFETY: valid symbol; HiZ port = 1.
                        let ret = am(1);
                        if ret != MIR_SDR_SUCCESS {
                            return Err(DeviceError::ParameterRejected(error_string(ret)));
                        }
                        Ok(())
                    } else {
                        Err(DeviceError::ParameterRejected(format!(
                            "antenna index {antenna} not in 0..=2"
                        )))
                    }
                }
            }
            _ => Err(DeviceError::Unsupported(
                "command not supported by SDRPlay backend",
            )),
        }
    }

    fn start_rx(
        &mut self,
        config: &DeviceConfig,
        sink: std::sync::mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        if self.cb_state.is_some() {
            return Err(DeviceError::StreamError("stream already initialized".into()));
        }
        if self.device_index.is_none() {
            // SDRPlay needs an explicit device index for multi-device setups;
            // default to the first device like a bare `mir_sdr_StreamInit`.
            self.set_device_index(0)?;
            self.device_index = Some(0);
        }
        // init_stream in sdrplay.pyx
        let mut g_rdb = gain_reduction(config.rf_gain);
        let mut g_rdb_system: c_int = 0;
        let mut samples_per_packet: c_int = 0;
        let state = Box::new(CbState {
            sink,
            reset_rx: AtomicBool::new(false),
            reset_rx_request_received: AtomicBool::new(false),
        });
        let ctx = &*state as *const CbState as *mut c_void;
        let ret = unsafe {
            let f: libloading::Symbol<FnStreamInit> = self
                .lib
                .get(b"mir_sdr_StreamInit\0")
                .map_err(|e| DeviceError::OpenFailed(format!("mir_sdr_StreamInit: {e}")))?;
            // SAFETY: valid symbol; `g_rdb`/`g_rdb_system`/`samples_per_packet`
            // are valid in/out pointers, `ctx` points to the Box<CbState> that
            // we store in self and keep alive until after StreamUninit. The
            // lna_state is 0 and the bw/if types come from the pyx nearest-
            // value tables.
            f(
                &mut g_rdb,
                config.sample_rate / 1e6,
                config.frequency / 1e6,
                nearest_bandwidth(config.bandwidth) as c_int,
                nearest_if_gain(config.if_gain) as c_int,
                0, // lna_state
                &mut g_rdb_system,
                self.set_gr_mode,
                &mut samples_per_packet,
                rx_stream_callback,
                gain_change_callback,
                ctx,
            )
        };
        if ret != MIR_SDR_SUCCESS {
            return Err(DeviceError::StreamError(error_string(ret)));
        }
        self.cb_state = Some(state);
        Ok(())
    }

    fn send(&mut self, _config: &DeviceConfig, _samples: &[Complex32]) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported("SDRPlay is receive-only"))
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        // shutdown_device in SDRPlay.py: close stream, release device.
        if self.cb_state.is_some() {
            let ret = unsafe {
                let f: libloading::Symbol<FnStreamUninit> = self
                    .lib
                    .get(b"mir_sdr_StreamUninit\0")
                    .map_err(|e| DeviceError::OpenFailed(format!("mir_sdr_StreamUninit: {e}")))?;
                // SAFETY: valid symbol; no arguments.
                f()
            };
            if ret != MIR_SDR_SUCCESS {
                return Err(DeviceError::StreamError(error_string(ret)));
            }
            // reclaim the callback context; safe because StreamUninit has
            // returned, i.e. callbacks have stopped.
            self.cb_state = None;
        }
        if let Some(index) = self.device_index.take() {
            let _ = index;
            unsafe {
                if let Ok(f) = self.lib.get::<FnReleaseDeviceIdx>(b"mir_sdr_ReleaseDeviceIdx\0") {
                    // SAFETY: valid symbol; no arguments.
                    let _ = f();
                }
            }
        }
        Ok(())
    }
}

impl Drop for SdrPlayDevice {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn load_library() -> Result<libloading::Library, DeviceError> {
    for name in LIBRARY_NAMES {
        // SAFETY: loading a shared library by name is the intended use of
        // libloading; all further calls go through checked symbols.
        if let Ok(lib) = unsafe { libloading::Library::new(name) } {
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
        assert!(LIBRARY_NAMES.iter().all(|n| n.contains("mirsdrapi-rsp")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dylib")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".so")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dll")));
    }

    fn lib_present() -> bool {
        LIBRARY_NAMES.iter().any(|n| unsafe {
            // SAFETY: probe dlopen of a library name, immediately dropped.
            libloading::Library::new(n).is_ok()
        })
    }

    #[test]
    fn open_without_library_returns_library_unavailable() {
        if lib_present() {
            return; // lib present: real hardware test territory
        }
        match SdrPlayDevice::open(None) {
            Err(DeviceError::LibraryUnavailable(_)) => {}
            Err(e) => panic!("expected LibraryUnavailable, got {e:?}"),
            Ok(_) => panic!("unexpectedly opened a device"),
        }
        match SdrPlayDevice::open(Some("not-a-number")) {
            Err(DeviceError::LibraryUnavailable(_)) => {}
            Err(e) => panic!("expected LibraryUnavailable, got {e:?}"),
            Ok(_) => panic!("unexpectedly opened a device"),
        }
    }

    #[test]
    fn device_list_without_library_is_empty_not_panic() {
        assert!(SdrPlayDevice::device_list().is_empty());
    }

    /// get_nearest_bandwidth table from sdrplay.pyx (incl. 7/8 MHz).
    #[test]
    fn nearest_bandwidth_matches_pyx() {
        assert_eq!(nearest_bandwidth(200e3), MirSdrBw::Bw200);
        assert_eq!(nearest_bandwidth(250e3), MirSdrBw::Bw200); // 200 vs 300 equidistant -> keeps first (200)
        assert_eq!(nearest_bandwidth(1e6), MirSdrBw::Bw600);
        assert_eq!(nearest_bandwidth(2e6), MirSdrBw::Bw1536);
        assert_eq!(nearest_bandwidth(4e6), MirSdrBw::Bw5000);
        assert_eq!(nearest_bandwidth(6.1e6), MirSdrBw::Bw6000);
        assert_eq!(nearest_bandwidth(7.4e6), MirSdrBw::Bw7000);
        assert_eq!(nearest_bandwidth(100e9), MirSdrBw::Bw8000);
        // below the table the first entry wins (pyx best_match starts at 0)
        assert_eq!(nearest_bandwidth(1e3), MirSdrBw::Bw200);
    }

    /// get_nearest_if_gain table from sdrplay.pyx.
    #[test]
    fn nearest_if_gain_matches_pyx() {
        assert_eq!(nearest_if_gain(0.0), MirSdrIf::Zero);
        assert_eq!(nearest_if_gain(100.0), MirSdrIf::Zero); // closer to 0 than 450
        assert_eq!(nearest_if_gain(500.0), MirSdrIf::Khz450);
        assert_eq!(nearest_if_gain(1600.0), MirSdrIf::Khz1620);
        assert_eq!(nearest_if_gain(2049.0), MirSdrIf::Khz2048);
        assert_eq!(nearest_if_gain(1e9), MirSdrIf::Khz2048);
    }

    /// calculate_gain_reduction: clamp to [20, 59], then 79 - gain.
    #[test]
    fn gain_reduction_matches_pyx() {
        assert_eq!(gain_reduction(20.0), 59);
        assert_eq!(gain_reduction(59.0), 20);
        assert_eq!(gain_reduction(40.0), 39);
        // clamping outside the supported range
        assert_eq!(gain_reduction(0.0), 59);
        assert_eq!(gain_reduction(80.0), 20);
        assert_eq!(gain_reduction(-5.0), 59);
    }

    /// mir_sdr error code names from SDRPlay.py.
    #[test]
    fn error_string_matches_pyx_table() {
        assert_eq!(error_string(0), "SUCCESS");
        assert_eq!(error_string(3), "OUT OF RANGE");
        assert_eq!(error_string(9), "ALREADY INITIALIZED");
        assert_eq!(error_string(13), "OUT OF MEMORY ERROR");
    }

    #[test]
    #[ignore = "requires libmirsdrapi-rsp and a connected RSP"]
    fn hardware_open_and_rx() {
        let mut dev = SdrPlayDevice::open(Some("0")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        dev.start_rx(
            &DeviceConfig {
                frequency: 433.92e6,
                sample_rate: 2e6,
                bandwidth: 1536e3,
                rf_gain: 50.0,
                if_gain: 0.0,
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
