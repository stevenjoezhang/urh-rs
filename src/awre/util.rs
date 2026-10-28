//! Port of `urh/cythonext/awre_util.pyx` (AWRE utility hot loops).
//!
//! Bit sequences are `&[u8]` with elements 0/1. Semantics follow the Cython
//! original, including its quirks (noted inline).

use std::collections::{HashMap, HashSet};

use crate::encoding::crc as crc_check;
use crate::iq::bit_array_to_number;

/// pyx `lower_multiple_of_n`: `n * floor(number / n)`.
/// Floor division (Cython Python-semantics division), so negative inputs go
/// further from zero; callers clamp with `max(0, ..)` where the pyx does.
fn lower_multiple_of_n(number: i64, n: i64) -> i64 {
    n * number.div_euclid(n)
}

/// pyx `find`: index of `element` in `data` at or after `start`, else -1.
fn find(data: &[u8], element: u8, start: i64) -> i64 {
    for i in start..data.len() as i64 {
        if data[i as usize] == element {
            return i;
        }
    }
    -1
}

/// Find the index pairs (start, end) of the longest common sub sequence of
/// `seq1` and `seq2`. Both indices refer to positions in `seq1` (end is
/// exclusive); the pyx returns a Python `set` of up to 10 such pairs, so we
/// deduplicate while preserving first-encounter order.
///
/// Quirk (kept): pairs recorded while `longest` was still growing remain in
/// the result, so windows shorter than the final LCS length can appear; with
/// no common element at all the pyx yields `{(0, 0)}` — replicated here.
pub fn find_longest_common_sub_sequence_indices(seq1: &[u8], seq2: &[u8]) -> Vec<(usize, usize)> {
    const MAX_RESULTS: usize = 10;
    let len_bits1 = seq1.len();
    let len_bits2 = seq2.len();

    let mut m = vec![0u32; (len_bits1 + 1) * (len_bits2 + 1)];
    let mut longest = 0u32;
    let mut result_indices: Vec<(u32, u32)> = Vec::with_capacity(MAX_RESULTS);

    for i in 0..len_bits1 {
        for j in 0..len_bits2 {
            if seq1[i] == seq2[j] {
                let counter = m[i * (len_bits2 + 1) + j] + 1;
                m[(i + 1) * (len_bits2 + 1) + j + 1] = counter;

                if counter > longest {
                    longest = counter;
                    result_indices.clear();
                    result_indices.push((i as u32 + 1 - counter, i as u32 + 1));
                } else if counter == longest {
                    if result_indices.len() < MAX_RESULTS {
                        result_indices.push((i as u32 + 1 - counter, i as u32 + 1));
                    }
                }
            }
        }
    }

    if result_indices.is_empty() {
        // pyx reads the zero-initialized result slot -> {(0, 0)}
        return vec![(0, 0)];
    }

    let mut seen: Vec<(usize, usize)> = Vec::new();
    for pair in result_indices {
        let pair = (pair.0 as usize, pair.1 as usize);
        if !seen.contains(&pair) {
            seen.push(pair);
        }
    }
    seen
}

/// Index of the first differing bit, or the length of the shorter sequence.
pub fn find_first_difference(bits1: &[u8], bits2: &[u8]) -> u32 {
    let smaller_len = bits1.len().min(bits2.len());
    for i in 0..smaller_len {
        if bits1[i] != bits2[i] {
            return i as u32;
        }
    }
    smaller_len as u32
}

/// N x N matrix (only the upper triangle filled, rest 0) of the first
/// difference index between all pairs of bitvectors.
pub fn get_difference_matrix(bitvectors: &[Vec<u8>]) -> Vec<Vec<u32>> {
    let n = bitvectors.len();
    let mut result = vec![vec![0u32; n]; n];
    for (i, bitvector_i) in bitvectors.iter().enumerate() {
        for j in i + 1..n {
            result[i][j] = find_first_difference(bitvector_i, &bitvectors[j]);
        }
    }
    result
}

/// Pack each bitvector into nibbles (4 bits -> one number 0..15).
///
/// Quirk (kept): the pyx computes `ceil(len / 4)` over C integers, which is
/// integer division, so a trailing partial nibble is dropped (len 10 -> 2).
pub fn get_hexvectors(bitvectors: &[Vec<u8>]) -> Vec<Vec<u32>> {
    let mut result = Vec::with_capacity(bitvectors.len());
    for bitvector in bitvectors {
        let len_bitvector = bitvector.len();
        let m = len_bitvector / 4;
        let mut hexvector = Vec::with_capacity(m);
        for j in 0..m {
            hexvector.push(bit_array_to_number(
                bitvector,
                len_bitvector.min(4 * j + 4) as i64,
                (4 * j) as i64,
            ) as u32);
        }
        result.push(hexvector);
    }
    result
}

