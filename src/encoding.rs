//! Encoding chain and CRC — ported from URH's
//! `urh/cythonext/util.pyx` (crc functions), `urh/util/GenericCRC.py` and
//! `urh/signalprocessing/Encoding.py`.
//!
//! Bit sequences are `&[Bit]` (crate::Bit = u8, elements 0/1), matching the
//! Python reference where bit arrays are arrays of 0/1.

use crate::Bit;

// ---------------------------------------------------------------------------
// small helpers (mirrors of urh.util.util)
// ---------------------------------------------------------------------------

pub fn str2bits(s: &str) -> Vec<Bit> {
    s.bytes()
        .map(|b| if b == b'1' { 1 } else { 0 })
        .collect()
}

pub fn bits2str(bits: &[Bit]) -> String {
    bits.iter()
        .map(|&b| if b != 0 { '1' } else { '0' })
        .collect()
}

pub fn hex2bits(hex_str: &str) -> Vec<Bit> {
    let h = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let mut out = Vec::new();
    for c in h.bytes() {
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => continue,
        };
        for k in (0..4).rev() {
            out.push(((v >> k) & 1) as Bit);
        }
    }
    out
}

/// Mirror of util.bit2hex (no padding): nibbles, last partial nibble allowed.
pub fn bits2hex(bits: &[Bit]) -> String {
    let s = bits2str(bits);
    let mut out = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + 4).min(bytes.len());
        let v = u8::from_str_radix(std::str::from_utf8(&bytes[i..end]).unwrap(), 2).unwrap_or(0);
        out.push(std::char::from_digit(v as u32, 16).unwrap());
        i += 4;
    }
    out
}

/// util.number_to_bits: binary, MSB first, zero padded to `length`.
pub fn number_to_bits(n: u64, length: usize) -> Vec<Bit> {
    (0..length)
        .map(|i| ((n >> (length - 1 - i)) & 1) as Bit)
        .collect()
}

pub fn bits2number(bits: &[Bit]) -> u64 {
    bits.iter().fold(0u64, |acc, &b| (acc << 1) | (b as u64))
}

// ---------------------------------------------------------------------------
// ErrorState
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorState {
    Success,
    PreambleNotFound,
    SyncNotFound,
    EofNotFound,
    WrongInput,
    MissingExternalProgram,
    InvalidCutmark,
    Misc,
    WrongParameters,
}

impl std::fmt::Display for ErrorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ErrorState::Success => "success",
            ErrorState::PreambleNotFound => "preamble not found",
            ErrorState::SyncNotFound => "sync not found",
            ErrorState::EofNotFound => "eof not found",
            ErrorState::WrongInput => "wrong input",
            ErrorState::MissingExternalProgram => "Please set external de/encoder program!",
            ErrorState::InvalidCutmark => "cutmark is not valid",
            ErrorState::Misc => "general error",
            ErrorState::WrongParameters => "wrong parameters",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// CRC core (port of cythonext/util.pyx)
// ---------------------------------------------------------------------------

/// 2^(n-1) - 1 as uint64; for n == 65 this is u64::MAX (all CRC bits).
fn crc_mask_of(poly_order: usize) -> u64 {
    if poly_order > 64 {
        u64::MAX
    } else {
        (1u64 << (poly_order - 1)) - 1
    }
}

/// pyx arr_to_number. `start` skips leading bits (used with 1 for polynomials
/// to drop the x^n coefficient).
fn arr_to_number(inpt: &[Bit], reverse: bool, start: usize) -> u64 {
    let len_inpt = inpt.len();
    let mut result = 0u64;
    for i in start..len_inpt {
        if !reverse {
            if inpt[len_inpt - 1 - i + start] != 0 {
                result |= 1 << (i - start);
            }
        } else if inpt[i] != 0 {
            result |= 1 << (i - start);
        }
    }
    result
}

fn reverse_all_bits(crc: u64, poly_order: usize, crc_mask: u64) -> u64 {
    let mut temp = 0u64;
    for i in 0..poly_order - 1 {
        if crc & (1 << i) != 0 {
            temp |= 1 << (poly_order - 2 - i);
        }
    }
    temp & crc_mask
}

fn little_endian_swap(crc: u64, poly_order: usize) -> u64 {
    if poly_order - 1 == 16 {
        ((crc << 8) & 0xFF00) | (crc >> 8)
    } else if poly_order - 1 == 32 {
        ((crc << 24) & 0xFF00_0000)
            | ((crc << 8) & 0x00FF_0000)
            | ((crc >> 8) & 0x0000_FF00)
            | (crc >> 24)
    } else if poly_order - 1 == 64 {
        ((crc << 56) & 0xFF00_0000_0000_0000)
            | (crc >> 56)
            | ((crc >> 40) & 0x0000_0000_0000_FF00)
            | ((crc << 40) & 0x00FF_0000_0000_0000)
            | ((crc << 24) & 0x0000_FF00_0000_0000)
            | ((crc >> 24) & 0x0000_0000_00FF_0000)
            | ((crc << 8) & 0x0000_00FF_0000_0000)
            | ((crc >> 8) & 0x0000_0000_FF00_0000)
    } else {
        crc
    }
}

/// Bit-by-bit CRC, direct port of `pyx crc`. Inputs are bit sequences.
#[allow(clippy::too_many_arguments)]
pub fn crc(
    inpt: &[Bit],
    polynomial: &[Bit],
    start_value: &[Bit],
    final_xor: &[Bit],
    lsb_first: bool,
    reverse_polynomial: bool,
    reverse_all: bool,
    little_endian: bool,
) -> u64 {
    let len_inpt = inpt.len();
    let poly_order = polynomial.len();
    let crc_mask = crc_mask_of(poly_order);
    let poly_mask = (crc_mask + 1) >> 1;
    let poly_int = arr_to_number(polynomial, reverse_polynomial, 1) & crc_mask;

    let mut crcv = arr_to_number(start_value, false, 0) & crc_mask;

    let mut i = 0usize;
    while i < len_inpt + 7 {
        for j in 0..8 {
            let idx = if lsb_first { i + (7 - j) } else { i + j };
            if idx >= len_inpt {
                break;
            }
            // (crc & poly_mask) > 0  <=>  MSB of register set
            if ((crcv & poly_mask) > 0) != (inpt[idx] != 0) {
                crcv = (crcv << 1) & crc_mask;
                crcv ^= poly_int;
            } else {
                crcv = (crcv << 1) & crc_mask;
            }
        }
        i += 8;
    }

    crcv ^= arr_to_number(final_xor, false, 0) & crc_mask;

    if reverse_all {
        crcv = reverse_all_bits(crcv, poly_order, crc_mask);
    }
    if little_endian {
        crcv = little_endian_swap(crcv, poly_order);
    }
    crcv & crc_mask
}

/// pyx calculate_cache: table of 2^bits entries.
pub fn calculate_cache(polynomial: &[Bit], reverse_polynomial: bool, bits: u32) -> Vec<u64> {
    let poly_order = polynomial.len();
    let crc_mask = crc_mask_of(poly_order);
    let poly_mask = (crc_mask + 1) >> 1;
    let poly_int = arr_to_number(polynomial, reverse_polynomial, 1) & crc_mask;

    let mut cache = vec![0u64; 1usize << bits];
    for (i, entry) in cache.iter_mut().enumerate() {
        let mut crcv = (i as u64) << (poly_order - 1 - bits as usize);
        for _ in 0..bits {
            if (crcv & poly_mask) > 0 {
                crcv = (crcv << 1) & crc_mask;
                crcv ^= poly_int;
            } else {
                crcv = (crcv << 1) & crc_mask;
            }
        }
        *entry = crcv;
    }
    cache
}

/// pyx cached_crc. `start_value` is copied (the pyx version mutates it for the
/// remainder call).
#[allow(clippy::too_many_arguments)]
pub fn cached_crc(
    cache: &[u64],
    bits: u32,
    inpt: &[Bit],
    polynomial: &[Bit],
    start_value: &[Bit],
    final_xor: &[Bit],
    lsb_first: bool,
    reverse_polynomial: bool,
    reverse_all: bool,
    little_endian: bool,
) -> u64 {
    let len_inpt = inpt.len();
    let poly_order = polynomial.len();
    let crc_mask = crc_mask_of(poly_order);

    if len_inpt < bits as usize {
        return crc(
            inpt, polynomial, start_value, final_xor, lsb_first, reverse_polynomial, reverse_all,
            little_endian,
        );
    }

    let mut crcv = arr_to_number(start_value, false, 0) & crc_mask;
    let mut i = 0usize;
    while i < len_inpt - bits as usize + 1 {
        let mut data = 0u64;
        if lsb_first {
            for j in 0..bits as usize {
                if inpt[i + j] != 0 {
                    data |= 1 << j;
                }
            }
        } else {
            for j in 0..bits as usize {
                if inpt[i + bits as usize - 1 - j] != 0 {
                    data |= 1 << j;
                }
            }
        }
        let pos = (crcv >> (poly_order - bits as usize - 1)) ^ data;
        crcv = ((crcv << bits) ^ cache[pos as usize]) & crc_mask;
        i += bits as usize;
    }

    if !len_inpt.is_multiple_of(bits as usize) {
        // compute the rest with the normal function, using the current crc as start value
        let rem = len_inpt % bits as usize;
        let mut sv: Vec<Bit> = start_value.to_vec();
        for k in 0..sv.len() {
            let last = sv.len() - 1 - k;
            sv[last] = if (crcv & (1 << k)) > 0 { 1 } else { 0 };
        }
        crc(
            &inpt[len_inpt - rem..],
            polynomial,
            &sv,
            final_xor,
            lsb_first,
            reverse_polynomial,
            reverse_all,
            little_endian,
        )
    } else {
        crcv ^= arr_to_number(final_xor, false, 0) & crc_mask;
        if reverse_all {
            crcv = reverse_all_bits(crcv, poly_order, crc_mask);
        }
        if little_endian {
            crcv = little_endian_swap(crcv, poly_order);
        }
        crcv & crc_mask
    }
}

