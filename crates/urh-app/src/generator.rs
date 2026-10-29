//! Generator service — v2 contract section "Generator (v2, P4)"
//! (docs/ipc-contract.md).
//!
//! # What this module implements
//!
//! [`SignalRepository`] gains two methods: [`generate`] turns a
//! [`GeneratorParams`] into an in-memory `"gen-<n>"` signal (registered in
//! the repository, returned as its [`SignalMeta`](crate::SignalMeta)), and
//! [`save_signal_as`] writes any repository signal to disk as a `.complex`
//! file (interleaved little-endian f32 I/Q). No other state is introduced:
//! generated signals are ordinary repository signals, so they flow through
//! the existing `list`/`open`/analysis/spectrogram paths unchanged.
//!
//! # Carrier and parameter semantics
//!
//! The contract fixes the wire type but not the modulation parameters, so
//! this module pins them (documented interpretation, mirroring the URH
//! simulator defaults):
//!
//! - sample rate: [`DEFAULT_SAMPLE_RATE`] (1 MHz, the repository-wide
//!   default), carried in the signal's metadata only;
//! - carrier: amplitude 1.0, frequency 40 kHz, phase 0 (URH simulator
//!   defaults);
//! - `center` is modulation-dependent:
//!   - ASK: amplitudes `[0, 2·center]` — `center` is the decision threshold,
//!     the UI default 0.5 yields the URH default amplitudes `[0, 1]`;
//!   - FSK/GFSK: tones at `center − 10 kHz` and `center + 10 kHz`;
//!   - PSK: phases `center` and `center + π`;
//!   - OQPSK: phases `center + k·π/2`, `k = 0..3` (2 bits per symbol).
//!
//! # Validation
//!
//! [`generate`] rejects, with `invalid-argument`: an empty or non-0/1
//! `bits` string, `samples_per_bit < 1`, an unknown `modulation`
//! (case-insensitive match of ASK/FSK/PSK/GFSK/OQPSK; there is no separate
//! OOK — 2-ASK with a 0 amplitude *is* OOK), a non-finite `center`, a
//! negative ASK `center` (it is half the bit-1 amplitude),
//! `pause_samples > u32::MAX`, and an odd bit count for OQPSK (the core's
//! OQPSK tail fixup underflows on symbol counts the bits do not back).
//!
//! # Saving
//!
//! [`save_signal_as`] creates or truncates the target file and writes the
//! signal's samples in `.complex` format; I/O failures are `io` errors with
//! the path prefixed. The repository signal itself is left untouched — it
//! stays in-memory (`path = ""`, `display_name = ""`); the frontend offers
//! save-as and, once saved, can `open` the file to get a file-backed copy.

use serde::{Deserialize, Serialize};
use urh::iq::{IqArray, IqFormat};
use urh::modulation::modulate;
use urh::Modulation;

use crate::signal::Signal;
use crate::{AppError, SignalRepository, DEFAULT_SAMPLE_RATE};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Carrier amplitude for generated signals (URH simulator default).
const CARRIER_AMPLITUDE: f32 = 1.0;
/// Carrier frequency in Hz for generated signals (URH simulator default).
const CARRIER_FREQUENCY: f32 = 40_000.0;
/// Carrier phase in radians for generated signals (URH simulator default).
const CARRIER_PHASE: f32 = 0.0;
/// Distance of the two FSK/GFSK tones from `center`, in Hz.
const FSK_TONE_DISTANCE: f32 = 10_000.0;

// ---------------------------------------------------------------------------
// Wire type (contract: "Generator")
// ---------------------------------------------------------------------------

/// Generator parameterization (contract: "Generator"). `Default` is a
/// construction convenience only — the all-default config (empty bits,
/// `samples_per_bit` 0) fails [`generate`] validation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GeneratorParams {
    /// One of ASK, FSK, PSK, GFSK, OQPSK (case-insensitive).
    pub modulation: String,
    /// The bits to modulate, e.g. `"1011"`; non-empty, only '0'/'1'.
    pub bits: String,
    /// Samples per bit (symbols for OQPSK's 2 bits per symbol); >= 1.
    pub samples_per_bit: u32,
    /// Zero samples appended after the last bit; 0 is allowed.
    pub pause_samples: u64,
    /// Modulation-dependent parameter (see the module's semantics table).
    pub center: f32,
}

// ---------------------------------------------------------------------------
// Public service methods (contract: "Generator")
// ---------------------------------------------------------------------------

