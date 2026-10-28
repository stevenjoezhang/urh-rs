//! Port of `urh/awre/engines/ChecksumEngine.py`, including the parts of
//! `urh/util/GenericCRC.py` (`guess_all`,
//! `guess_standard_parameters_and_datarange`) and
//! `urh/util/WSPChecksum.py` (`search_for_wsp_checksum`) that the engine
//! needs and that are not part of the frozen `encoding.rs` API.

use std::collections::{BTreeSet, HashSet};

use crate::awre::common_range::CommonRange;
use crate::awre::engines::Clusters;
use crate::awre::util;
use crate::encoding::{self, GenericCRC, STANDARD_CHECKSUMS};

/// The checksum algorithm attached to a found checksum field.
/// Python uses `GenericCRC` or `WSPChecksum` instances.
#[derive(Clone)]
pub enum ChecksumSpec {
    Wsp,
    Crc(GenericCRC),
}

impl std::fmt::Debug for ChecksumSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChecksumSpec::Wsp => write!(f, "Wsp"),
            ChecksumSpec::Crc(c) => write!(f, "Crc({})", c.polynomial_as_hex_str()),
        }
    }
}

impl ChecksumSpec {
    fn crc(crc: &GenericCRC) -> Self {
        ChecksumSpec::Crc(crc.clone())
    }

    pub fn as_crc(&self) -> Option<&GenericCRC> {
        match self {
            ChecksumSpec::Wsp => None,
            ChecksumSpec::Crc(c) => Some(c),
        }
    }

    pub fn is_wsp(&self) -> bool {
        matches!(self, ChecksumSpec::Wsp)
    }

    /// Python `GenericCRC.__eq__`-ish: compare by parameters.
    pub fn spec_eq(a: &ChecksumSpec, b: &ChecksumSpec) -> bool {
        match (a, b) {
            (ChecksumSpec::Wsp, ChecksumSpec::Wsp) => true,
            (ChecksumSpec::Crc(x), ChecksumSpec::Crc(y)) => crc_eq(x, y),
            _ => false,
        }
    }
}

fn crc_eq(x: &GenericCRC, y: &GenericCRC) -> bool {
    x.polynomial == y.polynomial
        && x.start_value == y.start_value
        && x.final_xor == y.final_xor
        && x.reverse_polynomial == y.reverse_polynomial
        && x.reverse_all == y.reverse_all
        && x.little_endian == y.little_endian
        && x.lsb_first == y.lsb_first
}

/// Port of `CommonRange.ChecksumRange`: a common range plus the data range
/// the CRC is computed over and the CRC parameters.
#[derive(Clone, Debug)]
pub struct ChecksumRange {
    pub range: CommonRange,
    pub data_range_start: usize,
    pub data_range_end: usize,
    pub crc: ChecksumSpec,
}

impl ChecksumRange {
    /// Python `ChecksumRange.__eq__`.
    pub fn eq_range(a: &ChecksumRange, b: &ChecksumRange) -> bool {
        crate::awre::engines::rng_eq(&a.range, &b.range)
            && a.data_range_start == b.data_range_start
            && a.data_range_end == b.data_range_end
            && ChecksumSpec::spec_eq(&a.crc, &b.crc)
    }
}

pub struct ChecksumEngine<'a> {
    /// bitvectors behind the synchronization
    bitvectors: &'a [Vec<u8>],
    n_gram_length: usize,
    minimum_score: f64,
    already_labeled_cols: HashSet<usize>,
}

impl<'a> ChecksumEngine<'a> {
    pub fn new(
        bitvectors: &'a [Vec<u8>],
        n_gram_length: usize,
        minimum_score: f64,
        already_labeled: &[(usize, usize)],
    ) -> Self {
        let mut already_labeled_cols = HashSet::new();
        for &(start, end) in already_labeled {
            for e in start..end {
                already_labeled_cols.insert(e);
            }
        }
        ChecksumEngine {
            bitvectors,
            n_gram_length,
            minimum_score,
            already_labeled_cols,
        }
    }