/// pyx get_crc_datarange: brute-force the data range whose CRC matches the
/// `len_crc` bits starting at `vrfy_crc_start`. Can be slow on long inputs.
/// Returns (start, end); (0, 0) means nothing found.
#[allow(clippy::too_many_arguments)]
pub fn get_crc_datarange(
    inpt: &[Bit],
    polynomial: &[Bit],
    vrfy_crc_start: usize,
    start_value: &[Bit],
    final_xor: &[Bit],
    lsb_first: bool,
    reverse_polynomial: bool,
    reverse_all: bool,
    little_endian: bool,
) -> (usize, usize) {
    let len_inpt = inpt.len();
    let poly_order = polynomial.len();
    let len_crc = poly_order - 1;

    // pyx: vrfy_crc_start is uint; "vrfy_crc_start-1+len_crc >= len_inpt or vrfy_crc_start < 2"
    if vrfy_crc_start == 0 {
        // would have wrapped in C; treat as invalid like the < 2 check
        return (0, 0);
    }
    if vrfy_crc_start - 1 + len_crc >= len_inpt || vrfy_crc_start < 2 {
        return (0, 0);
    }

    let crc_mask = crc_mask_of(poly_order);
    let poly_mask = (crc_mask + 1) >> 1;
    let poly_int = arr_to_number(polynomial, reverse_polynomial, 1) & crc_mask;
    let final_xor_int = arr_to_number(final_xor, false, 0) & crc_mask;
    let vrfy_crc_int = arr_to_number(&inpt[vrfy_crc_start..vrfy_crc_start + len_crc], false, 0)
        & crc_mask;
    let mut crcvalue = arr_to_number(start_value, false, 0) & crc_mask;
    let data_end = vrfy_crc_start;

    // step[] is all zeros except step[0] = 1 (pyx allocates it but only sets [0])
    let step = |idx: usize| -> Bit {
        if idx == 0 {
            1
        } else {
            0
        }
    };

    // crcvalue is initialized with start_value
    let mut steps = vec![0u64; len_inpt + 2];
    let mut i = 0usize;
    while i < data_end + 7 {
        for j in 0..8 {
            let idx = if lsb_first { i + (7 - j) } else { i + j };
            if idx >= data_end {
                break;
            }
            if ((crcvalue & poly_mask) > 0) != (step(idx) != 0) {
                crcvalue = (crcvalue << 1) & crc_mask;
                crcvalue ^= poly_int;
            } else {
                crcvalue = (crcvalue << 1) & crc_mask;
            }
            // Save steps XORed with final_xor
            steps[idx] = crcvalue ^ final_xor_int;
        }
        i += 8;
    }

    // Reverse and little endian
    if reverse_all || little_endian {
        for k in 0..data_end {
            let mut s = steps[k];
            if reverse_all {
                // faithful to the pyx, which indexes steps[j] (== steps[poly_order-1])
                // instead of steps[i] here
                let temp = reverse_all_bits(s, poly_order, crc_mask);
                steps[poly_order - 1] = temp;
                s = steps[k];
            }
            if little_endian {
                let swapped = little_endian_swap(s, poly_order);
                steps[poly_order - 1] = swapped;
                s = steps[k];
            }
            steps[k] = s;
        }
    }

    // Compute crc over inpt[:data_end]
    crcvalue = crc(
        &inpt[..data_end],
        polynomial,
        start_value,
        final_xor,
        lsb_first,
        reverse_polynomial,
        reverse_all,
        little_endian,
    );

    if vrfy_crc_int == crcvalue {
        return (0, data_end);
    }

    let mut found = false;
    let mut i = 0usize;
    while i < data_end - 1 {
        let mut offset = 0usize;
        // skip leading 0s in data (doesn't change crc...)
        while i + offset < data_end - 1 && inpt[i + offset] == 0 {
            offset += 1;
        }
        // XOR delta=crc(10000...) to last crc value to create next crc value
        crcvalue ^= steps[data_end - i - offset - 1];
        if found {
            return (i, data_end); // Return start_data, end_data
        }
        if vrfy_crc_int == crcvalue {
            found = true;
        }
        i += 1 + offset;
    }

    (0, 0)
}

// ---------------------------------------------------------------------------
// GenericCRC (port of urh/util/GenericCRC.py)
// ---------------------------------------------------------------------------

pub struct CrcParams {
    pub polynomial_hex: &'static str,
    pub start_value: u64, // 0 or 1; 1 means "all ones" register
    pub final_xor: u64,  // 0 or 1; 1 means "all ones"
    pub ref_in: bool,
    pub ref_out: bool,
    pub little_endian: bool,
    pub reverse_polynomial: bool,
}

