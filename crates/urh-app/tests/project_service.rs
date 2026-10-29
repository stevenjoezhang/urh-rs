//! Integration tests for the v2 project service
//! (docs/ipc-contract.md, "Project").
//!
//! These exercise the public file flow end to end: save→load roundtrips
//! (byte-sane pretty JSON), validation of everything the loader checks,
//! the error codes for bad paths and bad files, and the contract's "load
//! only parses" rule (states referencing nonexistent signal files load
//! fine — the frontend re-opens them).

use std::fs;

use urh_app::project::{ProjectSignal, ProjectState, PROJECT_VERSION};
use urh_app::{AnalysisParams, SignalRepository};

fn sample_state() -> ProjectState {
    ProjectState {
        version: PROJECT_VERSION,
        signals: vec![
            ProjectSignal {
                path: "/data/ask.complex".to_string(),
                sample_rate: 1e6,
            },
            ProjectSignal {
                path: "/data/fsk.cs16".to_string(),
                sample_rate: 2.5e6,
            },
        ],
        active_signal_id: Some("sig-1".to_string()),
        analysis: vec![
            (
                "sig-1".to_string(),
                AnalysisParams {
                    modulation: Some("ASK".to_string()),
                    bit_length: Some(100),
                    center: Some(0.02),
                    noise_threshold: Some(0.01),
                    tolerance: Some(2),
                },
            ),
            ("sig-2".to_string(), AnalysisParams::default()),
        ],
        view: vec![
            ("sig-1".to_string(), 0, 10_000, 1),
            ("sig-2".to_string(), 512, 4096, 2),
        ],
    }
}

/// The acceptance path: save a populated state, read the file back and
/// check the on-disk shape, then load it and compare, then save the loaded
/// state again and require byte-identical output (a stable, diffable
/// format).
#[test]
fn save_load_roundtrip_is_byte_sane() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.urhproj");
    let path = path.to_str().unwrap();

    repo.save_project(path, &sample_state()).unwrap();
    let json = fs::read_to_string(path).unwrap();

    // Pretty JSON with the contract's snake_case field names, a top-level
    // version field, a trailing newline, and tuples as arrays.
    assert!(json.starts_with("{\n  \"version\": 2,\n"), "{json}");
    assert!(json.contains("\"active_signal_id\": \"sig-1\""), "{json}");
    assert!(json.contains("\"path\": \"/data/ask.complex\""), "{json}");
    assert!(json.contains("\"sample_rate\": 1000000.0"), "{json}");
    assert!(
        json.contains("\"analysis\": [\n    [\n      \"sig-1\",\n      {"),
        "{json}"
    );
    assert!(
        json.contains(
            "\"view\": [\n    [\n      \"sig-1\",\n      0,\n      10000,\n      1\n    ]"
        ),
        "{json}"
    );
    assert!(json.ends_with("}\n"), "{json}");

    let loaded = repo.load_project(path).unwrap();
    assert_eq!(loaded, sample_state());

    // Save the loaded state again: byte-identical (no churn on roundtrip).
    let again = dir.path().join("again.urhproj");
    repo.save_project(again.to_str().unwrap(), &loaded).unwrap();
    assert_eq!(fs::read(again).unwrap(), fs::read(path).unwrap());

    // The file is plain JSON (parseable by a stock parser, no magic).
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["version"], 2);
    assert_eq!(value["signals"].as_array().unwrap().len(), 2);
}

/// The empty/default state roundtrips and validates (a fresh session can
/// be saved before anything is opened).
#[test]
fn default_state_roundtrips() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.urhproj");
    let path = path.to_str().unwrap();

    repo.save_project(path, &ProjectState::default()).unwrap();
    let loaded = repo.load_project(path).unwrap();
    assert_eq!(loaded, ProjectState::default());
    assert_eq!(loaded.version, PROJECT_VERSION);
    assert!(loaded.signals.is_empty());
    assert_eq!(loaded.active_signal_id, None);
}

/// Loading does NOT open signals: a state referencing files that do not
/// exist loads fine (per-signal failures belong to the frontend that
/// re-opens them), and the repository stays empty.
#[test]
fn load_does_not_open_signals() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dangling.urhproj");
    let path = path.to_str().unwrap();

    let state = ProjectState {
        signals: vec![ProjectSignal {
            path: "/definitely/not/a/signal.complex".to_string(),
            sample_rate: 1e6,
        }],
        active_signal_id: Some("sig-9".to_string()),
        ..ProjectState::default()
    };
    repo.save_project(path, &state).unwrap();
    let loaded = repo.load_project(path).unwrap();
    assert_eq!(loaded, state);
    assert!(repo.list().is_empty(), "load must not open signals");
}