impl SignalRepository {
    /// Modulate `params.bits` into a new in-memory signal (contract:
    /// "Generator"). Registers the signal under a fresh `"gen-<n>"` id with
    /// `path = ""` (the frontend offers save-as via
    /// [`save_signal_as`](SignalRepository::save_signal_as)) and returns its
    /// metadata. See the module docs for the carrier/`center` semantics and
    /// the validation rules (`invalid-argument` on violation).
    pub fn generate(&self, params: &GeneratorParams) -> Result<crate::SignalMeta, AppError> {
        let modulation = validate(params)?;

        let bits: Vec<urh::Bit> = params.bits.bytes().map(|b| b - b'0').collect();
        let parameters = parameters_for(modulation, params.center);
        // OQPSK carries 2 bits per symbol; everything else 1.
        let bits_per_symbol: u16 = if modulation == Modulation::Oqpsk {
            2
        } else {
            1
        };

        let iq = modulate(
            &bits,
            params.samples_per_bit,
            modulation,
            &parameters,
            bits_per_symbol,
            CARRIER_AMPLITUDE,
            CARRIER_FREQUENCY,
            CARRIER_PHASE,
            DEFAULT_SAMPLE_RATE as f32,
            params.pause_samples.min(u32::MAX as u64) as u32,
            0,
        );

        let id = self.alloc_id("gen-");
        let signal = self.register(
            id.clone(),
            Signal::new(id, "", IqArray::from_vec(iq), DEFAULT_SAMPLE_RATE),
        );
        Ok(signal.meta())
    }

    /// Write a repository signal to `path` as a `.complex` file (contract:
    /// "Generator"): interleaved little-endian f32 I/Q, creating or
    /// truncating the file. Works for generated and file-backed signals
    /// alike; the repository entry is not modified. Unknown ids are
    /// `unknown-signal`, file errors are `io` (path prefixed).
    pub fn save_signal_as(&self, signal_id: &str, path: &str) -> Result<(), AppError> {
        let signal = self.lookup(signal_id)?;
        IqArray::from_vec(signal.samples().to_vec())
            .to_file_with_format(path, IqFormat::ComplexF32)
            .map_err(|err| AppError {
                code: AppError::CODE_IO,
                message: format!("{path}: {err}"),
            })
    }
}

// ---------------------------------------------------------------------------
// Validation and parameter derivation
// ---------------------------------------------------------------------------

/// Validate the params (see the module docs) and resolve the modulation.
fn validate(params: &GeneratorParams) -> Result<Modulation, AppError> {
    if params.bits.is_empty() {
        return Err(AppError::invalid_argument(
            "bits must be a non-empty string of 0s and 1s",
        ));
    }
    if let Some(bad) = params.bits.bytes().find(|b| !matches!(b, b'0' | b'1')) {
        return Err(AppError::invalid_argument(format!(
            "bits must contain only '0' and '1', found {:?}",
            char::from(bad)
        )));
    }
    if params.samples_per_bit < 1 {
        return Err(AppError::invalid_argument(format!(
            "samples_per_bit must be at least 1, got {}",
            params.samples_per_bit
        )));
    }
    let modulation = Modulation::from_str(&params.modulation).ok_or_else(|| {
        AppError::invalid_argument(format!(
            "unknown modulation {:?} (expected ASK, FSK, PSK, GFSK or OQPSK)",
            params.modulation
        ))
    })?;
    if !params.center.is_finite() {
        return Err(AppError::invalid_argument(format!(
            "center must be a finite number, got {}",
            params.center
        )));
    }
    if modulation == Modulation::Ask && params.center < 0.0 {
        return Err(AppError::invalid_argument(format!(
            "ASK center must be non-negative (it is half the bit-1 amplitude), got {}",
            params.center
        )));
    }
    if params.pause_samples > u32::MAX as u64 {
        return Err(AppError::invalid_argument(format!(
            "pause_samples must not exceed {}, got {}",
            u32::MAX,
            params.pause_samples
        )));
    }
    if modulation == Modulation::Oqpsk && !params.bits.len().is_multiple_of(2) {
        return Err(AppError::invalid_argument(
            "OQPSK modulates 2 bits per symbol, so bits needs an even number of bits",
        ));
    }
    Ok(modulation)
}

