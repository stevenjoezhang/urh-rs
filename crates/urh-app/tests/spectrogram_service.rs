//! Integration tests for the v2 spectrogram tile service
//! (docs/ipc-contract.md, "Spectrogram tiles").
//!
//! The service types live in a private module, so these tests drive the
//! public method with primitive arguments only; the fine-grained payload
//! parity checks run as unit tests inside
//! `crates/urh-app/src/spectrogram.rs`.

use urh_app::SignalRepository;

const ASK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/ask.complex");

/// Decoder for the frozen 24-byte frame header (mirrors the frontend's
/// validation duties; the payload stays raw u8).
struct Header {
    request_id: u32,
    n_bins: u32,
    mode: u32,
    flags: u32,
}

fn decode_header(frame: &[u8]) -> Header {
    assert!(frame.len() >= 24, "frame shorter than the 24-byte header");
    assert_eq!(&frame[0..4], b"URH1", "bad magic");
    assert_eq!(
        u16::from_le_bytes([frame[4], frame[5]]),
        1,
        "bad frame version"
    );
    assert_eq!(
        u16::from_le_bytes([frame[6], frame[7]]),
        2,
        "spectrogram tiles must use dtype 2 (u8)"
    );
    Header {
        request_id: u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]),
        n_bins: u32::from_le_bytes([frame[12], frame[13], frame[14], frame[15]]),
        mode: u32::from_le_bytes([frame[16], frame[17], frame[18], frame[19]]),
        flags: u32::from_le_bytes([frame[20], frame[21], frame[22], frame[23]]),
    }
}

#[test]
fn full_range_tile_header_and_payload() {
    let repo = SignalRepository::new();
    let id = repo.open(ASK, None).unwrap().signal_id; // 13710 samples

    for (window_size, freq_bins, hop) in [
        (256u32, 128u32, 128u64),
        (1024, 512, 512),
        (4096, 2048, 2048),
    ] {
        let frame = repo
            .spectrogram_tile(&id, 0, 13710, freq_bins, window_size, 42)
            .unwrap();
        let header = decode_header(&frame);
        assert_eq!(header.mode, 3, "mode must be 3 (spectrogram)");
        assert_eq!(header.flags, 0);
        assert_eq!(header.request_id, 42);
        let expected_bins = (13710 / hop) as u32;
        assert_eq!(header.n_bins, expected_bins);
        assert_eq!(
            frame.len(),
            24 + expected_bins as usize * freq_bins as usize,
            "payload must be n_bins x freq_bins u8"
        );
    }
}

#[test]
fn request_id_is_echoed_verbatim() {
    let repo = SignalRepository::new();
    let id = repo.open(ASK, None).unwrap().signal_id;
    for rid in [0u32, 1, 65_535, u32::MAX] {
        let frame = repo.spectrogram_tile(&id, 0, 4096, 128, 256, rid).unwrap();
        assert_eq!(decode_header(&frame).request_id, rid);
    }
}

#[test]
fn loudest_sample_reaches_top_of_scale() {
    let repo = SignalRepository::new();
    let id = repo.open(ASK, None).unwrap().signal_id;
    let frame = repo.spectrogram_tile(&id, 0, 13710, 2048, 4096, 7).unwrap();
    let header = decode_header(&frame);
    let payload = &frame[24..];
    assert_eq!(payload.len(), header.n_bins as usize * 2048);
    // The whole-signal reference maximum falls inside a full-range tile, so
    // at least one byte must be fully bright.
    assert!(payload.contains(&255), "no reference-bright pixel");
}

#[test]
fn short_range_renders_zero_time_bins() {
    let repo = SignalRepository::new();
    let id = repo.open(ASK, None).unwrap().signal_id;
    // 100 < hop (128): zero time bins, header-only frame — valid per the
    // contract.
    let frame = repo.spectrogram_tile(&id, 0, 100, 128, 256, 9).unwrap();
    let header = decode_header(&frame);
    assert_eq!(header.n_bins, 0);
    assert_eq!(frame.len(), 24);
}

#[test]
fn tiles_are_deterministic_and_cached() {
    let repo = SignalRepository::new();
    let id = repo.open(ASK, None).unwrap().signal_id;
    let a = repo
        .spectrogram_tile(&id, 1000, 5000, 128, 256, 11)
        .unwrap();
    let b = repo
        .spectrogram_tile(&id, 1000, 5000, 128, 256, 12)
        .unwrap();
    // Same arguments: identical payload (second call is a cache hit), the
    // echoed request ids differ.
    assert_eq!(&a[24..], &b[24..]);
    assert_ne!(&a[8..12], &b[8..12]);
    // A neighboring range must not collide with the cached tile (frames are
    // anchored at `start`, so shifting the start shifts every window).
    let c = repo
        .spectrogram_tile(&id, 1008, 5008, 128, 256, 13)
        .unwrap();
    assert_ne!(&a[24..], &c[24..]);
}

/// A pure tone at DFT bin 64 (window 256) must light up column 64 — DC
/// first, no fftshift.
#[test]
fn tone_brightens_expected_frequency_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone.complex");
    let mut bytes = Vec::with_capacity(4096 * 8);
    for i in 0..4096u32 {
        let phase = 2.0 * std::f64::consts::PI * 64.0 * i as f64 / 256.0;
        bytes.extend_from_slice(&(phase.cos() as f32).to_le_bytes());
        bytes.extend_from_slice(&(phase.sin() as f32).to_le_bytes());
    }
    std::fs::write(&path, bytes).unwrap();

    let repo = SignalRepository::new();
    let id = repo.open(path.to_str().unwrap(), None).unwrap().signal_id;
    let frame = repo.spectrogram_tile(&id, 0, 4096, 128, 256, 21).unwrap();
    let header = decode_header(&frame);
    assert_eq!(header.n_bins, 4096 / 128);
    let payload = &frame[24..];
    for (t, row) in payload.chunks_exact(128).enumerate() {
        let (best, _) = row
            .iter()
            .enumerate()
            .max_by_key(|&(f, &v)| (v, std::cmp::Reverse(f)))
            .unwrap();
        assert_eq!(best, 64, "time bin {t}: brightest column {best}");
    }
}

#[test]
fn validation_errors() {
    let repo = SignalRepository::new();
    let id = repo.open(ASK, None).unwrap().signal_id;

    let err = repo
        .spectrogram_tile(&id, 0, 13710, 128, 512, 1)
        .unwrap_err();
    assert_eq!(err.code, "invalid-argument", "{err}");

    let err = repo
        .spectrogram_tile(&id, 0, 13710, 100, 256, 1)
        .unwrap_err();
    assert_eq!(err.code, "invalid-argument", "{err}");

    for (start, end) in [(5u64, 5u64), (10u64, 5u64), (0u64, 13711u64)] {
        let err = repo
            .spectrogram_tile(&id, start, end, 128, 256, 1)
            .unwrap_err();
        assert_eq!(err.code, "invalid-range", "({start}, {end}): {err}");
    }

    let err = repo
        .spectrogram_tile("sig-99", 0, 100, 128, 256, 1)
        .unwrap_err();
    assert_eq!(err.code, "unknown-signal");
}