/// Malformed JSON is `invalid-argument` with the parse error in the
/// message; unknown keys are ignored; missing optional fields default.
#[test]
fn malformed_json_is_invalid_argument() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.urhproj");
    let path = path.to_str().unwrap();

    for (what, contents) in [
        ("empty file", ""),
        ("truncated JSON", r#"{"version": 2, "signals": ["#),
        ("not JSON at all", "hello"),
        (
            "wrong types",
            r#"{"version": "two", "signals": [], "analysis": [], "view": []}"#,
        ),
        (
            "a view entry that is not a 4-tuple",
            r#"{"version": 2, "signals": [], "analysis": [], "view": [["sig-1", 0, 10]]}"#,
        ),
    ] {
        fs::write(path, contents).unwrap();
        let err = repo.load_project(path).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{what}: {err}");
        assert!(
            err.message.contains("malformed project file"),
            "{what}: {err}"
        );
        assert!(err.message.contains("bad.urhproj"), "{what}: {err}");
        assert!(
            err.message.contains("line") || err.message.contains("expected"),
            "{what}: parse error text missing: {err}"
        );
    }

    // Unknown keys are ignored (forward compatibility); missing optional
    // fields default; a missing version is a parse error (required field).
    fs::write(
        path,
        r#"{"version": 2, "signals": [], "analysis": [], "view": [],
            "future_field": [1, 2, 3]}"#,
    )
    .unwrap();
    assert_eq!(repo.load_project(path).unwrap(), ProjectState::default());

    fs::write(path, r#"{"signals": [], "analysis": [], "view": []}"#).unwrap();
    assert_eq!(
        repo.load_project(path).unwrap_err().code,
        "invalid-argument"
    );
}

/// Only version 2 loads: version 1 and 3 (and 0) are `invalid-argument`,
/// on load and on save (this build never writes a file it would reject).
#[test]
fn wrong_versions_are_rejected() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("version.urhproj");
    let path = path.to_str().unwrap();

    for version in [0u32, 1, 3, u32::MAX] {
        // On load: a well-formed file with the wrong version.
        fs::write(
            path,
            format!(r#"{{"version": {version}, "signals": [], "analysis": [], "view": []}}"#),
        )
        .unwrap();
        let err = repo.load_project(path).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "version {version}: {err}");
        assert!(err.message.contains("version"), "{err}");
        assert!(
            err.message.contains(&version.to_string()),
            "version {version} not named: {err}"
        );

        // On save: a state carrying the wrong version.
        let state = ProjectState {
            version,
            ..ProjectState::default()
        };
        let err = repo.save_project(path, &state).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "version {version}: {err}");
        // And nothing was written (the pre-existing file is untouched).
        assert!(fs::read_to_string(path)
            .unwrap()
            .contains(&format!("\"version\": {version}")));
    }
}

