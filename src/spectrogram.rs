//! Spectrogram — port of `urh/signalprocessing/Spectrogram.py`.
//!
//! Short-time Fourier transform with a Hann window (`np.hanning`, symmetric),
//! overlap between windows, frequency-axis fftshift and dB conversion of the
//! magnitudes. The result of [`Spectrogram::data`] is the time x frequency
//! dB matrix, where row `t` column `j` corresponds to the DFT bin
//! `(freq_bins - 1 - j + freq_bins / 2) % freq_bins` (fftshift + `fliplr`,
//! exactly like the reference `__calculate_spectrogram`).

use crate::iq::arr2decibel;
use crate::Complex32;

pub const MAX_LINES_PER_VIEW: usize = 1000;
pub const DEFAULT_FFT_WINDOW_SIZE: usize = 1024;

pub struct Spectrogram {
    samples: Vec<Complex32>,
    window_size: usize,
    /// Value between 0 (no overlapping) and 1 (full overlapping) of windows.
    overlap_factor: f64,
    /// dB range mapped when rendering (kept for API parity with the
    /// reference's `data_min` / `data_max` attributes).
    pub data_min: f32,
    pub data_max: f32,
}

impl Spectrogram {
    pub fn new(samples: &[Complex32], window_size: usize, overlap_factor: f64) -> Self {
        Spectrogram {
            samples: if samples.is_empty() {
                vec![Complex32::default()]
            } else {
                samples.to_vec()
            },
            window_size: window_size.max(1),
            overlap_factor: overlap_factor.clamp(0.0, 1.0),
            data_min: -140.0,
            data_max: 10.0,
        }
    }

    pub fn samples(&self) -> &[Complex32] {
        &self.samples
    }

    pub fn window_size(&self) -> usize {
        self.window_size
    }

    pub fn overlap_factor(&self) -> f64 {
        self.overlap_factor
    }

    pub fn time_bins(&self) -> usize {
        (self.samples.len() as f64 / self.hop_size() as f64).ceil() as usize
    }

    pub fn freq_bins(&self) -> usize {
        self.window_size
    }

    /// hop size determines by how many samples the window is advanced
    pub fn hop_size(&self) -> usize {
        self.window_size - (self.overlap_factor * self.window_size as f64) as usize
    }

    /// Symmetric Hann window (`np.hanning`).
    pub fn window(&self) -> Vec<f32> {
        let m = self.window_size;
        if m == 1 {
            return vec![1.0];
        }
        (0..m)
            .map(|i| {
                (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (m - 1) as f64).cos())
                    as f32
            })
            .collect()
    }

    /// Short-time Fourier transform of the signal, one row per frame
    /// (frame length = window size), each FFT normalized by the window
    /// size like the reference `stft`.
    pub fn stft(&self) -> Vec<Vec<Complex32>> {
        let ws = self.window_size;
        let hop = self.hop_size();
        let window = self.window();

        let mut samples = self.samples.clone();
        if samples.len() < ws {
            samples.resize(ws, Complex32::default());
        }
        let num_frames = ((samples.len() - ws) / hop + 1).max(1);

        (0..num_frames)
            .map(|f| {
                let frame: Vec<Complex32> = samples[f * hop..f * hop + ws]
                    .iter()
                    .zip(&window)
                    .map(|(s, w)| s * *w)
                    .collect();
                let mut spectrum = fft(&frame);
                for c in &mut spectrum {
                    *c /= ws as f32;
                }
                spectrum
            })
            .collect()
    }

    /// The spectrogram as time x frequency dB matrix (fftshift on the
    /// frequency axis, then flipped left-right, like the reference).
    pub fn data(&self) -> Vec<Vec<f32>> {
        let n = self.freq_bins();
        let stft = self.stft();
        stft
            .into_iter()
            .map(|row| {
                // fftshift on the frequency axis: np.fft.fftshift rolls the
                // DFT bins left by n/2 (correct for odd and even n)
                let mut shifted = row;
                shifted.rotate_left(n / 2);
                let db = arr2decibel(&shifted);
                // fliplr so the frequency axis runs from +fs/2 down to -fs/2
                db.into_iter().rev().collect()
            })
            .collect()
    }
}

/// Radix-2 FFT for power-of-two lengths, naive DFT otherwise.
fn fft(input: &[Complex32]) -> Vec<Complex32> {
    let n = input.len();
    if n <= 1 {
        return input.to_vec();
    }
    if n.is_power_of_two() {
        let mut a = input.to_vec();
        fft_radix2(&mut a);
        return a;
    }
    // naive O(n^2) fallback keeps arbitrary window sizes correct
    (0..n)
        .map(|k| {
            let mut acc = Complex32::default();
            for (j, &x) in input.iter().enumerate() {
                let angle = -2.0 * std::f64::consts::PI * (k * j) as f64 / n as f64;
                acc += x * Complex32::new(angle.cos() as f32, angle.sin() as f32);
            }
            acc
        })
        .collect()
}

