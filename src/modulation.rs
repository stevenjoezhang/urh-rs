//! Modulation / demodulation core, ported line-by-line from
//! `urh/cythonext/signal_functions.pyx` (float32 IQ semantics throughout).
//!
//! Numeric semantics notes:
//! - All arithmetic is f32 to mirror the Cython `float` code paths
//!   (numpy float32 arrays, `cosf`/`sinf`/`atan2` on floats).
//! - `afp_demod` assumes normalized f32 input, i.e. the pyx `float[:, ::1]`
//!   branch where `max_magnitude = sqrt(2)`.

use crate::{Bit, Complex32, Modulation};
use std::f32::consts::PI;

const PAUSE_STATE: i64 = -1;
const NOISE_FSK_PSK: f32 = -4.0;
const NOISE_ASK: f32 = 0.0;

fn get_noise_for_mod_type(mod_type: Modulation) -> f32 {
    match mod_type {
        Modulation::Ask => NOISE_ASK,
        Modulation::Fsk | Modulation::Psk | Modulation::Oqpsk => NOISE_FSK_PSK,
        // pyx: no explicit GFSK branch -> else -> 0
        Modulation::Gfsk => 0.0,
    }
}

/// util.pyx bit_array_to_number: bits[start..end], LSB = bits[end-1].
fn bit_array_to_number(bits: &[Bit], start: usize, end: usize) -> usize {
    if end < 1 || end <= start {
        return 0;
    }
    let mut result: u64 = 0;
    let mut acc: u64 = 1;
    for i in start..end {
        result += bits[end - 1 - i + start] as u64 * acc;
        acc *= 2;
    }
    result as usize
}

/// util.py number_to_bits: MSB-first binary representation, zero padded to `length`.
fn number_to_bits(n: usize, length: usize) -> Vec<Bit> {
    let mut result = vec![0u8; length];
    for i in 0..length {
        let bit = (n >> (length - 1 - i)) & 1;
        result[i] = bit as Bit;
    }
    result
}

// ---------------------------------------------------------------------------
// Modulation
// ---------------------------------------------------------------------------

/// Port of `modulate_c` / `__modulate` (pyx float branch). Produces normalized
/// f32 IQ samples. `gauss_bt` / `filter_width` only apply to GFSK.
#[allow(clippy::too_many_arguments)]
pub fn modulate(
    bits: &[Bit],
    samples_per_symbol: u32,
    modulation_type: Modulation,
    parameters: &[f32],
    bits_per_symbol: u16,
    carrier_amplitude: f32,
    carrier_frequency: f32,
    carrier_phase: f32,
    sample_rate: f32,
    pause: u32,
    start: u32,
) -> Vec<Complex32> {
    modulate_gauss(
        bits,
        samples_per_symbol,
        modulation_type,
        parameters,
        bits_per_symbol,
        carrier_amplitude,
        carrier_frequency,
        carrier_phase,
        sample_rate,
        pause,
        start,
        0.5,
        1.0,
    )
}

