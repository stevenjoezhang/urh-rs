//! Integration tests for the v2 generator service
//! (docs/ipc-contract.md, "Generator").
//!
//! As with the other v2 services, `GeneratorParams` lives in a private
//! module and is not nameable outside the crate: tests construct it through
//! inference — `default_of()` plus a type-pinning call to `generate`, which
//! rejects the default (empty bits) harmlessly — and then mutate fields.
//! Exact-sample assertions live in the unit tests inside
//! `crates/urh-app/src/generator.rs`; these tests drive the public flow:
//! generate, save-as, reopen.

use std::sync::OnceLock;

use urh_app::SignalRepository;

fn default_of<T: Default>() -> T {
    T::default()
}

/// Build a `GeneratorParams` by field. The pinning call inside resolves the
/// unnameable params type; the default params are rejected by validation, so
/// it has no side effects.
macro_rules! generator_params {
    ($repo:expr, { $($field:ident : $value:expr),* $(,)? }) => {{
        let mut params = default_of();
        let _ = $repo.generate(&params); // type-pinning call
        $(params.$field = $value;)*
        params
    }};
}

/// Generator tests can use fresh repositories: generated signals live in
/// the repository itself (only captures use the process-wide registry), but
/// a shared one keeps ids comparable across tests anyway.
fn shared_repo() -> &'static SignalRepository {
    static REPO: OnceLock<SignalRepository> = OnceLock::new();
    REPO.get_or_init(SignalRepository::new)
}

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

/// The contract's acceptance path: ASK `1011` @ 100 samples/bit round-trips
/// through `save_signal_as` and `open` with matching counts and a
/// non-trivial envelope.
#[test]
fn ask_1011_roundtrips_through_save_and_reopen() {
    let repo = shared_repo();
    let params = generator_params!(repo, {
        modulation: "ASK".to_string(),
        bits: "1011".to_string(),
        samples_per_bit: 100,
        pause_samples: 0,
        center: 0.5,
    });
    let meta = repo.generate(&params).unwrap();
    assert!(meta.signal_id.starts_with("gen-"), "{}", meta.signal_id);
    assert_eq!(meta.sample_count, 400);
    assert_eq!(meta.format, "complex");
    assert_eq!(meta.path, "");
    assert_eq!(meta.display_name, "");
    assert_eq!(meta.sample_rate, 1e6);
    assert!(
        (meta.duration_secs - 400.0 / 1e6).abs() < 1e-15,
        "{}",
        meta.duration_secs
    );

    // Generated signals appear in the repository listing.
    assert!(repo.list().iter().any(|m| m.signal_id == meta.signal_id));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gen.complex");
    repo.save_signal_as(&meta.signal_id, path.to_str().unwrap())
        .unwrap();

    // .complex layout: 8 bytes per complex sample, interleaved LE f32.
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len(), 400 * 8);

    // Envelope: bits 1,0,1,1 with amplitudes [0, 2*0.5] — one silent bit
    // period in the middle, live carrier elsewhere.
    let mags = magnitudes(&bytes);
    for (k, mag) in mags.iter().enumerate() {
        let expected = if (100..200).contains(&k) { 0.0 } else { 1.0 };
        assert!(
            (mag - expected).abs() < 1e-4,
            "sample {k}: magnitude {mag} vs expected {expected}"
        );
    }

    // The saved file reopens as an ordinary repository signal with matching
    // metadata (and its own id).
    let reopened = repo.open(path.to_str().unwrap(), None).unwrap();
    assert_eq!(reopened.sample_count, 400);
    assert_eq!(reopened.format, "complex");
    assert_ne!(reopened.signal_id, meta.signal_id);
}

/// `pause_samples` appends exactly that many silent samples.
#[test]
fn pause_appends_a_silent_tail() {
    let repo = shared_repo();
    let params = generator_params!(repo, {
        modulation: "ASK".to_string(),
        bits: "1011".to_string(),
        samples_per_bit: 100,
        pause_samples: 250,
        center: 0.5,
    });
    let meta = repo.generate(&params).unwrap();
    assert_eq!(meta.sample_count, 650);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paused.complex");
    repo.save_signal_as(&meta.signal_id, path.to_str().unwrap())
        .unwrap();
    let mags = magnitudes(&std::fs::read(&path).unwrap());
    assert!(mags[399] > 0.9, "the last bit sample precedes the pause");
    for (k, mag) in mags[400..650].iter().enumerate() {
        assert_eq!(*mag, 0.0, "pause sample {k} must be silent");
    }
}

