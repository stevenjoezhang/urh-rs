//! # urh-app — application service layer for the urh GUI.
//!
//! This crate is the Rust half of the P0/P1 vertical slice described in
//! `docs/ipc-contract.md` (the normative contract): it owns the open-signal
//! repository and computes viewport waveform envelopes over the `urh` DSP
//! core. The Tauri shell (`apps/desktop/src-tauri`) is a thin adapter on top;
//! the React frontend talks to it via the commands in that document.
//!
//! Layout:
//! - [`SignalRepository`], [`SignalMeta`], [`WaveformMode`], [`AppError`] —
//!   the public API (this module).
//! - `signal` — per-signal state: raw samples plus the block min/max LOD
//!   used to reduce envelope queries without losing peaks.
//! - `envelope` — the binary waveform frame codec.
//!
//! Storage is the MVP decision from the contract: the whole file is loaded
//! into memory via `urh::iq::IqArray::from_file` (10M samples = 80 MB). The
//! repository API is shaped so a paged/mmap backend can replace it later.
//!
//! Two contract points needed a defensible reading (reported upstream):
//! - `path` in [`SignalMeta`] is stored verbatim as passed to
//!   [`SignalRepository::open`] ("as opened"); the frontend supplies absolute
//!   paths and canonicalizing here would silently rewrite symlinks.
//! - The bin grid `w = ceil(span / n)` can overshoot the viewport, leaving
//!   trailing bins that cover no samples (see `Signal::envelope` in
//!   `signal.rs`). The frame still carries exactly `n` bins; empty trailing
//!   bins repeat the previous bin's values.

mod envelope;
mod signal;

// v2 service modules (docs/ipc-contract.md "v2 additions"). Each module owns
// its file and adds `impl SignalRepository` blocks; none of them may edit
// this file's shared state except through the crate-private helpers below.
// `pub` so the Tauri command layer can name the IPC types (they are part of
// the crate's public API surface per the contract).
pub mod analysis;
pub mod awre;
pub mod devices;
pub mod generator;
pub mod project;
pub mod simulator;
pub mod spectrogram;

pub use analysis::{AnalysisParams, AnalysisResult, EstimatedParams, MessageInfo};
pub use awre::FieldLabel;
pub use devices::{CaptureConfig, CaptureHandle, CaptureStats, DeviceInfo};
pub use generator::GeneratorParams;
pub use project::{ProjectSignal, ProjectState, PROJECT_VERSION};
pub use simulator::{SimRuleInfo, SimTranscriptEntry};

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use urh::iq::IqArray;

use signal::Signal;

/// Default sample rate in Hz when `open` gets no explicit rate — the URH
/// default, matching `urh-cli`.
pub const DEFAULT_SAMPLE_RATE: f64 = 1_000_000.0;

/// Inclusive upper bound for envelope bin counts (contract).
pub const MAX_BINS: u32 = 65_536;

/// Metadata describing an open signal (contract: "Model types").
///
/// Serialized as-is over Tauri JSON IPC; the frontend codes against these
/// field names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalMeta {
    /// `"sig-<n>"`, unique per repository instance.
    pub signal_id: String,
    /// Path as passed to [`SignalRepository::open`] (frontends pass absolute paths).
    pub path: String,
    /// File name only.
    pub display_name: String,
    /// `"complex"` | `"complex16s"` | `"complex16u"` | `"complex32s"` | `"complex32u"`.
    pub format: String,
    /// Complex samples in the signal.
    pub sample_count: u64,
    /// Sample rate in Hz; metadata only (time is always derived).
    pub sample_rate: f64,
    /// `sample_count / sample_rate`.
    pub duration_secs: f64,
}

/// Waveform component selection for envelope queries (contract: 3 = Phase and
/// 4 = Frequency are reserved and must be rejected by the IPC layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveformMode {
    /// Per-bin min/max of I and Q; frame mode value 1.
    Iq = 1,
    /// Per-bin min/max of the magnitude |s|; frame mode value 2.
    Amplitude = 2,
}

