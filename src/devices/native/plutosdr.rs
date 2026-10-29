//! PlutoSDR backend — runtime binding of **libiio** (port of
//! `urh.dev.native.lib.plutosdr` / `PlutoSDR.py`).
//!
//! The library is dlopened at runtime and every C symbol is resolved via
//! `dlsym` when the device is opened; on machines without libiio
//! [`PlutoSdrDevice::open`] returns [`DeviceError::LibraryUnavailable`] and
//! the crate still compiles/links (no link-time dependency on libiio).
//!
//! C API declared below mirrors `cplutosdr.pxd` (iio.h).

use std::ffi::{c_char, c_int, c_longlong, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use libloading::{Library, Symbol};

use crate::devices::{Command, Device, DeviceConfig, DeviceError, IqChunk};

/// Library file names tried (in order) when dlopening libiio.
pub const LIBRARY_NAMES: &[&str] = &[
    "libiio.0.dylib",
    "libiio.dylib",
    "libiio.so.0",
    "libiio.so",
    "libiio.dll",
    "iio.dll",
];

/// RX buffer size in samples (PlutoSDR.SYNC_RX_CHUNK_SIZE).
const SYNC_RX_CHUNK_SIZE: usize = 65536;
/// TX buffer size in samples (SYNC_TX_CHUNK_SIZE // 2).
const SYNC_TX_CHUNK_SIZE: usize = 65536 / 2;

// ---------------------------------------------------------------------------
// C API types (iio.h) — opaque handles only, no struct layouts needed.
// ---------------------------------------------------------------------------

#[repr(C)]
struct IioContext(c_void);
#[repr(C)]
struct IioDevice(c_void);
#[repr(C)]
struct IioChannel(c_void);
#[repr(C)]
struct IioBuffer(c_void);
#[repr(C)]
struct IioScanContext(c_void);
#[repr(C)]
struct IioContextInfo(c_void);

/// The iio.h function subset URH uses (`cplutosdr.pxd`), resolved at
/// runtime. All pointers are opaque libiio handles.
#[allow(clippy::type_complexity)]
struct IioApi {
    create_scan_context: unsafe extern "C" fn(*const c_char, u32) -> *mut IioScanContext,
    scan_context_destroy: unsafe extern "C" fn(*mut IioScanContext),
    scan_context_get_info_list:
        unsafe extern "C" fn(*mut IioScanContext, *mut *mut *mut IioContextInfo) -> isize,
    context_info_list_free: unsafe extern "C" fn(*mut *mut IioContextInfo),
    context_info_get_description: unsafe extern "C" fn(*const IioContextInfo) -> *const c_char,
    context_info_get_uri: unsafe extern "C" fn(*const IioContextInfo) -> *const c_char,

    create_context_from_uri: unsafe extern "C" fn(*const c_char) -> *mut IioContext,
    context_find_device: unsafe extern "C" fn(*const IioContext, *const c_char) -> *mut IioDevice,
    context_destroy: unsafe extern "C" fn(*mut IioContext),

    device_find_channel:
        unsafe extern "C" fn(*const IioDevice, *const c_char, bool) -> *mut IioChannel,
    channel_enable: unsafe extern "C" fn(*mut IioChannel),
    channel_disable: unsafe extern "C" fn(*mut IioChannel),
    channel_attr_write_longlong:
        unsafe extern "C" fn(*const IioChannel, *const c_char, c_longlong) -> c_int,
    channel_attr_write: unsafe extern "C" fn(*const IioChannel, *const c_char, *const c_char) -> isize,

    device_create_buffer:
        unsafe extern "C" fn(*const IioDevice, usize, bool) -> *mut IioBuffer,
    buffer_destroy: unsafe extern "C" fn(*mut IioBuffer),
    buffer_refill: unsafe extern "C" fn(*mut IioBuffer) -> isize,
    buffer_step: unsafe extern "C" fn(*const IioBuffer) -> isize,
    buffer_first: unsafe extern "C" fn(*const IioBuffer, *const IioChannel) -> *mut c_void,
    buffer_end: unsafe extern "C" fn(*const IioBuffer) -> *mut c_void,
    buffer_push: unsafe extern "C" fn(*mut IioBuffer) -> isize,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn load_library() -> Result<Library, DeviceError> {
    let mut last = String::new();
    for name in LIBRARY_NAMES {
        // SAFETY: `name` is a compile-time constant NUL-free string; the
        // call only resolves/loads the shared object.
        match crate::devices::native::try_open(name) {
            Ok(lib) => return Ok(lib),
            Err(e) => last = format!("{name}: {e}"),
        }
    }
    Err(DeviceError::LibraryUnavailable(format!("libiio ({last})")))
}

fn sym<'a, T>(lib: &'a Library, name: &[u8]) -> Result<Symbol<'a, T>, DeviceError> {
    // SAFETY: generic dlsym lookup; the caller is responsible for calling the
    // obtained function with a signature matching the vendor header.
    unsafe { lib.get::<T>(name) }.map_err(|e| DeviceError::LibraryUnavailable(e.to_string()))
}

