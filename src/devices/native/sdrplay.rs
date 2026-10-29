//! SDRPlay (RSP) backend — runtime dlopen binding to the SDRPlay **v3 API**
//! (`libsdrplay_api`, the `sdrplay_api_*` service-style interface).
//!
//! Port of the stevenjoezhang/urh fork's SDRPlay v3 API backend
//! (`sdrplay.pyx` / `csdrplay.pxd` / `SDRPlay.py`), which replaced the old
//! v2 `libmirsdrapi-rsp` (`mir_sdr_*`) binding. RX-only (the API has no TX).
//!
//! v3 is parameter-tree based: an explicit API session
//! (`sdrplay_api_Open` → ... → `sdrplay_api_Close`) wraps everything; a
//! device is selected with `sdrplay_api_SelectDevice` and configured by
//! writing into the tree returned by `sdrplay_api_GetDeviceParams`
//! (`devParams.fsFreq.fsHz`, `rxChannelA.tunerParams.*`,
//! `rxChannelA.rsp2TunerParams.*`), then applying changes with
//! `sdrplay_api_Update` using per-field reason bits. Streaming starts with
//! `sdrplay_api_Init` + a `sdrplay_api_CallbackFnsT` (stream A / stream B /
//! event callbacks); the Rust port forwards the interleaved `int16` xi/xq
//! buffers to the sink as normalized `Complex32` (divided by 32768.0).
//!
//! FFI declarations mirror the fork's `csdrplay.pxd` one-to-one (field
//! types, field order, `repr(C)`).

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_uint};

use crate::devices::{Command, Device, DeviceConfig, DeviceError, IqChunk};
use crate::Complex32;

/// Shared-library file names tried (in order) at open time. The v3 service
/// library first, the legacy v2 `libmirsdrapi-rsp` as a last-resort fallback
/// (its exports do not match the v3 ABI; only the v3 names are meaningful).
pub const LIBRARY_NAMES: &[&str] = &[
    // SDRPlay API v3 (sdrplay_api_*)
    "libsdrplay_api.so",
    "libsdrplay_api.dylib",
    "sdrplay_api.dll",
    // legacy v2 fallback names (won't provide the v3 symbols)
    "libmirsdrapi-rsp.so",
    "libmirsdrapi-rsp.dylib",
];

// ---------------------------------------------------------------------------
// sdrplay_api v3 C API (declarations follow csdrplay.pxd exactly)
// ---------------------------------------------------------------------------

/// `HANDLE` (void*).
type Handle = *mut c_void;

/// `sdrplay_api_TunerSelectT`.
const TUNER_A: c_int = 1;

/// `sdrplay_api_Rsp2_AmPortSelectT`.
const RSP2_AMPORT_1: i32 = 1; // Hi-Z
const RSP2_AMPORT_2: i32 = 0; // 50Ω A/B

/// `sdrplay_api_Rsp2_AntennaSelectT`.
const RSP2_ANTENNA_A: i32 = 5;
const RSP2_ANTENNA_B: i32 = 6;

/// `sdrplay_api_ReasonForUpdateT` bits (u32: the RspDuo_RfDabNotch bit is
/// 0x80000000 and does not fit in i32).
pub const UPDATE_DEV_FS: u32 = 0x0000_0001;
pub const UPDATE_RSP2_AM_PORT_SELECT: u32 = 0x0000_0100;
pub const UPDATE_RSP2_ANTENNA_CONTROL: u32 = 0x0000_0200;
pub const UPDATE_TUNER_GR: u32 = 0x0000_8000;
pub const UPDATE_TUNER_GR_LIMITS: u32 = 0x0001_0000;
pub const UPDATE_TUNER_FRF: u32 = 0x0002_0000;
pub const UPDATE_TUNER_BW_TYPE: u32 = 0x0004_0000;
pub const UPDATE_TUNER_IF_TYPE: u32 = 0x0008_0000;
pub const UPDATE_TUNER_LO_MODE: u32 = 0x0020_0000;

/// `sdrplay_api_ReasonForUpdateExtension1T`.
const UPDATE_EXT1_NONE: u32 = 0x0000_0000;

/// `sdrplay_api_MinGainReductionT`.
const EXTENDED_MIN_GR: i32 = 0;
const NORMAL_MIN_GR: i32 = 20;

/// `sdrplay_api_LoModeT`: LO_Auto = 1 (common default, sdrplay.pyx
/// init_stream).
const LO_AUTO: i32 = 1;

/// `sdrplay_api_ErrT`: sdrplay_api_Success = 0.
const API_SUCCESS: c_int = 0;
/// `sdrplay_api_ErrT`: sdrplay_api_NotInitialised = 10.
const API_NOT_INITIALISED: c_int = 10;
/// `sdrplay_api_ErrT`: sdrplay_api_OutOfRange = 3.
const API_OUT_OF_RANGE: c_int = 3;

/// `sdrplay_api_Bw_MHzT` (values are kHz).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BwMHz {
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

/// `sdrplay_api_If_kHzT`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfKHz {
    Undefined = -1,
    Zero = 0,
    Khz450 = 450,
    Khz1620 = 1620,
    Khz2048 = 2048,
}

/// `sdrplay_api_GainValuesT`.
#[repr(C)]
struct GainValuesT {
    curr: f32,
    max: f32,
    min: f32,
}

/// `sdrplay_api_GainT`.
#[repr(C)]
struct GainT {
    g_rdb: c_int,
    lna_state: u8,
    sync_update: u8,
    /// `sdrplay_api_MinGainReductionT` (enum → int)
    min_gr: i32,
    gain_vals: GainValuesT,
}

/// `sdrplay_api_RfFreqT`.
#[repr(C)]
struct RfFreqT {
    rf_hz: f64,
    sync_update: u8,
}

/// `sdrplay_api_DcOffsetTunerT`.
#[repr(C)]
struct DcOffsetTunerT {
    dc_cal: u8,
    speed_up: u8,
    track_time: c_int,
    refresh_rate_time: c_int,
}

/// `sdrplay_api_TunerParamsT` (bwType/ifType/loMode are enums → int).
#[repr(C)]
struct TunerParamsT {
    bw_type: i32,
    if_type: i32,
    lo_mode: i32,
    gain: GainT,
    rf_freq: RfFreqT,
    dc_offset_tuner: DcOffsetTunerT,
}

