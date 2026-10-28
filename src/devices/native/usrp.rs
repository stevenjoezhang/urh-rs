//! USRP backend — runtime binding of **libuhd** (port of
//! `urh.dev.native.lib.usrp` / `USRP.py`).
//!
//! The library is dlopened at runtime and every C symbol is resolved via
//! `dlsym` when the device is opened; on machines without libuhd
//! [`UsrpDevice::open`] returns [`DeviceError::LibraryUnavailable`] and the
//! crate still compiles/links (no link-time dependency on libuhd).
//!
//! C API declared below mirrors `cusrp.pxd` (uhd C API).

use std::ffi::{c_char, c_double, c_int, c_long, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use libloading::{Library, Symbol};

use crate::devices::{Command, Device, DeviceConfig, DeviceError, IqChunk};

/// Library file names tried (in order) when dlopening libuhd.
pub const LIBRARY_NAMES: &[&str] = &[
    "libuhd.410.dylib",
    "libuhd.dylib",
    "libuhd.so.40.0.0",
    "libuhd.so",
    "uhd.dll",
];

/// SYNC_RX_CHUNK_SIZE (scaled with sample rate in
/// `adapt_num_read_samples_to_sample_rate`).
const SYNC_RX_CHUNK_SIZE: usize = 16384;

/// uhd_error values (subset of cusrp.pxd used here).
const UHD_ERROR_NONE: c_int = 0;
/// Marker used by USRP.py for "antenna index not supported" (4711).
const ANTENNA_INDEX_ERROR: c_int = 4711;

// ---------------------------------------------------------------------------
// C API types (uhd) — handles are opaque pointers in the C API.
// ---------------------------------------------------------------------------

#[repr(C)]
struct UhdUsrp(c_void);
#[repr(C)]
struct UhdRxStreamer(c_void);
#[repr(C)]
struct UhdTxStreamer(c_void);
#[repr(C)]
struct UhdRxMetadata(c_void);
#[repr(C)]
struct UhdTxMetadata(c_void);
#[repr(C)]
struct UhdStringVector(c_void);
#[repr(C)]
struct UhdSubdevSpec(c_void);

type UhdUsrpHandle = *mut UhdUsrp;
type UhdRxStreamerHandle = *mut UhdRxStreamer;
type UhdTxStreamerHandle = *mut UhdTxStreamer;
type UhdRxMetadataHandle = *mut UhdRxMetadata;
type UhdTxMetadataHandle = *mut UhdTxMetadata;
type UhdStringVectorHandle = *mut UhdStringVector;
type UhdSubdevSpecHandle = *mut UhdSubdevSpec;
type UhdError = c_int;

/// uhd_tune_request_policy_t
const UHD_TUNE_REQUEST_POLICY_AUTO: c_int = 65;

#[repr(C)]
struct UhdTuneRequest {
    target_freq: c_double,
    rf_freq_policy: c_int,
    rf_freq: c_double,
    dsp_freq_policy: c_int,
    dsp_freq: c_double,
    args: *const c_char,
}

#[repr(C)]
struct UhdTuneResult {
    clipped_rf_freq: c_double,
    target_rf_freq: c_double,
    actual_rf_freq: c_double,
    target_dsp_freq: c_double,
    actual_dsp_freq: c_double,
}

/// uhd_stream_mode_t
const UHD_STREAM_MODE_START_CONTINUOUS: c_int = 97;
const UHD_STREAM_MODE_STOP_CONTINUOUS: c_int = 111;

#[repr(C)]
struct UhdStreamCmd {
    stream_mode: c_int,
    num_samps: usize,
    stream_now: bool,
    time_spec_full_secs: c_long,
    time_spec_frac_secs: c_double,
}

#[repr(C)]
struct UhdStreamArgs {
    cpu_format: *const c_char,
    otw_format: *const c_char,
    args: *const c_char,
    channel_list: *mut usize,
    n_channels: c_int,
}

/// The libuhd function subset URH uses (`cusrp.pxd`), resolved at runtime.
#[allow(clippy::type_complexity)]
struct UhdApi {
    usrp_find: unsafe extern "C" fn(*const c_char, *mut UhdStringVectorHandle) -> UhdError,
    usrp_make: unsafe extern "C" fn(*mut UhdUsrpHandle, *const c_char) -> UhdError,
    usrp_free: unsafe extern "C" fn(*mut UhdUsrpHandle) -> UhdError,
    usrp_last_error:
        unsafe extern "C" fn(UhdUsrpHandle, *mut c_char, usize) -> UhdError,
    usrp_get_pp_string:
        unsafe extern "C" fn(UhdUsrpHandle, *mut c_char, usize) -> UhdError,

    usrp_get_rx_stream:
        unsafe extern "C" fn(UhdUsrpHandle, *mut UhdStreamArgs, UhdRxStreamerHandle) -> UhdError,
    usrp_get_tx_stream:
        unsafe extern "C" fn(UhdUsrpHandle, *mut UhdStreamArgs, UhdTxStreamerHandle) -> UhdError,

    rx_streamer_make: unsafe extern "C" fn(*mut UhdRxStreamerHandle) -> UhdError,
    tx_streamer_make: unsafe extern "C" fn(*mut UhdTxStreamerHandle) -> UhdError,
    rx_streamer_free: unsafe extern "C" fn(*mut UhdRxStreamerHandle) -> UhdError,
    tx_streamer_free: unsafe extern "C" fn(*mut UhdTxStreamerHandle) -> UhdError,
    rx_streamer_num_channels:
        unsafe extern "C" fn(UhdRxStreamerHandle, *mut usize) -> UhdError,
    tx_streamer_num_channels:
        unsafe extern "C" fn(UhdTxStreamerHandle, *mut usize) -> UhdError,
    rx_streamer_max_num_samps:
        unsafe extern "C" fn(UhdRxStreamerHandle, *mut usize) -> UhdError,
    tx_streamer_max_num_samps:
        unsafe extern "C" fn(UhdTxStreamerHandle, *mut usize) -> UhdError,
    rx_streamer_recv: unsafe extern "C" fn(
        UhdRxStreamerHandle,
        *mut *mut c_void,
        usize,
        *mut UhdRxMetadataHandle,
        c_double,
        bool,
        *mut usize,
    ) -> UhdError,
    tx_streamer_send: unsafe extern "C" fn(
        UhdTxStreamerHandle,
        *const *const c_void,
        usize,
        *mut UhdTxMetadataHandle,
        c_double,
        *mut usize,
    ) -> UhdError,
    rx_streamer_issue_stream_cmd:
        unsafe extern "C" fn(UhdRxStreamerHandle, *const UhdStreamCmd) -> UhdError,

    rx_metadata_make: unsafe extern "C" fn(*mut UhdRxMetadataHandle) -> UhdError,
    tx_metadata_make: unsafe extern "C" fn(
        *mut UhdTxMetadataHandle,
        bool,
        c_long,
        c_double,
        bool,
        bool,
    ) -> UhdError,
    rx_metadata_free: unsafe extern "C" fn(*mut UhdRxMetadataHandle) -> UhdError,
    tx_metadata_free: unsafe extern "C" fn(*mut UhdTxMetadataHandle) -> UhdError,

    usrp_set_rx_rate: unsafe extern "C" fn(UhdUsrpHandle, c_double, usize) -> UhdError,
    usrp_set_tx_rate: unsafe extern "C" fn(UhdUsrpHandle, c_double, usize) -> UhdError,
    usrp_set_rx_freq:
        unsafe extern "C" fn(UhdUsrpHandle, *mut UhdTuneRequest, usize, *mut UhdTuneResult) -> UhdError,
    usrp_set_tx_freq:
        unsafe extern "C" fn(UhdUsrpHandle, *mut UhdTuneRequest, usize, *mut UhdTuneResult) -> UhdError,
    usrp_set_normalized_rx_gain:
        unsafe extern "C" fn(UhdUsrpHandle, c_double, usize) -> UhdError,
    usrp_set_normalized_tx_gain:
        unsafe extern "C" fn(UhdUsrpHandle, c_double, usize) -> UhdError,
    usrp_set_rx_bandwidth: unsafe extern "C" fn(UhdUsrpHandle, c_double, usize) -> UhdError,
    usrp_set_tx_bandwidth: unsafe extern "C" fn(UhdUsrpHandle, c_double, usize) -> UhdError,

    usrp_set_rx_antenna: unsafe extern "C" fn(UhdUsrpHandle, *const c_char, usize) -> UhdError,
    usrp_get_rx_antenna:
        unsafe extern "C" fn(UhdUsrpHandle, usize, *mut c_char, usize) -> UhdError,
    usrp_get_rx_antennas:
        unsafe extern "C" fn(UhdUsrpHandle, usize, *mut UhdStringVectorHandle) -> UhdError,
    usrp_set_tx_antenna: unsafe extern "C" fn(UhdUsrpHandle, *const c_char, usize) -> UhdError,
    usrp_get_tx_antenna:
        unsafe extern "C" fn(UhdUsrpHandle, usize, *mut c_char, usize) -> UhdError,
    usrp_get_tx_antennas:
        unsafe extern "C" fn(UhdUsrpHandle, usize, *mut UhdStringVectorHandle) -> UhdError,

    string_vector_make: unsafe extern "C" fn(*mut UhdStringVectorHandle) -> UhdError,
    string_vector_free: unsafe extern "C" fn(*mut UhdStringVectorHandle) -> UhdError,
    string_vector_size: unsafe extern "C" fn(UhdStringVectorHandle, *mut usize) -> UhdError,
    string_vector_at:
        unsafe extern "C" fn(UhdStringVectorHandle, usize, *mut c_char, usize) -> UhdError,

    subdev_spec_make: unsafe extern "C" fn(*mut UhdSubdevSpecHandle, *const c_char) -> UhdError,
    usrp_set_rx_subdev_spec:
        unsafe extern "C" fn(UhdUsrpHandle, UhdSubdevSpecHandle, usize) -> UhdError,
    usrp_set_tx_subdev_spec:
        unsafe extern "C" fn(UhdUsrpHandle, UhdSubdevSpecHandle, usize) -> UhdError,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn load_library() -> Result<Library, DeviceError> {
    let mut last = String::new();
    for name in LIBRARY_NAMES {
        // SAFETY: compile-time constant names; plain dlopen probe.
        match unsafe { Library::new(name) } {
            Ok(lib) => return Ok(lib),
            Err(e) => last = format!("{name}: {e}"),
        }
    }
    Err(DeviceError::LibraryUnavailable(format!("libuhd ({last})")))
}

fn sym<'a, T>(lib: &'a Library, name: &[u8]) -> Result<Symbol<'a, T>, DeviceError> {
    // SAFETY: generic dlsym lookup; the caller calls the obtained function
    // with a signature matching the uhd C header.
    unsafe { lib.get::<T>(name) }.map_err(|e| DeviceError::LibraryUnavailable(e.to_string()))
}

