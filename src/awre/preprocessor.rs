//! Port of `urh/awre/Preprocessor.py`.
//!
//! This preprocessor:
//! 1) identifies the preamble / length of preamble,
//! 2) identifies the sync word(s),
//! 3) derives per-message preamble lengths aligned on the sync words.
//!
//! Deviation from Python: the `existing_message_types` parameter (labeled
//! preamble/sync fields per message) is not ported; instead already known
//! sync words are passed directly to [`Preprocessor::new`]. The main code
//! path in URH passes `None` for `existing_message_types`, i.e. no existing
//! labels, which corresponds to passing an empty list here. The current
//! reference Preprocessor.py has no `__align_sync_words` method, so there is
//! nothing to port for it.

use std::collections::HashMap;

use crate::awre::util;

/// One row of the raw preamble position table:
/// `(start, lower - start, upper - start)` — the layout of the Nx3 numpy
/// array the Python Preprocessor feeds into `awre_util.find_possible_sync_words`.
pub type RawPreamblePosition = (u32, u32, u32);

/// Word -> score pair; a Vec is used where Python relies on dict insertion
/// order (sort by score with stable ties keeps first-seen order).
type ScoredWord = (Vec<u8>, f64);

pub struct Preprocessor {
    bitvectors: Vec<Vec<u8>>,
    existing_sync_words: Vec<Vec<u8>>,
}

impl Preprocessor {
    pub fn new(bitvectors: Vec<Vec<u8>>, existing_sync_words: Vec<Vec<u8>>) -> Self {
        Preprocessor {
            bitvectors,
            existing_sync_words,
        }
    }

    /// Returns `(preamble_starts, preamble_lengths, sync_len)`.
    pub fn preprocess(&self) -> (Vec<usize>, Vec<usize>, usize) {
        let raw_preamble_positions = self.get_raw_preamble_positions();
        let existing_sync_words = &self.existing_sync_words;

        let sync_words: Vec<Vec<u8>> = if existing_sync_words.is_empty() {
            self.find_possible_syncs(Some(&raw_preamble_positions))
        } else {
            // NOTE: This does not cover the case of a protocol with multiple
            // sync words where not all of them were labeled (same as Python).
            existing_sync_words.clone()
        };

        let preamble_starts: Vec<usize> = raw_preamble_positions
            .iter()
            .map(|p| p.0 as usize)
            .collect();
        let preamble_lengths =
            self.get_preamble_lengths_from_sync_words(&sync_words, &preamble_starts);
        let sync_len = sync_words.first().map_or(0, |s| s.len());
        (preamble_starts, preamble_lengths, sync_len)
    }

    /// Get the preamble lengths based on the found sync words for all
    /// messages. If there should be more than one sync word in a message,
    /// the first one is used.
    pub fn get_preamble_lengths_from_sync_words(
        &self,
        sync_words: &[Vec<u8>],
        preamble_starts: &[usize],
    ) -> Vec<usize> {
        // If there should be varying sync word lengths we would need to
        // return an array of sync lengths per message (asserted in Python).
        debug_assert!(sync_words
            .iter()
            .all(|s| s.len() == sync_words.first().map_or(0, |f| f.len())));

        let mut result = vec![0usize; self.bitvectors.len()];

        for (i, bitvector) in self.bitvectors.iter().enumerate() {
            let mut preamble_lengths: Vec<i64> = Vec::new();
            let preamble_start = preamble_starts[i] as i64;

            for sync_word in sync_words {
                let mut sync_start = find_sub(bitvector, sync_word, 0, bitvector.len());
                if sync_start != -1 {
                    if sync_start - preamble_start >= 2 {
                        preamble_lengths.push(sync_start - preamble_start);
                    }

                    // Consider case where sync word starts with preamble pattern
                    sync_start = find_sub(
                        bitvector,
                        sync_word,
                        (sync_start + 1) as usize,
                        (sync_start + 2 * sync_word.len() as i64) as usize,
                    );

                    if sync_start != -1 && sync_start - preamble_start >= 2 {
                        preamble_lengths.push(sync_start - preamble_start);
                    }
                }
            }

            preamble_lengths.sort_unstable();

            if preamble_lengths.is_empty() {
                result[i] = 0;
            } else if preamble_lengths.len() == 1 {
                result[i] = preamble_lengths[0] as usize;
            } else {
                // consider all indices not more than one byte before first one
                let first = preamble_lengths[0];
                preamble_lengths.retain(|&x| x < first + 7);

                // take the smallest preamble_length, but prefer a greater one
                // if it is divisible by 8 (or 4)
                let preamble_length = preamble_lengths
                    .iter()
                    .cloned()
                    .find(|&pl| pl % 8 == 0)
                    .or_else(|| {
                        preamble_lengths
                            .iter()
                            .cloned()
                            .find(|&pl| pl % 4 == 0)
                    })
                    .unwrap_or(first);
                result[i] = preamble_length as usize;
            }
        }

        result
    }

