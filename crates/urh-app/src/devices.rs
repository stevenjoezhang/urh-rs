//! Device capture service — v2 contract section "Device capture (v2, P3)"
//! (docs/ipc-contract.md).
//!
//! # What this module implements
//!
//! [`SignalRepository`] gains the four contract methods: [`list_devices`],
//! [`start_capture`], [`stop_capture`] and [`capture_stats`], plus the wire
//! types [`DeviceInfo`], [`CaptureConfig`], [`CaptureHandle`] and
//! [`CaptureStats`]. Sessions are identified by `"cap-<n>"` ids allocated
//! from the repository's shared counter; recordings are registered as
//! `"rec-<n>"` signals when the capture stops.
//!
//! [`list_devices`]: SignalRepository::list_devices
//! [`start_capture`]: SignalRepository::start_capture
//! [`stop_capture`]: SignalRepository::stop_capture
//! [`capture_stats`]: SignalRepository::capture_stats
//!
//! # Architecture
//!
//! One capture spawns three threads (the `Device` trait is synchronous and
//! its implementations are not `Send`, so the device is opened, driven and
//! closed *inside* its own thread and never crosses a thread boundary):
//!
//! ```text
//! device thread ──open, start_rx──▶ device channel (std mpsc, unbounded)
//!   parks until stop, then stop()         │
//!                                         ▼
//!                                  pump thread ──re-batches to 4096-sample
//!                                    │   │     batches, flushes the tail
//!                    preview queue   │   │  record queue
//!                    (bounded 16,    │   │  (bounded 4096, overflow ⇒ stop)
//!                     drop-oldest)   │   │
//!                          ▼         │   ▼
//!                    Tauri shell ◀──┘  recorder thread ──▶ recorded buffer
//!                    (preview events)   (registered as "rec-<n>" on stop)
//! ```
//!
//! The unbounded device channel mirrors what the `Device` trait expects
//! (backends push chunks from vendor callbacks that must never block); all
//! bounding happens in the pump, which is the only writer to both bounded
//! queues. The pump exits when the device channel closes (device stopped or
//! the backend's stream ended on its own) and then closes both queues, which
//! is what wakes the preview consumer and the recorder.
//!
//! # Batch forwarding to the Tauri shell
//!
//! The contract fixes the *wire events* (`capture://batch` with
//! `{capture_id, seq}` plus an ArrayBuffer body of interleaved LE f32 I/Q,
//! and `capture://stats` at ~1 Hz) but not how the shell obtains the batches.
//! This module exposes a single-consumer receiver instead:
//!
//! - [`SignalRepository::take_preview_receiver`] hands out the one
//!   [`PreviewReceiver`] of a capture (first call wins; `None` for an unknown
//!   id or an already-taken receiver);
//! - the shell runs one poller thread per capture that loops over
//!   [`PreviewReceiver::recv_timeout`]: `Ok(Some(batch))` emits
//!   `capture://batch` with the batch's `seq` and `body` (already
//!   interleaved LE f32 — forward verbatim); `Ok(None)` is a poll timeout;
//!   `Err(_)` ([`PreviewClosed`]) means the capture ended and the queue is
//!   drained — end the poller;
//! - the shell's ~1 Hz timer calls [`SignalRepository::capture_stats`] and
//!   emits `capture://stats` with the JSON body.
//!
//! The receiver's result shapes are chosen so callers outside the crate
//! never need to name a type from this module (the v2 modules are private):
//! they match `Ok(Some(b))` / `Ok(None)` / `Err(_)`, hold the batch via
//! inference, and read its `seq`/`body` fields.
//!
//! The receiver is deadlock-free by construction: it only ever takes the
//! preview queue's own mutex (released while waiting), never the session
//! registry lock, and never joins a thread.
//!
//! # Queue discipline (contract, hard requirements)
//!
//! 1. Preview queue: bounded, default [`PREVIEW_QUEUE_CAPACITY`] = 16 batches
//!    of [`PREVIEW_BATCH_SAMPLES`] = 4096 samples. On overflow the **oldest**
//!    batch is dropped and `dropped_preview_batches` increments; each batch
//!    carries a `seq` starting at 0 so the frontend can detect gaps.
//!    Preview is display-only and may lose data silently.
//! 2. Record queue: bounded, default [`RECORD_QUEUE_CAPACITY`] = 4096
//!    batches. On overflow the capture sets `overflow_detected = true` and
//!    **stops** (the stop flag wakes the device thread); recorded samples are
//!    never silently dropped. Batches produced after the overflow during
//!    wind-down are not recorded — the loss is already declared and flagged.
//! 3. On `stop_capture` the device thread is joined (it stops the hardware),
//!    then the pump (it drains the device channel and flushes a possible
//!    sub-batch tail into the record queue — the tail is *not* sent to
//!    preview, whose batches must stay >= 4096 samples), then the recorder;
//!    the recorded buffer is registered in the repository as `"rec-<n>"` with
//!    the capture's sample rate, and [`CaptureStats`] is returned. With
//!    `record = false` no signal is registered, the handle's `signal_id` is
//!    `None` and `recorded_samples` is 0.
//!
//! # Error mapping
//!
//! | `DeviceError` | `AppError` code |
//! |---|---|
//! | `LibraryUnavailable` | `invalid-argument`, message `"device unavailable: …"` |
//! | `Unsupported` | `invalid-argument`, message `"device unavailable: …"` |
//! | `OpenFailed`, `ParameterRejected`, `StreamError`, `Io` | `io` |
//!
//! `OpenFailed`/`ParameterRejected`/`StreamError` are runtime failures of
//! open hardware (v2 maps them all to `io`; a rejected parameter arriving as
//! `io` still carries the backend's message). Unknown devices and unknown
//! capture ids are `invalid-argument`. No error codes beyond the contract's
//! set are introduced.
//!
//! # Session registry
//!
//! `SignalRepository` cannot gain fields, so live captures live in a
//! process-wide static registry ([`captures`]) — the same tradeoff as the
//! analysis job registry: the registry is shared by every repository
//! instance in the process. Capture ids come from each repository's own
//! counter, so they are only unique per repository; the desktop app holds
//! exactly one repository, and `start_capture` refuses an id collision with
//! a live session instead of silently replacing it. Lock discipline: the
//! registry lock is only held for map operations; threads are joined and
//! channels read with the lock released.
//!
//! # Caveats
//!
//! - `rtl-tcp`: `RtlTcpDevice::connect` uses the OS connect timeout, so
//!   `start_capture` can block for that long before the connection error
//!   surfaces (the contract only fixes that it *does* surface there).
//! - Device errors *after* a successful start (e.g. `stop()` failing on
//!   hardware teardown) have no reporting channel: `stop_capture` returns
//!   stats, and the recording is already safe in the buffer. They are
//!   swallowed.
//! - A vendor `stop()` that never returns hangs `stop_capture` at the device
//!   join; that is inherent to the synchronous `Device` trait.
//! - The sound card backend is compiled only with urh's `soundcard` feature,
//!   which this crate does not enable: it is listed as unavailable, and
//!   starting it is rejected with `invalid-argument`.

use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use urh::devices::native;
use urh::devices::rtltcp::RtlTcpDevice;
use urh::devices::{Device, DeviceConfig, DeviceError, IqChunk};
use urh::iq::IqArray;
use urh::Complex32;

use crate::signal::Signal;
use crate::{AppError, SignalRepository};

// ---------------------------------------------------------------------------
// Tunables (contract defaults)
// ---------------------------------------------------------------------------

