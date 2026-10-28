//! Port of `urh/awre/engines/Engine.py` (shared static helpers) plus the
//! CommonRange operations the engines need that are not part of the frozen
//! `common_range.rs` API (`bit_start`/`bit_end`, equality, overlap,
//! `ensure_not_overlaps`).

pub mod address;
pub mod checksum;
pub mod length;
pub mod sequence_number;

use crate::awre::common_range::{CommonRange, RangeType};
use crate::awre::histogram::Histogram;
use crate::awre::util;

/// Result of `find_common_ranges_by_cluster`: one `(cluster_key, ranges)`
/// entry per input cluster, in input order.
pub type RangesByCluster<K> = Vec<(K, Vec<CommonRange>)>;

/// Ordered cluster list: `(cluster_key, message_indices)`. A Vec (instead of
/// Python's dict) keeps Python's insertion order, which the engines rely on
/// when iterating `common_ranges_by_cluster`.
pub type Clusters<K> = Vec<(K, Vec<usize>)>;

fn convert_number(n: usize, range_type: RangeType) -> usize {
    match range_type {
        RangeType::Bit => n,
        RangeType::Hex => 4 * n,
        RangeType::Byte => 8 * n,
    }
}

/// URH `CommonRange.bit_start`.
pub fn bit_start(rng: &CommonRange) -> usize {
    convert_number(rng.start, rng.range_type) + rng.sync_end
}

/// URH `CommonRange.bit_end` (inclusive).
pub fn bit_end(rng: &CommonRange) -> usize {
    convert_number(rng.start, rng.range_type) + convert_number(rng.length, rng.range_type)
        - 1
        + rng.sync_end
}

/// URH `CommonRange.length_in_bits` (note the reference's off-by-one quirk:
/// `bit_end - bit_start - 1`).
pub fn length_in_bits(rng: &CommonRange) -> usize {
    bit_end(rng).saturating_sub(bit_start(rng)).saturating_sub(1)
}

/// URH `CommonRange.__eq__` / `__hash__`: equality by bit_start, bit_end and
/// field type (used wherever Python puts ranges into sets).
pub fn rng_eq(a: &CommonRange, b: &CommonRange) -> bool {
    bit_start(a) == bit_start(b) && bit_end(a) == bit_end(b) && a.field_type == b.field_type
}

/// URH `CommonRange.__lt__`: ordering by bit_start (ties by bit_end).
pub fn rng_lt(a: &CommonRange, b: &CommonRange) -> std::cmp::Ordering {
    bit_start(a)
        .cmp(&bit_start(b))
        .then(bit_end(a).cmp(&bit_end(b)))
}

/// URH `CommonRange.overlaps_with` — note the reference treats `bit_end`
/// as an *exclusive* bound here (`range(bit_start, bit_end)`).
pub fn rng_overlaps(a: &CommonRange, b: &CommonRange) -> bool {
    let (s1, e1) = (bit_start(a), bit_end(a));
    let (s2, e2) = (bit_start(b), bit_end(b));
    std::cmp::max(s1, s2) < std::cmp::min(e1, e2)
}

/// URH `CommonRange.matches`.
pub fn rng_matches(rng: &CommonRange, start: usize, value: &[u8]) -> bool {
    rng.start == start
        && rng.length == value.len()
        && rng.values.first().map(|v| v.as_slice()) == Some(value)
}

/// Port of `Engine.find_common_ranges_by_cluster`. Returns one entry per
/// input cluster, in input order.
pub fn find_common_ranges_by_cluster(
    msg_vectors: &[Vec<u8>],
    clustered_bitvectors: &Clusters<usize>,
    alpha: f64,
    range_type: RangeType,
) -> RangesByCluster<usize> {
    clustered_bitvectors
        .iter()
        .map(|(cluster, message_indices)| {
            let histogram = Histogram::new(msg_vectors.to_vec(), Some(message_indices.clone()));
            let ranges = histogram.find_common_ranges(alpha, range_type);
            (*cluster, ranges)
        })
        .collect()
}

/// Port of `Engine.find_common_ranges_exhaustive`.
pub fn find_common_ranges_exhaustive(
    msg_vectors: &[Vec<u8>],
    msg_indices: &[usize],
    range_type: RangeType,
) -> Vec<CommonRange> {
    let mut result: Vec<CommonRange> = Vec::new();
    // itertools.combinations(msg_indices, 2) order
    for a in 0..msg_indices.len() {
        for b in a + 1..msg_indices.len() {
            merge_exhaustive_pair(&mut result, msg_vectors, msg_indices[a], msg_indices[b], range_type);
        }
    }
    result
}

