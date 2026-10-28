//! Port of `urh/awre/MessageTypeBuilder.py` plus the label typing layer.
//!
//! DTO mapping vs. Python (see report): the crate's `ProtocolLabel` has no
//! field type and no checksum payload, and `Message` references its message
//! type only by name. `AnalyzedMessageType` therefore pairs a
//! `crate::protocol::MessageType` (labels by value, kept in sync) with a
//! parallel `typed_labels` list carrying the `FieldType.Function` and, for
//! checksum labels, the data ranges and CRC parameters of Python's
//! `ChecksumLabel`.

use crate::awre::engines::checksum::ChecksumSpec;
use crate::protocol::{Message, MessageType, ProtocolLabel};

/// `FieldType.Function` from `urh/signalprocessing/FieldType.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldFunction {
    Preamble,
    Sync,
    Length,
    SrcAddress,
    DstAddress,
    SequenceNumber,
    Type,
    Data,
    Checksum,
    Custom,
}

impl FieldFunction {
    /// The Python enum's *value*, which doubles as the field type caption
    /// and (in this port) the auto-created label name.
    pub fn caption(&self) -> &'static str {
        match self {
            FieldFunction::Preamble => "preamble",
            FieldFunction::Sync => "synchronization",
            FieldFunction::Length => "length",
            FieldFunction::SrcAddress => "source address",
            FieldFunction::DstAddress => "destination address",
            FieldFunction::SequenceNumber => "sequence number",
            FieldFunction::Type => "type",
            FieldFunction::Data => "data",
            FieldFunction::Checksum => "checksum",
            FieldFunction::Custom => "custom",
        }
    }

    pub fn from_caption(caption: &str) -> Option<Self> {
        Some(match caption {
            "preamble" => FieldFunction::Preamble,
            "synchronization" => FieldFunction::Sync,
            "length" => FieldFunction::Length,
            "source address" => FieldFunction::SrcAddress,
            "destination address" => FieldFunction::DstAddress,
            "sequence number" => FieldFunction::SequenceNumber,
            "type" => FieldFunction::Type,
            "data" => FieldFunction::Data,
            "checksum" => FieldFunction::Checksum,
            "custom" => FieldFunction::Custom,
            _ => return None,
        })
    }
}

/// Checksum payload of a `ChecksumLabel` (data ranges + CRC parameters /
/// WSP category).
#[derive(Debug, Clone)]
pub struct ChecksumAnnotation {
    /// list of (start, end) bit ranges the CRC is computed over (end
    /// exclusive, like Python's `data_ranges`)
    pub data_ranges: Vec<(usize, usize)>,
    pub checksum: ChecksumSpec,
}

/// A protocol label together with its field type function.
#[derive(Debug, Clone)]
pub struct TypedLabel {
    pub label: ProtocolLabel,
    pub function: FieldFunction,
    pub checksum: Option<ChecksumAnnotation>,
}

/// A `MessageType` whose labels carry field types — the working type of the
/// AWRE orchestration (Python: `MessageType` full of `ProtocolLabel`s with
/// `field_type`, plus a list of member message indices).
#[derive(Debug, Clone)]
pub struct AnalyzedMessageType {
    pub message_type: MessageType,
    /// parallel to `message_type.labels`
    pub typed_labels: Vec<TypedLabel>,
    /// indices of messages belonging to this type (FormatFinder bookkeeping)
    pub message_indices: Vec<usize>,
}