/// All five modulations generate the contract-sized waveform, and nothing
/// but the ASK zero bit goes silent.
#[test]
fn all_modulations_generate() {
    let repo = shared_repo();
    let dir = tempfile::tempdir().unwrap();
    for (modulation, bits, bits_per_symbol) in [
        ("ASK", "1011", 1u16),
        ("FSK", "1011", 1),
        ("PSK", "1011", 1),
        ("GFSK", "1011", 1),
        ("OQPSK", "1011", 2),
    ] {
        let params = generator_params!(repo, {
            modulation: modulation.to_string(),
            bits: bits.to_string(),
            samples_per_bit: 100,
            pause_samples: 0,
            center: 0.5,
        });
        let meta = repo
            .generate(&params)
            .unwrap_or_else(|err| panic!("{modulation}: {err}"));
        let symbols = bits.len() / bits_per_symbol as usize;
        assert_eq!(
            meta.sample_count,
            (symbols * 100) as u64,
            "{modulation} sample count"
        );

        // Non-trivial waveform, checked through the saved file.
        let path = dir
            .path()
            .join(format!("{}.complex", modulation.to_lowercase()));
        repo.save_signal_as(&meta.signal_id, path.to_str().unwrap())
            .unwrap();
        let mags = magnitudes(&std::fs::read(&path).unwrap());
        let max = mags.iter().cloned().fold(0.0f32, f32::max);
        assert!(max > 0.5, "{modulation} waveform is trivially small");
    }
}

/// Validation failures are `invalid-argument`, with helpful messages.
#[test]
fn generate_validation_errors() {
    let repo = shared_repo();
    let mut params = generator_params!(repo, {
        modulation: "ASK".to_string(),
        bits: "1011".to_string(),
        samples_per_bit: 100,
        pause_samples: 0,
        center: 0.5,
    });

    for (what, bad) in [
        ("empty bits", {
            let mut p = params.clone();
            p.bits = String::new();
            p
        }),
        ("non-0/1 bits", {
            let mut p = params.clone();
            p.bits = "10a1".to_string();
            p
        }),
        ("samples_per_bit 0", {
            let mut p = params.clone();
            p.samples_per_bit = 0;
            p
        }),
        ("unknown modulation", {
            let mut p = params.clone();
            p.modulation = "QAM".to_string();
            p
        }),
        ("OOK is not a modulation of this build", {
            let mut p = params.clone();
            p.modulation = "OOK".to_string();
            p
        }),
        ("non-finite center", {
            let mut p = params.clone();
            p.center = f32::NAN;
            p
        }),
        ("negative ASK center", {
            let mut p = params.clone();
            p.center = -0.5;
            p
        }),
        ("oversized pause", {
            let mut p = params.clone();
            p.pause_samples = u32::MAX as u64 + 1;
            p
        }),
        ("odd OQPSK bits", {
            let mut p = params.clone();
            p.modulation = "OQPSK".to_string();
            p.bits = "101".to_string();
            p
        }),
    ] {
        let err = repo.generate(&bad).unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{what}: {err}");
    }

    // The good cases: case-insensitive modulation, pause 0, non-ASK centers
    // may be negative.
    params.modulation = "ask".to_string();
    assert!(repo.generate(&params).is_ok());
    params.modulation = "GFSK".to_string();
    params.center = -1.0;
    assert!(repo.generate(&params).is_ok());
}

/// `save_signal_as` error codes: unknown signals and I/O failures.
#[test]
fn save_signal_as_errors() {
    let repo = shared_repo();

    let err = repo
        .save_signal_as("gen-999999", "/tmp/nowhere.complex")
        .unwrap_err();
    assert_eq!(err.code, "unknown-signal", "{err}");

    let params = generator_params!(repo, {
        modulation: "ASK".to_string(),
        bits: "1".to_string(),
        samples_per_bit: 10,
        pause_samples: 0,
        center: 0.5,
    });
    let meta = repo.generate(&params).unwrap();
    let err = repo
        .save_signal_as(&meta.signal_id, "/no-such-dir-xyz/gen.complex")
        .unwrap_err();
    assert_eq!(err.code, "io", "{err}");
    assert!(err.message.contains("no-such-dir-xyz"), "{err}");
}
