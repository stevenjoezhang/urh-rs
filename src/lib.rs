//! # urh-core
//!
//! Rust port of the Universal Radio Hacker (URH) signal processing core.
//!
//! Module map (mirrors URH's `signalprocessing` + `cythonext` layers):
//! - [`iq`]: IQ sample containers, file I/O (`.complex*` formats), magnitudes, dB conversion
//! - [`filter`]: FIR/IIR filtering, moving average
//! - [`modulation`]: modulators (ASK/FSK/PSK/GFSK/OQPSK) and demodulation primitives
//! - [`encoding`]: encoding/decoding chains (differential, Manchester, ...) and CRC
//! - [`protocol`]: messages, message types, protocol analysis (bits -> messages)
//! - [`autointerpret`]: automatic signal parameter estimation / message segmentation

pub mod autointerpret;
pub mod encoding;
pub mod filter;
pub mod iq;
pub mod modulation;
pub mod protocol;

pub use num_complex::Complex32;

/// A single bit, stored as `u8` with value 0 or 1 (URH convention).
pub type Bit = u8;

/// A sequence of bits (each element 0 or 1).
pub type Bits = Vec<Bit>;

/// Modulation schemes supported by URH's modulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modulation {
    Ask,
    Fsk,
    Psk,
    Gfsk,
    Oqpsk,
}

impl Modulation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Modulation::Ask => "ASK",
            Modulation::Fsk => "FSK",
            Modulation::Psk => "PSK",
            Modulation::Gfsk => "GFSK",
            Modulation::Oqpsk => "OQPSK",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "ASK" => Modulation::Ask,
            "FSK" => Modulation::Fsk,
            "PSK" => Modulation::Psk,
            "GFSK" => Modulation::Gfsk,
            "OQPSK" => Modulation::Oqpsk,
            _ => return None,
        })
    }
}

/// Number of symbols per carrier state, e.g. ASK order 2 = OOK/2-ASK.
/// URH names this `modulation_order` / `samples_per_symbol` alongside.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModulationParams {
    pub modulation_type: Modulation,
    /// samples per symbol (bit length)
    pub samples_per_symbol: u32,
    /// symbol count per amplitude/frequency/phase state, e.g. 2 for OOK
    pub modulation_order: u32,
    pub center: f32,
    pub tolerance: u32,
    pub noise_mag: f32,
    pub bit_length: u32,
}

impl Default for ModulationParams {
    fn default() -> Self {
        ModulationParams {
            modulation_type: Modulation::Ask,
            samples_per_symbol: 100,
            modulation_order: 2,
            center: 0.0,
            tolerance: 5,
            noise_mag: 0.0,
            bit_length: 100,
        }
    }
}