/// `sdrplay_api_AgcT` (enable is `sdrplay_api_AgcControlT`, enum → int).
#[repr(C)]
struct AgcT {
    enable: i32,
    set_point_dbfs: c_int,
    attack_ms: u16,
    decay_ms: u16,
    decay_delay_ms: u16,
    decay_threshold_db: u16,
    sync_update: c_int,
}

/// `sdrplay_api_DecimationT`.
#[repr(C)]
struct DecimationT {
    enable: u8,
    decimation_factor: u8,
    wide_band_signal: u8,
}

/// `sdrplay_api_DcOffsetT`.
#[repr(C)]
struct DcOffsetT {
    dc_enable: u8,
    iq_enable: u8,
}

/// `sdrplay_api_ControlParamsT` (adsbMode is enum → int).
#[repr(C)]
struct ControlParamsT {
    dc_offset: DcOffsetT,
    decimation: DecimationT,
    agc: AgcT,
    adsb_mode: i32,
}

/// `sdrplay_api_Rsp1aTunerParamsT`.
#[repr(C)]
struct Rsp1aTunerParamsT {
    bias_t_enable: u8,
}

/// `sdrplay_api_Rsp2TunerParamsT` (amPortSel/antennaSel are enums → int).
#[repr(C)]
struct Rsp2TunerParamsT {
    bias_t_enable: u8,
    am_port_sel: i32,
    antenna_sel: i32,
    rf_notch_enable: u8,
}

/// `sdrplay_api_RspDuo_ResetSlaveFlagsT`.
#[repr(C)]
struct RspDuoResetSlaveFlagsT {
    reset_gain_update: u8,
    reset_rf_update: u8,
}

/// `sdrplay_api_RspDuoTunerParamsT`.
#[repr(C)]
struct RspDuoTunerParamsT {
    bias_t_enable: u8,
    tuner1_am_port_sel: i32,
    tuner1_am_notch_enable: u8,
    rf_notch_enable: u8,
    rf_dab_notch_enable: u8,
    reset_slave_flags: RspDuoResetSlaveFlagsT,
}

/// `sdrplay_api_RspDxTunerParamsT` (hdrBw is enum → int).
#[repr(C)]
struct RspDxTunerParamsT {
    hdr_bw: i32,
}

/// `sdrplay_api_RxChannelParamsT`.
#[repr(C)]
struct RxChannelParamsT {
    tuner_params: TunerParamsT,
    ctrl_params: ControlParamsT,
    rsp1a_tuner_params: Rsp1aTunerParamsT,
    rsp2_tuner_params: Rsp2TunerParamsT,
    rsp_duo_tuner_params: RspDuoTunerParamsT,
    rsp_dx_tuner_params: RspDxTunerParamsT,
}

/// `sdrplay_api_FsFreqT`.
#[repr(C)]
struct FsFreqT {
    fs_hz: f64,
    sync_update: u8,
    re_cal: u8,
}

/// `sdrplay_api_SyncUpdateT`.
#[repr(C)]
struct SyncUpdateT {
    sample_num: c_uint,
    period: c_uint,
}

/// `sdrplay_api_ResetFlagsT`.
#[repr(C)]
struct ResetFlagsT {
    reset_gain_update: u8,
    reset_rf_update: u8,
    reset_fs_update: u8,
}

/// `sdrplay_api_Rsp1aParamsT`.
#[repr(C)]
struct Rsp1aParamsT {
    rf_notch_enable: u8,
    rf_dab_notch_enable: u8,
}

/// `sdrplay_api_Rsp2ParamsT`.
#[repr(C)]
struct Rsp2ParamsT {
    ext_ref_output_en: u8,
}

/// `sdrplay_api_RspDuoParamsT`.
#[repr(C)]
struct RspDuoParamsT {
    ext_ref_output_en: c_int,
}

/// `sdrplay_api_RspDxParamsT` (antennaSel is enum → int).
#[repr(C)]
struct RspDxParamsT {
    hdr_enable: u8,
    bias_t_enable: u8,
    antenna_sel: i32,
    rf_notch_enable: u8,
    rf_dab_notch_enable: u8,
}

/// `sdrplay_api_DevParamsT` (mode is `sdrplay_api_TransferModeT` → int).
#[repr(C)]
struct DevParamsT {
    ppm: f64,
    fs_freq: FsFreqT,
    sync_update: SyncUpdateT,
    reset_flags: ResetFlagsT,
    mode: i32,
    samples_per_pkt: c_uint,
    rsp1a_params: Rsp1aParamsT,
    rsp2_params: Rsp2ParamsT,
    rsp_duo_params: RspDuoParamsT,
    rsp_dx_params: RspDxParamsT,
}

/// `sdrplay_api_DeviceParamsT`.
#[repr(C)]
struct DeviceParamsT {
    dev_params: *mut DevParamsT,
    rx_channel_a: *mut RxChannelParamsT,
    rx_channel_b: *mut RxChannelParamsT,
}

/// `sdrplay_api_DeviceT`.
#[repr(C)]
struct DeviceT {
    ser_no: [c_char; 64],
    hw_ver: u8,
    /// `sdrplay_api_TunerSelectT`
    tuner: c_int,
    /// `sdrplay_api_RspDuoModeT`
    rsp_duo_mode: c_int,
    valid: u8,
    rsp_duo_sample_freq: f64,
    dev: Handle,
}

/// `sdrplay_api_StreamCbParamsT`.
#[repr(C)]
struct StreamCbParamsT {
    first_sample_num: c_uint,
    gr_changed: c_int,
    rf_changed: c_int,
    fs_changed: c_int,
    num_samples: c_uint,
}

/// `sdrplay_api_EventParamsT` (a union; only ever passed as a pointer, never
/// dereferenced — sized generously, alignment irrelevant for a by-pointer
/// parameter).
#[repr(C)]
struct EventParamsT {
    _opaque: [u8; 32],
}

