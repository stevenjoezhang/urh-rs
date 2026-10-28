//! Simulator item tree — port of `SimulatorItem.py` / `SimulatorMessage.py` /
//! `SimulatorRule.py` / `SimulatorCounterAction.py` / `SimulatorGotoAction.py`
//! / `SimulatorSleepAction.py` / `SimulatorTriggerCommandAction.py` /
//! `SimulatorProtocolLabel.py`.
//!
//! Python builds a mutable object tree with parent pointers and a dynamic
//! attribute system; this port uses an arena (`Vec<SimNode>`) owned by
//! [`crate::simulator::config::SimulatorConfiguration`] with `NodeId`
//! handles and a closed `NodeKind` enum.

use crate::encoding::GenericCRC;
use crate::Bit;

pub type NodeId = usize;

/// Port of `SimulatorRuleCondition.ConditionType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionType {
    If,
    ElseIf,
    Else,
}

impl ConditionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConditionType::If => "IF",
            ConditionType::ElseIf => "ELSE IF",
            ConditionType::Else => "ELSE",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "IF" => ConditionType::If,
            "ELSE IF" => ConditionType::ElseIf,
            "ELSE" => ConditionType::Else,
            _ => return None,
        })
    }
}

/// Port of the label payload of `SimulatorProtocolLabel` (which wraps a
/// `ProtocolLabel` or `ChecksumLabel` via `__getattr__` delegation).
#[derive(Debug, Clone)]
pub struct SimLabel {
    pub name: String,
    /// Start bit (inclusive).
    pub start: usize,
    /// End bit (exclusive), URH's `ProtocolLabel.end`.
    pub end: usize,
    pub color_index: usize,
    pub display_format_index: usize,
    /// 0 = constant, 1 = live input, 2 = formula, 3 = external program,
    /// 4 = random value (`SimulatorProtocolLabel.VALUE_TYPES`).
    pub value_type_index: usize,
    pub formula: String,
    pub external_program: String,
    pub random_min: i64,
    pub random_max: i64,
    /// Present when the wrapped label is a `ChecksumLabel`.
    pub checksum: Option<ChecksumSpec>,
}

impl SimLabel {
    pub fn new(name: &str, start: usize, end: usize) -> Self {
        let len = end.saturating_sub(start);
        SimLabel {
            name: name.to_string(),
            start,
            end,
            color_index: 0,
            display_format_index: 0,
            value_type_index: 0,
            formula: String::new(),
            external_program: String::new(),
            random_min: 0,
            // Python: random_max = label.fuzz_maximum - 1
            random_max: if len >= 63 { i64::MAX } else { (1i64 << len) - 1 },
            checksum: None,
        }
    }

    pub fn length(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

/// Port of `ChecksumLabel`'s generic-CRC configuration.
#[derive(Clone)]
pub struct ChecksumSpec {
    /// Bit ranges (start, end) over which the checksum is computed.
    pub data_ranges: Vec<(usize, usize)>,
    pub crc: GenericCRC,
}

impl std::fmt::Debug for ChecksumSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChecksumSpec")
            .field("data_ranges", &self.data_ranges)
            .field("polynomial", &self.crc.polynomial)
            .finish()
    }
}

impl ChecksumSpec {
    /// `ChecksumLabel.calculate_checksum_for_message` restricted to plain
    /// bits (the simulator engine's NRZ path).
    pub fn calculate(&self, bits: &[Bit]) -> Vec<Bit> {
        let mut data: Vec<Bit> = Vec::new();
        for (start, end) in &self.data_ranges {
            let start = (*start).min(bits.len());
            let end = (*end).min(bits.len());
            data.extend_from_slice(&bits[start..end]);
        }
        self.crc.crc(&data)
    }
}

/// Port of `SimulatorMessage` (a `Message` + `SimulatorItem` hybrid).
#[derive(Debug, Clone)]
pub struct SimMessage {
    pub plain_bits: Vec<Bit>,
    pub pause: u64,
    pub message_type_name: String,
    /// Index into `SimulatorConfiguration::participants` (Python: the
    /// `participant` attribute, exposed as `source`).
    pub source: Option<usize>,
    /// Index into `SimulatorConfiguration::participants`.
    pub destination: Option<usize>,
    pub repeat: usize,
    pub modulator_index: usize,
    pub decoding_index: usize,
    /// Messages actually sent/received during a simulation run. For sent
    /// messages these are the (plain) bits; for received messages the
    /// decoded bits (identical for the NRZ decoding used by the engine).
    pub send_recv: Vec<Vec<Bit>>,
}

impl SimMessage {
    pub fn new(plain_bits: Vec<Bit>, pause: u64) -> Self {
        SimMessage {
            plain_bits,
            pause,
            message_type_name: "default".to_string(),
            source: None,
            destination: None,
            repeat: 1,
            modulator_index: 0,
            decoding_index: 0,
            send_recv: Vec::new(),
        }
    }

    /// `SimulatorMessage.plain_bits_str`: last sent/received message if any,
    /// else the template bits.
    pub fn effective_bits(&self) -> &[Bit] {
        match self.send_recv.last() {
            Some(bits) => bits,
            None => &self.plain_bits,
        }
    }
}

/// The closed variant set replacing Python's simulator item class hierarchy.
#[derive(Debug, Clone)]
pub enum NodeKind {
    Root,
    Message(SimMessage),
    Label(SimLabel),
    /// `SimulatorRule`
    Rule,
    /// `SimulatorRuleCondition`
    RuleCondition {
        cond_type: ConditionType,
        condition: String,
    },
    /// `SimulatorGotoAction`
    Goto {
        goto_target: Option<String>,
    },
    /// `SimulatorSleepAction`
    Sleep {
        sleep_time: f64,
    },
    /// `SimulatorCounterAction` (start/step plus the running value)
    Counter {
        start: i64,
        step: i64,
        value: i64,
    },
    /// `SimulatorTriggerCommandAction`
    TriggerCommand {
        command: Option<String>,
        pass_transcript: bool,
        return_code: i64,
    },
}

/// One node of the simulator tree. Parents/children are `NodeId`s into the
/// owning arena.
#[derive(Debug, Clone)]
pub struct SimNode {
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub kind: NodeKind,
    pub logging_active: bool,
    pub is_valid: bool,
}

impl SimNode {
    pub fn new(kind: NodeKind) -> Self {
        SimNode {
            parent: None,
            children: Vec::new(),
            kind,
            logging_active: true,
            is_valid: true,
        }
    }

    pub fn root() -> Self {
        SimNode::new(NodeKind::Root)
    }
}

/// Port of `Formatter.science_time` (used by the sleep action caption).
pub fn science_time(time_in_seconds: f64) -> String {
    // Python branches on `<` (upper bounds), so 0.0 ends up in the ns bucket.
    if time_in_seconds < 1e-6 {
        format!("{:.2} ns", time_in_seconds * 1e9)
    } else if time_in_seconds < 1e-3 {
        format!("{:.2} µs", time_in_seconds * 1e6)
    } else if time_in_seconds < 1.0 {
        format!("{:.2} ms", time_in_seconds * 1e3)
    } else {
        format!("{time_in_seconds:.2} s")
    }
}
