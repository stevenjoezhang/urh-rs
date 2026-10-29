//! Protocol data structures and analysis.
//!
//! Port of the core (non-GUI) parts of `urh/signalprocessing/`:
//! `Interval.py`, `ProtocoLabel.py`, `MessageType.py`, `Message.py` and
//! `ProtocolAnalyzer.py`.
//!
//! Simplifications vs. the reference:
//! - No Qt dependency: label visibility / fuzzing / display formats are
//!   reduced to the core fields.
//! - `Message` stores its labels and a message-type *name* by value instead of
//!   referencing a shared `MessageType` object.
//! - Decoded/encoded bit views (`Encoding` integration) are not ported yet;
//!   only the plain views exist. TODO: hook up `crate::encoding`.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::Bit;

// ---------------------------------------------------------------------------
// Interval
// ---------------------------------------------------------------------------

/// A half-open interval `[start, end)` over bit indices.
/// Note the URH convention: label `end` values are exclusive (the reference
/// stores `end + 1` internally).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Interval {
    pub start: usize,
    pub end: usize,
}

impl Interval {
    pub fn new(start: usize, end: usize) -> Self {
        Interval { start, end }
    }

    /// Number of bits covered by this interval.
    pub fn bit_length(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        self.start..self.end
    }

    pub fn overlaps_with(&self, other: &Interval) -> bool {
        // any(r in self.range() for r in other.range())
        std::cmp::max(self.start, other.start) < std::cmp::min(self.end, other.end)
    }

    /// Intersection as an Interval; `end` of the result is max+1 like the
    /// reference (`Interval(min(common), max(common) + 1)`).
    pub fn find_common_interval(&self, other: &Interval) -> Option<Interval> {
        let start = std::cmp::max(self.start, other.start);
        let end = std::cmp::min(self.end, other.end);
        if start < end {
            Some(Interval::new(start, end))
        } else {
            None
        }
    }

    /// The interval covering the most bits.
    /// (Deviation: the reference's `find_greatest` compares `len(intervals)`
    /// which is always 2 in Python and thus returns the first element; we use
    /// the obviously intended bit length.)
    pub fn find_greatest(intervals: &[Interval]) -> Option<&Interval> {
        intervals.iter().max_by_key(|i| i.bit_length())
    }
}

// ---------------------------------------------------------------------------
// ProtocolLabel
// ---------------------------------------------------------------------------

/// A field in the protocol, e.g. temperature.
/// `start` and `end` always refer to the bit view; `end` is exclusive
/// (the reference constructor takes an inclusive end and stores `end + 1`).
#[derive(Debug, Clone)]
pub struct ProtocolLabel {
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub color_index: usize,
    pub apply_decoding: bool,
    pub auto_created: bool,
}

/// Equality follows the reference `ProtocolLabel.__eq__`: start, end and name
/// (color etc. are irrelevant).
impl PartialEq for ProtocolLabel {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.end == other.end && self.name == other.name
    }
}

impl Eq for ProtocolLabel {}

impl ProtocolLabel {
    /// `end` is inclusive here, matching the Python constructor.
    pub fn new(name: &str, start: usize, end_inclusive: usize, color_index: usize) -> Self {
        ProtocolLabel {
            name: if name.is_empty() {
                "No name".to_string()
            } else {
                name.to_string()
            },
            start,
            end: end_inclusive + 1,
            color_index,
            apply_decoding: true,
            auto_created: false,
        }
    }

    pub fn length(&self) -> usize {
        self.end - self.start
    }

    pub fn overlaps_with(&self, other: &ProtocolLabel) -> bool {
        Interval::new(self.start, self.end).overlaps_with(&Interval::new(other.start, other.end))
    }
}

// ---------------------------------------------------------------------------
// MessageType
// ---------------------------------------------------------------------------

static MESSAGE_TYPE_COUNTER: AtomicUsize = AtomicUsize::new(1);

/// A message type is a list of protocol fields (labels).
#[derive(Debug, Clone, PartialEq)]
pub struct MessageType {
    pub name: String,
    pub id: String,
    pub labels: Vec<ProtocolLabel>,
    pub assigned_by_ruleset: bool,
}