/// `sdrplay_api_StreamCallback_t`.
type StreamCallback = unsafe extern "C" fn(
    xi: *mut i16,
    xq: *mut i16,
    params: *mut StreamCbParamsT,
    num_samples: c_uint,
    reset: c_uint,
    cb_context: *mut c_void,
);

/// `sdrplay_api_EventCallback_t` (eventId/tuner are enums → int).
type EventCallback = unsafe extern "C" fn(
    event_id: c_int,
    tuner: c_int,
    params: *mut EventParamsT,
    cb_context: *mut c_void,
);

/// `sdrplay_api_CallbackFnsT`.
#[repr(C)]
struct CallbackFnsT {
    stream_a_cb_fn: StreamCallback,
    stream_b_cb_fn: StreamCallback,
    event_cb_fn: EventCallback,
}

// function-pointer types for the runtime-resolved symbols

type FnOpen = unsafe extern "C" fn() -> c_int;
type FnClose = unsafe extern "C" fn() -> c_int;
type FnApiVersion = unsafe extern "C" fn(*mut f32) -> c_int;
type FnSelectDevice = unsafe extern "C" fn(*mut DeviceT) -> c_int;
type FnReleaseDevice = unsafe extern "C" fn(*mut DeviceT) -> c_int;
type FnInit = unsafe extern "C" fn(Handle, *mut CallbackFnsT, *mut c_void) -> c_int;
type FnUninit = unsafe extern "C" fn(Handle) -> c_int;
type FnGetDeviceParams = unsafe extern "C" fn(Handle, *mut *mut DeviceParamsT) -> c_int;
type FnGetDevices =
    unsafe extern "C" fn(*mut DeviceT, *mut c_uint, c_uint) -> c_int;
type FnUpdate =
    unsafe extern "C" fn(Handle, c_int, u32, u32) -> c_int;

/// `sdrplay_api_ErrT` names (SDRPlay.py error_codes + the v3-only codes from
/// csdrplay.pxd).
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
        14 => "SERVICE NOT RESPONDING".into(),
        15 => "START PENDING".into(),
        16 => "STOP PENDING".into(),
        17 => "INVALID MODE".into(),
        18..=23 => format!("FAILED VERIFICATION {}", code - 17),
        24 => "INVALID SERVICE VERSION".into(),
        other => format!("sdrplay_api error {other}"),
    }
}

// ---------------------------------------------------------------------------
// parameter tables from sdrplay.pyx
// ---------------------------------------------------------------------------

/// Bandwidths understood by `get_nearest_bandwidth` (Hz). v3 table incl. the
/// 7 MHz / 8 MHz entries.
pub const BANDWIDTHS: &[f64] = &[
    200e3, 300e3, 600e3, 1536e3, 5000e3, 6000e3, 7000e3, 8000e3,
];

/// IF modes understood by `get_nearest_if_mode` (kHz).
pub const IF_MODES: &[f64] = &[0.0, 450.0, 1620.0, 2048.0];

/// Port of `get_nearest_bandwidth`: pick the tabulated bandwidth closest to
/// the requested one and return its `sdrplay_api_Bw_MHzT` value.
pub fn nearest_bandwidth(bandwidth: f64) -> BwMHz {
    let mut best = BANDWIDTHS[0];
    for &bw in BANDWIDTHS.iter().skip(1) {
        if (bw - bandwidth).abs() < (best - bandwidth).abs() {
            best = bw;
        }
    }
    match best as i64 {
        200_000 => BwMHz::Bw200,
        300_000 => BwMHz::Bw300,
        600_000 => BwMHz::Bw600,
        1_536_000 => BwMHz::Bw1536,
        5_000_000 => BwMHz::Bw5000,
        6_000_000 => BwMHz::Bw6000,
        7_000_000 => BwMHz::Bw7000,
        8_000_000 => BwMHz::Bw8000,
        _ => BwMHz::Bw200,
    }
}

/// Port of `get_nearest_if_mode`: pick the tabulated IF closest to the
/// requested one and return its `sdrplay_api_If_kHzT` value.
pub fn nearest_if_mode(if_mode: f64) -> IfKHz {
    let mut best = IF_MODES[0];
    for &m in IF_MODES.iter().skip(1) {
        if (m - if_mode).abs() < (best - if_mode).abs() {
            best = m;
        }
    }
    match best as i32 {
        0 => IfKHz::Zero,
        450 => IfKHz::Khz450,
        1620 => IfKHz::Khz1620,
        2048 => IfKHz::Khz2048,
        _ => IfKHz::Zero,
    }
}

/// Port of `calculate_gain_reduction`: URH's gain is clamped to [20, 59] dB
/// and inverted into a gain reduction for the API (highest gain = lowest
/// gain reduction, offset 79).
pub fn gain_reduction(gain: f64) -> c_int {
    79 - gain.clamp(20.0, 59.0) as c_int
}

/// Port of the RSP2 antenna selection logic in `set_antenna` — pure part:
/// the amPortSel / antennaSel values and the OR-ed reason bits for
/// `sdrplay_api_Update`.
///
/// - antenna 0: 50Ω port A (AMPORT_2 + ANTENNA_A)
/// - antenna 1: 50Ω port B (AMPORT_2 + ANTENNA_B)
/// - antenna 2: Hi-Z port  (AMPORT_1 only)
pub fn antenna_update(
    antenna: i32,
) -> Result<(i32, Option<i32>, u32), DeviceError> {
    let mut reason: u32 = 0;
    if antenna == 0 || antenna == 1 {
        // 50Ω A/B: first switch back to AMPORT_2, then select A or B
        reason |= UPDATE_RSP2_AM_PORT_SELECT;
        let antenna_sel = if antenna == 0 {
            RSP2_ANTENNA_A
        } else {
            RSP2_ANTENNA_B
        };
        reason |= UPDATE_RSP2_ANTENNA_CONTROL;
        Ok((RSP2_AMPORT_2, Some(antenna_sel), reason))
    } else if antenna == 2 {
        // Hi-Z port
        reason |= UPDATE_RSP2_AM_PORT_SELECT;
        Ok((RSP2_AMPORT_1, None, reason))
    } else {
        Err(DeviceError::ParameterRejected(format!(
            "antenna index {antenna} not in 0..=2"
        )))
    }
}