pub const STANDARD_CHECKSUMS: &[(&str, CrcParams)] = &[
    ("CRC8 (default)", CrcParams { polynomial_hex: "0xD5", start_value: 0, final_xor: 0, ref_in: false, ref_out: false, little_endian: false, reverse_polynomial: false }),
    ("CRC8 CCITT", CrcParams { polynomial_hex: "0x07", start_value: 0, final_xor: 0, ref_in: false, ref_out: false, little_endian: false, reverse_polynomial: false }),
    ("CRC8 Bluetooth", CrcParams { polynomial_hex: "0xA7", start_value: 0, final_xor: 0, ref_in: true, ref_out: true, little_endian: false, reverse_polynomial: false }),
    ("CRC8 DARC", CrcParams { polynomial_hex: "0x39", start_value: 0, final_xor: 0, ref_in: true, ref_out: true, little_endian: false, reverse_polynomial: false }),
    ("CRC8 NRSC-5", CrcParams { polynomial_hex: "0x31", start_value: 1, final_xor: 0, ref_in: false, ref_out: false, little_endian: false, reverse_polynomial: false }),
    ("CRC16 (default)", CrcParams { polynomial_hex: "0x8005", start_value: 0, final_xor: 0, ref_in: true, ref_out: true, little_endian: false, reverse_polynomial: false }),
    ("CRC16 CCITT", CrcParams { polynomial_hex: "0x1021", start_value: 0, final_xor: 0, ref_in: true, ref_out: true, little_endian: false, reverse_polynomial: false }),
    ("CRC16 NRSC-5", CrcParams { polynomial_hex: "0x080B", start_value: 1, final_xor: 0, ref_in: true, ref_out: true, little_endian: false, reverse_polynomial: false }),
    ("CRC16 CC1101", CrcParams { polynomial_hex: "0x8005", start_value: 1, final_xor: 0, ref_in: false, ref_out: false, little_endian: false, reverse_polynomial: false }),
    ("CRC16 CDMA2000", CrcParams { polynomial_hex: "0xC867", start_value: 1, final_xor: 0, ref_in: false, ref_out: false, little_endian: false, reverse_polynomial: false }),
    ("CRC32 (default)", CrcParams { polynomial_hex: "0x04C11DB7", start_value: 1, final_xor: 1, ref_in: true, ref_out: true, little_endian: false, reverse_polynomial: false }),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Polynomial {
    Standard8,
    Standard16,
    Ccitt16,
    Dnp16,
    Ccitt8,
}

pub const DEFAULT_POLYNOMIALS: &[(&str, &[Bit])] = &[
    // x^8 + x^7 + x^6 + x^4 + x^2 + 1
    ("8_standard", &[1, 1, 1, 0, 1, 0, 1, 0, 1]),
    // x^16+x^15+x^2+x^0
    (
        "16_standard",
        &[1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1],
    ),
    // x^16+x^12+x^5+x^0
    (
        "16_ccitt",
        &[1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1],
    ),
    // x^16+x^13+x^12+x^11+x^10+x^8+x^6+x^5+x^2+x^0
    (
        "16_dnp",
        &[1, 0, 0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 1],
    ),
    // x^8 + x^2 + x + 1
    ("8_ccitt", &[1, 0, 0, 0, 0, 0, 1, 1, 1]),
];

#[derive(Clone)]
pub struct GenericCRC {
    pub polynomial: Vec<Bit>,
    pub start_value: Vec<Bit>,
    pub final_xor: Vec<Bit>,
    pub reverse_polynomial: bool,
    pub reverse_all: bool,
    pub little_endian: bool,
    pub lsb_first: bool,
    cache: Vec<u64>,
    cache_bits: u32,
}

impl Default for GenericCRC {
    fn default() -> Self {
        GenericCRC::with_polynomial_name("16_standard")
    }
}

impl GenericCRC {
    pub fn new() -> Self {
        Self::default()
    }

    /// GenericCRC(polynomial="16_standard", start_value=False, final_xor=False, ...)
    pub fn with_polynomial_name(name: &str) -> Self {
        let poly = DEFAULT_POLYNOMIALS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, p)| p.to_vec())
            .unwrap_or_else(|| DEFAULT_POLYNOMIALS[1].1.to_vec());
        let n = poly.len() - 1;
        GenericCRC {
            polynomial: poly,
            start_value: vec![0; n],
            final_xor: vec![0; n],
            reverse_polynomial: false,
            reverse_all: false,
            little_endian: false,
            lsb_first: false,
            cache: Vec::new(),
            cache_bits: 8,
        }
    }

    pub fn from_polynomial_bits(polynomial: Vec<Bit>) -> Self {
        let n = polynomial.len() - 1;
        GenericCRC {
            polynomial,
            start_value: vec![0; n],
            final_xor: vec![0; n],
            reverse_polynomial: false,
            reverse_all: false,
            little_endian: false,
            lsb_first: false,
            cache: Vec::new(),
            cache_bits: 8,
        }
    }

    /// set_polynomial_from_hex
    pub fn set_polynomial_from_hex(&mut self, hex_str: &str) {
        let mut poly = vec![1u8];
        poly.extend(hex2bits(hex_str));
        if poly != self.polynomial {
            self.cache.clear();
            self.cache_bits = 8;
        }
        self.polynomial = poly;
    }

    /// Build from a hex polynomial plus flags (set_individual_parameters).
    /// `start_value`/`final_xor` of 1 mean an all-ones register, like Python.
    pub fn from_hex(
        polynomial_hex: &str,
        start_value: u64,
        final_xor: u64,
        ref_in: bool,
        ref_out: bool,
        little_endian: bool,
        reverse_polynomial: bool,
    ) -> Self {
        let mut c = GenericCRC::default();
        c.set_polynomial_from_hex(polynomial_hex);
        let n = c.polynomial.len() - 1;
        c.start_value = vec![start_value as Bit; n];
        c.final_xor = vec![final_xor as Bit; n];
        c.reverse_polynomial = reverse_polynomial;
        c.reverse_all = ref_out;
        c.little_endian = little_endian;
        c.lsb_first = ref_in;
        c
    }

    pub fn from_standard_checksum(name: &str) -> Option<Self> {
        let p = STANDARD_CHECKSUMS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, p)| p)?;
        Some(GenericCRC::from_hex(
            p.polynomial_hex,
            p.start_value,
            p.final_xor,
            p.ref_in,
            p.ref_out,
            p.little_endian,
            p.reverse_polynomial,
        ))
    }

    pub fn poly_order(&self) -> usize {
        self.polynomial.len()
    }

    pub fn polynomial_as_bit_str(&self) -> String {
        bits2str(&self.polynomial)
    }

    pub fn polynomial_as_hex_str(&self) -> String {
        bits2hex(&self.polynomial[1..]) // do not show leading one
    }

    /// Polynomial as "x^n + ... " style terms (polynomial_to_html without markup).
    pub fn polynomial_to_string(&self) -> String {
        let mut result = String::new();
        for i in 0..self.poly_order() {
            let index = self.poly_order() - 1 - i;
            if self.polynomial[i] > 0 {
                if index > 1 {
                    result.push_str(&format!("x^{index} + "));
                } else if index == 1 {
                    result.push_str("x + ");
                } else if index == 0 {
                    result.push('1');
                }
            }
        }
        result
    }

    pub fn calculate_cache(&mut self, bits: u32) {
        let bits = if 0 < bits && (bits as usize) < self.poly_order() {
            bits
        } else if self.poly_order() > 8 {
            8
        } else {
            (self.poly_order() - 1) as u32
        };
        self.cache_bits = bits;
        self.cache = calculate_cache(&self.polynomial, self.reverse_polynomial, bits);
    }

    /// CRC as bit array (number_to_bits with poly_order - 1 bits).
    pub fn crc(&self, inpt: &[Bit]) -> Vec<Bit> {
        let result = crc(
            inpt,
            &self.polynomial,
            &self.start_value,
            &self.final_xor,
            self.lsb_first,
            self.reverse_polynomial,
            self.reverse_all,
            self.little_endian,
        );
        number_to_bits(result, self.poly_order() - 1)
    }

    pub fn cached_crc(&mut self, inpt: &[Bit]) -> Vec<Bit> {
        if self.cache.is_empty() {
            self.calculate_cache(8);
        }
        let result = cached_crc(
            &self.cache,
            self.cache_bits,
            inpt,
            &self.polynomial,
            &self.start_value,
            &self.final_xor,
            self.lsb_first,
            self.reverse_polynomial,
            self.reverse_all,
            self.little_endian,
        );
        number_to_bits(result, self.poly_order() - 1)
    }

    /// Convenience: is the given checksum valid for these bits?
    pub fn is_crc_valid(&self, bits: &[Bit], checksum: &[Bit]) -> bool {
        self.crc(bits) == checksum
    }

    pub fn get_crc_datarange(&self, inpt: &[Bit], vrfy_crc_start: usize) -> (usize, usize) {
        get_crc_datarange(
            inpt,
            &self.polynomial,
            vrfy_crc_start,
            &self.start_value,
            &self.final_xor,
            self.lsb_first,
            self.reverse_polynomial,
            self.reverse_all,
            self.little_endian,
        )
    }

    /// Pure bit-level reference implementation (GenericCRC.reference_crc),
    /// useful for cross-checking the fast path.
    pub fn reference_crc(&self, inpt: &[Bit]) -> Vec<Bit> {
        let len_inpt = inpt.len();
        let poly_order = self.poly_order();
        if self.start_value.len() < poly_order - 1 {
            return Vec::new();
        }
        let mut crcv = self.start_value[0..poly_order - 1].to_vec();

        let mut i = 0usize;
        while i < len_inpt + 7 {
            for j in 0..8 {
                let idx = if self.lsb_first {
                    i + (7 - j)
                } else {
                    i + j
                };
                if idx >= len_inpt {
                    break;
                }
                if crcv[0] != inpt[idx] {
                    // crc = crc << 1, then xor polynomial
                    crcv.rotate_left(1);
                    crcv[poly_order - 2] = 0;
                    for x in 0..poly_order - 1 {
                        let p = if self.reverse_polynomial {
                            self.polynomial[poly_order - 1 - x]
                        } else {
                            self.polynomial[x + 1]
                        };
                        crcv[x] ^= p;
                    }
                } else {
                    crcv.rotate_left(1);
                    crcv[poly_order - 2] = 0;
                }
            }
            i += 8;
        }

        for i in 0..poly_order - 1 {
            if self.final_xor.get(i).copied().unwrap_or(0) != 0 {
                crcv[i] = if crcv[i] != 0 { 0 } else { 1 };
            }
        }

        if self.reverse_all {
            let old = crcv.clone();
            for i in 0..poly_order - 1 {
                crcv[i] = old[poly_order - 2 - i];
            }
        }

        if poly_order - 1 == 16 && self.little_endian {
            Self::swap_bytes(&mut crcv, 0, 1);
        } else if poly_order - 1 == 32 && self.little_endian {
            Self::swap_bytes(&mut crcv, 0, 3);
            Self::swap_bytes(&mut crcv, 1, 2);
        } else if poly_order - 1 == 64 && self.little_endian {
            for (p1, p2) in [(0, 7), (1, 6), (2, 5), (3, 4)] {
                Self::swap_bytes(&mut crcv, p1, p2);
            }
        }
        crcv
    }

    fn swap_bytes(arr: &mut [Bit], pos1: usize, pos2: usize) {
        for k in 0..8 {
            arr.swap(pos1 * 8 + k, pos2 * 8 + k);
        }
    }
}

// ---------------------------------------------------------------------------
// Encoding (port of urh/signalprocessing/Encoding.py)
// ---------------------------------------------------------------------------

pub const DECODING_INVERT: &str = "Invert";
pub const DECODING_DIFFERENTIAL: &str = "Differential Encoding";
pub const DECODING_REDUNDANCY: &str = "Remove Redundancy";
pub const DECODING_DATAWHITENING: &str = "Remove Data Whitening (CC1101)";
pub const DECODING_CARRIER: &str = "Remove Carrier";
pub const DECODING_BITORDER: &str = "Change Bitorder";
pub const DECODING_EDGE: &str = "Edge Trigger";
pub const DECODING_SUBSTITUTION: &str = "Substitution";
pub const DECODING_EXTERNAL: &str = "External Program";
pub const DECODING_ENOCEAN: &str = "Wireless Short Packet (WSP)";
pub const DECODING_CUT: &str = "Cut before/after";
pub const DECODING_MORSE: &str = "Morse Code";

/// Cut mark: either a bit pattern (cutmode 0/1) or a position (cutmode 2/3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutMark {
    Bits(Vec<Bit>),
    Pos(usize),
}

/// CC1101 data whitening (XOR whitening with LFSR keystream).
#[derive(Debug, Clone)]
pub struct DataWhitening {
    pub sync: Vec<Bit>,
    pub polynomial: Vec<Bit>,
    pub overwrite_crc: bool,
}