/// Preview queue capacity in batches (contract: "default 16 batches").
pub(crate) const PREVIEW_QUEUE_CAPACITY: usize = 16;

/// Record queue capacity in batches (contract: "default 4096 batches").
pub(crate) const RECORD_QUEUE_CAPACITY: usize = 4096;

/// Samples per preview/record batch (contract: "≥ 4096 samples"). The pump
/// re-batches device chunks to exactly this size so every preview batch
/// satisfies the minimum and the recorded stream stays batch-aligned.
pub(crate) const PREVIEW_BATCH_SAMPLES: usize = 4096;

// ---------------------------------------------------------------------------
// Wire types (contract: "Device capture")
// ---------------------------------------------------------------------------

/// One entry of [`SignalRepository::list_devices`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// E.g. `"RTL-SDR"`, `"mock"`, `"rtl-tcp"`; this is the string
    /// [`CaptureConfig::device`] matches (case-insensitively, and a few
    /// obvious spellings like `"RTL-TCP"` are accepted).
    pub name: String,
    /// `"native-dlopen" | "network" | "mock" | "cpal"`.
    pub backend: String,
    /// Driver library loadable (native) / startable (mock, network). Never
    /// involves opening hardware — `list_devices` is side-effect free.
    pub available: bool,
    /// Empty when available, otherwise the reason.
    pub detail: String,
}

/// Capture parameterization (contract: "Device capture"). `Default` is a
/// construction convenience only — the all-default config (empty device,
/// zero sample rate) fails [`SignalRepository::start_capture`] validation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureConfig {
    /// [`DeviceInfo::name`] of the device to open.
    pub device: String,
    /// Hz; must be finite and positive (it becomes the recording's rate).
    pub sample_rate: f64,
    /// Hz; must be finite.
    pub frequency: f64,
    /// Hz; must be finite.
    pub bandwidth: f64,
    /// dB; must be finite (sign and range are backend-specific).
    pub gain: f64,
    /// Append samples into a repository signal registered on stop.
    pub record: bool,
    /// rtl-tcp only: server host.
    pub host: Option<String>,
    /// rtl-tcp only: server port.
    pub port: Option<u16>,
    /// Native backends only: device identifier (USB serial / index) of the
    /// specific hardware to open — the values [`DeviceInfo`] selection is
    /// built from `list_device_ids`. `None`/empty opens the first device.
    #[serde(default)]
    pub identifier: Option<String>,
}

/// Returned by [`SignalRepository::start_capture`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureHandle {
    /// `"cap-<n>"`.
    pub capture_id: String,
    /// `Some("rec-<n>")` iff `record` was set; the signal is registered in
    /// the repository when the capture stops.
    pub signal_id: Option<String>,
}

/// Accounting of one capture (contract: "Device capture"), also the body of
/// the `capture://stats` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureStats {
    /// Preview batches dropped because the bounded preview queue overflowed
    /// (oldest-first; display-only data).
    pub dropped_preview_batches: u64,
    /// Complex samples appended to the recording (0 when not recording).
    pub recorded_samples: u64,
    /// The record queue overflowed: the capture stopped itself and recorded
    /// data was lost.
    pub overflow_detected: bool,
}

// ---------------------------------------------------------------------------
// Device table
// ---------------------------------------------------------------------------

/// Backend tags used in [`DeviceInfo::backend`] (contract values).
const BACKEND_NATIVE: &str = "native-dlopen";
const BACKEND_NETWORK: &str = "network";
const BACKEND_MOCK: &str = "mock";
const BACKEND_CPAL: &str = "cpal";

/// Every device the UI can name: `(canonical, display name, backend)`. The
/// native entries mirror `urh::devices::native::registry()` (a unit test
/// cross-checks the two so they cannot drift). Order is the `list_devices`
/// order: the mock demo first, then the native backends, then the network
/// backend, then the (unavailable) sound card.
const DEVICES: &[(&str, &str, &str)] = &[
    ("mock", "mock", BACKEND_MOCK),
    ("hackrf", "HackRF", BACKEND_NATIVE),
    ("rad1o", "rad1o", BACKEND_NATIVE),
    ("rtlsdr", "RTL-SDR", BACKEND_NATIVE),
    ("airspy", "AirSpy", BACKEND_NATIVE),
    ("bladerf", "BladeRF", BACKEND_NATIVE),
    ("limesdr", "LimeSDR", BACKEND_NATIVE),
    ("plutosdr", "PlutoSDR", BACKEND_NATIVE),
    ("usrp", "USRP", BACKEND_NATIVE),
    ("sdrplay", "SDRPlay", BACKEND_NATIVE),
    ("rtl-tcp", "rtl-tcp", BACKEND_NETWORK),
    ("soundcard", "SoundCard", BACKEND_CPAL),
];

/// Extra accepted spellings (normalized) → canonical name. Display names
/// such as `"RTL-SDR"` lower-case to spellings that need an alias, and the
/// space-to-dash normalization turns `"sound card"` into `sound-card`.
const DEVICE_ALIASES: &[(&str, &str)] = &[
    ("rtl-sdr", "rtlsdr"),
    ("rtltcp", "rtl-tcp"),
    ("sound-card", "soundcard"),
];

/// Normalize a `CaptureConfig::device` to the canonical backend name, or
/// `None` for an unknown device.
fn canonical_device_name(name: &str) -> Option<&'static str> {
    let normalized = name.trim().to_ascii_lowercase().replace([' ', '_'], "-");
    DEVICES
        .iter()
        .find(|d| d.0 == normalized)
        .map(|d| d.0)
        .or_else(|| {
            DEVICE_ALIASES
                .iter()
                .find(|a| a.0 == normalized)
                .map(|a| a.1)
        })
}

// ---------------------------------------------------------------------------
// Public service methods (contract: "Device capture")
// ---------------------------------------------------------------------------