    pub fn find(&self) -> Vec<ChecksumRange> {
        let mut result: Vec<ChecksumRange> = Vec::new();

        let mut bitvectors_by_n_gram_length: Clusters<usize> = Vec::new();
        for (i, bitvector) in self.bitvectors.iter().enumerate() {
            let bin_num =
                (bitvector.len() + self.n_gram_length - 1) / self.n_gram_length;
            match bitvectors_by_n_gram_length
                .iter_mut()
                .find(|(k, _)| *k == bin_num)
            {
                Some((_, v)) => v.push(i),
                None => bitvectors_by_n_gram_length.push((bin_num, vec![i])),
            }
        }

        for (_length, message_indices) in &bitvectors_by_n_gram_length {
            let mut checksums_for_length: Vec<ChecksumRange> = Vec::new();
            let score = 1.0 / message_indices.len() as f64;

            for &index in message_indices {
                let bits = &self.bitvectors[index];

                let wsp = search_for_wsp_checksum(bits);
                if wsp != (0, 0, 0, 0) {
                    let (data_start, data_stop, crc_start, crc_stop) = wsp;
                    let checksum_range = ChecksumRange {
                        range: Self::make_range(crc_start, crc_stop - crc_start, score, index),
                        data_range_start: data_start,
                        data_range_end: data_stop,
                        crc: ChecksumSpec::Wsp,
                    };
                    match checksums_for_length
                        .iter_mut()
                        .find(|c| ChecksumRange::eq_range(c, &checksum_range))
                    {
                    Some(present) => {
                    present.range.message_indices.insert(index);
                }
                        None => checksums_for_length.push(checksum_range),
                    }
                    continue;
                }

                let Some((crc_spec, data_start, data_stop, crc_start, crc_stop)) =
                    guess_all(bits, &self.already_labeled_cols)
                else {
                    continue;
                };

                let checksum_range = ChecksumRange {
                    range: Self::make_range(crc_start, crc_stop - crc_start, score, index),
                    data_range_start: data_start,
                    data_range_end: data_stop,
                    crc: crc_spec,
                };

                if let Some(present) = checksums_for_length
                    .iter_mut()
                    .find(|rng| ChecksumRange::eq_range(rng, &checksum_range))
                {
                    present.range.message_indices.insert(index);
                    continue;
                }

                checksums_for_length.push(checksum_range);

                if let Some(crc) = checksums_for_length
                    .last()
                    .unwrap()
                    .crc
                    .as_crc()
                    .cloned()
                {
                    let matching = util::check_crc_for_messages(
                        message_indices,
                        self.bitvectors,
                        data_start,
                        data_stop,
                        crc_start,
                        crc_stop,
                        &crc.polynomial,
                        &crc.start_value,
                        &crc.final_xor,
                        crc.lsb_first,
                        crc.reverse_polynomial,
                        crc.reverse_all,
                        crc.little_endian,
                    );
                    checksums_for_length
                        .last_mut()
                        .unwrap()
                        .range
                        .message_indices
                        .extend(matching);
                }
            }

            // Score ranges
            for rng in checksums_for_length.iter_mut() {
                rng.range.score =
                    rng.range.message_indices.len() as f64 / message_indices.len() as f64;
            }

            // Python max(..., key=score): first max wins
            if let Some(best) = checksums_for_length
                .iter()
                .enumerate()
                .max_by(|(ia, a), (ib, b)| {
                    a.range
                        .score
                        .partial_cmp(&b.range.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(ib.cmp(ia))
                })
            {
                result.push(best.1.clone());
            }
        }

        let Some(max_scored) = result
            .iter()
            .enumerate()
            .filter(|(_, x)| {
                x.range.message_indices.len() >= 2 && x.range.score >= self.minimum_score
            })
            .max_by(|(ia, a), (ib, b)| {
                a.range
                    .score
                    .partial_cmp(&b.range.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(ib.cmp(ia))
            })
            .map(|(_, x)| x.clone())
        else {
            return vec![];
        };

        result.retain(|x| ChecksumSpec::spec_eq(&x.crc, &max_scored.crc));

        result
    }

    fn make_range(start: usize, length: usize, score: f64, index: usize) -> CommonRange {
        let mut range = CommonRange::new(start, length);
        range.score = score;
        range.field_type = "checksum".to_string();
        range.message_indices = BTreeSet::from([index]);
        range
    }
}

/// Port of `WSPChecksum.search_for_wsp_checksum`.
fn search_for_wsp_checksum(bits_behind_sync: &[u8]) -> (usize, usize, usize, usize) {
    if bits_behind_sync.len() < 8 {
        // Python would index negatively here; treat as no checksum
        return (0, 0, 0, 0);
    }
    if bits_behind_sync[bits_behind_sync.len() - 4..] != [1, 0, 1, 1] {
        // Check for EOF
        return (0, 0, 0, 0);
    }

    let rorg = &bits_behind_sync[0..4];
    if rorg == [0, 1, 0, 1] || rorg == [0, 1, 1, 0] {
        // Switch telegram
        let n = bits_behind_sync.len();
        if wsp_checksum4(&bits_behind_sync[n - 8..]) == bits_behind_sync[n - 8..n - 4] {
            return (0, n - 8, n - 8, n - 4);
        }
    }

    (0, 0, 0, 0)
}

/// Port of `WSPChecksum.checksum4`.
fn wsp_checksum4(bits: &[u8]) -> Vec<u8> {
    let mut val = bits.to_vec();
    let n = val.len();
    for b in &mut val[n - 4..] {
        *b = 0;
    }
    let mut hash: u32 = 0;
    for chunk in val.chunks(8) {
        let mut byte = 0u32;
        for &b in chunk {
            byte = (byte << 1) | b as u32;
        }
        hash += byte;
    }
    hash = (((hash & 0xF0) >> 4) + (hash & 0x0F)) & 0x0F;
    (0..4)
        .rev()
        .map(|i| ((hash >> i) & 1) as u8)
        .collect()
}

/// Port of `GenericCRC.guess_all`. Returns
/// `(crc, data_range_start, data_range_end, crc_start, crc_end)`.
fn guess_all(
    bits: &[u8],
    ignore_positions: &HashSet<usize>,
) -> Option<(ChecksumSpec, usize, usize, usize, usize)> {
    let trash_max = 7;
    for trash in 0..trash_max {
        let Some((name, data_begin, data_end)) =
            guess_standard_parameters_and_datarange(bits, trash)
        else {
            continue;
        };

        let poly_order = polynomial_len(name) + 1;
        if bits.len() + 1 >= trash + poly_order {
            let crc_start = bits.len() - trash - poly_order + 1;
            let crc_end = bits.len() - trash;
            if !(crc_start..crc_end).any(|i| ignore_positions.contains(&i)) {
                let crc = GenericCRC::from_standard_checksum(name).unwrap();
                return Some((ChecksumSpec::crc(&crc), data_begin, data_end, crc_start, crc_end));
            }
        }
    }
    None
}

/// length of the polynomial bit vector (without the leading 1) of a
/// standard checksum
fn polynomial_len(name: &str) -> usize {
    let p = STANDARD_CHECKSUMS.iter().find(|(n, _)| *n == name).unwrap();
    encoding::hex2bits(p.1.polynomial_hex).len()
}

/// Port of `GenericCRC.guess_standard_parameters_and_datarange`:
/// test standard parameters (longer polynomials first) and determine the
/// start and end of the crc data range (end is set before the crc).
fn guess_standard_parameters_and_datarange(
    inpt: &[u8],
    trash: usize,
) -> Option<(&'static str, usize, usize)> {
    // Test longer polynomials first, because smaller polynomials have a
    // higher risk of false positives. Python's sorted() is stable, so equal
    // lengths keep the dict order of STANDARD_CHECKSUMS.
    let mut checks: Vec<&(&'static str, crate::encoding::CrcParams)> =
        STANDARD_CHECKSUMS.iter().collect();
    checks.sort_by_key(|(_, p)| std::cmp::Reverse(hex2bit_len(p.polynomial_hex)));

    for (name, parameters) in checks {
        let polynomial = {
            let mut poly = vec![1u8];
            poly.extend(encoding::hex2bits(parameters.polynomial_hex));
            poly
        };
        let start_value = vec![parameters.start_value as u8; polynomial.len() - 1];
        let final_xor = vec![parameters.final_xor as u8; polynomial.len() - 1];

        let vrfy_crc_start = (inpt.len() as i64 - trash as i64 - polynomial.len() as i64)
            .max(0) as usize
            + 1;

        let (data_begin, data_end) = encoding::get_crc_datarange(
            inpt,
            &polynomial,
            vrfy_crc_start,
            &start_value,
            &final_xor,
            parameters.ref_in, // pyx lsb_first slot
            parameters.reverse_polynomial,
            parameters.ref_out, // pyx reverse_all slot
            parameters.little_endian,
        );
        if (data_begin, data_end) != (0, 0) {
            return Some((name, data_begin, data_end));
        }
    }
    None
}

fn hex2bit_len(hex: &str) -> usize {
    encoding::hex2bits(hex).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awre::format_finder::FormatFinder;
    use crate::awre::message_type_builder::{FieldFunction, MessageTypeBuilder};
    use crate::awre::protocol_generator::{test_util, ProtocolGenerator};
    use crate::encoding::GenericCRC;

    fn hex_messages(hexes: &[&str]) -> Vec<Vec<u8>> {
        hexes
            .iter()
            .map(|h| encoding::hex2bits(h))
            .collect()
    }

    /// test_checksum_engine.py::test_find_crc8
    #[test]
    fn test_find_crc8() {
        let messages = ["aabbcc7d", "abcdee24", "dacafe33"];
        let message_bits = hex_messages(&messages);

        let checksum_engine = ChecksumEngine::new(&message_bits, 8, 0.9, &[]);
        let result = checksum_engine.find();
        assert_eq!(result.len(), 1);
        let checksum_range = &result[0];
        assert_eq!(checksum_range.range.length, 8);
        assert_eq!(checksum_range.range.start, 24);

        let reference = GenericCRC::from_hex("0x07", 0, 0, false, false, false, false);
        let crc = checksum_range.crc.as_crc().unwrap();
        assert_eq!(crc.polynomial, reference.polynomial);

        assert_eq!(
            checksum_range.range.message_indices,
            [0, 1, 2].into_iter().collect()
        );
    }

    /// test_checksum_engine.py::test_find_crc16
    #[test]
    fn test_find_crc16() {
        let messages = ["12345678347B", "abcdefffABBD", "cafe1337CE12"];
        let message_bits = hex_messages(&messages);

        let checksum_engine = ChecksumEngine::new(&message_bits, 8, 0.9, &[]);
        let result = checksum_engine.find();
        assert_eq!(result.len(), 1);
        let checksum_range = &result[0];
        assert_eq!(checksum_range.range.start, 32);
        assert_eq!(checksum_range.range.length, 16);

        let reference = GenericCRC::from_hex("0x8005", 0, 0, false, false, false, false);
        let crc = checksum_range.crc.as_crc().unwrap();
        assert_eq!(crc.polynomial, reference.polynomial);

        assert_eq!(
            checksum_range.range.message_indices,
            [0, 1, 2].into_iter().collect()
        );
    }

    /// test_checksum_engine.py::test_find_crc32
    #[test]
    fn test_find_crc32() {
        let messages = ["deadcafe5D7F3F5A", "47111337E3319242", "beefaffe0DCD0E15"];
        let message_bits = hex_messages(&messages);

        let checksum_engine = ChecksumEngine::new(&message_bits, 8, 0.9, &[]);
        let result = checksum_engine.find();
        assert_eq!(result.len(), 1);
        let checksum_range = &result[0];
        assert_eq!(checksum_range.range.start, 32);
        assert_eq!(checksum_range.range.length, 32);

        let reference = GenericCRC::from_hex("0x04C11DB7", 0, 0, false, false, false, false);
        let crc = checksum_range.crc.as_crc().unwrap();
        assert_eq!(crc.polynomial, reference.polynomial);

        assert_eq!(
            checksum_range.range.message_indices,
            [0, 1, 2].into_iter().collect()
        );
    }

    /// test_checksum_engine.py::test_find_generated_crc16
    #[test]
    fn test_find_generated_crc16() {
        let crc = GenericCRC::from_standard_checksum("CRC16 CCITT").unwrap();

        let mut mb = MessageTypeBuilder::new("data");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        mb.add_label(FieldFunction::Data, 32, None);
        mb.add_checksum_label(16, ChecksumSpec::Crc(crc.clone()), None, None, None);

        let mut mb2 = MessageTypeBuilder::new("data2");
        mb2.add_label(FieldFunction::Preamble, 8, None);
        mb2.add_label(FieldFunction::Sync, 16, None);
        mb2.add_label(FieldFunction::Length, 8, None);
        mb2.add_label(FieldFunction::Data, 16, None);
        mb2.add_checksum_label(16, ChecksumSpec::Crc(crc.clone()), None, None, None);

        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone(), mb2.message_type.clone()],
            vec![],
            &[],
            &["0x1234", "0x1234"],
            false,
            true,
        );

        let num_messages = 5;
        for i in 0..num_messages {
            let data32 = format!("{:032b}", i);
            pg.generate_message(Some(0), &data32, None, None);
            let data16 = format!("{:016b}", i);
            pg.generate_message(Some(1), &data16, None, None);
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);

        let mut ff = FormatFinder::new(messages);
        ff.perform_analysis();

        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 2);
        for mt in message_types {
            let checksum_label = mt.get_first_label_with_type(FieldFunction::Checksum).unwrap();
            assert_eq!(checksum_label.label.end - checksum_label.label.start, 16);
            let annotation = checksum_label.checksum.as_ref().unwrap();
            let expected = ChecksumSpec::Crc(crc.clone());
            assert!(ChecksumSpec::spec_eq(&annotation.checksum, &expected));
        }
    }
}
