//! Automatic signal interpretation.
//!
//! Port of `urh/cythonext/auto_interpretation.pyx` and
//! `urh/ainterpretation/AutoInterpretation.py` (plus the small Haar wavelet
//! helper from `urh/ainterpretation/Wavelet.py`).
//!
//! Demodulation for the estimation pipeline (`afp_demod`) is ported locally
//! from `cythonext/signal_functions.pyx` so this module stays self-contained;
//! once `crate::modulation` provides the agreed `afp_demod` signature the local
//! copy can be dropped.

use std::collections::HashMap;

use num_complex::{Complex, Complex64};

use crate::{Complex32, Modulation};

/// Result of the automatic interpretation of a signal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interpretation {
    pub modulation: Modulation,
    pub bit_length: usize,
    pub center: f32,
    pub noise_threshold: f32,
    pub tolerance: u32,
}

// ---------------------------------------------------------------------------
// cythonext/auto_interpretation.pyx
// ---------------------------------------------------------------------------

/// k-means clustering with k initial centers taken from the distinct values.
/// Python picks the initial centers from a `set` (unordered); we use the first
/// k distinct values in data order for determinism.
pub fn k_means(data: &[f32], k: usize) -> (Vec<f32>, Vec<Vec<f32>>) {
    let mut k = k.max(1);
    let mut unique: Vec<f32> = Vec::new();
    for &v in data {
        if !unique.contains(&v) {
            unique.push(v);
        }
    }
    if unique.len() < k {
        k = unique.len();
    }
    let mut centers: Vec<f32> = unique.iter().take(k).copied().collect();
    let mut clusters: Vec<Vec<f32>> = vec![Vec::new(); k];

    // Python loops while error != 0; we add an iteration cap for safety.
    for _ in 0..1000 {
        for c in clusters.iter_mut() {
            c.clear();
        }
        for &v in data {
            let mut index = 0;
            let mut min_distance = f32::INFINITY;
            for (j, &c) in centers.iter().enumerate() {
                let distance = (c - v) * (c - v);
                if distance < min_distance {
                    min_distance = distance;
                    index = j;
                }
            }
            clusters[index].push(v);
        }
        let old_centers = centers.clone();
        for i in 0..k {
            if !clusters[i].is_empty() {
                centers[i] = clusters[i].iter().sum::<f32>() / clusters[i].len() as f32;
            }
        }
        let mut error = 0.0f32;
        for i in 0..k {
            error += old_centers[i] * old_centers[i] - centers[i] * centers[i];
        }
        if error == 0.0 {
            break;
        }
    }
    (centers, clusters)
}

/// Get (start, end) sample index pairs of messages, i.e. plateaus above the
/// noise threshold. Robust against up to `outlier_tolerance` (10) consecutive
/// outliers, like the reference implementation.
pub fn segment_messages_from_magnitudes(magnitudes: &[f32], noise_threshold: f32) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    if magnitudes.is_empty() {
        return result;
    }

    let n = magnitudes.len();
    let mut start = 0usize;

    let outlier_tolerance = 10usize;
    let mut conseq_above = 0usize;
    let mut conseq_below = 0usize;

    // Three states: 1 = above noise, -1 = in noise
    let mut state: i32 = if magnitudes[0] > noise_threshold { 1 } else { -1 };

    for i in 0..n {
        let is_above_noise = magnitudes[i] > noise_threshold;
        if state == 1 {
            if is_above_noise {
                conseq_below = 0;
            } else {
                conseq_below += 1;
            }
        } else if state == -1 {
            if is_above_noise {
                conseq_above += 1;
            } else {
                conseq_above = 0;
            }
        }

        if state == 1 && conseq_below >= outlier_tolerance {
            state = -1;
            result.push((start, i - conseq_below));
            conseq_below = 0;
            conseq_above = 0;
        } else if state == -1 && conseq_above >= outlier_tolerance {
            state = 1;
            start = i - conseq_above;
            conseq_below = 0;
            conseq_above = 0;
        }
    }

    // append last message
    if state == 1 && start < n - conseq_below {
        result.push((start, n - conseq_below));
    }

    result
}

/// Histogram counting how often a value is a "threshold divisor" of another
/// value: max/min has a fractional part below `threshold` (0.2).
pub fn get_threshold_divisor_histogram(plateau_lengths: &[u64]) -> Vec<u64> {
    let num_lengths = plateau_lengths.len();
    if num_lengths == 0 {
        return Vec::new();
    }
    let max_len = *plateau_lengths.iter().max().unwrap();
    let mut histogram = vec![0u64; max_len as usize + 1];

    let threshold = 0.2f64;
    for i in 0..num_lengths {
        for j in (i + 1)..num_lengths {
            let (x, y) = (plateau_lengths[i], plateau_lengths[j]);
            if x == 0 || y == 0 {
                continue;
            }
            let (minimum, maximum) = if x < y { (x, y) } else { (y, x) };
            // Python: maximum / (double)minimum - maximum / minimum  (int division)
            let frac = maximum as f64 / minimum as f64 - (maximum / minimum) as f64;
            if frac < threshold {
                histogram[minimum as usize] += 1;
            }
        }
    }
    histogram
}