impl MessageType {
    pub fn new(name: &str) -> Self {
        let n = MESSAGE_TYPE_COUNTER.fetch_add(1, Ordering::Relaxed);
        MessageType {
            name: name.to_string(),
            id: format!("message-type-{}", n),
            labels: Vec::new(),
            assigned_by_ruleset: false,
        }
    }

    /// Ranges of the message not covered by any label. The final range has
    /// `None` as end (open ended), like the reference.
    pub fn unlabeled_ranges(&self) -> Vec<(usize, Option<usize>)> {
        let mut result = Vec::new();
        let mut start = 0;
        for lbl in &self.labels {
            if lbl.start > start {
                result.push((start, Some(lbl.start)));
            }
            start = lbl.end;
        }
        result.push((start, None));
        result
    }

    /// Add a label (end inclusive, like `MessageType.add_protocol_label`).
    /// Labels are kept sorted and deduplicated.
    pub fn add_protocol_label(
        &mut self,
        start: usize,
        end_inclusive: usize,
        name: Option<&str>,
        color_index: Option<usize>,
    ) -> ProtocolLabel {
        let used_colors: Vec<usize> = self.labels.iter().map(|l| l.color_index).collect();
        let color_index = color_index.unwrap_or_else(|| {
            (0..16).find(|i| !used_colors.contains(i)).unwrap_or(0)
        });
        let name = name.unwrap_or("").to_string();
        let label = ProtocolLabel::new(&name, start, end_inclusive, color_index);
        self.add_label(label.clone(), true);
        label
    }

    /// Add a label given start + length.
    pub fn add_protocol_label_start_length(
        &mut self,
        start: usize,
        length: usize,
        name: Option<&str>,
        color_index: Option<usize>,
    ) -> ProtocolLabel {
        self.add_protocol_label(start, start + length - 1, name, color_index)
    }

    /// Insert a label, optionally checking for overlaps (like `add_label`).
    pub fn add_label(&mut self, label: ProtocolLabel, allow_overlapping: bool) -> bool {
        if !allow_overlapping && self.labels.iter().any(|l| l.overlaps_with(&label)) {
            return false;
        }
        if !self.labels.contains(&label) {
            self.labels.push(label);
            self.labels.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
        }
        true
    }

    pub fn remove(&mut self, label: &ProtocolLabel) {
        self.labels.retain(|l| l != label);
    }

    pub fn get_label_count(&self) -> usize {
        self.labels.len()
    }
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// A single line of a protocol.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Plain (undecoded) bits, each 0 or 1.
    pub plain_bits: Vec<Bit>,
    /// Pause AFTER the message, in samples.
    pub pause: u64,
    /// Name of the message type this message belongs to.
    pub message_type: String,
    /// Labels (fields) applied to this message.
    pub labels: Vec<ProtocolLabel>,
    pub rssi: f32,
    pub samples_per_symbol: u32,
    pub bits_per_symbol: u8,
    /// Sample position of each bit; the last position is the pause.
    pub bit_sample_pos: Vec<u64>,
}

impl Message {
    pub fn new(plain_bits: Vec<Bit>, pause: u64) -> Self {
        Message {
            plain_bits,
            pause,
            message_type: "Default".to_string(),
            labels: Vec::new(),
            rssi: 0.0,
            samples_per_symbol: 100,
            bits_per_symbol: 1,
            bit_sample_pos: Vec::new(),
        }
    }

    pub fn with_message_type(mut self, message_type: &str) -> Self {
        self.message_type = message_type.to_string();
        self
    }

    pub fn with_labels(mut self, labels: Vec<ProtocolLabel>) -> Self {
        self.labels = labels;
        self
    }

    pub fn len(&self) -> usize {
        self.plain_bits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plain_bits.is_empty()
    }

    pub fn from_plain_bits_str(bits: &str, pause: u64) -> Self {
        let plain_bits: Vec<Bit> = bits
            .chars()
            .filter(|c| c.is_ascii_digit())
            .map(|c| c as u8 - b'0')
            .collect();
        Message::new(plain_bits, pause)
    }

    pub fn from_plain_hex_str(hex_str: &str, pause: u64) -> Self {
        let mut bits = String::with_capacity(hex_str.len() * 4);
        for h in hex_str.chars() {
            let v = h.to_digit(16).expect("valid hex digit") as u8;
            bits.push_str(&format!("{:04b}", v));
        }
        Message::from_plain_bits_str(&bits, pause)
    }