impl Default for DataWhitening {
    fn default() -> Self {
        // x^5+x^0 and "e9cae9ca" sync, as configured in Encoding.__init__
        DataWhitening {
            polynomial: vec![0, 0, 1, 0, 0, 0, 0, 1],
            sync: hex2bits("e9cae9ca"),
            overwrite_crc: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Codec {
    Invert,
    Differential,
    Redundancy { multiple: u32 },
    DataWhitening(DataWhitening),
    Carrier { carrier: String },
    /// Change Bitorder (LSB first <-> LSB last)
    LsbFirst,
    /// Edge Trigger
    Edge,
    Substitution {
        src: Vec<Vec<Bit>>,
        dst: Vec<Vec<Bit>>,
    },
    /// External programs are not supported by this port.
    External,
    /// mode: 0 = before, 1 = after, 2 = before_pos, 3 = after_pos
    Cut { mode: u8, mark: CutMark },
    Morse {
        low: u32,
        high: u32,
        wait: u32,
    },
    /// Wireless Short Packet (WSP / EnOcean)
    Enocean,
}

#[derive(Debug, Clone)]
#[derive(Default)]
pub struct Encoding {
    pub name: String,
    pub chain: Vec<Codec>,
}


type CodecResult = (Vec<Bit>, usize, ErrorState);

impl Encoding {
    pub fn new(name: &str) -> Self {
        Encoding {
            name: name.to_string(),
            chain: Vec::new(),
        }
    }

    pub fn is_nrz(&self) -> bool {
        self.chain.is_empty()
    }

    /// Parse a chain from settings-style names, mirroring Encoding.set_chain.
    pub fn from_chain_names(names: &[&str]) -> Self {
        let mut e = Encoding::new(names.first().copied().unwrap_or(""));
        if names.is_empty() {
            return e;
        }
        let mut i = 1;
        while i < names.len() {
            let n = names[i];
            if n.contains(DECODING_INVERT) {
                e.chain.push(Codec::Invert);
            } else if n.contains(DECODING_ENOCEAN) {
                e.chain.push(Codec::Enocean);
            } else if n.contains(DECODING_DIFFERENTIAL) {
                e.chain.push(Codec::Differential);
            } else if n.contains(DECODING_REDUNDANCY) {
                let param = names.get(i + 1).copied().unwrap_or("2");
                let multiple = param.trim().parse::<u32>().unwrap_or(2);
                e.chain.push(Codec::Redundancy { multiple });
                i += 1;
            } else if n.contains(DECODING_DATAWHITENING) {
                let param = names
                    .get(i + 1)
                    .copied()
                    .unwrap_or("0xe9cae9ca;0x21;0");
                e.chain.push(Codec::DataWhitening(
                    DataWhitening::from_param(param),
                ));
                i += 1;
            } else if n.contains(DECODING_CARRIER) {
                let param = names.get(i + 1).copied().unwrap_or("1_");
                e.chain.push(Codec::Carrier {
                    carrier: param.to_string(),
                });
                i += 1;
            } else if n.contains(DECODING_BITORDER) {
                e.chain.push(Codec::LsbFirst);
            } else if n.contains(DECODING_EDGE) {
                e.chain.push(Codec::Edge);
            } else if n.contains(DECODING_SUBSTITUTION) {
                let param = names.get(i + 1).copied().unwrap_or("0:1;1:0;");
                let (src, dst) = parse_subst_array(param);
                e.chain.push(Codec::Substitution { src, dst });
                i += 1;
            } else if n.contains(DECODING_EXTERNAL) {
                e.chain.push(Codec::External);
                i += 1;
            } else if n.contains(DECODING_CUT) {
                let param = names.get(i + 1).copied().unwrap_or("0;1010");
                e.chain.push(Codec::from_param(param));
                i += 1;
            } else if n.contains(DECODING_MORSE) {
                let param = names.get(i + 1).copied().unwrap_or("1;3;1");
                let (low, high, wait) = parse_morse_param(param);
                e.chain.push(Codec::Morse { low, high, wait });
                i += 1;
            }
            i += 1;
        }
        e
    }

    /// Run the whole chain. Returns Err for unsupported codecs (External).
    fn run(
        &self,
        decoding: bool,
        input: &[Bit],
    ) -> Result<(Vec<Bit>, usize, ErrorState), ErrorState> {
        let mut temp = input.to_vec();
        let mut errors = 0usize;
        let mut error_states: Vec<ErrorState> = Vec::new();

        let indices: Vec<usize> = if decoding {
            (0..self.chain.len()).collect()
        } else {
            (0..self.chain.len()).rev().collect()
        };

        for idx in indices {
            if temp.is_empty() {
                break;
            }
            let (out, temp_errors, state) = self.apply_codec(&self.chain[idx], decoding, &temp)?;
            errors += temp_errors;
            if state != ErrorState::Success && !error_states.contains(&state) {
                error_states.push(state);
            }
            temp = out;
        }

        let state = error_states.first().copied().unwrap_or(ErrorState::Success);
        Ok((temp, errors, state))
    }

    fn apply_codec(&self, codec: &Codec, decoding: bool, inpt: &[Bit]) -> Result<CodecResult, ErrorState> {
        Ok(match codec {
            Codec::Invert => code_invert(decoding, inpt),
            Codec::Differential => code_differential(decoding, inpt),
            Codec::Redundancy { multiple } => code_redundancy(decoding, *multiple, inpt),
            Codec::DataWhitening(dw) => dw.apply(decoding, inpt),
            Codec::Carrier { carrier } => code_carrier(decoding, carrier, inpt),
            Codec::LsbFirst => code_lsb_first(decoding, inpt),
            Codec::Edge => code_edge(decoding, inpt),
            Codec::Substitution { src, dst } => code_substitution(decoding, src, dst, inpt),
            Codec::External => return Err(ErrorState::MissingExternalProgram),
            Codec::Cut { mode, mark } => code_cut(decoding, *mode, mark, inpt),
            Codec::Morse { low, high, wait } => code_morse(decoding, *low, *high, *wait, inpt),
            Codec::Enocean => code_enocean(decoding, inpt),
        })
    }

    /// decode -> (bits, error state); Err only for unsupported codecs.
    pub fn decode(&self, bits: &[Bit]) -> Result<(Vec<Bit>, ErrorState), ErrorState> {
        let (out, _errors, state) = self.run(true, bits)?;
        Ok((out, state))
    }

    pub fn encode(&self, bits: &[Bit]) -> Result<Vec<Bit>, ErrorState> {
        let (out, _errors, _state) = self.run(false, bits)?;
        Ok(out)
    }

    /// number of bit errors and state (analyze)
    pub fn analyze(&self, bits: &[Bit]) -> Result<(usize, ErrorState), ErrorState> {
        let (_out, errors, state) = self.run(true, bits)?;
        Ok((errors, state))
    }

    pub fn applies_for_message(&self, msg: &[Bit]) -> bool {
        matches!(self.analyze(msg), Ok((0, ErrorState::Success)))
    }
}

// --- parameter parsing helpers ----------------------------------------------

fn parse_subst_array(string: &str) -> (Vec<Vec<Bit>>, Vec<Vec<Bit>>) {
    let mut src = Vec::new();
    let mut dst = Vec::new();
    for element in string.split(';') {
        if element.is_empty() {
            continue;
        }
        let mut parts = element.split(':');
        if let (Some(s), Some(d), None) = (parts.next(), parts.next(), parts.next()) {
            src.push(str2bits(s));
            dst.push(str2bits(d));
        }
    }
    (src, dst)
}

pub fn subst_array_to_string(src: &[Vec<Bit>], dst: &[Vec<Bit>]) -> String {
    let mut output = String::new();
    if src.len() == dst.len() {
        for i in 0..src.len() {
            output.push_str(&bits2str(&src[i]));
            output.push(':');
            output.push_str(&bits2str(&dst[i]));
            output.push(';');
        }
    }
    output
}

fn parse_morse_param(param: &str) -> (u32, u32, u32) {
    let parts: Vec<&str> = param.split(';').collect();
    if parts.len() == 3 {
        let l = parts[0].trim().parse().unwrap_or(1);
        let h = parts[1].trim().parse().unwrap_or(3);
        let w = parts[2].trim().parse().unwrap_or(1);
        (l, h, w)
    } else {
        (1, 3, 1)
    }
}

impl DataWhitening {
    /// Parse "sync;polynomial;overwrite" or "sync;polynomial" (hex values).
    pub fn from_param(param: &str) -> Self {
        let mut dw = DataWhitening::default();
        let parts: Vec<&str> = param.split(';').collect();
        if (parts.len() == 3 || parts.len() == 2)
            && !parts[0].is_empty() && !parts[1].is_empty() {
                dw.sync = hex2bits(parts[0]);
                dw.polynomial = hex2bits(parts[1]);
                dw.overwrite_crc = parts.len() == 3 && parts[2] == "1";
            }
        dw
    }
}

impl Codec {
    /// Parse "cutmode;cutmark".
    pub fn from_param(param: &str) -> Codec {
        let parts: Vec<&str> = param.split(';').collect();
        if parts.len() == 2 {
            let mut mode = parts[0].trim().parse::<u8>().unwrap_or(0);
            if mode > 3 {
                mode = 0;
            }
            let mark = if mode == 0 || mode == 1 {
                let mut bits = str2bits(parts[1]);
                if bits.is_empty() {
                    bits = vec![1, 0, 1, 0];
                }
                CutMark::Bits(bits)
            } else {
                CutMark::Pos(parts[1].trim().parse().unwrap_or(1))
            };
            Codec::Cut { mode, mark }
        } else {
            Codec::Cut {
                mode: 0,
                mark: CutMark::Bits(vec![1, 0, 1, 0]),
            }
        }
    }
}

// --- individual codecs -------------------------------------------------------

pub fn code_invert(_decoding: bool, inpt: &[Bit]) -> CodecResult {
    (
        inpt.iter().map(|&b| if b != 0 { 0 } else { 1 }).collect(),
        0,
        ErrorState::Success,
    )
}

pub fn code_differential(decoding: bool, inpt: &[Bit]) -> CodecResult {
    if inpt.is_empty() {
        return (Vec::new(), 0, ErrorState::Success);
    }
    let mut output = vec![inpt[0]];
    if decoding {
        for i in 1..inpt.len() {
            output.push(if inpt[i] != inpt[i - 1] { 1 } else { 0 });
        }
    } else {
        for i in 1..inpt.len() {
            if inpt[i] == 0 {
                output.push(output[i - 1]);
            } else {
                output.push(if output[i - 1] == 0 { 1 } else { 0 });
            }
        }
    }
    (output, 0, ErrorState::Success)
}

pub fn code_edge(decoding: bool, inpt: &[Bit]) -> CodecResult {
    let mut errors = 0;
    let mut output = Vec::new();
    if decoding {
        let mut i = 1;
        while i < inpt.len() {
            if inpt[i] == inpt[i - 1] {
                errors += 1;
                i += 1;
                continue;
            }
            output.push(inpt[i]);
            i += 2;
        }
    } else {
        for &b in inpt {
            if b == 0 {
                output.extend_from_slice(&[1, 0]);
            } else {
                output.extend_from_slice(&[0, 1]);
            }
        }
    }
    (output, errors, ErrorState::Success)
}

pub fn code_lsb_first(_decoding: bool, inpt: &[Bit]) -> CodecResult {
    let errors = inpt.len() % 8;
    let mut output = inpt.to_vec();
    let mut i = 0;
    while i + 7 < output.len() {
        output[i..i + 8].reverse();
        i += 8;
    }
    (output, errors, ErrorState::Success)
}

pub fn code_redundancy(decoding: bool, multiple: u32, inpt: &[Bit]) -> CodecResult {
    let mut output = Vec::new();
    let mut errors = 0;

    if !inpt.is_empty() && multiple > 1 {
        if decoding {
            let mut count = 0u32;
            let mut what: i8 = -1;
            for &b in inpt {
                if b != 0 {
                    if what == 0 {
                        if count > 0 {
                            errors += 1;
                        }
                        count = 0;
                    }
                    what = 1;
                    count += 1;
                    if count >= multiple {
                        output.push(1);
                        count = 0;
                    }
                } else {
                    if what == 1 {
                        if count > 0 {
                            errors += 1;
                        }
                        count = 0;
                    }
                    what = 0;
                    count += 1;
                    if count >= multiple {
                        output.push(0);
                        count = 0;
                    }
                }
            }
        } else {
            for &b in inpt {
                output.extend(std::iter::repeat_n(b, multiple as usize));
            }
        }
    }
    (output, errors, ErrorState::Success)
}

pub fn code_carrier(decoding: bool, carrier: &str, inpt: &[Bit]) -> CodecResult {
    let mut output = Vec::new();
    let mut errors = 0;
    let c = carrier.as_bytes();
    if c.is_empty() {
        return (output, errors, ErrorState::Success);
    }
    if decoding {
        for x in 0..inpt.len() {
            let tmp = c[x % c.len()];
            if tmp != b'0' && tmp != b'1' && tmp != b'*' {
                // Data!
                output.push(inpt[x]);
            } else {
                // Carrier -> 0, 1, *
                if tmp == b'0' || tmp == b'1' {
                    let carrier_bit = tmp == b'1';
                    if (inpt[x] != 0) != carrier_bit {
                        errors += 1;
                    }
                }
            }
        }
    } else {
        let mut x = 0usize;
        for &i in inpt {
            while matches!(c[x % c.len()], b'0' | b'1' | b'*') {
                // wildcard (*) becomes 0
                output.push(if c[x % c.len()] == b'1' { 1 } else { 0 });
                x += 1;
            }
            output.push(i);
            x += 1;
        }
        // Consume the trailing carrier pattern avoiding any wrap around
        while !x.is_multiple_of(c.len()) && matches!(c[x % c.len()], b'0' | b'1' | b'*') {
            output.push(if c[x % c.len()] == b'1' { 1 } else { 0 });
            x += 1;
        }
    }
    (output, errors, ErrorState::Success)
}

pub fn code_substitution(
    decoding: bool,
    src: &[Vec<Bit>],
    dst: &[Vec<Bit>],
    inpt: &[Bit],
) -> CodecResult {
    if src.is_empty() || dst.is_empty() {
        return (Vec::new(), 1, ErrorState::WrongInput);
    }

    // When encoding, swap directions
    let (src, dst) = if decoding { (src, dst) } else { (dst, src) };

    let mut padded_inpt = inpt.to_vec();
    // Padding of inpt with zeros to multiple of SRC[0] length
    let minimum_item_size = src[0].len();
    if minimum_item_size == 0 {
        return (Vec::new(), 42, ErrorState::WrongInput);
    }
    let zero_padding = (minimum_item_size - (padded_inpt.len() % minimum_item_size))
        % minimum_item_size;
    padded_inpt.extend(std::iter::repeat_n(0u8, zero_padding));
    let mut errors = zero_padding;

    let mut output = Vec::new();
    let mut i = 0usize;
    while i < padded_inpt.len() {
        if i + minimum_item_size > padded_inpt.len() {
            // Python would raise IndexError on the slice -> caught -> WRONG_INPUT
            return (Vec::new(), 42, ErrorState::WrongInput);
        }
        let window = &padded_inpt[i..i + minimum_item_size];
        let matches: Vec<usize> = src
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_slice() == window)
            .map(|(k, _)| k)
            .collect();
        if matches.len() == 1 {
            output.extend_from_slice(&dst[matches[0]]);
            i += minimum_item_size;
        } else if matches.is_empty() {
            output.extend_from_slice(&padded_inpt[i..i + 1]);
            i += 1;
            errors += 1;
        } else {
            // ambiguous: no output, skip (faithful to the reference)
            i += minimum_item_size;
        }
    }
    (output, errors, ErrorState::Success)
}

pub fn code_cut(decoding: bool, cutmode: u8, cutmark: &CutMark, inpt: &[Bit]) -> CodecResult {
    let mut output = Vec::new();
    let state = ErrorState::Success;

    // cutmode: 0 = before, 1 = after, 2 = before_pos, 3 = after_pos
    if decoding {
        let mut pos: i64 = -1;
        if cutmode == 0 || cutmode == 1 {
            let mark = match cutmark {
                CutMark::Bits(b) => b,
                CutMark::Pos(_) => return (inpt.to_vec(), 0, ErrorState::Success),
            };
            if mark.is_empty() {
                return (inpt.to_vec(), 0, ErrorState::InvalidCutmark);
            }
            // faithful to the reference: range excludes the last possible position
            'outer: for i in 0..inpt.len().saturating_sub(mark.len()) {
                for j in 0..mark.len() {
                    if inpt[i + j] != mark[j] {
                        continue 'outer;
                    }
                }
                pos = i as i64;
                break;
            }
        } else {
            pos = match cutmark {
                CutMark::Bits(_) => return (inpt.to_vec(), 0, ErrorState::Success),
                CutMark::Pos(p) => *p as i64,
            };
        }

        if (0..inpt.len() as i64).contains(&pos) {
            let pos = pos as usize;
            if cutmode == 0 || cutmode == 2 {
                // Delete before
                output.extend_from_slice(&inpt[pos..]);
            } else {
                // Delete after
                let pos = if cutmode == 1 {
                    match cutmark {
                        CutMark::Bits(b) => pos + b.len(),
                        CutMark::Pos(_) => pos + 1,
                    }
                } else {
                    pos + 1
                };
                output.extend_from_slice(&inpt[..pos]);
            }
        } else {
            // Position not found or not in range, do nothing!
            output.extend_from_slice(inpt);
            return (output, 0, ErrorState::PreambleNotFound);
        }
    } else {
        // Can't undo removing information :-(
        output.extend_from_slice(inpt);
    }
    (output, 0, state)
}

pub fn code_morse(decoding: bool, morse_low: u32, morse_high: u32, morse_wait: u32, inpt: &[Bit]) -> CodecResult {
    let mut errors = 0;
    let mut output = Vec::new();

    if morse_low >= morse_high {
        return (inpt.to_vec(), 1, ErrorState::WrongParameters);
    }

    if decoding {
        let mut cnt = 0u32;
        let mut i = 0usize;
        while i < inpt.len() {
            if inpt[i] != 0 && i < inpt.len() - 1 {
                cnt += 1;
            } else {
                // Consider last value
                if i == inpt.len() - 1 && inpt[inpt.len() - 1] != 0 {
                    cnt += 1;
                }

                // Evaluate sequence whenever we get a zero
                if cnt >= morse_high {
                    output.push(1);
                } else if cnt > 0 && cnt <= morse_low {
                    output.push(0);
                } else if cnt > 0 {
                    if cnt > (morse_high + morse_low / 2) {
                        output.push(1);
                    } else {
                        output.push(0);
                    }
                    errors += 1;
                }
                cnt = 0;
            }
            i += 1;
        }
    } else {
        let mut i = 0usize;
        while i < inpt.len() {
            output.extend(std::iter::repeat_n(0u8, morse_wait as usize));
            if inpt[i] != 0 {
                output.extend(std::iter::repeat_n(1u8, morse_high as usize));
            } else {
                output.extend(std::iter::repeat_n(1u8, morse_low as usize));
            }
            i += 1;
        }
        output.extend(std::iter::repeat_n(0u8, morse_wait as usize));
    }

    (output, errors, ErrorState::Success)
}

pub fn code_externalprogram(_decoding: bool, _inpt: &[Bit]) -> CodecResult {
    // External programs are intentionally not supported in this port.
    (Vec::new(), 1, ErrorState::MissingExternalProgram)
}

// --- data whitening ----------------------------------------------------------

impl DataWhitening {
    /// Encoding.lfsr — advance the LFSR `clock` times, return state[1..len_pol].
    fn lfsr(poly: &[Bit], state: &mut Vec<Bit>, clock: u32) -> Vec<Bit> {
        let mut poly_full = vec![0u8];
        poly_full.extend_from_slice(poly);
        let len_pol = poly_full.len();

        if state.is_empty() {
            state.extend(std::iter::repeat_n(1u8, len_pol));
        }
        for _ in 0..clock {
            // Determine first bit with polynomial (XOR of all poly&state taps)
            let mut first_bit = false;
            for j in (0..len_pol).rev() {
                if poly_full[j] != 0 && state[j] != 0 {
                    first_bit = !first_bit;
                }
            }
            // Clock
            for j in (1..len_pol).rev() {
                state[j] = state[j - 1];
            }
            state[0] = if first_bit { 1 } else { 0 };
        }
        state[1..len_pol].to_vec()
    }

