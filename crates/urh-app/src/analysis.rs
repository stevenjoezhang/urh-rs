//! Protocol analysis service — v2 contract section "Protocol analysis (P2)"
//! (docs/ipc-contract.md).
//!
//! [`SignalRepository::analyze`] runs the proven CLI flow
//! (`src/bin/urh-cli.rs::run_analysis`) as a parameterizable job:
//!
//! 1. resolve the optional half-open `range` against the signal (returned
//!    sample positions are always absolute; the range offset is added back),
//! 2. fill every `None` in [`AnalysisParams`] by estimation, using
//!    user-supplied fields verbatim,
//! 3. demodulate (`afp_demod`) and run [`urh::protocol::ProtocolAnalyzer`],
//! 4. project the analyzer's messages into wire-friendly [`MessageInfo`]s.
//!
//! Jobs run under a process-wide registry so the UI can cancel them
//! ([`SignalRepository::cancel_analysis`]); duplicate `job_id`s are rejected
//! while running. The registry is a static in this module because
//! `SignalRepository` cannot gain fields (see the note on [`jobs`]).
//!
//! Lock discipline (contract requirement 4): the signal table lock is only
//! held inside `lookup`, which clones the `Arc`; all DSP runs lock-free on
//! that clone.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use urh::autointerpret as ai;
use urh::protocol::{Message, ProtocolAnalyzer};
use urh::Modulation;

use crate::signal::Signal;
use crate::{AppError, SignalRepository};

// ---------------------------------------------------------------------------
// Wire types (contract: "Protocol analysis")
// ---------------------------------------------------------------------------

/// Analysis parameterization; every field is optional and `None` means
/// "estimate this value" (contract: "all fields optional = auto").
///
/// Missing JSON keys deserialize to `None` (`#[serde(default)]`), so a Tauri
/// command can pass a partial object. User-supplied fields are used verbatim;
/// a supplied `modulation` mirrors `urh-cli.rs::estimate_with_modulation`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AnalysisParams {
    /// `"ASK" | "FSK" | "PSK" | "GFSK" | "OQPSK"` (case-insensitive). As a
    /// CLI-parity extension `"OOK"` is also accepted and analyzed as ASK with
    /// OOK segment merging; the reported modulation is `"ASK"`.
    #[serde(default)]
    pub modulation: Option<String>,
    /// Samples per symbol; must be >= 1 when supplied.
    #[serde(default)]
    pub bit_length: Option<u32>,
    /// Demodulation decision threshold; must be finite when supplied.
    #[serde(default)]
    pub center: Option<f32>,
    /// Noise magnitude threshold; must be finite when supplied.
    #[serde(default)]
    pub noise_threshold: Option<f32>,
    /// Symbol-length tolerance; must fit `u16` (the analyzer's limit).
    #[serde(default)]
    pub tolerance: Option<u32>,
}

/// The interpretation actually used for the analysis (estimated fields,
/// user-supplied fields, or a mix). Mirrors the header the CLI prints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimatedParams {
    /// Canonical modulation name (`Modulation::as_str`).
    pub modulation: String,
    pub bit_length: u32,
    pub center: f32,
    pub noise_threshold: f32,
    pub tolerance: u32,
}

/// One analyzed message, in wire form (contract: "Protocol analysis").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageInfo {
    pub index: usize,
    /// Bit count of the undecoded message (`Message::len`).
    pub bit_count: usize,
    /// Pause after the message, in samples.
    pub pause: u64,
    /// Undecoded bits, `"0101…"`.
    pub plain_bits: String,
    /// Absolute sample index of the first bit.
    pub start_sample: u64,
    /// Exclusive absolute sample index of the message end (data + pause).
    pub end_sample: u64,
}

/// Result of [`SignalRepository::analyze`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub params: EstimatedParams,
    pub messages: Vec<MessageInfo>,
}

/// New error code for cancelled analysis jobs (contract: "New `AppError`
/// code: `cancelled`"). AppError has no constructor for it, hence the struct
/// literal in [`cancelled_error`].
pub const CODE_CANCELLED: &str = "cancelled";

