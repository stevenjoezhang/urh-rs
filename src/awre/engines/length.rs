//! Port of `urh/awre/engines/LengthEngine.py`.

use std::collections::BTreeSet;

use crate::awre::common_range::{CommonRange, RangeType};
use crate::awre::engines::{
    find_common_ranges_by_cluster, ignore_already_labeled, Clusters, RangesByCluster,
};
use crate::iq::bit_array_to_number;

pub struct LengthEngine<'a> {
    /// bitvectors behind the synchronization
    bitvectors: &'a [Vec<u8>],
    already_labeled: Vec<(usize, usize)>,
}

impl<'a> LengthEngine<'a> {
    pub fn new(bitvectors: &'a [Vec<u8>], already_labeled: Vec<(usize, usize)>) -> Self {
        LengthEngine {
            bitvectors,
            already_labeled,
        }
    }

    pub fn find(&self, n_gram_length: usize, minimum_score: f64) -> Vec<CommonRange> {
        // Consider the n_gram_length
        let mut bitvectors_by_n_gram_length: Clusters<usize> = Vec::new();
        for (i, bitvector) in self.bitvectors.iter().enumerate() {
            let bin_num = bitvector.len().div_ceil(n_gram_length);
            match bitvectors_by_n_gram_length
                .iter_mut()
                .find(|(k, _)| *k == bin_num)
            {
                Some((_, v)) => v.push(i),
                None => bitvectors_by_n_gram_length.push((bin_num, vec![i])),
            }
        }

        let mut common_ranges_by_length =
            find_common_ranges_by_cluster(self.bitvectors, &bitvectors_by_n_gram_length, 0.7, RangeType::Bit);

        for (_, ranges) in common_ranges_by_length.iter_mut() {
            *ranges = ignore_already_labeled(ranges, &self.already_labeled);
        }

        Self::filter_common_ranges(&mut common_ranges_by_length);

        let scored_ranges = Self::score_ranges(&common_ranges_by_length, n_gram_length);

        // Take the ranges with highest score per cluster if their score
        // surpasses the minimum score
        let high_scores_by_length = self.choose_high_scored_ranges(
            scored_ranges,
            &bitvectors_by_n_gram_length,
            minimum_score,
        );

        high_scores_by_length.into_iter().map(|(_, r)| r).collect()
    }

    /// Ranges must be common along length clusters but their values must
    /// differ, so rule out all ranges that are (1) common across clusters
    /// AND (2) have the same value.
    fn filter_common_ranges(common_ranges_by_length: &mut RangesByCluster<usize>) {
        let ranges: Vec<CommonRange> = common_ranges_by_length
            .iter()
            .flat_map(|(_, r)| r.iter().cloned())
            .collect();

        for rng in &ranges {
            let count = ranges
                .iter()
                .filter(|r| {
                    rng.start == r.start
                        && rng.length == r.length
                        && rng.values.first().map(|v| v.as_slice())
                            == r.values.first().map(|v| v.as_slice())
                })
                .count();
            if count < 2 {
                continue;
            }

            for (_, cluster_ranges) in common_ranges_by_length.iter_mut() {
                cluster_ranges.retain(|r| {
                    !(r.start == rng.start && r.length == rng.length && r.field_type == rng.field_type)
                });
            }
        }
    }

