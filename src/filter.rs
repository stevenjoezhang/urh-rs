//! FIR/IIR filtering, ported from URH's `cythonext/signal_functions.pyx`
//! (`fir_filter` / `iir_filter`) and the small `signalprocessing/Filter.py`
//! wrapper types.

use num_complex::Complex32;

/// FIR convolution with the exact semantics of `signal_functions.pyx fir_filter`:
/// the full convolution is accumulated into a buffer of length `N + M - 1`, and
/// only the first `N` samples (the causal part) are returned.
pub fn fir_filter(input: &[Complex32], taps: &[Complex32]) -> Vec<Complex32> {
    let n = input.len();
    let m = taps.len();
    let mut output = vec![Complex32::default(); n + m.saturating_sub(1)];
    for (i, &sample) in input.iter().enumerate() {
        for (j, &tap) in taps.iter().enumerate() {
            output[i + j] += sample * tap;
        }
    }
    output.truncate(n);
    output
}

/// Direct-form transversal IIR filter with the exact semantics of
/// `signal_functions.pyx iir_filter`: output samples with index below
/// `max(len(a), len(b) + 1)` stay zero, and the recursion references the
/// *previous* output sample `result[n - 1 - k]` (note the off-by-one relative
/// to the textbook form, faithfully reproduced from the reference).
pub fn iir_filter(a: &[f64], b: &[f64], signal: &[f64]) -> Vec<f64> {
    let mut result = vec![0.0f64; signal.len()];
    let m = a.len();
    let n = b.len();
    for idx in m.max(n + 1)..signal.len() {
        let mut acc = 0.0;
        for (j, &aj) in a.iter().enumerate() {
            acc += aj * signal[idx - j];
        }
        for (k, &bk) in b.iter().enumerate() {
            acc += bk * result[idx - 1 - k];
        }
        result[idx] = acc;
    }
    result
}

/// Filter classification, ported from `Filter.py FilterType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterType {
    MovingAverage,
    DcCorrection,
    Custom,
}

impl FilterType {
    pub fn as_str(self) -> &'static str {
        match self {
            FilterType::MovingAverage => "moving average",
            FilterType::DcCorrection => "DC correction",
            FilterType::Custom => "custom",
        }
    }
}

/// A configured filter, ported from `Filter.py Filter`.
#[derive(Debug, Clone)]
pub struct Filter {
    pub filter_type: FilterType,
    pub taps: Vec<Complex32>,
}

impl Filter {
    pub fn new(taps: Vec<Complex32>, filter_type: FilterType) -> Self {
        Filter { filter_type, taps }
    }

    /// Uniform moving-average filter of `n` taps (URH's bandpass pre-filter).
    pub fn moving_average(n: usize) -> Self {
        Filter {
            filter_type: FilterType::MovingAverage,
            taps: vec![Complex32::new(1.0 / n as f32, 0.0); n],
        }
    }

    /// `Filter.work`: DC correction subtracts the per-component mean, everything
    /// else is an FIR pass.
    pub fn work(&self, input_signal: &[Complex32]) -> Vec<Complex32> {
        if self.filter_type == FilterType::DcCorrection {
            let n = input_signal.len().max(1) as f32;
            let mean_re: f32 = input_signal.iter().map(|s| s.re).sum::<f32>() / n;
            let mean_im: f32 = input_signal.iter().map(|s| s.im).sum::<f32>() / n;
            input_signal
                .iter()
                .map(|s| Complex32::new(s.re - mean_re, s.im - mean_im))
                .collect()
        } else {
            self.apply_fir_filter(input_signal)
        }
    }