/// Estimate the preamble position of a single bitvector.
///
/// Returns `(start, lower, upper)` where lower/upper bracket the preamble
/// length; `None` is the pyx' `(0, 0, 0)` "no preamble found" result
/// (including its quirk that exactly `k == 2` pattern repetitions count as
/// no preamble — only `k > 2` is accepted).
///
/// Deviation from the pyx: the original can index past the end of the
/// bitvector (latent out-of-bounds bug) when the message ends mid-pattern;
/// here an out-of-bounds read is treated as a pattern mismatch.
pub fn get_raw_preamble_position(bitvector: &[u8]) -> Option<(usize, usize, usize)> {
    let n = bitvector.len() as i64;
    if n == 0 {
        return None;
    }

    let mut start: i64 = -1;
    let mut k = 0.0f64;
    let mut lower: i64 = 0;
    let mut upper: i64 = 0;

    while k < 2.0 && start < n {
        start += 1;
        if start >= n {
            // pyx would index out of bounds here; treat as "no preamble"
            return None;
        }

        let a = bitvector[start as usize];
        let b = if a == 0 { 1u8 } else { 0u8 };

        // search for the pattern a^n b^m
        let n_run = find(bitvector, b, start) - start;
        if n_run <= 0 {
            return None;
        }
        let m_run = find(bitvector, a, start + n_run) - n_run - start;
        if m_run <= 0 {
            return None;
        }

        let len_preamble_pattern = (n_run + m_run) as usize;
        let mut preamble_pattern = vec![a; n_run as usize];
        preamble_pattern.resize(len_preamble_pattern, b);

        // Quirk (kept): preamble_end stays `start` when the pattern never
        // mismatches before the end of the bitvector.
        let mut preamble_end = start;
        'outer: for i in (start..n).step_by(len_preamble_pattern) {
            for (j, &pattern_bit) in preamble_pattern.iter().enumerate() {
                match bitvector.get((i + j as i64) as usize) {
                    Some(&bit) if bit == pattern_bit => {}
                    _ => {
                        preamble_end = i;
                        break 'outer;
                    }
                }
            }
        }

        upper = start
            + lower_multiple_of_n(preamble_end + 1 - start, len_preamble_pattern as i64);
        lower = upper - len_preamble_pattern as i64;

        k = (upper - start) as f64 / len_preamble_pattern as f64;
    }

    if k > 2.0 {
        Some((start as usize, lower as usize, upper as usize))
    } else {
        // no preamble found
        None
    }
}

/// Score candidate sync words from the difference matrix.
///
/// `raw_preamble_positions` holds one row per message in the layout produced
/// by `Preprocessor::get_raw_preamble_positions`:
/// `(start, lower - start, upper - start)`.
///
/// Returns a map of sync word (bits) -> accumulated score; words that end
/// aligned to `n_gram_length` score 1.0, others 0.5. The pyx builds a Python
/// dict (insertion ordered); the HashMap here has no iteration order, which
/// callers must not rely on.
#[allow(clippy::too_many_arguments)]
pub fn find_possible_sync_words(
    difference_matrix: &[Vec<u32>],
    raw_preamble_positions: &[(u32, u32, u32)],
    bitvectors: &[Vec<u8>],
    n_gram_length: u32,
) -> HashMap<Vec<u8>, f64> {
    let mut possible_sync_words: HashMap<Vec<u8>, f64> = HashMap::new();

    let num_rows = difference_matrix.len();
    let num_cols = difference_matrix.first().map_or(0, |r| r.len());

    for i in 0..num_rows {
        for j in i + 1..num_cols {
            // position of first difference between message i and j
            let sync_end = difference_matrix[i][j];
            if sync_end == 0 {
                continue;
            }

            for k in 0..2usize {
                for &index in &[i, j] {
                    let pos = raw_preamble_positions[index];
                    let start = pos.0 as i64 + if k == 0 { pos.1 as i64 } else { pos.2 as i64 };

                    // Take the next lower multiple of n for the sync len.
                    // In doubt, it is better to under estimate the sync len
                    // to prevent it from taking needed values from other
                    // fields e.g. leading zeros for a length field.
                    let sync_len =
                        std::cmp::max(0, lower_multiple_of_n(sync_end as i64 - start, n_gram_length as i64));

                    if sync_len >= 2 {
                        let bitvector = &bitvectors[index];
                        let start_u = start as usize;
                        if sync_len == 2 {
                            // Sync word must not be just "10" or "01" because
                            // that would be indistinguishable from the preamble
                            if bitvector[start_u] == 0 && bitvector[start_u + 1] == 1 {
                                continue;
                            }
                            if bitvector[start_u] == 1 && bitvector[start_u + 1] == 0 {
                                continue;
                            }
                        }

                        let sync_word =
                            bitvector[start_u..start_u + sync_len as usize].to_vec();
                        let score = if (start_u + sync_len as usize) % n_gram_length as usize == 0
                        {
                            // if sync end aligns nicely at n gram length give it a larger score
                            1.0
                        } else {
                            0.5
                        };
                        *possible_sync_words.entry(sync_word).or_insert(0.0) += score;
                    }
                }
            }
        }
    }

    possible_sync_words
}