    pub fn plain_bits_str(&self) -> String {
        self.plain_bits.iter().map(|b| char::from(b'0' + b)).collect()
    }

    /// Bit positions the hex/ascii views are aligned to (label starts/ends).
    fn bit_alignments(&self) -> Vec<usize> {
        let mut alignments: Vec<usize> = Vec::new();
        for l in &self.labels {
            for pos in [l.start, l.end] {
                if pos <= self.plain_bits.len() && !alignments.contains(&pos) {
                    alignments.push(pos);
                }
            }
        }
        alignments.sort_unstable();
        alignments
    }

    /// Split the bits into chains at the label alignment positions
    /// (like `Message.split(decode=False)`).
    pub fn split_bit_chains(&self) -> Vec<Vec<Bit>> {
        let mut result = Vec::new();
        let mut start = 0;
        for &pos in &self.bit_alignments() {
            result.push(self.plain_bits[start..pos].to_vec());
            start = pos;
        }
        result.push(self.plain_bits[start..].to_vec());
        result
    }

    fn bit_chains_to_hex(chains: &[Vec<Bit>]) -> Vec<u8> {
        let mut result = Vec::new();
        for bc in chains {
            let mut bc = bc.clone();
            while bc.len() % 4 != 0 {
                bc.push(0);
            }
            for chunk in bc.chunks_exact(4) {
                result.push(
                    8 * chunk[0] + 4 * chunk[1] + 2 * chunk[2] + chunk[3],
                );
            }
        }
        result
    }

    fn bit_chains_to_ascii(chains: &[Vec<Bit>]) -> Vec<u8> {
        let mut result = Vec::new();
        for bc in chains {
            let mut bc = bc.clone();
            while bc.len() % 8 != 0 {
                bc.push(0);
            }
            for chunk in bc.chunks_exact(8) {
                result.push(
                    128 * chunk[0]
                        + 64 * chunk[1]
                        + 32 * chunk[2]
                        + 16 * chunk[3]
                        + 8 * chunk[4]
                        + 4 * chunk[5]
                        + 2 * chunk[6]
                        + chunk[7],
                );
            }
        }
        result
    }

    pub fn plain_hex_str(&self) -> String {
        Self::bit_chains_to_hex(&self.split_bit_chains())
            .iter()
            .map(|h| format!("{:x}", h))
            .collect()
    }

    pub fn plain_ascii_str(&self) -> String {
        Self::bit_chains_to_ascii(&self.split_bit_chains())
            .iter()
            .map(|&b| b as char)
            .collect()
    }

    /// The plain bits covered by a label (clamped to the message length).
    pub fn label_bits(&self, label: &ProtocolLabel) -> Vec<Bit> {
        let end = std::cmp::min(label.end, self.plain_bits.len());
        let start = std::cmp::min(label.start, end);
        self.plain_bits[start..end].to_vec()
    }

    /// Convert a bit index to the hex (factor 4) or ascii (factor 8) view,
    /// respecting the label bit alignments (like
    /// `Message.__get_hex_ascii_index_from_bit_index`).
    fn hex_ascii_index_from_bit_index(&self, bit_index: usize, to_hex: bool) -> usize {
        let factor = if to_hex { 4 } else { 8 };
        let mut result = 0;
        let mut last_alignment = 0;
        for &ba in &self.bit_alignments() {
            if ba <= bit_index {
                result += (ba - last_alignment).div_ceil(factor); // ceil
                last_alignment = ba;
            } else {
                break;
            }
        }
        result + (bit_index - last_alignment) / factor
    }

    /// Inverse of [`hex_ascii_index_from_bit_index`]
    /// (like `Message.__get_bit_range_from_hex_or_ascii_index`).
    fn bit_range_from_hex_ascii_index(&self, from_index: usize, to_hex: bool) -> (usize, usize) {
        let factor = if to_hex { 4 } else { 8 };
        for i in 0..self.plain_bits.len() {
            if self.hex_ascii_index_from_bit_index(i, to_hex) == from_index {
                return (i, i + factor - 1);
            }
        }
        (factor * from_index, factor * (from_index + 1) - 1)
    }