/// Modulation parameters derived from `center` (see the module's semantics
/// table): indexed by the symbol's bit value (bit pair for OQPSK).
fn parameters_for(modulation: Modulation, center: f32) -> Vec<f32> {
    match modulation {
        // Bit-0 amplitude 0 (silence), bit-1 amplitude 2 * center.
        Modulation::Ask => vec![0.0, 2.0 * center],
        // Tones at center +/- 10 kHz.
        Modulation::Fsk | Modulation::Gfsk => {
            vec![center - FSK_TONE_DISTANCE, center + FSK_TONE_DISTANCE]
        }
        // Phases center and center + pi.
        Modulation::Psk => vec![center, center + std::f32::consts::PI],
        // Phases center + k * pi/2.
        Modulation::Oqpsk => vec![
            center,
            center + std::f32::consts::FRAC_PI_2,
            center + std::f32::consts::PI,
            center + 3.0 * std::f32::consts::FRAC_PI_2,
        ],
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use urh::Complex32;

    #[test]
    fn parameters_match_modulation_semantics() {
        let c = 0.25f32;
        assert_eq!(parameters_for(Modulation::Ask, c), vec![0.0, 0.5]);
        assert_eq!(
            parameters_for(Modulation::Fsk, c),
            vec![c - 10_000.0, c + 10_000.0]
        );
        assert_eq!(
            parameters_for(Modulation::Gfsk, c),
            vec![c - 10_000.0, c + 10_000.0]
        );
        assert_eq!(
            parameters_for(Modulation::Psk, c),
            vec![c, c + std::f32::consts::PI]
        );
        assert_eq!(
            parameters_for(Modulation::Oqpsk, c),
            vec![
                c,
                c + std::f32::consts::FRAC_PI_2,
                c + std::f32::consts::PI,
                c + 3.0 * std::f32::consts::FRAC_PI_2
            ]
        );
    }

    #[test]
    fn validation_rejects_bad_params() {
        let ok = |bits: &str, spb: u32, modulation: &str, center: f32| GeneratorParams {
            modulation: modulation.to_string(),
            bits: bits.to_string(),
            samples_per_bit: spb,
            pause_samples: 0,
            center,
        };

        // The good cases (one per modulation; OQPSK needs even bits).
        assert!(validate(&ok("1011", 100, "ASK", 0.5)).is_ok());
        assert!(validate(&ok("10", 100, "OQPSK", 0.0)).is_ok());

        for bad in [
            ok("", 100, "ASK", 0.5),               // empty bits
            ok("10a1", 100, "ASK", 0.5),           // non-0/1 bits
            ok(" 101", 100, "ASK", 0.5),           // whitespace bit
            ok("1011", 0, "ASK", 0.5),             // samples_per_bit 0
            ok("1011", 100, "OOK", 0.5),           // no such modulation
            ok("1011", 100, "", 0.5),              // empty modulation
            ok("1011", 100, "ASK", f32::NAN),      // non-finite center
            ok("1011", 100, "ASK", f32::INFINITY), // non-finite center
            ok("1011", 100, "ASK", -0.1),          // negative ASK center
            GeneratorParams {
                pause_samples: u32::MAX as u64 + 1,
                ..ok("1011", 100, "ASK", 0.5)
            },
            ok("101", 100, "OQPSK", 0.0), // odd OQPSK bits
        ] {
            let err = validate(&bad).unwrap_err();
            assert_eq!(err.code, "invalid-argument", "{bad:?}: {err}");
        }

        // Pause 0 is allowed, modulation matching is case-insensitive, and
        // negative centers are fine for non-ASK modulations.
        assert!(validate(&ok("1011", 100, "ask", 0.5)).is_ok());
        assert!(validate(&ok("1011", 100, "Ask", 0.0)).is_ok());
        assert!(validate(&ok("1011", 1, "GFSK", -5.0)).is_ok());
    }

    /// ASK "1011" @ 100 spb, center 0.5 (amplitudes [0, 1]): exact envelope.
    #[test]
    fn generate_ask_pattern_is_exact() {
        let repo = SignalRepository::new();
        let meta = repo
            .generate(&GeneratorParams {
                modulation: "ASK".to_string(),
                bits: "1011".to_string(),
                samples_per_bit: 100,
                pause_samples: 0,
                center: 0.5,
            })
            .unwrap();

        assert!(meta.signal_id.starts_with("gen-"));
        assert_eq!(meta.sample_count, 400);
        assert_eq!(meta.path, "");
        assert_eq!(meta.display_name, "");
        assert_eq!(meta.format, "complex");
        assert_eq!(meta.sample_rate, DEFAULT_SAMPLE_RATE);

        let samples = repo.lookup(&meta.signal_id).unwrap().samples().to_vec();
        for (k, s) in samples.iter().enumerate() {
            let expected = if (100..200).contains(&k) { 0.0 } else { 1.0 };
            assert!(
                (s.norm() - expected).abs() < 1e-5,
                "sample {k}: magnitude {} vs expected {expected}",
                s.norm()
            );
        }
        // The carrier really rotates at 40 kHz / 1 MHz = 25 samples/period:
        // sample 6 sits at phase 6 * 2π/25.
        assert!(samples[0].im.abs() < 1e-6);
        let phase = 2.0 * std::f32::consts::PI * 6.0 / 25.0;
        assert!((samples[6].im - phase.sin()).abs() < 1e-5);
    }

    #[test]
    fn generate_pause_appends_zero_tail() {
        let repo = SignalRepository::new();
        let meta = repo
            .generate(&GeneratorParams {
                modulation: "ASK".to_string(),
                bits: "1011".to_string(),
                samples_per_bit: 100,
                pause_samples: 250,
                center: 0.5,
            })
            .unwrap();
        assert_eq!(meta.sample_count, 650);
        let signal = repo.lookup(&meta.signal_id).unwrap();
        let samples = signal.samples();
        assert!(samples[399].norm() > 0.9, "bit sample before the pause");
        for s in &samples[400..650] {
            assert_eq!(*s, Complex32::new(0.0, 0.0), "pause must be silent");
        }
    }

    /// Every modulation produces the contract-sized, non-trivial waveform.
    #[test]
    fn generate_all_modulations() {
        let repo = SignalRepository::new();
        for (modulation, bits, bps) in [
            ("ASK", "1011", 1u16),
            ("FSK", "1011", 1),
            ("PSK", "1011", 1),
            ("GFSK", "1011", 1),
            ("OQPSK", "1011", 2), // even bit count, 2 bits/symbol
        ] {
            let meta = repo
                .generate(&GeneratorParams {
                    modulation: modulation.to_string(),
                    bits: bits.to_string(),
                    samples_per_bit: 100,
                    pause_samples: 0,
                    center: 0.5,
                })
                .unwrap_or_else(|err| panic!("{modulation}: {err}"));
            let symbols = bits.len() / bps as usize;
            assert_eq!(
                meta.sample_count,
                (symbols * 100) as u64,
                "{modulation} sample count"
            );

            let samples = repo.lookup(&meta.signal_id).unwrap().samples().to_vec();
            let max_magnitude = samples.iter().map(|s| s.norm()).fold(0.0f32, f32::max);
            assert!(
                max_magnitude > 0.5,
                "{modulation} waveform is trivially small"
            );
            // FSK/PSK/GFSK carry a constant 1.0 envelope: no sample may go
            // silent. OQPSK zeroes one quadrature per edge symbol (the
            // core's OQPSK half-symbol fixup), so only its peak is checked.
            match modulation {
                "ASK" => {}
                "OQPSK" => {
                    let max = samples.iter().map(|s| s.norm()).fold(0.0f32, f32::max);
                    assert!(max > 0.9, "{modulation} waveform is trivially small");
                }
                _ => {
                    for (k, s) in samples.iter().enumerate() {
                        assert!(s.norm() > 0.5, "{modulation} sample {k} went silent");
                    }
                }
            }
        }
    }

    #[test]
    fn save_signal_as_roundtrips() {
        let repo = SignalRepository::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gen.complex");
        let path = path.to_str().unwrap();

        let meta = repo
            .generate(&GeneratorParams {
                modulation: "FSK".to_string(),
                bits: "1011".to_string(),
                samples_per_bit: 100,
                pause_samples: 0,
                center: 0.0,
            })
            .unwrap();
        repo.save_signal_as(&meta.signal_id, path).unwrap();

        // File layout: interleaved LE f32, 8 bytes per complex sample.
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len(), meta.sample_count as usize * 8);
        let saved: Vec<Complex32> = bytes
            .chunks_exact(8)
            .map(|c| {
                Complex32::new(
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect();
        assert_eq!(saved, repo.lookup(&meta.signal_id).unwrap().samples());

        // The file reopens as an ordinary repository signal.
        let reopened = repo.open(path, None).unwrap();
        assert_eq!(reopened.sample_count, meta.sample_count);
        assert_eq!(reopened.format, "complex");
        assert_ne!(reopened.signal_id, meta.signal_id);

        // The in-memory original is untouched (still path-less) and remains
        // saveable a second time (create-or-truncate).
        assert_eq!(repo.lookup(&meta.signal_id).unwrap().meta().path, "");
        repo.save_signal_as(&meta.signal_id, path).unwrap();
        assert_eq!(
            std::fs::read(path).unwrap().len(),
            meta.sample_count as usize * 8
        );
    }

    #[test]
    fn save_signal_as_errors() {
        let repo = SignalRepository::new();
        let dir = tempfile::tempdir().unwrap();

        let err = repo
            .save_signal_as("gen-9999", "whatever.complex")
            .unwrap_err();
        assert_eq!(err.code, "unknown-signal");

        let meta = repo
            .generate(&GeneratorParams {
                modulation: "ASK".to_string(),
                bits: "1".to_string(),
                samples_per_bit: 10,
                pause_samples: 0,
                center: 0.5,
            })
            .unwrap();
        // A file inside a directory that cannot exist.
        let bad = dir.path().join("no-such-dir").join("x.complex");
        let err = repo
            .save_signal_as(&meta.signal_id, bad.to_str().unwrap())
            .unwrap_err();
        assert_eq!(err.code, "io");
        assert!(err.message.contains("no-such-dir"), "{err}");
    }
}