/// Merge plateaus that are separated only by short (<= tolerance) plateaus.
/// Direct port of the (quirky) reference algorithm.
pub fn merge_plateaus(plateaus: &[u64], tolerance: u64, max_count: u64) -> Vec<u64> {
    let l = plateaus.len();
    if l == 0 {
        return Vec::new();
    }

    let mut result = vec![0u64; l];
    result[0] = if plateaus[0] <= tolerance { 0 } else { plateaus[0] };

    let mut current: u64 = 0;
    let mut i: u64 = 1;
    while (i as usize) < l && current < max_count {
        if plateaus[i as usize] <= tolerance {
            // Look ahead to see whether we need to merge a larger window
            // e.g. for 67, 1, 10, 1, 21
            let mut n: u64 = 2;
            while ((i + n) as usize) < l && plateaus[(i + n) as usize] <= tolerance {
                n += 2;
            }
            let upper = std::cmp::min(l as u64, i + n);
            let mut tmp_sum = 0u64;
            for j in (i - 1)..upper {
                tmp_sum += plateaus[j as usize];
            }
            result[current as usize] = tmp_sum;
            i += n;
        } else {
            current += 1;
            result[current as usize] = plateaus[i as usize];
            i += 1;
        }
    }

    result.truncate(current as usize + 1);
    result
}

/// Plateau lengths (runs of samples on the same side of `center`), sampled
/// from the beginning of the data until `percentage` % of samples are covered.
pub fn get_plateau_lengths(rect_data: &[f32], center: f32, percentage: f64) -> Vec<u64> {
    if rect_data.is_empty() {
        return Vec::new();
    }

    let len_data = rect_data.len();
    let mut state: i32 = if rect_data[0] <= center { -1 } else { 1 };
    let mut plateau_length: u64 = 0;
    let mut current_sum: u64 = 0;
    let mut result: Vec<u64> = Vec::new();

    for &sample in rect_data {
        if current_sum as f64 >= percentage * len_data as f64 / 100.0 {
            break;
        }
        let new_state: i32 = if sample <= center { -1 } else { 1 };
        if state == new_state {
            plateau_length += 1;
        } else {
            result.push(plateau_length);
            current_sum += plateau_length;
            state = new_state;
            plateau_length = 1;
        }
    }
    result
}

/// Median filter. Note: like the reference (cython) implementation, the window
/// starts *at* index i (not centered on i) — the centered `start` computed in
/// the reference is never actually used.
pub fn median_filter(data: &[f64], k: usize) -> Vec<f32> {
    let n = data.len();
    let mut result = vec![0f32; n];
    for i in 0..n {
        let mut kk = k;
        if i + kk > n {
            kk = n - i;
        }
        let mut buffer: Vec<f64> = data[i..i + kk].to_vec();
        buffer.sort_by(|a, b| a.partial_cmp(b).unwrap());
        result[i] = buffer[kk / 2] as f32;
    }
    result
}

// ---------------------------------------------------------------------------
// ainterpretation/AutoInterpretation.py
// ---------------------------------------------------------------------------

fn variance(data: &[f64]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mean = data.iter().sum::<f64>() / data.len() as f64;
    data.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / data.len() as f64
}

fn mean_f64(data: &[f64]) -> f64 {
    if data.is_empty() {
        0.0
    } else {
        data.iter().sum::<f64>() / data.len() as f64
    }
}

fn std_f64(data: &[f64]) -> f64 {
    variance(data).sqrt()
}

/// np.percentile(..., 50) with linear interpolation.
fn percentile_50(data: &[f64]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut sorted = data.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let pos = 0.5 * (n - 1) as f64;
    let lower = pos.floor() as usize;
    let frac = pos - lower as f64;
    sorted[lower] * (1.0 - frac) + sorted[lower + 1] * frac
}

pub fn max_without_outliers(data: &[f64], z: f64) -> Option<f64> {
    if data.is_empty() {
        return None;
    }
    let m = mean_f64(data);
    let s = std_f64(data);
    data.iter()
        .filter(|&&v| (v - m).abs() <= z * s)
        .cloned()
        .fold(None, |acc: Option<f64>, v| Some(match acc {
            Some(a) if a > v => a,
            _ => v,
        }))
}

pub fn min_without_outliers(data: &[f64], z: f64) -> Option<f64> {
    if data.is_empty() {
        return None;
    }
    let m = mean_f64(data);
    let s = std_f64(data);
    data.iter()
        .filter(|&&v| (v - m).abs() <= z * s)
        .cloned()
        .fold(None, |acc: Option<f64>, v| Some(match acc {
            Some(a) if a < v => a,
            _ => v,
        }))
}

/// Most frequent value; on ties the value whose *first occurrence is latest*
/// wins (mirrors the reference's iteration over Counter.most_common()).
pub fn get_most_frequent_value<T: Eq + std::hash::Hash + Clone>(values: &[T]) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    let mut counts: HashMap<&T, usize> = HashMap::new();
    for v in values {
        *counts.entry(v).or_insert(0) += 1;
    }
    let max_count = *counts.values().max().unwrap();
    let mut result = None;
    for v in values {
        // iterate in first-occurrence order, keep last max-count value
        if counts[v] == max_count {
            result = Some(v.clone());
        }
    }
    result
}