    /// View conversion. `view`: 0 = bits, 1 = hex, 2 = ascii.
    pub fn convert_index(&self, index: usize, from_view: u8, to_view: u8) -> (usize, usize) {
        if to_view == from_view {
            return (index, index);
        }
        if to_view == 0 {
            self.bit_range_from_hex_ascii_index(index, from_view == 1)
        } else if to_view == 1 {
            match from_view {
                0 => {
                    let i = self.hex_ascii_index_from_bit_index(index, true);
                    (i, i)
                }
                _ => {
                    let bi = self.bit_range_from_hex_ascii_index(index, false).0;
                    let i = self.hex_ascii_index_from_bit_index(bi, true);
                    (i, i)
                }
            }
        } else {
            match from_view {
                0 => {
                    let i = self.hex_ascii_index_from_bit_index(index, false);
                    (i, i)
                }
                _ => {
                    let bi = self.bit_range_from_hex_ascii_index(index, true).0;
                    let i = self.hex_ascii_index_from_bit_index(bi, false);
                    (i, i)
                }
            }
        }
    }

    /// Label range in the given view (0 bits, 1 hex, 2 ascii).
    pub fn get_label_range(&self, label: &ProtocolLabel, view: u8) -> (usize, usize) {
        let start = self.convert_index(label.start, 0, view).0;
        let end = self.convert_index(label.end, 0, view).1;
        (start, end)
    }
}

// ---------------------------------------------------------------------------
// ProtocolAnalyzer
// ---------------------------------------------------------------------------

/// The protocol analyzer holds the messages of a protocol and offers methods
/// for protocol analysis.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolAnalyzer {
    pub messages: Vec<Message>,
    pub message_types: Vec<MessageType>,
}

/// Symbol type marking a pause in a pulse-length sequence.
pub const PAUSE_TYPE: i64 = -1;

impl ProtocolAnalyzer {
    pub fn new() -> Self {
        ProtocolAnalyzer {
            messages: Vec::new(),
            message_types: vec![MessageType::new("Default")],
        }
    }

    pub fn default_message_type(&mut self) -> &mut MessageType {
        if self.message_types.is_empty() {
            self.message_types.push(MessageType::new("Default"));
        }
        &mut self.message_types[0]
    }

    pub fn num_messages(&self) -> usize {
        self.messages.iter().filter(|m| !m.is_empty()).count()
    }

    pub fn pauses(&self) -> Vec<u64> {
        self.messages.iter().map(|m| m.pause).collect()
    }

    pub fn plain_bits_str(&self) -> Vec<String> {
        self.messages.iter().map(|m| m.plain_bits_str()).collect()
    }

    pub fn plain_hex_str(&self) -> Vec<String> {
        self.messages.iter().map(|m| m.plain_hex_str()).collect()
    }

    pub fn plain_ascii_str(&self) -> Vec<String> {
        self.messages.iter().map(|m| m.plain_ascii_str()).collect()
    }

    pub fn protocol_labels(&self) -> Vec<&ProtocolLabel> {
        self.message_types
            .iter()
            .flat_map(|mt| mt.labels.iter())
            .collect()
    }

    /// Build messages from bit sequences and their pauses.
    pub fn messages_from_bits(&mut self, bit_data: Vec<Vec<Bit>>, pauses: Vec<u64>) {
        self.messages.clear();
        for (bits, pause) in bit_data.into_iter().zip(pauses) {
            let mut msg = Message::new(bits, pause);
            msg.message_type = self
                .message_types
                .first()
                .map(|mt| mt.name.clone())
                .unwrap_or_else(|| "Default".to_string());
            self.messages.push(msg);
        }
    }

    /// Demodulate a (quadrature-demodulated) signal into messages.
    ///
    /// This is the port of `ProtocolAnalyzer.get_protocol_from_signal`,
    /// taking the output of `crate::modulation::grab_pulse_lens` as
    /// `(states, lens)` parallel arrays (state -1 = pause). RSSI/timestamps
    /// need the raw IQ data and are left at their defaults.
    pub fn analyze_signal(
        &mut self,
        qad: &[f32],
        center: f32,
        tolerance: u16,
        modulation_type: crate::Modulation,
        samples_per_symbol: u32,
        bits_per_symbol: u8,
        center_spacing: f32,
        pause_threshold: u32,
    ) {
        let (states, lens) = crate::modulation::grab_pulse_lens(
            qad,
            center,
            tolerance,
            modulation_type,
            samples_per_symbol,
            bits_per_symbol,
            center_spacing,
        );
        let ppseq: Vec<(i64, i64)> = states.into_iter().zip(lens).collect();
        self.set_messages_from_ppseq(&ppseq, samples_per_symbol, bits_per_symbol, pause_threshold);
    }