/// In-place iterative radix-2 Cooley-Tukey FFT (decimation in time).
fn fft_radix2(a: &mut [Complex32]) {
    let n = a.len();
    debug_assert!(n.is_power_of_two());
    // bit-reversal permutation
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = (i as u32).reverse_bits() >> (u32::BITS - bits);
        if i < j as usize {
            a.swap(i, j as usize);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        let wlen = Complex32::new(ang.cos() as f32, ang.sin() as f32);
        let mut start = 0;
        while start < n {
            let mut w = Complex32::new(1.0, 0.0);
            for k in 0..len / 2 {
                let u = a[start + k];
                let v = a[start + k + len / 2] * w;
                a[start + k] = u + v;
                a[start + k + len / 2] = u - v;
                w *= wlen;
            }
            start += len;
        }
        len *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iq::IqArray;

    const DATA_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/");

    // Port of the shape checks of tests/SpectrogramTest.py
    #[test]
    fn test_shapes_and_hop_size() {
        let samples = vec![Complex32::default(); 10_000];
        let s = Spectrogram::new(&samples, 1024, 0.5);
        assert_eq!(s.hop_size(), 512);
        assert_eq!(s.freq_bins(), 1024);
        assert_eq!(s.time_bins(), 10_000usize.div_ceil(512));

        let stft = s.stft();
        // num_frames = (len - ws) // hop + 1 (no padding beyond window size)
        assert_eq!(stft.len(), (10_000 - 1024) / 512 + 1);
        assert!(stft.iter().all(|row| row.len() == 1024));

        let data = s.data();
        assert_eq!(data.len(), stft.len());
        assert!(data.iter().all(|row| row.len() == 1024));
        // zero input: all dB values are the dB of 0 = -inf (reference yields
        // -inf as well); use a nonzero signal for finiteness below
    }

    #[test]
    fn test_short_signal_padded_to_one_frame() {
        let samples: Vec<Complex32> = (0..100)
            .map(|i| Complex32::new(i as f32 * 0.01, 0.0))
            .collect();
        let s = Spectrogram::new(&samples, 1024, 0.5);
        let stft = s.stft();
        assert_eq!(stft.len(), 1);
        assert_eq!(stft[0].len(), 1024);
    }

    #[test]
    fn test_hann_window() {
        let s = Spectrogram::new(&[Complex32::default()], 8, 0.5);
        let w = s.window();
        assert_eq!(w.len(), 8);
        assert!((w[0]).abs() < 1e-6, "symmetric hann starts at 0");
        assert!((w[7]).abs() < 1e-6, "symmetric hann ends at 0");
        // w[i] = 0.5 - 0.5*cos(2*pi*i/(m-1))
        let expect = |i: usize| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / 7.0).cos();
        for i in 1..7 {
            assert!((w[i] as f64 - expect(i)).abs() < 1e-5);
        }
        assert!((w[3] - w[4]).abs() < 1e-6, "symmetric around the center");
        assert!((w.iter().sum::<f32>() - 3.5).abs() < 1e-2, "np.hanning(8) sums to ~3.5");
    }

    #[test]
    fn test_single_tone_peak_bin() {
        let n = 256;
        // complex exponential at f = 0.25 (a quarter of the sample rate)
        let samples: Vec<Complex32> = (0..n)
            .map(|i| {
                let phase = 2.0 * std::f64::consts::PI * 0.25 * i as f64;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        let s = Spectrogram::new(&samples, n, 0.5);
        let data = s.data();
        assert_eq!(data.len(), 1);

        let peak_j = data[0]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        // output column j maps to DFT bin (n - 1 - j + n/2) % n
        let bin_of = |j: usize| (n - 1 - j + n / 2) % n;
        let peak_bin = bin_of(peak_j);
        // expected DFT bin for f = 0.25 is n/4 = 64
        assert!(
            (peak_bin as i64 - 64).abs() <= 1,
            "peak bin {peak_bin} (col {peak_j}) not near 64"
        );

        // negative frequency tone lands in the mirrored bin
        let samples_neg: Vec<Complex32> = (0..n)
            .map(|i| {
                let phase = -2.0 * std::f64::consts::PI * 0.125 * i as f64;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        let s2 = Spectrogram::new(&samples_neg, n, 0.5);
        let d2 = s2.data();
        let peak_j2 = d2[0]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        let peak_bin2 = bin_of(peak_j2);
        // f = -0.125 -> DFT bin n - n/8 = 224
        assert!(
            (peak_bin2 as i64 - 224).abs() <= 1,
            "peak bin {peak_bin2} (col {peak_j2}) not near 224"
        );
    }

    #[test]
    fn test_db_range() {
        let n = 1024;
        let samples: Vec<Complex32> = (0..n)
            .map(|i| {
                let phase = 2.0 * std::f64::consts::PI * 0.1 * i as f64;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        let s = Spectrogram::new(&samples, n, 0.5);
        let data = s.data();
        let all: Vec<f32> = data.iter().flatten().copied().collect();
        assert!(all.iter().all(|v| v.is_finite()));
        let max = all.iter().cloned().fold(f32::MIN, f32::max);
        // peak of a unit-amplitude windowed tone after /window_size is
        // around hann gain 0.5 -> about -6 dB
        assert!(max > -12.0 && max < 0.0, "unexpected peak level {max}");
        // off-peak bins are far below the peak (window sidelobes ~ -31 dB)
        assert!(all.iter().cloned().fold(f32::MAX, f32::min) < max - 20.0);
    }

    #[test]
    fn test_ask_complex_smoke() {
        let arr = IqArray::from_file(&format!("{DATA_DIR}ask.complex")).expect("read ask.complex");
        let s = Spectrogram::new(arr.as_slice(), 1024, 0.5);
        let data = s.data();
        assert_eq!(data.len(), (arr.len() - 1024) / 512 + 1);
        assert!(data.iter().all(|row| row.len() == 1024));
        for row in &data {
            for v in row {
                assert!(v.is_finite(), "non-finite dB value");
                assert!(*v <= 10.0 && *v > -500.0);
            }
        }
    }
}