#[allow(clippy::type_complexity)]
fn resolve_api(lib: &Library) -> Result<UhdApi, DeviceError> {
    Ok(UhdApi {
        usrp_find: *sym(lib, b"uhd_usrp_find\0")?,
        usrp_make: *sym(lib, b"uhd_usrp_make\0")?,
        usrp_free: *sym(lib, b"uhd_usrp_free\0")?,
        usrp_last_error: *sym(lib, b"uhd_usrp_last_error\0")?,
        usrp_get_pp_string: *sym(lib, b"uhd_usrp_get_pp_string\0")?,
        usrp_get_rx_stream: *sym(lib, b"uhd_usrp_get_rx_stream\0")?,
        usrp_get_tx_stream: *sym(lib, b"uhd_usrp_get_tx_stream\0")?,
        rx_streamer_make: *sym(lib, b"uhd_rx_streamer_make\0")?,
        tx_streamer_make: *sym(lib, b"uhd_tx_streamer_make\0")?,
        rx_streamer_free: *sym(lib, b"uhd_rx_streamer_free\0")?,
        tx_streamer_free: *sym(lib, b"uhd_tx_streamer_free\0")?,
        rx_streamer_num_channels: *sym(lib, b"uhd_rx_streamer_num_channels\0")?,
        tx_streamer_num_channels: *sym(lib, b"uhd_tx_streamer_num_channels\0")?,
        rx_streamer_max_num_samps: *sym(lib, b"uhd_rx_streamer_max_num_samps\0")?,
        tx_streamer_max_num_samps: *sym(lib, b"uhd_tx_streamer_max_num_samps\0")?,
        rx_streamer_recv: *sym(lib, b"uhd_rx_streamer_recv\0")?,
        tx_streamer_send: *sym(lib, b"uhd_tx_streamer_send\0")?,
        rx_streamer_issue_stream_cmd: *sym(lib, b"uhd_rx_streamer_issue_stream_cmd\0")?,
        rx_metadata_make: *sym(lib, b"uhd_rx_metadata_make\0")?,
        tx_metadata_make: *sym(lib, b"uhd_tx_metadata_make\0")?,
        rx_metadata_free: *sym(lib, b"uhd_rx_metadata_free\0")?,
        tx_metadata_free: *sym(lib, b"uhd_tx_metadata_free\0")?,
        usrp_set_rx_rate: *sym(lib, b"uhd_usrp_set_rx_rate\0")?,
        usrp_set_tx_rate: *sym(lib, b"uhd_usrp_set_tx_rate\0")?,
        usrp_set_rx_freq: *sym(lib, b"uhd_usrp_set_rx_freq\0")?,
        usrp_set_tx_freq: *sym(lib, b"uhd_usrp_set_tx_freq\0")?,
        usrp_set_normalized_rx_gain: *sym(lib, b"uhd_usrp_set_normalized_rx_gain\0")?,
        usrp_set_normalized_tx_gain: *sym(lib, b"uhd_usrp_set_normalized_tx_gain\0")?,
        usrp_set_rx_bandwidth: *sym(lib, b"uhd_usrp_set_rx_bandwidth\0")?,
        usrp_set_tx_bandwidth: *sym(lib, b"uhd_usrp_set_tx_bandwidth\0")?,
        usrp_set_rx_antenna: *sym(lib, b"uhd_usrp_set_rx_antenna\0")?,
        usrp_get_rx_antenna: *sym(lib, b"uhd_usrp_get_rx_antenna\0")?,
        usrp_get_rx_antennas: *sym(lib, b"uhd_usrp_get_rx_antennas\0")?,
        usrp_set_tx_antenna: *sym(lib, b"uhd_usrp_set_tx_antenna\0")?,
        usrp_get_tx_antenna: *sym(lib, b"uhd_usrp_get_tx_antenna\0")?,
        usrp_get_tx_antennas: *sym(lib, b"uhd_usrp_get_tx_antennas\0")?,
        string_vector_make: *sym(lib, b"uhd_string_vector_make\0")?,
        string_vector_free: *sym(lib, b"uhd_string_vector_free\0")?,
        string_vector_size: *sym(lib, b"uhd_string_vector_size\0")?,
        string_vector_at: *sym(lib, b"uhd_string_vector_at\0")?,
        subdev_spec_make: *sym(lib, b"uhd_subdev_spec_make\0")?,
        usrp_set_rx_subdev_spec: *sym(lib, b"uhd_usrp_set_rx_subdev_spec\0")?,
        usrp_set_tx_subdev_spec: *sym(lib, b"uhd_usrp_set_tx_subdev_spec\0")?,
    })
}