    pub fn find_possible_syncs(
        &self,
        raw_preamble_positions: Option<&[RawPreamblePosition]>,
    ) -> Vec<Vec<u8>> {
        let difference_matrix = util::get_difference_matrix(&self.bitvectors);
        let positions: Vec<RawPreamblePosition> = match raw_preamble_positions {
            Some(p) => p.to_vec(),
            None => self.get_raw_preamble_positions(),
        };
        self.determine_sync_candidates(&positions, &difference_matrix, 4)
    }

    /// Merge possible sync words by looking for common prefixes.
    ///
    /// Deviation from Python: iteration order over the (HashMap-based) word
    /// set is made deterministic by sorting words lexicographically; Python
    /// iterates in dict insertion order, which only affects tie-breaking.
    pub fn merge_possible_sync_words(
        possible_sync_words: &HashMap<Vec<u8>, f64>,
        n_gram_length: usize,
    ) -> Vec<ScoredWord> {
        if possible_sync_words.len() < 2 {
            return possible_sync_words
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect();
        }

        let mut words: Vec<ScoredWord> = possible_sync_words
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        words.sort_by(|a, b| a.0.cmp(&b.0));

        let mut result: Vec<ScoredWord> = Vec::new();
        for a in 0..words.len() {
            for b in a + 1..words.len() {
                let (w1, s1) = &words[a];
                let (w2, s2) = &words[b];
                let common_prefix_len =
                    w1.iter().zip(w2.iter()).take_while(|(x, y)| x == y).count();
                if common_prefix_len > n_gram_length {
                    add_scored(
                        &mut result,
                        w1[..common_prefix_len].to_vec(),
                        s1 + s2,
                    );
                } else {
                    add_scored(&mut result, w1.clone(), *s1);
                    add_scored(&mut result, w2.clone(), *s2);
                }
            }
        }
        result
    }

    fn determine_sync_candidates(
        &self,
        raw_preamble_positions: &[RawPreamblePosition],
        difference_matrix: &[Vec<u32>],
        n_gram_length: usize,
    ) -> Vec<Vec<u8>> {
        let possible_sync_words = util::find_possible_sync_words(
            difference_matrix,
            raw_preamble_positions,
            &self.bitvectors,
            n_gram_length as u32,
        );

        if possible_sync_words.is_empty() {
            return vec![];
        }

        let possible_sync_words =
            Self::merge_possible_sync_words(&possible_sync_words, n_gram_length);

        // score sync lengths, keeping first-seen order for tie-breaking
        // (Python dict order + stable sorted)
        let mut lengths: Vec<(usize, f64)> = Vec::new();
        for (word, score) in &possible_sync_words {
            match lengths.iter_mut().find(|(l, _)| *l == word.len()) {
                Some(entry) => entry.1 += *score,
                None => lengths.push((word.len(), *score)),
            }
        }

        // estimated length: highest score, first seen wins ties
        let mut estimated_sync_length = lengths[0].0;
        let mut best_score = f64::NEG_INFINITY;
        for &(length, score) in &lengths {
            if score > best_score {
                best_score = score;
                estimated_sync_length = length;
            }
        }

        if estimated_sync_length % 8 != 0 {
            let mut sorted_scores = lengths.clone();
            sorted_scores.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for &(other, _) in &sorted_scores {
                if 0 < estimated_sync_length.saturating_sub(other)
                    && estimated_sync_length - other < 7
                    && other % 8 == 0
                {
                    estimated_sync_length = other;
                    break;
                }
            }
        }

        // Now we look at all possible sync words with this length
        let mut sync_words: Vec<ScoredWord> = possible_sync_words
            .iter()
            .filter(|(word, _)| word.len() == estimated_sync_length)
            .cloned()
            .collect();

        let additional_syncs =
            self.find_additional_sync_words(estimated_sync_length, &sync_words, &possible_sync_words);

        // dict.update semantics: replace score of existing keys, append new
        for (word, score) in additional_syncs {
            match sync_words.iter_mut().find(|(w, _)| *w == word) {
                Some(entry) => entry.1 = score,
                None => sync_words.push((word, score)),
            }
        }

        // sort by score desc (stable: ties keep first-seen order)
        sync_words.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        sync_words.into_iter().map(|(word, _)| word).collect()
    }

