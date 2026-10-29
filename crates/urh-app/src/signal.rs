//! Per-signal state: raw samples plus the min/max level-of-detail (LOD) index
//! that answers waveform envelope queries.
//!
//! The LOD stores, for every block of [`BLOCK_SIZE`] consecutive samples, the
//! exact minimum and maximum of I, Q and the magnitude |s|. Reducing a query
//! range then costs a few block-table lookups plus raw scans of the (at most
//! two) partially covered blocks at the range edges — no sample is ever
//! skipped, so peaks cannot be lost to decimation (contract requirement 1).
//! The magnitude extrema live in their own LOD columns: they cannot be derived
//! from the I/Q block extrema (max |s| != sqrt(max I^2 + max Q^2)).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use urh::iq::{IqArray, IqFormat};
use urh::Complex32;

use crate::WaveformMode;

/// Samples per LOD block (contract: "block = 256 samples").
pub(crate) const BLOCK_SIZE: usize = 256;

/// Minimum and maximum of the six envelope components over a block or bin.
///
/// Serves double duty as the LOD table entry and the per-bin query result.
/// Fields of the component a query does not need are left at the [`Extrema::of_sample`]
/// initializer value and are simply never read by the frame encoder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Extrema {
    pub(crate) min_i: f32,
    pub(crate) max_i: f32,
    pub(crate) min_q: f32,
    pub(crate) max_q: f32,
    pub(crate) min_m: f32,
    pub(crate) max_m: f32,
}

impl Extrema {
    /// Initializer from the first sample of a range: both bounds start there.
    ///
    /// The magnitude bounds are only initialized when `need_mag` is set — the
    /// I/Q comparisons are cheap, but `norm()` (an `hypot`) is not, and I/Q
    /// queries must not pay for a magnitude they never serialize.
    fn of_sample(s: &Complex32, need_mag: bool) -> Self {
        let mut e = Extrema {
            min_i: s.re,
            max_i: s.re,
            min_q: s.im,
            max_q: s.im,
            min_m: 0.0,
            max_m: 0.0,
        };
        if need_mag {
            let m = s.norm();
            e.min_m = m;
            e.max_m = m;
        }
        e
    }

    /// Fold one sample into the bounds (same `need_mag` convention as
    /// [`Extrema::of_sample`]). Comparisons instead of `f32::min`/`max` so
    /// NaN samples are consistently dropped rather than propagated.
    fn absorb(&mut self, s: &Complex32, need_mag: bool) {
        if s.re < self.min_i {
            self.min_i = s.re;
        }
        if s.re > self.max_i {
            self.max_i = s.re;
        }
        if s.im < self.min_q {
            self.min_q = s.im;
        }
        if s.im > self.max_q {
            self.max_q = s.im;
        }
        if need_mag {
            let m = s.norm();
            if m < self.min_m {
                self.min_m = m;
            }
            if m > self.max_m {
                self.max_m = m;
            }
        }
    }

    /// Fold another range's extrema into `self` (union of the two ranges).
    fn merge(&mut self, other: &Extrema, need_mag: bool) {
        if other.min_i < self.min_i {
            self.min_i = other.min_i;
        }
        if other.max_i > self.max_i {
            self.max_i = other.max_i;
        }
        if other.min_q < self.min_q {
            self.min_q = other.min_q;
        }
        if other.max_q > self.max_q {
            self.max_q = other.max_q;
        }
        if need_mag {
            if other.min_m < self.min_m {
                self.min_m = other.min_m;
            }
            if other.max_m > self.max_m {
                self.max_m = other.max_m;
            }
        }
    }
}

/// Build the block LOD: one [`Extrema`] per (possibly short) trailing block.
fn build_lod(samples: &[Complex32]) -> Vec<Extrema> {
    samples
        .chunks(BLOCK_SIZE)
        .map(|chunk| {
            // chunks() never yields an empty chunk, so chunk[0] exists.
            let mut e = Extrema::of_sample(&chunk[0], true);
            for s in &chunk[1..] {
                e.absorb(s, true);
            }
            e
        })
        .collect()
}