/// Most common value; on ties the value appearing first in the list wins.
pub fn most_common<T: Eq + std::hash::Hash + Clone>(values: &[T]) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    let mut counts: HashMap<&T, usize> = HashMap::new();
    for v in values {
        *counts.entry(v).or_insert(0) += 1;
    }
    let count_of = |v: &T| counts[v];
    values.iter().max_by_key(|v| count_of(v)).cloned()
}

/// Estimate the noise level from the magnitudes by looking at 1% chunks of
/// the signal and finding the maximum sample of the quietest chunks.
pub fn detect_noise_level(magnitudes: &[f32]) -> f32 {
    if magnitudes.len() <= 3 {
        return 0.0;
    }

    let chunksize = std::cmp::max(1, magnitudes.len() / 100);
    let mut chunks: Vec<&[f32]> = Vec::new();
    let mut i = magnitudes.len();
    while i as i64 - chunksize as i64 >= 0 {
        chunks.push(&magnitudes[i - chunksize..i]);
        i -= chunksize;
    }

    let mean_values: Vec<f32> = chunks
        .iter()
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect();
    let minimum = mean_values.iter().cloned().fold(f32::INFINITY, f32::min);
    let maximum = mean_values.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

    if maximum == 0.0 || minimum as f64 / maximum as f64 > 0.9 {
        // Mean values are very close to each other, so there is probably no
        // noise in the signal
        return 0.0;
    }

    // All indices for values within 10% of the minimum mean value
    let mut result = 0.0f32;
    for (idx, &mv) in mean_values.iter().enumerate() {
        if mv <= 1.1 * minimum {
            let chunk_max = chunks[idx].iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            if chunk_max > result {
                result = chunk_max;
            }
        }
    }

    // Round up to fourth digit
    ((result as f64 * 10000.0).ceil() / 10000.0) as f32
}

/// Merge message segments that stem from an OOK signal (many short pulses
/// separated by short pauses) into longer messages at large pauses.
pub fn merge_message_segments_for_ook(segments: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if segments.len() <= 1 {
        return segments.to_vec();
    }

    let pauses: Vec<u64> = (0..segments.len() - 1)
        .map(|i| (segments[i + 1].0 - segments[i].1) as u64)
        .collect();
    let pulses: Vec<u64> = (0..segments.len())
        .map(|i| (segments[i].1 - segments[i].0) as u64)
        .collect();

    let min_pulse_length = match min_without_outliers(
        &pulses.iter().map(|&p| p as f64).collect::<Vec<_>>(),
        1.0,
    ) {
        Some(v) if v > 0.0 => v,
        _ => return segments.to_vec(),
    };
    let large_pause_indices: Vec<usize> = (0..pauses.len())
        .filter(|&i| pauses[i] as f64 >= 8.0 * min_pulse_length)
        .collect();

    let mut result = Vec::new();
    for i in 0..=large_pause_indices.len() {
        let (start, end) = if i == 0 {
            if !large_pause_indices.is_empty() {
                (0, large_pause_indices[0] + 1)
            } else {
                (0, segments.len())
            }
        } else if i == large_pause_indices.len() {
            (large_pause_indices[i - 1] + 1, segments.len())
        } else {
            (large_pause_indices[i - 1] + 1, large_pause_indices[i] + 1)
        };

        let msg_begin = segments[start].0;
        let mut msg_length = 0i64;
        for j in start..end {
            msg_length += (segments[j].1 - segments[j].0) as i64;
        }
        for j in start + 1..end {
            msg_length += (segments[j].0 - segments[j - 1].1) as i64;
        }
        result.push((msg_begin, (msg_begin as i64 + msg_length) as usize));
    }

    result
}

// --- wavelet (Wavelet.py) --------------------------------------------------

fn normalized_haar_wavelet(omega: &[f64], scale: f64) -> Vec<Complex64> {
    let mut omega_cpy: Vec<f64> = omega.iter().map(|&w| w / scale).collect();
    if !omega_cpy.is_empty() {
        omega_cpy[0] = 1.0; // prevent division by zero
    }
    omega_cpy
        .iter()
        .map(|&w| {
            let e = Complex64::from_polar(1.0, 0.5 * w); // exp(0.5j * w)
            Complex::new(0.0, 1.0) * (-1.0 + e) * (-1.0 + e) / w
        })
        .collect()
}

/// In-place iterative radix-2 FFT. `n` must be a power of two.
fn fft_inplace(a: &mut [Complex64], inverse: bool) {
    let n = a.len();
    if n <= 1 {
        return;
    }
    // bit reversal permutation
    let mut j = 0usize;
    for i in 0..n {
        if i < j {
            a.swap(i, j);
        }
        let mut m = n >> 1;
        while m >= 1 && j & m != 0 {
            j &= !m;
            m >>= 1;
        }
        j |= m;
    }
    let mut len = 2usize;
    while len <= n {
        let ang = 2.0 * std::f64::consts::PI / len as f64 * if inverse { 1.0 } else { -1.0 };
        let wlen = Complex64::from_polar(1.0, ang);
        let mut i = 0;
        while i < n {
            let mut w = Complex64::new(1.0, 0.0);
            for k in 0..len / 2 {
                let u = a[i + k];
                let v = a[i + k + len / 2] * w;
                a[i + k] = u + v;
                a[i + k + len / 2] = u - v;
                w *= wlen;
            }
            i += len;
        }
        len <<= 1;
    }
    if inverse {
        for v in a.iter_mut() {
            *v /= n as f64;
        }
    }
}