/// Histogram of common ranges: `result[k]` is the fraction of message pairs
/// whose bit at position `k` is equal. Only vectors indexed by
/// `active_indices` participate.
pub fn create_difference_histogram(vectors: &[Vec<u8>], active_indices: &[usize]) -> Vec<f64> {
    let l = active_indices.len();
    let longest = active_indices
        .iter()
        .map(|&i| vectors[i].len())
        .max()
        .unwrap_or(0);

    let mut histogram = vec![0.0f64; longest];
    let n = (l as i64 * (l as i64 - 1) / 2) as f64;

    for i in 0..l {
        let index_i = active_indices[i];
        let bitvector_i = &vectors[index_i];
        let len_vector_i = bitvector_i.len();
        for j in i + 1..l {
            let bitvector_j = &vectors[active_indices[j]];
            for k in 0..len_vector_i.min(bitvector_j.len()) {
                if bitvector_i[k] == bitvector_j[k] {
                    histogram[k] += 1.0 / n;
                }
            }
        }
    }
    histogram
}

/// Find the start indices of occurrences of `b` in `a`.
///
/// Quirk (kept): when `ignore_indices` is given, an ignored position makes
/// the candidate fail even if the bits would match.
pub fn find_occurrences(
    a: &[u8],
    b: &[u8],
    ignore_indices: Option<&[usize]>,
    return_after_first: bool,
) -> Vec<usize> {
    let len_a = a.len();
    let len_b = b.len();
    if len_b > len_a {
        return vec![];
    }

    let mut result = Vec::new();
    for i in 0..=(len_a - len_b) {
        let mut found = true;
        for j in 0..len_b {
            if let Some(ign) = ignore_indices {
                if ign.contains(&(i + j)) {
                    found = false;
                    break;
                }
            }
            if a[i + j] != b[j] {
                found = false;
                break;
            }
        }
        if found {
            if return_after_first {
                return vec![i];
            }
            result.push(i);
        }
    }
    result
}

/// Difference matrix of consecutive messages: entry (i, j) is the difference
/// of the j-th n-gram (as a number) between message i and i+1, modulo
/// 2^n_gram_length. Rows past the shorter of two messages stay -1.
///
/// Quirk (kept): the column count is floor(max_len / n_gram_length), so a
/// trailing partial n-gram is dropped.
pub fn create_seq_number_difference_matrix(
    bitvectors: &[Vec<u8>],
    n_gram_length: usize,
) -> Vec<Vec<i32>> {
    let n = bitvectors.len();
    if n == 0 {
        return vec![];
    }
    let max_len = bitvectors.iter().map(|b| b.len()).max().unwrap_or(0);
    let m = max_len / n_gram_length;
    // pyx casts pow(2, n) to int32; overflow behaves the same (wrapping) here
    let n_gram_power_two = (1i32).wrapping_shl(n_gram_length as u32);

    let mut result = vec![vec![-1i32; m]; n - 1];
    for i in 1..n {
        let bv1 = &bitvectors[i - 1];
        let bv2 = &bitvectors[i];
        let k = bv1.len().min(bv2.len());
        for j in (0..k).step_by(n_gram_length) {
            let index = j / n_gram_length;
            if index < m {
                let diff = bit_array_to_number(
                    bv2,
                    bv2.len().min(j + n_gram_length) as i64,
                    j as i64,
                ) as i32
                    - bit_array_to_number(
                        bv1,
                        bv1.len().min(j + n_gram_length) as i64,
                        j as i64,
                    ) as i32;
                // + n_gram_power_two because plain % can be negative
                result[i - 1][index] = (diff + n_gram_power_two) % n_gram_power_two;
            }
        }
    }
    result
}