/// An open signal: samples held fully in memory (MVP decision, ADR-002) plus
/// the derived query structures. Sample rate is atomic because
/// [`crate::SignalRepository::set_sample_rate`] mutates it through a shared
/// `Arc` while readers run lock-free.
pub(crate) struct Signal {
    signal_id: String,
    path: String,
    display_name: String,
    format: &'static str,
    samples: Vec<Complex32>,
    lod: Vec<Extrema>,
    /// `f64::to_bits` of the sample rate; always finite and positive.
    sample_rate_bits: AtomicU64,
}

impl Signal {
    /// Build from an already loaded [`IqArray`]; derives display metadata from
    /// `path` and computes the block LOD in one pass.
    pub(crate) fn new(signal_id: String, path: &str, iq: IqArray, sample_rate: f64) -> Self {
        let samples = iq.into_vec();
        let lod = build_lod(&samples);
        Signal {
            signal_id,
            path: path.to_string(),
            display_name: file_name_of(path),
            format: format_string(IqFormat::from_filename(path)),
            lod,
            sample_rate_bits: AtomicU64::new(sample_rate.to_bits()),
            samples,
        }
    }

    pub(crate) fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Update the metadata-only sample rate (Hz).
    pub(crate) fn set_sample_rate(&self, rate: f64) {
        self.sample_rate_bits
            .store(rate.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn meta(&self) -> crate::SignalMeta {
        let sample_count = self.samples.len() as u64;
        let sample_rate = f64::from_bits(self.sample_rate_bits.load(Ordering::Relaxed));
        crate::SignalMeta {
            signal_id: self.signal_id.clone(),
            path: self.path.clone(),
            display_name: self.display_name.clone(),
            format: self.format.to_string(),
            sample_count,
            sample_rate,
            duration_secs: sample_count as f64 / sample_rate,
        }
    }

    /// Exact extrema over the half-open range `[a, b)` (requires `a < b`).
    ///
    /// Narrow ranges (bin width below two blocks) are scanned from the raw
    /// samples directly — the LOD could not save any work there. Wider ranges
    /// raw-scan only the partially covered head and tail blocks and take the
    /// fully covered inner blocks from the LOD table.
    fn extrema_over(&self, a: usize, b: usize, need_mag: bool) -> Extrema {
        debug_assert!(a < b, "extrema_over requires a non-empty range");
        if b - a < 2 * BLOCK_SIZE {
            return self.scan_raw(a, b, need_mag);
        }
        // With b - a >= 2 * BLOCK_SIZE the head cannot reach past the tail:
        // head_end <= a + 255 < b - 255 <= tail_start.
        let head_end = a.div_ceil(BLOCK_SIZE) * BLOCK_SIZE;
        let tail_start = b / BLOCK_SIZE * BLOCK_SIZE;
        let mut acc: Option<Extrema> = None;
        if a < head_end {
            acc = Some(self.scan_raw(a, head_end, need_mag));
        }
        for block in head_end / BLOCK_SIZE..tail_start / BLOCK_SIZE {
            let entry = self.lod[block];
            acc = Some(match acc {
                None => entry,
                Some(mut merged) => {
                    merged.merge(&entry, need_mag);
                    merged
                }
            });
        }
        if tail_start < b {
            let tail = self.scan_raw(tail_start, b, need_mag);
            acc = Some(match acc {
                None => tail,
                Some(mut merged) => {
                    merged.merge(&tail, need_mag);
                    merged
                }
            });
        }
        // A non-empty range always yields at least one non-empty fragment
        // (head, inner blocks or tail); see the argument above.
        acc.expect("non-empty range produced at least one fragment")
    }

    /// Straight min/max pass over the raw samples of `[a, b)`.
    fn scan_raw(&self, a: usize, b: usize, need_mag: bool) -> Extrema {
        debug_assert!(a < b, "scan_raw requires a non-empty range");
        let mut e = Extrema::of_sample(&self.samples[a], need_mag);
        for s in &self.samples[a + 1..b] {
            e.absorb(s, need_mag);
        }
        e
    }

    /// Per-bin extrema for the viewport `[start, end)` with `bins` requested
    /// bins, following the contract's uniform-in-sample-space bin grid.
    ///
    /// Bin `k` covers `[start + k*w, min(start + (k+1)*w, end))` with
    /// `w = ceil(span / n)`; when `bins >= span` the effective count `n` is
    /// clamped to `span`, which degenerates to one bin per sample with
    /// `min == max ==` the sample value (contract requirement 2).
    ///
    /// Because `w` rounds *up*, the grid can overshoot: trailing bins whose
    /// start index already reaches `end` cover no samples at all (e.g.
    /// `span = 4, bins = 3` gives `w = 2` and an empty third bin — the
    /// contract only says "the last bin may be narrower", it does not cover
    /// this case). The result still carries exactly `n` bins; an empty
    /// trailing bin repeats the previous bin's values so a renderer never
    /// sees a fabricated dip to zero and no peak is invented.
    pub(crate) fn envelope(
        &self,
        start: usize,
        end: usize,
        bins: usize,
        mode: WaveformMode,
    ) -> Vec<Extrema> {
        debug_assert!(start < end, "envelope requires a non-empty viewport");
        let need_mag = mode == WaveformMode::Amplitude;
        let span = end - start;
        let n = bins.min(span);
        let w = span.div_ceil(n);
        let mut out: Vec<Extrema> = Vec::with_capacity(n);
        let mut cursor = start;
        for _ in 0..n {
            if cursor >= end {
                // Bin 0 always covers `start` (n <= span), so a predecessor
                // exists whenever we get here.
                out.push(*out.last().expect("bin 0 always covers start"));
                continue;
            }
            // (end - cursor).min(w) cannot overflow: cursor < end here.
            let bin_end = cursor + (end - cursor).min(w);
            out.push(self.extrema_over(cursor, bin_end, need_mag));
            cursor = bin_end;
        }
        out
    }
}

/// File name component of a path, or the whole path if it has none
/// (e.g. ends in `..`).
fn file_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Contract format string for an on-disk IQ format.
fn format_string(format: IqFormat) -> &'static str {
    match format {
        IqFormat::ComplexF32 => "complex",
        IqFormat::Complex16S => "complex16s",
        IqFormat::Complex16U => "complex16u",
        IqFormat::Complex32S => "complex32s",
        IqFormat::Complex32U => "complex32u",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic LCG (Numerical Recipes constants) — the repo avoids a
    /// rand dependency in tests.
    struct Lcg(u64);

    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }

        /// Sample in [-1, 1) per component. The 24-bit integer -> f32 -> /2^23
        /// -> -1 chain is exact in f32, so values are reproducible everywhere.
        fn next_sample(&mut self) -> Complex32 {
            let re = (self.next_u64() >> 40) as f32 / 8_388_608.0 - 1.0;
            let im = (self.next_u64() >> 40) as f32 / 8_388_608.0 - 1.0;
            Complex32::new(re, im)
        }
    }