fn resolve_api(lib: &Library) -> Result<IioApi, DeviceError> {
    Ok(IioApi {
        create_scan_context: *sym(
            lib,
            b"iio_create_scan_context\0",
        )?,
        scan_context_destroy: *sym(lib, b"iio_scan_context_destroy\0")?,
        scan_context_get_info_list: *sym(lib, b"iio_scan_context_get_info_list\0")?,
        context_info_list_free: *sym(lib, b"iio_context_info_list_free\0")?,
        context_info_get_description: *sym(lib, b"iio_context_info_get_description\0")?,
        context_info_get_uri: *sym(lib, b"iio_context_info_get_uri\0")?,
        create_context_from_uri: *sym(lib, b"iio_create_context_from_uri\0")?,
        context_find_device: *sym(lib, b"iio_context_find_device\0")?,
        context_destroy: *sym(lib, b"iio_context_destroy\0")?,
        device_find_channel: *sym(lib, b"iio_device_find_channel\0")?,
        channel_enable: *sym(lib, b"iio_channel_enable\0")?,
        channel_disable: *sym(lib, b"iio_channel_disable\0")?,
        channel_attr_write_longlong: *sym(lib, b"iio_channel_attr_write_longlong\0")?,
        channel_attr_write: *sym(lib, b"iio_channel_attr_write\0")?,
        device_create_buffer: *sym(lib, b"iio_device_create_buffer\0")?,
        buffer_destroy: *sym(lib, b"iio_buffer_destroy\0")?,
        buffer_refill: *sym(lib, b"iio_buffer_refill\0")?,
        buffer_step: *sym(lib, b"iio_buffer_step\0")?,
        buffer_first: *sym(lib, b"iio_buffer_first\0")?,
        buffer_end: *sym(lib, b"iio_buffer_end\0")?,
        buffer_push: *sym(lib, b"iio_buffer_push\0")?,
    })
}

/// Extract the URI from a `"desc [uri]"` device-list entry
/// (PlutoSDR.get_device_list format). Falls back to the raw string.
fn extract_uri(identifier: &str) -> String {
    if let (Some(a), Some(b)) = (identifier.find('['), identifier.rfind(']')) {
        if a + 1 < b {
            return identifier[a + 1..b].to_string();
        }
    }
    identifier.to_string()
}

/// Convert a raw AD9361 12-bit sample pair to normalized complex
/// (PlutoSDR.bytes_to_iq does `int16 << 4`, URH then scales by 1/32768).
/// Deviation from numpy: we saturate instead of wrapping on overflow.
#[inline]
fn raw_pair_to_complex(i: i16, q: i16) -> crate::Complex32 {
    let i = ((i as i32) << 4).clamp(i16::MIN as i32, i16::MAX as i32) as f32 / 32768.0;
    let q = ((q as i32) << 4).clamp(i16::MIN as i32, i16::MAX as i32) as f32 / 32768.0;
    crate::Complex32::new(i, q)
}

