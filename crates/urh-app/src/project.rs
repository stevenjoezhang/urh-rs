//! Project service — v2 contract section "Project (v2, P4)"
//! (docs/ipc-contract.md).
//!
//! # What this module implements
//!
//! [`SignalRepository`] gains two methods: [`SignalRepository::save_project`]
//! writes a [`ProjectState`] to a `.urhproj` file, and
//! [`SignalRepository::load_project`] reads one back. Both are pure file
//! operations — no repository state is touched: per the contract, loading
//! "only parses"; the frontend re-opens the referenced files via
//! [`open_signal`](SignalRepository::open) and re-applies params/views itself
//! (open failures are per-signal errors the UI surfaces, not a failed load).
//! Neither call takes a lock, so nothing long-running can ever sit under a
//! repository lock (contract lock discipline).
//!
//! # File format
//!
//! `.urhproj` files are `serde_json::to_string_pretty` output (2-space
//! indent) plus a trailing newline, so they are diff- and editor-friendly.
//! Tuples serialize as JSON arrays (`["sig-1", 0, 1000, 1]`) and
//! `Option::None` as `null`, per serde's defaults; the field names are the
//! snake_case names from the contract. Unknown keys are ignored on load
//! (forward compatibility, the same policy as v1 command arguments).
//!
//! # Validation (documented interpretation)
//!
//! The contract mandates `version == 2`, malformed-JSON rejection and cheap
//! semantic checks. This module validates, with `invalid-argument`:
//!
//! - `version != 2` — on save as well as load, so a caller bug can never
//!   write a file the loader would reject;
//! - view entries with `start >= end` (empty and reversed intervals alike);
//! - view modes outside `{1, 2}` — the [`WaveformMode`] wire values; 3 and 4
//!   are reserved and could never be applied to an envelope query;
//! - non-finite or non-positive `sample_rate`s — they could never be applied
//!   through `open`/`set_sample_rate`.
//!
//! Deliberately *not* validated: whether `analysis`/`view`/
//! `active_signal_id` reference signals listed in `signals` (per the
//! contract, per-signal concerns belong to the UI that re-opens them), and
//! the internals of [`AnalysisParams`] (roundtripped verbatim; the analysis
//! service owns their rules).
//!
//! # Error codes
//!
//! Filesystem failures (missing file, missing parent directory, permissions)
//! are `io` with the path and OS error in the message — this module never
//! uses `file-not-found`, keeping project/simulator file handling uniform.
//! Malformed JSON and semantic violations are `invalid-argument` (the parse
//! error text is included). Serialization of a validated state cannot fail;
//! if it ever does, it is reported as `internal`.

use serde::{Deserialize, Serialize};

use crate::analysis::AnalysisParams;
use crate::{AppError, SignalRepository, WaveformMode};

/// The only `.urhproj` format version this build reads and writes
/// (contract: `version = 2`). Bumping this is a contract change.
pub const PROJECT_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// Wire types (contract: "Project")
// ---------------------------------------------------------------------------

/// One signal referenced by a saved project (contract: "Project").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSignal {
    /// Absolute path, as the frontend would pass it to
    /// [`open_signal`](SignalRepository::open).
    pub path: String,
    /// Sample rate in Hz; must be finite and positive (see module docs).
    pub sample_rate: f64,
}

impl Default for ProjectSignal {
    /// The repository-wide default rate — a `Default` project signal can
    /// actually be opened, unlike a `0.0` one.
    fn default() -> Self {
        ProjectSignal {
            path: String::new(),
            sample_rate: crate::DEFAULT_SAMPLE_RATE,
        }
    }
}

/// A whole saved GUI session (contract: "Project"). Serialized as-is over
/// Tauri JSON IPC and to `.urhproj` files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectState {
    /// Format version; always [`PROJECT_VERSION`] for files this build
    /// writes, and required on load.
    pub version: u32,
    /// Signals to re-open, in tab order.
    pub signals: Vec<ProjectSignal>,
    /// The signal tab that was active, if any.
    pub active_signal_id: Option<String>,
    /// Analysis parameterization per signal id (signal id → params).
    pub analysis: Vec<(String, AnalysisParams)>,
    /// Persisted viewport per signal id: (signal id, start, end, mode) with
    /// the half-open sample interval `[start, end)` and the
    /// [`WaveformMode`] wire value.
    pub view: Vec<(String, u64, u64, u32)>,
}