    /// Segment a pulse-length sequence into messages
    /// (port of `ProtocolAnalyzer._ppseq_to_bits`).
    ///
    /// Returns (bit data, pauses, bit sample positions).
    pub fn ppseq_to_bits(
        ppseq: &[(i64, i64)],
        samples_per_symbol: u32,
        bits_per_symbol: u8,
        pause_threshold: u32,
    ) -> (Vec<Vec<Bit>>, Vec<u64>, Vec<Vec<u64>>) {
        let mut bit_sampl_pos: Vec<u64> = Vec::new();
        let mut bit_sample_positions: Vec<Vec<u64>> = Vec::new();

        let mut data_bits: Vec<Bit> = Vec::new();
        let mut resulting_data_bits: Vec<Vec<Bit>> = Vec::new();
        let mut pauses: Vec<u64> = Vec::new();
        let mut start = 0usize;
        let mut total_samples: u64 = 0;

        let mut there_was_data = false;

        let samples_per_bit = (samples_per_symbol / bits_per_symbol as u32) as u64;

        if !ppseq.is_empty() && ppseq[0].0 == PAUSE_TYPE {
            start = 1; // Starts with Pause
            total_samples = ppseq[0].1 as u64;
        }

        for &(cur_pulse_type, num_samples) in &ppseq[start..] {
            let num_samples = num_samples as u64;
            let num_symbols_float = num_samples as f64 / samples_per_symbol as f64;
            let mut num_symbols = num_symbols_float as u64;
            if num_symbols_float - num_symbols as f64 > 0.5 {
                num_symbols += 1;
            }

            if cur_pulse_type == PAUSE_TYPE {
                // OOK
                if num_symbols <= pause_threshold as u64 || pause_threshold == 0 {
                    data_bits.extend(std::iter::repeat_n(0, (num_symbols * bits_per_symbol as u64) as usize));
                    bit_sampl_pos.extend(
                        (0..num_symbols * bits_per_symbol as u64)
                            .map(|k| total_samples + k * samples_per_bit),
                    );
                } else if !there_was_data {
                    // Ignore this pause, if there was no information
                    // transmitted previously
                    data_bits.clear();
                    bit_sampl_pos.clear();
                } else {
                    bit_sampl_pos.push(total_samples);
                    bit_sampl_pos.push(total_samples + num_samples);
                    bit_sample_positions.push(bit_sampl_pos.clone());
                    bit_sampl_pos.clear();

                    resulting_data_bits.push(std::mem::take(&mut data_bits));
                    pauses.push(num_samples);
                    there_was_data = false;
                }
            } else {
                let symbol_bits = number_to_bits(cur_pulse_type, bits_per_symbol);
                for _ in 0..num_symbols {
                    data_bits.extend_from_slice(&symbol_bits);
                }
                if !there_was_data && num_symbols > 0 {
                    there_was_data = true;
                }
                bit_sampl_pos.extend(
                    (0..num_symbols * bits_per_symbol as u64)
                        .map(|k| total_samples + k * samples_per_bit),
                );
            }

            total_samples += num_samples;
        }

        if there_was_data {
            resulting_data_bits.push(data_bits);
            let mut positions = bit_sampl_pos.clone();
            positions.push(total_samples);
            bit_sample_positions.push(positions);
            let pause = if ppseq[ppseq.len() - 1].0 == PAUSE_TYPE {
                ppseq[ppseq.len() - 1].1 as u64
            } else {
                0
            };
            pauses.push(pause);
        }

        (resulting_data_bits, pauses, bit_sample_positions)
    }

    /// Create messages from a pulse-length sequence
    /// (`(state, n_samples)` pairs, state -1 = pause).
    pub fn set_messages_from_ppseq(
        &mut self,
        ppseq: &[(i64, i64)],
        samples_per_symbol: u32,
        bits_per_symbol: u8,
        pause_threshold: u32,
    ) {
        let (bit_data, pauses, bit_sample_pos) =
            Self::ppseq_to_bits(ppseq, samples_per_symbol, bits_per_symbol, pause_threshold);

        self.messages.clear();
        let type_name = self
            .message_types
            .first()
            .map(|mt| mt.name.clone())
            .unwrap_or_else(|| "Default".to_string());

        for (i, (bits, pause)) in bit_data.into_iter().zip(pauses).enumerate() {
            let mut msg = Message::new(bits, pause);
            msg.message_type = type_name.clone();
            msg.samples_per_symbol = samples_per_symbol;
            msg.bits_per_symbol = bits_per_symbol;
            msg.bit_sample_pos = bit_sample_pos
                .get(i)
                .cloned()
                .unwrap_or_default();
            self.messages.push(msg);
        }
    }

