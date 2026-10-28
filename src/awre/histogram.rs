//! Port of `urh/awre/Histogram.py`.

use crate::awre::common_range::{CommonRange, RangeType};
use crate::awre::util::create_difference_histogram;

/// Create a histogram based on the equalness of vectors.
pub struct Histogram {
    vectors: Vec<Vec<u8>>,
    /// Indices of vectors for which the histogram is created (useful for
    /// clustering); all vectors when `None` was passed to `new`.
    active_indices: Vec<usize>,
    /// Kept for API parity with Python; the underlying pyx histogram is
    /// always normalized, so this only labels plots there.
    pub normalize: bool,
    /// Fraction of message pairs with equal bits, per bit position.
    pub data: Vec<f64>,
}

impl Histogram {
    pub fn new(vectors: Vec<Vec<u8>>, indices: Option<Vec<usize>>) -> Self {
        let active_indices =
            indices.unwrap_or_else(|| (0..vectors.len()).collect());
        let data = create_difference_histogram(&vectors, &active_indices);
        Histogram {
            vectors,
            active_indices,
            normalize: true,
            data,
        }
    }

    /// Find all common ranges where at least `alpha` percent of bits are equal.
    pub fn find_common_ranges(&self, alpha: f64, range_type: RangeType) -> Vec<CommonRange> {
        let data_indices: Vec<usize> = self
            .data
            .iter()
            .enumerate()
            .filter(|(_, &v)| v >= alpha)
            .map(|(i, _)| i)
            .collect();

        if data_indices.len() < 2 {
            return vec![];
        }

        let mut result = Vec::new();
        let mut start: Option<usize> = None;
        let mut length = 0usize;
        let n = data_indices.len();

        for i in 1..n {
            if start.is_none() {
                start = Some(data_indices[i - 1]);
                length = 1;
            }

            if data_indices[i] - data_indices[i - 1] == 1 {
                length += 1;
            } else {
                if length >= 2 {
                    result.push(self.make_range(start.unwrap(), length, range_type));
                }
                start = None;
                length = 0;
            }

            if i == n - 1 && length >= 2 {
                result.push(self.make_range(start.unwrap(), length, range_type));
            }
        }

        result
    }

    fn make_range(&self, start: usize, length: usize, range_type: RangeType) -> CommonRange {
        let mut range = CommonRange::new(start, length);
        range.values = vec![self.get_value_for_common_range(start, length)];
        range.range_type = range_type;
        range.message_indices = self.active_indices.iter().copied().collect();
        range
    }

    /// Get the value for a range of common numbers: the value that appears
    /// most (first one wins ties — Python's `max` keeps the first key).
    ///
    /// Python quirk (kept): `max(values, key=len)` measures the length of the
    /// *key* (the bit slice), not how many messages share it; a truncated
    /// slice from a too-short vector would win over full-length keys.
    fn get_value_for_common_range(&self, start: usize, length: usize) -> Vec<u8> {
        let mut best: Option<Vec<u8>> = None;
        for &i in &self.active_indices {
            let vector = &self.vectors[i];
            let end = (start + length).min(vector.len());
            let slice = &vector[start.min(vector.len())..end];
            match &best {
                None => best = Some(slice.to_vec()),
                Some(b) if slice.len() > b.len() => best = Some(slice.to_vec()),
                _ => {}
            }
        }
        best.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awre::common_range::RangeType;

    fn bits(s: &str) -> Vec<u8> {
        s.chars().map(|c| if c == '1' { 1 } else { 0 }).collect()
    }

    fn vectors() -> Vec<Vec<u8>> {
        vec![
            bits("10101111"),
            bits("10101000"),
            bits("10101111"),
            bits("10100000"),
        ]
    }

    // position 4 is the only one where only 2 of 6 pairs agree
    fn vectors_with_gap() -> Vec<Vec<u8>> {
        vec![
            bits("10101111"),
            bits("10101111"),
            bits("10100111"),
            bits("10100111"),
        ]
    }

    fn assert_approx(a: &[f64], b: &[f64]) {
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b) {
            assert!((x - y).abs() < 1e-9, "{x} != {y}");
        }
    }

    #[test]
    fn test_histogram_data() {
        let h = Histogram::new(vectors(), None);
        assert_approx(
            &h.data,
            &[1.0, 1.0, 1.0, 1.0, 0.5, 2.0 / 6.0, 2.0 / 6.0, 2.0 / 6.0],
        );
    }

    #[test]
    fn test_find_common_ranges() {
        let h = Histogram::new(vectors_with_gap(), None);
        let ranges = h.find_common_ranges(0.95, RangeType::Bit);
        assert_eq!(ranges.len(), 2);

        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[0].length, 4);
        assert_eq!(ranges[0].values, vec![bits("1010")]);
        assert_eq!(ranges[0].range_type, RangeType::Bit);
        assert_eq!(
            ranges[0].message_indices,
            [0, 1, 2, 3].into_iter().collect()
        );

        assert_eq!(ranges[1].start, 5);
        assert_eq!(ranges[1].length, 3);
        assert_eq!(ranges[1].values, vec![bits("111")]);
    }

    #[test]
    fn test_find_common_ranges_below_alpha() {
        // the gap position (1/3 agreement) joins only at low alpha
        let h = Histogram::new(vectors_with_gap(), None);
        let ranges = h.find_common_ranges(0.3, RangeType::Bit);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[0].length, 8);
    }

    #[test]
    fn test_find_common_ranges_single_run() {
        let vs = vectors_with_gap();
        let h = Histogram::new(vs.clone(), Some(vec![0, 1]));
        assert_approx(&h.data, &[1.0; 8]);
        let ranges = h.find_common_ranges(0.95, RangeType::Bit);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[0].length, 8);
        assert_eq!(ranges[0].values, vec![vs[0].clone()]);
        assert_eq!(ranges[0].message_indices, [0, 1].into_iter().collect());
    }

    #[test]
    fn test_find_common_ranges_none() {
        // single active index -> all-zero histogram -> no common ranges
        let h = Histogram::new(vectors(), Some(vec![0]));
        assert!(h.find_common_ranges(0.95, RangeType::Bit).is_empty());

        // fewer than 2 indices above alpha
        let h = Histogram::new(vec![bits("00"), bits("01")], None);
        assert!(h.find_common_ranges(0.95, RangeType::Bit).is_empty());
    }

    #[test]
    fn test_find_common_ranges_range_type_passthrough() {
        let h = Histogram::new(vectors_with_gap(), None);
        let ranges = h.find_common_ranges(0.95, RangeType::Hex);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].range_type, RangeType::Hex);
    }
}
