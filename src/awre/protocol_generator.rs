//! Test-support port of `urh/awre/ProtocolGenerator.py` (the parts the AWRE
//! tests use): builds labeled messages from message types described with
//! `MessageTypeBuilder`.
//!
//! Deviation: `Message` has no typed message type / participant, so the
//! generator records the message type *name* on each message and returns the
//! per-message participant indices from `generate_message`.

use crate::awre::engines::checksum::ChecksumSpec;
use crate::awre::message_type_builder::{
    AnalyzedMessageType, FieldFunction,
};
use crate::awre::auto_assigner::Participant;
use crate::encoding::{self, GenericCRC};
use crate::protocol::Message;

pub struct ProtocolGenerator {
    pub message_types: Vec<AnalyzedMessageType>,
    pub participants: Vec<Participant>,
    /// per-message participant indices (index into `participants`)
    pub participant_indices: Vec<isize>,
    /// (message index, participant index) pairs for auto-appended participants
    pub length_in_bytes: bool,
    pub little_endian: bool,
    preambles_by_message_type: Vec<String>,
    syncs_by_message_type: Vec<String>,
    sequence_numbers: Vec<u64>,
    sequence_number_increment: u64,
    message_type_codes: Vec<u64>,
    pub messages: Vec<Message>,
}

impl ProtocolGenerator {
    pub const DEFAULT_PREAMBLE: &'static str = "10101010";
    pub const DEFAULT_SYNC: &'static str = "1001";
    pub const BROADCAST_ADDRESS: &'static str = "0xffff";