impl SignalRepository {
    /// Enumerate the devices this build knows about (contract: "Device
    /// capture"). Native backends are probed by attempting to load their
    /// vendor library — no hardware is opened here; `rtl-tcp` is always
    /// listed available (connection errors surface at
    /// [`start_capture`](SignalRepository::start_capture)); the mock device
    /// is always available; the sound card is listed unavailable because
    /// cpal support is not compiled in.
    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        DEVICES
            .iter()
            .map(|&(canonical, display, backend)| {
                let (available, detail) = match backend {
                    BACKEND_NATIVE => {
                        let available = native::library_available(canonical);
                        let detail = if available {
                            String::new()
                        } else {
                            format!(
                                "{display} vendor library not loadable; install the device SDK \
                                 to use this backend"
                            )
                        };
                        (available, detail)
                    }
                    // Mock and network availability is a start-time property.
                    BACKEND_MOCK | BACKEND_NETWORK => (true, String::new()),
                    _ => (
                        false,
                        "sound card support is not compiled into this build (urh 'soundcard' \
                         feature)"
                            .to_string(),
                    ),
                };
                DeviceInfo {
                    name: display.to_string(),
                    backend: backend.to_string(),
                    available,
                    detail,
                }
            })
            .collect()
    }

    /// Enumerate the *connected physical devices* of one backend (contract:
    /// "Device capture") — the values for the device-identifier selection,
    /// port of the original `DeviceSettingsWidget`'s refresh button calling
    /// `device.get_device_list()`.
    ///
    /// Returns an empty list when no hardware is connected, the vendor
    /// library is missing, or the backend has no multi-device identifiers
    /// (mock, rtl-tcp). Unknown device names are `invalid-argument`.
    pub fn list_device_ids(&self, device: &str) -> Result<Vec<String>, AppError> {
        let canonical = canonical_device_name(device).ok_or_else(|| {
            AppError::invalid_argument(format!(
                "unknown device {:?} (see list_devices)",
                device
            ))
        })?;
        Ok(match DEVICES
            .iter()
            .find(|&&(name, _, _)| name == canonical)
            .map(|&(_, _, backend)| backend)
        {
            Some(BACKEND_NATIVE) => native::device_list_for(canonical),
            _ => Vec::new(),
        })
    }

    /// Open a device and start streaming (contract: "Device capture").
    ///
    /// Spawns the device, pump and (when `record`) recorder threads, waits
    /// for the device to confirm open + `start_rx` — connection and open
    /// errors surface here — and returns the handle. Blocks for as long as
    /// the backend open does (USB enumeration; rtl-tcp's OS connect timeout).
    ///
    /// Validation failures (unknown device, non-positive/non-finite
    /// `sample_rate`, non-finite `frequency`/`bandwidth`/`gain`, rtl-tcp
    /// without host/port) are `invalid-argument`; device errors map per the
    /// module's table.
    pub fn start_capture(&self, cfg: &CaptureConfig) -> Result<CaptureHandle, AppError> {
        let canonical = canonical_device_name(&cfg.device).ok_or_else(|| {
            AppError::invalid_argument(format!(
                "unknown device {:?} (see list_devices)",
                cfg.device
            ))
        })?;
        if !cfg.sample_rate.is_finite() || cfg.sample_rate <= 0.0 {
            return Err(AppError::invalid_argument(format!(
                "sample_rate must be a positive finite number, got {}",
                cfg.sample_rate
            )));
        }
        for (field, value) in [
            ("frequency", cfg.frequency),
            ("bandwidth", cfg.bandwidth),
            ("gain", cfg.gain),
        ] {
            if !value.is_finite() {
                return Err(AppError::invalid_argument(format!(
                    "{field} must be a finite number, got {value}"
                )));
            }
        }
        if canonical == "rtl-tcp"
            && (cfg.host.as_deref().is_none_or(|h| h.trim().is_empty()) || cfg.port.is_none())
        {
            return Err(AppError::invalid_argument(
                "rtl-tcp requires both host and port",
            ));
        }
        if canonical == "soundcard" {
            return Err(AppError::invalid_argument(
                "device unavailable: the sound card backend is not compiled into this build",
            ));
        }

        let capture_id = self.alloc_id("cap-");
        let signal_id = cfg.record.then(|| self.alloc_id("rec-"));

        let core = Arc::new(CaptureCore::new(
            PREVIEW_QUEUE_CAPACITY,
            cfg.record.then_some(RECORD_QUEUE_CAPACITY),
        ));
        let preview_receiver = PreviewReceiver {
            queue: Arc::clone(&core.preview),
        };

        let (device_tx, device_rx) = mpsc::channel::<IqChunk>();
        let (hs_tx, hs_rx) = mpsc::channel::<Result<(), AppError>>();
        let spec = DeviceSpec {
            canonical,
            host: cfg.host.clone(),
            port: cfg.port,
            config: DeviceConfig {
                identifier: cfg.identifier.clone().unwrap_or_default(),
                frequency: cfg.frequency,
                sample_rate: cfg.sample_rate,
                bandwidth: cfg.bandwidth,
                rf_gain: cfg.gain,
                ..DeviceConfig::default()
            },
        };

        // Pump first (it must be running before the device starts pushing),
        // then the recorder, then the device.
        let pump_core = Arc::clone(&core);
        let pump_thread = spawn_named("capture-pump", move || pump_main(pump_core, device_rx))?;
        let recorder_thread = match &core.record {
            None => None,
            Some(queue) => {
                let recorder_core = Arc::clone(&core);
                let queue = Arc::clone(queue);
                Some(spawn_named("capture-recorder", move || {
                    recorder_main(recorder_core, queue)
                })?)
            }
        };
        let device_core = Arc::clone(&core);
        let device_thread = spawn_named("capture-device", move || {
            device_thread_main(spec, device_tx, hs_tx, device_core)
        })?;

        // Handshake: the device thread reports open + start_rx here.
        let startup = match hs_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(AppError::internal(
                "device thread exited without reporting a result",
            )),
        };
        if let Err(err) = startup {
            core.stop.set();
            join_quietly_owned(device_thread);
            join_quietly_owned(pump_thread);
            if let Some(handle) = recorder_thread {
                join_quietly_owned(handle);
            }
            return Err(err);
        }

        // Refuse an id collision with a live session instead of silently
        // replacing it (see the module's registry note).
        {
            let mut sessions = captures().lock().expect("capture registry lock poisoned");
            if sessions.contains_key(&capture_id) {
                drop(sessions);
                core.stop.set();
                join_quietly_owned(device_thread);
                join_quietly_owned(pump_thread);
                if let Some(handle) = recorder_thread {
                    join_quietly_owned(handle);
                }
                return Err(AppError::internal(format!(
                    "capture id {capture_id} collides with a live capture of another repository \
                     instance (the desktop app uses a single repository)"
                )));
            }
            sessions.insert(
                capture_id.clone(),
                Arc::new(CaptureSession {
                    signal_id: signal_id.clone(),
                    sample_rate: cfg.sample_rate,
                    core: Arc::clone(&core),
                    preview_rx: Mutex::new(Some(preview_receiver)),
                    device_thread: Mutex::new(Some(device_thread)),
                    pump_thread: Mutex::new(Some(pump_thread)),
                    recorder_thread: Mutex::new(recorder_thread),
                }),
            );
        }
        Ok(CaptureHandle {
            capture_id,
            signal_id,
        })
    }

    /// Stop a capture, finalize its recording and return the stats (contract:
    /// "Device capture").
    ///
    /// Joins the device thread (stops the hardware), the pump (drains the
    /// device channel, flushes the sub-batch tail into the record queue,
    /// closes the queues) and the recorder; with `record = true` the recorded
    /// buffer is registered in the repository under the handle's
    /// `signal_id` (`"rec-<n>"`, `format = "complex"`, in-memory `path`).
    /// Unknown ids are `invalid-argument` (`"unknown capture id"`); stopping
    /// twice fails the same way because the session is removed here.
    pub fn stop_capture(&self, capture_id: &str) -> Result<CaptureStats, AppError> {
        // Take the session out of the registry and drop the registry lock
        // before joining anything (lock discipline).
        let session = captures()
            .lock()
            .expect("capture registry lock poisoned")
            .remove(capture_id)
            .ok_or_else(|| {
                AppError::invalid_argument(format!("unknown capture id {capture_id}"))
            })?;

        // 1. Wake the device thread; it stops the hardware, which closes the
        //    device channel.
        session.core.stop.set();
        if let Some(handle) = session
            .device_thread
            .lock()
            .expect("session lock poisoned")
            .take()
        {
            join_quietly_owned(handle);
        }
        // 2. The pump drains the closed channel, flushes its tail and closes
        //    the preview/record queues.
        if let Some(handle) = session
            .pump_thread
            .lock()
            .expect("session lock poisoned")
            .take()
        {
            join_quietly_owned(handle);
        }
        // 3. The recorder appends whatever the pump queued, then exits when
        //    the record queue closes.
        let mut stats = session.core.stats();
        if let Some(handle) = session
            .recorder_thread
            .lock()
            .expect("session lock poisoned")
            .take()
        {
            match handle.join() {
                Ok(recorded) => {
                    stats.recorded_samples = recorded.len() as u64;
                    if let Some(signal_id) = session.signal_id.as_deref() {
                        let id = signal_id.to_string();
                        self.register(
                            id.clone(),
                            Signal::new(id, "", IqArray::from_vec(recorded), session.sample_rate),
                        );
                    }
                }
                Err(_) => {
                    return Err(AppError::internal(
                        "recorder thread panicked; the recording is lost",
                    ))
                }
            }
        }
        Ok(stats)
    }

    /// Snapshot of a running (or self-stopped, not yet finalized) capture's
    /// counters — the body of the ~1 Hz `capture://stats` poll. Only atomics
    /// and the preview drop counter are read; no thread is joined and no
    /// channel waited on. Unknown ids are `invalid-argument`.
    pub fn capture_stats(&self, capture_id: &str) -> Result<CaptureStats, AppError> {
        let session = captures()
            .lock()
            .expect("capture registry lock poisoned")
            .get(capture_id)
            .cloned()
            .ok_or_else(|| {
                AppError::invalid_argument(format!("unknown capture id {capture_id}"))
            })?;
        Ok(session.core.stats())
    }

    /// Hand out the single-consumer preview receiver of a capture (see the
    /// module's "Batch forwarding" section). `None` if the id is unknown or
    /// the receiver was already taken.
    pub fn take_preview_receiver(&self, capture_id: &str) -> Option<PreviewReceiver> {
        let session = captures()
            .lock()
            .expect("capture registry lock poisoned")
            .get(capture_id)
            .cloned()?;
        let receiver = session
            .preview_rx
            .lock()
            .expect("session lock poisoned")
            .take();
        receiver
    }
}