    /// Encoding.apply_data_whitening
    pub fn apply(&self, decoding: bool, inpt: &[Bit]) -> CodecResult {
        let len_sync = self.sync.len();
        let len_polynomial = self.polynomial.len();
        let mut inpt = inpt.to_vec();
        let mut inpt_to = inpt.len();

        // Crop last bit, if duplicate
        if decoding && inpt_to > 1 && inpt[inpt_to - 1] == inpt[inpt_to - 2] {
            inpt_to -= 1;
        }

        if inpt_to < 1 || len_polynomial < 1 || len_sync < 1 {
            return (inpt[..inpt_to].to_vec(), 0, ErrorState::Misc);
        }

        // Search for whitening start position (after sync bytes)
        let mut whitening_start_pos = 0usize;
        let mut i = 0usize;
        while i + len_sync < inpt_to {
            let equalbits = (0..len_sync)
                .filter(|&j| inpt[i + j] == self.sync[j])
                .count();
            if equalbits == len_sync {
                whitening_start_pos = i + len_sync;
                break;
            }
            i += 1;
        }
        if decoding && whitening_start_pos == 0 {
            return (inpt[..inpt_to].to_vec(), 0, ErrorState::SyncNotFound);
        }

        // Prepare keystream
        let mut state: Vec<Bit> = Vec::new();
        let mut keystream = Self::lfsr(&self.polynomial, &mut state, 0);
        let mut p = whitening_start_pos;
        while p < inpt_to {
            keystream.extend(Self::lfsr(&self.polynomial, &mut state, 8));
            p += 8;
        }

        // If data whitening polynomial is wrong, keystream can be less than needed
        if keystream.len() < inpt_to - whitening_start_pos {
            return (inpt[..inpt_to].to_vec(), 0, ErrorState::Misc);
        }

        // Overwrite crc16 in encoding case
        if !decoding && self.overwrite_crc {
            let offset = inpt_to % 8;
            let data_end = inpt_to - 16 - offset;
            let mut c = GenericCRC::with_polynomial_name("16_standard");
            let n = c.poly_order() - 1;
            c.start_value = vec![1; n];
            let crcv = c.crc(&inpt[whitening_start_pos..data_end]);
            for k in 0..16 {
                inpt[data_end + k] = crcv[k];
            }
        }

        // Apply keystream (xor)
        for k in whitening_start_pos..inpt_to {
            inpt[k] ^= keystream[k - whitening_start_pos];
        }

        // Duplicate last bit when encoding
        if !decoding {
            let last = inpt[inpt.len() - 1];
            inpt.push(last);
            inpt_to += 1;
        }

        (inpt[..inpt_to].to_vec(), 0, ErrorState::Success)
    }
}

// --- EnOcean / WSP -------------------------------------------------------------

pub fn code_enocean(decoding: bool, inpt: &[Bit]) -> CodecResult {
    let mut errors = 0;
    let mut output: Vec<Bit> = Vec::new();
    let preamble: [Bit; 8] = [1, 0, 1, 0, 1, 0, 1, 0];
    let sof: [Bit; 4] = [1, 0, 0, 1];
    let eof: [Bit; 4] = [1, 0, 1, 1];

    let mut inpt = inpt.to_vec();
    if decoding {
        let (inv, _, _) = code_invert(true, &inpt);
        inpt = inv;
        // Insert a leading 1 to ensure the protocol starts with 1
        inpt.insert(0, 1);
        // If the signal has zero noise, the last two zeros get swallowed
        inpt.extend_from_slice(&[1, 1]);
    }

    // search for begin
    let first_zero = match inpt.iter().position(|&b| b == 0) {
        Some(p) if p > 0 => p - 1,
        _ => return (inpt, 0, ErrorState::PreambleNotFound),
    };
    let n = first_zero;

    let check = |arr: &[Bit], off: usize, pat: &[Bit]| -> bool {
        arr.get(off..off + pat.len())
            .map(|w| w == pat)
            .unwrap_or(false)
    };

    // check preamble
    if !check(&inpt, n, &preamble) {
        return (inpt, 0, ErrorState::PreambleNotFound);
    }
    // check SoF
    if !check(&inpt, n + 8, &sof) {
        return (inpt, 0, ErrorState::SyncNotFound);
    }

    output.extend_from_slice(&inpt[n..n + 12]);

    // search for data limits
    let start = n + 12;
    let mut n2 = inpt.len();
    while n2 > start && !check(&inpt, n2 - 4, &eof) {
        n2 -= 1;
    }
    let end = n2 - 4;

    let state = ErrorState::Success;

    if decoding {
        let mut n = start;
        while n < end {
            // Python catches IndexError here -> MISC
            if n + 11 >= inpt.len() {
                return (inpt, 0, ErrorState::Misc);
            }
            errors += (inpt[n + 2] == inpt[n + 3]) as usize;
            errors += (inpt[n + 6] == inpt[n + 7]) as usize;
            if end >= 11 && n < end - 11 {
                errors += (inpt[n + 10] != 0) as usize;
                errors += (inpt[n + 11] != 1) as usize;
            }
            output.extend_from_slice(&[
                inpt[n],
                inpt[n + 1],
                inpt[n + 2],
                inpt[n + 4],
                inpt[n + 5],
                inpt[n + 6],
                inpt[n + 8],
                inpt[n + 9],
            ]);
            n += 12;
        }

        // Finalize output
        if end + 4 <= inpt.len() {
            output.extend_from_slice(&inpt[end..end + 4]);
        }
    } else {
        let mut n = start;
        while n < end {
            if n + 7 >= inpt.len() {
                output.extend_from_slice(&[0, 1]);
                break;
            }
            output.extend_from_slice(&[
                inpt[n],
                inpt[n + 1],
                inpt[n + 2],
                (1 - inpt[n + 2]),
                inpt[n + 3],
                inpt[n + 4],
                inpt[n + 5],
                (1 - inpt[n + 5]),
                inpt[n + 6],
                inpt[n + 7],
            ]);

            if n < inpt.len().saturating_sub(15) {
                output.extend_from_slice(&[0, 1]);
            }
            n += 8;
        }

        // Extend eof and trash
        output.extend_from_slice(&eof);
        output.push(1);

        // Invert
        let (inv, _, _) = code_invert(true, &output);
        output = inv;
    }

    (output, errors, state)
}

// ---------------------------------------------------------------------------
// tests (ported from tests/test_CRC.py and tests/test_encoding.py)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- CRC ---------------------------------------------------------------

