//! Integration tests for the v2 device capture service
//! (docs/ipc-contract.md, "Device capture").
//!
//! The v2 service modules are declared private in `lib.rs`, so their types
//! (`CaptureConfig`, `CaptureHandle`, `CaptureStats`, …) are not nameable
//! outside the crate. The tests drive them through inference instead:
//! `default_of()` builds the default value and a *type-pinning call* —
//! passing it to `start_capture`, which fails validation harmlessly on the
//! default (empty device, zero sample rate) — resolves the otherwise
//! unnameable type so the field writes inside the macro compile. Values of
//! the unnameable types are then inspected through field access, which works
//! fine. The queue-discipline unit tests (drop-oldest preview, overflow
//! stop) live in `crates/urh-app/src/devices.rs`; they need the internal
//! queue constructor, which is not visible from here.

use std::net::TcpListener;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use urh_app::SignalRepository;

fn default_of<T: Default>() -> T {
    T::default()
}

/// Build a `CaptureConfig` by field. The pinning call inside resolves the
/// unnameable config type; the default config is rejected by validation, so
/// it has no side effects.
macro_rules! capture_config {
    ($repo:expr, { $($field:ident : $value:expr),* $(,)? }) => {{
        let mut cfg = default_of();
        let _ = $repo.start_capture(&cfg); // type-pinning call
        $(cfg.$field = $value;)*
        cfg
    }};
}

/// Live captures live in a process-wide registry while capture ids are
/// per-repository, so capture-starting tests share one repository (see the
/// devices module docs for the tradeoff).
fn shared_repo() -> &'static SignalRepository {
    static REPO: OnceLock<SignalRepository> = OnceLock::new();
    REPO.get_or_init(SignalRepository::new)
}

/// The mock's repeating bits (contract: `10101100`, 100 samples/bit,
/// amplitudes 1.0/0.1).
const MOCK_PATTERN: [u8; 8] = [1, 0, 1, 0, 1, 1, 0, 0];
const MOCK_SAMPLES_PER_BIT: usize = 100;
/// Preview batches hold 4096 samples (contract) of 8 interleaved LE f32
/// bytes each.
const BATCH_SAMPLES: usize = 4096;
const BATCH_BYTES: usize = BATCH_SAMPLES * 8;

/// Decode `.complex` bytes (interleaved LE f32) into magnitudes.
fn magnitudes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(8)
        .map(|c| {
            let re = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            let im = f32::from_le_bytes([c[4], c[5], c[6], c[7]]);
            (re * re + im * im).sqrt()
        })
        .collect()
}

/// Mock capture, the contract's acceptance path: ~20 preview batches of
/// 4096 samples, a recording that is a whole number of batches and at least
/// as long as what was previewed, and a registered `"rec-<n>"` signal whose
/// samples decode as the mock's ASK pattern.
#[test]
fn mock_capture_records_ask_pattern() {
    let repo = shared_repo();
    let cfg = capture_config!(repo, {
        device: "mock".to_string(),
        sample_rate: 1e6,
        frequency: 433.92e6,
        bandwidth: 1e6,
        gain: 0.0,
        record: true,
    });
    let handle = repo.start_capture(&cfg).unwrap();
    assert!(
        handle.capture_id.starts_with("cap-"),
        "{:?}",
        handle.capture_id
    );
    let signal_id = handle
        .signal_id
        .clone()
        .expect("record = true yields a signal id");
    assert!(signal_id.starts_with("rec-"), "{signal_id}");

    // Consume 20 preview batches while the capture records. A consumer this
    // fast keeps up, so seqs are gapless.
    let receiver = repo
        .take_preview_receiver(&handle.capture_id)
        .expect("the capture's receiver");
    let mut last_seq = None;
    let mut batches = 0usize;
    while batches < 20 {
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(b)) => {
                assert_eq!(b.body.len(), BATCH_BYTES);
                if let Some(prev) = last_seq {
                    assert_eq!(b.seq, prev + 1, "no drops expected while consuming");
                }
                last_seq = Some(b.seq);
                batches += 1;
            }
            other => panic!("expected a preview batch, got {other:?}"),
        }
    }

    let stats = repo.stop_capture(&handle.capture_id).unwrap();
    assert!(
        stats.recorded_samples >= (20 * BATCH_SAMPLES) as u64,
        "recorded {} after 20 preview batches",
        stats.recorded_samples
    );
    // The mock pushes whole 4096-sample chunks, so nothing overflows and the
    // recording is a whole number of batches.
    assert_eq!(stats.recorded_samples % BATCH_SAMPLES as u64, 0);
    assert!(!stats.overflow_detected);
    assert_eq!(stats.dropped_preview_batches, 0);

    // The recording is registered as a repository signal with the capture's
    // metadata (in-memory: empty path, complex format).
    let meta = repo
        .list()
        .into_iter()
        .find(|m| m.signal_id == signal_id)
        .expect("the recording is registered");
    assert_eq!(meta.sample_count, stats.recorded_samples);
    assert_eq!(meta.sample_rate, 1e6);
    assert_eq!(meta.format, "complex");
    assert_eq!(meta.path, "");

    // The recorded samples decode as the mock's ASK pattern. Exercised
    // through save_signal_as (the only public way out of the repository),
    // which also proves recordings can be persisted.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.complex");
    repo.save_signal_as(&signal_id, path.to_str().unwrap())
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len() as u64, stats.recorded_samples * 8);
    let mags = magnitudes(&bytes);
    for k in 0..2 * MOCK_PATTERN.len() * MOCK_SAMPLES_PER_BIT {
        let bit = MOCK_PATTERN[(k / MOCK_SAMPLES_PER_BIT) % MOCK_PATTERN.len()];
        let expected = if bit == 1 { 1.0 } else { 0.1 };
        assert!(
            (mags[k] - expected).abs() < 1e-4,
            "sample {k}: magnitude {} vs expected {expected}",
            mags[k]
        );
    }
}