/// Inverse of [`raw_pair_to_complex`] for TX (`iq_to_bytes` does `>> 4`).
#[inline]
fn complex_to_raw_pair(c: crate::Complex32) -> (i16, i16) {
    let i = ((c.re * 32768.0) as i32) >> 4;
    let q = ((c.im * 32768.0) as i32) >> 4;
    (
        i.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        q.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    )
}

fn check_iio(ret: c_int, what: &str) -> Result<(), DeviceError> {
    // iio attr writes return 0 on success, negative errno on failure.
    if ret == 0 {
        Ok(())
    } else {
        Err(DeviceError::ParameterRejected(format!(
            "{what} failed with iio error {ret}"
        )))
    }
}

// ---------------------------------------------------------------------------
// device
// ---------------------------------------------------------------------------

/// A PlutoSDR opened through libiio.
///
/// Not `Send`/`Sync` (raw pointers); the RX worker thread borrows the raw
/// context handle, so the device must not be dropped while streaming.
pub struct PlutoSdrDevice {
    /// Keeps the dlopened vendor library alive for the lifetime of the device
    /// (the resolved fn pointers in `api` point into it).
    #[allow(dead_code)] // write-only by design: drop-order keep-alive
    lib: Library,
    api: IioApi,
    ctx: *mut IioContext,
    rx_buffer: *mut IioBuffer,
    tx_buffer: *mut IioBuffer,
    rx_thread: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    uri: String,
    /// Mirrors the pyx's global `IS_TX` flag: selects the TX LO channel and
    /// the output voltage0 channel. Set by `send()`, cleared by `stop()`.
    is_tx: bool,
}

impl PlutoSdrDevice {
    /// dlopen libiio and open a PlutoSDR context.
    ///
    /// `identifier` is either a bare URI (`ip:192.168.2.1`, `usb:...`) or a
    /// `"desc [uri]"` entry from [`Device::device_list`]. If absent, the
    /// backend scan is used and the first found device opened
    /// (PlutoSDR.setup_device).
    pub fn open(identifier: Option<&str>) -> Result<Self, DeviceError> {
        let lib = load_library()?;
        let api = resolve_api(&lib)?;

        let uri = match identifier.filter(|s| !s.is_empty()) {
            Some(id) => extract_uri(id),
            None => {
                let (_, uris) = Self::scan_with(&api)?;
                uris.into_iter().next().ok_or_else(|| {
                    DeviceError::OpenFailed("Could not find a connected PlutoSDR".into())
                })?
            }
        };

        let c_uri = CString::new(uri.clone())
            .map_err(|_| DeviceError::ParameterRejected("uri contains NUL".into()))?;
        // SAFETY: `c_uri` is a valid NUL-terminated C string; the returned
        // pointer is stored and only passed back to libiio while alive.
        let ctx = unsafe { (api.create_context_from_uri)(c_uri.as_ptr()) };
        if ctx.is_null() {
            return Err(DeviceError::OpenFailed(format!(
                "iio_create_context_from_uri({uri}) failed"
            )));
        }

        Ok(Self {
            lib,
            api,
            ctx,
            rx_buffer: std::ptr::null_mut(),
            tx_buffer: std::ptr::null_mut(),
            rx_thread: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            uri,
            is_tx: false,
        })
    }