/// USRP gain arrives in URH as 0..100 and is normalized to 0..1 with
/// `gain * 0.01` (USRP.set_device_gain).
#[inline]
fn normalize_gain(gain: f64) -> f64 {
    gain * 0.01
}

/// `adapt_num_read_samples_to_sample_rate` (USRP.py). Sub-1 MHz rates clamp
/// to one chunk (Python's `int()` truncation would give 0 there).
#[inline]
fn rx_chunk_size(sample_rate: f64) -> usize {
    SYNC_RX_CHUNK_SIZE * (sample_rate / 1e6).max(1.0) as usize
}

fn check_uhd(ret: UhdError, what: &str) -> Result<(), DeviceError> {
    if ret == UHD_ERROR_NONE {
        Ok(())
    } else if ret == ANTENNA_INDEX_ERROR {
        Err(DeviceError::ParameterRejected(
            "Antenna index not supported on this device".into(),
        ))
    } else {
        Err(DeviceError::ParameterRejected(format!(
            "{what} failed with uhd error {ret}"
        )))
    }
}

/// Read a uhd string-vector handle into deduplicated `Vec<String>`
/// (`get_antennas` / `find_devices` shape).
fn read_string_vector(
    api: &UhdApi,
    mut h: UhdStringVectorHandle,
) -> Vec<String> {
    let mut result = Vec::new();
    if h.is_null() {
        return result;
    }
    // SAFETY: `h` is a live string-vector handle created by the caller (or
    // NULL, guarded above); the 512-byte buffer matches the pyx allocation;
    // the handle is freed at the end and never used afterwards.
    unsafe {
        let mut n = 0usize;
        (api.string_vector_size)(h, &mut n);
        let mut buf = vec![0u8; 512];
        for i in 0..n {
            (api.string_vector_at)(h, i, buf.as_mut_ptr().cast(), buf.len());
            if let Ok(s) = CStr::from_bytes_until_nul(&buf) {
                let s = s.to_string_lossy().into_owned();
                if !result.contains(&s) {
                    result.push(s);
                }
            }
        }
        (api.string_vector_free)(&mut h);
    }
    result
}