    fn lcg_samples(n: usize) -> Vec<Complex32> {
        let mut gen = Lcg(0x243F_6A88_85A3_08D3);
        (0..n).map(|_| gen.next_sample()).collect::<Vec<_>>()
    }

    fn test_signal(samples: &[Complex32]) -> Signal {
        Signal::new(
            "sig-1".to_string(),
            "test.complex",
            IqArray::from_vec(samples.to_vec()),
            1e6,
        )
    }

    /// Naive single-pass reference over a raw range (all six components).
    fn naive_range(samples: &[Complex32], a: usize, b: usize) -> Extrema {
        let mut e = Extrema::of_sample(&samples[a], true);
        for s in &samples[a + 1..b] {
            e.absorb(s, true);
        }
        e
    }

    /// Compare only the I/Q columns (an Iq-mode result leaves magnitude unset).
    fn assert_iq_eq(got: &Extrema, want: &Extrema) {
        assert_eq!(
            (got.min_i, got.max_i, got.min_q, got.max_q),
            (want.min_i, want.max_i, want.min_q, want.max_q)
        );
    }

    #[test]
    fn test_extrema_over_lod_matches_naive_scan() {
        let samples = lcg_samples(5000);
        let signal = test_signal(&samples);
        // LOD table sanity: one entry per 256-sample block, last one short.
        assert_eq!(signal.lod.len(), 5000usize.div_ceil(BLOCK_SIZE));

        // Ranges below/around/above the two-block LOD threshold, aligned and
        // unaligned, including single samples and the whole signal.
        let ranges: &[(usize, usize)] = &[
            (0, 1),
            (5, 6),
            (0, 100),
            (7, 511),
            (7, 512),
            (7, 513),
            (100, 900),
            (255, 256),
            (255, 511),
            (256, 512),
            (37, 4971),
            (128, 4999),
            (0, 5000),
            (4744, 5000),
        ];
        for &(a, b) in ranges {
            // The I/Q pass and the magnitude pass must each match the naive
            // scan exactly (bit-for-bit: both sides fold the same f32 values).
            let iq = signal.extrema_over(a, b, false);
            let want = naive_range(&samples, a, b);
            assert_eq!(
                (iq.min_i, iq.max_i),
                (want.min_i, want.max_i),
                "I at {a}..{b}"
            );
            assert_eq!(
                (iq.min_q, iq.max_q),
                (want.min_q, want.max_q),
                "Q at {a}..{b}"
            );
            let mag = signal.extrema_over(a, b, true);
            assert_eq!(
                (mag.min_m, mag.max_m),
                (want.min_m, want.max_m),
                "|s| at {a}..{b}"
            );
        }
    }