    /// The URI this device was opened with.
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// `plutosdr.scan_devices()` using an already-resolved API table.
    fn scan_with(api: &IioApi) -> Result<(Vec<String>, Vec<String>), DeviceError> {
        // SAFETY: the function pointers were dlsym'd from a live libiio and
        // match the iio.h signatures; the info-list out-pointer is a valid
        // local, and libiio owns the returned strings.
        unsafe {
            let ctx = (api.create_scan_context)(std::ptr::null(), 0);
            if ctx.is_null() {
                return Err(DeviceError::OpenFailed("iio_create_scan_context failed".into()));
            }

            let mut infos: *mut *mut IioContextInfo = std::ptr::null_mut();
            let n = (api.scan_context_get_info_list)(ctx, &mut infos);
            let mut descs = Vec::new();
            let mut uris = Vec::new();
            for i in 0..n.max(0) as usize {
                let info = *infos.add(i); // `infos` is non-null when n > 0
                let d = CStr::from_ptr((api.context_info_get_description)(info))
                    .to_string_lossy()
                    .into_owned();
                let u = CStr::from_ptr((api.context_info_get_uri)(info))
                    .to_string_lossy()
                    .into_owned();
                descs.push(d);
                uris.push(u);
            }
            if !infos.is_null() {
                (api.context_info_list_free)(infos);
            }
            (api.scan_context_destroy)(ctx);
            Ok((descs, uris))
        }
    }

    /// Find a channel on a named device (`iio_context_find_device` +
    /// `iio_device_find_channel`); returns `None` when not found.
    fn find_channel(&self, device: &str, channel: &str, output: bool) -> Option<*mut IioChannel> {
        let c_dev = CString::new(device).ok()?;
        let c_chn = CString::new(channel).ok()?;
        // SAFETY: both CStrings are valid; `self.ctx` is a live context
        // handle owned by `self`. Pure lookup, no ownership transfer.
        unsafe {
            let dev = (self.api.context_find_device)(self.ctx, c_dev.as_ptr());
            if dev.is_null() {
                return None;
            }
            let chn = (self.api.device_find_channel)(dev, c_chn.as_ptr(), output);
            (!chn.is_null()).then_some(chn)
        }
    }

    /// `plutosdr.set_center_freq` — writes the LO frequency attribute
    /// (altvoltage0 for RX, altvoltage1 for TX).
    fn set_center_freq(&mut self, freq: f64) -> Result<(), DeviceError> {
        let chn = self
            .find_channel(
                "ad9361-phy",
                if self.is_tx { "altvoltage1" } else { "altvoltage0" },
                true,
            )
            .ok_or_else(|| DeviceError::ParameterRejected("ad9361-phy LO channel not found".into()))?;
        let attr = CString::new("frequency").unwrap();
        // SAFETY: `chn` was obtained from the live context; `attr` is a
        // valid NUL-terminated attribute name.
        let ret = unsafe { (self.api.channel_attr_write_longlong)(chn, attr.as_ptr(), freq as c_longlong) };
        check_iio(ret, "set_center_freq")
    }

    /// `plutosdr.set_sample_rate` — `sampling_frequency` on voltage0.
    fn set_sample_rate(&mut self, rate: f64) -> Result<(), DeviceError> {
        let chn = self
            .find_channel("ad9361-phy", "voltage0", self.is_tx)
            .ok_or_else(|| DeviceError::ParameterRejected("ad9361-phy voltage0 not found".into()))?;
        let attr = CString::new("sampling_frequency").unwrap();
        // SAFETY: see set_center_freq.
        let ret = unsafe { (self.api.channel_attr_write_longlong)(chn, attr.as_ptr(), rate as c_longlong) };
        check_iio(ret, "set_sample_rate")
    }

    /// `plutosdr.set_bandwidth` — `rf_bandwidth` on voltage0.
    fn set_bandwidth(&mut self, bw: f64) -> Result<(), DeviceError> {
        let chn = self
            .find_channel("ad9361-phy", "voltage0", self.is_tx)
            .ok_or_else(|| DeviceError::ParameterRejected("ad9361-phy voltage0 not found".into()))?;
        let attr = CString::new("rf_bandwidth").unwrap();
        // SAFETY: see set_center_freq.
        let ret = unsafe { (self.api.channel_attr_write_longlong)(chn, attr.as_ptr(), bw as c_longlong) };
        check_iio(ret, "set_bandwidth")
    }

