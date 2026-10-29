//! Port of `urh/awre/engines/SequenceNumberEngine.py`.

use std::collections::{BTreeSet, HashSet};

use crate::awre::common_range::CommonRange;
use crate::awre::util;

pub struct SequenceNumberEngine<'a> {
    /// bitvectors behind the synchronization
    bitvectors: &'a [Vec<u8>],
    n_gram_length: usize,
    minimum_score: f64,
    already_labeled_cols: HashSet<usize>,
}

/// Working copy of a CommonRange plus the Python `byte_order` tri-state
/// (None = unknown, "big", "little"). The frozen `CommonRange` type only has
/// `byte_order_big: bool`; unknown is normalized to big (the Python property
/// getter also reports "big" for unknown) at the end of `find`.
struct SeqRange {
    rng: CommonRange,
    byte_order: Option<bool>, // None = unknown
}

impl<'a> SequenceNumberEngine<'a> {
    pub fn new(
        bitvectors: &'a [Vec<u8>],
        n_gram_length: usize,
        minimum_score: f64,
        already_labeled: &[(usize, usize)],
    ) -> Self {
        let mut already_labeled_cols = HashSet::new();
        for &(start, end) in already_labeled {
            for e in start..end {
                already_labeled_cols.insert(e / n_gram_length);
            }
        }
        SequenceNumberEngine {
            bitvectors,
            n_gram_length,
            minimum_score,
            already_labeled_cols,
        }
    }

    pub fn find(&self) -> Vec<CommonRange> {
        let n = self.n_gram_length;

        if self.bitvectors.len() < 3 {
            // We need at least 3 bitvectors to properly find a sequence number
            return vec![];
        }

        let diff_matrix = util::create_seq_number_difference_matrix(self.bitvectors, n);
        let _num_rows = diff_matrix.len();
        let num_cols = diff_matrix.first().map_or(0, |r| r.len());

        // diff_frequencies_by_column: sorted keys (np.unique returns sorted values,
        // so ties in get_most_frequent resolve to the smaller difference)
        let mut diff_frequencies_by_column: Vec<Vec<(i32, usize)>> = Vec::with_capacity(num_cols);
        for j in 0..num_cols {
            let mut freqs: Vec<(i32, usize)> = Vec::new();
            for row in &diff_matrix {
                match freqs.iter_mut().find(|(v, _)| *v == row[j]) {
                    Some(e) => e.1 += 1,
                    None => freqs.push((row[j], 1)),
                }
            }
            freqs.sort_by_key(|(v, _)| *v);
            diff_frequencies_by_column.push(freqs);
        }

        let mut scores_by_column: Vec<(usize, f64)> = Vec::with_capacity(num_cols);
        for (column, frequencies) in diff_frequencies_by_column.iter().enumerate() {
            let score = if self.already_labeled_cols.contains(&column) {
                0.0
            } else {
                Self::calc_score(frequencies)
            };
            scores_by_column.push((column, score));
        }

        // sort by score descending; stable for equal scores (ascending columns)
        let mut candidates = scores_by_column.clone();
        candidates.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        let mut result: Vec<SeqRange> = Vec::new();
        for &(candidate_column, score) in &candidates {
            if score < self.minimum_score {
                continue;
            }

            let most_common_diff =
                Self::get_most_frequent(&diff_frequencies_by_column[candidate_column]);

            let mut message_indices: BTreeSet<usize> = BTreeSet::new();
            for (row, diffs) in diff_matrix.iter().enumerate() {
                // get all rows that have the most common difference or zero
                if diffs[candidate_column] == most_common_diff || diffs[candidate_column] == 0 {
                    // index i in diff matrix corresponds to messages i and i+1
                    message_indices.insert(row);
                    message_indices.insert(row + 1);
                }
            }

            // values in ascending message order, deduplicated
            let mut values: Vec<Vec<u8>> = Vec::new();
            for &i in &message_indices {
                let from = candidate_column * n;
                let to = ((candidate_column + 1) * n).min(self.bitvectors[i].len());
                if from < to {
                    let v = self.bitvectors[i][from..to].to_vec();
                    if !values.contains(&v) {
                        values.push(v);
                    }
                }
            }

            let matching: Vec<usize> = result
                .iter()
                .enumerate()
                .filter(|(_, r)| r.rng.message_indices == message_indices)
                .map(|(idx, _)| idx)
                .collect();

            // Try to extend an existing range on the right (big endian)
            if let Some(&idx) = matching.iter().find(|&&idx| {
                let r = &result[idx];
                r.rng.start as i64 == candidate_column as i64 * n as i64 - n as i64
                    && (r.byte_order.is_none() || r.byte_order == Some(true))
            }) {
                let r = &mut result[idx];
                r.rng.length += n;
                r.byte_order = Some(true);
                for v in values {
                    if !r.rng.values.contains(&v) {
                        r.rng.values.push(v);
                    }
                }
                continue;
            }

            // Try to extend an existing range on the left (little endian)
            if let Some(&idx) = matching.iter().find(|&&idx| {
                let r = &result[idx];
                r.rng.start == (candidate_column + 1) * n
                    && (r.byte_order.is_none() || r.byte_order == Some(false))
            }) {
                let r = &mut result[idx];
                r.rng.start -= n;
                r.rng.length += n;
                r.byte_order = Some(false);
                for v in values {
                    if !r.rng.values.contains(&v) {
                        r.rng.values.push(v);
                    }
                }
                continue;
            }

            let mut rng = CommonRange::new(candidate_column * n, n);
            rng.score = score;
            rng.field_type = "sequence number".to_string();
            rng.message_indices = message_indices;
            rng.values = values;
            result.push(SeqRange {
                rng,
                byte_order: None,
            });
        }

        // At least three different values needed to reliably identify a
        // sequence number; normalize unknown byte order to big
        result
            .into_iter()
            .filter(|r| {
                let mut unique: Vec<&Vec<u8>> = Vec::new();
                for v in &r.rng.values {
                    if !unique.contains(&v) {
                        unique.push(v);
                    }
                }
                unique.len() > 2
            })
            .map(|mut r| {
                r.rng.byte_order_big = r.byte_order != Some(false);
                r.rng
            })
            .collect()
    }