impl WaveformMode {
    /// Wire value used in the frame header and the Tauri command.
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    /// Parse a wire value; reserved codes (3, 4) and anything else yield `None`.
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(WaveformMode::Iq),
            2 => Some(WaveformMode::Amplitude),
            _ => None,
        }
    }
}

/// Service-layer error. Frontends branch on [`code`](AppError::code), never
/// parse [`message`](AppError::message); over IPC it serializes as
/// `{ code: String, message: String }`.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct AppError {
    /// Stable machine-readable error code.
    pub code: &'static str,
    /// Human-readable detail; not stable, not for programmatic use.
    pub message: String,
}

impl AppError {
    /// No signal with this id is open (never opened, or already closed).
    pub const CODE_UNKNOWN_SIGNAL: &str = "unknown-signal";
    /// `open` targeted a path that does not exist.
    pub const CODE_FILE_NOT_FOUND: &str = "file-not-found";
    /// A sample range or bin count is outside its contract bounds.
    pub const CODE_INVALID_RANGE: &str = "invalid-range";
    /// A scalar argument (e.g. sample rate) is invalid.
    pub const CODE_INVALID_ARGUMENT: &str = "invalid-argument";
    /// Filesystem error other than "not found".
    pub const CODE_IO: &str = "io";
    /// Unexpected internal condition (bug).
    pub const CODE_INTERNAL: &str = "internal";

    pub fn unknown_signal(signal_id: &str) -> Self {
        AppError {
            code: Self::CODE_UNKNOWN_SIGNAL,
            message: format!("no open signal with id {signal_id}"),
        }
    }