// ---------------------------------------------------------------------------
// device
// ---------------------------------------------------------------------------

/// A USRP opened through libuhd. Not `Send`/`Sync` (raw pointers); the RX
/// worker thread borrows raw handles, so keep the device alive while
/// streaming.
pub struct UsrpDevice {
    /// Keeps the dlopened vendor library alive for the lifetime of the device
    /// (the resolved fn pointers in `api` point into it).
    #[allow(dead_code)] // write-only by design: drop-order keep-alive
    lib: Library,
    api: UhdApi,
    usrp: UhdUsrpHandle,
    rx_streamer: UhdRxStreamerHandle,
    tx_streamer: UhdTxStreamerHandle,
    rx_metadata: UhdRxMetadataHandle,
    tx_metadata: UhdTxMetadataHandle,
    max_num_rx_samples: usize,
    max_num_tx_samples: usize,
    channel: usize,
    is_tx: bool,
    rx_thread: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    device_args: String,
}

impl UsrpDevice {
    /// dlopen libuhd and make a USRP handle (`usrp.open(device_args)`).
    /// An empty identifier opens the default device (uhd's empty args "").
    pub fn open(identifier: Option<&str>) -> Result<Self, DeviceError> {
        let lib = load_library()?;
        let api = resolve_api(&lib)?;

        let device_args = identifier.unwrap_or("").to_string();
        let c_args = CString::new(device_args.clone())
            .map_err(|_| DeviceError::ParameterRejected("args contain NUL".into()))?;

        let mut usrp: UhdUsrpHandle = std::ptr::null_mut();
        // SAFETY: `c_args` is a valid C string; `usrp` is a valid out-pointer
        // to a local initialized to NULL; the resulting handle is owned by us
        // and freed via uhd_usrp_free in Drop.
        let ret = unsafe { (api.usrp_make)(&mut usrp, c_args.as_ptr()) };
        if ret != UHD_ERROR_NONE {
            let detail = Self::last_error_with(&api, usrp);
            return Err(DeviceError::OpenFailed(format!(
                "uhd_usrp_make failed with {ret}: {detail}"
            )));
        }

        Ok(Self {
            lib,
            api,
            usrp,
            rx_streamer: std::ptr::null_mut(),
            tx_streamer: std::ptr::null_mut(),
            rx_metadata: std::ptr::null_mut(),
            tx_metadata: std::ptr::null_mut(),
            max_num_rx_samples: 300,
            max_num_tx_samples: 300,
            channel: 0,
            is_tx: false,
            rx_thread: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            device_args,
        })
    }

    /// The device args this device was opened with.
    pub fn device_args(&self) -> &str {
        &self.device_args
    }

    /// `usrp.get_last_error()` — handles a NULL device like the pyx does.
    fn last_error_with(api: &UhdApi, usrp: UhdUsrpHandle) -> String {
        if usrp.is_null() {
            return "Could not retrieve more detailed error message from device.".into();
        }
        // SAFETY: live usrp handle; the 1024-byte buffer is a valid
        // out-pointer matching the pyx allocation.
        unsafe {
            let mut buf = vec![0u8; 1024];
            (api.usrp_last_error)(usrp, buf.as_mut_ptr().cast(), buf.len());
            CStr::from_bytes_until_nul(&buf)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        }
    }

    /// `usrp.find_devices(args)` (`USRP.get_device_list`).
    pub fn find_devices(args: &str) -> Result<Vec<String>, DeviceError> {
        let lib = load_library()?;
        let api = resolve_api(&lib)?;
        let c_args =
            CString::new(args).map_err(|_| DeviceError::ParameterRejected("NUL in args".into()))?;
        let mut h: UhdStringVectorHandle = std::ptr::null_mut();
        // SAFETY: valid C string; the vector handle is created and freed
        // inside read_string_vector.
        unsafe {
            (api.string_vector_make)(&mut h);
            (api.usrp_find)(c_args.as_ptr(), &mut h);
        }
        Ok(read_string_vector(&api, h))
    }

    /// `usrp.get_device_representation()` — pretty-print string of the
    /// opened device.
    pub fn device_representation(&self) -> String {
        // SAFETY: `self.usrp` is a live handle; the 3000-byte buffer matches
        // the pyx's allocation.
        unsafe {
            let mut buf = vec![0u8; 3000];
            (self.api.usrp_get_pp_string)(self.usrp, buf.as_mut_ptr().cast(), buf.len());
            CStr::from_bytes_until_nul(&buf)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        }
    }