    #[test]
    fn test_spike_at_every_position_survives() {
        // A spike must show up in the max of exactly the bin covering it,
        // wherever it sits: block-interior, block edge or bin boundary. Run
        // both the LOD path (w >= 2 blocks) and the direct-scan fallback.
        let len = 1300;
        let (start, end) = (13, len);
        for bins in [2usize, 3] {
            let span: usize = end - start;
            let w = span.div_ceil(bins.min(span));
            for spike in start..end {
                let mut samples = vec![Complex32::new(0.0, 0.0); len];
                samples[spike] = Complex32::new(5.0, 0.0);
                let signal = test_signal(&samples);

                let expected_bin = (spike - start) / w;
                let iq = signal.envelope(start, end, bins, WaveformMode::Iq);
                let mag = signal.envelope(start, end, bins, WaveformMode::Amplitude);
                assert_eq!(iq.len(), bins);
                for (k, bin) in iq.iter().enumerate() {
                    let want_max = if k == expected_bin { 5.0 } else { 0.0 };
                    assert_eq!(bin.max_i, want_max, "spike {spike}, bin {k}, bins {bins}");
                    assert_eq!(bin.min_i, 0.0, "spike {spike}, bin {k}, bins {bins}");
                    assert_eq!(bin.max_q, 0.0, "spike {spike}, bin {k}, bins {bins}");
                }
                assert_eq!(
                    mag[expected_bin].max_m, 5.0,
                    "magnitude spike {spike}, bins {bins}"
                );
            }
        }
    }