// According to SDRPlay API v3 docs (sdrplay.pyx comment):
// Conditions for LIF down-conversion to be enabled for RSP1, RSP1a, RSP2a
// and RSPduo in single tuner mode:
//   (fsHz == 8192000) && (bwType == BW_1_536) && (ifType == IF_2_048)
//   (fsHz == 8000000) && (bwType == BW_1_536) && (ifType == IF_2_048)
//   (fsHz == 8000000) && (bwType == BW_5_000) && (ifType == IF_2_048)
//   (fsHz == 2000000) && (bwType <= BW_0_300) && (ifType == IF_0_450)
//   (fsHz == 2000000) && (bwType == BW_0_600) && (ifType == IF_0_450)
//   (fsHz == 6000000) && (bwType <= BW_1_536) && (ifType == IF_1_620)

// ---------------------------------------------------------------------------
// callback state
// ---------------------------------------------------------------------------

/// State shared with the C stream callback. Boxed and passed as `cbContext`
/// while the stream is alive; reclaimed on stop.
struct CbState {
    sink: std::sync::mpsc::Sender<IqChunk>,
    /// while true the stream callback drops its buffers (close_stream /
    /// sdrplay.pyx `reset_rx` flag, prevents use-after-free races at Uninit)
    reset_rx: std::sync::atomic::AtomicBool,
}