/// Continuous Haar wavelet transform (port of Wavelet.cwt_haar).
fn cwt_haar(x: &[Complex64], scale: usize) -> Vec<Complex64> {
    let scale = scale as f64;
    let next_power_two = 1usize << (x.len() as f64).log2() as usize;
    let x = &x[..next_power_two];
    let num_data = x.len();
    if num_data == 0 {
        return Vec::new();
    }

    let mut x_hat = x.to_vec();
    fft_inplace(&mut x_hat, false);

    // omega (eq. (5) in the paper)
    let f = 2.0 * std::f64::consts::PI / num_data as f64;
    let omega: Vec<f64> = (0..num_data)
        .map(|k| {
            if k < num_data / 2 {
                f * k as f64
            } else {
                -f * k as f64
            }
        })
        .collect();

    // psi hat (eq. (6) in the paper): sqrt(2 pi s) * psi_hat(s * omega)
    let scaled_omega: Vec<f64> = omega.iter().map(|&w| w * scale).collect();
    let base = normalized_haar_wavelet(&scaled_omega, scale);
    let psi_hat: Vec<Complex64> = base
        .iter()
        .map(|&b| b * (2.0 * std::f64::consts::PI * scale).sqrt())
        .collect();

    let mut w = x_hat
        .iter()
        .zip(psi_hat.iter())
        .map(|(&a, &b)| a * b)
        .collect::<Vec<_>>();
    fft_inplace(&mut w, true);

    let cut = (2.0 * scale) as usize;
    if 2 * cut >= w.len() {
        return Vec::new();
    }
    w[cut..w.len() - cut].to_vec()
}

// --- modulation detection ---------------------------------------------------

/// Internal modulation tags: the reference uses the string "OOK" as a tag
/// distinct from "ASK"; both map to [`Modulation::Ask`] in the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetectedModulation {
    Ook,
    Ask,
    Fsk,
    Psk,
}

impl DetectedModulation {
    fn to_modulation(self) -> Modulation {
        match self {
            DetectedModulation::Ook | DetectedModulation::Ask => Modulation::Ask,
            DetectedModulation::Fsk => Modulation::Fsk,
            DetectedModulation::Psk => Modulation::Psk,
        }
    }
}

/// Detect the modulation of a signal chunk using wavelet variance statistics.
/// Returns `None` if no decision can be made (like the reference).
pub fn detect_modulation(
    data: &[Complex32],
    wavelet_scale: usize,
    median_filter_order: usize,
) -> Option<Modulation> {
    detect_modulation_tag(data, wavelet_scale, median_filter_order).map(|t| t.to_modulation())
}

pub fn detect_modulation_tag(
    data: &[Complex32],
    wavelet_scale: usize,
    median_filter_order: usize,
) -> Option<DetectedModulation> {
    use DetectedModulation::*;
    let n_data = data.len();
    let data: Vec<Complex64> = data
        .iter()
        .filter(|c| c.norm() > 0.0)
        .map(|c| Complex64::new(c.re as f64, c.im as f64))
        .collect();
    if data.is_empty() {
        return None;
    }

    if n_data - data.len() > 3 {
        return Some(Ook);
    }

    let max_abs = data.iter().map(|c| c.norm()).fold(0.0, f64::max);
    let data: Vec<Complex64> = data.iter().map(|c| c / max_abs).collect();

    let mag_wavlt: Vec<f64> = cwt_haar(&data, wavelet_scale)
        .iter()
        .map(|c| c.norm())
        .collect();
    if mag_wavlt.is_empty() {
        return None;
    }
    let norm_data: Vec<Complex64> = data.iter().map(|c| c / c.norm()).collect();
    let norm_mag_wavlt: Vec<f64> = cwt_haar(&norm_data, wavelet_scale)
        .iter()
        .map(|c| c.norm())
        .collect();

    let var_mag = variance(&mag_wavlt);
    let var_norm_mag = variance(&norm_mag_wavlt);
    let var_filtered_mag = {
        let filtered = median_filter(&mag_wavlt, median_filter_order);
        variance(&filtered.iter().map(|&v| v as f64).collect::<Vec<_>>())
    };
    let var_filtered_norm_mag = {
        let filtered = median_filter(&norm_mag_wavlt, median_filter_order);
        variance(&filtered.iter().map(|&v| v as f64).collect::<Vec<_>>())
    };

    if var_mag < 0.15
        && var_norm_mag < 0.15
        && var_filtered_mag < 0.15
        && var_filtered_norm_mag < 0.15
    {
        return Some(Ook);
    }

    if var_mag > 1.5 * var_norm_mag {
        // ASK or QAM
        Some(Ask)
    } else {
        // FSK or PSK
        if var_mag > 10.0 * var_filtered_mag {
            Some(Psk)
        } else {
            // FSK should show at least two peaks in the FFT
            let n_fft = 1usize << (data.len() as f64).log2() as usize;
            let mut fft: Vec<Complex64> = data[..n_fft].to_vec();
            fft_inplace(&mut fft, false);
            // fftshift of magnitudes
            let mut fft_mag: Vec<f64> = fft.iter().map(|c| c.norm()).collect();
            fft_mag.rotate_right(n_fft / 2);

            let mut indices: Vec<usize> = (0..fft_mag.len()).collect();
            indices.sort_by(|&a, &b| fft_mag[b].partial_cmp(&fft_mag[a]).unwrap());
            let ten_greatest_indices: Vec<usize> = indices.into_iter().take(10).collect();
            let greatest_index = ten_greatest_indices[0];
            let min_distance = 10.0f64;
            let min_freq = 100.0f64;

            let is_fsk = ten_greatest_indices.iter().any(|&i| {
                (i as f64 - greatest_index as f64).abs() >= min_distance && fft_mag[i] >= min_freq
            });
            if is_fsk {
                Some(Fsk)
            } else {
                Some(Ook)
            }
        }
    }
}