    /// Look for additional sync words, in case we had varying preamble
    /// lengths and multiple sync words.
    fn find_additional_sync_words(
        &self,
        sync_length: usize,
        present_sync_words: &[ScoredWord],
        possible_sync_words: &[ScoredWord],
    ) -> Vec<ScoredWord> {
        let mut messages_without_sync: Vec<usize> = (0..self.bitvectors.len())
            .filter(|&i| {
                !present_sync_words
                    .iter()
                    .any(|(s, _)| {
                        !util::find_occurrences(&self.bitvectors[i], s, None, true).is_empty()
                    })
            })
            .collect();

        let mut result: Vec<ScoredWord> = Vec::new();
        if messages_without_sync.is_empty() {
            return result;
        }

        // Is there another sync word that applies to all messages without sync?
        let mut additional_candidates: Vec<ScoredWord> = possible_sync_words
            .iter()
            .filter(|(word, _)| {
                word.len() > sync_length
                    && !present_sync_words
                        .iter()
                        .any(|(s, _)| contains(word, s))
            })
            .cloned()
            .collect();
        additional_candidates.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });

        for (sync, score) in additional_candidates {
            if messages_without_sync.is_empty() {
                break;
            }

            let s = sync[..sync_length].to_vec();
            let matching: Vec<usize> = messages_without_sync
                .iter()
                .copied()
                .filter(|&i| {
                    !util::find_occurrences(&self.bitvectors[i], &s, None, true).is_empty()
                })
                .collect();
            if !matching.is_empty() {
                result.push((s, score));
                messages_without_sync.retain(|m| !matching.contains(m));
            }
        }

        result
    }

    /// Per message: `(start, lower - start, upper - start)` of the raw
    /// preamble estimate (all zeros when no preamble was found).
    pub fn get_raw_preamble_positions(&self) -> Vec<RawPreamblePosition> {
        self.bitvectors
            .iter()
            .map(|bitvector| match util::get_raw_preamble_position(bitvector) {
                Some((start, lower, upper)) => {
                    let start = start as i64;
                    let lower = lower as i64;
                    let upper = upper as i64;
                    (
                        start as u32,
                        (lower - start) as u32,
                        (upper - start) as u32,
                    )
                }
                None => (0, 0, 0),
            })
            .collect()
    }

    /// Matrix of the first difference index between all messages.
    pub fn get_difference_matrix(&self) -> Vec<Vec<u32>> {
        util::get_difference_matrix(&self.bitvectors)
    }
}

fn add_scored(map: &mut Vec<ScoredWord>, key: Vec<u8>, value: f64) {
    match map.iter_mut().find(|(k, _)| *k == key) {
        Some(entry) => entry.1 += value,
        None => map.push((key, value)),
    }
}