/// Port of `_stream_a_callback` / `__rx_stream_callback`: forward the
/// interleaved int16 samples, unless a reset was requested.
unsafe extern "C" fn stream_a_callback(
    xi: *mut i16,
    xq: *mut i16,
    _params: *mut StreamCbParamsT,
    num_samples: c_uint,
    _reset: c_uint,
    cb_context: *mut c_void,
) {
    // SAFETY: `cb_context` is the Box<CbState> pointer we passed to
    // sdrplay_api_Init; the box lives until sdrplay_api_Uninit has returned
    // and the stream callbacks have stopped (reset_rx gates the window right
    // before Uninit). This function must not unwind across the FFI boundary:
    // it contains no panicking operations (Vec allocation may abort under
    // OOM, which is the panic=abort behavior we accept).
    let state = &*(cb_context as *const CbState);
    if state
        .reset_rx
        .load(std::sync::atomic::Ordering::SeqCst)
    {
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

/// Port of `_stream_b_callback` (no-op: only tuner A is used).
unsafe extern "C" fn stream_b_callback(
    _xi: *mut i16,
    _xq: *mut i16,
    _params: *mut StreamCbParamsT,
    _num_samples: c_uint,
    _reset: c_uint,
    _cb_context: *mut c_void,
) {
}

/// Port of `_event_callback` (no-op in sdrplay.pyx).
unsafe extern "C" fn event_callback(
    _event_id: c_int,
    _tuner: c_int,
    _params: *mut EventParamsT,
    _cb_context: *mut c_void,
) {
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// An SDRPlay RSP receiver driven through the v3 service API (RX only).
pub struct SdrPlayDevice {
    lib: libloading::Library,
    /// sdrplay_api_Open succeeded (explicit API session)
    api_open: bool,
    /// the selected device (handle + hw version); valid until ReleaseDevice
    device: Option<DeviceT>,
    /// parameter tree of the selected device, owned by the API (valid until
    /// ReleaseDevice); null while no device is selected
    dev_params: *mut DeviceParamsT,
    /// hardware version of the selected device (1 = RSP1)
    hw_version: u8,
    /// callback context while the stream is alive
    cb_state: Option<Box<CbState>>,
}

// SAFETY: the raw `dev_params` pointer (and the HANDLE inside `device`) are
// only dereferenced from `&mut self` methods; the SDRPlay API itself is not
// thread-safe and the Device trait drives a device from one owner thread.
unsafe impl Send for SdrPlayDevice {}

/// Plain-data view of `sdrplay_api_DeviceT` (port of `get_devices` dicts).
#[derive(Debug, Clone)]
pub struct SdrPlayDeviceFields {
    pub serial: String,
    pub hw_version: u8,
}

impl SdrPlayDevice {
    /// dlopen the SDRPlay v3 API library. `identifier` is the device number
    /// (index into the device list) as a decimal string; when given, the full
    /// v3 setup from `SDRPlay.init_device` runs immediately: open_api →
    /// set_device_index → set_gr_mode_for_dev_model.
    pub fn open(identifier: Option<&str>) -> Result<Self, DeviceError> {
        let lib = load_library()?;
        let mut device = Self {
            lib,
            api_open: false,
            device: None,
            dev_params: std::ptr::null_mut(),
            hw_version: 0,
            cb_state: None,
        };
        let index: Option<c_uint> = match identifier {
            Some(id) => Some(
                id.trim().parse().map_err(|_| {
                    DeviceError::OpenFailed(format!(
                        "invalid SDRPlay device number {id:?}"
                    ))
                })?,
            ),
            None => None,
        };
        // SDRPlay.init_device: open the API session first
        device.open_api()?;
        if let Some(index) = index {
            device.set_device_index(index)?;
            // set_gr_mode_for_dev_model (return ignored by the fork's
            // init_device)
            let _ = device.set_gr_mode_for_dev_model(device.hw_version as i32);
        }
        Ok(device)
    }

    /// `sdrplay_api_ApiVersion` (port of `get_api_version`).
    pub fn api_version(&self) -> Result<f32, DeviceError> {
        unsafe {
            let f: libloading::Symbol<FnApiVersion> = self
                .lib
                .get(b"sdrplay_api_ApiVersion\0")
                .map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_ApiVersion: {e}")))?;
            let mut version = 0.0f32;
            // SAFETY: valid symbol; `version` is a valid out-pointer.
            let ret = f(&mut version);
            if ret != API_SUCCESS {
                return Err(DeviceError::OpenFailed(error_string(ret)));
            }
            Ok(version)
        }
    }

    /// Port of `open_api` / `close_api`: explicit v3 API session.
    fn open_api(&mut self) -> Result<(), DeviceError> {
        if self.api_open {
            return Ok(());
        }
        unsafe {
            let f: libloading::Symbol<FnOpen> = self
                .lib
                .get(b"sdrplay_api_Open\0")
                .map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_Open: {e}")))?;
            // SAFETY: valid symbol; no arguments.
            let ret = f();
            if ret != API_SUCCESS {
                return Err(DeviceError::OpenFailed(error_string(ret)));
            }
        }
        self.api_open = true;
        Ok(())
    }

    fn close_api(&mut self) -> Result<(), DeviceError> {
        if !self.api_open {
            return Ok(());
        }
        unsafe {
            let f: libloading::Symbol<FnClose> = self
                .lib
                .get(b"sdrplay_api_Close\0")
                .map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_Close: {e}")))?;
            // SAFETY: valid symbol; no arguments.
            let ret = f();
            if ret != API_SUCCESS {
                return Err(DeviceError::StreamError(error_string(ret)));
            }
        }
        self.api_open = false;
        Ok(())
    }

    /// Port of `set_device_index`: GetDevices (16 slots) → bounds check →
    /// SelectDevice → GetDeviceParams. On GetDeviceParams failure the device
    /// is released again (as in the fork).
    fn set_device_index(&mut self, index: c_uint) -> Result<(), DeviceError> {
        unsafe {
            let get_devices: libloading::Symbol<FnGetDevices> = self
                .lib
                .get(b"sdrplay_api_GetDevices\0")
                .map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_GetDevices: {e}")))?;
            // sdrplay.pyx allocates 16 device slots
            let mut devs: Vec<DeviceT> = Vec::with_capacity(16);
            let mut num_devs: c_uint = 0;
            // SAFETY: valid symbol; `devs` provides 16 uninitialized
            // sdrplay_api_DeviceT slots, and we only read the first
            // `num_devs` entries which the API fully initializes.
            let ret = get_devices(devs.as_mut_ptr(), &mut num_devs, 16);
            if ret != API_SUCCESS {
                return Err(DeviceError::OpenFailed(error_string(ret)));
            }
            if index >= num_devs {
                return Err(DeviceError::OpenFailed(error_string(API_OUT_OF_RANGE)));
            }
            devs.set_len(num_devs as usize);
            let mut dev = devs.remove(index as usize);

            let select: libloading::Symbol<FnSelectDevice> = self
                .lib
                .get(b"sdrplay_api_SelectDevice\0")
                .map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_SelectDevice: {e}")))?;
            // SAFETY: valid symbol; `dev` is a valid sdrplay_api_DeviceT
            // that the API fills with the device handle.
            let ret = select(&mut dev);
            if ret != API_SUCCESS {
                return Err(DeviceError::OpenFailed(error_string(ret)));
            }

            let get_params: libloading::Symbol<FnGetDeviceParams> = self.lib.get(
                b"sdrplay_api_GetDeviceParams\0",
            ).map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_GetDeviceParams: {e}")))?;
            let mut params: *mut DeviceParamsT = std::ptr::null_mut();
            // SAFETY: valid symbol; `dev.dev` is the handle returned by
            // SelectDevice; `params` is a valid out-pointer receiving the
            // API-owned parameter tree.
            let ret = get_params(dev.dev, &mut params);
            if ret != API_SUCCESS {
                // release the device again to avoid occupying it (fork)
                if let Ok(release) = self.lib.get::<FnReleaseDevice>(b"sdrplay_api_ReleaseDevice\0") {
                    // SAFETY: valid symbol; `dev` was successfully selected.
                    let _ = release(&mut dev);
                }
                return Err(DeviceError::OpenFailed(error_string(ret)));
            }

            self.hw_version = dev.hw_ver;
            self.dev_params = params;
            self.device = Some(dev);
        }
        Ok(())
    }

    /// Port of `release_device_index` (`sdrplay_api_ReleaseDevice`).
    fn release_device(&mut self) -> Result<(), DeviceError> {
        let mut dev = match self.device.take() {
            Some(d) => d,
            None => return Ok(()),
        };
        self.dev_params = std::ptr::null_mut();
        self.hw_version = 0;
        unsafe {
            let f: libloading::Symbol<FnReleaseDevice> = self.lib.get(
                b"sdrplay_api_ReleaseDevice\0",
            ).map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_ReleaseDevice: {e}")))?;
            // SAFETY: valid symbol; `dev` is the previously selected device.
            let ret = f(&mut dev);
            if ret != API_SUCCESS {
                return Err(DeviceError::StreamError(error_string(ret)));
            }
        }
        Ok(())
    }

    /// Port of `set_gr_mode_for_dev_model`: RSP1 (dev_model == 1) gets the
    /// extended gain range (EXTENDED_MIN_GR), everything else the normal one
    /// (NORMAL_MIN_GR); applied via Update with the GrLimits reason.
    fn set_gr_mode_for_dev_model(&mut self, dev_model: i32) -> Result<(), DeviceError> {
        let (_, params) = self.selected()?;
        unsafe {
            // SAFETY: `params` is the API-owned parameter tree of the
            // selected device (valid until ReleaseDevice); rxChannelA is a
            // valid sub-struct for every device.
            (*(*params).rx_channel_a)
                .tuner_params
                .gain
                .min_gr = if dev_model == 1 {
                EXTENDED_MIN_GR
            } else {
                NORMAL_MIN_GR
            };
        }
        self.update(UPDATE_TUNER_GR_LIMITS)
    }

    /// The selected device handle + parameter tree, or NotInitialised.
    fn selected(&self) -> Result<(Handle, *mut DeviceParamsT), DeviceError> {
        match self.device {
            Some(ref d) if !d.dev.is_null() && !self.dev_params.is_null() => {
                Ok((d.dev, self.dev_params))
            }
            _ => Err(DeviceError::ParameterRejected(error_string(
                API_NOT_INITIALISED,
            ))),
        }
    }

    /// `sdrplay_api_Update` with Ext1 = None and tuner A.
    fn update(&mut self, reason: u32) -> Result<(), DeviceError> {
        let (dev, _) = self.selected()?;
        unsafe {
            let f: libloading::Symbol<FnUpdate> = self
                .lib
                .get(b"sdrplay_api_Update\0")
                .map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_Update: {e}")))?;
            // SAFETY: valid symbol; `dev` is the selected device handle.
            let ret = f(dev, TUNER_A, reason, UPDATE_EXT1_NONE);
            if ret != API_SUCCESS {
                return Err(DeviceError::ParameterRejected(error_string(ret)));
            }
        }
        Ok(())
    }

    /// `sdrplay_api_GetDevices` without selecting anything (port of
    /// `get_devices`; used by `device_list`, which wraps it in
    /// open_api/close_api).
    fn raw_devices(lib: &libloading::Library) -> Vec<SdrPlayDeviceFields> {
        unsafe {
            let f: libloading::Symbol<FnGetDevices> = match lib.get(b"sdrplay_api_GetDevices\0") {
                Ok(f) => f,
                Err(_) => return vec![],
            };
            let mut devs: Vec<DeviceT> = Vec::with_capacity(16);
            let mut num_devs: c_uint = 0;
            // SAFETY: as in set_device_index — 16 slots, only the first
            // `num_devs` (fully initialized) entries are read.
            let ret = f(devs.as_mut_ptr(), &mut num_devs, 16);
            if ret != API_SUCCESS {
                return vec![];
            }
            devs.set_len(num_devs as usize);
            devs.into_iter()
                .map(|d| SdrPlayDeviceFields {
                    serial: buffer_to_string(&d.ser_no),
                    hw_version: d.hw_ver,
                })
                .collect()
        }
    }

    /// Port of `init_stream`: write the initial parameter tree (v3 units are
    /// Hz, not MHz) and start streaming with `sdrplay_api_Init`.
    fn init_stream(
        &mut self,
        gain: f64,
        sample_rate: f64,
        center_freq: f64,
        bandwidth: f64,
        if_mode: f64,
        sink: std::sync::mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        let (_, params) = self.selected()?;
        let bw_type = nearest_bandwidth(bandwidth);
        let if_type = nearest_if_mode(if_mode);
        // gain: map UI "gain" to gRdB (IF attenuation, the larger the value
        // the smaller the gain)
        let g_rdb = gain_reduction(gain);

        unsafe {
            // SAFETY: `params` is the API-owned parameter tree of the
            // selected device; all writes below stay within the declared
            // sub-structs. The tree is applied by sdrplay_api_Init right
            // after, exactly as in sdrplay.pyx init_stream.
            (*(*params).dev_params).fs_freq.fs_hz = sample_rate;
            let ch = &mut *(*params).rx_channel_a;
            ch.tuner_params.rf_freq.rf_hz = center_freq;
            ch.tuner_params.bw_type = bw_type as i32;
            ch.tuner_params.if_type = if_type as i32;
            ch.tuner_params.lo_mode = LO_AUTO; // common default
            ch.tuner_params.gain.g_rdb = g_rdb;
            ch.tuner_params.gain.lna_state = 0;
            // minGr follows the device model (set_gr_mode_for_dev_model):
            // RSP1 gets the extended gain range.
            ch.tuner_params.gain.min_gr = if self.hw_version == 1 {
                EXTENDED_MIN_GR
            } else {
                NORMAL_MIN_GR
            };
        }

        let state = Box::new(CbState {
            sink,
            reset_rx: std::sync::atomic::AtomicBool::new(false),
        });
        let ctx = &*state as *const CbState as *mut c_void;
        let ret = unsafe {
            let f: libloading::Symbol<FnInit> = self
                .lib
                .get(b"sdrplay_api_Init\0")
                .map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_Init: {e}")))?;
            let mut cbs = CallbackFnsT {
                stream_a_cb_fn: stream_a_callback,
                stream_b_cb_fn: stream_b_callback,
                event_cb_fn: event_callback,
            };
            let (dev, _) = self.selected()?;
            // SAFETY: valid symbol; `dev` is the selected device handle,
            // `cbs` is a fully initialized CallbackFnsT for the duration of
            // the call (the API copies it), and `ctx` points to the
            // Box<CbState> that we store in self and keep alive until after
            // sdrplay_api_Uninit has returned.
            f(dev, &mut cbs, ctx)
        };
        if ret != API_SUCCESS {
            return Err(DeviceError::StreamError(error_string(ret)));
        }
        self.cb_state = Some(state);
        Ok(())
    }

    /// Port of `close_stream`: set the reset_rx flag (stops the callback
    /// from touching the sink during teardown) then `sdrplay_api_Uninit`.
    fn close_stream(&mut self) -> Result<(), DeviceError> {
        let (dev, _) = self.selected()?;
        if let Some(state) = self.cb_state.as_deref() {
            state
                .reset_rx
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let ret = unsafe {
            let f: libloading::Symbol<FnUninit> = self
                .lib
                .get(b"sdrplay_api_Uninit\0")
                .map_err(|e| DeviceError::OpenFailed(format!("sdrplay_api_Uninit: {e}")))?;
            // SAFETY: valid symbol; `dev` is the selected device handle.
            f(dev)
        };
        // Uninit has returned: stream callbacks have stopped, the callback
        // context can be reclaimed.
        self.cb_state = None;
        if ret != API_SUCCESS {
            return Err(DeviceError::StreamError(error_string(ret)));
        }
        Ok(())
    }

    /// Port of `set_center_freq`.
    fn set_center_freq(&mut self, frequency: f64) -> Result<(), DeviceError> {
        let (_, params) = self.selected()?;
        unsafe {
            // SAFETY: API-owned parameter tree of the selected device.
            (*(*params).rx_channel_a).tuner_params.rf_freq.rf_hz = frequency;
        }
        self.update(UPDATE_TUNER_FRF)
    }

    /// Port of `set_sample_rate`.
    fn set_sample_rate(&mut self, sample_rate: f64) -> Result<(), DeviceError> {
        let (_, params) = self.selected()?;
        unsafe {
            // SAFETY: API-owned parameter tree of the selected device.
            (*(*params).dev_params).fs_freq.fs_hz = sample_rate;
        }
        self.update(UPDATE_DEV_FS)
    }

    /// Port of `set_bandwidth`.
    fn set_bandwidth(&mut self, bandwidth: f64) -> Result<(), DeviceError> {
        let (_, params) = self.selected()?;
        unsafe {
            // SAFETY: API-owned parameter tree of the selected device.
            (*(*params).rx_channel_a).tuner_params.bw_type =
                nearest_bandwidth(bandwidth) as i32;
        }
        self.update(UPDATE_TUNER_BW_TYPE)
    }

    /// Port of `set_gain`.
    fn set_gain(&mut self, gain: f64) -> Result<(), DeviceError> {
        let (_, params) = self.selected()?;
        unsafe {
            // SAFETY: API-owned parameter tree of the selected device.
            (*(*params).rx_channel_a).tuner_params.gain.g_rdb =
                gain_reduction(gain);
        }
        self.update(UPDATE_TUNER_GR)
    }

    /// Port of `set_if_gain` (v3: this selects the IF mode, not a gain).
    fn set_if_gain(&mut self, if_mode: f64) -> Result<(), DeviceError> {
        let (_, params) = self.selected()?;
        unsafe {
            // SAFETY: API-owned parameter tree of the selected device.
            (*(*params).rx_channel_a).tuner_params.if_type =
                nearest_if_mode(if_mode) as i32;
        }
        self.update(UPDATE_TUNER_IF_TYPE)
    }

    /// Port of `set_antenna` (RSP2 three-port: 50Ω A, 50Ω B, Hi-Z).
    fn set_antenna(&mut self, antenna: i32) -> Result<(), DeviceError> {
        let (_, params) = self.selected()?;
        let (am_port, antenna_sel, reason) = antenna_update(antenna)?;
        unsafe {
            // SAFETY: API-owned parameter tree of the selected device; the
            // rsp2TunerParams sub-struct is only meaningful on RSP2 (the
            // fork also writes it unconditionally when the antenna command
            // arrives).
            let rsp2 = &mut (*(*params).rx_channel_a).rsp2_tuner_params;
            rsp2.am_port_sel = am_port;
            if let Some(sel) = antenna_sel {
                rsp2.antenna_sel = sel;
            }
        }
        self.update(reason)
    }
}

/// Decode a fixed-size NUL-terminated C char buffer (`SerNo[64]`).
fn buffer_to_string(buf: &[c_char]) -> String {
    let bytes = unsafe {
        // SAFETY: c_char == i8 on all supported targets; the slice is a
        // fixed 64-byte array inside a DeviceT we own.
        std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len())
    };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    // fork decodes with iso-8859-1 (lossless byte→char)
    bytes[..end].iter().map(|&b| b as char).collect()
}

impl Device for SdrPlayDevice {
    fn name(&self) -> &'static str {
        "SDRPlay"
    }

    /// "RSP {hw_version} ({serial})" entries (SDRPlay.get_device_list: the
    /// API session is opened and closed around the enumeration).
    fn device_list() -> Vec<String> {
        let lib = match load_library() {
            Ok(l) => l,
            Err(_) => return vec![],
        };
        if unsafe {
            // SAFETY: probe-load of the API session; symbol looked up by
            // name and only called if present.
            match lib.get::<FnOpen>(b"sdrplay_api_Open\0") {
                Ok(f) => f() == API_SUCCESS,
                Err(_) => false,
            }
        } {
            let devs = Self::raw_devices(&lib);
            unsafe {
                // SAFETY: symbol checked by name; closing the session we
                // opened above.
                if let Ok(f) = lib.get::<FnClose>(b"sdrplay_api_Close\0") {
                    let _ = f();
                }
            }
            devs.into_iter()
                .map(|d| format!("RSP {} ({})", d.hw_version, d.serial))
                .collect()
        } else {
            vec![]
        }
    }

    /// DEVICE_METHODS mapping from SDRPlay.py:
    /// SET_RF_GAIN → set_gain, SET_IF_GAIN → set_if_gain,
    /// SET_ANTENNA_INDEX → set_antenna (plus the generic
    /// frequency/sample-rate/bandwidth setters).
    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        match cmd {
            Command::SetFrequency => self.set_center_freq(value),
            Command::SetSampleRate => self.set_sample_rate(value),
            Command::SetBandwidth => self.set_bandwidth(value),
            Command::SetRfGain => self.set_gain(value),
            Command::SetIfGain => self.set_if_gain(value),
            Command::SetAntennaIndex => self.set_antenna(value as i32),
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
        // SDRPlay needs an explicit API session and device index (fork:
        // init_device); default to the first device when none was given.
        self.open_api()?;
        if self.device.is_none() {
            self.set_device_index(0)?;
            let _ = self.set_gr_mode_for_dev_model(self.hw_version as i32);
        }
        // init_device applies the antenna selection for RSP2 devices only
        if self.hw_version == 2 && (0..=2).contains(&config.antenna_index) {
            self.set_antenna(config.antenna_index)?;
        }
        // enter_async_receive_mode → init_stream
        self.init_stream(
            config.rf_gain,
            config.sample_rate,
            config.frequency,
            config.bandwidth,
            config.if_gain,
            sink,
        )
    }

    fn send(&mut self, _config: &DeviceConfig, _samples: &[Complex32]) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported("SDRPlay is receive-only"))
    }

    /// shutdown_device in SDRPlay.py: close_stream → release_device_index →
    /// close_api (the full v3 chain).
    fn stop(&mut self) -> Result<(), DeviceError> {
        if self.device.is_some() {
            self.close_stream()?;
            self.release_device()?;
        }
        self.close_api()?;
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
    fn library_names_prefer_v3_api() {
        assert!(LIBRARY_NAMES[0].contains("sdrplay_api"));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".so")));
        assert!(LIBRARY_NAMES.iter().any(|n| n.ends_with(".dll")));
        // legacy v2 fallback present
        assert!(LIBRARY_NAMES.iter().any(|n| n.contains("mirsdrapi-rsp")));
        // v3 names come before the v2 fallback
        let v3 = LIBRARY_NAMES
            .iter()
            .position(|n| n.contains("sdrplay_api"))
            .unwrap();
        let v2 = LIBRARY_NAMES
            .iter()
            .position(|n| n.contains("mirsdrapi-rsp"))
            .unwrap();
        assert!(v3 < v2);
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
        if lib_present() {
            // The v3 library is installed and its service may be running:
            // sdrplay_api_Open talks to the real service (and can block with
            // no RSP attached) — real hardware territory, skip.
            return;
        }
        assert!(SdrPlayDevice::device_list().is_empty());
    }

    /// get_nearest_bandwidth table from sdrplay.pyx (v3, incl. 7/8 MHz).
    #[test]
    fn nearest_bandwidth_matches_pyx() {
        assert_eq!(nearest_bandwidth(200e3), BwMHz::Bw200);
        assert_eq!(nearest_bandwidth(250e3), BwMHz::Bw200); // 200 vs 300 equidistant -> keeps first (200)
        assert_eq!(nearest_bandwidth(1e6), BwMHz::Bw600);
        assert_eq!(nearest_bandwidth(2e6), BwMHz::Bw1536);
        assert_eq!(nearest_bandwidth(4e6), BwMHz::Bw5000);
        assert_eq!(nearest_bandwidth(6.1e6), BwMHz::Bw6000);
        assert_eq!(nearest_bandwidth(7.4e6), BwMHz::Bw7000);
        assert_eq!(nearest_bandwidth(100e9), BwMHz::Bw8000);
        // below the table the first entry wins (pyx best_match starts at 0)
        assert_eq!(nearest_bandwidth(1e3), BwMHz::Bw200);
    }

    /// get_nearest_if_mode table from sdrplay.pyx.
    #[test]
    fn nearest_if_mode_matches_pyx() {
        assert_eq!(nearest_if_mode(0.0), IfKHz::Zero);
        assert_eq!(nearest_if_mode(100.0), IfKHz::Zero); // closer to 0 than 450
        assert_eq!(nearest_if_mode(500.0), IfKHz::Khz450);
        assert_eq!(nearest_if_mode(1600.0), IfKHz::Khz1620);
        assert_eq!(nearest_if_mode(2049.0), IfKHz::Khz2048);
        assert_eq!(nearest_if_mode(1e9), IfKHz::Khz2048);
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

    /// set_antenna reason bits from sdrplay.pyx (RSP2 three ports).
    #[test]
    fn antenna_update_reason_bits_match_pyx() {
        // 50Ω A: AMPORT_2 + ANTENNA_A, both reason bits
        let (am, sel, reason) = antenna_update(0).unwrap();
        assert_eq!(am, RSP2_AMPORT_2);
        assert_eq!(sel, Some(RSP2_ANTENNA_A));
        assert_eq!(
            reason,
            UPDATE_RSP2_AM_PORT_SELECT | UPDATE_RSP2_ANTENNA_CONTROL
        );

        // 50Ω B: AMPORT_2 + ANTENNA_B, both reason bits
        let (am, sel, reason) = antenna_update(1).unwrap();
        assert_eq!(am, RSP2_AMPORT_2);
        assert_eq!(sel, Some(RSP2_ANTENNA_B));
        assert_eq!(
            reason,
            UPDATE_RSP2_AM_PORT_SELECT | UPDATE_RSP2_ANTENNA_CONTROL
        );

        // Hi-Z: AMPORT_1 only, just the am-port reason bit
        let (am, sel, reason) = antenna_update(2).unwrap();
        assert_eq!(am, RSP2_AMPORT_1);
        assert_eq!(sel, None);
        assert_eq!(reason, UPDATE_RSP2_AM_PORT_SELECT);

        // out of range
        assert!(antenna_update(3).is_err());
        assert!(antenna_update(-1).is_err());
    }

    /// reason-bit constants from csdrplay.pxd (spot checks).
    #[test]
    fn reason_for_update_constants_match_pxd() {
        assert_eq!(UPDATE_DEV_FS, 0x0000_0001);
        assert_eq!(UPDATE_RSP2_AM_PORT_SELECT, 0x0000_0100);
        assert_eq!(UPDATE_RSP2_ANTENNA_CONTROL, 0x0000_0200);
        assert_eq!(UPDATE_TUNER_GR, 0x0000_8000);
        assert_eq!(UPDATE_TUNER_GR_LIMITS, 0x0001_0000);
        assert_eq!(UPDATE_TUNER_FRF, 0x0002_0000);
        assert_eq!(UPDATE_TUNER_BW_TYPE, 0x0004_0000);
        assert_eq!(UPDATE_TUNER_IF_TYPE, 0x0008_0000);
        assert_eq!(UPDATE_TUNER_LO_MODE, 0x0020_0000);
        assert_eq!(EXTENDED_MIN_GR, 0);
        assert_eq!(NORMAL_MIN_GR, 20);
        assert_eq!(RSP2_ANTENNA_A, 5);
        assert_eq!(RSP2_ANTENNA_B, 6);
        assert_eq!(RSP2_AMPORT_1, 1);
        assert_eq!(RSP2_AMPORT_2, 0);
    }

    /// sdrplay_api_ErrT names (SDRPlay.py table + v3-only codes).
    #[test]
    fn error_string_matches_tables() {
        assert_eq!(error_string(0), "SUCCESS");
        assert_eq!(error_string(3), "OUT OF RANGE");
        assert_eq!(error_string(9), "ALREADY INITIALIZED");
        assert_eq!(error_string(10), "NOT INITIALIZED");
        assert_eq!(error_string(13), "OUT OF MEMORY ERROR");
        assert_eq!(error_string(14), "SERVICE NOT RESPONDING");
        assert_eq!(error_string(24), "INVALID SERVICE VERSION");
        assert_eq!(error_string(18), "FAILED VERIFICATION 1");
    }

    /// SerNo decoding: NUL-terminated fixed buffer, iso-8859-1 style.
    #[test]
    fn buffer_to_string_decodes_serno() {
        let mut buf = [0 as c_char; 64];
        for (i, b) in b"ABC1234".iter().enumerate() {
            buf[i] = *b as c_char;
        }
        assert_eq!(buffer_to_string(&buf), "ABC1234");
        let empty = [0 as c_char; 64];
        assert_eq!(buffer_to_string(&empty), "");
        // high bytes survive as latin-1 chars (fork decodes iso-8859-1)
        let mut hi = [0 as c_char; 64];
        hi[0] = 0xE9u8 as c_char;
        assert_eq!(buffer_to_string(&hi), "é");
    }

    #[test]
    #[ignore = "requires libsdrplay_api and a connected RSP"]
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