/// Full `__modulate` including the GFSK knobs `gauss_bt` and `filter_width`.
#[allow(clippy::too_many_arguments)]
pub fn modulate_gauss(
    bits: &[Bit],
    samples_per_symbol: u32,
    modulation_type: Modulation,
    parameters: &[f32],
    bits_per_symbol: u16,
    carrier_amplitude: f32,
    carrier_frequency: f32,
    carrier_phase: f32,
    sample_rate: f32,
    pause: u32,
    start: u32,
    gauss_bt: f32,
    filter_width: f32,
) -> Vec<Complex32> {
    let num_bits = bits.len();
    let total_symbols = (num_bits / bits_per_symbol as usize) as u32;
    let total_samples = (total_symbols as i64) * (samples_per_symbol as i64) + pause as i64;

    let mut result = vec![Complex32::new(0.0, 0.0); total_samples as usize];
    if num_bits == 0 {
        return result;
    }

    let is_ask = modulation_type == Modulation::Ask;
    let is_fsk = modulation_type == Modulation::Fsk;
    let is_psk = modulation_type == Modulation::Psk;
    let is_oqpsk = modulation_type == Modulation::Oqpsk;
    let is_gfsk = modulation_type == Modulation::Gfsk;

    let oqpsk_bits_storage;
    let bits: &[Bit] = if is_oqpsk {
        assert_eq!(bits_per_symbol, 2);
        oqpsk_bits_storage = get_oqpsk_bits(bits);
        &oqpsk_bits_storage
    } else {
        bits
    };
    let bits_per_symbol = if is_oqpsk { 2 } else { bits_per_symbol };

    let gauss_filtered_freqs_phases: Option<Vec<(f32, f32)>> = if is_gfsk {
        Some(get_gauss_filtered_freqs_phases(
            bits,
            parameters,
            total_symbols,
            samples_per_symbol,
            sample_rate,
            carrier_phase,
            start,
            gauss_bt,
            filter_width,
        ))
    } else {
        None
    };

    // FSK phase corrections to prevent spiky jumps between symbols.
    let mut phase_corrections: Vec<f32> = Vec::new();
    if is_fsk && total_symbols > 0 {
        phase_corrections = vec![0.0; total_symbols as usize];
        for s_i in 1..total_symbols as usize {
            let index = bit_array_to_number(
                bits,
                s_i * bits_per_symbol as usize,
                (s_i + 1) * bits_per_symbol as usize,
            );
            let prev_index = bit_array_to_number(
                bits,
                (s_i - 1) * bits_per_symbol as usize,
                s_i * bits_per_symbol as usize,
            );
            let f = parameters[index];
            let f_previous = parameters[prev_index];
            if f != f_previous {
                let t = ((s_i as u32 * samples_per_symbol + start - 1) as f32) / sample_rate;
                phase_corrections[s_i] =
                    (phase_corrections[s_i - 1] + 2.0 * PI * (f_previous - f) * t) % (2.0 * PI);
            } else {
                phase_corrections[s_i] = phase_corrections[s_i - 1];
            }
        }
    }

    for s_i in 0..total_symbols as usize {
        let index = bit_array_to_number(
            bits,
            s_i * bits_per_symbol as usize,
            (s_i + 1) * bits_per_symbol as usize,
        );

        let mut a = carrier_amplitude;
        let mut f = carrier_frequency;
        let mut phi = carrier_phase;
        let mut phase_correction = 0.0f32;

        if is_ask {
            a = parameters[index];
            if a == 0.0 {
                continue; // leave zeros
            }
        } else if is_fsk {
            f = parameters[index];
            phase_correction = phase_corrections[s_i];
        } else if is_psk || is_oqpsk {
            phi = parameters[index];
        }

        for i in (s_i as u32 * samples_per_symbol)..((s_i as u32 + 1) * samples_per_symbol) {
            let t = (i + start) as f32 / sample_rate;
            if is_gfsk {
                let (gf, gp) = gauss_filtered_freqs_phases.as_ref().unwrap()[i as usize];
                f = gf;
                phi = gp;
            }
            let current_arg = 2.0 * PI * f * t + phi + phase_correction;
            result[i as usize] = Complex32::new(a * current_arg.cos(), a * current_arg.sin());
        }
    }

    if is_oqpsk {
        for i in 0..samples_per_symbol as usize {
            result[i].im = 0.0;
        }
        let n = total_samples as usize;
        let sps = samples_per_symbol as usize;
        for i in (n - pause as usize - sps)..(n - pause as usize) {
            result[i].re = 0.0;
        }
    }

    result
}

/// pyx get_oqpsk_bits. NOTE: upstream marks this as not working correctly;
/// ported verbatim (including the untouched zero entries).
pub fn get_oqpsk_bits(original_bits: &[Bit]) -> Vec<Bit> {
    let num_bits = original_bits.len();
    if num_bits == 0 {
        return Vec::new();
    }
    let mut result = vec![0u8; num_bits + 2];
    result[0] = original_bits[0];
    result[num_bits + 2 - 1] = original_bits[num_bits - 1];

    // pyx: range(2, num_bits - 2, 2)
    let mut i = 2usize;
    while i + 2 < num_bits {
        // Python range excludes num_bits-2, i.e. i < num_bits - 2
        result[i] = original_bits[i];
        result[i + 1] = original_bits[i - 1];
        i += 2;
    }
    result
}