    #[test]
    fn test_crc_wsp() {
        // test_CRC.py::test_crc
        let poly = vec![1, 0, 0, 0, 0, 0, 1, 1, 1]; // WSPChecksum.CRC_8_POLYNOMIAL
        let c = GenericCRC::from_polynomial_bits(poly);
        let bitstr = [
            "010101010110100111011010111011101110111011100110001011101010001011101110110110101101",
            "010101010110101001101110111011101110111011100110001011101010001011101110110111100101",
            "010101010110100111010010111011101110111011100110001011101010001011101110110110100101",
        ];
        let expected = ["78", "c9", "f2"];
        for (value, expect) in bitstr.iter().zip(expected.iter()) {
            // value[4:-8]
            let bits = str2bits(&value[4..value.len() - 8]);
            assert_eq!(bits2hex(&c.crc(&bits)), *expect);
        }
    }

    #[test]
    fn test_crc8() {
        // test_CRC.py::test_crc8
        let messages = ["aabbcc", "abcdee", "dacafe"];
        let expected = ["7d", "24", "33"];
        let crc = GenericCRC::from_polynomial_bits(DEFAULT_POLYNOMIALS[4].1.to_vec()); // 8_ccitt
        for (msg, expect) in messages.iter().zip(expected.iter()) {
            let bits = hex2bits(msg);
            assert_eq!(bits2hex(&crc.crc(&bits)), *expect);
        }
    }

    #[test]
    fn test_crc16_ccitt_known_vectors() {
        let bits = hex2bits("313233343536373839"); // "123456789"
        // CRC-16/XMODEM (0x1021, init 0, no reflection) = 0x31C3
        let mut c = GenericCRC::default();
        c.set_polynomial_from_hex("0x1021");
        assert_eq!(bits2hex(&c.crc(&bits)), "31c3");
        assert!(c.is_crc_valid(&bits, &hex2bits("31c3")));
        assert!(!c.is_crc_valid(&bits, &hex2bits("31c2")));

        // CRC-16/CCITT-FALSE (init 0xFFFF) = 0x29B1
        c.start_value = vec![1; 16];
        assert_eq!(bits2hex(&c.crc(&bits)), "29b1");
    }

    #[test]
    fn test_crc16_standard_known_vector() {
        // CRC-16/ARC? no: plain 0x8005, no reflection, init 0 -> "B1 53"? Actually
        // CRC-16/UMTS (0x8005, no reflect) of "123456789" = 0x fee8? We use the
        // value computed by the reference algorithm instead of an external one.
        let c = GenericCRC::with_polynomial_name("16_standard");
        let bits = hex2bits("313233343536373839");
        let fast = c.crc(&bits);
        let reference = c.reference_crc(&bits);
        assert_eq!(fast, reference);
    }