    /// Python `get_most_frequent`: most frequent difference excluding 0 and
    /// -1; first (smallest) key wins ties.
    pub fn get_most_frequent(diff_frequencies: &[(i32, usize)]) -> i32 {
        diff_frequencies
            .iter()
            .filter(|(v, _)| *v != 0 && *v != -1)
            .max_by(|a, b| {
                a.1.cmp(&b.1)
                    .then(b.0.cmp(&a.0)) // smaller key on tie
            })
            .map(|(v, _)| *v)
            .unwrap_or(-1)
    }

    /// Calculate the score based on the distribution of differences:
    /// 1. high if one constant (!= zero) dominates
    /// 2. other constants (!= zero) lower the score, zero means the sequence
    ///    number stays the same for some messages
    pub fn calc_score(diff_frequencies: &[(i32, usize)]) -> f64 {
        let total: usize = diff_frequencies.iter().map(|(_, c)| c).sum();
        let num_zeros: usize = diff_frequencies
            .iter()
            .filter(|(v, _)| *v == 0)
            .map(|(_, c)| c)
            .sum();
        if num_zeros == total {
            return 0.0;
        }

        if !diff_frequencies
            .iter()
            .any(|(v, _)| *v != 0 && *v != -1)
        {
            return 0.0;
        }

        let most_frequent = Self::get_most_frequent(diff_frequencies);
        let freq = diff_frequencies
            .iter()
            .find(|(v, _)| *v == most_frequent)
            .map(|(_, c)| *c)
            .unwrap_or(0);
        freq as f64 / (total - num_zeros) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awre::auto_assigner::Participant;
    use crate::awre::format_finder::FormatFinder;
    use crate::awre::message_type_builder::{FieldFunction, MessageTypeBuilder};
    use crate::awre::protocol_generator::{test_util, ProtocolGenerator};

    /// test_sequence_number_engine.py::test_simple_protocol
    #[test]
    fn test_simple_protocol() {
        let mut mb = MessageTypeBuilder::new("simple_seq_test");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::SequenceNumber, 8, None);

        let num_messages = 20;
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![],
            &[],
            &["0x9a9d"],
            false,
            true,
        );
        for _ in 0..num_messages {
            pg.generate_message(None, "0xcafe", None, None);
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let mut ff = FormatFinder::new(messages);

        let seq_engine = SequenceNumberEngine::new(&ff.bitvectors, 8, 0.75, &[]);
        let highscored_ranges = seq_engine.find();
        assert_eq!(highscored_ranges.len(), 1);

        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 1);
        assert!(!message_types[0].is_empty());
        assert_eq!(
            message_types[0].num_labels_with_type(FieldFunction::SequenceNumber),
            1
        );
        let label = message_types[0]
            .get_first_label_with_type(FieldFunction::SequenceNumber)
            .unwrap();
        assert_eq!(label.label.start, 24);
        assert_eq!(label.label.end - label.label.start, 8);
    }

    /// test_sequence_number_engine.py::test_16bit_seq_nr
    #[test]
    fn test_16bit_seq_nr() {
        let mut mb = MessageTypeBuilder::new("16bit_seq_test");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::SequenceNumber, 16, None);

        let num_messages = 10;
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![],
            &[],
            &["0x9a9d"],
            false,
            true,
        )
        .with_sequence_numbers(&[], 64);

        for _ in 0..num_messages {
            pg.generate_message(None, "0xcafe", None, None);
        }

        let bitvectors = FormatFinder::get_bitvectors_from_messages(
            &pg.messages,
            Some(&vec![24; num_messages]),
        );
        let seq_engine = SequenceNumberEngine::new(&bitvectors, 8, 0.75, &[]);
        let highscored_ranges = seq_engine.find();
        assert_eq!(highscored_ranges.len(), 1);

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let mut ff = FormatFinder::new(messages);
        ff.perform_iteration();

        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 1);
        assert!(!message_types[0].is_empty());
        assert_eq!(
            message_types[0].num_labels_with_type(FieldFunction::SequenceNumber),
            1
        );
        let label = message_types[0]
            .get_first_label_with_type(FieldFunction::SequenceNumber)
            .unwrap();
        assert_eq!(label.label.start, 24);
        assert_eq!(label.label.end - label.label.start, 16);
    }

    /// test_sequence_number_engine.py::test_16bit_seq_nr_with_zeros_in_first_part
    #[test]
    fn test_16bit_seq_nr_with_zeros_in_first_part() {
        let mut mb = MessageTypeBuilder::new("16bit_seq_first_byte_zero_test");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        mb.add_label(FieldFunction::SequenceNumber, 16, None);

        let num_messages = 10;
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![],
            &[],
            &["0x9a9d"],
            false,
            true,
        );

        for i in 0..num_messages {
            let data = format!("0xcafe{}", "abc".repeat(i));
            pg.generate_message(None, &data, None, None);
        }

        let bitvectors = FormatFinder::get_bitvectors_from_messages(
            &pg.messages,
            Some(&vec![24; num_messages]),
        );
        let seq_engine = SequenceNumberEngine::new(&bitvectors, 8, 0.75, &[]);
        let highscored_ranges = seq_engine.find();
        assert_eq!(highscored_ranges.len(), 1);

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let mut ff = FormatFinder::new(messages);
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 1);
        assert!(!message_types[0].is_empty());
        assert_eq!(
            message_types[0].num_labels_with_type(FieldFunction::SequenceNumber),
            1
        );
        let label = message_types[0]
            .get_first_label_with_type(FieldFunction::SequenceNumber)
            .unwrap();

        // Not consider constants as part of SEQ Nr!
        assert_eq!(label.label.start, 40);
        assert_eq!(label.label.end - label.label.start, 8);
    }

    /// test_sequence_number_engine.py::test_no_sequence_number
    #[test]
    fn test_no_sequence_number() {
        let alice = Participant::with_address("Alice", "dead");
        let bob = Participant::with_address("Bob", "beef");

        let mut mb = MessageTypeBuilder::new("protocol_with_one_message_type");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        mb.add_label(FieldFunction::SrcAddress, 16, None);
        mb.add_label(FieldFunction::DstAddress, 16, None);

        let num_messages = 3;
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![alice.clone(), bob.clone()],
            &[],
            &["0x1337"],
            false,
            true,
        );

        for i in 0..num_messages {
            let (source, destination) = if i % 2 == 0 {
                (&alice, &bob)
            } else {
                (&bob, &alice)
            };
            pg.generate_message(None, "", Some(source), Some(destination));
        }

        // Delete message type information -> no prior knowledge
        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);

        let mut ff = FormatFinder::with_participants(
            messages,
            vec![],
            vec![alice, bob],
            pg.participant_indices.clone(),
        );
        ff.known_participant_addresses.clear();
        ff.perform_iteration();

        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 1);
        assert_eq!(
            message_types[0].num_labels_with_type(FieldFunction::SequenceNumber),
            0
        );
    }

    /// test_sequence_number_engine.py::test_sequence_number_little_endian_16_bit
    #[test]
    fn test_sequence_number_little_endian_16_bit() {
        let mut mb = MessageTypeBuilder::new("16bit_seq_test");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::SequenceNumber, 16, None);

        let num_messages = 8;
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![],
            &[],
            &["0x9a9d"],
            true, // little endian
            true,
        )
        .with_sequence_numbers(&[], 64);

        for _ in 0..num_messages {
            pg.generate_message(None, "0xcafe", None, None);
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let mut ff = FormatFinder::new(messages);
        ff.perform_iteration();

        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 1);
        assert_eq!(
            message_types[0].num_labels_with_type(FieldFunction::SequenceNumber),
            1
        );
        let label = message_types[0]
            .get_first_label_with_type(FieldFunction::SequenceNumber)
            .unwrap();
        assert_eq!(label.label.start, 24);
        assert_eq!(label.label.end - label.label.start, 16);
    }
}