/// View entries must be half-open intervals with a valid waveform mode —
/// on load and on save alike.
#[test]
fn bad_view_entries_are_rejected() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("view.urhproj");
    let path = path.to_str().unwrap();

    // On save: equal, reversed and zero-length intervals, and bad modes.
    for (what, start, end, mode) in [
        ("equal start/end", 100u64, 100u64, 1u32),
        ("reversed interval", 200, 100, 1),
        ("empty interval at 0", 0, 0, 1),
        ("reserved mode 3", 0, 10, 3),
        ("reserved mode 4", 0, 10, 4),
        ("mode 0", 0, 10, 0),
    ] {
        let state = ProjectState {
            view: vec![("sig-1".to_string(), start, end, mode)],
            ..ProjectState::default()
        };
        let err = repo.save_project(path, &state).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{what}: {err}");
    }

    // On load: the same violations, written by hand.
    for (what, view_json) in [
        ("equal start/end", r#"[["sig-1", 100, 100, 1]]"#),
        ("reversed interval", r#"[["sig-1", 200, 100, 1]]"#),
        ("reserved mode 3", r#"[["sig-1", 0, 10, 3]]"#),
        ("reserved mode 4", r#"[["sig-1", 0, 10, 4]]"#),
    ] {
        fs::write(
            path,
            format!(r#"{{"version": 2, "signals": [], "analysis": [], "view": {view_json}}}"#),
        )
        .unwrap();
        let err = repo.load_project(path).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{what}: {err}");
        assert!(err.message.contains("view"), "{what}: {err}");
    }

    // The good cases: both real modes, and a minimal [0, 1) interval.
    for mode in [1u32, 2] {
        let state = ProjectState {
            view: vec![("sig-1".to_string(), 0, 1, mode)],
            ..ProjectState::default()
        };
        repo.save_project(path, &state).unwrap();
        assert_eq!(repo.load_project(path).unwrap(), state);
    }
}

/// Sample rates must be finite and positive — on load and on save, with
/// null and negative values in the JSON rejected too.
#[test]
fn bad_sample_rates_are_rejected() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rate.urhproj");
    let path = path.to_str().unwrap();

    // Type violations are parse errors (malformed file); the semantic
    // violations below carry the sample-rate explanation.
    for (what, rate_json) in [("null", "null"), ("not a number", "\"1e6\"")] {
        fs::write(
            path,
            format!(
                r#"{{"version": 2, "signals": [{{"path": "a.complex", "sample_rate": {rate_json}}}], "analysis": [], "view": []}}"#
            ),
        )
        .unwrap();
        let err = repo.load_project(path).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{what}: {err}");
        assert!(
            err.message.contains("malformed project file"),
            "{what}: {err}"
        );
    }

    // Values that parse but are not usable rates fail semantic validation.
    for (what, rate_json) in [("zero", "0.0"), ("negative", "-1000000.0")] {
        fs::write(
            path,
            format!(
                r#"{{"version": 2, "signals": [{{"path": "a.complex", "sample_rate": {rate_json}}}], "analysis": [], "view": []}}"#
            ),
        )
        .unwrap();
        let err = repo.load_project(path).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{what}: {err}");
        assert!(err.message.contains("sample rate"), "{what}: {err}");
    }

    // On save: non-finite rates cannot even be expressed in JSON, so only
    // the non-positive ones are reachable.
    for rate in [0.0, -1.0] {
        let state = ProjectState {
            signals: vec![ProjectSignal {
                path: "a.complex".to_string(),
                sample_rate: rate,
            }],
            ..ProjectState::default()
        };
        let err = repo.save_project(path, &state).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "rate {rate}: {err}");
    }
}

/// Filesystem failures are `io` with the path in the message: saving into
/// a missing directory, loading a missing file, loading a directory.
#[test]
fn io_errors() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();

    // Save: parent directory does not exist (and is not created).
    let missing_dir = dir.path().join("no-such-dir").join("p.urhproj");
    let err = repo
        .save_project(missing_dir.to_str().unwrap(), &ProjectState::default())
        .unwrap_err();
    assert_eq!(err.code, "io", "{err}");
    assert!(err.message.contains("no-such-dir"), "{err}");
    assert!(
        !dir.path().join("no-such-dir").exists(),
        "no implicit mkdir"
    );

    // Load: file does not exist.
    let missing = dir.path().join("missing.urhproj");
    let err = repo.load_project(missing.to_str().unwrap()).unwrap_err();
    assert_eq!(err.code, "io", "{err}");
    assert!(err.message.contains("missing.urhproj"), "{err}");

    // Load: a directory is not a project file.
    let err = repo.load_project(dir.path().to_str().unwrap()).unwrap_err();
    assert_eq!(err.code, "io", "{err}");

    // Save overwrites an existing file (create-or-truncate, not append).
    let path = dir.path().join("existing.urhproj");
    let path = path.to_str().unwrap();
    fs::write(
        path,
        "old contents that are much longer than the new state\n",
    )
    .unwrap();
    repo.save_project(path, &ProjectState::default()).unwrap();
    let json = fs::read_to_string(path).unwrap();
    assert!(json.ends_with("}\n"), "{json}");
    assert!(!json.contains("old contents"), "{json}");
}

/// The full contract shape roundtrips through a realistic session state:
/// multiple signals, per-signal analysis params (partial and default), a
/// persisted viewport per signal, and an active signal id.
#[test]
fn realistic_session_roundtrips() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.urhproj");
    let path = path.to_str().unwrap();

    let state = sample_state();
    repo.save_project(path, &state).unwrap();
    let loaded = repo.load_project(path).unwrap();

    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.signals.len(), 2);
    assert_eq!(loaded.signals[0].path, "/data/ask.complex");
    assert_eq!(loaded.signals[1].sample_rate, 2.5e6);
    assert_eq!(loaded.active_signal_id.as_deref(), Some("sig-1"));
    assert_eq!(loaded.analysis.len(), 2);
    assert_eq!(
        loaded.analysis[0].1.modulation.as_deref(),
        Some("ASK"),
        "analysis params survive the roundtrip"
    );
    assert_eq!(loaded.analysis[1].1, AnalysisParams::default());
    assert_eq!(loaded.view.len(), 2);
    assert_eq!(loaded.view[1], ("sig-2".to_string(), 512, 4096, 2));
    assert_eq!(loaded, state);
}