    /// `plutosdr.set_rf_gain` — switch to manual gain control, then write
    /// `hardwaregain` (dB).
    fn set_rf_gain(&mut self, gain: f64) -> Result<(), DeviceError> {
        let chn = self
            .find_channel("ad9361-phy", "voltage0", self.is_tx)
            .ok_or_else(|| DeviceError::ParameterRejected("ad9361-phy voltage0 not found".into()))?;
        let mode = CString::new("manual").unwrap();
        let attr = CString::new("gain_control_mode").unwrap();
        // SAFETY: see set_center_freq.
        unsafe { (self.api.channel_attr_write)(chn, attr.as_ptr(), mode.as_ptr()) };
        let attr = CString::new("hardwaregain").unwrap();
        // SAFETY: see set_center_freq.
        let ret = unsafe { (self.api.channel_attr_write_longlong)(chn, attr.as_ptr(), gain as c_longlong) };
        check_iio(ret, "set_rf_gain")
    }

    /// `set_rx_channels_status` — enable/disable the cf-ad9361-lpc I/Q pair.
    fn set_rx_channels_status(&self, enable: bool) -> Result<(), DeviceError> {
        let i = self
            .find_channel("cf-ad9361-lpc", "voltage0", false)
            .ok_or_else(|| DeviceError::OpenFailed("cf-ad9361-lpc voltage0 not found".into()))?;
        let q = self
            .find_channel("cf-ad9361-lpc", "voltage1", false)
            .ok_or_else(|| DeviceError::OpenFailed("cf-ad9361-lpc voltage1 not found".into()))?;
        // SAFETY: channel handles from the live context; enable/disable take
        // no ownership.
        unsafe {
            if enable {
                (self.api.channel_enable)(i);
                (self.api.channel_enable)(q);
            } else {
                (self.api.channel_disable)(i);
                (self.api.channel_disable)(q);
            }
        }
        Ok(())
    }

    /// `set_tx_channels_status` — enable/disable the dds-core I/Q pair.
    fn set_tx_channels_status(&self, enable: bool) -> Result<(), DeviceError> {
        let i = self
            .find_channel("cf-ad9361-dds-core-lpc", "voltage0", true)
            .ok_or_else(|| DeviceError::OpenFailed("cf-ad9361-dds-core-lpc voltage0 not found".into()))?;
        let q = self
            .find_channel("cf-ad9361-dds-core-lpc", "voltage1", true)
            .ok_or_else(|| DeviceError::OpenFailed("cf-ad9361-dds-core-lpc voltage1 not found".into()))?;
        // SAFETY: see set_rx_channels_status.
        unsafe {
            if enable {
                (self.api.channel_enable)(i);
                (self.api.channel_enable)(q);
            } else {
                (self.api.channel_disable)(i);
                (self.api.channel_disable)(q);
            }
        }
        Ok(())
    }

    /// `plutosdr.setup_rx` — create the RX buffer of `buffer_size` samples.
    fn setup_rx(&mut self, buffer_size: usize) -> Result<(), DeviceError> {
        self.set_rx_channels_status(true)?;
        let c_dev = CString::new("cf-ad9361-lpc").unwrap();
        // SAFETY: `self.ctx` is a live context; the returned buffer handle is
        // owned by us and destroyed in `teardown_rx`.
        unsafe {
            let dev = (self.api.context_find_device)(self.ctx, c_dev.as_ptr());
            if dev.is_null() {
                return Err(DeviceError::OpenFailed("cf-ad9361-lpc not found".into()));
            }
            self.rx_buffer = (self.api.device_create_buffer)(dev, buffer_size, false);
        }
        if self.rx_buffer.is_null() {
            return Err(DeviceError::StreamError("iio_device_create_buffer (rx) failed".into()));
        }
        Ok(())
    }