    /// In case of ASK modulation, use pauses after messages as zero bits so
    /// that message bit lengths become divisible by `divisor`
    /// (port of `__ensure_message_length_multiple`).
    pub fn ensure_message_length_multiple(&mut self, divisor: usize) {
        for i in 0..self.messages.len() {
            let missing_bits = (divisor - self.messages[i].plain_bits.len() % divisor) % divisor;
            let sps = self.messages[i].samples_per_symbol as u64;
            if missing_bits > 0
                && self.messages[i].pause >= sps * missing_bits as u64
                && !self.messages[i].plain_bits.len().is_multiple_of(divisor)
            {
                let msg = &mut self.messages[i];
                msg.plain_bits.extend(std::iter::repeat_n(0, missing_bits));
                msg.pause -= missing_bits as u64 * sps;
                let n = msg.bit_sample_pos.len();
                if n >= 2 {
                    let last = msg.bit_sample_pos[n - 2] + sps;
                    msg.bit_sample_pos[n - 1] = last;
                    for k in 0..missing_bits - 1 {
                        msg.bit_sample_pos.push(last + (k as u64 + 1) * sps);
                    }
                    msg.bit_sample_pos.push(last + msg.pause);
                }
            }
        }
    }
}

impl Default for ProtocolAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// Binary representation of a symbol value with `length` bits
/// (port of `util.number_to_bits`).
pub fn number_to_bits(n: i64, length: u8) -> Vec<Bit> {
    (0..length)
        .rev()
        .map(|i| ((n >> i) & 1) as u8)
        .collect()
}

