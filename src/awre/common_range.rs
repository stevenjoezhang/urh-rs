//! Shared range type for AWRE field inference (port of `CommonRange.py`).

use std::collections::BTreeSet;

use crate::Bit;

/// bit / hex / byte addressing of a range (URH `range_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeType {
    Bit,
    Hex,
    Byte,
}

/// A candidate field: same bit pattern at the same position across a set of
/// messages, with an inference score. Port of `urh.awre.CommonRange`.
#[derive(Debug, Clone)]
pub struct CommonRange {
    pub start: usize,
    pub length: usize,
    /// observed values (nibble-aligned bits when range_type != bit)
    pub values: Vec<Vec<Bit>>,
    pub score: f64,
    /// semantic tag: "Generic", "length", "address", "sequence number", ...
    pub field_type: String,
    pub range_type: RangeType,
    pub byte_order_big: bool,
    /// end of sync word (bit offset added when converting to absolute bits)
    pub sync_end: usize,
    /// indices of messages this range applies to
    pub message_indices: BTreeSet<usize>,
}

impl CommonRange {
    pub fn new(start: usize, length: usize) -> Self {
        CommonRange {
            start,
            length,
            values: Vec::new(),
            score: 0.0,
            field_type: "Generic".to_string(),
            range_type: RangeType::Bit,
            byte_order_big: true,
            sync_end: 0,
            message_indices: BTreeSet::new(),
        }
    }

    /// Inclusive end position (URH: `start + length - 1`).
    pub fn end(&self) -> usize {
        if self.length == 0 {
            self.start
        } else {
            self.start + self.length - 1
        }
    }

    /// The single value, if exactly one was observed (URH `CommonRange.value`).
    pub fn value(&self) -> Option<&[Bit]> {
        match self.values.len() {
            0 => None,
            1 => Some(&self.values[0]),
            _ => panic!("This range has multiple values!"),
        }
    }
}