fn get_gauss_filtered_freqs_phases(
    bits: &[Bit],
    parameters: &[f32],
    num_symbols: u32,
    samples_per_symbol: u32,
    sample_rate: f32,
    phi: f32,
    start: u32,
    gauss_bt: f32,
    filter_width: f32,
) -> Vec<(f32, f32)> {
    let num_values = (num_symbols as usize) * (samples_per_symbol as usize);
    let bits_per_symbol = bits.len() / num_symbols as usize;

    let mut frequencies = vec![0.0f32; num_values];
    for s_i in 0..num_symbols as usize {
        let index = bit_array_to_number(
            bits,
            s_i * bits_per_symbol,
            (s_i + 1) * bits_per_symbol,
        );
        for i in (s_i * samples_per_symbol as usize)..((s_i + 1) * samples_per_symbol as usize) {
            frequencies[i] = parameters[index];
        }
    }

    let gfir = gauss_fir(sample_rate, samples_per_symbol, gauss_bt, filter_width);

    // np.convolve(..., mode="same") semantics (see convolve_same).
    let mut frequencies = convolve_same(&frequencies, &gfir);
    if frequencies.len() > num_values {
        frequencies.truncate(num_values);
    }

    let mut phases = vec![0.0f32; frequencies.len()];
    if !phases.is_empty() {
        phases[0] = phi;
    }
    for i in 0..phases.len().saturating_sub(1) {
        let t = ((start as usize + i) as f32) / sample_rate;
        // Correct the phase to prevent spiky jumps
        phases[i + 1] = 2.0 * PI * t * (frequencies[i] - frequencies[i + 1]) + phases[i];
    }

    frequencies.into_iter().zip(phases).collect()
}

/// np.convolve(a, v, mode="same"): the middle max(len(a), len(v)) samples of
/// the full convolution, then (caller) truncated when the FIR outgrows the signal.
fn convolve_same(a: &[f32], v: &[f32]) -> Vec<f32> {
    let n = a.len();
    let m = v.len();
    if n == 0 || m == 0 {
        return Vec::new();
    }
    let full_len = n + m - 1;
    let out_len = n.max(m);
    let mut full = vec![0.0f64; full_len];
    // f64 accumulation for parity with numpy's pairwise summation stability.
    for i in 0..n {
        for j in 0..m {
            full[i + j] += a[i] as f64 * v[j] as f64;
        }
    }
    let start_idx = (full_len - out_len) / 2;
    full[start_idx..start_idx + out_len]
        .iter()
        .map(|&x| x as f32)
        .collect()
}

/// pyx gauss_fir (http://onlinelibrary.wiley.com/doi/10.1002/9780470041956.app2/pdf)
fn gauss_fir(sample_rate: f32, samples_per_symbol: u32, bt: f32, filter_width: f32) -> Vec<f32> {
    let k_min = -(filter_width * samples_per_symbol as f32) as i64;
    let k_max = (filter_width * samples_per_symbol as f32) as i64;
    let ts = samples_per_symbol as f32 / sample_rate; // symbol time

    let c = (2.0 * PI / 2.0f32.ln()).sqrt() * bt / ts;
    let d = (2.0f32.sqrt() * PI / 2.0f32.ln().sqrt() * bt) / samples_per_symbol as f32;

    let mut h: Vec<f32> = (k_min..=k_max)
        .map(|k| c * (-(d * k as f32) * (d * k as f32)).exp())
        .collect();
    let sum: f32 = h.iter().sum();
    for x in &mut h {
        *x /= sum;
    }
    h
}

// ---------------------------------------------------------------------------
// Demodulation
// ---------------------------------------------------------------------------

fn clamp(x: f32) -> f32 {
    if x < -1.0 {
        -1.0
    } else if x > 1.0 {
        1.0
    } else {
        x
    }
}