impl AnalyzedMessageType {
    pub fn new(name: &str) -> Self {
        AnalyzedMessageType {
            message_type: MessageType::new(name),
            typed_labels: Vec::new(),
            message_indices: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.message_type.labels.clear();
        self.typed_labels.clear();
    }

    pub fn len(&self) -> usize {
        self.message_type.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.message_type.labels.is_empty()
    }

    /// `MessageType.append`: append and keep sorted by start.
    /// Returns false (and does not append) when the label overlaps an
    /// existing one, like `add_label(allow_overlapping=false)`.
    pub fn add_typed_label(&mut self, typed: TypedLabel, allow_overlapping: bool) -> bool {
        if !allow_overlapping
            && self
                .message_type
                .labels
                .iter()
                .any(|l| l.overlaps_with(&typed.label))
        {
            return false;
        }
        self.message_type.labels.push(typed.label.clone());
        self.message_type
            .labels
            .sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
        self.typed_labels.push(typed);
        self.typed_labels
            .sort_by(|a, b| a.label.start.cmp(&b.label.start).then(a.label.end.cmp(&b.label.end)));
        true
    }

    /// `MessageType.get_first_label_with_type`.
    pub fn get_first_label_with_type(&self, function: FieldFunction) -> Option<&TypedLabel> {
        self.typed_labels
            .iter()
            .find(|tl| tl.function == function)
    }

    /// `MessageType.num_labels_with_type`.
    pub fn num_labels_with_type(&self, function: FieldFunction) -> usize {
        self.typed_labels
            .iter()
            .filter(|tl| tl.function == function)
            .count()
    }

    /// First SRC address value of a message as hex string
    /// (`Message.get_src_address_from_data`, simplified: no view conversion,
    /// the label range is interpreted as plain nibbles).
    pub fn get_src_address_from_data(&self, msg: &Message) -> Option<String> {
        let lbl = self.get_first_label_with_type(FieldFunction::SrcAddress)?;
        let bits = msg.label_bits(&lbl.label);
        if bits.is_empty() {
            return None;
        }
        let mut hex = String::new();
        for chunk in bits.chunks(4) {
            let mut v = 0u8;
            for &b in chunk {
                v = (v << 1) | b;
            }
            hex.push(char::from_digit(v as u32, 16)?);
        }
        Some(hex)
    }
}

/// Port of `urh/awre/MessageTypeBuilder.py`.
pub struct MessageTypeBuilder {
    pub message_type: AnalyzedMessageType,
}

impl MessageTypeBuilder {
    pub fn new(name: &str) -> Self {
        MessageTypeBuilder {
            message_type: AnalyzedMessageType::new(name),
        }
    }

    fn next_start_and_color(&self) -> (usize, usize) {
        match self.message_type.typed_labels.last() {
            Some(last) => (last.label.end, last.label.color_index + 1),
            None => (0, 0),
        }
    }

    pub fn add_label(&mut self, label_type: FieldFunction, length: usize, name: Option<&str>) {
        let (start, color_index) = self.next_start_and_color();
        let name = name.unwrap_or(label_type.caption());
        let label = ProtocolLabel::new(name, start, start + length - 1, color_index);
        self.message_type
            .add_typed_label(TypedLabel { label, function: label_type, checksum: None }, true);
    }

    pub fn add_checksum_label(
        &mut self,
        length: usize,
        checksum: ChecksumSpec,
        data_start: Option<usize>,
        data_end: Option<usize>,
        name: Option<&str>,
    ) {
        let label_type = FieldFunction::Checksum;
        let (start, color_index) = self.next_start_and_color();
        let name = name.unwrap_or(label_type.caption());

        let data_start = data_start.unwrap_or_else(|| {
            // End of sync or preamble
            if let Some(sync) = self
                .message_type
                .get_first_label_with_type(FieldFunction::Sync)
            {
                sync.label.end
            } else if let Some(preamble) = self
                .message_type
                .get_first_label_with_type(FieldFunction::Preamble)
            {
                preamble.label.end
            } else {
                0
            }
        });
        let data_end = data_end.unwrap_or(start);

        let label = ProtocolLabel::new(name, start, start + length - 1, color_index);
        self.message_type.add_typed_label(
            TypedLabel {
                label,
                function: label_type,
                checksum: Some(ChecksumAnnotation {
                    data_ranges: vec![(data_start, data_end)],
                    checksum,
                }),
            },
            true,
        );
    }
}