    #[test]
    fn test_different_crcs_against_reference() {
        // test_CRC.py::test_different_crcs (subset)
        let bitstring_set = [
            "101001001010101010101011101111111000000000000111101010011101011",
            "101001001010101101111010110111101010010110111010",
            "00000000000000000000000000000000100000000000000000000000000000000001111111111111",
        ];
        for (_, poly) in DEFAULT_POLYNOMIALS {
            for i in bitstring_set.iter() {
                let mut c = GenericCRC::from_polynomial_bits(poly.to_vec());
                let bits = str2bits(i);

                // Standard
                assert_eq!(c.crc(&bits), c.reference_crc(&bits));

                // Special final xor
                c.final_xor = str2bits("0000111100001111");
                assert_eq!(c.crc(&bits), c.reference_crc(&bits));
                c.final_xor = vec![0; 16];

                // Special start value
                c.start_value = str2bits("1010101010101010");
                assert_eq!(c.crc(&bits), c.reference_crc(&bits));
                c.start_value = vec![0; 16];

                // reverse_polynomial
                c.reverse_polynomial = true;
                assert_eq!(c.crc(&bits), c.reference_crc(&bits));
                c.reverse_polynomial = false;

                // lsb_first
                c.lsb_first = true;
                assert_eq!(c.crc(&bits), c.reference_crc(&bits));
                c.lsb_first = false;

                // little_endian
                c.little_endian = true;
                assert_eq!(c.crc(&bits), c.reference_crc(&bits));
                c.little_endian = false;

                // reverse all
                c.reverse_all = true;
                assert_eq!(c.crc(&bits), c.reference_crc(&bits));
                c.reverse_all = false;
            }
        }
    }

    #[test]
    fn test_cache() {
        // test_CRC.py::test_cache
        let mut c = GenericCRC::with_polynomial_name("16_standard");
        c.calculate_cache(8);
        assert_eq!(c.cache.len(), 256);
    }

    #[test]
    fn test_different_crcs_fast() {
        // test_CRC.py::test_different_crcs_fast (subset of cache sizes/flags)
        let bitstring_set = [
            "10101010",
            "00000001",
            "000000010",
            "000000011",
            "0000000100000001",
            "101001001010101010101011101111111000000000000111101010011101011",
        ];
        for (_, poly) in DEFAULT_POLYNOMIALS {
            for i in bitstring_set.iter() {
                for cache_bits in [8u32, 4, 7, 12, 16] {
                    let mut c = GenericCRC::from_polynomial_bits(poly.to_vec());
                    c.calculate_cache(cache_bits);
                    let bits = str2bits(i);

                    // Standard
                    assert_eq!(c.cached_crc(&bits), c.reference_crc(&bits));

                    // Special final xor
                    c.final_xor = str2bits("0000111100001111");
                    assert_eq!(c.cached_crc(&bits), c.reference_crc(&bits));
                    c.final_xor = vec![0; 16];

                    // Special start value
                    c.start_value = str2bits("1010101010101010");
                    assert_eq!(c.cached_crc(&bits), c.reference_crc(&bits));
                    c.start_value = vec![0; 16];

                    // little_endian
                    c.little_endian = true;
                    assert_eq!(c.cached_crc(&bits), c.reference_crc(&bits));
                    c.little_endian = false;

                    // reverse all
                    c.reverse_all = true;
                    assert_eq!(c.cached_crc(&bits), c.reference_crc(&bits));
                    c.reverse_all = false;

                    // reverse_polynomial (cache must be rebuilt)
                    c.reverse_polynomial = true;
                    c.cache.clear();
                    c.calculate_cache(cache_bits);
                    assert_eq!(c.cached_crc(&bits), c.reference_crc(&bits));
                    c.reverse_polynomial = false;
                    c.cache.clear();

                    // lsb_first (only works for cache size 8, like the reference)
                    c.calculate_cache(8);
                    c.lsb_first = true;
                    assert_eq!(c.cached_crc(&bits), c.reference_crc(&bits));
                    c.lsb_first = false;
                }
            }
        }
    }

    #[test]
    fn test_not_aligned_data_len() {
        // test_CRC.py::test_not_aligned_data_len
        let crcs = [
            ("8_standard", 0xD5u64),
            ("16_standard", 0x8005),
            ("16_ccitt", 0x1021),
            ("16_dnp", 0x3D65),
        ];
        for (name, expected) in crcs {
            let c = GenericCRC::with_polynomial_name(name);
            let mut inpt = String::from("1");
            for _ in 0..32 {
                let val = bits2number(&c.crc(&str2bits(&inpt)));
                assert_eq!(val, expected);
                inpt = format!("0{inpt}");
            }
        }
    }

    #[test]
    fn test_standard_checksums() {
        // known vectors for a few presets
        let c = GenericCRC::from_standard_checksum("CRC8 CCITT").unwrap();
        // CRC-8 (plain 0x07) of "123456789" = 0xF4
        assert_eq!(bits2hex(&c.crc(&hex2bits("313233343536373839"))), "f4");

        let c = GenericCRC::from_standard_checksum("CRC16 CC1101").unwrap();
        // CRC-16/MAXIM? no: 0x8005, init 0xFFFF, no reflect -> CRC-16/UMTS init FFFF?
        // compare against the bit-by-bit implementation instead
        let bits = hex2bits("313233343536373839");
        assert_eq!(
            bits2number(&c.crc(&bits)),
            crc(&bits, &c.polynomial, &c.start_value, &c.final_xor, c.lsb_first,
                c.reverse_polynomial, c.reverse_all, c.little_endian)
        );
        assert_eq!(c.polynomial_as_hex_str(), "8005");

        // CRC-32/BZIP2 style (0x04C11DB7, init/xorout all-ones, reflected)
        let c = GenericCRC::from_standard_checksum("CRC32 (default)").unwrap();
        let fast = c.crc(&hex2bits("313233343536373839"));
        assert_eq!(fast, c.reference_crc(&hex2bits("313233343536373839")));
    }

    #[test]
    fn test_get_crc_datarange() {
        // small input: data + crc, expect (0, data_len)
        let c = GenericCRC::with_polynomial_name("16_standard");
        let data = str2bits("10100100101010101010");
        let checksum = c.crc(&data);
        let mut inpt = data.clone();
        inpt.extend_from_slice(&checksum);
        let (begin, end) = c.get_crc_datarange(&inpt, data.len());
        assert_eq!((begin, end), (0, data.len()));
    }

    #[test]
    fn test_get_crc_datarange_bruteforce_vector() {
        // test_CRC.py::test_bruteforce_parameters_and_data_range
        // parameters i=2: polynomial 16_ccitt, start 0, xor 0, no flags
        let c = GenericCRC::with_polynomial_name("16_ccitt");
        let inpt = "101010101010101010000000111000000000000011100000001011010010110100000000111000000101001010000100000000000100111001111110010000000011011111111001001101100001100010100000000000111011110100010";
        let vrfy_crc = "0011101111010001";
        let bits = str2bits(inpt);
        let vrfy_start = inpt.len() - vrfy_crc.len() - 1;
        let result = c.get_crc_datarange(&bits, vrfy_start);
        assert_eq!(result, (84, 172));
        assert_eq!(bits2str(&c.crc(&bits[84..172])), vrfy_crc);
    }

    #[test]
    fn test_adaptive_crc_calculation() {
        // test_CRC.py::test_adaptive_crc_calculation
        let mut c = GenericCRC::with_polynomial_name("16_ccitt");
        let inpt1 = "10101010101010";
        let inpt2 = "1010101010101001";

        let crc1 = c.crc(&str2bits(inpt1));
        let crc2 = c.crc(&str2bits(inpt2));

        let delta = "01";
        c.start_value = crc1;
        let crcx = c.crc(&str2bits(delta));
        assert_eq!(crcx, crc2);
    }

    #[test]
    fn test_polynomial_helpers() {
        let c = GenericCRC::with_polynomial_name("16_standard");
        assert_eq!(c.polynomial_as_bit_str(), "11000000000000101");
        assert_eq!(c.polynomial_as_hex_str(), "8005");
        assert_eq!(c.poly_order(), 17);
        assert_eq!(
            GenericCRC::with_polynomial_name("16_ccitt").polynomial_to_string(),
            "x^16 + x^12 + x^5 + 1"
        );
    }

    // ---- Encoding ----------------------------------------------------------

    #[test]
    fn test_invert() {
        let e = Encoding::from_chain_names(&["test invert", DECODING_INVERT]);
        let data = str2bits("1010100110");
        let encoded = e.encode(&data).unwrap();
        assert_eq!(bits2str(&encoded), "0101011001");
        assert_eq!(e.decode(&encoded).unwrap().0, data);
    }

    #[test]
    fn test_differential() {
        // known vector
        let (enc, _, _) = code_differential(false, &str2bits("1010"));
        assert_eq!(bits2str(&enc), "1100");
        let (dec, _, _) = code_differential(true, &enc);
        assert_eq!(bits2str(&dec), "1010");

        let e = Encoding::from_chain_names(&["test diff", DECODING_DIFFERENTIAL]);
        let data = str2bits("1100100101");
        let encoded = e.encode(&data).unwrap();
        assert_eq!(e.decode(&encoded).unwrap().0, data);
    }

    #[test]
    fn test_edge() {
        // Manchester II: 0 -> 10, 1 -> 01
        let (enc, err, _) = code_edge(false, &str2bits("0110"));
        assert_eq!((bits2str(&enc).as_str(), err), ("10010110", 0));
        let (dec, err, _) = code_edge(true, &enc);
        assert_eq!((bits2str(&dec).as_str(), err), ("0110", 0));

        let e = Encoding::from_chain_names(&["test edge", DECODING_EDGE]);
        let data = str2bits("0100101111");
        let encoded = e.encode(&data).unwrap();
        assert_eq!(e.decode(&encoded).unwrap().0, data);
    }