/// pyx costa_demod (float branch: scale = 1, shift = 0). Used for PSK.
fn costa_demod(
    samples: &[Complex32],
    noise_sqrd: f32,
    loop_order: i32,
    bandwidth: f32,
    damping: f32,
) -> Vec<f32> {
    let alpha = (4.0 * damping * bandwidth)
        / (1.0 + 2.0 * damping * bandwidth + bandwidth * bandwidth);
    let beta = (4.0 * bandwidth * bandwidth)
        / (1.0 + 2.0 * damping * bandwidth + bandwidth * bandwidth);

    let num_samples = samples.len();
    let mut result = vec![0.0f32; num_samples];

    let mut loop_order = loop_order;
    if loop_order > 4 {
        loop_order = 4;
    }

    let mut costa_freq = 0.0f32;
    let mut costa_error = 0.0f32;
    let mut costa_phase = 1.5f32;

    for i in 1..num_samples {
        let real = samples[i].re;
        let imag = samples[i].im;

        if real * real + imag * imag <= noise_sqrd {
            result[i] = NOISE_FSK_PSK;
            continue;
        }

        // float branch: scale = 1.0, shift = 0.0
        let current_sample = Complex32::new(real, imag);
        let nco_out = Complex32::new((-costa_phase).cos(), (-costa_phase).sin());
        let nco_times_sample = nco_out * current_sample;

        if loop_order == 2 {
            costa_error = nco_times_sample.im * nco_times_sample.re;
        } else if loop_order == 4 {
            let f1 = if nco_times_sample.re > 0.0 { 1.0 } else { -1.0 };
            let f2 = if nco_times_sample.im > 0.0 { 1.0 } else { -1.0 };
            costa_error = f1 * nco_times_sample.im - f2 * nco_times_sample.re;
        }

        costa_error = clamp(costa_error);

        // advance the loop
        costa_freq += beta * costa_error;
        costa_phase += costa_freq + alpha * costa_error;

        // wrap the phase
        while costa_phase > 2.0 * PI {
            costa_phase -= 2.0 * PI;
        }
        while costa_phase < -2.0 * PI {
            costa_phase += 2.0 * PI;
        }
        costa_freq = clamp(costa_freq);

        if loop_order == 2 {
            result[i] = nco_times_sample.re;
        } else if loop_order == 4 {
            result[i] = 2.0 * nco_times_sample.re + nco_times_sample.im;
        }
    }

    result
}

/// pyx afp_demod, float branch (max_magnitude = sqrt(2)).
pub fn afp_demod(
    samples: &[Complex32],
    noise_mag: f32,
    mod_type: Modulation,
    mod_order: i32,
    costas_loop_bandwidth: f32,
) -> Vec<f32> {
    let ns = samples.len();
    if ns <= 2 {
        return vec![0.0; ns];
    }

    let noise = get_noise_for_mod_type(mod_type);
    let noise_sqrd = noise_mag * noise_mag;
    let max_magnitude = 2.0f32.sqrt(); // float branch

    if mod_type == Modulation::Psk {
        return costa_demod(
            samples,
            noise_sqrd,
            mod_order,
            costas_loop_bandwidth,
            2.0f32.sqrt() / 2.0,
        );
    }

    let mut result = vec![0.0f32; ns];
    result[0] = noise;

    for i in 1..ns {
        let real = samples[i].re;
        let imag = samples[i].im;
        let magnitude = real * real + imag * imag;
        if magnitude <= noise_sqrd {
            result[i] = noise;
            continue;
        }

        match mod_type {
            Modulation::Ask => {
                result[i] = magnitude.sqrt() / max_magnitude;
            }
            Modulation::Fsk => {
                let tmp = Complex32::new(samples[i - 1].re, -samples[i - 1].im)
                    * Complex32::new(real, imag);
                result[i] = tmp.im.atan2(tmp.re); // Freq
            }
            _ => {}
        }
    }

    result
}

/// pyx get_center_thresholds.
pub fn get_center_thresholds(center: f32, spacing: f32, modulation_order: usize) -> Vec<f32> {
    let mut result = vec![0.0f32; modulation_order.saturating_sub(1)];
    let n = modulation_order / 2;

    for (i, r) in result.iter_mut().enumerate().take(n) {
        *r = center - (n as f32 - (i as f32 + 1.0)) * spacing;
    }
    for (i, r) in result.iter_mut().enumerate().skip(n) {
        *r = center + (i as f32 + 1.0 - n as f32) * spacing;
    }
    result
}