    fn score_ranges(
        common_ranges_by_length: &RangesByCluster<usize>,
        n_gram_length: usize,
    ) -> Vec<(usize, Vec<(usize, Vec<CommonRange>)>)> {
        // The window length must be smaller than the common range's length
        // and is something like 8 in case of an 8 bit integer.
        let window_lengths: Vec<usize> = if n_gram_length == 8 {
            vec![8, 16, 32, 64]
        } else {
            (1..5).map(|i| n_gram_length * i).collect()
        };

        let mut scored_ranges: Vec<(usize, Vec<(usize, Vec<CommonRange>)>)> = Vec::new();
        for (length, _common_ranges) in common_ranges_by_length {
            scored_ranges.push((
                *length,
                window_lengths
                    .iter()
                    .map(|wl| (*wl, Vec::new()))
                    .collect(),
            ));
        }

        let byteorders: Vec<bool> = if n_gram_length == 8 {
            vec![true, false] // big, little
        } else {
            vec![true]
        };

        for &window_length in &window_lengths {
            for (length, common_ranges) in common_ranges_by_length {
                for common_range in common_ranges
                    .iter()
                    .filter(|cr| cr.length >= window_length)
                {
                    let bits: &[u8] = common_range
                        .values
                        .first()
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);

                    let mut rng_byte_order = true;
                    let mut max_score = -1.0f64;
                    let mut max_start: i64 = -1;

                    if bits.len() + 1 >= window_length {
                        let mut start: i64 = 0;
                        while (start as usize) + window_length <= bits.len() {
                            for &big in &byteorders {
                                let score = Self::score_bits(
                                    &bits[start as usize..start as usize + window_length],
                                    *length,
                                    start as usize,
                                    big,
                                );
                                if score > max_score {
                                    max_score = score;
                                    max_start = start;
                                    rng_byte_order = big;
                                }
                            }
                            start += n_gram_length as i64;
                        }
                    }

                    let mut rng = CommonRange::new(
                        (common_range.start as i64 + max_start).max(0) as usize,
                        window_length,
                    );
                    rng.values = vec![common_range
                        .values
                        .first()
                        .map(|v| {
                            v.get(max_start as usize..max_start as usize + window_length)
                                .map(|s| s.to_vec())
                                .unwrap_or_default()
                        })
                        .unwrap_or_default()];
                    rng.score = max_score;
                    rng.field_type = "length".to_string();
                    rng.message_indices = common_range.message_indices.clone();
                    rng.range_type = common_range.range_type;
                    rng.byte_order_big = rng_byte_order;

                    let entry = scored_ranges
                        .iter_mut()
                        .find(|(l, _)| l == length)
                        .unwrap();
                    entry
                        .1
                        .iter_mut()
                        .find(|(wl, _)| *wl == window_length)
                        .unwrap()
                        .1
                        .push(rng);
                }
            }
        }