/// Detect the modulation for a set of message segments and return the most
/// common result over (up to) 100 messages.
pub fn detect_modulation_for_messages(
    signal: &[Complex32],
    message_indices: &[(usize, usize)],
) -> Option<DetectedModulation> {
    let max_messages = 100;
    let mut modulations = Vec::new();
    for &(start, end) in message_indices.iter().take(max_messages) {
        if let Some(m) = detect_modulation_tag(&signal[start..end], 4, 11) {
            modulations.push(m);
        }
    }
    most_common(&modulations)
}

/// Detect the center (decision threshold) of a demodulated (rectangular)
/// signal by finding the two most common levels in a histogram.
pub fn detect_center(rectangular_signal: &[f32], max_size: Option<usize>) -> Option<f32> {
    // do not consider noise
    let mut rect: Vec<f64> = rectangular_signal
        .iter()
        .filter(|&&v| v > -4.0)
        .map(|&v| v as f64)
        .collect();

    // Ignore the first and last 5% of samples (overshoot at start/end)
    let lo = (0.05 * rect.len() as f64) as usize;
    let hi = (0.95 * rect.len() as f64) as usize;
    if lo >= hi {
        return None;
    }
    rect = rect[lo..hi].to_vec();

    if let Some(max_size) = max_size {
        if rect.len() > max_size {
            rect.truncate(max_size);
        }
    }
    if rect.is_empty() {
        return None;
    }

    let hist_min = rect.iter().cloned().fold(f64::INFINITY, f64::min);
    let hist_max = rect.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    // The step size of the histogram is the variance of the signal
    let hist_step = variance(&rect);
    if !(hist_step > 0.0) || hist_max <= hist_min {
        // zero variance -> no center detectable
        return None;
    }

    // bins = np.arange(hist_min, hist_max + hist_step, hist_step)
    let mut edges: Vec<f64> = Vec::new();
    let mut edge = hist_min;
    while edge < hist_max + hist_step - f64::EPSILON * 4.0 * hist_step.max(1.0) {
        edges.push(edge);
        edge += hist_step;
    }
    if edges.len() < 2 {
        return None;
    }

    let y: Vec<usize> = {
        let mut counts = vec![0usize; edges.len() - 1];
        for &v in &rect {
            let mut idx = ((v - hist_min) / hist_step) as usize;
            if idx >= counts.len() {
                idx = counts.len() - 1;
            }
            counts[idx] += 1;
        }
        counts
    };

    let num_values = 2;
    let mut most_common_levels: Vec<f64> = Vec::new();
    let window_size = std::cmp::max(2, (0.05 * y.len() as f64) as usize + 1);

    let get_elem = |arr: &[usize], index: isize, default: usize| -> usize {
        if index >= 0 && (index as usize) < arr.len() {
            arr[index as usize]
        } else {
            default
        }
    };

    // indices sorted by count descending
    let mut sorted_indices: Vec<usize> = (0..y.len()).collect();
    sorted_indices.sort_by(|&a, &b| y[b].cmp(&y[a]).then(b.cmp(&a)));

    for index in sorted_indices {
        let i = index as isize;
        let mut is_local_max = true;
        for w in 1..=window_size as isize {
            if !(y[index] > get_elem(&y, i + w, 0) && y[index] > get_elem(&y, i - w, 0)) {
                is_local_max = false;
                break;
            }
        }
        if is_local_max {
            most_common_levels.push(edges[index]);
        }
        if most_common_levels.len() == num_values {
            break;
        }
    }

    if most_common_levels.is_empty() {
        return None;
    }

    Some(mean_f64(&most_common_levels) as f32)
}