/// pyx grab_pulse_lens. Returns (states, lens): states[i] is the symbol type
/// (pause = -1), lens[i] the pulse length in samples.
#[allow(clippy::too_many_arguments)]
pub fn grab_pulse_lens(
    samples: &[f32],
    center: f32,
    tolerance: u16,
    modulation_type: Modulation,
    samples_per_symbol: u32,
    bits_per_symbol: u8,
    center_spacing: f32,
) -> (Vec<i64>, Vec<i64>) {
    let is_ask = modulation_type == Modulation::Ask;
    let num_samples = samples.len() as i64;
    let mut pulse_length: i64 = 0;
    let mut cur_index: usize = 0;
    let mut consecutive_pause: i64 = 0;
    let noise = get_noise_for_mod_type(modulation_type);

    let modulation_order: usize = 1usize << bits_per_symbol;
    let tolerance = tolerance as i64;

    let thresholds = get_center_thresholds(center, center_spacing, modulation_order);

    let mut states: Vec<i64> = Vec::new();
    let mut lens: Vec<i64> = Vec::new();
    if num_samples == 0 {
        return (states, lens);
    }

    let mut state_count = vec![0i64; modulation_order];

    // Initial state. NOTE: the pyx classifies the *initial* state using the
    // uninitialized-then-defaulted variable `s` (0.0), not samples[0] -- this
    // quirk is reproduced faithfully.
    let s_prev = samples[0];
    let mut cur_state: i64 = if s_prev == noise {
        PAUSE_STATE
    } else {
        let mut st = (modulation_order - 1) as i64;
        let s = 0.0f32;
        for k in 0..modulation_order - 1 {
            if s <= thresholds[k] {
                st = k as i64;
                break;
            }
        }
        st
    };

    for i in 0..num_samples as usize {
        pulse_length += 1;
        let s = samples[i];

        let tmp_state: i64 = if s == noise {
            PAUSE_STATE
        } else {
            let mut st = (modulation_order - 1) as i64;
            for k in 0..modulation_order - 1 {
                if s <= thresholds[k] {
                    st = k as i64;
                    break;
                }
            }
            st
        };

        if tmp_state == PAUSE_STATE {
            consecutive_pause += 1;
        } else {
            consecutive_pause = 0;
        }

        for j in 0..modulation_order {
            if j as i64 == tmp_state {
                state_count[j] += 1;
            } else {
                state_count[j] = 0;
            }
        }

        if cur_state == tmp_state {
            continue;
        }

        let mut new_state: i64 = -42;

        if consecutive_pause > tolerance {
            new_state = PAUSE_STATE;
        } else {
            for j in 0..modulation_order {
                if state_count[j] > tolerance {
                    new_state = j as i64;
                    break;
                }
            }
        }

        if new_state == -42 {
            continue;
        }

        if is_ask && cur_state == PAUSE_STATE && (pulse_length - tolerance) < samples_per_symbol as i64
        {
            // Aggregate short pauses for ASK
            cur_state = 0;
        }

        if cur_index > 0 && states[cur_index - 1] == cur_state {
            lens[cur_index - 1] += pulse_length - tolerance;
        } else {
            states.push(cur_state);
            lens.push(pulse_length - tolerance);
            cur_index += 1;
        }

        pulse_length = tolerance;
        cur_state = new_state;
    }

    // Append last one
    if cur_index > 0 && states[cur_index - 1] == cur_state {
        lens[cur_index - 1] += pulse_length - tolerance;
    } else {
        states.push(cur_state);
        lens.push(pulse_length - tolerance);
    }

    (states, lens)
}