    /// `ProtocolGenerator(message_types, participants, preambles_by_mt,
    /// syncs_by_mt, little_endian, length_in_bytes)`. The preamble / sync /
    /// sequence-number maps are given per message type index.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        message_types: Vec<AnalyzedMessageType>,
        participants: Vec<Participant>,
        preambles_by_mt: &[&str],
        syncs_by_mt: &[&str],
        little_endian: bool,
        length_in_bytes: bool,
    ) -> Self {
        ProtocolGenerator {
            message_types,
            participants,
            participant_indices: Vec::new(),
            length_in_bytes,
            little_endian,
            preambles_by_message_type: preambles_by_mt
                .iter()
                .map(|s| s.to_string())
                .collect(),
            syncs_by_message_type: syncs_by_mt.iter().map(|s| s.to_string()).collect(),
            sequence_numbers: Vec::new(),
            sequence_number_increment: 1,
            message_type_codes: Vec::new(),
            messages: Vec::new(),
        }
    }

    pub fn with_sequence_numbers(
        mut self,
        sequence_numbers: &[u64],
        sequence_number_increment: u64,
    ) -> Self {
        self.sequence_numbers = sequence_numbers.to_vec();
        self.sequence_number_increment = sequence_number_increment;
        self
    }

    pub fn with_message_type_codes(mut self, message_type_codes: &[u64]) -> Self {
        self.message_type_codes = message_type_codes.to_vec();
        self
    }

    fn preamble_of(&self, mt_index: usize) -> String {
        // Python converts the configured value with to_bits in __init__
        Self::to_bits(
            &self
                .preambles_by_message_type
                .get(mt_index)
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| Self::DEFAULT_PREAMBLE.to_string()),
        )
    }

    fn sync_of(&self, mt_index: usize) -> String {
        Self::to_bits(
            &self
                .syncs_by_message_type
                .get(mt_index)
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| Self::DEFAULT_SYNC.to_string()),
        )
    }

    fn seq_number_of(&self, mt_index: usize) -> u64 {
        self.sequence_numbers.get(mt_index).copied().unwrap_or(0)
    }

    fn code_of(&self, mt_index: usize) -> u64 {
        self.message_type_codes
            .get(mt_index)
            .copied()
            .unwrap_or(mt_index as u64)
    }

    fn address_for_participant(&self, participant: Option<&Participant>) -> String {
        match participant {
            None => Self::to_bits(Self::BROADCAST_ADDRESS),
            Some(p) => {
                let hex = p
                    .address_hex
                    .strip_prefix("0x")
                    .unwrap_or(&p.address_hex)
                    .to_string();
                Self::to_bits(&format!("0x{}", hex))
            }
        }
    }

    /// Python `to_bits`: "0x..." strings are expanded from hex to bits.
    pub fn to_bits(bit_or_hex_str: &str) -> String {
        if let Some(hex) = bit_or_hex_str.strip_prefix("0x") {
            let mut out = String::new();
            for c in hex.chars() {
                let v = c.to_digit(16).expect("valid hex");
                out.push_str(&format!("{:04b}", v));
            }
            out
        } else {
            bit_or_hex_str.to_string()
        }
    }

    pub fn decimal_to_bits(&self, number: u64, num_bits: usize) -> String {
        let mut out = String::new();
        let bytes = num_bits / 8;
        let be_bytes = number.to_be_bytes();
        let slice = &be_bytes[8 - bytes..];
        let slice: Vec<u8> = if self.little_endian {
            slice.iter().rev().cloned().collect()
        } else {
            slice.to_vec()
        };
        for b in slice {
            out.push_str(&format!("{:08b}", b));
        }
        out
    }

    /// Port of `generate_message`. Returns the generated message index.
    #[allow(clippy::too_many_arguments)]
    pub fn generate_message(
        &mut self,
        message_type: Option<usize>,
        data: &str,
        source: Option<&Participant>,
        destination: Option<&Participant>,
    ) -> usize {
        for participant in [source, destination].into_iter().flatten() {
            if !self
                .participants
                .iter()
                .any(|p| p.address_hex == participant.address_hex && p.name == participant.name)
            {
                self.participants.push(participant.clone());
            }
        }

        let message_type_index = message_type.unwrap_or(0);
        let mt = self.message_types[message_type_index].clone();
        let data = Self::to_bits(data);

        let mut bits = String::new();
        let mut start = 0usize;

        let data_label_present = mt
            .get_first_label_with_type(FieldFunction::Data)
            .is_some();

        // total bit length (crate label `end` is exclusive, like Python)
        let total_bits = mt
            .typed_labels
            .last()
            .map(|tl| tl.label.end)
            .unwrap_or(0);

        let mut message_length = if data_label_present {
            total_bits - 1
        } else {
            total_bits - 1 + data.len()
        };

        let mut checksum_labels = Vec::new();

        for tl in &mt.typed_labels {
            bits.push_str(&"0".repeat(tl.label.start.saturating_sub(start)));
            let len_field = tl.label.end - tl.label.start;

            if tl.function == FieldFunction::Checksum {
                checksum_labels.push(tl.clone());
                start = tl.label.end;
                continue; // processed last
            }

            match tl.function {
                FieldFunction::Preamble => {
                    let preamble = self.preamble_of(message_type_index);
                    assert_eq!(preamble.len(), len_field);
                    bits.push_str(&preamble);
                    message_length -= preamble.len();
                }
                FieldFunction::Sync => {
                    let sync = self.sync_of(message_type_index);
                    assert_eq!(sync.len(), len_field);
                    bits.push_str(&sync);
                    message_length -= sync.len();
                }
                FieldFunction::Length => {
                    let mut value = (message_length as f64 / 8.0).ceil() as u64;
                    if !self.length_in_bytes {
                        value *= 8;
                    }
                    bits.push_str(&self.decimal_to_bits(value, len_field));
                }
                FieldFunction::Type => {
                    bits.push_str(&self.decimal_to_bits(
                        self.code_of(message_type_index) % (1u64 << len_field),
                        len_field,
                    ));
                }
                FieldFunction::SequenceNumber => {
                    bits.push_str(&self.decimal_to_bits(
                        self.seq_number_of(message_type_index) % (1u64 << len_field),
                        len_field,
                    ));
                }
                FieldFunction::DstAddress => {
                    let dst_bits = self.address_for_participant(destination);
                    assert_eq!(
                        dst_bits.len(),
                        len_field,
                        "Length of dst bits != length dst field bits",
                    );
                    bits.push_str(&dst_bits);
                }
                FieldFunction::SrcAddress => {
                    let src_bits = self.address_for_participant(source);
                    assert_eq!(
                        src_bits.len(),
                        len_field,
                        "Length of src bits != length src field bits",
                    );
                    bits.push_str(&src_bits);
                }
                FieldFunction::Data => {
                    assert_eq!(data.len(), len_field, "Length of data does not match data field");
                    bits.push_str(&data);
                }
                _ => {}
            }

            start = tl.label.end;
        }

        if !data_label_present {
            bits.push_str(&data);
        }

        let mut msg = Message::from_plain_bits_str(&bits, 0);
        msg.message_type = mt.message_type.name.clone();

        // Python uses a defaultdict(0); grow to cover this message type
        while self.sequence_numbers.len() <= message_type_index {
            self.sequence_numbers.push(0);
        }
        self.sequence_numbers[message_type_index] += self.sequence_number_increment;

        // set source participant index
        let src_index = source
            .and_then(|src| {
                self.participants
                    .iter()
                    .position(|p| p.address_hex == src.address_hex && p.name == src.name)
            })
            .map(|i| i as isize)
            .unwrap_or(-1);
        self.participant_indices.push(src_index);

        for checksum_label in checksum_labels {
            let annotation = checksum_label.checksum.as_ref().unwrap();
            let mut data_bits: Vec<u8> = Vec::new();
            for &(dr_start, dr_end) in &annotation.data_ranges {
                data_bits.extend_from_slice(&msg.plain_bits[dr_start..dr_end.min(msg.plain_bits.len())]);
            }
            let checksum = match &annotation.checksum {
                ChecksumSpec::Crc(crc) => crc.crc(&data_bits),
                ChecksumSpec::Wsp => wsp_checksum(&data_bits),
            };
            let lbl = &checksum_label.label;
            // Python's slice assignment extends the bit array when the
            // checksum label reaches beyond the generated bits
            while msg.plain_bits.len() < lbl.end {
                msg.plain_bits.push(0);
            }
            msg.plain_bits[lbl.start..lbl.end].copy_from_slice(&checksum);
        }

        self.messages.push(msg);
        self.messages.len() - 1
    }

    /// Convenience for tests: build the `FormatFinder` input.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
}