impl Default for ProjectState {
    fn default() -> Self {
        ProjectState {
            version: PROJECT_VERSION,
            signals: Vec::new(),
            active_signal_id: None,
            analysis: Vec::new(),
            view: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public service methods (contract: "Project")
// ---------------------------------------------------------------------------

impl SignalRepository {
    /// Write `state` to `path` as a `.urhproj` file (contract: "Project"):
    /// pretty-printed JSON. The file is created or truncated; the parent
    /// directory must exist (no implicit mkdir — failures are `io`). The
    /// state is validated first (see the module docs), so this build never
    /// writes a file [`load_project`](Self::load_project) would reject.
    pub fn save_project(&self, path: &str, state: &ProjectState) -> Result<(), AppError> {
        validate_state(state)?;
        let json = serde_json::to_string_pretty(state).map_err(|err| {
            AppError::internal(format!("failed to serialize project state: {err}"))
        })?;
        std::fs::write(path, format!("{json}\n")).map_err(|err| AppError {
            code: AppError::CODE_IO,
            message: format!("{path}: {err}"),
        })
    }

    /// Read and validate a `.urhproj` file (contract: "Project"). This only
    /// parses — signals are *not* opened; the frontend re-opens the listed
    /// files and re-applies params/views, surfacing per-signal failures
    /// itself. Malformed JSON and semantic violations (version, view
    /// intervals — see the module docs) are `invalid-argument` with the
    /// details in the message; filesystem failures are `io`.
    pub fn load_project(&self, path: &str) -> Result<ProjectState, AppError> {
        let json = std::fs::read_to_string(path).map_err(|err| AppError {
            code: AppError::CODE_IO,
            message: format!("{path}: {err}"),
        })?;
        let state: ProjectState = serde_json::from_str(&json).map_err(|err| {
            AppError::invalid_argument(format!("{path}: malformed project file: {err}"))
        })?;
        validate_state(&state)?;
        Ok(state)
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Semantic validation shared by save and load (see the module docs for the
/// rationale of each rule).
fn validate_state(state: &ProjectState) -> Result<(), AppError> {
    if state.version != PROJECT_VERSION {
        return Err(AppError::invalid_argument(format!(
            "unsupported project version {} (expected {PROJECT_VERSION})",
            state.version
        )));
    }
    for (i, signal) in state.signals.iter().enumerate() {
        if !signal.sample_rate.is_finite() || signal.sample_rate <= 0.0 {
            return Err(AppError::invalid_argument(format!(
                "signal {i} ({}) has a non-positive or non-finite sample rate {}",
                signal.path, signal.sample_rate
            )));
        }
    }
    for (i, (signal_id, start, end, mode)) in state.view.iter().enumerate() {
        if start >= end {
            return Err(AppError::invalid_argument(format!(
                "view {i} for {signal_id} is not a half-open interval: start {start} >= end {end}"
            )));
        }
        if WaveformMode::from_u32(*mode).is_none() {
            return Err(AppError::invalid_argument(format!(
                "view {i} for {signal_id} has mode {mode}, expected 1 (IQ) or 2 (amplitude)"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::AnalysisParams;

    fn sample_state() -> ProjectState {
        ProjectState {
            version: PROJECT_VERSION,
            signals: vec![
                ProjectSignal {
                    path: "/data/a.complex".to_string(),
                    sample_rate: 1e6,
                },
                ProjectSignal {
                    path: "/data/b.cs16".to_string(),
                    sample_rate: 2.5e6,
                },
            ],
            active_signal_id: Some("sig-2".to_string()),
            analysis: vec![(
                "sig-1".to_string(),
                AnalysisParams {
                    modulation: Some("ASK".to_string()),
                    bit_length: Some(100),
                    center: Some(0.02),
                    noise_threshold: None,
                    tolerance: Some(2),
                },
            )],
            view: vec![("sig-2".to_string(), 100, 900, 1)],
        }
    }

    #[test]
    fn validation_accepts_the_sample_state() {
        assert!(validate_state(&sample_state()).is_ok());
        assert!(validate_state(&ProjectState::default()).is_ok());
        // Modes 1 and 2 are the WaveformMode wire values.
        for mode in [1u32, 2] {
            let state = ProjectState {
                view: vec![("sig-1".to_string(), 0, 1, mode)],
                ..ProjectState::default()
            };
            assert!(validate_state(&state).is_ok(), "mode {mode}");
        }
    }

    #[test]
    fn validation_rejects_bad_versions_rates_and_views() {
        for version in [0u32, 1, 3, u32::MAX] {
            let state = ProjectState {
                version,
                ..ProjectState::default()
            };
            let err = validate_state(&state).unwrap_err();
            assert_eq!(err.code, "invalid-argument", "version {version}: {err}");
            assert!(err.message.contains(&version.to_string()), "{err}");
        }

        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let state = ProjectState {
                signals: vec![ProjectSignal {
                    path: "x".to_string(),
                    sample_rate: rate,
                }],
                ..ProjectState::default()
            };
            let err = validate_state(&state).unwrap_err();
            assert_eq!(err.code, "invalid-argument", "rate {rate}: {err}");
        }

        for (start, end) in [(10u64, 10u64), (20u64, 5u64), (0u64, 0u64)] {
            let state = ProjectState {
                view: vec![("sig-1".to_string(), start, end, 1)],
                ..ProjectState::default()
            };
            let err = validate_state(&state).unwrap_err();
            assert_eq!(err.code, "invalid-argument", "view [{start}, {end}): {err}");
        }

        for mode in [0u32, 3, 4, 99] {
            let state = ProjectState {
                view: vec![("sig-1".to_string(), 0, 10, mode)],
                ..ProjectState::default()
            };
            let err = validate_state(&state).unwrap_err();
            assert_eq!(err.code, "invalid-argument", "mode {mode}: {err}");
        }
    }

    /// The JSON shape the contract implies: snake_case field names, tuples as
    /// arrays, `None` as `null`, pretty-printed with 2-space indent.
    #[test]
    fn serde_shape_matches_the_contract() {
        let json = serde_json::to_string_pretty(&sample_state()).unwrap();
        assert!(json.contains("\"version\": 2"), "{json}");
        assert!(json.contains("\"active_signal_id\": \"sig-2\""), "{json}");
        assert!(json.contains("\"sample_rate\": 1000000.0"), "{json}");
        // analysis tuple: [signal id, params object]
        assert!(
            json.contains(
                "\"analysis\": [\n    [\n      \"sig-1\",\n      {\n        \"modulation\": \"ASK\",\n        \"bit_length\": 100,"
            ),
            "{json}"
        );
        // view tuple: [signal id, start, end, mode]
        assert!(
            json.contains(
                "\"view\": [\n    [\n      \"sig-2\",\n      100,\n      900,\n      1\n    ]\n  ]"
            ),
            "{json}"
        );
        // None serializes as null.
        assert!(json.contains("\"noise_threshold\": null"), "{json}");

        // Roundtrip is lossless.
        let back: ProjectState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sample_state());
    }

    #[test]
    fn serde_tolerates_missing_option_fields_and_unknown_keys() {
        // Missing active_signal_id -> None (serde's Option behavior); unknown
        // keys are ignored; missing analysis params default to auto.
        let json = r#"{
            "version": 2,
            "signals": [],
            "analysis": [["sig-1", {}]],
            "view": [],
            "future_field": {"anything": true}
        }"#;
        let state: ProjectState = serde_json::from_str(json).unwrap();
        assert_eq!(state.active_signal_id, None);
        assert_eq!(
            state.analysis,
            vec![("sig-1".to_string(), AnalysisParams::default())]
        );
        assert!(validate_state(&state).is_ok());
    }

    #[test]
    fn serde_rejects_missing_required_fields_and_wrong_types() {
        for (what, json) in [
            (
                "version missing entirely",
                r#"{"signals": [], "analysis": [], "view": []}"#,
            ),
            (
                "signals missing",
                r#"{"version": 2, "analysis": [], "view": []}"#,
            ),
            (
                "start/end must be integers, not strings",
                r#"{"version": 2, "signals": [], "analysis": [],
                    "view": [["sig-1", "0", "10", 1]]}"#,
            ),
            (
                "a signal entry must be a 2-tuple",
                r#"{"version": 2, "signals": [["a.complex"]], "analysis": [], "view": []}"#,
            ),
        ] {
            let err = serde_json::from_str::<ProjectState>(json)
                .unwrap_err()
                .to_string();
            assert!(!err.is_empty(), "{what}");
        }
    }

    #[test]
    fn default_state_is_valid_and_empty() {
        let state = ProjectState::default();
        assert_eq!(state.version, PROJECT_VERSION);
        assert!(state.signals.is_empty());
        assert!(state.analysis.is_empty());
        assert!(state.view.is_empty());
        assert_eq!(state.active_signal_id, None);
        assert!(validate_state(&state).is_ok());
    }
}