    /// `plutosdr.setup_tx`.
    fn setup_tx(&mut self, buffer_size: usize) -> Result<(), DeviceError> {
        self.set_tx_channels_status(true)?;
        let c_dev = CString::new("cf-ad9361-dds-core-lpc").unwrap();
        // SAFETY: see setup_rx.
        unsafe {
            let dev = (self.api.context_find_device)(self.ctx, c_dev.as_ptr());
            if dev.is_null() {
                return Err(DeviceError::OpenFailed("cf-ad9361-dds-core-lpc not found".into()));
            }
            self.tx_buffer = (self.api.device_create_buffer)(dev, buffer_size, false);
        }
        if self.tx_buffer.is_null() {
            return Err(DeviceError::StreamError("iio_device_create_buffer (tx) failed".into()));
        }
        Ok(())
    }

    /// `plutosdr.send_sync` — fill the TX buffer with interleaved i16 and push.
    fn send_sync(&mut self, samples: &[crate::Complex32]) -> Result<(), DeviceError> {
        let chn = self
            .find_channel("cf-ad9361-dds-core-lpc", "voltage0", true)
            .ok_or_else(|| DeviceError::StreamError("tx channel lost".into()))?;
        // SAFETY: `self.tx_buffer` is a live buffer; the pointer walk writes
        // only within [first, end) with the buffer's own stride.
        unsafe {
            let mut p = (self.api.buffer_first)(self.tx_buffer, chn) as *mut i16;
            let end = (self.api.buffer_end)(self.tx_buffer) as *mut i16;
            let step =
                (self.api.buffer_step)(self.tx_buffer) / std::mem::size_of::<i16>() as isize;

            let flat: Vec<i16> = samples
                .iter()
                .flat_map(|c| {
                    let (i, q) = complex_to_raw_pair(*c);
                    [i, q]
                })
                .collect();

            let mut i = 0usize;
            while p < end {
                *p = flat.get(i).copied().unwrap_or(0);
                *p.offset(1) = flat.get(i + 1).copied().unwrap_or(0);
                p = p.offset(step);
                i += 2;
            }
            if (self.api.buffer_push)(self.tx_buffer) < 0 {
                return Err(DeviceError::StreamError("iio_buffer_push failed".into()));
            }
        }
        Ok(())
    }

    /// Stop the RX worker thread (if running) and join it.
    fn stop_rx_thread(&mut self) -> Result<(), DeviceError> {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.rx_thread.take() {
            let _ = t.join();
        }
        Ok(())
    }

    /// `plutosdr.stop_rx` — disable channels and destroy the RX buffer.
    fn teardown_rx(&mut self) -> Result<(), DeviceError> {
        self.stop_rx_thread()?;
        self.set_rx_channels_status(false)?;
        if !self.rx_buffer.is_null() {
            // SAFETY: buffer handle owned by us, created in setup_rx, not
            // used by any thread anymore (worker joined above).
            unsafe { (self.api.buffer_destroy)(self.rx_buffer) };
            self.rx_buffer = std::ptr::null_mut();
        }
        Ok(())
    }

    /// `plutosdr.stop_tx`.
    fn teardown_tx(&mut self) -> Result<(), DeviceError> {
        self.set_tx_channels_status(false)?;
        if !self.tx_buffer.is_null() {
            // SAFETY: buffer handle owned by us, created in setup_tx.
            unsafe { (self.api.buffer_destroy)(self.tx_buffer) };
            self.tx_buffer = std::ptr::null_mut();
        }
        Ok(())
    }

    /// `plutosdr.close` — full teardown, buffers + context.
    pub fn close(&mut self) -> Result<(), DeviceError> {
        self.teardown_rx()?;
        self.teardown_tx()?;
        if !self.ctx.is_null() {
            // SAFETY: context handle owned by us; all buffers were destroyed
            // above so no dangling libiio objects remain.
            unsafe { (self.api.context_destroy)(self.ctx) };
            self.ctx = std::ptr::null_mut();
        }
        Ok(())
    }

    /// Scan for PlutoSDR devices without opening one
    /// (`PlutoSDR.get_device_list`). Returns `"desc [uri]"` strings.
    pub fn scan() -> Result<Vec<String>, DeviceError> {
        let lib = load_library()?;
        let api = resolve_api(&lib)?;
        let (descs, uris) = Self::scan_with(&api)?;
        Ok(descs
            .into_iter()
            .zip(uris)
            .map(|(d, u)| format!("{d} [{u}]"))
            .collect())
    }
}

