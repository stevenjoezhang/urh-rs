//! Integration tests for the v2 protocol analysis service
//! (docs/ipc-contract.md, "Protocol analysis").
//!
//! The v2 service modules are declared private in `lib.rs`, so their types
//! (`AnalysisParams`, …) are not nameable outside the crate; the tests drive
//! the public methods through inference instead (`&Default::default()` is
//! the all-auto parameterization, i.e. exactly the CLI `analyze` flow). The
//! parameterized cases are covered by the unit tests inside
//! `crates/urh-app/src/analysis.rs`.

use std::sync::Arc;
use std::time::Duration;

use urh_app::SignalRepository;

const ASK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/ask.complex");
const FSK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/fsk.complex");
const ELEKTROMATEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/data/elektromaten.complex16s"
);

fn assert_close(a: f32, b: f32, what: &str) {
    assert!((a - b).abs() < 1e-6, "{what}: {a} vs expected {b}");
}

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

/// Golden: `urh-cli analyze tests/data/ask.complex` (auto interpretation).
#[test]
fn ask_auto_matches_cli() {
    let repo = SignalRepository::new();
    let id = repo.open(ASK, None).unwrap().signal_id;
    let result = repo.analyze(&id, None, &Default::default(), 1).unwrap();

    assert_eq!(result.params.modulation, "ASK");
    assert_eq!(result.params.bit_length, 300);
    assert_close(result.params.center, 0.025245, "center");
    assert_close(result.params.noise_threshold, 0.0175, "noise");
    assert_eq!(result.params.tolerance, 3);

    assert_eq!(result.messages.len(), 1);
    let m = &result.messages[0];
    assert_eq!((m.index, m.bit_count, m.pause), (0, 43, 1694));
    assert_eq!(m.plain_bits, bits_from_hex("b25b6db6c80", 43));
    assert_eq!(m.start_sample, 466);
    assert_eq!(m.end_sample, 13707);
}

/// Golden: `urh-cli analyze tests/data/fsk.complex` (auto interpretation).
#[test]
fn fsk_auto_matches_cli() {
    let repo = SignalRepository::new();
    let id = repo.open(FSK, None).unwrap().signal_id;
    let result = repo.analyze(&id, None, &Default::default(), 2).unwrap();

    assert_eq!(result.params.modulation, "FSK");
    assert_eq!(result.params.bit_length, 100);
    assert_close(result.params.center, -0.046264, "center");
    assert_close(result.params.noise_threshold, 0.0011, "noise");
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
fn error_codes() {
    let repo = SignalRepository::new();
    let id = repo.open(ASK, None).unwrap().signal_id;

    let err = repo
        .analyze("sig-99", None, &Default::default(), 3)
        .unwrap_err();
    assert_eq!(err.code, "unknown-signal");

    for range in [
        Some((5u64, 5u64)),
        Some((10u64, 5u64)),
        Some((0u64, 13711u64)),
    ] {
        let err = repo
            .analyze(&id, range, &Default::default(), 3)
            .unwrap_err();
        assert_eq!(err.code, "invalid-range", "{range:?}: {err}");
    }

    // Cancelling a job that never ran reports "not found" as false.
    assert!(!repo.cancel_analysis(999_999));
}

#[test]
fn range_restricts_analysis() {
    let repo = SignalRepository::new();
    let id = repo.open(FSK, None).unwrap().signal_id;
    // The full range is bit-identical to no range at all.
    let whole = repo
        .analyze(&id, Some((0, 67912)), &Default::default(), 4)
        .unwrap();
    let bare = repo.analyze(&id, None, &Default::default(), 5).unwrap();
    assert_eq!(whole, bare);

    // A sub-range keeps positions absolute and inside the range.
    let cut = repo
        .analyze(&id, Some((0, 20000)), &Default::default(), 6)
        .unwrap();
    for m in &cut.messages {
        assert!(m.end_sample <= 20000, "end {} exceeds range", m.end_sample);
    }
}

/// A job cancelled while it runs returns `code = "cancelled"` (elektromaten
/// analysis takes long enough to cancel reliably mid-run).
#[test]
fn cancel_during_run_returns_cancelled() {
    let repo = Arc::new(SignalRepository::new());
    let id = repo.open(ELEKTROMATEN, None).unwrap().signal_id;

    let r2 = Arc::clone(&repo);
    let id2 = id.clone();
    let handle = std::thread::spawn(move || r2.analyze(&id2, None, &Default::default(), 3101));

    // The job registers before it starts computing, so a cancel lands while
    // it runs; keep trying until the registry has it.
    let mut cancelled = false;
    for _ in 0..2000 {
        if repo.cancel_analysis(3101) {
            cancelled = true;
            break;
        }
        if handle.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(cancelled, "job never became visible to cancel_analysis");
    let result = handle.join().unwrap();
    let err = result.unwrap_err();
    assert_eq!(err.code, "cancelled", "{err}");
    // The finished (cancelled) job is deregistered again.
    assert!(!repo.cancel_analysis(3101));
}

/// A second `analyze` with the same `job_id` while one is running is
/// rejected with `invalid-argument`.
#[test]
fn duplicate_job_id_is_rejected_while_running() {
    let repo = Arc::new(SignalRepository::new());
    let id = repo.open(ELEKTROMATEN, None).unwrap().signal_id;

    let r2 = Arc::clone(&repo);
    let id2 = id.clone();
    let handle = std::thread::spawn(move || r2.analyze(&id2, None, &Default::default(), 3102));

    // Wait until the job is registered by peeking through cancel_analysis
    // (it also flags the job for cancellation, which is fine: the registry
    // entry lives until the job returns). The head start puts the job well
    // past its registration-time checkpoint, deep inside the ~1.5 s
    // estimation phase, so the duplicate call below is guaranteed to find
    // the id taken.
    std::thread::sleep(Duration::from_millis(100));
    let mut registered = false;
    for _ in 0..2000 {
        if repo.cancel_analysis(3102) {
            registered = true;
            break;
        }
        if handle.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(registered, "job never became visible to cancel_analysis");

    // While the (cancelled-but-still-running) job holds the id, a second
    // analyze with the same id is rejected.
    match repo.analyze(&id, None, &Default::default(), 3102) {
        Err(err) => {
            assert_eq!(err.code, "invalid-argument", "{err}");
            assert!(err.message.contains("3102"), "{err}");
        }
        // Only reachable if the job slipped away between the peek and this
        // call; the unit tests cover the rejection deterministically.
        Ok(result) => panic!("lost the race: duplicate analyze ran to completion ({result:?})"),
    }

    let result = handle.join().unwrap();
    assert_eq!(result.unwrap_err().code, "cancelled");

    // After the job is gone the id is free again (checked on a small file).
    let ask = repo.open(ASK, None).unwrap().signal_id;
    repo.analyze(&ask, None, &Default::default(), 3102).unwrap();
}