// ---------------------------------------------------------------------------
// Preview receiver API (internal, for the Tauri shell)
// ---------------------------------------------------------------------------

/// One preview batch as forwarded to the `capture://batch` event: `seq` (0,
/// 1, 2, …; gaps mean drops) and the body bytes — interleaved little-endian
/// f32 I/Q of the batch, to be emitted verbatim as the event's ArrayBuffer.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewBatch {
    /// Monotonic batch counter starting at 0 (contract: gap detection).
    pub seq: u64,
    /// Interleaved LE f32 I/Q samples (`re`, `im` per sample).
    pub body: Vec<u8>,
}

/// The capture ended and the preview queue is drained — the error side of
/// [`PreviewReceiver::recv_timeout`]. A zero-sized marker: outside callers
/// match it as `Err(_)` (the v2 modules are private, so most callers cannot
/// spell this type — by design they never need to).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewClosed;

/// The single consumer of a capture's preview batches (see
/// [`SignalRepository::take_preview_receiver`]). Cheap to move into the
/// shell's poller thread; not `Clone` on purpose — one poller per capture.
///
/// The receiver's methods are shaped for use from *outside* the crate: the
/// v2 service modules are private, so `PreviewReceiver`/`PreviewBatch` are
/// unnameable there — results stick to shapes whose variants callers can
/// match without naming anything from this module, with the batch type
/// inferred and its `seq`/`body` fields reachable.
pub struct PreviewReceiver {
    queue: Arc<PreviewQueue>,
}

impl PreviewReceiver {
    /// Wait for the next batch up to `timeout`:
    ///
    /// - `Ok(Some(batch))` — a batch arrived (`.seq`, `.body`);
    /// - `Ok(None)` — nothing within the timeout; keep polling;
    /// - `Err(_)` ([`PreviewClosed`]) — the capture ended and the queue is
    ///   drained; end the poller.
    ///
    /// Queued batches drain first, so a slow poller recovers the newest 16
    /// batches after a stall. `Duration::ZERO` is the non-blocking variant.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<PreviewBatch>, PreviewClosed> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .queue
            .state
            .lock()
            .expect("preview queue lock poisoned");
        loop {
            if let Some(batch) = state.ring.pop_front() {
                return Ok(Some(batch));
            }
            if state.closed {
                return Err(PreviewClosed);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let (guard, _) = self
                .queue
                .not_empty
                .wait_timeout(state, deadline - now)
                .expect("preview queue lock poisoned");
            state = guard;
        }
    }
}

// ---------------------------------------------------------------------------
// Bounded queues
// ---------------------------------------------------------------------------

/// Shared preview queue state.
struct PreviewState {
    ring: VecDeque<PreviewBatch>,
    capacity: usize,
    dropped: u64,
    closed: bool,
}

/// Bounded preview queue with drop-oldest overflow (queue discipline 1).
/// Leaf lock: taken only by `push`/`recv`/`dropped`, never nested.
struct PreviewQueue {
    state: Mutex<PreviewState>,
    not_empty: Condvar,
}

impl PreviewQueue {
    fn new(capacity: usize) -> Self {
        debug_assert!(
            capacity >= 1,
            "a zero-capacity preview queue drops everything"
        );
        PreviewQueue {
            state: Mutex::new(PreviewState {
                ring: VecDeque::with_capacity(capacity),
                capacity,
                dropped: 0,
                closed: false,
            }),
            not_empty: Condvar::new(),
        }
    }

    /// Enqueue one batch, dropping the oldest if full. Never blocks.
    fn push(&self, batch: PreviewBatch) {
        let mut state = self.state.lock().expect("preview queue lock poisoned");
        if state.closed {
            return;
        }
        if state.ring.len() >= state.capacity {
            state.ring.pop_front();
            state.dropped += 1;
        }
        state.ring.push_back(batch);
        drop(state);
        self.not_empty.notify_all();
    }

    /// Batches dropped to overflow so far.
    fn dropped(&self) -> u64 {
        self.state
            .lock()
            .expect("preview queue lock poisoned")
            .dropped
    }

    /// Close the queue: no further pushes; consumers drain and then see
    /// [`PreviewClosed`] from [`PreviewReceiver::recv_timeout`].
    fn close(&self) {
        let mut state = self.state.lock().expect("preview queue lock poisoned");
        state.closed = true;
        drop(state);
        self.not_empty.notify_all();
    }
}

/// RAII close of the preview queue's producer side. The pump owns it so the
/// queue closes even if the pump unwinds.
struct PreviewSender {
    queue: Arc<PreviewQueue>,
}

impl Drop for PreviewSender {
    fn drop(&mut self) {
        self.queue.close();
    }
}

/// Result of a record-queue push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushOutcome {
    /// The batch was queued for the recorder.
    Queued,
    /// The queue is full: overflow (queue discipline 2).
    Full,
    /// The queue is closed (the pump is exiting); the batch is not recorded.
    Closed,
}

/// Shared record queue state.
struct RecordState {
    ring: VecDeque<Vec<Complex32>>,
    closed: bool,
}

/// Bounded record queue (queue discipline 2). The pump pushes (never
/// blocking), the recorder drains; `Full` must stop the capture.
struct RecordQueue {
    state: Mutex<RecordState>,
    not_empty: Condvar,
    capacity: usize,
}