/// Check a configurable subset of bitvectors for a matching CRC and return
/// the indices of the vectors that match the CRC with the given parameters.
#[allow(clippy::too_many_arguments)]
pub fn check_crc_for_messages(
    message_indices: &[usize],
    bitvectors: &[Vec<u8>],
    data_start: usize,
    data_stop: usize,
    crc_start: usize,
    crc_stop: usize,
    crc_polynomial: &[u8],
    crc_start_value: &[u8],
    crc_final_xor: &[u8],
    crc_lsb_first: bool,
    crc_reverse_polynomial: bool,
    crc_reverse_all: bool,
    crc_little_endian: bool,
) -> HashSet<usize> {
    let mut result = HashSet::new();
    for &index in message_indices {
        let bits = &bitvectors[index];
        let crc_input = &bits[data_start..data_stop];
        let check = bit_array_to_number(
            &bits[crc_start..crc_stop],
            (crc_stop - crc_start) as i64,
            0,
        );
        if crc_check(
            crc_input,
            crc_polynomial,
            crc_start_value,
            crc_final_xor,
            crc_lsb_first,
            crc_reverse_polynomial,
            crc_reverse_all,
            crc_little_endian,
        ) == check
        {
            result.insert(index);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(s: &str) -> Vec<u8> {
        s.chars().map(|c| if c == '1' { 1 } else { 0 }).collect()
    }

    #[test]
    fn test_find_longest_common_sub_sequence_indices() {
        // no common element -> pyx' {(0, 0)}
        assert_eq!(
            find_longest_common_sub_sequence_indices(&[0], &[1]),
            vec![(0, 0)]
        );

        let mut result = find_longest_common_sub_sequence_indices(&[1, 1, 1], &[1, 1]);
        result.sort_unstable();
        assert_eq!(result, vec![(0, 2), (1, 3)]);

        let mut result = find_longest_common_sub_sequence_indices(
            &bits("1010"),
            &bits("0101"),
        );
        result.sort_unstable();
        assert_eq!(result, vec![(0, 3), (1, 4)]);
    }

    #[test]
    fn test_find_first_difference() {
        assert_eq!(find_first_difference(&bits("101"), &bits("100")), 2);
        assert_eq!(find_first_difference(&bits("101"), &bits("011")), 0);
        assert_eq!(find_first_difference(&bits("101"), &bits("101")), 3);
        assert_eq!(find_first_difference(&bits("101"), &bits("1011")), 3);
    }

    #[test]
    fn test_get_difference_matrix() {
        let bvs = vec![bits("101"), bits("100"), bits("111")];
        let m = get_difference_matrix(&bvs);
        assert_eq!(
            m,
            vec![
                vec![0, 2, 1],
                vec![0, 0, 1],
                vec![0, 0, 0],
            ]
        );
    }

    #[test]
    fn test_get_hexvectors() {
        let bvs = vec![bits("10110010")];
        assert_eq!(get_hexvectors(&bvs), vec![vec![0b1011, 0b0010]]);

        // pyx quirk: 10 bits -> only 2 nibbles (trailing partial nibble dropped)
        let bvs = vec![bits("1111000010")];
        let hex = get_hexvectors(&bvs);
        assert_eq!(hex, vec![vec![15, 0]]);
    }

    #[test]
    fn test_get_raw_preamble_position() {
        // preamble + sync + payload
        let bv = bits("1010101011010000");
        assert_eq!(get_raw_preamble_position(&bv), Some((0, 6, 8)));

        assert_eq!(get_raw_preamble_position(&[]), None);
        assert_eq!(get_raw_preamble_position(&bits("0000")), None);
        assert_eq!(get_raw_preamble_position(&bits("1")), None);
        // pure alternation: never more than 2 clean repetitions -> no preamble
        assert_eq!(get_raw_preamble_position(&bits("1010")), None);

        // alternating pattern with 3+ repetitions, then a break
        let bv = bits("0101010110");
        assert_eq!(get_raw_preamble_position(&bv), Some((0, 6, 8)));
    }

    #[test]
    fn test_find_possible_sync_words() {
        let bv1 = bits("10101010110100000000");
        let bv2 = bits("10101010110100001010");
        let bitvectors = vec![bv1.clone(), bv2.clone()];
        let difference_matrix = get_difference_matrix(&bitvectors);
        // (start, lower - start, upper - start) for raw position (0, 6, 8)
        let raw_preamble_positions = vec![(0u32, 6u32, 8u32), (0u32, 6u32, 8u32)];

        let result = find_possible_sync_words(
            &difference_matrix,
            &raw_preamble_positions,
            &bitvectors,
            4,
        );

        let mut expected = HashMap::new();
        // start=6: "10" + sync "1101" + "00" (unaligned -> 0.5, from both messages)
        expected.insert(bits("10110100"), 1.0);
        // start=8: sync "1101" + "0000" (aligned -> 1.0, from both messages)
        expected.insert(bits("11010000"), 2.0);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_create_difference_histogram() {
        let vectors = vec![
            bits("10101111"),
            bits("10101000"),
            bits("10101111"),
            bits("10100000"),
        ];
        let hist = create_difference_histogram(&vectors, &[0, 1, 2, 3]);
        // 1/6 added six times per position -> 0.999... like the pyx
        let expected = vec![
            1.0,
            1.0,
            1.0,
            1.0,
            3.0 / 6.0,
            2.0 / 6.0,
            2.0 / 6.0,
            2.0 / 6.0,
        ];
        assert_eq!(hist.len(), expected.len());
        for (a, b) in hist.iter().zip(&expected) {
            assert!((a - b).abs() < 1e-9, "{a} != {b}");
        }

        // subset of active indices
        let hist = create_difference_histogram(&vectors, &[0, 2]);
        assert_eq!(hist, vec![1.0; 8]);

        // single active index -> zero histogram (n == 0, nothing accumulated)
        let hist = create_difference_histogram(&vectors, &[0]);
        assert_eq!(hist, vec![0.0; 8]);
    }

    #[test]
    fn test_find_occurrences() {
        let a = bits("1011011");
        let b = bits("11");
        assert_eq!(find_occurrences(&a, &b, None, false), vec![2, 5]);
        assert_eq!(find_occurrences(&a, &b, None, true), vec![2]);

        // quirk: ignored index kills the candidate even though bits match
        assert_eq!(find_occurrences(&a, &b, Some(&[3]), false), vec![5]);
        assert_eq!(find_occurrences(&a, &bits("111"), None, false), Vec::<usize>::new());
        assert_eq!(find_occurrences(&bits("10"), &bits("101"), None, false), vec![]);
    }

    #[test]
    fn test_create_seq_number_difference_matrix() {
        let bvs = vec![bits("00000101"), bits("00000111")];
        let m = create_seq_number_difference_matrix(&bvs, 4);
        // second nibble: 7 - 5 = 2
        assert_eq!(m, vec![vec![0, 2]]);

        let bvs = vec![bits("00000000"), bits("00000001"), bits("00000011")];
        let m = create_seq_number_difference_matrix(&bvs, 4);
        assert_eq!(m, vec![vec![0, 1], vec![0, 2]]);
    }

    #[test]
    fn test_check_crc_for_messages() {
        // polynomial "11" (x + 1) with zero start value degenerates to a
        // parity check over the data bits
        let poly = bits("11");
        let zero = bits("0");
        let make = |data: &str, crc_bit: u8| {
            let mut bv = bits(data);
            bv.push(crc_bit);
            bv
        };
        let bitvectors = vec![
            make("1011", 1), // valid parity
            make("1011", 0), // broken
            make("1100", 0), // valid
        ];

        let result = check_crc_for_messages(
            &[0, 1, 2],
            &bitvectors,
            0,
            4,
            4,
            5,
            &poly,
            &zero,
            &zero,
            false,
            false,
            false,
            false,
        );
        let expected: HashSet<usize> = [0, 2].into_iter().collect();
        assert_eq!(result, expected);

        // non-zero start value shifts the parity
        let result = check_crc_for_messages(
            &[0, 1],
            &bitvectors,
            0,
            4,
            4,
            5,
            &poly,
            &bits("1"),
            &zero,
            false,
            false,
            false,
            false,
        );
        let expected: HashSet<usize> = [1].into_iter().collect();
        assert_eq!(result, expected);
    }
}