fn cancelled_error() -> AppError {
    AppError {
        code: CODE_CANCELLED,
        message: "analysis was cancelled before completion".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Job registry (process-wide static)
// ---------------------------------------------------------------------------

/// Cancellation flags of running analysis jobs, keyed by `job_id`.
///
/// `SignalRepository` cannot gain fields (the v1 type is frozen), so the
/// registry lives in a process-wide static. Tradeoff, accepted per the
/// contract's caching note: the registry is shared by every repository
/// instance in the process, so `job_id`s only need to be unique among
/// concurrently running jobs process-wide, not per repository. Entries are
/// added by [`SignalRepository::analyze`] and removed by [`JobGuard`] when
/// the call returns (normally, with an error, or on panic), so the map only
/// holds flags of in-flight jobs.
static JOBS: OnceLock<RwLock<HashMap<u32, Arc<AtomicBool>>>> = OnceLock::new();

fn jobs() -> &'static RwLock<HashMap<u32, Arc<AtomicBool>>> {
    JOBS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// RAII deregistration for a job: dropping it (return, `?`, or unwinding)
/// removes the flag from [`jobs`].
struct JobGuard {
    job_id: u32,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        jobs()
            .write()
            .expect("job registry lock poisoned")
            .remove(&self.job_id);
    }
}

/// Cancellation checkpoint (contract: the job returns `Err(code =
/// "cancelled")` at the next checkpoint). Library calls such as
/// `afp_demod`/`analyze_signal` are atomic, so checkpoints sit at stage
/// boundaries and inside the estimation segment loop; a cancel therefore
/// takes effect after the current library call finishes, not mid-way through
/// it.
pub(crate) fn check_cancelled(flag: &AtomicBool) -> Result<(), AppError> {
    if flag.load(Ordering::Relaxed) {
        Err(cancelled_error())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// [`AnalysisParams`] after validation: the modulation string is parsed and
/// bad scalars rejected, so the heavy pipeline runs on trusted values only.
pub(crate) struct ValidatedParams {
    /// `(modulation, ook)` — `ook` requests ASK segment merging for the
    /// `"OOK"` extension, exactly like `--modulation OOK` in the CLI.
    pub(crate) modulation: Option<(Modulation, bool)>,
    pub(crate) bit_length: Option<u32>,
    pub(crate) center: Option<f32>,
    pub(crate) noise_threshold: Option<f32>,
    pub(crate) tolerance: Option<u32>,
}

impl ValidatedParams {
    /// Validate an [`AnalysisParams`] (contract requirement 3: bad input
    /// returns an error, never a panic).
    pub(crate) fn validate(params: &AnalysisParams) -> Result<Self, AppError> {
        let modulation = match &params.modulation {
            None => None,
            // CLI-parity extension: "OOK" = ASK + OOK segment merge.
            Some(s) if s.eq_ignore_ascii_case("OOK") => Some((Modulation::Ask, true)),
            Some(s) => Some((
                Modulation::from_str(s).ok_or_else(|| {
                    AppError::invalid_argument(format!(
                        "unknown modulation {s:?} (expected ASK|FSK|PSK|GFSK|OQPSK)"
                    ))
                })?,
                false,
            )),
        };
        if let Some(0) = params.bit_length {
            return Err(AppError::invalid_argument(
                "bit_length must be at least 1 sample per symbol",
            ));
        }
        if let Some(t) = params.tolerance {
            if t > u16::MAX as u32 {
                return Err(AppError::invalid_argument(format!(
                    "tolerance must be at most {} (analyzer limit), got {t}",
                    u16::MAX
                )));
            }
        }
        if let Some(c) = params.center {
            if !c.is_finite() {
                return Err(AppError::invalid_argument(format!(
                    "center must be a finite number, got {c}"
                )));
            }
        }
        if let Some(n) = params.noise_threshold {
            if !n.is_finite() {
                return Err(AppError::invalid_argument(format!(
                    "noise_threshold must be a finite number, got {n}"
                )));
            }
        }
        Ok(ValidatedParams {
            modulation,
            bit_length: params.bit_length,
            center: params.center,
            noise_threshold: params.noise_threshold,
            tolerance: params.tolerance,
        })
    }

    /// Whether every value the analyzer needs is user-supplied, in which case
    /// estimation can be skipped entirely (it would be wasted work and its
    /// result overridden anyway).
    fn complete_for_fixed_modulation(&self) -> bool {
        self.bit_length.is_some()
            && self.center.is_some()
            && self.tolerance.is_some()
            && self.noise_threshold.is_some()
    }
}

/// Validate an optional analysis range against a sample count (contract:
/// same rules as envelopes, `start < end <= sample_count`).
pub(crate) fn validate_range_args(
    sample_count: u64,
    range: Option<(u64, u64)>,
) -> Result<(), AppError> {
    if let Some((start, end)) = range {
        if start >= end || end > sample_count {
            return Err(AppError::invalid_range(format!(
                "range must satisfy start < end <= {sample_count}, got [{start}, {end})"
            )));
        }
    }
    Ok(())
}

/// Resolve `range` to `(offset, slice)` where `offset` is added back to
/// relative positions to make them absolute (contract: "returned
/// start_sample/end_sample are absolute"). Re-validates defensively; the
/// slice borrows from `samples`.
pub(crate) fn resolve_range(
    samples: &[urh::Complex32],
    range: Option<(u64, u64)>,
) -> Result<(u64, &[urh::Complex32]), AppError> {
    match range {
        None => Ok((0, samples)),
        Some((start, end)) => {
            validate_range_args(samples.len() as u64, Some((start, end)))?;
            let start = start as usize;
            Ok((start as u64, &samples[start..end as usize]))
        }
    }
}

// ---------------------------------------------------------------------------
// Estimation
// ---------------------------------------------------------------------------

/// The interpretation the analyzer will run with.
struct WorkParams {
    modulation: Modulation,
    bit_length: u32,
    center: f32,
    noise: f32,
    tolerance: u32,
}

/// Override estimated values with user-supplied ones (contract: "user-supplied
/// fields are used verbatim").
fn apply_overrides(base: WorkParams, params: &ValidatedParams) -> WorkParams {
    WorkParams {
        modulation: base.modulation,
        bit_length: params.bit_length.unwrap_or(base.bit_length),
        center: params.center.unwrap_or(base.center),
        noise: params.noise_threshold.unwrap_or(base.noise),
        tolerance: params.tolerance.unwrap_or(base.tolerance),
    }
}

/// Interpretation with a fixed modulation, a faithful port of
/// `urh-cli.rs::estimate_with_modulation` (itself mirroring
/// `AutoInterpretation.estimate(modulation=X)`). Differences from the CLI,
/// both intentional:
///
/// - a user-supplied `noise_threshold` is honored (the CLI has no flag for
///   it and always auto-detects),
/// - the per-segment loop takes a cancellation checkpoint.
///
/// Returns `Ok(None)` when estimation fails (the CLI's `?`-aborts); the
/// caller turns that into an `invalid-argument` error. Faithful to the CLI,
/// a single segment whose center cannot be detected aborts the whole
/// estimation.
fn estimate_fixed_modulation(
    slice: &[urh::Complex32],
    modulation: Modulation,
    ook: bool,
    user_noise: Option<f32>,
    flag: &AtomicBool,
) -> Result<Option<WorkParams>, AppError> {
    let magnitudes: Vec<f32> = slice.iter().map(|c| c.norm()).collect();
    let noise = user_noise.unwrap_or_else(|| ai::detect_noise_level(&magnitudes));
    let mut message_indices = ai::segment_messages_from_magnitudes(&magnitudes, noise);
    if ook {
        message_indices = ai::merge_message_segments_for_ook(&message_indices);
    }
    let data = urh::modulation::afp_demod(slice, noise, modulation, 2, 0.1);

    let mut centers: Vec<f64> = Vec::new();
    let mut bit_lengths: Vec<usize> = Vec::new();
    let mut tolerances: Vec<u64> = Vec::new();

    for &(start, end) in &message_indices {
        check_cancelled(flag)?;
        let msg_rect_data = &data[start..end];
        // CLI parity: `?` on detect_center aborts the whole estimation.
        let Some(center) = ai::detect_center(msg_rect_data, None) else {
            return Ok(None);
        };
        let plateau_lengths = ai::get_plateau_lengths(msg_rect_data, center, 25.0);
        let tolerance = match ai::estimate_tolerance_from_plateau_lengths(&plateau_lengths) {
            Some(t) => {
                tolerances.push(t);
                t
            }
            None => 0,
        };
        let merged_lengths = ai::merge_plateau_lengths(&plateau_lengths, Some(tolerance));
        if merged_lengths.len() < 2 {
            continue;
        }
        let bit_length = ai::get_bit_length_from_plateau_lengths(&merged_lengths);
        if bit_length > (tolerance + 1) as usize {
            centers.push(center as f64);
            bit_lengths.push(bit_length);
        }
    }

    if bit_lengths.is_empty() {
        return Ok(None);
    }
    // centers/bit_lengths are pushed in pairs, so centers is non-empty here.
    let center = if modulation == Modulation::Ask {
        // For ASK the center tends to be the minimum of all found centers.
        match ai::min_without_outliers(&centers, 2.0) {
            Some(c) => c,
            None => return Ok(None),
        }
    } else {
        centers.iter().sum::<f64>() / centers.len() as f64
    };
    let bit_length = match ai::get_most_frequent_value(&bit_lengths) {
        Some(b) => b,
        None => return Ok(None),
    };
    let tolerance = if tolerances.is_empty() {
        // Default to 5% of the bit length (CLI fallback).
        std::cmp::max(1, (0.05 * bit_length as f64) as u32)
    } else {
        // Manual median, like the CLI; u64-as-f64 values are never NaN, so
        // total_cmp is a total order here.
        let mut sorted: Vec<f64> = tolerances.iter().map(|&t| t as f64).collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 1 {
            sorted[mid] as u32
        } else {
            ((sorted[mid - 1] + sorted[mid]) / 2.0) as u32
        }
    };

    Ok(Some(WorkParams {
        modulation,
        bit_length: bit_length as u32,
        center: center as f32,
        noise,
        tolerance,
    }))
}

/// Fill every `None` of `params` for the given slice.
///
/// - No modulation supplied: [`ai::estimate_with_noise`] (which is the CLI's
///   `auto_detect` when no noise is supplied) detects the modulation and
///   every parameter; user fields then override.
/// - Modulation supplied: [`estimate_fixed_modulation`] (CLI parity), skipped
///   entirely when the user supplied bit length, center, tolerance and noise.
fn estimate_work_params(
    slice: &[urh::Complex32],
    params: &ValidatedParams,
    flag: &AtomicBool,
) -> Result<WorkParams, AppError> {
    match params.modulation {
        None => match ai::estimate_with_noise(slice, params.noise_threshold) {
            Some(est) => Ok(apply_overrides(
                WorkParams {
                    modulation: est.modulation,
                    bit_length: est.bit_length as u32,
                    center: est.center,
                    noise: est.noise_threshold,
                    tolerance: est.tolerance,
                },
                params,
            )),
            None => Err(AppError::invalid_argument(
                "could not auto-detect an interpretation for this signal (or range); \
                 supply `modulation` and optionally the remaining parameters explicitly",
            )),
        },
        Some((modulation, ook)) => {
            if params.complete_for_fixed_modulation() {
                // Everything is user-supplied; the placeholders below are
                // immediately replaced by `apply_overrides`.
                Ok(apply_overrides(
                    WorkParams {
                        modulation,
                        bit_length: 1,
                        center: 0.0,
                        noise: 0.0,
                        tolerance: 0,
                    },
                    params,
                ))
            } else {
                match estimate_fixed_modulation(
                    slice,
                    modulation,
                    ook,
                    params.noise_threshold,
                    flag,
                )? {
                    Some(wp) => Ok(apply_overrides(wp, params)),
                    None => Err(AppError::invalid_argument(format!(
                        "could not estimate an interpretation with modulation {}; supply \
                         bit_length, center, tolerance and noise_threshold explicitly",
                        modulation.as_str()
                    ))),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Core pipeline (shared with the AWRE service)
// ---------------------------------------------------------------------------

/// Internal analysis outcome: the wire result plus the analyzer itself, so
/// [`crate::awre`] can feed the messages to the FormatFinder without
/// re-running the pipeline.
#[derive(Debug)]
pub(crate) struct AnalysisOutcome {
    pub(crate) params: EstimatedParams,
    pub(crate) messages: Vec<MessageInfo>,
    pub(crate) analyzer: ProtocolAnalyzer,
}

/// Exclusive end sample of a message, relative to the analyzed slice.
///
/// Deviation from the contract's literal formula
/// (`end_sample = bit_sample_pos.last() + pause`), which was written for the
/// Python layout where the trailing entry is the *data* end: in this port the
/// trailing `bit_sample_pos` entry **already includes the pause** —
/// mid-signal messages store `[..., pause_start, pause_end]` and the final
/// message stores `[..., data_end + pause]` (see `ppseq_to_bits`). So
/// `last()` alone equals the contract's intended `data_end + pause`; adding
/// `pause` again would double-count it. The value is clamped to the analyzed
/// extent as a defensive bound.
fn message_end_sample(msg: &Message, span: u64) -> u64 {
    msg.bit_sample_pos.last().copied().unwrap_or(0).min(span)
}

/// The full analysis pipeline on one signal (see the module docs). Shared by
/// [`SignalRepository::analyze`] and [`crate::awre`]; callers wrap it in
/// `catch_unwind` because the underlying library calls predate the GUI and
/// are not guaranteed panic-free.
pub(crate) fn analyze_samples(
    signal: &Signal,
    range: Option<(u64, u64)>,
    params: &ValidatedParams,
    flag: &AtomicBool,
) -> Result<AnalysisOutcome, AppError> {
    let samples = signal.samples();
    let (offset, slice) = resolve_range(samples, range)?;
    if slice.is_empty() {
        return Err(AppError::invalid_range(
            "analysis needs at least one sample (signal or range is empty)",
        ));
    }

    let wp = estimate_work_params(slice, params, flag)?;
    check_cancelled(flag)?;

    // Demodulation and message segmentation, exactly like the CLI's
    // `run_analysis` (modulation order 2 = 1 bit/symbol, Costas bandwidth
    // 0.1, pause threshold 8 = the Signal default).
    let qad = urh::modulation::afp_demod(slice, wp.noise, wp.modulation, 2, 0.1);
    check_cancelled(flag)?;
    let mut analyzer = ProtocolAnalyzer::new();
    analyzer.analyze_signal(
        &qad,
        wp.center,
        wp.tolerance as u16,
        wp.modulation,
        wp.bit_length,
        1,
        0.1,
        8,
    );
    check_cancelled(flag)?;

    let span = slice.len() as u64;
    let messages = analyzer
        .messages
        .iter()
        .enumerate()
        .map(|(i, msg)| MessageInfo {
            index: i,
            bit_count: msg.len(),
            pause: msg.pause,
            plain_bits: msg.plain_bits_str(),
            start_sample: offset + msg.bit_sample_pos.first().copied().unwrap_or(0).min(span),
            end_sample: offset + message_end_sample(msg, span),
        })
        .collect();

    Ok(AnalysisOutcome {
        params: EstimatedParams {
            modulation: wp.modulation.as_str().to_string(),
            bit_length: wp.bit_length,
            center: wp.center,
            noise_threshold: wp.noise,
            tolerance: wp.tolerance,
        },
        messages,
        analyzer,
    })
}

/// Best-effort panic payload rendering for `catch_unwind` handlers.
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

// ---------------------------------------------------------------------------
// Service API
// ---------------------------------------------------------------------------

impl SignalRepository {
    /// Analyze a signal (contract: "Protocol analysis", v2 P2).
    ///
    /// `range = Some((start, end))` restricts analysis to that half-open
    /// interval; `params` fields set to `None` are auto-detected, the rest
    /// are used verbatim. `job_id` identifies the job in the process-wide
    /// registry: while this call runs, [`Self::cancel_analysis`] with the
    /// same id makes it return `Err(code = "cancelled")` at the next
    /// checkpoint, and a second `analyze` with the same id is rejected with
    /// `invalid-argument`.
    ///
    /// This is a blocking call whose duration is dominated by the library
    /// DSP (roughly a second per 600k samples on current hardware); the
    /// Tauri shell must run it on `spawn_blocking`.
    pub fn analyze(
        &self,
        signal_id: &str,
        range: Option<(u64, u64)>,
        params: &AnalysisParams,
        job_id: u32,
    ) -> Result<AnalysisResult, AppError> {
        // Cheap validation first, so a bad call never occupies the job id.
        let signal = self.lookup(signal_id)?;
        validate_range_args(signal.sample_count() as u64, range)?;
        let params = ValidatedParams::validate(params)?;

        // Register before any long-running work (contract: duplicate job
        // ids are rejected while running, cancels must be visible).
        let flag = Arc::new(AtomicBool::new(false));
        {
            let mut jobs = jobs().write().expect("job registry lock poisoned");
            if jobs.contains_key(&job_id) {
                return Err(AppError::invalid_argument(format!(
                    "analysis job {job_id} is already running"
                )));
            }
            jobs.insert(job_id, flag.clone());
        }
        // Removes the registry entry on every exit path, including panics.
        let _guard = JobGuard { job_id };
        check_cancelled(&flag)?;

        // The library DSP predates the GUI and may panic on pathological
        // input; a panic must surface as `internal`, not take down the
        // process (contract requirement 3).
        let signal_for_job = signal.clone();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            analyze_samples(&signal_for_job, range, &params, &flag)
        }));
        match outcome {
            Ok(Ok(outcome)) => Ok(AnalysisResult {
                params: outcome.params,
                messages: outcome.messages,
            }),
            Ok(Err(err)) => Err(err),
            Err(panic) => Err(AppError::internal(format!(
                "analysis job {job_id} panicked: {}",
                panic_message(panic.as_ref())
            ))),
        }
    }

    /// Request cancellation of a running analysis job (contract: returns
    /// whether the job existed; the job itself returns
    /// `Err(code = "cancelled")` at its next checkpoint).
    pub fn cancel_analysis(&self, job_id: u32) -> bool {
        let jobs = jobs().read().expect("job registry lock poisoned");
        match jobs.get(&job_id) {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::Signal;
    use urh::iq::IqArray;

    const ASK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/ask.complex");
    const FSK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/fsk.complex");
    const ASK_SHORT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/ask_short.complex"
    );

    fn open(repo: &SignalRepository, path: &str) -> String {
        repo.open(path, None).unwrap().signal_id
    }

    fn auto_params() -> AnalysisParams {
        AnalysisParams::default()
    }

    fn params(
        modulation: Option<&str>,
        bit_length: Option<u32>,
        center: Option<f32>,
        noise: Option<f32>,
        tolerance: Option<u32>,
    ) -> AnalysisParams {
        AnalysisParams {
            modulation: modulation.map(str::to_string),
            bit_length,
            center,
            noise_threshold: noise,
            tolerance,
        }
    }

    fn assert_close(a: f32, b: f32, what: &str) {
        assert!((a - b).abs() < 1e-6, "{what}: {a} vs expected {b}");
    }

    /// Expand a hex string into a bit string and truncate to `len` (the
    /// inverse of the CLI's `plain_hex_str` padding).
    fn bits_from_hex(hex: &str, len: usize) -> String {
        let mut bits = String::new();
        for b in hex.bytes() {
            let nibble = char::from(b).to_digit(16).unwrap();
            for k in (0..4).rev() {
                bits.push(if nibble >> k & 1 == 1 { '1' } else { '0' });
            }
        }
        bits.truncate(len);
        bits
    }

    // --- Golden values (CLI `urh-cli analyze`, the acceptance baseline) ---

    #[test]
    fn ask_auto_golden() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        let result = repo.analyze(&id, None, &auto_params(), 101).unwrap();

        assert_eq!(result.params.modulation, "ASK");
        assert_eq!(result.params.bit_length, 300);
        assert_close(result.params.center, 0.025_245, "center");
        assert_close(result.params.noise_threshold, 0.017_5, "noise");
        assert_eq!(result.params.tolerance, 3);

        assert_eq!(result.messages.len(), 1);
        let m = &result.messages[0];
        assert_eq!((m.index, m.bit_count, m.pause), (0, 43, 1694));
        assert_eq!(m.plain_bits, bits_from_hex("b25b6db6c80", 43));
        assert_eq!(m.start_sample, 466);
        // data end 12013 + pause 1694 (see `message_end_sample`).
        assert_eq!(m.end_sample, 13707);
    }

    #[test]
    fn fsk_auto_golden() {
        let repo = SignalRepository::new();
        let id = open(&repo, FSK);
        let result = repo.analyze(&id, None, &auto_params(), 102).unwrap();

        assert_eq!(result.params.modulation, "FSK");
        assert_eq!(result.params.bit_length, 100);
        assert_close(result.params.center, -0.046_264, "center");
        assert_close(result.params.noise_threshold, 0.001_1, "noise");
        assert_eq!(result.params.tolerance, 4);

        assert_eq!(result.messages.len(), 1);
        let m = &result.messages[0];
        assert_eq!((m.index, m.bit_count, m.pause), (0, 177, 50164));
        assert_eq!(
            m.plain_bits,
            bits_from_hex("aaaaaaaac626c626f4dc1d98eef7a427999cd239d3f18", 177)
        );
        assert_eq!(m.start_sample, 6);
        assert_eq!(m.end_sample, 67908);
    }

    #[test]
    fn ask_short_auto_golden() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK_SHORT);
        let result = repo.analyze(&id, None, &auto_params(), 103).unwrap();

        assert_eq!(result.params.modulation, "ASK");
        assert_eq!(result.params.bit_length, 20);
        assert_close(result.params.center, 0.181_258, "center");
        assert_close(result.params.noise_threshold, 0.005_6, "noise");
        assert_eq!(result.params.tolerance, 0);

        assert_eq!(result.messages.len(), 1);
        let m = &result.messages[0];
        assert_eq!((m.index, m.bit_count, m.pause), (0, 8, 0));
        assert_eq!(m.plain_bits, "10101010");
        assert_eq!(m.start_sample, 3);
        assert_eq!(m.end_sample, 131);
    }

    /// CLI: `analyze --modulation ASK tests/data/ask.complex`
    /// (ASK/10/0.053076/0.0175/1, 13 messages).
    #[test]
    fn ask_fixed_modulation_golden() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        let result = repo
            .analyze(&id, None, &params(Some("ASK"), None, None, None, None), 104)
            .unwrap();

        assert_eq!(result.params.modulation, "ASK");
        assert_eq!(result.params.bit_length, 10);
        assert_close(result.params.center, 0.053_076, "center");
        assert_close(result.params.noise_threshold, 0.017_5, "noise");
        assert_eq!(result.params.tolerance, 1);
        assert_eq!(result.messages.len(), 13);
        assert_eq!(
            (result.messages[0].bit_count, result.messages[0].pause),
            (28, 331)
        );
        assert_eq!(result.messages[0].plain_bits, bits_from_hex("70e1860", 28));
    }

    /// CLI: `analyze --modulation OOK` on ask.complex equals the auto result
    /// (OOK is ASK with segment merging, so the 13 tiny messages merge back
    /// into the single 43-bit one).
    #[test]
    fn ask_ook_extension_golden() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        let result = repo
            .analyze(&id, None, &params(Some("OOK"), None, None, None, None), 105)
            .unwrap();
        // OOK reports as its underlying modulation, like the CLI.
        assert_eq!(result.params.modulation, "ASK");
        assert_eq!(result.params.bit_length, 300);
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].bit_count, 43);
    }

    #[test]
    fn fully_supplied_params_skip_estimation() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        let result = repo
            .analyze(
                &id,
                None,
                &params(Some("ASK"), Some(100), Some(0.02), Some(0.0175), Some(5)),
                106,
            )
            .unwrap();
        // Used verbatim (mirrors the CLI header for these inputs).
        assert_eq!(result.params.bit_length, 100);
        assert_close(result.params.center, 0.02, "center");
        assert_close(result.params.noise_threshold, 0.0175, "noise");
        assert_eq!(result.params.tolerance, 5);
    }

    #[test]
    fn auto_params_partial_override() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        // Auto-detect everything except the bit length.
        let result = repo
            .analyze(&id, None, &params(None, Some(150), None, None, None), 107)
            .unwrap();
        assert_eq!(result.params.bit_length, 150);
        // The remaining fields still come from estimation.
        assert_eq!(result.params.modulation, "ASK");
        assert_close(result.params.noise_threshold, 0.0175, "noise");
    }

    // --- Ranges ---

    #[test]
    fn full_range_equals_no_range() {
        let repo = SignalRepository::new();
        let id = open(&repo, FSK);
        let whole = repo
            .analyze(&id, Some((0, 67912)), &auto_params(), 108)
            .unwrap();
        let bare = repo.analyze(&id, None, &auto_params(), 109).unwrap();
        assert_eq!(whole, bare);
    }

    #[test]
    fn range_positions_are_absolute_and_bounded() {
        let repo = SignalRepository::new();
        let id = open(&repo, FSK);
        let result = repo
            .analyze(&id, Some((0, 20000)), &auto_params(), 110)
            .unwrap();
        for m in &result.messages {
            assert!(m.end_sample <= 20000, "end {} exceeds range", m.end_sample);
            assert!(m.start_sample < m.end_sample || m.bit_count == 0);
        }
    }

    #[test]
    fn range_offset_is_added_back() {
        let repo = SignalRepository::new();
        let id = open(&repo, FSK);
        // Cut the first 3 samples of the leading pause: the first bit moves
        // from absolute 6 to relative 3, proving the offset is re-added.
        let result = repo
            .analyze(&id, Some((3, 67912)), &auto_params(), 111)
            .unwrap();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].start_sample, 6);
        assert_eq!(result.messages[0].end_sample, 67908);
    }

    #[test]
    fn range_validation_errors() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK_SHORT); // 131 samples
        for range in [
            Some((5u64, 5u64)),
            Some((10u64, 5u64)),
            Some((0u64, 132u64)),
        ] {
            let err = repo.analyze(&id, range, &auto_params(), 112).unwrap_err();
            assert_eq!(err.code, "invalid-range", "range {range:?}: {err}");
        }
    }

    // --- Degenerate inputs ---

    #[test]
    fn estimation_failure_is_invalid_argument() {
        let repo = SignalRepository::new();
        // All-zero signal: the CLI reports "could not auto-interpret".
        let zeros = IqArray::from_vec(vec![urh::Complex32::default(); 4096]);
        let id = repo.alloc_id("sig-");
        let signal = Signal::new(id.clone(), "<memory>", zeros, 1e6);
        repo.register(id.clone(), signal);

        let err = repo.analyze(&id, None, &auto_params(), 113).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{err}");

        // Fixed modulation does not rescue it (no plateaus to estimate from).
        let err = repo
            .analyze(&id, None, &params(Some("ASK"), None, None, None, None), 114)
            .unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{err}");
    }

    #[test]
    fn empty_signal_is_invalid_range() {
        let repo = SignalRepository::new();
        let empty = IqArray::from_vec(Vec::new());
        let id = repo.alloc_id("sig-");
        let signal = Signal::new(id.clone(), "<memory>", empty, 1e6);
        repo.register(id.clone(), signal);
        let err = repo.analyze(&id, None, &auto_params(), 115).unwrap_err();
        assert_eq!(err.code, "invalid-range", "{err}");
    }

    #[test]
    fn unknown_signal_error() {
        let repo = SignalRepository::new();
        let err = repo
            .analyze("sig-99", None, &auto_params(), 116)
            .unwrap_err();
        assert_eq!(err.code, "unknown-signal");
    }

    #[test]
    fn param_validation_errors() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK_SHORT);
        let bad = [
            params(Some("QAM"), None, None, None, None),
            params(Some(""), None, None, None, None),
            params(None, Some(0), None, None, None),
            params(None, None, None, None, Some(65_536)),
            params(None, None, Some(f32::NAN), None, None),
            params(None, None, None, Some(f32::NEG_INFINITY), None),
        ];
        for p in &bad {
            let err = repo.analyze(&id, None, p, 117).unwrap_err();
            assert_eq!(err.code, "invalid-argument", "{err}");
        }
    }

    // --- Job registry / cancellation ---

    #[test]
    fn cancelled_flag_short_circuits_pipeline() {
        // Pre-set cancellation, as if cancel_analysis had raced ahead of the
        // job: the pipeline must bail out at the first checkpoint, before
        // any DSP work (ask.complex analysis takes ~100 ms; the budget here
        // is a fraction of that).
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        let signal = repo.lookup(&id).unwrap();
        let validated = ValidatedParams::validate(&auto_params()).unwrap();
        let flag = AtomicBool::new(true);
        let t0 = std::time::Instant::now();
        let err = analyze_samples(&signal, None, &validated, &flag).unwrap_err();
        assert_eq!(err.code, CODE_CANCELLED, "{err}");
        assert!(
            t0.elapsed().as_millis() < 50,
            "cancellation must preempt the DSP, took {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn duplicate_job_id_is_rejected() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        jobs()
            .write()
            .unwrap()
            .insert(9102, Arc::new(AtomicBool::new(false)));
        let err = repo.analyze(&id, None, &auto_params(), 9102).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{err}");
        assert!(err.message.contains("9102"), "{err}");
        jobs().write().unwrap().remove(&9102);
    }

    #[test]
    fn cancel_analysis_unknown_job() {
        let repo = SignalRepository::new();
        assert!(!repo.cancel_analysis(9199));
    }

    #[test]
    fn job_is_deregistered_after_completion() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK_SHORT);
        repo.analyze(&id, None, &auto_params(), 9103).unwrap();
        // The guard removed the entry, so the id is free again and cancels
        // report "no such job".
        assert!(!repo.cancel_analysis(9103));
        repo.analyze(&id, None, &auto_params(), 9103).unwrap();
    }

    #[test]
    fn cancel_after_completion_is_false_but_harmless() {
        let repo = Arc::new(SignalRepository::new());
        let id = open(&repo, ASK_SHORT);
        let r2 = Arc::clone(&repo);
        let handle =
            std::thread::spawn(move || r2.analyze(&id, None, &auto_params(), 9104).unwrap());
        // ask_short analyzes in well under the scheduling delay, so this
        // cancel usually misses the job — which must be a no-op, not an error.
        let n = handle.join().unwrap();
        assert_eq!(n.messages.len(), 1);
        assert!(!repo.cancel_analysis(9104));
    }

    // --- helpers ---

    #[test]
    fn message_end_sample_matches_port_layout() {
        // Mid-signal shape: [..., pause_start, pause_end].
        let mut msg = Message::new(vec![1, 0], 500);
        msg.bit_sample_pos = vec![10, 20, 30, 40];
        assert_eq!(message_end_sample(&msg, 1000), 40);
        // Final-message shape: [..., data_end + pause] (pause 0 here).
        let mut msg = Message::new(vec![1, 0], 0);
        msg.bit_sample_pos = vec![10, 20, 35];
        assert_eq!(message_end_sample(&msg, 1000), 35);
        // Defensive clamp to the analyzed extent.
        let mut msg = Message::new(vec![1, 0], 70);
        msg.bit_sample_pos = vec![10, 20, 100];
        assert_eq!(message_end_sample(&msg, 50), 50);
        // No positions recorded: degrade to 0 instead of panicking.
        let msg = Message::new(vec![], 0);
        assert_eq!(message_end_sample(&msg, 50), 0);
    }

    #[test]
    fn validated_params_parses_modulations() {
        for (s, expected) in [
            ("ASK", Modulation::Ask),
            ("ask", Modulation::Ask),
            ("FSK", Modulation::Fsk),
            ("PSK", Modulation::Psk),
            ("GFSK", Modulation::Gfsk),
            ("OQPSK", Modulation::Oqpsk),
        ] {
            let v = ValidatedParams::validate(&params(Some(s), None, None, None, None)).unwrap();
            assert_eq!(v.modulation, Some((expected, false)), "{s}");
        }
        // The OOK extension maps to ASK + segment merge.
        let v = ValidatedParams::validate(&params(Some("OOK"), None, None, None, None)).unwrap();
        assert_eq!(v.modulation, Some((Modulation::Ask, true)));
    }

    // `SignalRepository` is `Send + Sync`, so tests share one instance via
    // `Arc` when they need to spawn threads.
}