impl RecordQueue {
    fn new(capacity: usize) -> Self {
        debug_assert!(
            capacity >= 1,
            "a zero-capacity record queue always overflows"
        );
        RecordQueue {
            state: Mutex::new(RecordState {
                ring: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            not_empty: Condvar::new(),
            capacity,
        }
    }

    /// Enqueue one batch unless the queue is full or closed.
    fn try_push(&self, batch: Vec<Complex32>) -> PushOutcome {
        let mut state = self.state.lock().expect("record queue lock poisoned");
        if state.closed {
            return PushOutcome::Closed;
        }
        if state.ring.len() >= self.capacity {
            return PushOutcome::Full;
        }
        state.ring.push_back(batch);
        drop(state);
        self.not_empty.notify_all();
        PushOutcome::Queued
    }

    /// Block until a batch is available or the producer side closes (then
    /// drain and return `None`). The recorder thread runs this.
    fn recv(&self) -> Option<Vec<Complex32>> {
        let mut state = self.state.lock().expect("record queue lock poisoned");
        loop {
            if let Some(batch) = state.ring.pop_front() {
                return Some(batch);
            }
            if state.closed {
                return None;
            }
            state = self
                .not_empty
                .wait(state)
                .expect("record queue lock poisoned");
        }
    }

    /// Close the producer side (see [`RecordSender`]).
    fn close(&self) {
        let mut state = self.state.lock().expect("record queue lock poisoned");
        state.closed = true;
        drop(state);
        self.not_empty.notify_all();
    }
}

/// RAII close of the record queue's producer side, owned by the pump so the
/// recorder wakes even if the pump unwinds.
struct RecordSender {
    queue: Arc<RecordQueue>,
}

impl Drop for RecordSender {
    fn drop(&mut self) {
        self.queue.close();
    }
}

// ---------------------------------------------------------------------------
// Stop flag
// ---------------------------------------------------------------------------

/// Cooperative stop flag with a parking place for the device thread.
#[derive(Default)]
struct StopState {
    stopped: Mutex<bool>,
    resumed: Condvar,
}

impl StopState {
    /// Request the stop and wake every waiter. Idempotent.
    fn set(&self) {
        let mut stopped = self.stopped.lock().expect("stop flag lock poisoned");
        *stopped = true;
        drop(stopped);
        self.resumed.notify_all();
    }

    /// Block until [`StopState::set`]; the device thread parks here between
    /// `start_rx` and `stop`.
    fn wait(&self) {
        let mut stopped = self.stopped.lock().expect("stop flag lock poisoned");
        while !*stopped {
            stopped = self.resumed.wait(stopped).expect("stop flag lock poisoned");
        }
    }
}

// ---------------------------------------------------------------------------
// Capture core (queues + counters, shared by all threads)
// ---------------------------------------------------------------------------

/// The state every capture thread shares. All queue locks are leaf locks and
/// [`StopState::set`] is never called while holding one, so the capture is
/// deadlock-free.
struct CaptureCore {
    preview: Arc<PreviewQueue>,
    record: Option<Arc<RecordQueue>>,
    stop: Arc<StopState>,
    /// Preview batch counter (queue discipline 1: seq starts at 0).
    seq: AtomicU64,
    overflow: AtomicBool,
    /// Recorded samples so far; the exact final count comes from the
    /// recorder's buffer at `stop_capture`.
    recorded_samples: AtomicU64,
}

impl CaptureCore {
    /// Build the queue/discipline state for a session.
    fn new(preview_capacity: usize, record_capacity: Option<usize>) -> Self {
        CaptureCore {
            preview: Arc::new(PreviewQueue::new(preview_capacity)),
            record: record_capacity.map(|cap| Arc::new(RecordQueue::new(cap))),
            stop: Arc::new(StopState::default()),
            seq: AtomicU64::new(0),
            overflow: AtomicBool::new(false),
            recorded_samples: AtomicU64::new(0),
        }
    }

    /// Forward one full batch (the pump guarantees
    /// `batch.len() >= PREVIEW_BATCH_SAMPLES`) to preview and record.
    fn forward(&self, batch: &[Complex32]) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.preview.push(PreviewBatch {
            seq,
            body: encode_iq_body(batch),
        });
        self.push_record(batch);
    }

    /// Flush the sub-batch tail at stream end into the record queue only:
    /// recorded samples must not be lost, while preview batches must stay
    /// `>= PREVIEW_BATCH_SAMPLES`.
    fn forward_tail(&self, tail: &[Complex32]) {
        if !tail.is_empty() {
            self.push_record(tail);
        }
    }

    /// Record-side push with the contract's overflow rule: a full queue sets
    /// `overflow_detected` and stops the capture; after the overflow no
    /// further batches are recorded (the loss is already declared).
    fn push_record(&self, batch: &[Complex32]) {
        if let Some(record) = &self.record {
            if !self.overflow.load(Ordering::Relaxed) {
                match record.try_push(batch.to_vec()) {
                    PushOutcome::Queued => {}
                    PushOutcome::Full => {
                        self.overflow.store(true, Ordering::SeqCst);
                        self.stop.set();
                    }
                    PushOutcome::Closed => {}
                }
            }
        }
    }

    /// Stats snapshot (see [`SignalRepository::capture_stats`]).
    fn stats(&self) -> CaptureStats {
        CaptureStats {
            dropped_preview_batches: self.preview.dropped(),
            recorded_samples: self.recorded_samples.load(Ordering::Relaxed),
            overflow_detected: self.overflow.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// Sessions and the process-wide registry
// ---------------------------------------------------------------------------

/// One live capture: its identity, queue core and thread handles. Thread
/// handles sit behind `Mutex<Option<..>>` because `stop_capture` takes them
/// by value through a shared `Arc` (it runs at most once — the session is
/// removed from the registry under the registry lock first).
struct CaptureSession {
    /// `Some("rec-<n>")` iff recording; registered by `stop_capture`.
    signal_id: Option<String>,
    /// The capture's sample rate, becomes the recording's rate.
    sample_rate: f64,
    core: Arc<CaptureCore>,
    preview_rx: Mutex<Option<PreviewReceiver>>,
    device_thread: Mutex<Option<JoinHandle<()>>>,
    pump_thread: Mutex<Option<JoinHandle<()>>>,
    recorder_thread: Mutex<Option<JoinHandle<Vec<Complex32>>>>,
}

/// Live captures, keyed by `"cap-<n>"`. Process-wide static because
/// `SignalRepository` cannot gain fields — see the module's "Session
/// registry" note for the one-instance tradeoff.
static CAPTURES: OnceLock<Mutex<HashMap<String, Arc<CaptureSession>>>> = OnceLock::new();

fn captures() -> &'static Mutex<HashMap<String, Arc<CaptureSession>>> {
    CAPTURES.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Thread bodies
// ---------------------------------------------------------------------------

/// Everything the device thread needs; `DeviceConfig` mirrors the contract's
/// `CaptureConfig` fields plus the pre-validated canonical backend name.
struct DeviceSpec {
    canonical: &'static str,
    host: Option<String>,
    port: Option<u16>,
    config: DeviceConfig,
}

/// Open the device, start receiving, park until stopped, stop. The device
/// value never leaves this thread (`Box<dyn Device>` is not `Send`).
fn device_thread_main(
    spec: DeviceSpec,
    device_tx: mpsc::Sender<IqChunk>,
    hs_tx: mpsc::Sender<Result<(), AppError>>,
    core: Arc<CaptureCore>,
) {
    let outcome = (|| -> Result<(), AppError> {
        let mut device = open_device(&spec)?;
        if let Err(err) = device.start_rx(&spec.config, device_tx) {
            // Best-effort close of the half-started device.
            let _ = device.stop();
            return Err(device_error(err));
        }
        let _ = hs_tx.send(Ok(()));
        core.stop.wait();
        device.stop().map_err(device_error)
    })();
    // Only observable when the failure preceded the handshake; after it,
    // start_capture has returned and late device errors have no channel
    // (documented in the module's caveats).
    if let Err(err) = outcome {
        let _ = hs_tx.send(Err(err));
    }
}

/// Re-batch device chunks into [`PREVIEW_BATCH_SAMPLES`]-sample batches and
/// forward them to the preview/record queues; flush the tail when the device
/// channel closes. `catch_unwind` guarantees the stop flag is set even if
/// the pump panics, so the device thread can never park forever.
fn pump_main(core: Arc<CaptureCore>, device_rx: mpsc::Receiver<IqChunk>) {
    // RAII senders: dropping them (return or unwind) closes the queues,
    // which wakes the preview consumer and the recorder.
    let _preview = PreviewSender {
        queue: Arc::clone(&core.preview),
    };
    let _record = core.record.as_ref().map(|queue| RecordSender {
        queue: Arc::clone(queue),
    });
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut acc: Vec<Complex32> = Vec::with_capacity(PREVIEW_BATCH_SAMPLES);
        // Exit on channel close: the device channel closes when the
        // backend's stream ends (stop or remote EOF), after std mpsc has
        // delivered every buffered chunk.
        while let Ok(chunk) = device_rx.recv() {
            acc.extend_from_slice(&chunk);
            while acc.len() >= PREVIEW_BATCH_SAMPLES {
                let batch: Vec<Complex32> = acc.drain(..PREVIEW_BATCH_SAMPLES).collect();
                core.forward(&batch);
            }
        }
        core.forward_tail(&acc);
    }));
    // Wake the device thread even if the pump panicked mid-stream; otherwise
    // it would park forever and stop_capture would hang joining it.
    core.stop.set();
    if outcome.is_err() {
        // The stop flag is set above and the RAII senders close the queues on
        // unwind, so the device halts, the preview consumer and the recorder
        // drain, and stop_capture still joins everything and returns the
        // stats of whatever was recorded. There is no channel to report a
        // pump panic on, so it is deliberately swallowed.
    }
}

/// Drain the record queue into the recorded buffer; returns the buffer.
fn recorder_main(core: Arc<CaptureCore>, queue: Arc<RecordQueue>) -> Vec<Complex32> {
    let mut recorded: Vec<Complex32> = Vec::new();
    while let Some(batch) = queue.recv() {
        recorded.extend_from_slice(&batch);
        core.recorded_samples
            .fetch_add(batch.len() as u64, Ordering::Relaxed);
    }
    recorded
}

// ---------------------------------------------------------------------------
// Device construction and error mapping
// ---------------------------------------------------------------------------

/// Open the device named by the spec (on the caller's thread). Native
/// backends dlopen their vendor library here — `LibraryUnavailable` surfaces
/// as `invalid-argument` ("device unavailable").
fn open_device(spec: &DeviceSpec) -> Result<Box<dyn Device>, AppError> {
    match spec.canonical {
        "mock" => Ok(Box::new(MockDevice::new())),
        "rtl-tcp" => {
            let host = spec.host.as_deref().unwrap_or_default();
            let port = spec.port.unwrap_or(0);
            Ok(Box::new(
                RtlTcpDevice::connect(host, port).map_err(device_error)?,
            ))
        }
        // "soundcard" is rejected in start_capture and unknown names never
        // pass validation, so only native registry names arrive here.
        name => Ok(native::open_by_name(
            name,
            Some(spec.config.identifier.as_str()),
        )
        .map_err(device_error)?),
    }
}

/// `DeviceError` → `AppError` per the module's mapping table.
fn device_error(err: DeviceError) -> AppError {
    match err {
        DeviceError::LibraryUnavailable(detail) => {
            AppError::invalid_argument(format!("device unavailable: {detail}"))
        }
        DeviceError::Unsupported(what) => {
            AppError::invalid_argument(format!("device unavailable: {what}"))
        }
        DeviceError::OpenFailed(detail)
        | DeviceError::ParameterRejected(detail)
        | DeviceError::StreamError(detail) => AppError {
            code: AppError::CODE_IO,
            message: format!("device error: {detail}"),
        },
        DeviceError::Io(err) => AppError {
            code: AppError::CODE_IO,
            message: format!("device io error: {err}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Preview body codec
// ---------------------------------------------------------------------------

/// Interleaved little-endian f32 I/Q bytes — the `capture://batch` body.
fn encode_iq_body(samples: &[Complex32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(samples.len() * 8);
    for s in samples {
        body.extend_from_slice(&s.re.to_le_bytes());
        body.extend_from_slice(&s.im.to_le_bytes());
    }
    body
}

// ---------------------------------------------------------------------------
// Mock device (contract queue discipline 4)
// ---------------------------------------------------------------------------

/// The mock's repeating ASK bits: `10101100` (contract).
const MOCK_PATTERN: [u8; 8] = [1, 0, 1, 0, 1, 1, 0, 0];
/// Samples per mock bit (contract).
const MOCK_SAMPLES_PER_BIT: usize = 100;
/// Bit-1 amplitude (contract: "amplitude 1.0/0.1").
const MOCK_HIGH: f32 = 1.0;
/// Bit-0 amplitude.
const MOCK_LOW: f32 = 0.1;
/// Mock chunk size: one preview batch per push.
const MOCK_CHUNK_SAMPLES: usize = PREVIEW_BATCH_SAMPLES;
/// Pacing between pushes (~2M samples/s): the contract requires a sleep of
/// at most 2 ms between chunk pushes, fast enough to stream yet paced
/// enough to exercise the bounded queues.
const MOCK_CHUNK_INTERVAL: Duration = Duration::from_millis(2);

/// Carrier coordinates for a 4-samples-per-period rotation. The values are
/// exact unit-circle coordinates, so every mock sample's magnitude equals
/// its bit's amplitude bit-for-bit and the pattern decodes without
/// tolerances.
const MOCK_CARRIER_RE: [f32; 4] = [1.0, 0.0, -1.0, 0.0];
const MOCK_CARRIER_IM: [f32; 4] = [0.0, 1.0, 0.0, -1.0];

/// The mock device's sample at global index `k`.
fn mock_sample(k: usize) -> Complex32 {
    let bit = MOCK_PATTERN[(k / MOCK_SAMPLES_PER_BIT) % MOCK_PATTERN.len()];
    let amplitude = if bit == 1 { MOCK_HIGH } else { MOCK_LOW };
    Complex32::new(
        amplitude * MOCK_CARRIER_RE[k % 4],
        amplitude * MOCK_CARRIER_IM[k % 4],
    )
}

/// The always-available mock device (contract queue discipline 4): a
/// repeating ASK pattern (bits `10101100`, 100 samples/bit, amplitudes
/// 1.0/0.1) on a rotating carrier, pushed as
/// [`PREVIEW_BATCH_SAMPLES`]-sample chunks every [`MOCK_CHUNK_INTERVAL`].
/// It is the test fixture *and* the no-hardware demo. Tuning parameters are
/// accepted and ignored — the pattern is fixed; the configured sample rate
/// only interprets it.
struct MockDevice {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockDevice {
    fn new() -> Self {
        MockDevice {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl Device for MockDevice {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn device_list() -> Vec<String> {
        Vec::new()
    }

    fn set_parameter(
        &mut self,
        _cmd: urh::devices::Command,
        _value: f64,
    ) -> Result<(), DeviceError> {
        Ok(())
    }

    fn start_rx(
        &mut self,
        _config: &DeviceConfig,
        sink: mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(DeviceError::StreamError("already receiving".into()));
        }
        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        self.handle = Some(thread::spawn(move || {
            let mut emitted = 0usize;
            while running.load(Ordering::SeqCst) {
                let chunk: IqChunk = (emitted..emitted + MOCK_CHUNK_SAMPLES)
                    .map(mock_sample)
                    .collect();
                emitted += MOCK_CHUNK_SAMPLES;
                if sink.send(chunk).is_err() {
                    break; // the capture went away
                }
                thread::sleep(MOCK_CHUNK_INTERVAL);
            }
            running.store(false, Ordering::SeqCst);
        }));
        Ok(())
    }

    fn send(&mut self, _config: &DeviceConfig, _samples: &[Complex32]) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported("the mock device is receive-only"))
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }
}

impl Drop for MockDevice {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ---------------------------------------------------------------------------
// Thread helpers
// ---------------------------------------------------------------------------

/// Spawn a named thread (names show up in panic messages and debuggers).
fn spawn_named<T, F>(name: &str, f: F) -> Result<JoinHandle<T>, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .map_err(|err| AppError::internal(format!("spawning the {name} thread: {err}")))
}

/// Join a thread, discarding its result: used on the error paths of
/// `start_capture`, where the threads' outcomes no longer matter (the
/// session is never registered).
fn join_quietly_owned<T>(handle: JoinHandle<T>) {
    let _ = handle.join();
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture registry is process-wide while capture ids are per
    /// repository, so capture-starting tests in this binary share one
    /// repository (unique ids, no collisions; see the module's registry
    /// note).
    fn shared_repo() -> &'static SignalRepository {
        static REPO: OnceLock<SignalRepository> = OnceLock::new();
        REPO.get_or_init(SignalRepository::new)
    }

    fn config(device: &str) -> CaptureConfig {
        CaptureConfig {
            device: device.to_string(),
            sample_rate: 1e6,
            frequency: 433.92e6,
            bandwidth: 1e6,
            gain: 0.0,
            record: false,
            host: None,
            port: None,
            identifier: None,
        }
    }

    /// Whether the capture's stop flag was set (raw flag read: the accessor
    /// exists only for these tests, so it lives here too).
    fn stop_requested(core: &CaptureCore) -> bool {
        *core.stop.stopped.lock().unwrap()
    }

    #[test]
    fn list_device_ids_rejects_unknown_and_returns_empty_for_non_native() {
        let repo = shared_repo();
        // unknown device name → invalid-argument (mirrors other lookups)
        let err = repo.list_device_ids("does-not-exist").unwrap_err();
        assert_eq!(err.code, "invalid-argument");
        // non-native backends never enumerate identifiers
        assert!(repo.list_device_ids("mock").unwrap().is_empty());
        assert!(repo.list_device_ids("rtl-tcp").unwrap().is_empty());
        // Native backends are NOT probed here on purpose: their device_list
        // does real USB/network discovery (uhd find, iio context scan) that
        // can block for a long time on a machine without hardware — CI must
        // not depend on that. The empty-list-on-no-hardware behavior is
        // covered by the drivers job on real libraries instead.
    }

    #[test]
    fn list_devices_reports_every_backend() {
        let devices = shared_repo().list_devices();

        let mock = devices.iter().find(|d| d.name == "mock").unwrap();
        assert!(mock.available);
        assert_eq!(mock.backend, "mock");
        assert_eq!(mock.detail, "");

        let network = devices.iter().find(|d| d.name == "rtl-tcp").unwrap();
        assert!(network.available);
        assert_eq!(network.backend, "network");
        assert_eq!(network.detail, "");

        // Every native backend of the urh registry is listed exactly once,
        // with the native-dlopen backend tag (cross-check against the source
        // of truth so the table cannot drift).
        let native_entries: Vec<_> = devices
            .iter()
            .filter(|d| d.backend == "native-dlopen")
            .collect();
        assert_eq!(native_entries.len(), native::registry().len());
        for backend in native::registry() {
            assert!(
                native_entries
                    .iter()
                    .any(|d| canonical_device_name(&d.name) == Some(backend.name)),
                "missing native backend {}",
                backend.name
            );
        }

        // The sound card is listed but unavailable, with a reason.
        let sound = devices.iter().find(|d| d.name == "SoundCard").unwrap();
        assert!(!sound.available);
        assert_eq!(sound.backend, "cpal");
        assert!(!sound.detail.is_empty());

        // Stable shape: unique names, detail set iff unavailable.
        let mut names: Vec<_> = devices.iter().map(|d| d.name.clone()).collect();
        names.sort();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate device names");
        for d in &devices {
            assert_eq!(d.detail.is_empty(), d.available, "device {}", d.name);
        }
    }

    #[test]
    fn canonical_names_accept_display_and_alias_spellings() {
        for (input, expected) in [
            ("mock", "mock"),
            ("MOCK", "mock"),
            ("HackRF", "hackrf"),
            ("RTL-SDR", "rtlsdr"),
            ("rtl-sdr", "rtlsdr"),
            ("rtlsdr", "rtlsdr"),
            ("rtl-tcp", "rtl-tcp"),
            ("RTL-TCP", "rtl-tcp"),
            ("rtltcp", "rtl-tcp"),
            ("SoundCard", "soundcard"),
            (" sound card ", "soundcard"),
        ] {
            assert_eq!(
                canonical_device_name(input),
                Some(expected),
                "input {input:?}"
            );
        }
        for unknown in ["", "does-not-exist", "hackrf2", "rtl"] {
            assert_eq!(canonical_device_name(unknown), None, "input {unknown:?}");
        }
    }

    #[test]
    fn mock_pattern_is_exact_ask() {
        // Bits 10101100, 100 samples/bit, amplitudes 1.0/0.1 — magnitudes
        // are exact (unit-circle carrier), over more than two full
        // repetitions and across bit/chunk boundaries.
        for k in 0..2 * MOCK_PATTERN.len() * MOCK_SAMPLES_PER_BIT + 37 {
            let bit = MOCK_PATTERN[(k / MOCK_SAMPLES_PER_BIT) % MOCK_PATTERN.len()];
            let expected = if bit == 1 { MOCK_HIGH } else { MOCK_LOW };
            assert_eq!(mock_sample(k).norm(), expected, "sample {k}");
        }
        // The carrier really rotates (non-trivial I and Q).
        assert_eq!(mock_sample(0).re, 1.0);
        assert_eq!(mock_sample(1).im, 1.0);
        assert_eq!(mock_sample(2).re, -1.0);
    }

    #[test]
    fn mock_device_chunks_carry_the_pattern() {
        let mut device = MockDevice::new();
        let (tx, rx) = mpsc::channel();
        device.start_rx(&DeviceConfig::default(), tx).unwrap();

        let mut global = 0usize;
        for _ in 0..2 {
            let chunk = rx.recv_timeout(Duration::from_secs(5)).expect("mock chunk");
            assert_eq!(chunk.len(), MOCK_CHUNK_SAMPLES);
            for (k, sample) in chunk.iter().enumerate() {
                assert_eq!(*sample, mock_sample(global + k), "sample {}", global + k);
            }
            global += chunk.len();
        }
        device.stop().unwrap();
        // Stopped: nothing else arrives (the channel closes with the sender).
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        // Restart on the same device instance works (fresh pattern phase).
        let (tx, rx) = mpsc::channel();
        device.start_rx(&DeviceConfig::default(), tx).unwrap();
        let chunk = rx.recv_timeout(Duration::from_secs(5)).expect("chunk");
        assert_eq!(chunk[0], mock_sample(0));
        device.stop().unwrap();
    }

    #[test]
    fn encode_iq_body_roundtrips() {
        let samples: Vec<Complex32> = (0..100)
            .map(|k| Complex32::new(k as f32 * 0.5 - 25.0, -(k as f32) * 0.25))
            .collect();
        let body = encode_iq_body(&samples);
        assert_eq!(body.len(), 100 * 8);
        let decoded: Vec<Complex32> = body
            .chunks_exact(8)
            .map(|c| {
                Complex32::new(
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect();
        assert_eq!(decoded, samples);
    }

    /// Queue discipline 1: preview drops the oldest batch, counts it, and
    /// the surviving seqs expose the gap.
    #[test]
    fn preview_queue_drops_oldest_with_seq_gaps() {
        let core = CaptureCore::new(2, None);
        let batch = |n| vec![Complex32::new(n as f32, 0.0); 8];
        for n in 0..5u32 {
            core.forward(&batch(n));
        }
        let stats = core.stats();
        assert_eq!(stats.dropped_preview_batches, 3);
        assert_eq!(stats.recorded_samples, 0);
        assert!(!stats.overflow_detected);

        // Close the producer side (the pump's RAII sender) and consume: the
        // two survivors are the newest, seq 3 and 4 — 0..=2 were dropped.
        drop(PreviewSender {
            queue: Arc::clone(&core.preview),
        });
        let receiver = PreviewReceiver {
            queue: Arc::clone(&core.preview),
        };
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(Some(b)) => {
                assert_eq!(b.seq, 3);
                assert_eq!(b.body, encode_iq_body(&batch(3)));
            }
            other => panic!("expected a batch, got {other:?}"),
        }
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(Some(b)) => assert_eq!(b.seq, 4),
            other => panic!("expected a batch, got {other:?}"),
        }
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Err(PreviewClosed)
        );
        assert_eq!(core.stats().dropped_preview_batches, 3);
    }

    /// Queue discipline 2: record overflow flags `overflow_detected` and
    /// stops the capture; the overflowing batch is not recorded.
    #[test]
    fn record_overflow_flags_and_stops() {
        let core = CaptureCore::new(2, Some(2));
        let batch = |n| vec![Complex32::new(n as f32, 1.0); 8];
        core.forward(&batch(1));
        core.forward(&batch(2));
        assert!(!core.stats().overflow_detected);
        assert!(!stop_requested(&core));

        // The queue is full (no recorder running): the third batch overflows.
        core.forward(&batch(3));
        assert!(core.stats().overflow_detected);
        assert!(stop_requested(&core), "overflow must stop the capture");

        // The overflowing batch was not queued: draining the (now closed)
        // queue yields batches 1 and 2 only.
        drop(RecordSender {
            queue: Arc::clone(core.record.as_ref().unwrap()),
        });
        let queue = Arc::clone(core.record.as_ref().unwrap());
        assert_eq!(queue.recv(), Some(batch(1)));
        assert_eq!(queue.recv(), Some(batch(2)));
        assert_eq!(queue.recv(), None);

        // Post-overflow pushes are skipped, not queued.
        core.forward(&batch(4));
        assert_eq!(queue.recv(), None);
    }

    /// The tail flush reaches the record queue but not the preview queue
    /// (preview batches must be >= PREVIEW_BATCH_SAMPLES).
    #[test]
    fn tail_flush_records_without_preview() {
        let core = CaptureCore::new(4, Some(4));
        let tail = vec![Complex32::new(7.0, 0.0); 100];
        core.forward_tail(&tail);
        assert_eq!(core.stats().dropped_preview_batches, 0);
        let receiver = PreviewReceiver {
            queue: Arc::clone(&core.preview),
        };
        // Duration::ZERO is the non-blocking probe: the tail never reached
        // the preview queue.
        assert_eq!(receiver.recv_timeout(Duration::ZERO), Ok(None));
        drop(RecordSender {
            queue: Arc::clone(core.record.as_ref().unwrap()),
        });
        let queue = Arc::clone(core.record.as_ref().unwrap());
        assert_eq!(queue.recv(), Some(tail));
        assert_eq!(queue.recv(), None);
    }

    #[test]
    fn start_capture_rejects_bad_configs() {
        let repo = shared_repo();

        let mut cfg = config("does-not-exist");
        assert_eq!(
            repo.start_capture(&cfg).unwrap_err().code,
            "invalid-argument"
        );

        cfg = config("mock");
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            cfg.sample_rate = rate;
            assert_eq!(
                repo.start_capture(&cfg).unwrap_err().code,
                "invalid-argument",
                "rate {rate}"
            );
        }
        for (field, value) in [
            ("frequency", f64::NAN),
            ("bandwidth", f64::INFINITY),
            ("gain", f64::NAN),
        ] {
            let mut bad = config("mock");
            match field {
                "frequency" => bad.frequency = value,
                "bandwidth" => bad.bandwidth = value,
                _ => bad.gain = value,
            }
            assert_eq!(
                repo.start_capture(&bad).unwrap_err().code,
                "invalid-argument",
                "{field} {value}"
            );
        }

        // rtl-tcp needs both host and port.
        let mut rtl = config("rtl-tcp");
        assert_eq!(
            repo.start_capture(&rtl).unwrap_err().code,
            "invalid-argument"
        );
        rtl.host = Some("127.0.0.1".to_string());
        assert_eq!(
            repo.start_capture(&rtl).unwrap_err().code,
            "invalid-argument"
        );
        rtl.host = Some("  ".to_string());
        rtl.port = Some(1234);
        assert_eq!(
            repo.start_capture(&rtl).unwrap_err().code,
            "invalid-argument"
        );

        // The sound card is listed but not startable in this build.
        assert_eq!(
            repo.start_capture(&config("SoundCard")).unwrap_err().code,
            "invalid-argument"
        );

        // None of the rejected configs left a session behind.
        assert_eq!(
            repo.capture_stats("cap-9999").unwrap_err().code,
            "invalid-argument"
        );
    }

    /// Threaded smoke test of the full plumbing (record = false): start,
    /// observe preview batches, stop; the recording round-trip with the ASK
    /// decode lives in the integration test file.
    #[test]
    fn mock_capture_start_stop_smoke() {
        let repo = shared_repo();
        let handle = repo.start_capture(&config("mock")).unwrap();
        assert!(handle.capture_id.starts_with("cap-"));
        assert_eq!(handle.signal_id, None);

        let receiver = repo.take_preview_receiver(&handle.capture_id).unwrap();
        let mut last_seq = None;
        let mut batches = 0;
        while batches < 3 {
            match receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(b)) => {
                    assert_eq!(b.body.len(), PREVIEW_BATCH_SAMPLES * 8);
                    if let Some(prev) = last_seq {
                        assert!(b.seq > prev, "seq must not go backwards");
                    }
                    last_seq = Some(b.seq);
                    batches += 1;
                }
                other => panic!("expected a preview batch, got {other:?}"),
            }
        }
        let stats = repo.capture_stats(&handle.capture_id).unwrap();
        assert!(stats.recorded_samples == 0);
        assert!(!stats.overflow_detected);

        let stats = repo.stop_capture(&handle.capture_id).unwrap();
        assert_eq!(stats.recorded_samples, 0);
        assert_eq!(
            repo.capture_stats(&handle.capture_id).unwrap_err().code,
            "invalid-argument"
        );
        assert_eq!(
            repo.stop_capture(&handle.capture_id).unwrap_err().code,
            "invalid-argument"
        );
    }
}