    /// `usrp.set_subdevice(markup, mboard)` — empty markup is a no-op,
    /// exactly like the pyx. Not reachable through the `Device` trait (URH
    /// sends it as a custom "SET_SUBDEVICE" parameter).
    pub fn set_subdevice(&mut self, markup: &str, mboard: usize) -> Result<(), DeviceError> {
        if markup.is_empty() {
            return Ok(());
        }
        let c_markup = CString::new(markup)
            .map_err(|_| DeviceError::ParameterRejected("markup contains NUL".into()))?;
        let mut spec: UhdSubdevSpecHandle = std::ptr::null_mut();
        // SAFETY: valid C string; `spec` out-pointer; the subdev handle is
        // passed to uhd which takes it (the pyx likewise never frees it —
        // the C API has no subdev_spec_free).
        unsafe {
            let ret = (self.api.subdev_spec_make)(&mut spec, c_markup.as_ptr());
            check_uhd(ret, "uhd_subdev_spec_make")?;
            let ret = if self.is_tx {
                (self.api.usrp_set_tx_subdev_spec)(self.usrp, spec, mboard)
            } else {
                (self.api.usrp_set_rx_subdev_spec)(self.usrp, spec, mboard)
            };
            check_uhd(ret, "set_subdev_spec")
        }
    }

    /// `usrp.get_antennas()` for the current direction.
    pub fn get_antennas(&self) -> Vec<String> {
        let mut h: UhdStringVectorHandle = std::ptr::null_mut();
        // SAFETY: live usrp handle; the vector handle is created here and
        // freed inside read_string_vector.
        unsafe {
            (self.api.string_vector_make)(&mut h);
            if self.is_tx {
                (self.api.usrp_get_tx_antennas)(self.usrp, self.channel, &mut h);
            } else {
                (self.api.usrp_get_rx_antennas)(self.usrp, self.channel, &mut h);
            }
        }
        read_string_vector(&self.api, h)
    }

    /// `usrp.get_antenna()` — current antenna name of the active direction.
    pub fn get_antenna(&self) -> String {
        // SAFETY: live usrp handle; the 512-byte buffer matches the pyx
        // allocation; nothing escapes the call.
        unsafe {
            let mut buf = vec![0u8; 512];
            if self.is_tx {
                (self.api.usrp_get_tx_antenna)(self.usrp, self.channel, buf.as_mut_ptr().cast(), buf.len());
            } else {
                (self.api.usrp_get_rx_antenna)(self.usrp, self.channel, buf.as_mut_ptr().cast(), buf.len());
            }
            CStr::from_bytes_until_nul(&buf)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        }
    }

    /// `usrp.set_antenna(index)` — index into [`Self::get_antennas`];
    /// out-of-range yields the pyx's error 4711.
    fn set_antenna(&mut self, index: i32) -> Result<(), DeviceError> {
        let antennas = self.get_antennas();
        if index < 0 || index as usize >= antennas.len() {
            return check_uhd(ANTENNA_INDEX_ERROR, "set_antenna");
        }
        let c_ant = CString::new(antennas[index as usize].clone())
            .map_err(|_| DeviceError::ParameterRejected("antenna contains NUL".into()))?;
        // SAFETY: valid C string; live usrp handle.
        let ret = unsafe {
            if self.is_tx {
                (self.api.usrp_set_tx_antenna)(self.usrp, c_ant.as_ptr(), self.channel)
            } else {
                (self.api.usrp_set_rx_antenna)(self.usrp, c_ant.as_ptr(), self.channel)
            }
        };
        check_uhd(ret, "set_antenna")
    }

    /// `usrp.setup_stream()` — fc32/sc16 stream on channel `self.channel`.
    fn setup_stream(&mut self) -> Result<(), DeviceError> {
        let mut channels = [self.channel];
        let mut stream_args = UhdStreamArgs {
            cpu_format: c"fc32".as_ptr(),
            otw_format: c"sc16".as_ptr(),
            args: c"".as_ptr(),
            channel_list: channels.as_mut_ptr(),
            n_channels: 1,
        };
        // SAFETY: `stream_args` is a fully initialized repr(C) struct whose
        // pointers stay valid for the duration of the calls; the streamer
        // handles are owned by us and freed in `destroy_stream`.
        unsafe {
            if self.is_tx {
                (self.api.tx_streamer_make)(&mut self.tx_streamer);
                if self.tx_streamer.is_null() {
                    return Err(DeviceError::StreamError("uhd_tx_streamer_make failed".into()));
                }
                let ret = (self.api.usrp_get_tx_stream)(self.usrp, &mut stream_args, self.tx_streamer);
                check_uhd(ret, "uhd_usrp_get_tx_stream")?;
                let mut n = 0usize;
                (self.api.tx_streamer_num_channels)(self.tx_streamer, &mut n);
                (self.api.tx_streamer_max_num_samps)(self.tx_streamer, &mut self.max_num_tx_samples);
            } else {
                (self.api.rx_streamer_make)(&mut self.rx_streamer);
                if self.rx_streamer.is_null() {
                    return Err(DeviceError::StreamError("uhd_rx_streamer_make failed".into()));
                }
                let ret = (self.api.usrp_get_rx_stream)(self.usrp, &mut stream_args, self.rx_streamer);
                check_uhd(ret, "uhd_usrp_get_rx_stream")?;
                let mut n = 0usize;
                (self.api.rx_streamer_num_channels)(self.rx_streamer, &mut n);
                (self.api.rx_streamer_max_num_samps)(self.rx_streamer, &mut self.max_num_rx_samples);
            }
        }
        if self.max_num_rx_samples == 0 {
            self.max_num_rx_samples = 300;
        }
        if self.max_num_tx_samples == 0 {
            self.max_num_tx_samples = 300;
        }
        Ok(())
    }