/// `WSPChecksum.calculate` (checksum8 over the nibble-folded byte sum).
fn wsp_checksum(bits: &[u8]) -> Vec<u8> {
    let mut val = bits.to_vec();
    let n = val.len();
    for b in &mut val[n.saturating_sub(8)..] {
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
    (0..4).rev().map(|i| ((hash >> i) & 1) as u8).collect()
}

/// Helper: a CRC spec from the given parameters, for tests.
pub fn crc_spec(polynomial_hex: &str, start_value: u64, final_xor: u64) -> GenericCRC {
    GenericCRC::from_hex(polynomial_hex, start_value, final_xor, false, false, false, false)
}

/// Helper used by tests: format plain bits as hex (lowercase, nibbles).
pub fn bits_to_hex(bits: &[u8]) -> String {
    encoding::bits2hex(bits)
}

/// Shared helpers for the ported AWRE tests (`tests/awre/*.py`).
#[cfg(test)]
pub(crate) mod test_util {
    use crate::protocol::Message;

    /// Deterministic stand-in for Python's `random.seed(0)` (Mersenne
    /// Twister output is not replicated; only determinism matters here).
    pub struct Rng(u64);

    impl Rng {
        pub fn new(seed: u64) -> Self {
            Rng(seed.max(1))
        }

        pub fn next_u64(&mut self) -> u64 {
            // xorshift64*
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }

        pub fn next_bool(&mut self) -> bool {
            self.next_u64() & 1 == 1
        }

        pub fn next_below(&mut self, n: u64) -> u64 {
            self.next_u64() % n.max(1)
        }

        /// random.choice("01") * n
        pub fn random_bits(&mut self, n: usize) -> String {
            (0..n).map(|_| if self.next_bool() { '1' } else { '0' }).collect()
        }
    }

    /// `AWRETestCase.clear_message_types`.
    pub fn clear_message_types(messages: &mut [Message]) {
        for msg in messages.iter_mut() {
            msg.message_type = "empty".to_string();
        }
    }

    /// `util.convert_numbers_to_hex_string` for nibble values.
    pub fn nibbles_to_hex(nibbles: &[u8]) -> String {
        nibbles
            .iter()
            .map(|&n| char::from_digit(n as u32, 16).unwrap())
            .collect()
    }
}