    #[test]
    fn test_lsb_first() {
        let e = Encoding::from_chain_names(&["test bitorder", DECODING_BITORDER]);
        let data = str2bits("00010001");
        let encoded = e.encode(&data).unwrap();
        assert_eq!(bits2str(&encoded), "10001000");
        assert_eq!(e.decode(&encoded).unwrap().0, data);
    }

    #[test]
    fn test_redundancy() {
        let e = Encoding::from_chain_names(&["test redundancy", DECODING_REDUNDANCY, "3"]);
        let data = str2bits("10110");
        let encoded = e.encode(&data).unwrap();
        assert_eq!(bits2str(&encoded), "111000111111000");
        let (dec, state) = e.decode(&encoded).unwrap();
        assert_eq!((bits2str(&dec).as_str(), state), ("10110", ErrorState::Success));
    }

    #[test]
    fn test_carrier() {
        // test_encoding.py::test_carrier
        let carrier = "----1....1**";
        let original_inpt = str2bits("000010000100111111111100");
        let (output, _, _) = code_carrier(true, carrier, &original_inpt);
        let (newinpt, _, _) = code_carrier(false, carrier, &output);
        assert_eq!(original_inpt, newinpt);

        // default carrier "1_"
        let e = Encoding::from_chain_names(&["test carrier", DECODING_CARRIER, "1_"]);
        let data = str2bits("0110");
        let encoded = e.encode(&data).unwrap();
        assert_eq!(bits2str(&encoded), "10111110");
        assert_eq!(e.decode(&encoded).unwrap().0, data);
    }

    #[test]
    fn test_cut_decoding() {
        // test_encoding.py::test_cut_decoding
        let received = str2bits("00001010010101111111000");

        let (decoded, _, _) = code_cut(true, 0, &CutMark::Bits(str2bits("1010")), &received);
        assert_eq!(decoded, str2bits("1010010101111111000"));

        let (decoded, _, _) = code_cut(true, 1, &CutMark::Bits(str2bits("1010")), &received);
        assert_eq!(decoded, str2bits("00001010"));

        let (decoded, _, _) = code_cut(true, 2, &CutMark::Pos(2), &received);
        assert_eq!(decoded, str2bits("001010010101111111000"));

        let (decoded, _, _) = code_cut(true, 3, &CutMark::Pos(2), &received);
        assert_eq!(decoded, str2bits("000"));

        // cutmark not found -> unchanged + PREAMBLE_NOT_FOUND
        let (decoded, _, state) =
            code_cut(true, 0, &CutMark::Bits(str2bits("101010")), &received);
        assert_eq!(decoded, received);
        assert_eq!(state, ErrorState::PreambleNotFound);

        // long vector
        let received = str2bits("11010101010101010101010101010101011101001110010101110100111001010000100011011111110000100001011100101010101111101111010000001110101111010000100000110100000010011011110101100100011110000100011011100010100111100000000000000000");
        let expected = str2bits("1010101010101010101010101010101011101001110010101110100111001010000100011011111110000100001011100101010101111101111010000001110101111010000100000110100000010011011110101100100011110000100011011100010100111100000000000000000");
        let (decoded, _, _) = code_cut(
            true,
            0,
            &CutMark::Bits(str2bits("10101010101010101010101010101010")),
            &received,
        );
        assert_eq!(decoded, expected);
    }

    #[test]
    fn test_morse() {
        // test_encoding.py::test_morse
        let msg1 = "1111111000111100011111100100001111111111111111111111011";
        let msg2 = "0111110111011111011101111101110";

        let encoded = str2bits(msg1);
        let compare = str2bits(msg2);
        let (decoded, err, _) = code_morse(true, 3, 5, 1, &encoded);
        assert_eq!(err, 1);
        let (reencoded, _, _) = code_morse(false, 3, 5, 1, &decoded);
        assert_eq!(reencoded, compare);
    }

    #[test]
    fn test_substitution() {
        // test_encoding.py::test_substitution
        let src = vec![str2bits("1110"), str2bits("1000")];
        let dst = vec![str2bits("1"), str2bits("0")];

        // encoded-string with 3 missing trailing zeroes
        let encoded = str2bits("1000111010001110111011101110111011101110100011101110111011101110111011101000100010001000100010001");
        let compare = str2bits("1000111010001110111011101110111011101110100011101110111011101110111011101000100010001000100010001000");
        let (decoded, err, _) = code_substitution(true, &src, &dst, &encoded);
        assert_eq!(err, 3);
        let (reencoded, _, _) = code_substitution(false, &src, &dst, &decoded);
        assert_eq!(reencoded, compare);
    }

    #[test]
    fn test_data_whitening() {
        // test_encoding.py::test_data_whitening
        let nrz1 = str2bits("101010101010101010101010101010101110100111001010111010011100101011110011101011001001010011101110100011001011100111100111101011111110011100101001111111110011000111010000010111010101011100");
        let nrz2 = str2bits("101010101010101010101010101010101110100111001010111010011100101011110001101011001011010000011101101101011101101110110011010010011010001010010010000101111001100111000100001001111110000000001000000010011");

        let dw = DataWhitening::default();
        let (de_whitened1, _, state) = dw.apply(true, &nrz1);
        assert_eq!(state, ErrorState::Success);
        let (de_whitened2, _, _) = dw.apply(true, &nrz2);

        let dw = DataWhitening {
            overwrite_crc: false,
            ..DataWhitening::default()
        };
        let (nrz1_, _, _) = dw.apply(false, &de_whitened1);
        let (nrz2_, _, _) = dw.apply(false, &de_whitened2);
        assert_eq!(nrz1, nrz1_);
        assert_eq!(nrz2, nrz2_);

        let dw = DataWhitening {
            overwrite_crc: true,
            ..DataWhitening::default()
        };
        let (nrz1__, _, _) = dw.apply(false, &de_whitened1);
        let (nrz2__, _, _) = dw.apply(false, &de_whitened2);
        assert_eq!(nrz1, nrz1__);
        assert_eq!(nrz2, nrz2__);
    }

    #[test]
    fn test_enocean_switch_telegram() {
        // test_encoding.py::test_enocean_switch_telegram
        let received = "010101010110100111101010111011101110111011100110001011101010001011101110110111011101000";
        let preamble = "10101010";
        let sof = "1001";
        let eof = "1011";

        let expected_result = preamble.to_string()
            + sof
            + "011000010000000000000010110000011100000000100100"
            + eof;
        let expected_result2 = preamble.to_string()
            + sof
            + "010100000000000000000010110000011100000000100010"
            + eof;

        let (decoded, err, state) = code_enocean(true, &str2bits(received));
        assert_eq!((err, state), (0, ErrorState::Success));
        let (reencoded, err, state) = code_enocean(false, &decoded);
        assert_eq!((err, state), (0, ErrorState::Success));

        assert_eq!(bits2str(&decoded), expected_result);
        assert_eq!(bits2str(&reencoded), received);
        assert_ne!(bits2str(&decoded), expected_result2);
    }

    #[test]
    fn test_enocean_crc8_message() {
        // test_encoding.py::test_enocean_crc8_message
        let received = hex2bits("aacbac4cddd5ddd3bddd5ddcc5ddcddd4c2d5d5c2cdddab200000");

        let (decoded, err, state) = code_enocean(true, &received);
        assert_eq!((err, state), (0, ErrorState::Success));
        let hex = bits2hex(&decoded);
        assert!(hex.contains("aa"));
        assert!(hex.contains('9'));
        assert!(hex.contains('b'));

        let (reencoded, errors, state) = code_enocean(false, &decoded);
        assert_eq!((errors, state), (0, ErrorState::Success));

        let (redecoded, errors, state) = code_enocean(true, &reencoded);
        assert_eq!((errors, state), (0, ErrorState::Success));
        assert_eq!(decoded, redecoded);
    }

    #[test]
    fn test_external_unsupported() {
        let e = Encoding::from_chain_names(&["test external", DECODING_EXTERNAL, "./;./"]);
        assert_eq!(
            e.decode(&str2bits("1010")),
            Err(ErrorState::MissingExternalProgram)
        );
        assert_eq!(
            e.encode(&str2bits("1010")),
            Err(ErrorState::MissingExternalProgram)
        );
    }

    #[test]
    fn test_chain_decode_encode() {
        // combined chain roundtrip (differential + invert)
        let e = Encoding::from_chain_names(&["combined", DECODING_DIFFERENTIAL, DECODING_INVERT]);
        let data = str2bits("10110010100111001010011010101110");
        let encoded = e.encode(&data).unwrap();
        let (decoded, state) = e.decode(&encoded).unwrap();
        assert_eq!(state, ErrorState::Success);
        assert_eq!(decoded, data);
        assert!(!e.is_nrz());
        assert!(e.applies_for_message(&encoded));

        // data whitening in a chain: input must start with the sync bytes
        let e = Encoding::from_chain_names(&[
            "whitened",
            DECODING_DATAWHITENING,
            "0xe9cae9ca;0x21;0",
        ]);
        let mut data = hex2bits("e9cae9ca");
        data.extend_from_slice(&str2bits("101100101001110010100110101011100101"));
        let encoded = e.encode(&data).unwrap();
        let (decoded, state) = e.decode(&encoded).unwrap();
        assert_eq!(state, ErrorState::Success);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_empty_input() {
        let e = Encoding::from_chain_names(&["t", DECODING_INVERT]);
        assert_eq!(e.decode(&[]).unwrap().0, Vec::<Bit>::new());
        assert_eq!(e.encode(&[]).unwrap(), Vec::<Bit>::new());
        let e = Encoding::default();
        assert!(e.is_nrz());
        assert_eq!(e.decode(&str2bits("1010")).unwrap().0, str2bits("1010"));
    }
}