    /// `usrp.start_stream()` — issue start-continuous (RX) or make TX
    /// metadata (has_time_spec=false, frac_secs 0.1, start_of_burst=true).
    fn start_stream(&mut self) -> Result<(), DeviceError> {
        // SAFETY: handles are live (created in setup_stream); out-params are
        // valid locals; the stream command struct is fully initialized.
        unsafe {
            if self.is_tx {
                let ret = (self.api.tx_metadata_make)(&mut self.tx_metadata, false, 0, 0.1, true, false);
                return check_uhd(ret, "uhd_tx_metadata_make");
            }
            (self.api.rx_metadata_make)(&mut self.rx_metadata);
            let cmd = UhdStreamCmd {
                stream_mode: UHD_STREAM_MODE_START_CONTINUOUS,
                num_samps: 0,
                stream_now: true,
                time_spec_full_secs: 0,
                time_spec_frac_secs: 0.0,
            };
            let ret = (self.api.rx_streamer_issue_stream_cmd)(self.rx_streamer, &cmd);
            check_uhd(ret, "issue_stream_cmd(start)")
        }
    }

    /// `usrp.stop_stream()`.
    fn stop_stream(&mut self) -> Result<(), DeviceError> {
        // SAFETY: handles are owned by us; NULL guards mirror the pyx's
        // ordering (metadata freed before the stop command is issued).
        unsafe {
            if self.is_tx {
                if !self.tx_metadata.is_null() {
                    let ret = (self.api.tx_metadata_free)(&mut self.tx_metadata);
                    self.tx_metadata = std::ptr::null_mut();
                    return check_uhd(ret, "uhd_tx_metadata_free");
                }
                return Ok(());
            }
            if !self.rx_metadata.is_null() {
                (self.api.rx_metadata_free)(&mut self.rx_metadata);
                self.rx_metadata = std::ptr::null_mut();
            }
            if !self.rx_streamer.is_null() {
                let cmd = UhdStreamCmd {
                    stream_mode: UHD_STREAM_MODE_STOP_CONTINUOUS,
                    num_samps: 0,
                    stream_now: true,
                    time_spec_full_secs: 0,
                    time_spec_frac_secs: 0.0,
                };
                let ret = (self.api.rx_streamer_issue_stream_cmd)(self.rx_streamer, &cmd);
                return check_uhd(ret, "issue_stream_cmd(stop)");
            }
            Ok(())
        }
    }

    /// `usrp.destroy_stream()`.
    fn destroy_stream(&mut self) -> Result<(), DeviceError> {
        // SAFETY: handles owned by us, created in setup_stream; NULL guards
        // prevent double frees.
        unsafe {
            if !self.rx_streamer.is_null() {
                let ret = (self.api.rx_streamer_free)(&mut self.rx_streamer);
                self.rx_streamer = std::ptr::null_mut();
                check_uhd(ret, "uhd_rx_streamer_free")?;
            }
            if !self.tx_streamer.is_null() {
                let ret = (self.api.tx_streamer_free)(&mut self.tx_streamer);
                self.tx_streamer = std::ptr::null_mut();
                check_uhd(ret, "uhd_tx_streamer_free")?;
            }
            Ok(())
        }
    }

    /// `usrp.set_center_freq` — AUTO tune policies for RF and DSP
    /// (frontend+DSP split is done by uhd itself, not by us).
    fn set_center_freq(&mut self, freq: f64) -> Result<(), DeviceError> {
        let mut tune_request = UhdTuneRequest {
            target_freq: freq,
            rf_freq_policy: UHD_TUNE_REQUEST_POLICY_AUTO,
            rf_freq: 0.0,
            dsp_freq_policy: UHD_TUNE_REQUEST_POLICY_AUTO,
            dsp_freq: 0.0,
            args: std::ptr::null(),
        };
        let mut tune_result = UhdTuneResult {
            clipped_rf_freq: 0.0,
            target_rf_freq: 0.0,
            actual_rf_freq: 0.0,
            target_dsp_freq: 0.0,
            actual_dsp_freq: 0.0,
        };
        // SAFETY: both structs are fully initialized repr(C) locals whose
        // pointers stay valid for the call; `self.usrp` is live.
        let ret = unsafe {
            if self.is_tx {
                (self.api.usrp_set_tx_freq)(self.usrp, &mut tune_request, self.channel, &mut tune_result)
            } else {
                (self.api.usrp_set_rx_freq)(self.usrp, &mut tune_request, self.channel, &mut tune_result)
            }
        };
        check_uhd(ret, "set_center_freq")
    }

    /// `usrp.set_sample_rate`.
    fn set_sample_rate(&mut self, rate: f64) -> Result<(), DeviceError> {
        // SAFETY: live usrp handle, scalar args.
        let ret = unsafe {
            if self.is_tx {
                (self.api.usrp_set_tx_rate)(self.usrp, rate, self.channel)
            } else {
                (self.api.usrp_set_rx_rate)(self.usrp, rate, self.channel)
            }
        };
        check_uhd(ret, "set_sample_rate")
    }

    /// `usrp.set_bandwidth`.
    fn set_bandwidth(&mut self, bw: f64) -> Result<(), DeviceError> {
        // SAFETY: live usrp handle, scalar args.
        let ret = unsafe {
            if self.is_tx {
                (self.api.usrp_set_tx_bandwidth)(self.usrp, bw, self.channel)
            } else {
                (self.api.usrp_set_rx_bandwidth)(self.usrp, bw, self.channel)
            }
        };
        check_uhd(ret, "set_bandwidth")
    }