    /// `Filter.apply_fir_filter` for already-complex input.
    pub fn apply_fir_filter(&self, input_signal: &[Complex32]) -> Vec<Complex32> {
        fir_filter(input_signal, &self.taps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex;

    #[test]
    fn test_fir_rectangular() {
        // input [1,1,1,0,0], taps [1,1,1]:
        // full conv = [1,2,3,2,1,0,0], output keeps first N=5
        let input: Vec<Complex32> = [1.0, 1.0, 1.0, 0.0, 0.0]
            .iter()
            .map(|&v| Complex::new(v, 0.0))
            .collect();
        let taps: Vec<Complex32> = [1.0, 1.0, 1.0]
            .iter()
            .map(|&v| Complex::new(v, 0.0))
            .collect();
        let out = fir_filter(&input, &taps);
        let expected = [1.0, 2.0, 3.0, 2.0, 1.0];
        assert_eq!(out.len(), 5);
        for (o, &e) in out.iter().zip(expected.iter()) {
            assert!((o.re - e).abs() < 1e-6);
            assert!(o.im.abs() < 1e-6);
        }
    }

    #[test]
    fn test_fir_complex_and_edges() {
        let input = vec![Complex::new(1.0f32, 2.0), Complex::new(-1.0, 0.5)];
        let taps = vec![Complex::new(0.5f32, 0.5), Complex::new(1.0, 0.0)];
        let out = fir_filter(&input, &taps);
        assert_eq!(out.len(), 2);
        assert!((out[0].re - -0.5).abs() < 1e-6 && (out[0].im - 1.5).abs() < 1e-6);
        assert!((out[1].re - 0.25).abs() < 1e-6 && (out[1].im - 1.75).abs() < 1e-6);

        // empty input / empty taps
        assert!(fir_filter(&[], &taps).is_empty());
        let out = fir_filter(&input, &[]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|c| *c == Complex32::default()));
    }

    #[test]
    fn test_fir_moving_average_filter_type() {
        let f = Filter::moving_average(4);
        assert_eq!(f.filter_type, FilterType::MovingAverage);
        let input: Vec<Complex32> = [4.0f32, 8.0, 12.0, 16.0, 20.0]
            .iter()
            .map(|&v| Complex::new(v, 0.0))
            .collect();
        let out = f.work(&input);
        // causal moving average: [1, 3, 6, 10, 14]
        let expected = [1.0f32, 3.0, 6.0, 10.0, 14.0];
        for (o, &e) in out.iter().zip(expected.iter()) {
            assert!((o.re - e).abs() < 1e-5, "got {} want {}", o.re, e);
        }
    }

    #[test]
    fn test_filter_dc_correction() {
        let f = Filter::new(vec![], FilterType::DcCorrection);
        let input: Vec<Complex32> = [1.0f32, 2.0, 3.0]
            .iter()
            .map(|&v| Complex::new(v, 10.0))
            .collect();
        let out = f.work(&input);
        let expected = [-1.0f32, 0.0, 1.0];
        for (o, &e) in out.iter().zip(expected.iter()) {
            assert!((o.re - e).abs() < 1e-6);
            assert!(o.im.abs() < 1e-6);
        }
    }

    #[test]
    fn test_iir_filter() {
        // a = [1], b = [0.5]: y[n] = x[n] + 0.5 * y[n-1], warmup until n >= 2
        let a = [1.0f64];
        let b = [0.5f64];
        let signal = [0.0f64, 0.0, 1.0, 1.0, 1.0, 1.0];
        let out = iir_filter(&a, &b, &signal);
        let expected = [0.0, 0.0, 1.0, 1.5, 1.75, 1.875];
        assert_eq!(out.len(), expected.len());
        for (o, &e) in out.iter().zip(expected.iter()) {
            assert!((o - e).abs() < 1e-12);
        }
    }

    #[test]
    fn test_iir_filter_warmup_and_ma() {
        // M=2, N=0: samples 0 and 1 stay zero (warmup region max(2, 1) = 2),
        // then y[n] = a0*x[n] + a1*x[n-1]
        let a = [1.0f64, 1.0];
        let b: [f64; 0] = [];
        let signal = [9.0, 9.0, 3.0, 4.0, 5.0];
        let out = iir_filter(&a, &b, &signal);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
        // y[n] = x[n] + x[n-1] with the reference's signal[n-j] indexing
        assert_eq!(out[2], 12.0);
        assert_eq!(out[3], 7.0);
        assert_eq!(out[4], 9.0);
    }
}