impl Device for PlutoSdrDevice {
    fn name(&self) -> &'static str {
        "PlutoSDR"
    }

    fn device_list() -> Vec<String> {
        Self::scan().unwrap_or_default()
    }

    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        match cmd {
            Command::SetFrequency => self.set_center_freq(value),
            Command::SetSampleRate => self.set_sample_rate(value),
            Command::SetBandwidth => self.set_bandwidth(value),
            Command::SetRfGain => self.set_rf_gain(value),
            // PlutoSDR.device_parameters only exposes the four commands
            // above; everything else is unsupported.
            _ => Err(DeviceError::Unsupported(
                "PlutoSDR: command not in device_parameters",
            )),
        }
    }

    fn start_rx(
        &mut self,
        config: &DeviceConfig,
        sink: std::sync::mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        self.set_parameter(Command::SetFrequency, config.frequency)?;
        self.set_parameter(Command::SetSampleRate, config.sample_rate)?;
        if config.bandwidth > 0.0 {
            self.set_parameter(Command::SetBandwidth, config.bandwidth)?;
        }
        if config.rf_gain != 0.0 {
            self.set_parameter(Command::SetRfGain, config.rf_gain)?;
        }

        self.setup_rx(SYNC_RX_CHUNK_SIZE)?;
        self.stop_flag.store(false, Ordering::SeqCst);

        let ctx = self.ctx as usize;
        let buffer = self.rx_buffer as usize;
        let stop = Arc::clone(&self.stop_flag);

        // The worker owns raw handles as plain integers (the parent struct is
        // !Send because of the raw pointers). The parent guarantees they stay
        // valid until it sets `stop` and joins this thread; the copied
        // function-pointer table is plain data.
        let api = ApiSend {
            context_find_device: self.api.context_find_device,
            device_find_channel: self.api.device_find_channel,
            buffer_refill: self.api.buffer_refill,
            buffer_step: self.api.buffer_step,
            buffer_first: self.api.buffer_first,
            buffer_end: self.api.buffer_end,
        };

        self.rx_thread = Some(std::thread::spawn(move || {
            // Re-derive the helpers on raw handles inside the thread.
            let ctx = ctx as *mut IioContext;
            let buffer = buffer as *mut IioBuffer;

            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                // SAFETY: `ctx`/`buffer` are the live context and RX buffer
                // created by the parent; the parent guarantees they stay
                // valid until it sets `stop` and joins this thread.
                unsafe {
                    if (api.buffer_refill)(buffer) < 0 {
                        break;
                    }
                    // Deviation from the .pyx: compute first/step/end
                    // *after* refill (the pyx reads stale pre-refill
                    // pointers).
                    let dev = (api.context_find_device)(ctx, c"cf-ad9361-lpc".as_ptr());
                    if dev.is_null() {
                        break;
                    }
                    let chn = (api.device_find_channel)(dev, c"voltage0".as_ptr(), false);
                    if chn.is_null() {
                        break;
                    }
                    let mut p = (api.buffer_first)(buffer, chn) as *const i16;
                    let end = (api.buffer_end)(buffer) as *const i16;
                    let step = (api.buffer_step)(buffer)
                        / std::mem::size_of::<i16>() as isize;
                    let mut chunk: IqChunk = Vec::new();
                    while p < end {
                        let i = *p;
                        let q = *p.offset(1);
                        chunk.push(raw_pair_to_complex(i, q));
                        p = p.offset(step);
                    }
                    if sink.send(chunk).is_err() {
                        break; // receiver gone — stop silently, never panic
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
        // Enter TX direction: the pyx sets IS_TX before applying parameters.
        self.teardown_rx()?; // leave RX mode if it was entered
        self.is_tx = true;
        self.set_center_freq(config.frequency)?;
        self.set_sample_rate(config.sample_rate)?;
        if config.bandwidth > 0.0 {
            self.set_bandwidth(config.bandwidth)?;
        }
        if config.rf_gain != 0.0 {
            self.set_rf_gain(config.rf_gain)?;
        }
        self.setup_tx(SYNC_TX_CHUNK_SIZE)?;
        self.send_sync(samples)?;
        self.teardown_tx()
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.is_tx = false;
        self.close()
    }
}

/// The function-pointer subset copied into the RX worker thread (fn pointers
/// are `Send` even though the parent struct is not).
struct ApiSend {
    context_find_device: unsafe extern "C" fn(*const IioContext, *const c_char) -> *mut IioDevice,
    device_find_channel: unsafe extern "C" fn(*const IioDevice, *const c_char, bool) -> *mut IioChannel,
    buffer_refill: unsafe extern "C" fn(*mut IioBuffer) -> isize,
    buffer_step: unsafe extern "C" fn(*const IioBuffer) -> isize,
    buffer_first: unsafe extern "C" fn(*const IioBuffer, *const IioChannel) -> *mut c_void,
    buffer_end: unsafe extern "C" fn(*const IioBuffer) -> *mut c_void,
}

impl Drop for PlutoSdrDevice {
    fn drop(&mut self) {
        let _ = self.close();
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
        assert_eq!(LIBRARY_NAMES[0], "libiio.0.dylib");
        assert!(LIBRARY_NAMES.contains(&"libiio.so.0"));
        assert!(LIBRARY_NAMES.iter().all(|n| n.contains("iio")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dll")));
    }

    #[test]
    fn open_without_libiio_is_library_unavailable() {
        let installed = LIBRARY_NAMES
            .iter()
            .any(|n| crate::devices::native::try_open(n).is_ok());
        // SAFETY: constant NUL-free names; plain dlopen probe.
        if !installed {
            match PlutoSdrDevice::open(None) {
                Err(DeviceError::LibraryUnavailable(_)) => {}
                _ => panic!("expected LibraryUnavailable"),
            }
        }
    }

    #[test]
    fn extract_uri_from_device_list_entry() {
        assert_eq!(
            extract_uri("Analog Devices PlutoSDR-0 [usb:1.5.5]"),
            "usb:1.5.5"
        );
        assert_eq!(extract_uri("ip:192.168.2.1"), "ip:192.168.2.1");
        assert_eq!(extract_uri(""), "");
        assert_eq!(extract_uri("no brackets ] here"), "no brackets ] here");
        assert_eq!(extract_uri("[]"), "[]");
    }

    #[test]
    fn raw_conversion_matches_bytes_to_iq() {
        // 12-bit ADC full scale: raw 2047 -> (2047 << 4) / 32768 ~ 0.9995
        let c = raw_pair_to_complex(2047, -2048);
        assert!((c.re - 32752.0 / 32768.0).abs() < 1e-6);
        assert!((c.im - (-32768.0 / 32768.0)).abs() < 1e-6);
        // saturation instead of numpy's wraparound on overflow
        let c = raw_pair_to_complex(4096, 0);
        assert!((c.re - 32767.0 / 32768.0).abs() < 1e-9);
        // TX inverse
        let (i, q) = complex_to_raw_pair(crate::Complex32::new(1.0, -1.0));
        assert_eq!((i, q), (2048, -2048));
        let (i, _) = complex_to_raw_pair(crate::Complex32::new(0.5, 0.0));
        assert_eq!(i, 1024);
    }

    #[test]
    fn scan_without_libiio_errors_cleanly() {
        let installed = LIBRARY_NAMES
            .iter()
            .any(|n| crate::devices::native::try_open(n).is_ok());
        // SAFETY: constant names; plain dlopen probe.
        if !installed {
            assert!(matches!(
                PlutoSdrDevice::scan(),
                Err(DeviceError::LibraryUnavailable(_))
            ));
        }
    }
}