    pub fn invalid_range(message: impl Into<String>) -> Self {
        AppError {
            code: Self::CODE_INVALID_RANGE,
            message: message.into(),
        }
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        AppError {
            code: Self::CODE_INVALID_ARGUMENT,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        AppError {
            code: Self::CODE_INTERNAL,
            message: message.into(),
        }
    }

    /// Map an I/O failure from `open`: a missing file is reported as
    /// `file-not-found`, everything else (permissions, directories, ...) as `io`.
    fn from_io(path: &str, err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::NotFound {
            AppError {
                code: Self::CODE_FILE_NOT_FOUND,
                message: format!("{path}: {err}"),
            }
        } else {
            AppError {
                code: Self::CODE_IO,
                message: format!("{path}: {err}"),
            }
        }
    }
}

/// Repository of open signals (contract: "Service API").
///
/// All methods take `&self` and the type is `Send + Sync`, so the Tauri shell
/// can keep one instance in shared state. Envelope queries clone an `Arc` to
/// the signal and compute lock-free — the table lock is only held for the map
/// operations themselves (contract requirement 4).
pub struct SignalRepository {
    signals: RwLock<HashMap<String, Arc<Signal>>>,
    /// Monotonic id counter; `"sig-<n>"` ids are unique per instance.
    next_id: AtomicU64,
}

impl SignalRepository {
    /// An empty repository.
    pub fn new() -> Self {
        SignalRepository {
            signals: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Load an IQ file (whole file in memory, MVP storage) and register it
    /// under a fresh `"sig-<n>"` id.
    ///
    /// `sample_rate` is metadata-only and defaults to [`DEFAULT_SAMPLE_RATE`];
    /// an explicit rate must be finite and positive.
    pub fn open(&self, path: &str, sample_rate: Option<f64>) -> Result<SignalMeta, AppError> {
        let rate = match sample_rate {
            None => DEFAULT_SAMPLE_RATE,
            Some(rate) if rate.is_finite() && rate > 0.0 => rate,
            Some(rate) => {
                return Err(AppError::invalid_argument(format!(
                    "sample rate must be a positive finite number, got {rate}"
                )))
            }
        };
        let iq = IqArray::from_file(path).map_err(|err| AppError::from_io(path, err))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let signal = Arc::new(Signal::new(format!("sig-{id}"), path, iq, rate));
        let meta = signal.meta();
        self.signals
            .write()
            .expect("signal table lock poisoned")
            .insert(meta.signal_id.clone(), signal);
        Ok(meta)
    }

    /// Remove a signal from the repository (its memory is freed once in-flight
    /// envelope queries finish).
    pub fn close(&self, signal_id: &str) -> Result<(), AppError> {
        self.signals
            .write()
            .expect("signal table lock poisoned")
            .remove(signal_id)
            .map(|_| ())
            .ok_or_else(|| AppError::unknown_signal(signal_id))
    }

    /// Current metadata of one open signal.
    pub fn meta(&self, signal_id: &str) -> Result<SignalMeta, AppError> {
        Ok(self.lookup(signal_id)?.meta())
    }

    /// Metadata of all open signals, sorted by numeric id (`sig-1`, `sig-2`,
    /// ...) so the listing is stable across calls.
    pub fn list(&self) -> Vec<SignalMeta> {
        let signals = self.signals.read().expect("signal table lock poisoned");
        let mut metas: Vec<SignalMeta> = signals.values().map(|s| s.meta()).collect();
        drop(signals);
        metas.sort_by_key(|m| id_number(&m.signal_id));
        metas
    }

    /// Update the metadata-only sample rate of a signal.
    pub fn set_sample_rate(&self, signal_id: &str, rate: f64) -> Result<SignalMeta, AppError> {
        if !rate.is_finite() || rate <= 0.0 {
            return Err(AppError::invalid_argument(format!(
                "sample rate must be a positive finite number, got {rate}"
            )));
        }
        let signal = self.lookup(signal_id)?;
        signal.set_sample_rate(rate);
        Ok(signal.meta())
    }

    /// Compute the min/max waveform envelope of the viewport
    /// `[start, end)` (half-open, integer sample indices) and return it as a
    /// binary frame (see the contract's frame table).
    ///
    /// Validation: `1 <= bins <= `[`MAX_BINS`] and `start < end <=
    /// sample_count`, violations return `invalid-range` instead of panicking
    /// (contract requirement 3). `request_id` is opaque and echoed verbatim
    /// (requirement 5).
    pub fn waveform_envelope(
        &self,
        signal_id: &str,
        start: u64,
        end: u64,
        bins: u32,
        mode: WaveformMode,
        request_id: u32,
    ) -> Result<Vec<u8>, AppError> {
        if bins == 0 || bins > MAX_BINS {
            return Err(AppError::invalid_range(format!(
                "bins must be in 1..={MAX_BINS}, got {bins}"
            )));
        }
        // Clone the Arc, then drop the table lock before computing.
        let signal = self.lookup(signal_id)?;
        let sample_count = signal.sample_count() as u64;
        if start >= end || end > sample_count {
            return Err(AppError::invalid_range(format!(
                "viewport must satisfy start < end <= {sample_count}, got [{start}, {end})"
            )));
        }
        let extrema = signal.envelope(start as usize, end as usize, bins as usize, mode);
        Ok(envelope::encode_frame(request_id, &extrema, mode))
    }

    /// Look up a signal and clone the `Arc`; the only place the table lock is
    /// taken on the query path.
    fn lookup(&self, signal_id: &str) -> Result<Arc<Signal>, AppError> {
        self.signals
            .read()
            .expect("signal table lock poisoned")
            .get(signal_id)
            .cloned()
            .ok_or_else(|| AppError::unknown_signal(signal_id))
    }

    /// Allocate a fresh unique id with the given prefix (`"sig-"`,
    /// `"rec-"`, `"gen-"` …). v2 modules register signals they build in
    /// memory (generator output, finished recordings) through this plus
    /// [`Self::register`].
    pub(crate) fn alloc_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}{}",
            self.next_id.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

    /// Register a signal built in memory under `id` (from [`Self::alloc_id`])
    /// and return the shared handle.
    pub(crate) fn register(&self, id: String, signal: Signal) -> Arc<Signal> {
        let arc = Arc::new(signal);
        self.signals
            .write()
            .expect("signal table lock poisoned")
            .insert(id, arc.clone());
        arc
    }
}

impl Default for SignalRepository {
    fn default() -> Self {
        SignalRepository::new()
    }
}

/// Numeric part of a `"sig-<n>"` id (for stable `list` ordering); unrecognized
/// ids sort last.
fn id_number(signal_id: &str) -> u64 {
    signal_id
        .strip_prefix("sig-")
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use urh::Complex32;

    /// Deterministic LCG shared with the signal module tests.
    struct Lcg(u64);

    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 40) as f32 / 8_388_608.0 - 1.0
        }