    /// `usrp.set_rf_gain` — normalized 0..1 gain (URH sends 0..100).
    fn set_rf_gain(&mut self, gain: f64) -> Result<(), DeviceError> {
        let g = normalize_gain(gain);
        if !(0.0..=1.0).contains(&g) {
            return Err(DeviceError::ParameterRejected(format!(
                "normalized gain {g} out of range [0, 1]"
            )));
        }
        // SAFETY: live usrp handle, scalar args.
        let ret = unsafe {
            if self.is_tx {
                (self.api.usrp_set_normalized_tx_gain)(self.usrp, g, self.channel)
            } else {
                (self.api.usrp_set_normalized_rx_gain)(self.usrp, g, self.channel)
            }
        };
        check_uhd(ret, "set_rf_gain")
    }

    fn stop_rx_thread(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.rx_thread.take() {
            let _ = t.join();
        }
    }
}

/// The function-pointer subset copied into the RX worker thread (fn pointers
/// are `Send` even though the parent struct is not).
struct ApiWorker {
    rx_streamer_recv: unsafe extern "C" fn(
        UhdRxStreamerHandle,
        *mut *mut c_void,
        usize,
        *mut UhdRxMetadataHandle,
        c_double,
        bool,
        *mut usize,
    ) -> UhdError,
}

impl Device for UsrpDevice {
    fn name(&self) -> &'static str {
        "USRP"
    }

    fn device_list() -> Vec<String> {
        Self::find_devices("").unwrap_or_default()
    }

    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        match cmd {
            Command::SetFrequency => self.set_center_freq(value),
            Command::SetSampleRate => self.set_sample_rate(value),
            Command::SetBandwidth => self.set_bandwidth(value),
            Command::SetRfGain => self.set_rf_gain(value),
            Command::SetAntennaIndex => self.set_antenna(value as i32),
            // Extension over USRP.py: the pyx has usrp.set_channel but never
            // wires it into DEVICE_METHODS; we expose it through the trait's
            // channel command. The custom "SET_SUBDEVICE" parameter of USRP.py
            // has no trait equivalent — use [`UsrpDevice::set_subdevice`].
            Command::SetChannelIndex => {
                if value < 0.0 {
                    return Err(DeviceError::ParameterRejected("negative channel".into()));
                }
                self.channel = value as usize;
                Ok(())
            }
            _ => Err(DeviceError::Unsupported(
                "USRP: command not in device_parameters",
            )),
        }
    }

    fn start_rx(
        &mut self,
        config: &DeviceConfig,
        sink: std::sync::mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        self.stop_rx_thread();
        self.is_tx = false;
        self.set_parameter(Command::SetFrequency, config.frequency)?;
        self.set_parameter(Command::SetSampleRate, config.sample_rate)?;
        if config.bandwidth > 0.0 {
            self.set_parameter(Command::SetBandwidth, config.bandwidth)?;
        }
        if config.rf_gain != 0.0 {
            self.set_parameter(Command::SetRfGain, config.rf_gain)?;
        }
        if config.antenna_index >= 0 {
            self.set_parameter(Command::SetAntennaIndex, config.antenna_index as f64)?;
        }

        self.setup_stream()?;
        self.start_stream()?;

        let chunk_size = rx_chunk_size(config.sample_rate);
        let streamer = self.rx_streamer as usize;
        let metadata = self.rx_metadata as usize;
        let max_samps = self.max_num_rx_samples;
        let stop = Arc::clone(&self.stop_flag);
        self.stop_flag.store(false, Ordering::SeqCst);

        // Worker owns raw handles as plain integers (parent is !Send). The
        // parent guarantees the streamer/metadata stay alive until it sets
        // `stop` and joins this thread.
        let api = ApiWorker {
            rx_streamer_recv: self.api.rx_streamer_recv,
        };

        self.rx_thread = Some(std::thread::spawn(move || {
            let streamer = streamer as UhdRxStreamerHandle;
            let metadata = metadata as UhdRxMetadataHandle;
            let mut buff = vec![0.0f32; 2 * max_samps];
            let mut acc: Vec<crate::Complex32> = Vec::with_capacity(chunk_size);
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let mut items_received = 0usize;
                // SAFETY: `streamer`/`metadata` are the live handles created
                // by the parent (valid until stop+join); `buff` provides a
                // valid fc32 buffer of max_samps complex samples; the
                // metadata handle is passed by pointer-to-handle exactly as
                // the uhd C API expects. Results are copied out immediately;
                // no pointers escape; no panics on this path.
                let ret = unsafe {
                    let mut buffs = buff.as_mut_ptr().cast::<c_void>();
                    let mut md = metadata;
                    (api.rx_streamer_recv)(
                        streamer,
                        &mut buffs,
                        max_samps,
                        &mut md,
                        3.0,
                        false,
                        &mut items_received,
                    )
                };
                if ret != UHD_ERROR_NONE || items_received == 0 {
                    break; // timeout or error — stop silently, never panic
                }
                for pair in buff[..2 * items_received.min(max_samps)].chunks_exact(2) {
                    acc.push(crate::Complex32::new(pair[0], pair[1]));
                }
                if acc.len() >= chunk_size {
                    if sink.send(std::mem::take(&mut acc)).is_err() {
                        break; // receiver gone
                    }
                }
            }
        }));
        Ok(())
    }

    fn send(
        &mut self,
        config: &DeviceConfig,
        samples: &[crate::Complex32],
    ) -> Result<(), DeviceError> {
        self.stop_rx_thread();
        self.stop_stream()?;
        self.destroy_stream()?;
        self.is_tx = true;

        self.set_parameter(Command::SetFrequency, config.frequency)?;
        self.set_parameter(Command::SetSampleRate, config.sample_rate)?;
        if config.bandwidth > 0.0 {
            self.set_parameter(Command::SetBandwidth, config.bandwidth)?;
        }
        if config.rf_gain != 0.0 {
            self.set_parameter(Command::SetRfGain, config.rf_gain)?;
        }
        if config.antenna_index >= 0 {
            self.set_parameter(Command::SetAntennaIndex, config.antenna_index as f64)?;
        }

        self.setup_stream()?;
        self.start_stream()?; // makes TX metadata

        // `send_stream`: a single zero sample means "send zeros to prevent
        // underflows" (8 * max_num_tx_samples zeros).
        let zero_fill = samples.len() == 1 && samples[0] == crate::Complex32::new(0.0, 0.0);
        let flat: Vec<f32> = if zero_fill {
            vec![0.0; 2 * 8 * self.max_num_tx_samples]
        } else {
            samples.iter().flat_map(|c| [c.re, c.im]).collect()
        };

        let streamer = self.tx_streamer;
        let metadata = self.tx_metadata;
        let max_samps = self.max_num_tx_samples;
        let mut sent = 0usize;
        // SAFETY: streamer/metadata are live handles created above; each
        // chunk pointer is valid for its call; nothing escapes.
        unsafe {
            let mut num_samps_sent = 0usize;
            while sent < flat.len() {
                let take = (flat.len() - sent).min(2 * max_samps);
                let chunk = &flat[sent..sent + take];
                let buffs = chunk.as_ptr().cast::<c_void>();
                let mut md = metadata;
                let ret = (self.api.tx_streamer_send)(
                    streamer,
                    &buffs,
                    take / 2,
                    &mut md,
                    0.1,
                    &mut num_samps_sent,
                );
                if ret != UHD_ERROR_NONE {
                    self.stop_stream()?;
                    self.destroy_stream()?;
                    self.is_tx = false;
                    return Err(DeviceError::StreamError(format!(
                        "uhd_tx_streamer_send failed with {ret}"
                    )));
                }
                sent += take;
            }
        }

        // Teardown mirrors shutdown_device: stop stream, free streamer.
        self.stop_stream()?;
        self.destroy_stream()?;
        self.is_tx = false;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.stop_rx_thread();
        self.stop_stream()?;
        self.destroy_stream()?;
        // SAFETY: `self.usrp` is the live handle owned by us; uhd_usrp_free
        // NULLs the slot so Drop cannot double-free.
        let ret = unsafe { (self.api.usrp_free)(&mut self.usrp) };
        self.is_tx = false;
        check_uhd(ret, "uhd_usrp_free")
    }
}