/// Estimate the tolerance from plateau lengths (small plateaus below 5% of the
/// maximum are considered noise and get merged away).
pub fn estimate_tolerance_from_plateau_lengths(plateau_lengths: &[u64]) -> Option<u64> {
    if plateau_lengths.len() <= 1 {
        return None;
    }

    let mut unique = plateau_lengths.to_vec();
    unique.sort_unstable();
    unique.dedup();

    let maximum = max_without_outliers(
        &unique.iter().map(|&v| v as f64).collect::<Vec<_>>(),
        2.0,
    )?;

    let limit = 0.05 * maximum;
    if unique[0] > 1 && unique[0] as f64 >= limit {
        return Some(0);
    }

    let mut result = 0u64;
    for &value in &unique {
        if value > 1 && value as f64 >= limit {
            break;
        }
        result = value;
    }
    Some(result)
}

/// Merge plateau lengths using the given (or estimated) tolerance.
pub fn merge_plateau_lengths(plateau_lengths: &[u64], tolerance: Option<u64>) -> Vec<u64> {
    let tolerance = match tolerance {
        Some(t) => t,
        None => match estimate_tolerance_from_plateau_lengths(plateau_lengths) {
            Some(t) => t,
            None => return plateau_lengths.to_vec(),
        },
    };

    if tolerance == 0 {
        return plateau_lengths.to_vec();
    }

    merge_plateaus(plateau_lengths, tolerance, 10000)
}