        scored_ranges
    }

    fn choose_high_scored_ranges(
        &self,
        mut scored_ranges: Vec<(usize, Vec<(usize, Vec<CommonRange>)>)>,
        bitvectors_by_n_gram_length: &Clusters<usize>,
        minimum_score: f64,
    ) -> Vec<(usize, CommonRange)> {
        // Set for every window length the highest scored range as candidate
        let mut possible_window_lengths: Vec<(usize, usize)> = Vec::new(); // (window_length, count)
        for (_, ranges_by_window_length) in scored_ranges.iter_mut() {
            for (window_length, ranges) in ranges_by_window_length.iter_mut() {
                // Python: max(filter(score >= minimum), key=score) -> first max
                let best = ranges
                    .iter()
                    .filter(|x| x.score >= minimum_score)
                    .enumerate()
                    .max_by(|(ia, a), (ib, b)| {
                        a.score
                            .partial_cmp(&b.score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then(ib.cmp(ia)) // first max wins
                    })
                    .map(|(_, x)| x.clone());
                match best {
                    Some(best) => {
                        *ranges = vec![best];
                        match possible_window_lengths
                            .iter_mut()
                            .find(|(wl, _)| wl == window_length)
                        {
                            Some(e) => e.1 += 1,
                            None => possible_window_lengths.push((*window_length, 1)),
                        }
                    }
                    None => *ranges = Vec::new(),
                }
            }
        }

        // Choose window length that has a result most often, greater on tie
        let chosen_window_length = match possible_window_lengths
            .iter()
            .max_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)))
        {
            Some(&(wl, _)) => wl,
            None => return Vec::new(),
        };

        let mut high_scores_by_length: Vec<(usize, CommonRange)> = Vec::new();

        // Choose all ranges with highest score per cluster if score
        // surpasses the minimum score
        for (length, ranges_by_window_length) in &scored_ranges {
            if let Some((_, ranges)) = ranges_by_window_length
                .iter()
                .find(|(wl, _)| *wl == chosen_window_length)
            {
                if let Some(rng) = ranges.first() {
                    high_scores_by_length.push((*length, rng.clone()));
                }
            }
        }

        // If there are length clusters with only one message see if we can
        // assign a range from other clusters
        for (length, msg_indices) in bitvectors_by_n_gram_length {
            if msg_indices.len() != 1 {
                continue;
            }
            let msg_index = msg_indices[0];
            let bitvector = &self.bitvectors[msg_index];
            let mut max_score = 0.0f64;
            let mut best_match: Option<CommonRange> = None;

            for (_, rng) in &high_scores_by_length {
                let end = (rng.end() + 1).min(bitvector.len());
                if rng.start < end {
                    let bits = &bitvector[rng.start..end];
                    let score = Self::score_bits(bits, *length, rng.start, true);
                    if score > max_score {
                        max_score = score;
                        best_match = Some(rng.clone());
                    }
                }
            }

            if let Some(best) = best_match {
                let end = (best.end() + 1).min(bitvector.len());
                let mut rng = CommonRange::new(best.start, best.length);
                rng.values = vec![bitvector[best.start..end].to_vec()];
                rng.score = max_score;
                rng.field_type = "length".to_string();
                rng.message_indices = BTreeSet::from([msg_index]);
                rng.range_type = RangeType::Bit;
                high_scores_by_length.push((*length, rng));
            }
        }

        high_scores_by_length
    }

    pub fn score_bits(bits: &[u8], target_length: usize, position: usize, byte_order_big: bool) -> f64 {
        let mut value = bit_array_to_number(bits, bits.len() as i64, 0);
        if !byte_order_big && bits.len() > 8 && bits.len().is_multiple_of(8) {
            let n = bits.len() / 8;
            let bytes = value.to_be_bytes(); // big endian bytes of value
            // int.from_bytes(value.to_bytes(n, "big"), "little")
            let le: [u8; 8] = {
                let mut b = [0u8; 8];
                for i in 0..n {
                    b[i] = bytes[8 - n + i];
                }
                b
            };
            value = u64::from_le_bytes(le);
        }

        // Length field should be at front, so we give lower scores for large starts
        let f = 1.0 / (1.0 + 0.25 * position as f64);

        f * Self::gauss(value as f64, target_length as f64)
    }

    fn gauss(x: f64, mu: f64) -> f64 {
        let sigma = 2.0;
        (-0.5 * ((x - mu) / sigma).powi(2)).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awre::format_finder::FormatFinder;
    use crate::awre::message_type_builder::{FieldFunction, MessageTypeBuilder};
    use crate::awre::protocol_generator::{test_util, ProtocolGenerator};

    /// test_length_engine.py::test_simple_protocol
    #[test]
    fn test_simple_protocol() {
        let mut mb = MessageTypeBuilder::new("simple_length_test");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);

        let num_messages_by_data_length = [(8usize, 5usize), (16, 10), (32, 15)];
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![],
            &[],
            &["0x9a9d"],
            false,
            true,
        );
        let mut rng = test_util::Rng::new(0);
        for &(data_length, num_messages) in &num_messages_by_data_length {
            for _ in 0..num_messages {
                let data = rng.random_bits(data_length);
                pg.generate_message(None, &data, None, None);
            }
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let ff = FormatFinder::new(messages);

        let length_engine = LengthEngine::new(&ff.bitvectors, vec![]);
        let highscored_ranges = length_engine.find(8, 0.1);
        assert_eq!(highscored_ranges.len(), 3);

        let mut ff = ff;
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 1);
        assert!(!message_types[0].is_empty());
        let label = message_types[0]
            .get_first_label_with_type(FieldFunction::Length)
            .unwrap();
        assert_eq!(label.label.start, 24);
        assert_eq!(label.label.end - label.label.start, 8);
    }

    /// test_length_engine.py::test_easy_protocol
    #[test]
    fn test_easy_protocol() {
        let mut mb = MessageTypeBuilder::new("easy_length_test");
        mb.add_label(FieldFunction::Preamble, 16, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        mb.add_label(FieldFunction::SequenceNumber, 8, None);

        let num_messages_by_data_length = [(32usize, 10usize), (64, 15), (16, 5), (24, 7)];
        let preamble = "10".repeat(8);
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![],
            &[&preamble],
            &["0xcafe"],
            false,
            true,
        );
        for &(data_length, num_messages) in &num_messages_by_data_length {
            for i in 0..num_messages {
                let data = if i % 4 == 0 {
                    "1".repeat(data_length)
                } else if i % 4 == 1 {
                    "0".repeat(data_length)
                } else if i % 4 == 2 {
                    "10".repeat(data_length / 2)
                } else {
                    "01".repeat(data_length / 2)
                };
                pg.generate_message(None, &data, None, None);
            }
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let ff = FormatFinder::new(messages);

        let length_engine = LengthEngine::new(&ff.bitvectors, vec![]);
        let highscored_ranges = length_engine.find(8, 0.1);
        assert_eq!(highscored_ranges.len(), 4);

        let mut ff = ff;
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 1);
        assert!(!message_types[0].is_empty());
        let label = message_types[0]
            .get_first_label_with_type(FieldFunction::Length)
            .unwrap();
        assert_eq!(label.label.start, 32);
        assert_eq!(label.label.end - label.label.start, 8);
    }

    /// test_length_engine.py::test_medium_protocol
    #[test]
    fn test_medium_protocol() {
        let mut mb1 = MessageTypeBuilder::new("data");
        mb1.add_label(FieldFunction::Preamble, 8, None);
        mb1.add_label(FieldFunction::Sync, 8, None);
        mb1.add_label(FieldFunction::Length, 8, None);
        mb1.add_label(FieldFunction::SequenceNumber, 8, None);

        let mut mb2 = MessageTypeBuilder::new("ack");
        mb2.add_label(FieldFunction::Preamble, 8, None);
        mb2.add_label(FieldFunction::Sync, 8, None);

        let mut pg = ProtocolGenerator::new(
            vec![mb1.message_type.clone(), mb2.message_type.clone()],
            vec![],
            &[],
            &["11110011", "11110011"],
            false,
            true,
        );
        let num_messages_by_data_length = [(8usize, 5usize), (16, 10), (32, 5)];
        for &(data_length, num_messages) in &num_messages_by_data_length {
            for i in 0..num_messages {
                let data = pg.decimal_to_bits(10 * i as u64, data_length);
                pg.generate_message(Some(0), &data, None, None);
                pg.generate_message(Some(1), "0xaf", None, None);
            }
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let mut ff = FormatFinder::new(messages);

        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 2);
        let length_mt = message_types
            .iter()
            .find(|mt| mt.get_first_label_with_type(FieldFunction::Length).is_some())
            .unwrap();
        let length_label = length_mt.get_first_label_with_type(FieldFunction::Length).unwrap();

        for (i, sync_end) in ff.sync_ends.iter().enumerate() {
            assert_eq!(*sync_end, 16, "message {}", i);
        }

        assert_eq!(16, length_label.label.start);
        assert_eq!(8, length_label.label.end - length_label.label.start);
    }

    /// test_length_engine.py::test_little_endian_16_bit
    #[test]
    fn test_little_endian_16_bit() {
        let mut mb = MessageTypeBuilder::new("little_endian_16_length_test");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 16, None);

        let num_messages_by_data_length = [(256 * 8usize, 5usize), (16, 4), (512, 2)];
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![],
            &[],
            &["0x9a9d"],
            true, // little endian
            true,
        );
        let mut rng = test_util::Rng::new(0);
        for &(data_length, num_messages) in &num_messages_by_data_length {
            for _ in 0..num_messages {
                let data = rng.random_bits(data_length);
                pg.generate_message(None, &data, None, None);
            }
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let ff = FormatFinder::new(messages);

        let length_engine = LengthEngine::new(&ff.bitvectors, vec![]);
        let highscored_ranges = length_engine.find(8, 0.1);
        assert_eq!(highscored_ranges.len(), 3);

        let mut ff = ff;
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 1);
        assert!(!message_types[0].is_empty());
        let label = message_types[0]
            .get_first_label_with_type(FieldFunction::Length)
            .unwrap();
        assert_eq!(label.label.start, 24);
        assert_eq!(label.label.end - label.label.start, 16);
    }
}