impl Drop for UsrpDevice {
    fn drop(&mut self) {
        self.stop_rx_thread();
        // SAFETY (all blocks below): handles are owned by us; the NULL
        // guards prevent double frees regardless of which teardown path ran.
        unsafe {
            if !self.rx_metadata.is_null() {
                (self.api.rx_metadata_free)(&mut self.rx_metadata);
                self.rx_metadata = std::ptr::null_mut();
            }
            if !self.tx_metadata.is_null() {
                (self.api.tx_metadata_free)(&mut self.tx_metadata);
                self.tx_metadata = std::ptr::null_mut();
            }
            if !self.rx_streamer.is_null() {
                (self.api.rx_streamer_free)(&mut self.rx_streamer);
                self.rx_streamer = std::ptr::null_mut();
            }
            if !self.tx_streamer.is_null() {
                (self.api.tx_streamer_free)(&mut self.tx_streamer);
                self.tx_streamer = std::ptr::null_mut();
            }
            if !self.usrp.is_null() {
                (self.api.usrp_free)(&mut self.usrp);
                self.usrp = std::ptr::null_mut();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_names_table() {
        assert_eq!(LIBRARY_NAMES[0], "libuhd.410.dylib");
        assert!(LIBRARY_NAMES.contains(&"libuhd.so.40.0.0"));
        assert!(LIBRARY_NAMES.iter().all(|n| n.contains("uhd")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dll")));
    }

    #[test]
    fn open_without_libuhd_is_library_unavailable() {
        let installed = LIBRARY_NAMES
            .iter()
            .any(|n| unsafe { Library::new(n) }.is_ok());
        // SAFETY: constant names; plain dlopen probe.
        if !installed {
            match UsrpDevice::open(None) {
                Err(DeviceError::LibraryUnavailable(_)) => {}
                _ => panic!("expected LibraryUnavailable"),
            }
        }
    }

    #[test]
    fn gain_normalization_matches_usrp_py() {
        // USRP.set_device_gain: gain * 0.01
        assert!((normalize_gain(0.0) - 0.0).abs() < 1e-12);
        assert!((normalize_gain(50.0) - 0.5).abs() < 1e-12);
        assert!((normalize_gain(100.0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn chunk_size_adaptation_matches_usrp_py() {
        // 16384 * int(sample_rate / 1e6), minimum 1x
        assert_eq!(rx_chunk_size(1e6), 16384);
        assert_eq!(rx_chunk_size(2e6), 32768);
        assert_eq!(rx_chunk_size(10e6), 163840);
        // sub-1 MHz rates clamp to 1x (int() truncation gives 0 in Python;
        // the sync loop would then spin — we keep at least one chunk)
        assert_eq!(rx_chunk_size(500e3), 16384);
    }

    #[test]
    fn find_devices_without_libuhd_errors_cleanly() {
        let installed = LIBRARY_NAMES
            .iter()
            .any(|n| unsafe { Library::new(n) }.is_ok());
        // SAFETY: constant names; plain dlopen probe.
        if !installed {
            assert!(matches!(
                UsrpDevice::find_devices(""),
                Err(DeviceError::LibraryUnavailable(_))
            ));
        }
    }
}