/// pyx find_nearest_center.
pub fn find_nearest_center(sample: f32, centers: &[f32]) -> i32 {
    let mut result = 0i32;
    let mut min_diff = 99999.0f32;
    for (i, &center) in centers.iter().enumerate() {
        let cur_diff = (sample - center) * (sample - center);
        if cur_diff < min_diff {
            min_diff = cur_diff;
            result = i as i32;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Convenience: full demodulation chain to bits (ProtocolAnalyzer._ppseq_to_bits)
// ---------------------------------------------------------------------------

/// Demodulate IQ samples all the way to messages of bits, mirroring
/// `Signal.quad_demod` + `ProtocolAnalyzer._ppseq_to_bits`
/// (default `pause_threshold` in URH is 8).
#[allow(clippy::too_many_arguments)]
pub fn demod_to_bits(
    samples: &[Complex32],
    noise_mag: f32,
    mod_type: Modulation,
    mod_order: i32,
    costas_loop_bandwidth: f32,
    center: f32,
    tolerance: u16,
    samples_per_symbol: u32,
    bits_per_symbol: u8,
    center_spacing: f32,
    pause_threshold: u32,
) -> Vec<Vec<Bit>> {
    let qad = afp_demod(samples, noise_mag, mod_type, mod_order, costas_loop_bandwidth);
    let (states, lens) = grab_pulse_lens(
        &qad,
        center,
        tolerance,
        mod_type,
        samples_per_symbol,
        bits_per_symbol,
        center_spacing,
    );

    let bits_per_symbol = bits_per_symbol as usize;
    let pause_threshold = pause_threshold as i64;

    let mut data_bits: Vec<Bit> = Vec::new();
    let mut resulting: Vec<Vec<Bit>> = Vec::new();
    let mut there_was_data = false;

    let mut start = 0usize;
    if !states.is_empty() && states[0] == PAUSE_STATE {
        start = 1; // Starts with pause
    }

    for i in start..states.len() {
        let cur_pulse_type = states[i];
        let num_samples = lens[i];
        let num_symbols_float = num_samples as f64 / samples_per_symbol as f64;
        let mut num_symbols = num_symbols_float as i64;
        if num_symbols_float - num_symbols as f64 > 0.5 {
            num_symbols += 1;
        }

        if cur_pulse_type == PAUSE_STATE {
            // OOK
            if num_symbols <= pause_threshold || pause_threshold == 0 {
                data_bits.extend(std::iter::repeat(0u8).take((num_symbols * bits_per_symbol as i64) as usize));
            } else if !there_was_data {
                // Ignore this pause, if there were no information transmitted previously
                data_bits.clear();
            } else {
                resulting.push(std::mem::take(&mut data_bits));
                there_was_data = false;
            }
        } else {
            let symbol_bits = number_to_bits(cur_pulse_type as usize, bits_per_symbol);
            for _ in 0..num_symbols {
                data_bits.extend_from_slice(&symbol_bits);
            }
            if !there_was_data && num_symbols > 0 {
                there_was_data = true;
            }
        }
    }

    if there_was_data {
        resulting.push(data_bits);
    }

    resulting
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bits_of(s: &str) -> Vec<Bit> {
        s.bytes().map(|b| b - b'0').collect()
    }

    fn bits_str(bits: &[Bit]) -> String {
        bits.iter().map(|&b| (b'0' + b) as char).collect()
    }

    fn read_complex_file(path: &str) -> Vec<Complex32> {
        let data = std::fs::read(path).expect("read complex file");
        data.chunks_exact(8)
            .map(|c| {
                Complex32::new(
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect()
    }

    // ---- unit tests ----

    #[test]
    fn test_get_center_thresholds() {
        // order 2: single threshold at center
        assert_eq!(get_center_thresholds(0.0, 0.1, 2), vec![0.0]);
        // order 4, center 0, spacing 0.1
        let t = get_center_thresholds(0.0, 0.1, 4);
        assert_eq!(t, vec![-0.1, 0.0, 0.1]);
        // order 4, center 0.5, spacing 1
        let t = get_center_thresholds(0.5, 1.0, 4);
        assert_eq!(t, vec![-0.5, 0.5, 1.5]);
        // order 8, center 0, spacing 1
        let t = get_center_thresholds(0.0, 1.0, 8);
        assert_eq!(t, vec![-3.0, -2.0, -1.0, 0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_find_nearest_center() {
        let centers = [-0.1f32, 0.0, 0.1];
        assert_eq!(find_nearest_center(-0.11, &centers), 0);
        assert_eq!(find_nearest_center(0.04, &centers), 1);
        assert_eq!(find_nearest_center(0.2, &centers), 2);
        assert_eq!(find_nearest_center(5.0, &centers), 2);
        assert_eq!(find_nearest_center(-5.0, &centers), 0);
        // empty -> 0 (pyx semantics: min_diff never beaten)
        assert_eq!(find_nearest_center(1.0, &[]), 0);
    }

    #[test]
    fn test_get_oqpsk_bits() {
        assert!(get_oqpsk_bits(&[]).is_empty());
        // Single bit: [b0, 0, b0]
        assert_eq!(get_oqpsk_bits(&[1]), vec![1, 0, 1]);
        // Two bits: result[0]=b0, result[3]=b1, middle untouched (0)
        assert_eq!(get_oqpsk_bits(&[1, 0]), vec![1, 0, 0, 0]);
        // Hand-derived for 8 bits: loop runs i=2,4 (i<6)
        let inp = bits_of("10110010");
        let out = get_oqpsk_bits(&inp);
        assert_eq!(out.len(), 10);
        assert_eq!(out[0], 1);
        assert_eq!(out[9], 0);
        // i=2: result[2]=in[2], result[3]=in[1]
        assert_eq!((out[2], out[3]), (1, 0));
        // i=4: result[4]=in[4], result[5]=in[3]
        assert_eq!((out[4], out[5]), (0, 1));
        // untouched
        assert_eq!((out[1], out[6], out[7], out[8]), (0, 0, 0, 0));
    }

    // ---- roundtrip tests (mirroring tests/test_demodulations.py) ----

    #[test]
    fn test_fsk_short_bit_length_roundtrip() {
        // test_demodulations.py::test_fsk_short_bit_length
        let bits = bits_of("101010");
        let parameters = [-10e3f32, 10e3];
        let result = modulate(
            &bits, 8, Modulation::Fsk, &parameters, 1, 1.0, 40e3, 0.0, 1e6, 1000, 0,
        );

        // Ensure we have no spikes (np.max(qad) < 1, as in the Python test;
        // NOISE marks are -4 and therefore do not count)
        let qad = afp_demod(&result, 0.0, Modulation::Fsk, 2, 0.1);
        assert!(qad.iter().all(|&v| v < 1.0), "spikes in qad");

        let messages = demod_to_bits(
            &result, 0.0, Modulation::Fsk, 2, 0.1, 0.0, 5, 8, 1, 0.1, 8,
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(bits_str(&messages[0]), "101010");
    }

    #[test]
    fn test_4_fsk_roundtrip() {
        // test_demodulations.py::test_4_fsk
        let bits: Vec<Bit> = vec![1, 0, 1, 0, 1, 1, 0, 0, 0, 1];
        let parameters = [-20e3f32, -10e3, 10e3, 20e3];
        let result = modulate(
            &bits, 100, Modulation::Fsk, &parameters, 2, 1.0, 40e3, 0.0, 1e6, 1000, 0,
        );

        let messages = demod_to_bits(
            &result, 0.0, Modulation::Fsk, 4, 0.1, 0.0, 5, 100, 2, 0.1, 8,
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(bits_str(&messages[0]), "1010110001");
    }

    #[test]
    fn test_4_psk_roundtrip() {
        // test_demodulations.py::test_4_psk (noiseless part)
        let bits: Vec<Bit> = vec![1, 0, 1, 0, 1, 0, 1, 0, 1, 1, 0, 0, 0, 1, 0, 1];
        let parameters: Vec<f32> = [-135.0f32, -45.0, 45.0, 135.0]
            .iter()
            .map(|a| PI * a / 180.0)
            .collect();
        let result = modulate(
            &bits, 100, Modulation::Psk, &parameters, 2, 1.0, 40e3, 0.0, 1e6, 1000, 0,
        );

        let messages = demod_to_bits(
            &result, 0.0, Modulation::Psk, 4, 0.1, 0.0, 5, 100, 2, 1.0, 8,
        );
        assert_eq!(messages.len(), 1);
        let demod = bits_str(&messages[0]);
        assert_eq!(demod.len(), 16);
        assert!(demod.starts_with("10101010"), "got {}", demod);
    }

    #[test]
    fn test_ask_roundtrip() {
        // ASK/OOK roundtrip (no direct pyx-side counterpart test, parameters
        // analogous to test_fsk_short_bit_length). Note: a trailing 0 bit would
        // merge with the pause (100 + 1000 samples = 11 symbols > pause
        // threshold 8) and be swallowed, which is faithful pyx behavior; the
        // test sequence therefore ends with a 1.
        let bits = bits_of("101100101");
        let parameters = [0.0f32, 1.0];
        let result = modulate(
            &bits, 100, Modulation::Ask, &parameters, 1, 1.0, 40e3, 0.0, 1e6, 1000, 0,
        );
        assert_eq!(result.len(), 9 * 100 + 1000);

        // zero bits are silent (bits[1] == 0 -> samples 100..199)
        for i in (1 * 100)..(2 * 100) {
            assert_eq!(result[i], Complex32::new(0.0, 0.0));
        }

        let messages = demod_to_bits(
            &result, 0.0, Modulation::Ask, 2, 0.1, 0.5, 5, 100, 1, 0.1, 8,
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(bits_str(&messages[0]), "101100101");
    }

    #[test]
    fn test_gfsk_modulate_no_spikes() {
        // GFSK: gaussian filtering + phase correction must keep the demodulated
        // frequency estimate bounded (no phase discontinuity spikes).
        let bits = bits_of("1011001010101010");
        let parameters = [-10e3f32, 10e3];
        let result = modulate_gauss(
            &bits, 100, Modulation::Gfsk, &parameters, 1, 1.0, 40e3, 0.0, 1e6, 1000, 0, 0.5, 1.0,
        );
        assert_eq!(result.len(), 16 * 100 + 1000);

        let qad = afp_demod(&result, 0.0, Modulation::Fsk, 2, 0.1);
        // message part (skip demod[0] which is the NOISE mark by definition):
        // |freq estimate| stays well below pi (no phase-jump spikes)
        let max = qad[1..16 * 100].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(max < 0.5, "GFSK demod spikes: {}", max);
    }

    // ---- real data tests ----

    fn data_path(name: &str) -> String {
        format!("/Users/mimi/urh-ref/tests/data/{}", name)
    }

    #[test]
    fn test_ask_short_file() {
        // test_demodulations.py::test_ask_two (exact expected bits)
        let samples = read_complex_file(&data_path("ask_short.complex"));
        assert_eq!(samples.len(), 131);

        let messages = demod_to_bits(
            &samples, 0.0299, Modulation::Ask, 2, 0.1, 0.13, 0, 16, 1, 0.1, 8,
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(bits_str(&messages[0]), "10101010");
    }

    #[test]
    fn test_ask_file_prefix() {
        // test_demodulations.py::test_ask: sps 295, center 0.0219.
        // Noise threshold picked manually (the Python test uses automatic
        // noise detection, which lives in another module): the file's noise
        // floor is |c| ~ 0.008-0.011 and the signal is ~0.07-0.094.
        let samples = read_complex_file(&data_path("ask.complex"));
        assert_eq!(samples.len(), 13710);

        let messages = demod_to_bits(
            &samples, 0.03, Modulation::Ask, 2, 0.1, 0.0219, 5, 295, 1, 0.1, 8,
        );
        assert!(!messages.is_empty());
        let first = bits_str(&messages[0]);
        assert!(
            first.starts_with("1011001001011011011011011011011011001000000"),
            "got {}",
            first
        );
    }

    #[test]
    fn test_fsk_file() {
        // test_demodulations.py::test_fsk: sps 100, center 0.
        // Noise threshold 0.005 chosen manually (auto detection is out of scope
        // here): the file's noise floor is |c| ~ 0.0003 and the signal ~0.018.
        let samples = read_complex_file(&data_path("fsk.complex"));

        let messages = demod_to_bits(
            &samples, 0.005, Modulation::Fsk, 2, 0.1, 0.0, 5, 100, 1, 0.1, 8,
        );
        assert!(!messages.is_empty());
        let first = bits_str(&messages[0]);
        assert!(
            first.starts_with("10101010101010101010101010101010"),
            "got {}",
            first
        );
        // message section must be non-noise: substantial decoded payload
        assert!(first.len() > 100, "message too short: {}", first.len());
    }
}