/// A consumer slower than the 16-batch preview queue: the oldest batches are
/// dropped, `dropped_preview_batches` counts them, and the first surviving
/// batch's seq exposes the gap.
#[test]
fn slow_preview_consumer_sees_drops_and_seq_gaps() {
    let repo = shared_repo();
    let cfg = capture_config!(repo, {
        device: "mock".to_string(),
        sample_rate: 1e6,
        frequency: 433.92e6,
        bandwidth: 1e6,
        gain: 0.0,
        record: false,
    });
    let handle = repo.start_capture(&cfg).unwrap();

    // Do not touch the receiver for a while: the mock pushes a batch every
    // 2 ms, the preview queue holds 16 batches, so after 300 ms the queue
    // has wrapped and dropped the oldest batches (with or without a
    // consumer attached).
    thread::sleep(Duration::from_millis(300));
    let stats = repo.capture_stats(&handle.capture_id).unwrap();
    assert!(
        stats.dropped_preview_batches > 0,
        "expected preview drops, got {stats:?}"
    );

    let receiver = repo
        .take_preview_receiver(&handle.capture_id)
        .expect("the capture's receiver");
    match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(Some(b)) => assert!(
            b.seq > 0,
            "the first surviving batch (seq {}) must be past the dropped ones",
            b.seq
        ),
        other => panic!("expected a preview batch, got {other:?}"),
    }

    // record = false: no signal id, no recorded samples, nothing registered.
    let stats = repo.stop_capture(&handle.capture_id).unwrap();
    assert_eq!(stats.recorded_samples, 0);
    assert!(stats.dropped_preview_batches > 0);
    assert_eq!(handle.signal_id, None, "record = false registers nothing");

    // The receiver reports the capture's end once the queue is drained
    // (stop_capture has already joined the pump, so the close is in).
    let mut closed = false;
    for _ in 0..64 {
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(_)) | Ok(None) => {}
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    assert!(closed, "the receiver must report the capture's end");
}

#[test]
fn list_devices_reports_the_mock_and_backend_tags() {
    let devices = shared_repo().list_devices();

    let mock = devices.iter().find(|d| d.name == "mock").unwrap();
    assert!(mock.available);
    assert_eq!(mock.backend, "mock");
    assert_eq!(mock.detail, "");

    let network = devices.iter().find(|d| d.name == "rtl-tcp").unwrap();
    assert!(network.available);
    assert_eq!(network.backend, "network");

    assert!(devices.iter().any(|d| d.backend == "native-dlopen"));
    assert!(devices.iter().any(|d| d.backend == "cpal" && !d.available));

    for d in &devices {
        assert_eq!(d.detail.is_empty(), d.available, "device {}", d.name);
    }
}

#[test]
fn start_capture_validation_errors() {
    let repo = shared_repo();

    // Unknown device.
    let cfg = capture_config!(repo, {
        device: "no-such-device".to_string(),
        sample_rate: 1e6,
        frequency: 100e6,
        bandwidth: 1e6,
        gain: 0.0,
        record: false,
    });
    let err = repo.start_capture(&cfg).unwrap_err();
    assert_eq!(err.code, "invalid-argument");

    // rtl-tcp needs both host and port.
    let cfg = capture_config!(repo, {
        device: "rtl-tcp".to_string(),
        sample_rate: 1e6,
        frequency: 100e6,
        bandwidth: 1e6,
        gain: 0.0,
        record: false,
        host: Some("127.0.0.1".to_string()),
    });
    let err = repo.start_capture(&cfg).unwrap_err();
    assert_eq!(err.code, "invalid-argument");
    assert!(err.message.contains("rtl-tcp"), "{err}");

    // Unknown capture ids are invalid-argument across the board.
    for code in [
        repo.capture_stats("cap-999999").unwrap_err().code,
        repo.stop_capture("cap-999999").unwrap_err().code,
    ] {
        assert_eq!(code, "invalid-argument");
    }
    assert!(repo.take_preview_receiver("cap-999999").is_none());
}

/// A connection refused at start_capture surfaces as an `io` error (the
/// contract's rule that rtl-tcp availability is checked when starting).
#[test]
fn rtl_tcp_connection_failure_is_io() {
    let repo = shared_repo();
    // Reserve a port and close the listener again so nothing accepts.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let cfg = capture_config!(repo, {
        device: "RTL-TCP".to_string(),
        sample_rate: 1e6,
        frequency: 100e6,
        bandwidth: 1e6,
        gain: 0.0,
        record: false,
        host: Some("127.0.0.1".to_string()),
        port: Some(port),
    });
    let err = repo.start_capture(&cfg).unwrap_err();
    assert_eq!(err.code, "io", "{err}");
    // The failed start left no session behind.
    assert!(repo.take_preview_receiver("cap-999998").is_none());
}