/// `needle in haystack` for bit vectors (Python `s in word` on bytes).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Python `bytes.find(sub, start, end)`: first index in `[start, end)` where
/// `sub` matches, or -1.
fn find_sub(haystack: &[u8], needle: &[u8], start: usize, end: usize) -> i64 {
    if needle.is_empty() {
        return start as i64;
    }
    let end = end.min(haystack.len());
    let mut i = start;
    while i + needle.len() <= end {
        if &haystack[i..i + needle.len()] == needle {
            return i as i64;
        }
        i += 1;
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(s: &str) -> Vec<u8> {
        s.chars().map(|c| if c == '1' { 1 } else { 0 }).collect()
    }

    fn num_bits(n: usize, width: usize) -> Vec<u8> {
        (0..width)
            .rev()
            .map(|i| ((n >> i) & 1) as u8)
            .collect()
    }

    fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.iter().flatten().copied().collect()
    }

    // --- tests ported from tests/awre/test_awre_preprocessing.py ---

    #[test]
    fn test_very_simple_sync_word_finding() {
        let preamble = "10101010";
        let sync = "1101";

        let bitvectors: Vec<Vec<u8>> = (0..20)
            .map(|i| {
                concat(&[bits(preamble), bits(sync), num_bits(10 * i, 8)])
            })
            .collect();

        let preprocessor = Preprocessor::new(bitvectors, vec![]);
        let possible_syncs = preprocessor.find_possible_syncs(None);
        assert!(!possible_syncs.is_empty());
        assert_eq!(possible_syncs[0], bits(sync));
    }

    #[test]
    fn test_simple_sync_word_finding() {
        let sync = "1001";

        let mut bitvectors = vec![];
        for i in 0..20 {
            bitvectors.push(concat(&[bits("10101010"), bits(sync), num_bits(10 * i, 8)]));
        }
        for i in 0..5 {
            bitvectors.push(concat(&[
                bits("10101010"),
                bits("1010"),
                bits(sync),
                num_bits(22 * i, 8),
            ]));
        }

        let preprocessor = Preprocessor::new(bitvectors, vec![]);
        let possible_syncs = preprocessor.find_possible_syncs(None);
        assert!(!possible_syncs.is_empty());
        assert_eq!(possible_syncs[0], bits(sync));
    }

    #[test]
    fn test_sync_word_finding_odd_preamble() {
        // With an odd preamble length, the last bit of the preamble is
        // counted towards the sync word.
        let mut bitvectors = vec![];
        for i in 0..20 {
            bitvectors.push(concat(&[bits("0101010"), bits("1101"), num_bits(10 * i, 8)]));
        }
        for i in 0..5 {
            bitvectors.push(concat(&[
                bits("0101010"),
                bits("10"),
                bits("1101"),
                num_bits(i, 8),
            ]));
        }

        let preprocessor = Preprocessor::new(bitvectors, vec![]);
        let possible_syncs = preprocessor.find_possible_syncs(None);
        assert_eq!(possible_syncs[0], bits("0110"));
    }

    #[test]
    fn test_sync_word_finding_special_preamble() {
        let mut bitvectors = vec![];
        for i in 0..20 {
            bitvectors.push(concat(&[
                bits("111001110011100"),
                bits("0110"),
                num_bits(10 * i, 8),
            ]));
        }
        for i in 0..5 {
            bitvectors.push(concat(&[
                bits("111001110011100"),
                bits("10"),
                bits("0110"),
                num_bits(i, 8),
            ]));
        }

        let preprocessor = Preprocessor::new(bitvectors, vec![]);
        let possible_syncs = preprocessor.find_possible_syncs(None);
        assert_eq!(possible_syncs[0], bits("0110"));
    }

    #[test]
    fn test_sync_word_finding_errored_preamble() {
        // first bits are wrong
        let preamble = "00010101010";
        let sync = "0110";
        let mut bitvectors = vec![];
        for i in 0..20 {
            bitvectors.push(concat(&[bits(preamble), bits(sync), num_bits(10 * i, 8)]));
        }
        for i in 0..5 {
            bitvectors.push(concat(&[
                bits(preamble),
                bits("10"),
                bits(sync),
                num_bits(i, 8),
            ]));
        }

        let preprocessor = Preprocessor::new(bitvectors, vec![]);
        let possible_syncs = preprocessor.find_possible_syncs(None);
        // last preamble bit + sync minus last bit
        assert!(possible_syncs.contains(&bits("0011")));
    }

    #[test]
    fn test_sync_word_finding_with_two_sync_words() {
        let preamble = "1010101010101010"; // 0xaaaa
        let sync1 = "0001001000110100"; // 0x1234
        let sync2 = "1100101011111110"; // 0xcafe

        let mut bitvectors = vec![];
        for i in 0..15 {
            bitvectors.push(concat(&[bits(preamble), bits(sync1), num_bits(12 * i, 8)]));
        }
        for i in 0..10 {
            bitvectors.push(concat(&[bits(preamble), bits(sync2), num_bits(16 * i, 8)]));
        }

        let preprocessor = Preprocessor::new(bitvectors, vec![]);
        let possible_syncs = preprocessor.find_possible_syncs(None);
        assert!(possible_syncs.len() >= 2);
        assert!(possible_syncs.contains(&bits(sync1)));
        assert!(possible_syncs.contains(&bits(sync2)));
    }

    #[test]
    fn test_sync_word_finding_common_prefix() {
        // Messages are very similar; they do not have two different sync
        // words — the algorithm needs to check for a common prefix.
        let preamble = "10".repeat(36);
        let sync = "0001001100110111"; // 0x1337
        let alice = "dead01";
        let bob = "beef24";

        let mut bitvectors = vec![];
        for i in 0..10usize {
            let (src, dst, data_length) = if i % 2 == 0 {
                (alice, bob, 8)
            } else {
                (bob, alice, 16)
            };
            let data = (i * 73 + 41) % (1 << data_length);
            bitvectors.push(concat(&[
                bits(&preamble),
                bits(sync),
                num_bits(data_length, 8),
                bits(src),
                bits(dst),
                num_bits(i, 16),
                num_bits(data, data_length),
            ]));
        }

        let preprocessor = Preprocessor::new(bitvectors, vec![]);
        let possible_syncs = preprocessor.find_possible_syncs(None);
        assert_eq!(possible_syncs.len(), 1);
        // sync + "0000" is okay, this gets fixed by correction in FormatFinder
        assert!(possible_syncs[0] == bits(sync) || possible_syncs[0] == concat(&[bits(sync), bits("0000")]));
    }

    #[test]
    fn test_with_given_preamble_and_sync() {
        let preamble = "10101010";
        let sync = "10011";

        let bitvectors: Vec<Vec<u8>> = (0..20)
            .map(|i| concat(&[bits(preamble), bits(sync), num_bits(10 * i, 8)]))
            .collect();

        let preprocessor = Preprocessor::new(bitvectors, vec![bits(sync)]);
        let (preamble_starts, preamble_lengths, sync_len) = preprocessor.preprocess();

        assert!(preamble_starts.iter().all(|&s| s == 0));
        assert!(preamble_lengths
            .iter()
            .all(|&l| l == preamble.len()));
        assert_eq!(sync_len, sync.len());
    }

    // --- real data tests (tests/data/*.txt bit files) ---

    fn read_bit_file(name: &str) -> Vec<Vec<u8>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../urh-ref/tests/data")
            .join(name);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {name}: {e}"));
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| bits(l.trim()))
            .collect()
    }

    #[test]
    fn test_real_data_enocean() {
        // EnOcean telegram. Note: verified against the original Python
        // Preprocessor on this very file: sync_len is 4 (the algorithm's
        // best candidate), not the 8-bit sync word of the EnOcean spec.
        let bitvectors = read_bit_file("enocean_bits.txt");
        assert!(bitvectors.len() >= 5);

        let preprocessor = Preprocessor::new(bitvectors, vec![]);
        let (preamble_starts, preamble_lengths, sync_len) = preprocessor.preprocess();

        assert!(preamble_starts.iter().all(|&s| s == 0));
        assert_eq!(sync_len, 4);
        assert!(preamble_lengths.iter().all(|&l| l == 8));
    }

    #[test]
    fn test_real_data_decoded_bits_smoke() {
        let bitvectors = read_bit_file("decoded_bits.txt");
        assert!(bitvectors.len() >= 10);

        // histogram over the real messages finds plausible common ranges
        let histogram = crate::awre::histogram::Histogram::new(bitvectors.clone(), None);
        let ranges = histogram.find_common_ranges(
            0.9,
            crate::awre::common_range::RangeType::Bit,
        );
        assert!(!ranges.is_empty());

        // preprocessing runs and reports a preamble at the message start
        let preprocessor = Preprocessor::new(bitvectors, vec![]);
        let (preamble_starts, _preamble_lengths, sync_len) = preprocessor.preprocess();
        assert!(preamble_starts.iter().all(|&s| s < 4));
        assert!(sync_len >= 4);
    }
}