fn merge_exhaustive_pair(
    result: &mut Vec<CommonRange>,
    msg_vectors: &[Vec<u8>],
    i: usize,
    j: usize,
    range_type: RangeType,
) {
    for rng in Histogram::new(msg_vectors.to_vec(), Some(vec![i, j]))
        .find_common_ranges(1.0, range_type)
    {
        match result.iter_mut().find(|cr| {
            cr.start == rng.start
                && cr
                    .values
                    .first()
                    .map(|v| v.as_slice())
                    == rng.values.first().map(|v| v.as_slice())
        }) {
            Some(common_range) => {
                common_range.message_indices.insert(i);
                common_range.message_indices.insert(j);
            }
            None => result.push(rng),
        }
    }
}

/// Port of `Engine.ignore_already_labeled`. `already_labeled` holds
/// `(start, end)` pairs with exclusive end.
pub fn ignore_already_labeled(
    common_ranges: &[CommonRange],
    already_labeled: &[(usize, usize)],
) -> Vec<CommonRange> {
    let mut result = Vec::new();
    for common_range in common_ranges {
        let mut range_result = vec![common_range.clone()];
        for &(start, end) in already_labeled {
            let mut next = Vec::new();
            for rng in range_result {
                next.extend(ensure_not_overlaps(&rng, start, end));
            }
            range_result = next;
        }
        result.extend(range_result);
    }
    result
}

/// Port of `CommonRange.ensure_not_overlaps` (bit coordinates, `end`
/// exclusive). Only meaningful for ranges with a single value (as produced
/// by the histogram), like the reference which uses the `value` property.
pub fn ensure_not_overlaps(rng: &CommonRange, start: usize, end: usize) -> Vec<CommonRange> {
    if end < rng.start || start > rng.end() {
        // other range is right or left of our range -> no overlapping
        return vec![rng.clone()];
    }

    let value: &[u8] = rng.values.first().map(|v| v.as_slice()).unwrap_or(&[]);

    let mk = |start: usize, length: usize, value: Vec<u8>| {
        let mut r = rng.clone();
        r.start = start;
        r.length = length;
        r.values = if value.is_empty() && rng.values.is_empty() {
            Vec::new()
        } else {
            vec![value]
        };
        r
    };

    if start <= rng.start && rng.start < end && end < rng.end() {
        // overlaps on the left
        let mut length = rng.length;
        length -= end - rng.start;
        let new_start = end;
        let v = value
            .get(new_start - rng.start..(new_start - rng.start) + length)
            .map(|s| s.to_vec())
            .unwrap_or_default();
        return vec![mk(new_start, length, v)];
    }

    if rng.start < start && start <= rng.end() && rng.end() <= end {
        // overlaps on the right
        let mut length = rng.length;
        length -= rng.end() + 1 - start;
        let v = value.get(..length).map(|s| s.to_vec()).unwrap_or_default();
        return vec![mk(rng.start, length, v)];
    }

    if rng.start < start && rng.end() > end {
        // overlaps in the middle
        let mut left = rng.clone();
        left.length -= left.end() + 1 - start;
        if rng.values.len() == 1 {
            left.values = vec![value.get(..left.length).map(|s| s.to_vec()).unwrap_or_default()];
        }

        let mut right = rng.clone();
        right.start = end + 1;
        right.length = rng.end() - end;
        if rng.values.len() == 1 {
            right.values = vec![value
                .get(right.start - rng.start..(right.start - rng.start) + right.length)
                .map(|s| s.to_vec())
                .unwrap_or_default()];
        }
        return vec![left, right];
    }

    Vec::new()
}

/// Port of `Engine.find_longest_common_sub_sequences`.
pub fn find_longest_common_sub_sequences(
    seq1: Option<&[u8]>,
    seq2: Option<&[u8]>,
) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    let (seq1, seq2) = match (seq1, seq2) {
        (Some(s1), Some(s2)) => (s1, s2),
        _ => return result,
    };

    for ind in util::find_longest_common_sub_sequence_indices(seq1, seq2) {
        if ind.1 > ind.0 {
            result.push(seq1[ind.0..ind.1].to_vec());
        }
    }
    result
}