pub fn bits_to_number(bits: &[Bit]) -> u64 {
    bits.iter().fold(0u64, |acc, &b| (acc << 1) | b as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interval() {
        let a = Interval::new(0, 10);
        let b = Interval::new(5, 15);
        let c = Interval::new(10, 20);

        assert!(a.overlaps_with(&b));
        assert!(!a.overlaps_with(&c));
        assert_eq!(a.find_common_interval(&b), Some(Interval::new(5, 10)));
        assert_eq!(a.find_common_interval(&c), None);
        assert_eq!(a.bit_length(), 10);
        assert_eq!(
            Interval::find_greatest(&[a, b, Interval::new(0, 100)]).unwrap(),
            &Interval::new(0, 100)
        );
    }

    #[test]
    fn test_protocol_label() {
        // end inclusive in constructor, exclusive internally (URH semantics)
        let lbl = ProtocolLabel::new("sync", 0, 7, 0);
        assert_eq!(lbl.start, 0);
        assert_eq!(lbl.end, 8);
        assert_eq!(lbl.length(), 8);

        let lbl2 = ProtocolLabel::new("data", 8, 15, 1);
        assert!(!lbl.overlaps_with(&lbl2));
        assert!(lbl.overlaps_with(&ProtocolLabel::new("x", 7, 9, 2)));
    }

    #[test]
    fn test_message_type() {
        let mut mt = MessageType::new("Default");
        mt.add_protocol_label(0, 7, Some("preamble"), None);
        mt.add_protocol_label(8, 15, Some("sync"), None);
        // only fully equal labels (start/end/name) are deduplicated
        mt.add_protocol_label(0, 7, Some("preamble"), None);

        assert_eq!(mt.get_label_count(), 2);
        assert_eq!(mt.labels[0].name, "preamble");
        assert_eq!(mt.labels[1].name, "sync");

        let ranges = mt.unlabeled_ranges();
        // labels cover bits 0..16 contiguously, so only the open end remains
        assert_eq!(ranges, vec![(16, None)]);
    }

    #[test]
    fn test_message_bits_hex_ascii() {
        let msg = Message::from_plain_bits_str("10101010", 0);
        assert_eq!(msg.plain_hex_str(), "aa");
        assert_eq!(msg.plain_ascii_str(), "\u{aa}");

        // label alignment: label from bit 4 splits the hex view
        let msg = Message::from_plain_bits_str("10101110", 0)
            .with_labels(vec![ProtocolLabel::new("l", 4, 7, 0)]);
        // chains: "1010" -> a, "1110" -> e
        assert_eq!(msg.plain_hex_str(), "ae");
        assert_eq!(msg.plain_bits_str(), "10101110");
    }

    #[test]
    fn test_message_from_hex_str() {
        let msg = Message::from_plain_hex_str("b25b", 42);
        assert_eq!(msg.plain_bits_str(), "1011001001011011");
        assert_eq!(msg.pause, 42);
        assert_eq!(msg.plain_hex_str(), "b25b");
    }

    #[test]
    fn test_decoded_bits_file() {
        // reference data: tests/data/decoded_bits.txt (one message per line)
        let content = std::fs::read_to_string(format!(
            "{}/tests/data/decoded_bits.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("decoded_bits.txt");
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let bits_str = line.trim().to_string();
            let msg = Message::from_plain_bits_str(&bits_str, 0);
            assert_eq!(msg.plain_bits_str(), bits_str);

            // hex conversion must match the straightforward big-endian nibble
            // conversion of the bit string
            let padded = bits_str.clone() + &"0".repeat((4 - bits_str.len() % 4) % 4);
            let expected: String = padded
                .as_bytes()
                .chunks_exact(4)
                .map(|c| {
                    let v =
                        (c[0] - b'0') * 8 + (c[1] - b'0') * 4 + (c[2] - b'0') * 2 + (c[3] - b'0');
                    format!("{:x}", v)
                })
                .collect();
            assert_eq!(msg.plain_hex_str(), expected);
        }
    }

    #[test]
    fn test_label_bits_extraction() {
        let msg = Message::from_plain_bits_str("1111000010101010", 0).with_labels(vec![
            ProtocolLabel::new("preamble", 0, 7, 0),
            ProtocolLabel::new("data", 8, 15, 1),
        ]);
        assert_eq!(msg.label_bits(&msg.labels[0]), vec![1, 1, 1, 1, 0, 0, 0, 0]);
        assert_eq!(msg.label_bits(&msg.labels[1]), vec![1, 0, 1, 0, 1, 0, 1, 0]);

        // hex range of an 8 bit label is 2 nibbles
        assert_eq!(msg.get_label_range(&msg.labels[0], 1), (0, 2));
        assert_eq!(msg.get_label_range(&msg.labels[1], 2), (1, 2));
    }

    #[test]
    fn test_convert_index() {
        let msg = Message::from_plain_bits_str("1010101011110000", 0);
        assert_eq!(msg.convert_index(0, 0, 1), (0, 0));
        assert_eq!(msg.convert_index(7, 0, 1), (1, 1));
        assert_eq!(msg.convert_index(1, 1, 0), (4, 7));
        assert_eq!(msg.convert_index(0, 0, 2), (0, 0));
        assert_eq!(msg.convert_index(15, 0, 2), (1, 1));
    }

    #[test]
    fn test_ppseq_to_bits() {
        // pulse (1, 100 samples), pause 100 (1 symbol, <= threshold), pulse
        // (1, 300), pause 1000 (10 symbols > threshold 8), pulse (1, 100)
        let ppseq = vec![
            (1i64, 100i64),
            (-1, 100),
            (1, 300),
            (-1, 1000),
            (1, 100),
            (-1, 500),
        ];
        let (bit_data, pauses, bit_sample_pos) =
            ProtocolAnalyzer::ppseq_to_bits(&ppseq, 100, 1, 8);

        assert_eq!(bit_data.len(), 2);
        assert_eq!(bit_data[0], vec![1, 0, 1, 1, 1]);
        // trailing 500 sample pause: 5 symbols <= threshold -> zero bits
        assert_eq!(bit_data[1], vec![1, 0, 0, 0, 0, 0]);
        assert_eq!(pauses, vec![1000, 500]);
        assert_eq!(bit_sample_pos.len(), 2);
        // first bit of the first message starts at sample 0
        assert_eq!(bit_sample_pos[0][0], 0);
        // 5 bit positions + pause start/end positions
        assert_eq!(bit_sample_pos[0].len(), bit_data[0].len() + 2);
    }

    #[test]
    fn test_ppseq_to_bits_preamble_pause_ignored() {
        // leading pause without prior data is ignored
        let ppseq = vec![(-1, 5000), (1, 200), (0, 100), (1, 100), (-1, 5000)];
        let (bit_data, pauses, _) = ProtocolAnalyzer::ppseq_to_bits(&ppseq, 100, 1, 8);
        assert_eq!(bit_data.len(), 1);
        assert_eq!(bit_data[0], vec![1, 1, 0, 1]);
        assert_eq!(pauses, vec![5000]);
    }

    #[test]
    fn test_messages_from_bits() {
        let mut pa = ProtocolAnalyzer::new();
        pa.messages_from_bits(
            vec![vec![1, 0, 1, 1], vec![0, 0, 1]],
            vec![1000, 250],
        );
        assert_eq!(pa.num_messages(), 2);
        assert_eq!(pa.plain_bits_str(), vec!["1011".to_string(), "001".to_string()]);
        assert_eq!(pa.pauses(), vec![1000, 250]);
        assert_eq!(pa.plain_hex_str(), vec!["b".to_string(), "2".to_string()]); // 001 -> padded 0010
    }

    #[test]
    fn test_set_messages_from_ppseq() {
        let mut pa = ProtocolAnalyzer::new();
        let ppseq = vec![(1, 100), (-1, 1000), (1, 100), (0, 100), (-1, 2000)];
        pa.set_messages_from_ppseq(&ppseq, 100, 1, 8);
        assert_eq!(pa.num_messages(), 2);
        assert_eq!(pa.messages[0].plain_bits_str(), "1");
        assert_eq!(pa.messages[0].pause, 1000);
        assert_eq!(pa.messages[1].plain_bits_str(), "10");
        assert_eq!(pa.messages[1].pause, 2000);
        assert_eq!(pa.messages[0].samples_per_symbol, 100);
        assert_eq!(pa.messages[0].message_type, "Default");
    }

    #[test]
    fn test_ensure_message_length_multiple() {
        let mut pa = ProtocolAnalyzer::new();
        pa.set_messages_from_ppseq(&[(1, 100), (-1, 1000)], 100, 1, 8);
        assert_eq!(pa.messages[0].plain_bits.len(), 1);
        pa.ensure_message_length_multiple(8);
        // pause (1000 samples) is long enough to pad with 7 zero bits
        assert_eq!(pa.messages[0].plain_bits_str(), "10000000");
        assert_eq!(pa.messages[0].pause, 300);
    }

    #[test]
    fn test_analyze_signal_ask() {
        // generate an ASK-demodulated rectangular signal: 2 messages
        // 1011 with 100 samples per bit, separated by a 1000 sample pause
        let mut qad: Vec<f32> = Vec::new();
        let push = |state: f32, n: usize, qad: &mut Vec<f32>| {
            qad.extend(std::iter::repeat_n(state, n));
        };
        push(1.0, 100, &mut qad);
        push(0.0, 100, &mut qad);
        push(1.0, 300, &mut qad);
        push(0.0, 1000, &mut qad); // pause
        push(1.0, 100, &mut qad);
        push(0.0, 100, &mut qad);
        push(1.0, 100, &mut qad);
        push(0.0, 2000, &mut qad); // trailing pause

        let mut pa = ProtocolAnalyzer::new();
        pa.analyze_signal(&qad, 0.5, 0, crate::Modulation::Ask, 100, 1, 0.1, 8);
        assert_eq!(pa.num_messages(), 2, "{:?}", pa.plain_bits_str());
        assert_eq!(pa.plain_bits_str(), vec!["10111".to_string(), "101".to_string()]);
        assert_eq!(pa.pauses()[0], 1000);
        // trailing pause; grab_pulse_lens reports it one sample short
        assert!(pa.pauses()[1] >= 1900 && pa.pauses()[1] <= 2000, "{:?}", pa.pauses());
    }

    #[test]
    fn test_number_to_bits() {
        assert_eq!(number_to_bits(1, 1), vec![1]);
        assert_eq!(number_to_bits(0, 1), vec![0]);
        assert_eq!(number_to_bits(2, 2), vec![1, 0]);
        assert_eq!(bits_to_number(&[1, 0, 1, 1]), 11);
    }
}