        fn next_sample(&mut self) -> Complex32 {
            Complex32::new(self.next_f32(), self.next_f32())
        }
    }

    fn lcg_samples(n: usize) -> Vec<Complex32> {
        let mut gen = Lcg(0x243F_6A88_85A3_08D3);
        (0..n).map(|_| gen.next_sample()).collect::<Vec<_>>()
    }

    /// Write samples to a lossless f32 `.complex` file inside `dir` and return
    /// its path (f32 roundtrips exactly, so tests can compare against the
    /// in-memory samples).
    fn write_complex(dir: &tempfile::TempDir, name: &str, samples: &[Complex32]) -> String {
        let path = dir.path().join(name);
        IqArray::from_vec(samples.to_vec())
            .to_file(path.to_str().unwrap())
            .unwrap();
        path.to_str().unwrap().to_string()
    }

    /// Naive per-bin envelope recompute straight from the contract formula:
    /// bin k covers `[start + k*w, min(start + (k+1)*w, end))` with
    /// `w = ceil(span / n)`, `n = min(bins, span)`; trailing bins that start
    /// at or after `end` repeat the previous bin. Returns per-bin payload
    /// values in frame order.
    fn naive_envelope(
        samples: &[Complex32],
        start: usize,
        end: usize,
        bins: usize,
        mode: WaveformMode,
    ) -> Vec<Vec<f32>> {
        let span = end - start;
        let n = bins.min(span);
        let w = span.div_ceil(n);
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(n);
        let mut cursor = start;
        for _ in 0..n {
            if cursor >= end {
                out.push(out.last().expect("bin 0 always covers start").clone());
                continue;
            }
            let bin_end = (cursor + w).min(end);
            let mut min_i = f32::INFINITY;
            let mut max_i = f32::NEG_INFINITY;
            let mut min_q = f32::INFINITY;
            let mut max_q = f32::NEG_INFINITY;
            let mut min_m = f32::INFINITY;
            let mut max_m = f32::NEG_INFINITY;
            for s in &samples[cursor..bin_end] {
                min_i = min_i.min(s.re);
                max_i = max_i.max(s.re);
                min_q = min_q.min(s.im);
                max_q = max_q.max(s.im);
                let m = s.norm();
                min_m = min_m.min(m);
                max_m = max_m.max(m);
            }
            out.push(match mode {
                WaveformMode::Iq => vec![min_i, max_i, min_q, max_q],
                WaveformMode::Amplitude => vec![min_m, max_m],
            });
            cursor = bin_end;
        }
        out
    }

    #[test]
    fn test_repository_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SignalRepository>();
        // Moving a repository across a thread also proves Send at runtime.
        let repo = SignalRepository::new();
        let handle = std::thread::spawn(move || repo.list());
        assert!(handle.join().unwrap().is_empty());
    }

    #[test]
    fn test_open_meta_list_close_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_complex(&dir, "ask.complex16s", &lcg_samples(100));
        let repo = SignalRepository::new();

        let meta = repo.open(&path, None).unwrap();
        assert_eq!(meta.signal_id, "sig-1");
        assert_eq!(meta.path, path);
        assert_eq!(meta.display_name, "ask.complex16s");
        assert_eq!(meta.format, "complex16s");
        assert_eq!(meta.sample_count, 100);
        assert_eq!(meta.sample_rate, DEFAULT_SAMPLE_RATE);
        assert!((meta.duration_secs - 100.0 / DEFAULT_SAMPLE_RATE).abs() < 1e-15);

        assert_eq!(repo.list(), vec![meta.clone()]);
        assert_eq!(repo.meta("sig-1").unwrap(), meta);

        // Explicit sample rate, echoed by meta and duration.
        let meta = repo.set_sample_rate("sig-1", 2.5e6).unwrap();
        assert_eq!(meta.sample_rate, 2.5e6);
        assert!((meta.duration_secs - 100.0 / 2.5e6).abs() < 1e-15);
        assert_eq!(repo.meta("sig-1").unwrap().sample_rate, 2.5e6);

        // Re-opening the same file yields a distinct id; list stays sorted.
        let meta_b = repo.open(&path, Some(500.0)).unwrap();
        assert_eq!(meta_b.signal_id, "sig-2");
        assert_eq!(meta_b.sample_rate, 500.0);
        let listed = repo.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].signal_id, "sig-1");
        assert_eq!(listed[1].signal_id, "sig-2");

        // Close removes the signal; further access is unknown-signal.
        repo.close("sig-1").unwrap();
        assert_eq!(repo.list(), vec![meta_b]);
        assert_eq!(repo.meta("sig-1").unwrap_err().code, "unknown-signal");
        assert_eq!(repo.close("sig-1").unwrap_err().code, "unknown-signal");

        // Format detection follows the file extension.
        let path = write_complex(&dir, "plain.complex", &lcg_samples(10));
        assert_eq!(repo.open(&path, None).unwrap().format, "complex");
        let path = write_complex(&dir, "wide.cs16", &lcg_samples(10));
        assert_eq!(repo.open(&path, None).unwrap().format, "complex32s");

        // Empty files open (zero samples); any viewport on them is invalid.
        let path = write_complex(&dir, "empty.complex", &[]);
        let meta = repo.open(&path, None).unwrap();
        assert_eq!(meta.sample_count, 0);
        assert_eq!(meta.duration_secs, 0.0);
    }

    #[test]
    fn test_validation_error_codes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_complex(&dir, "v.complex", &lcg_samples(50));
        let repo = SignalRepository::new();
        repo.open(&path, None).unwrap();
        let id = "sig-1";
        let mode = WaveformMode::Iq;

        // unknown-signal: never opened / closed / fabricated id.
        for code in [
            repo.meta("sig-99").unwrap_err().code,
            repo.close("sig-99").unwrap_err().code,
            repo.set_sample_rate("sig-99", 1e6).unwrap_err().code,
            repo.waveform_envelope("sig-99", 0, 10, 8, mode, 0)
                .unwrap_err()
                .code,
        ] {
            assert_eq!(code, "unknown-signal");
        }

        // invalid-range: bins bounds.
        assert_eq!(
            repo.waveform_envelope(id, 0, 50, 0, mode, 0)
                .unwrap_err()
                .code,
            "invalid-range"
        );
        assert_eq!(
            repo.waveform_envelope(id, 0, 50, MAX_BINS + 1, mode, 0)
                .unwrap_err()
                .code,
            "invalid-range"
        );
        // MAX_BINS itself is fine.
        assert!(repo.waveform_envelope(id, 0, 50, MAX_BINS, mode, 0).is_ok());

        // invalid-range: start < end <= sample_count.
        for (start, end) in [(0u64, 0u64), (10, 10), (20, 5), (0, 51), (50, 51)] {
            assert_eq!(
                repo.waveform_envelope(id, start, end, 8, mode, 0)
                    .unwrap_err()
                    .code,
                "invalid-range",
                "viewport [{start}, {end})"
            );
        }
        // The boundary itself is fine.
        assert!(repo.waveform_envelope(id, 0, 50, 8, mode, 0).is_ok());

        // invalid-argument: bad sample rates on open and set_sample_rate.
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                repo.open(&path, Some(rate)).unwrap_err().code,
                "invalid-argument",
                "open rate {rate}"
            );
            assert_eq!(
                repo.set_sample_rate(id, rate).unwrap_err().code,
                "invalid-argument",
                "set rate {rate}"
            );
        }

        // file-not-found vs io.
        assert_eq!(
            repo.open("/nonexistent/dir/signal.complex", None)
                .unwrap_err()
                .code,
            "file-not-found"
        );
        assert_eq!(
            repo.open(dir.path().to_str().unwrap(), None)
                .unwrap_err()
                .code,
            "io" // reading a directory fails with EISDIR, not NotFound
        );
    }

    #[test]
    fn test_envelope_matches_naive_recompute() {
        let dir = tempfile::tempdir().unwrap();
        let samples = lcg_samples(5000);
        let path = write_complex(&dir, "bf.complex", &samples);
        let repo = SignalRepository::new();
        repo.open(&path, None).unwrap();

        // (start, end, bins): unaligned starts, non-power-of-two bin counts,
        // LOD-width and direct-scan-width bins, degenerate (bins >= span,
        // single-sample span), bins == 1 over the whole signal, and grids
        // whose ceil-width leaves empty trailing bins.
        let viewports: &[(u64, u64, u32)] = &[
            (0, 5000, 1024),
            (37, 4971, 333),
            (13, 4913, 9), // w = 545 >= 2 blocks: LOD path
            (100, 2600, 511),
            (250, 5000, 500), // w = 10, 25 empty trailing bins
            (0, 5000, 1),
            (0, 300, 299),     // w = 2, 149 empty trailing bins
            (4999, 5000, 100), // degenerate: clamps to 1 bin
            (1234, 1240, 8),   // degenerate: clamps to 6 bins
        ];
        for &(start, end, bins) in viewports {
            let n = (bins as usize).min((end - start) as usize);
            for mode in [WaveformMode::Iq, WaveformMode::Amplitude] {
                let request_id = 0xBEEF;
                let frame = repo
                    .waveform_envelope("sig-1", start, end, bins, mode, request_id)
                    .unwrap();
                let decoded = envelope::test_util::decode(&frame);
                assert_eq!(decoded.request_id, request_id);
                assert_eq!(decoded.mode, mode.as_u32());
                assert_eq!(decoded.flags, 0);
                assert_eq!(
                    decoded.n_bins as usize, n,
                    "viewport ({start}, {end}, {bins})"
                );

                let want =
                    naive_envelope(&samples, start as usize, end as usize, bins as usize, mode);
                assert_eq!(want.len(), n);
                let got_bins = decoded.values.chunks_exact(decoded.stride);
                for (k, (got, want_bin)) in got_bins.zip(&want).enumerate() {
                    assert_eq!(
                        got,
                        want_bin.as_slice(),
                        "viewport ({start}, {end}, {bins}), mode {:?}, bin {k}",
                        mode.as_u32()
                    );
                }
            }
        }
    }

    #[test]
    fn test_concurrent_queries_and_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let samples = lcg_samples(20_000);
        let path = write_complex(&dir, "conc.complex", &samples);
        let repo = Arc::new(SignalRepository::new());
        repo.open(&path, None).unwrap();

        let querier = {
            let repo = Arc::clone(&repo);
            std::thread::spawn(move || {
                for i in 0..50u32 {
                    let start = (i as usize * 37) % 10_000;
                    let end = start + 5_000 + (i as usize % 500);
                    for mode in [WaveformMode::Iq, WaveformMode::Amplitude] {
                        let frame = repo
                            .waveform_envelope("sig-1", start as u64, end as u64, 64, mode, i)
                            .unwrap();
                        assert!(frame.len() > envelope::HEADER_LEN);
                    }
                }
            })
        };
        let rate_flipper = {
            let repo = Arc::clone(&repo);
            std::thread::spawn(move || {
                for i in 0..50 {
                    repo.set_sample_rate("sig-1", 1e6 + i as f64).unwrap();
                    // sig-1 stays open for the whole test; the churner may
                    // transiently hold a second signal, so "exactly one" here
                    // would be a race.
                    assert!(repo.list().iter().any(|m| m.signal_id == "sig-1"));
                    repo.meta("sig-1").unwrap();
                }
            })
        };
        // Churn the table (write lock) while queries run lock-free.
        let churner = {
            let repo = Arc::clone(&repo);
            let path2 = write_complex(&dir, "churn.complex", &lcg_samples(100));
            std::thread::spawn(move || {
                for _ in 0..20 {
                    let meta = repo.open(&path2, None).unwrap();
                    assert!(repo
                        .waveform_envelope(&meta.signal_id, 0, 100, 10, WaveformMode::Iq, 0)
                        .is_ok());
                    repo.close(&meta.signal_id).unwrap();
                }
            })
        };

        for handle in [querier, rate_flipper, churner] {
            handle.join().expect("worker thread panicked");
        }
        assert_eq!(repo.list().len(), 1);
    }

    #[test]
    #[ignore = "slow: generates a 10M-sample (80 MB) signal; run with --ignored"]
    fn test_ten_million_sample_envelope() {
        let n = 10_000_000usize;
        // Sine + periodic spikes: dense data plus guaranteed peaks at every
        // 100k-th sample (amplitude 1.5 vs the 0.3 sine).
        let mut samples = Vec::with_capacity(n);
        for k in 0..n {
            let phase = 2.0 * std::f32::consts::PI * k as f32 / 97.0;
            samples.push(if k % 100_000 == 0 {
                Complex32::new(1.5, 0.0)
            } else {
                Complex32::new(0.3 * phase.sin(), 0.3 * phase.cos())
            });
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_complex(&dir, "big.complex", &samples);

        let repo = SignalRepository::new();
        let meta = repo.open(&path, None).unwrap();
        assert_eq!(meta.sample_count, n as u64);

        for mode in [WaveformMode::Iq, WaveformMode::Amplitude] {
            let frame = repo
                .waveform_envelope("sig-1", 0, n as u64, 1024, mode, 7)
                .unwrap();
            let per_bin = if mode == WaveformMode::Iq { 16 } else { 8 };
            assert_eq!(frame.len(), envelope::HEADER_LEN + per_bin * 1024);
            let decoded = envelope::test_util::decode(&frame);
            assert_eq!(decoded.request_id, 7);
            assert_eq!(decoded.n_bins, 1024);

            // Spot-check bins against a naive recompute of just those bins
            // (w = ceil(10M / 1024) = 9766): first, last, middle and the bin
            // holding the sample-1M spike.
            let w = n.div_ceil(1024);
            let spike_bin = 1_000_000 / w;
            for k in [0usize, 1, 500, 1023, spike_bin] {
                let (a, b) = ((k * w).min(n), (((k + 1) * w).min(n)));
                let want = naive_envelope(&samples, a, b, 1, mode).remove(0);
                let got = &decoded.values[k * decoded.stride..(k + 1) * decoded.stride];
                assert_eq!(got, want.as_slice(), "mode {:?}, bin {k}", mode.as_u32());
                if k == spike_bin && mode == WaveformMode::Amplitude {
                    // The 1.5 spike at sample 1M must survive the reduction.
                    assert_eq!(got[1], 1.5);
                }
            }
        }
    }
}