/// Round plateau lengths to the next divisible number of the digit count of
/// the median digit count, e.g. 99 -> 100, 293 -> 300. Modifies in place.
pub fn round_plateau_lengths(plateau_lengths: &mut [u64]) {
    let digit_counts: Vec<f64> = plateau_lengths
        .iter()
        .map(|p| p.to_string().len() as f64)
        .collect();
    let n_digits = std::cmp::min(3, percentile_50(&digit_counts) as usize).max(1);
    let f = 10u64.pow(n_digits as u32 - 1);

    for p in plateau_lengths.iter_mut() {
        *p = ((*p as f64 / f as f64).round() as u64) * f;
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Tolerant greatest common divisor: gcd of each pair, then the most frequent
/// non-trivial one.
pub fn get_tolerant_greatest_common_divisor(numbers: &[u64]) -> u64 {
    let mut gcds = Vec::new();
    for i in 0..numbers.len() {
        for j in (i + 1)..numbers.len() {
            let g = gcd(numbers[i], numbers[j]);
            if g != 1 {
                gcds.push(g);
            }
        }
    }
    if gcds.is_empty() {
        return 1;
    }
    get_most_frequent_value(&gcds).unwrap_or(1)
}

pub fn get_bit_length_from_plateau_lengths(merged_plateau_lengths: &[u64]) -> usize {
    if merged_plateau_lengths.is_empty() {
        return 0;
    }
    if merged_plateau_lengths.len() == 1 {
        return merged_plateau_lengths[0] as usize;
    }

    let mut rounded = merged_plateau_lengths.to_vec();
    round_plateau_lengths(&mut rounded);

    let histogram = get_threshold_divisor_histogram(&rounded);
    if histogram.is_empty() {
        return 0;
    }

    // Can't simply take the argmax, since it could be a multiple of the
    // result (e.g. 2 1s are transmitted often)
    let mut sorted_indices: Vec<usize> = (0..histogram.len()).collect();
    sorted_indices.sort_by(|&a, &b| histogram[b].cmp(&histogram[a]).then(b.cmp(&a)));

    let max_count = histogram[sorted_indices[0]];
    let mut result = sorted_indices[0];

    for &idx in sorted_indices.iter().skip(1) {
        if (histogram[idx] as f64) < 0.25 * max_count as f64 {
            break;
        }
        if idx as f64 <= 0.5 * result as f64 {
            result = idx;
        }
    }
    result
}

// --- top level estimate ------------------------------------------------------

/// Detect the modulation of `iq` samples, segmenting the signal into messages
/// first (like `AutoInterpretation.estimate`).
pub fn detect_modulation_of_signal(iq: &[Complex32]) -> Option<Modulation> {
    let magnitudes: Vec<f32> = iq.iter().map(|c| c.norm()).collect();
    let noise = detect_noise_level(&magnitudes);
    let message_indices = segment_messages_from_magnitudes(&magnitudes, noise);
    detect_modulation_for_messages(iq, &message_indices).map(|t| t.to_modulation())
}

/// Full automatic interpretation of a complex IQ signal.
/// Port of `AutoInterpretation.estimate`.
pub fn estimate(iq: &[Complex32]) -> Option<Interpretation> {
    estimate_with_noise(iq, None)
}

/// High-level entry point (alias of [`estimate`]).
pub fn auto_detect(iq: &[Complex32]) -> Option<Interpretation> {
    estimate(iq)
}

pub fn estimate_with_noise(iq: &[Complex32], noise: Option<f32>) -> Option<Interpretation> {
    let magnitudes: Vec<f32> = iq.iter().map(|c| c.norm()).collect();

    // find noise threshold
    let noise = noise.unwrap_or_else(|| detect_noise_level(&magnitudes));

    // segment messages
    let mut message_indices = segment_messages_from_magnitudes(&magnitudes, noise);

    // detect modulation
    let detected = detect_modulation_for_messages(iq, &message_indices)?;
    let modulation = detected.to_modulation();

    if detected == DetectedModulation::Ook {
        message_indices = merge_message_segments_for_ook(&message_indices);
    }

    let data = crate::modulation::afp_demod(iq, noise, modulation, 2, 0.1);

    let mut centers: Vec<f64> = Vec::new();
    let mut bit_lengths: Vec<usize> = Vec::new();
    let mut tolerances: Vec<u64> = Vec::new();

    for &(start, end) in &message_indices {
        let msg_rect_data = &data[start..end];

        let center = match detect_center(msg_rect_data, None) {
            Some(c) => c,
            None => continue,
        };

        let plateau_lengths = get_plateau_lengths(msg_rect_data, center, 25.0);

        let tolerance = match estimate_tolerance_from_plateau_lengths(&plateau_lengths) {
            Some(t) => {
                tolerances.push(t);
                t
            }
            None => 0,
        };

        let merged_lengths = merge_plateau_lengths(&plateau_lengths, Some(tolerance));
        if merged_lengths.len() < 2 {
            continue;
        }

        let bit_length = get_bit_length_from_plateau_lengths(&merged_lengths);

        let min_bit_length = tolerance + 1;
        if bit_length > min_bit_length as usize {
            // only score bit lengths surpassing the minimum bit length
            centers.push(center as f64);
            bit_lengths.push(bit_length);
        }
    }

    let center = if modulation == Modulation::Ask {
        // for ask modulations the center tends to be the minimum of all found centers
        match min_without_outliers(&centers, 2.0) {
            Some(c) => c,
            None => return None,
        }
    } else if !centers.is_empty() {
        mean_f64(&centers)
    } else {
        return None;
    };

    let bit_length = get_most_frequent_value(&bit_lengths)?;

    let tolerance = if tolerances.is_empty() {
        // default to 5% of bit length
        std::cmp::max(1, (0.05 * bit_length as f64) as u32)
    } else {
        percentile_50(&tolerances.iter().map(|&t| t as f64).collect::<Vec<_>>()) as u32
    };

    Some(Interpretation {
        modulation,
        bit_length,
        center: center as f32,
        noise_threshold: noise,
        tolerance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn read_complex(path: &str) -> Vec<Complex32> {
        let bytes = std::fs::read(path).expect("test data file");
        bytes
            .chunks_exact(8)
            .map(|c| {
                Complex32::new(
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect()
    }

    pub(crate) fn data_path(name: &str) -> String {
        format!("{}/../urh-ref/tests/data/{}", env!("CARGO_MANIFEST_DIR"), name)
    }

    #[test]
    fn test_median_filter() {
        // window starts at i (reference quirk), sorted, element k//2
        let data = vec![5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(median_filter(&data, 3), vec![3.0, 2.0, 3.0, 4.0, 4.0]);

        let data = vec![1.0, 9.0, 2.0, 8.0, 3.0, 7.0];
        let filtered = median_filter(&data, 3);
        // i=0: [1,9,2] -> 2 ; i=1: [9,2,8] -> 8 ; i=2: [2,8,3] -> 3
        assert_eq!(filtered[..3], [2.0, 8.0, 3.0]);
    }

    #[test]
    fn test_k_means() {
        let (centers, clusters) = k_means(&[0.0, 0.0, 0.1, 1.0, 0.9], 2);
        let mut sorted: Vec<f32> = centers.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(sorted.len(), 2);
        assert!((sorted[0] - 0.0333).abs() < 0.01, "{:?}", centers);
        assert!((sorted[1] - 0.95).abs() < 0.01, "{:?}", centers);
        assert_eq!(clusters.iter().map(|c| c.len()).sum::<usize>(), 5);

        // fewer distinct values than k
        let (centers, _) = k_means(&[1.0, 1.0, 1.0], 2);
        assert_eq!(centers, vec![1.0]);
    }

    #[test]
    fn test_segment_messages_from_magnitudes() {
        let mut mags = vec![0.0f32; 100];
        for m in mags.iter_mut().take(40).skip(10) {
            *m = 1.0;
        }
        let segments = segment_messages_from_magnitudes(&mags, 0.5);
        // reference semantics: start is the last sample below noise before the
        // message, end the last sample above noise (verified against Python)
        assert_eq!(segments, vec![(9, 39)]);
    }

    #[test]
    fn test_threshold_divisor_histogram() {
        let hist = get_threshold_divisor_histogram(&[100, 100, 50]);
        // 100 vs 100: 100/100 - 1 = 0 < 0.2 -> histogram[100] += 1
        // 100 vs 50: 2.0 - 2 = 0 < 0.2 -> histogram[50] += 2
        assert_eq!(hist[50], 2);
        assert_eq!(hist[100], 1);
    }

    // --- ported from tests/auto_interpretation/test_bit_length_detection.py ---

    fn run_merge(data: &[u64]) -> Vec<u64> {
        merge_plateau_lengths(data, None)
    }

    #[test]
    fn test_merge_plateau_lengths() {
        assert_eq!(merge_plateau_lengths(&[], None), Vec::<u64>::new());
        assert_eq!(merge_plateau_lengths(&[42], None), vec![42]);
        assert_eq!(
            merge_plateau_lengths(&[100, 100, 100], None),
            vec![100, 100, 100]
        );
        assert_eq!(run_merge(&[100, 49, 1, 50, 100]), vec![100, 100, 100]);
        assert_eq!(run_merge(&[100, 48, 2, 50, 100]), vec![100, 100, 100]);
        assert_eq!(run_merge(&[100, 100, 67, 1, 10, 1, 21]), vec![100, 100, 100]);
        assert_eq!(
            run_merge(&[100, 100, 67, 1, 10, 1, 21, 100, 50, 1, 49]),
            vec![100, 100, 100, 100, 100]
        );
    }

    #[test]
    fn test_estimate_tolerance_from_plateau_lengths() {
        assert_eq!(estimate_tolerance_from_plateau_lengths(&[]), None);
        assert_eq!(estimate_tolerance_from_plateau_lengths(&[10]), None);
        assert_eq!(
            estimate_tolerance_from_plateau_lengths(&[100, 49, 1, 50, 100]),
            Some(1)
        );
        assert_eq!(
            estimate_tolerance_from_plateau_lengths(&[100, 49, 2, 50, 100]),
            Some(2)
        );
        assert_eq!(
            estimate_tolerance_from_plateau_lengths(&[100, 49, 2, 50, 100, 1]),
            Some(2)
        );
        assert_eq!(
            estimate_tolerance_from_plateau_lengths(&[8, 8, 6, 1, 1]),
            Some(1)
        );
    }

    #[test]
    fn test_tolerant_greatest_common_divisor() {
        assert_eq!(get_tolerant_greatest_common_divisor(&[]), 1);
        assert_eq!(get_tolerant_greatest_common_divisor(&[22]), 1);
        assert_eq!(get_tolerant_greatest_common_divisor(&[10, 5, 5]), 5);
        assert_eq!(get_tolerant_greatest_common_divisor(&[10, 4, 5]), 5);
        assert_eq!(get_tolerant_greatest_common_divisor(&[100, 50, 49]), 50);
        assert_eq!(get_tolerant_greatest_common_divisor(&[6, 3, 7]), 3);
        assert_eq!(get_tolerant_greatest_common_divisor(&[100, 50, 49, 9]), 50);
    }

    #[test]
    fn test_round_plateau_lengths() {
        // median digit count 2.5 -> int 2 -> round to tens
        let mut p = vec![100, 99, 293, 42];
        round_plateau_lengths(&mut p);
        assert_eq!(p, vec![100, 100, 290, 40]);

        // median digit count 3 -> round to hundreds
        let mut p = vec![100, 293, 350, 42];
        round_plateau_lengths(&mut p);
        assert_eq!(p, vec![100, 300, 400, 0]);
    }

    #[test]
    fn test_get_bit_length_from_plateau_lengths() {
        assert_eq!(get_bit_length_from_plateau_lengths(&[]), 0);
        assert_eq!(get_bit_length_from_plateau_lengths(&[42]), 42);
        // 100s and 50s: bit length 50
        assert_eq!(
            get_bit_length_from_plateau_lengths(&[100, 100, 50, 100, 50, 50]),
            50
        );
    }

    // --- real data: modulation detection (test_modulation_detection.py) ------

    #[test]
    fn test_fsk_detection() {
        let fsk_signal = &read_complex(&data_path("fsk.complex"))[5..15000];
        let m = detect_modulation(fsk_signal, 4, 7);
        assert_eq!(m, Some(Modulation::Fsk));
    }

    #[test]
    fn test_ook_detection() {
        let data = read_complex(&data_path("ask.complex"));
        assert_eq!(detect_modulation(&data, 4, 11), Some(Modulation::Ask));

        let data = read_complex(&data_path("ASK_mod.complex"));
        assert_eq!(detect_modulation(&data, 4, 11), Some(Modulation::Ask));
    }

    // --- real data: full estimation (test_auto_interpretation_integration.py) -

    #[test]
    fn test_auto_interpretation_fsk() {
        let fsk_signal = read_complex(&data_path("fsk.complex"));
        let result = estimate(&fsk_signal).expect("estimation result");
        assert_eq!(result.modulation, Modulation::Fsk);
        assert_eq!(result.bit_length, 100);
        assert!(result.tolerance > 0);
        assert!(result.tolerance <= 5);
    }

    #[test]
    fn test_auto_interpretation_ask() {
        let ask_signal = read_complex(&data_path("ask.complex"));
        let result = estimate(&ask_signal).expect("estimation result");
        assert_eq!(result.modulation, Modulation::Ask);
        assert_eq!(result.bit_length, 300);
        assert!(result.tolerance > 0);
        assert!(result.tolerance <= 6);
    }

    #[test]
    fn test_auto_detect_ask_mod() {
        let data = read_complex(&data_path("ASK_mod.complex"));
        let result = auto_detect(&data).expect("estimation result");
        // ASK_mod.complex is an OOK signal; reference reports it as "ASK"
        assert_eq!(result.modulation, Modulation::Ask);
        assert!(result.bit_length > 0);
    }
}