    #[test]
    fn test_degenerate_viewport_is_one_bin_per_sample() {
        let samples = lcg_samples(300);
        let signal = test_signal(&samples);
        // bins > span and bins == span both clamp to one bin per sample.
        for bins in [300usize, 301, 1000] {
            let env = signal.envelope(0, 300, bins, WaveformMode::Iq);
            assert_eq!(env.len(), 300, "bins {bins}");
            for (bin, s) in env.iter().zip(&samples) {
                assert_eq!(bin.min_i, s.re);
                assert_eq!(bin.max_i, s.re);
                assert_eq!(bin.min_q, s.im);
                assert_eq!(bin.max_q, s.im);
            }
            let mag = signal.envelope(0, 300, bins, WaveformMode::Amplitude);
            for (bin, s) in mag.iter().zip(&samples) {
                assert_eq!(bin.min_m, s.norm());
                assert_eq!(bin.max_m, s.norm());
            }
        }
    }

    #[test]
    fn test_empty_trailing_bins_repeat_previous() {
        // span = 4, bins = 3 -> w = ceil(4/3) = 2: bins cover [0,2), [2,4)
        // and an empty third bin that must repeat bin 1's values.
        let samples = lcg_samples(4);
        let signal = test_signal(&samples);
        let env = signal.envelope(0, 4, 3, WaveformMode::Iq);
        assert_eq!(env.len(), 3);
        assert_iq_eq(&env[0], &naive_range(&samples, 0, 2));
        assert_iq_eq(&env[1], &naive_range(&samples, 2, 4));
        assert_eq!(env[2], env[1]);
    }

    #[test]
    fn test_tiny_signal_below_one_block() {
        let samples = lcg_samples(7);
        let signal = test_signal(&samples);
        // Non-degenerate grid over an unaligned sub-range: span 5, w = 2.
        let env = signal.envelope(2, 7, 3, WaveformMode::Iq);
        assert_eq!(env.len(), 3);
        assert_iq_eq(&env[0], &naive_range(&samples, 2, 4));
        assert_iq_eq(&env[1], &naive_range(&samples, 4, 6));
        assert_iq_eq(&env[2], &naive_range(&samples, 6, 7));
        // Degenerate over the whole tiny signal.
        let env = signal.envelope(0, 7, 64, WaveformMode::Amplitude);
        assert_eq!(env.len(), 7);
        for (bin, s) in env.iter().zip(&samples) {
            assert_eq!(bin.max_m, s.norm());
        }
        // Single bin over the whole signal = global extrema.
        let env = signal.envelope(0, 7, 1, WaveformMode::Iq);
        assert_eq!(env.len(), 1);
        assert_iq_eq(&env[0], &naive_range(&samples, 0, 7));
    }

    #[test]
    fn test_meta_and_format_strings() {
        let signal = test_signal(&lcg_samples(10));
        let meta = signal.meta();
        assert_eq!(meta.signal_id, "sig-1");
        assert_eq!(meta.display_name, "test.complex");
        assert_eq!(meta.format, "complex");
        assert_eq!(meta.sample_count, 10);
        assert_eq!(meta.sample_rate, 1e6);
        assert!((meta.duration_secs - 10.0 / 1e6).abs() < 1e-15);

        signal.set_sample_rate(2.5e6);
        assert_eq!(signal.meta().sample_rate, 2.5e6);

        // Extension -> contract format string mapping.
        for (name, format) in [
            ("a.complex", "complex"),
            ("a.complex16s", "complex16s"),
            ("a.cs8", "complex16s"),
            ("a.complex16u", "complex16u"),
            ("a.cu8", "complex16u"),
            ("a.complex32s", "complex32s"),
            ("a.cs16", "complex32s"),
            ("a.complex32u", "complex32u"),
            ("a.cu16", "complex32u"),
            ("a.txt", "complex"),
        ] {
            let s = Signal::new(
                "sig-x".to_string(),
                name,
                IqArray::from_vec(Vec::new()),
                1e6,
            );
            assert_eq!(s.meta().format, format, "file {name}");
            assert_eq!(s.meta().display_name, name.rsplit('/').next().unwrap());
        }

        // Paths without a file name fall back to the whole path.
        let s = Signal::new(
            "sig-x".to_string(),
            "..",
            IqArray::from_vec(Vec::new()),
            1e6,
        );
        assert_eq!(s.meta().display_name, "..");
    }
}
