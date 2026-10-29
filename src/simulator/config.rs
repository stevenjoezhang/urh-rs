//! Port of `urh/simulator/SimulatorConfiguration.py` — the configuration
//! model (item tree arena, participants, modulators, decodings, item
//! dictionary) plus XML save/load using quick-xml, in the same spirit as
//! `crate::proto_xml`.

use crate::encoding::GenericCRC;
use crate::modulation::modulate_gauss;
use crate::Modulation;
use crate::simulator::expression::{ExpressionParser, IdentifierResolver, Value};
use crate::simulator::item::{
    ConditionType, NodeId, NodeKind, SimLabel, SimMessage, SimNode,
};
use crate::{Bit, Complex32};

pub const MODULATION_TYPES: [&str; 5] = ["ASK", "FSK", "PSK", "GFSK", "OQPSK"];

/// Port of the `Participant` attributes the simulator needs (the reference
/// stores the full `urh.signalprocessing.Participant`).
#[derive(Debug, Clone, PartialEq)]
pub struct SimParticipant {
    pub name: String,
    pub shortname: String,
    pub address_hex: String,
    pub color_index: usize,
    pub relative_rssi: i64,
    pub id: String,
    /// Python stores an int; 0 = external (not simulated), non-zero = the
    /// simulator plays this participant.
    pub simulate: i64,
}

impl SimParticipant {
    pub fn new(name: &str, shortname: &str) -> Self {
        SimParticipant {
            name: name.to_string(),
            shortname: shortname.to_string(),
            address_hex: String::new(),
            color_index: 0,
            relative_rssi: 0,
            id: format!("participant-{}", name.to_lowercase().replace(' ', "_")),
            simulate: 0,
        }
    }

    pub fn simulates(&self) -> bool {
        self.simulate != 0
    }
}

impl From<crate::awre::auto_assigner::Participant> for SimParticipant {
    fn from(p: crate::awre::auto_assigner::Participant) -> Self {
        let id = format!("participant-{}", p.name.to_lowercase().replace(' ', "_"));
        SimParticipant {
            name: p.name,
            shortname: p.shortname,
            address_hex: p.address_hex,
            color_index: 0,
            relative_rssi: p.relative_rssi as i64,
            id,
            simulate: 0,
        }
    }
}

/// Port of the `Modulator` attributes the simulator needs for sending.
/// Parameter semantics follow `Modulator.modulate`: ASK parameters are
/// percentages scaled by the carrier amplitude, PSK parameters are degrees.
#[derive(Debug, Clone)]
pub struct SimModulator {
    pub name: String,
    /// Index into [`MODULATION_TYPES`].
    pub modulation_type: usize,
    pub samples_per_symbol: u32,
    pub carrier_freq_hz: f32,
    pub carrier_amplitude: f32,
    pub carrier_phase_deg: f32,
    pub param_for_zero: f32,
    pub param_for_one: f32,
    /// `None` mirrors Python's `sample_rate = None` (defaults to 1 MHz).
    pub sample_rate: Option<f32>,
    pub bits_per_symbol: u16,
    pub gauss_bt: f32,
    pub gauss_filter_width: f32,
}

impl Default for SimModulator {
    fn default() -> Self {
        SimModulator {
            name: "Modulator".to_string(),
            modulation_type: 0,
            samples_per_symbol: 100,
            carrier_freq_hz: 40_000.0,
            carrier_amplitude: 1.0,
            carrier_phase_deg: 0.0,
            param_for_zero: 0.0,
            param_for_one: 100.0,
            sample_rate: None,
            bits_per_symbol: 1,
            gauss_bt: 0.5,
            gauss_filter_width: 1.0,
        }
    }
}

impl SimModulator {
    pub fn modulation(&self) -> Modulation {
        Modulation::from_str(MODULATION_TYPES[self.modulation_type]).unwrap()
    }

    /// `Modulator.modulate(data, pause)` for the float32 dtype used by the
    /// simulator (full-scale amplitude 1.0).
    pub fn modulate(&self, bits: &[Bit], pause: u32) -> Vec<Complex32> {
        let a = self.carrier_amplitude; // dtype float32 -> min_max[1] == 1.0
        let parameters: Vec<f32> = match self.modulation() {
            Modulation::Ask => vec![
                a * self.param_for_zero / 100.0,
                a * self.param_for_one / 100.0,
            ],
            Modulation::Psk => vec![
                self.param_for_zero * (std::f32::consts::PI / 180.0),
                self.param_for_one * (std::f32::consts::PI / 180.0),
            ],
            _ => vec![self.param_for_zero, self.param_for_one],
        };
        modulate_gauss(
            bits,
            self.samples_per_symbol,
            self.modulation(),
            &parameters,
            self.bits_per_symbol,
            a,
            self.carrier_freq_hz,
            self.carrier_phase_deg * (std::f32::consts::PI / 180.0),
            self.sample_rate.unwrap_or(1e6),
            pause,
            0,
            self.gauss_bt,
            self.gauss_filter_width,
        )
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SimulatorConfiguration {
    nodes: Vec<SimNode>,
    pub root: NodeId,
    pub participants: Vec<SimParticipant>,
    pub modulators: Vec<SimModulator>,
    pub decodings: Vec<String>,
    /// Insertion-ordered identifier -> node mapping (Python's OrderedDict).
    item_dict: Vec<(String, NodeId)>,
}

impl Default for SimulatorConfiguration {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatorConfiguration {
    pub fn new() -> Self {
        let nodes = vec![SimNode::root()];
        // reserve the standard decodings like a fresh URH project
        let decodings = [
            "'Non Return To Zero (NRZ)', ",
            "'Non Return To Zero Inverted (NRZ-I)', 'Invert', ",
            "'Manchester I', 'Edge Trigger', ",
            "'Manchester II', 'Edge Trigger', 'Invert', ",
            "'Differential Manchester', 'Edge Trigger', 'Differential Encoding', ",
            "'Wireless Short Packet', 'Wireless Short Packet (WSP)', ",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut config = SimulatorConfiguration {
            nodes,
            root: 0,
            participants: Vec::new(),
            modulators: vec![SimModulator::default()],
            decodings,
            item_dict: Vec::new(),
        };
        config.update_item_dict();
        config
    }

    // -- arena access --

    pub fn node(&self, id: NodeId) -> &SimNode {
        &self.nodes[id]
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut SimNode {
        &mut self.nodes[id]
    }

    pub fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    /// `SimulatorItem.index()`: 1-based positions from the root, dot-joined.
    pub fn index_of(&self, id: NodeId) -> String {
        let Some(parent) = self.nodes[id].parent else {
            return String::new();
        };
        let pos = self.nodes[parent]
            .children
            .iter()
            .position(|c| *c == id)
            .unwrap_or(0);
        let own = format!("{}", pos + 1);
        let parent_index = self.index_of(parent);
        if parent_index.is_empty() {
            own
        } else {
            format!("{parent_index}.{own}")
        }
    }

    /// `SimulatorItem.get_pos`.
    pub fn pos_of(&self, id: NodeId) -> usize {
        match self.nodes[id].parent {
            Some(parent) => self.nodes[parent]
                .children
                .iter()
                .position(|c| *c == id)
                .unwrap_or(0),
            None => 0,
        }
    }

    /// `SimulatorItem.insert_child` / `add_child`: attach a new node with the
    /// given kind. Returns its id.
    pub fn add_item(&mut self, kind: NodeKind, parent: Option<NodeId>) -> NodeId {
        let parent = parent.unwrap_or(self.root);
        let id = self.nodes.len();
        let mut node = SimNode::new(kind);
        node.parent = Some(parent);
        self.nodes.push(node);
        self.nodes[parent].children.push(id);
        self.update_item_dict();
        id
    }

    /// `SimulatorConfiguration.add_items(items, pos, parent)` (appends at the
    /// given position; `pos` beyond the child count appends).
    pub fn add_items(&mut self, kinds: Vec<NodeKind>, pos: usize, parent: Option<NodeId>) -> Vec<NodeId> {
        let parent = parent.unwrap_or(self.root);
        let mut ids = Vec::with_capacity(kinds.len());
        let mut pos = pos.min(self.nodes[parent].children.len());
        for kind in kinds {
            let id = self.nodes.len();
            let mut node = SimNode::new(kind);
            node.parent = Some(parent);
            self.nodes.push(node);
            self.nodes[parent].children.insert(pos, id);
            pos += 1;
            ids.push(id);
        }
        self.update_item_dict();
        ids
    }

    /// `SimulatorItem.delete()`: detach from the tree (children of the
    /// deleted item become detached as in Python's `set_parent(None)` walk).
    pub fn delete_item(&mut self, id: NodeId) {
        let children = self.nodes[id].children.clone();
        for child in children {
            self.nodes[child].parent = None;
            self.nodes[id].children.clear();
        }
        if let Some(parent) = self.nodes[id].parent {
            self.nodes[parent].children.retain(|c| *c != id);
        }
        self.nodes[id].parent = None;
        self.update_item_dict();
    }

    /// `SimulatorItem.next_sibling`.
    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.nodes[id].parent?;
        let pos = self.nodes[parent]
            .children
            .iter()
            .position(|c| *c == id)?;
        self.nodes[parent].children.get(pos + 1).copied()
    }

    /// `SimulatorItem.prev_sibling`.
    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.nodes[id].parent?;
        let pos = self.nodes[parent]
            .children
            .iter()
            .position(|c| *c == id)?;
        if pos > 0 {
            Some(self.nodes[parent].children[pos - 1])
        } else {
            None
        }
    }

    /// `SimulatorItem.next()`: first child, else next sibling, else the next
    /// sibling of the nearest ancestor.
    pub fn next(&self, id: NodeId) -> Option<NodeId> {
        if let Some(first) = self.nodes[id].children.first() {
            return Some(*first);
        }
        let mut curr = id;
        while let Some(parent) = self.nodes[curr].parent {
            if let Some(sibling) = self.next_sibling(curr) {
                return Some(sibling);
            }
            curr = parent;
        }
        None
    }

    /// `SimulatorItem.prev()`.
    pub fn prev(&self, id: NodeId) -> Option<NodeId> {
        let prev = self.prev_sibling(id)?;
        let mut curr = prev;
        while let Some(last) = self.nodes[curr].children.last() {
            curr = *last;
        }
        Some(curr)
    }

    /// `get_all_items()`: pre-order over the root's children.
    pub fn get_all_items(&self) -> Vec<NodeId> {
        let mut items = Vec::new();
        for child in self.nodes[self.root].children.clone() {
            self.collect_items(child, &mut items);
        }
        items
    }

    fn collect_items(&self, id: NodeId, items: &mut Vec<NodeId>) {
        items.push(id);
        for child in self.nodes[id].children.clone() {
            self.collect_items(child, items);
        }
    }

    /// `get_all_messages()`.
    pub fn get_all_messages(&self) -> Vec<NodeId> {
        self.get_all_items()
            .into_iter()
            .filter(|id| matches!(self.nodes[*id].kind, NodeKind::Message(_)))
            .collect()
    }

    pub fn message(&self, id: NodeId) -> Option<&SimMessage> {
        match &self.nodes[id].kind {
            NodeKind::Message(m) => Some(m),
            _ => None,
        }
    }

    pub fn message_mut(&mut self, id: NodeId) -> Option<&mut SimMessage> {
        match &mut self.nodes[id].kind {
            NodeKind::Message(m) => Some(m),
            _ => None,
        }
    }

    /// Labels of a message node (Python: `SimulatorMessage.children`, which
    /// is the message type's label list).
    pub fn labels_of(&self, msg_id: NodeId) -> Vec<NodeId> {
        self.nodes[msg_id].children.clone()
    }

    pub fn participant(&self, idx: Option<usize>) -> Option<&SimParticipant> {
        self.participants.get(idx?)
    }

    /// Python `msg.source.simulate` truthiness.
    pub fn participant_simulates(&self, idx: Option<usize>) -> bool {
        self.participant(idx).map(|p| p.simulates()).unwrap_or(false)
    }

    // -- item dictionary --

    /// `update_item_dict()`.
    ///
    /// Key scheme (Python): `"item" + index.replace('.', '_')` plus
    /// `.<name>` for labels, `.counter_value` for counters and `.rc` for
    /// trigger commands. Note the path separators become underscores while
    /// the suffix keeps its dot: `item2_1.data`, `item1.counter_value`.
    pub fn update_item_dict(&mut self) {
        self.item_dict.clear();
        for id in self.get_all_items() {
            match &self.nodes[id].kind {
                NodeKind::Label(lbl) => {
                    let parent_index = self
                        .nodes[id]
                        .parent
                        .map(|p| self.index_of(p))
                        .unwrap_or_default();
                    let name = format!(
                        "item{}.{}",
                        parent_index.replace('.', "_"),
                        lbl.name.replace(' ', "_")
                    );
                    self.item_dict.push((name, id));
                }
                NodeKind::Counter { .. } => {
                    let index = self.index_of(id).replace('.', "_");
                    self.item_dict
                        .push((format!("item{index}.counter_value"), id));
                }
                NodeKind::TriggerCommand { .. } => {
                    let index = self.index_of(id).replace('.', "_");
                    self.item_dict.push((format!("item{index}"), id));
                    self.item_dict.push((format!("item{index}.rc"), id));
                }
                _ => {
                    let index = self.index_of(id).replace('.', "_");
                    self.item_dict.push((format!("item{index}"), id));
                }
            }
        }
    }

    /// Lookup an identifier (e.g. `item1.sequence_number`).
    pub fn lookup_item(&self, name: &str) -> Option<NodeId> {
        self.item_dict
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| *v)
    }

    /// `get_identifiers()`.
    pub fn get_identifiers(&self) -> Vec<String> {
        self.item_dict
            .iter()
            .map(|(k, v)| (k, *v))
            .filter(|(k, v)| self.is_valid_identifier_internal(k, *v))
            .map(|(k, _)| k.clone())
            .collect()
    }

    fn is_valid_identifier_internal(&self, identifier: &str, id: NodeId) -> bool {
        match &self.nodes[id].kind {
            NodeKind::Label(_) | NodeKind::Counter { .. } => true,
            NodeKind::TriggerCommand { .. } => identifier.ends_with("rc"),
            _ => false,
        }
    }

    /// `SimulatorGotoAction.is_valid_goto_target`.
    pub fn is_valid_goto_target(&self, caption: &str, id: Option<NodeId>) -> bool {
        let Some(id) = id else { return false };
        match &self.nodes[id].kind {
            NodeKind::Label(_) | NodeKind::Rule => false,
            NodeKind::RuleCondition { cond_type, .. } => *cond_type == ConditionType::If,
            NodeKind::Counter { .. } => false,
            NodeKind::TriggerCommand { .. } => !caption.ends_with("rc"),
            _ => true,
        }
    }

    /// `SimulatorGotoAction.validate` + `.target`.
    pub fn goto_target(&self, goto_id: NodeId) -> Option<NodeId> {
        let target_name = match &self.nodes[goto_id].kind {
            NodeKind::Goto { goto_target } => goto_target.clone(),
            _ => return None,
        }?;
        let target = self.lookup_item(&target_name);
        if self.is_valid_goto_target(&target_name, target) {
            target
        } else {
            None
        }
    }

    /// `get_valid_goto_targets`.
    pub fn get_valid_goto_targets(&self, goto_id: NodeId) -> Vec<String> {
        let mut result = Vec::new();
        for (key, id) in &self.item_dict {
            if *id != goto_id && self.is_valid_goto_target(key, Some(*id)) {
                result.push(key.clone());
            }
        }
        result
    }

    // -- validation --

    /// `update_valid_states` + `protocol_valid`.
    pub fn protocol_valid(&mut self) -> bool {
        for child in self.nodes[self.root].children.clone() {
            self.update_valid_states_rec(child);
        }
        self.get_all_items().iter().all(|id| self.nodes[*id].is_valid)
    }

    fn update_valid_states_rec(&mut self, id: NodeId) {
        for child in self.nodes[id].children.clone() {
            self.update_valid_states_rec(child);
        }
        let valid = self.validate_node(id);
        self.nodes[id].is_valid = valid;
    }

    /// `SimulatorItem.validate()` dispatch.
    fn validate_node(&self, id: NodeId) -> bool {
        match &self.nodes[id].kind {
            NodeKind::RuleCondition { cond_type, condition } => {
                if *cond_type == ConditionType::Else {
                    return true;
                }
                ExpressionParser::new(self)
                    .validate_condition(condition)
                    .is_ok()
            }
            NodeKind::Label(lbl) => match lbl.value_type_index {
                2 => ExpressionParser::new(self).validate_formula(&lbl.formula).is_ok(),
                3 => !lbl.external_program.trim().is_empty(),
                _ => true,
            },
            NodeKind::Goto { goto_target } => {
                let target = goto_target.as_ref().and_then(|t| self.lookup_item(t));
                self.is_valid_goto_target(goto_target.as_deref().unwrap_or(""), target)
            }
            NodeKind::Message(_) => self.nodes[id]
                .children
                .iter()
                .all(|c| self.nodes[*c].is_valid),
            _ => true,
        }
    }

    /// `consolidate_messages()`: merge consecutive identical messages into
    /// one message with an increased `repeat`.
    pub fn consolidate_messages(&mut self) {
        let mut current = Some(self.root);
        let mut redundant: Vec<NodeId> = Vec::new();
        let mut updated: Vec<NodeId> = Vec::new();

        while let Some(c) = current {
            if matches!(self.nodes[c].kind, NodeKind::Message(_)) {
                let first = c;
                let mut cur = c;
                let mut repeat_counter = 0usize;
                loop {
                    let sibling = self.next_sibling(cur);
                    let same = sibling
                        .map(|s| {
                            matches!(self.nodes[s].kind, NodeKind::Message(_))
                                && self.message_bits(first) == self.message_bits(s)
                        })
                        .unwrap_or(false);
                    if !same {
                        break;
                    }
                    repeat_counter += 1;
                    cur = sibling.unwrap();
                    redundant.push(cur);
                }
                if repeat_counter > 0 {
                    if let Some(m) = self.message_mut(first) {
                        m.repeat += repeat_counter;
                    }
                    updated.push(first);
                }
                current = self.next(cur);
            } else {
                current = self.next(c);
            }
        }

        for id in redundant {
            self.delete_item(id);
        }
        self.update_item_dict();
    }

    fn message_bits(&self, id: NodeId) -> Vec<Bit> {
        self.message(id).map(|m| m.plain_bits.clone()).unwrap_or_default()
    }

    // -- expression resolver over the item tree --

    fn label_bits_and_ascii(&self, label_id: NodeId) -> Option<(&SimLabel, Vec<Bit>)> {
        let lbl = match &self.nodes[label_id].kind {
            NodeKind::Label(lbl) => lbl,
            _ => return None,
        };
        let parent = self.nodes[label_id].parent?;
        let msg = self.message(parent)?;
        Some((lbl, msg.effective_bits().to_vec()))
    }
}

impl IdentifierResolver for SimulatorConfiguration {
    fn is_valid_identifier(&self, identifier: &str) -> bool {
        match self.lookup_item(identifier) {
            Some(id) => self.is_valid_identifier_internal(identifier, id),
            None => false,
        }
    }

    fn resolve(&self, identifier: &str, as_string: bool) -> Option<Value> {
        let id = self.lookup_item(identifier)?;
        match &self.nodes[id].kind {
            NodeKind::Label(_) => {
                let (lbl, bits) = self.label_bits_and_ascii(id)?;
                let start = lbl.start.min(bits.len());
                let end = lbl.end.min(bits.len());
                if as_string {
                    // Python: get_label_range(lbl, 2, False) over plain_ascii_str
                    let ascii = bits_to_ascii_padded(&bits);
                    let cs = start / 8;
                    let ce = end.div_ceil(8);
                    Some(Value::Str(
                        ascii.chars().skip(cs).take(ce.saturating_sub(cs)).collect(),
                    ))
                } else {
                    // Python: int(plain_bits_str[start:end], 2)
                    let mut n: i64 = 0;
                    for b in &bits[start..end] {
                        n = (n << 1) | (b & 1) as i64;
                    }
                    Some(Value::Int(n))
                }
            }
            NodeKind::Counter { value, .. } => Some(Value::Int(*value)),
            NodeKind::TriggerCommand { return_code, .. } => Some(Value::Int(*return_code)),
            _ => None,
        }
    }
}

fn bits_to_ascii_padded(bits: &[Bit]) -> String {
    let mut padded = bits.to_vec();
    while !padded.len().is_multiple_of(8) {
        padded.push(0);
    }
    padded
        .chunks(8)
        .map(|c| {
            let v = c.iter().fold(0u8, |acc, b| (acc << 1) | (b & 1));
            v as char
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Mini DOM + XML load/save
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct XmlElem {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<XmlElem>,
    text: String,
}

impl XmlElem {
    fn new(name: &str) -> Self {
        XmlElem {
            name: name.to_string(),
            attrs: Vec::new(),
            children: Vec::new(),
            text: String::new(),
        }
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn set_attr(&mut self, name: &str, value: &str) {
        self.attrs.push((name.to_string(), value.to_string()));
    }

    fn find(&self, name: &str) -> Option<&XmlElem> {
        self.children.iter().find(|c| c.name == name)
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlElem> {
        self.children.iter().filter(move |c| c.name == name)
    }
}

fn parse_xml_dom(input: &str) -> Result<XmlElem, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(input);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<XmlElem> = Vec::new();
    let mut root: Option<XmlElem> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let mut elem = XmlElem::new(&name);
                for a in e.attributes().flatten() {
                    elem.attrs.push((
                        String::from_utf8_lossy(a.key.as_ref()).to_string(),
                        String::from_utf8_lossy(&a.value).to_string(),
                    ));
                }
                stack.push(elem);
            }
            Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let mut elem = XmlElem::new(&name);
                for a in e.attributes().flatten() {
                    elem.attrs.push((
                        String::from_utf8_lossy(a.key.as_ref()).to_string(),
                        String::from_utf8_lossy(&a.value).to_string(),
                    ));
                }
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(elem);
                } else if root.is_none() {
                    root = Some(elem);
                }
            }
            Ok(Event::End(_)) => {
                let elem = stack.pop().ok_or("unbalanced end tag")?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(elem);
                } else if root.is_none() {
                    root = Some(elem);
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(parent) = stack.last_mut() {
                    parent.text.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("XML parse error: {e}")),
        }
    }
    root.ok_or_else(|| "no root element".to_string())
}

fn write_xml_dom(elem: &XmlElem) -> String {
    let mut out = String::from("<?xml version=\"1.0\" ?>\n");
    write_elem(&mut out, elem, 0);
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn write_elem(out: &mut String, elem: &XmlElem, depth: usize) {
    let indent = "  ".repeat(depth);
    out.push_str(&indent);
    out.push('<');
    out.push_str(&elem.name);
    for (k, v) in &elem.attrs {
        out.push_str(&format!(" {}=\"{}\"", k, xml_escape(v)));
    }
    if elem.children.is_empty() && elem.text.is_empty() {
        out.push_str("/>\n");
        return;
    }
    out.push('>');
    if !elem.text.is_empty() {
        out.push_str(&xml_escape(&elem.text));
    } else {
        out.push('\n');
    }
    for child in &elem.children {
        write_elem(out, child, depth + 1);
    }
    if !elem.children.is_empty() {
        out.push_str(&indent);
    }
    out.push_str(&format!("</{}>\n", elem.name));
}

fn parse_i64(s: Option<&str>, default: i64) -> i64 {
    s.and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn parse_f64(s: Option<&str>, default: f64) -> f64 {
    s.and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn parse_bit_str(s: &str) -> Vec<Bit> {
    s.bytes()
        .filter(|b| *b == b'0' || *b == b'1')
        .map(|b| b - b'0')
        .collect()
}

/// Parse a data_ranges attribute such as `[[12, 28]]` or `[[0, 8], [12, 28]]`.
fn parse_data_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut nums: Vec<usize> = Vec::new();
    let mut current = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            current.push(c);
        } else if !current.is_empty() {
            nums.push(current.parse().unwrap_or(0));
            current.clear();
        }
    }
    if !current.is_empty() {
        nums.push(current.parse().unwrap_or(0));
    }
    for pair in nums.chunks(2) {
        if pair.len() == 2 {
            ranges.push((pair[0], pair[1]));
        }
    }
    ranges
}

fn checksum_from_xml(tag: &XmlElem) -> ChecksumSpec {
    let data_ranges = parse_data_ranges(tag.attr("data_ranges").unwrap_or(""));
    let mut polynomial = "1".to_string();
    let mut start_value = String::new();
    let mut final_xor = String::new();
    if let Some(crc) = tag.find("crc") {
        polynomial = crc.attr("polynomial").unwrap_or("1").to_string();
        start_value = crc.attr("start_value").unwrap_or("").to_string();
        final_xor = crc.attr("final_xor").unwrap_or("").to_string();
    }
    let poly_bits = parse_bit_str(&polynomial);
    let n = poly_bits.len().saturating_sub(1);
    let mut crc = GenericCRC::from_polynomial_bits(poly_bits);
    crc.start_value = parse_bit_str(&start_value);
    crc.final_xor = parse_bit_str(&final_xor);
    while crc.start_value.len() < n {
        crc.start_value.push(0);
    }
    while crc.final_xor.len() < n {
        crc.final_xor.push(0);
    }
    ChecksumSpec { data_ranges, crc }
}

impl SimulatorConfiguration {
    /// `load_from_xml(xml_tag, message_types)`. Replaces the current item
    /// tree, participants, modulators and decodings.
    pub fn load_from_xml(&mut self, xml: &str) -> Result<(), String> {
        let dom = parse_xml_dom(xml)?;
        if dom.name != "simulator_config" {
            return Err(format!("expected <simulator_config>, got <{}>", dom.name));
        }

        if let Some(modulators) = dom.find("modulators") {
            self.modulators.clear();
            for m in modulators.children_named("modulator") {
                self.modulators.push(SimModulator {
                    name: m.attr("name").unwrap_or("Modulator").to_string(),
                    modulation_type: parse_i64(m.attr("modulation_type"), 0) as usize,
                    samples_per_symbol: parse_i64(m.attr("samples_per_bit"), 100) as u32,
                    carrier_freq_hz: parse_f64(m.attr("carrier_freq_hz"), 40_000.0) as f32,
                    carrier_amplitude: parse_f64(m.attr("carrier_amplitude"), 1.0) as f32,
                    carrier_phase_deg: parse_f64(m.attr("carrier_phase_deg"), 0.0) as f32,
                    param_for_zero: parse_f64(m.attr("param_for_zero"), 0.0) as f32,
                    param_for_one: parse_f64(m.attr("param_for_one"), 100.0) as f32,
                    sample_rate: m
                        .attr("sample_rate")
                        .and_then(|s| s.parse::<f32>().ok()),
                    bits_per_symbol: 1,
                    gauss_bt: parse_f64(m.attr("gauss_bt"), 0.5) as f32,
                    gauss_filter_width: parse_f64(m.attr("gauss_filter_width"), 1.0) as f32,
                });
            }
            if self.modulators.is_empty() {
                self.modulators.push(SimModulator::default());
            }
        }

        if let Some(participants) = dom.find("participants") {
            self.participants.clear();
            for p in participants.children_named("participant") {
                self.participants.push(SimParticipant {
                    name: p.attr("name").unwrap_or("").to_string(),
                    shortname: p.attr("shortname").unwrap_or("").to_string(),
                    address_hex: p.attr("address_hex").unwrap_or("").to_string(),
                    color_index: parse_i64(p.attr("color_index"), 0) as usize,
                    relative_rssi: parse_i64(p.attr("relative_rssi"), 0),
                    id: p.attr("id").unwrap_or("").to_string(),
                    simulate: parse_i64(p.attr("simulate"), 0),
                });
            }
        }

        if let Some(decodings) = dom.find("decodings") {
            self.decodings = decodings
                .children_named("decoding")
                .map(|d| d.text.clone())
                .collect();
        }

        // Reset the item tree to a bare root.
        self.nodes = vec![SimNode::root()];
        self.root = 0;

        let items = dom
            .find("items")
            .ok_or_else(|| "missing <items> element".to_string())?;
        for child in &items.children {
            if let Some(id) = self.load_item_from_xml(child, Some(self.root))? {
                let _ = id;
            }
        }

        self.update_item_dict();
        Ok(())
    }

    fn load_item_from_xml(
        &mut self,
        tag: &XmlElem,
        parent: Option<NodeId>,
    ) -> Result<Option<NodeId>, String> {
        let kind = match tag.name.as_str() {
            "simulator_message" => {
                let msg_tag = tag
                    .find("message")
                    .ok_or_else(|| "simulator_message without <message> child".to_string())?;
                let mut msg = SimMessage::new(
                    parse_bit_str(msg_tag.attr("bits").unwrap_or("")),
                    parse_i64(msg_tag.attr("pause"), 0) as u64,
                );
                msg.modulator_index = parse_i64(msg_tag.attr("modulator_index"), 0) as usize;
                msg.decoding_index = parse_i64(msg_tag.attr("decoding_index"), 0) as usize;
                if let Some(mt) = msg_tag.find("message_type") {
                    msg.message_type_name = mt.attr("name").unwrap_or("default").to_string();
                }
                msg.source = self.find_participant(msg_tag.attr("participant_id"));
                msg.destination = self.find_participant(tag.attr("destination_id"));
                msg.repeat = parse_i64(tag.attr("repeat"), 1).max(1) as usize;
                NodeKind::Message(msg)
            }
            "simulator_label" => {
                // Python: tag.find("label"), else ChecksumLabel.from_xml(
                // tag.find("checksum_label")) — both carry name/start/end.
                let cs_tag = tag.find("checksum_label");
                let label_tag = tag
                    .find("label")
                    .or(cs_tag)
                    .ok_or_else(|| "simulator_label without <label> child".to_string())?;
                let mut lbl = SimLabel {
                    name: label_tag.attr("name").unwrap_or("").to_string(),
                    start: parse_i64(label_tag.attr("start"), 0) as usize,
                    end: parse_i64(label_tag.attr("end"), 0) as usize,
                    color_index: parse_i64(label_tag.attr("color_index"), 0) as usize,
                    display_format_index: parse_i64(
                        label_tag.attr("display_format_index"),
                        0,
                    ) as usize,
                    value_type_index: parse_i64(tag.attr("value_type_index"), 0) as usize,
                    formula: tag.attr("formula").unwrap_or("").to_string(),
                    external_program: tag.attr("external_program").unwrap_or("").to_string(),
                    random_min: parse_i64(tag.attr("random_min"), 0),
                    random_max: parse_i64(tag.attr("random_max"), 0),
                    checksum: None,
                };
                // Python: random_max defaults to fuzz_maximum - 1 when absent.
                if tag.attr("random_max").is_none() {
                    lbl.random_max = lbl.default_random_max();
                }
                if let Some(cs_tag) = tag.find("checksum_label") {
                    lbl.checksum = Some(checksum_from_xml(cs_tag));
                }
                NodeKind::Label(lbl)
            }
            "simulator_trigger_command_action" => NodeKind::TriggerCommand {
                command: tag.attr("command").map(|s| s.to_string()),
                pass_transcript: parse_i64(tag.attr("pass_transcript"), 0) != 0,
                return_code: 0,
            },
            "simulator_sleep_action" => NodeKind::Sleep {
                sleep_time: parse_f64(tag.attr("sleep_time"), 1.0),
            },
            "simulator_counter_action" => {
                let start = parse_i64(tag.attr("start"), 1);
                NodeKind::Counter {
                    start,
                    step: parse_i64(tag.attr("step"), 1),
                    value: start,
                }
            }
            "simulator_rule" => NodeKind::Rule,
            "simulator_rule_condition" => NodeKind::RuleCondition {
                cond_type: ConditionType::from_str(tag.attr("type").unwrap_or("IF"))
                    .ok_or_else(|| "invalid rule condition type".to_string())?,
                condition: tag.attr("condition").unwrap_or("").to_string(),
            },
            "simulator_goto_action" => NodeKind::Goto {
                goto_target: tag.attr("goto_target").map(|s| s.to_string()),
            },
            // Skipped like the Python loader (message/label/checksum_label
            // inside <message>).
            "message" | "label" | "checksum_label" => return Ok(None),
            other => return Err(format!("Unknown simulator item tag: {other}")),
        };

        let id = self.add_item(kind, parent);
        for child in &tag.children {
            match child.name.as_str() {
                "message" | "label" | "checksum_label" | "message_type" | "crc" | "ruleset" => {
                    continue
                }
                _ => {}
            }
            self.load_item_from_xml(child, Some(id))?;
        }
        Ok(Some(id))
    }

    fn find_participant(&self, id: Option<&str>) -> Option<usize> {
        let id = id?;
        if id.is_empty() {
            return None;
        }
        self.participants.iter().position(|p| p.id == id)
    }

    /// `save_to_xml()`: returns the XML document as a string.
    pub fn save_to_xml(&self) -> Result<String, String> {
        let mut root = XmlElem::new("simulator_config");

        // modulators
        let mut modulators = XmlElem::new("modulators");
        for (i, m) in self.modulators.iter().enumerate() {
            let mut e = XmlElem::new("modulator");
            e.set_attr("name", &m.name);
            e.set_attr("modulation_type", &m.modulation_type.to_string());
            e.set_attr("samples_per_bit", &m.samples_per_symbol.to_string());
            e.set_attr("carrier_freq_hz", &format!("{}", m.carrier_freq_hz));
            e.set_attr("carrier_amplitude", &format!("{}", m.carrier_amplitude));
            e.set_attr("carrier_phase_deg", &format!("{}", m.carrier_phase_deg));
            e.set_attr("param_for_zero", &format!("{}", m.param_for_zero));
            e.set_attr("param_for_one", &format!("{}", m.param_for_one));
            e.set_attr(
                "sample_rate",
                &m.sample_rate.map(|s| format!("{s}")).unwrap_or_else(|| "None".to_string()),
            );
            e.set_attr("gauss_bt", &format!("{}", m.gauss_bt));
            e.set_attr("gauss_filter_width", &format!("{}", m.gauss_filter_width));
            e.set_attr("index", &i.to_string());
            modulators.children.push(e);
        }
        root.children.push(modulators);

        // decodings
        let mut decodings = XmlElem::new("decodings");
        for d in &self.decodings {
            let mut e = XmlElem::new("decoding");
            e.text = d.clone();
            decodings.children.push(e);
        }
        root.children.push(decodings);

        // participants
        let mut participants = XmlElem::new("participants");
        for p in &self.participants {
            let mut e = XmlElem::new("participant");
            e.set_attr("name", &p.name);
            e.set_attr("shortname", &p.shortname);
            e.set_attr("address_hex", &p.address_hex);
            e.set_attr("color_index", &p.color_index.to_string());
            e.set_attr("id", &p.id);
            e.set_attr("relative_rssi", &p.relative_rssi.to_string());
            e.set_attr("simulate", &p.simulate.to_string());
            participants.children.push(e);
        }
        root.children.push(participants);

        // items
        let mut items = XmlElem::new("items");
        for child in self.nodes[self.root].children.clone() {
            self.save_item_to_xml(&mut items, child);
        }
        root.children.push(items);

        Ok(write_xml_dom(&root))
    }

    fn save_item_to_xml(&self, parent_tag: &mut XmlElem, id: NodeId) {
        match &self.nodes[id].kind {
            NodeKind::Message(msg) => {
                let mut e = XmlElem::new("simulator_message");
                e.set_attr(
                    "destination_id",
                    &self.participant(msg.destination).map(|p| p.id.clone()).unwrap_or_default(),
                );
                e.set_attr("repeat", &msg.repeat.to_string());

                let mut m = XmlElem::new("message");
                m.set_attr("bits", &bits_to_str(&msg.plain_bits));
                m.set_attr("decoding_index", &msg.decoding_index.to_string());
                m.set_attr("message_type_id", &message_type_id(&msg.message_type_name));
                m.set_attr("modulator_index", &msg.modulator_index.to_string());
                m.set_attr(
                    "participant_id",
                    &self.participant(msg.source).map(|p| p.id.clone()).unwrap_or_default(),
                );
                m.set_attr("pause", &msg.pause.to_string());
                m.set_attr("timestamp", "0");

                let mut mt = XmlElem::new("message_type");
                mt.set_attr("id", &message_type_id(&msg.message_type_name));
                mt.set_attr("name", &msg.message_type_name);
                for lbl_child in &self.nodes[id].children {
                    if let NodeKind::Label(lbl) = &self.nodes[*lbl_child].kind {
                        mt.children.push(self.label_to_xml(lbl));
                    }
                }
                m.children.push(mt);
                e.children.push(m);

                // Direct label children are appended by the generic child
                // recursion below, mirroring the Python save layout.
                parent_tag.children.push(e);
            }
            NodeKind::Label(lbl) => {
                parent_tag.children.push(self.label_to_xml(lbl));
            }
            NodeKind::Rule => {
                parent_tag.children.push(XmlElem::new("simulator_rule"));
            }
            NodeKind::RuleCondition { cond_type, condition } => {
                let mut e = XmlElem::new("simulator_rule_condition");
                e.set_attr("type", cond_type.as_str());
                e.set_attr("condition", condition);
                parent_tag.children.push(e);
            }
            NodeKind::Goto { goto_target } => {
                let mut e = XmlElem::new("simulator_goto_action");
                if let Some(t) = goto_target {
                    e.set_attr("goto_target", t);
                }
                parent_tag.children.push(e);
            }
            NodeKind::Sleep { sleep_time } => {
                let mut e = XmlElem::new("simulator_sleep_action");
                e.set_attr("sleep_time", &format!("{sleep_time}"));
                parent_tag.children.push(e);
            }
            NodeKind::Counter { start, step, .. } => {
                let mut e = XmlElem::new("simulator_counter_action");
                e.set_attr("start", &start.to_string());
                e.set_attr("step", &step.to_string());
                parent_tag.children.push(e);
            }
            NodeKind::TriggerCommand {
                command,
                pass_transcript,
                ..
            } => {
                let mut e = XmlElem::new("simulator_trigger_command_action");
                if let Some(cmd) = command {
                    if !cmd.is_empty() {
                        e.set_attr("command", cmd);
                    }
                }
                e.set_attr("pass_transcript", &(*pass_transcript as i64).to_string());
                parent_tag.children.push(e);
            }
            NodeKind::Root => {}
        }

        let children = self.nodes[id].children.clone();
        if !children.is_empty() {
            let last = parent_tag.children.last_mut().unwrap();
            for child in children {
                self.save_item_to_xml(last, child);
            }
        }
    }

    fn label_to_xml(&self, lbl: &SimLabel) -> XmlElem {
        let mut e = XmlElem::new("simulator_label");
        e.set_attr("value_type_index", &lbl.value_type_index.to_string());
        e.set_attr("external_program", &lbl.external_program);
        e.set_attr("formula", &lbl.formula);
        e.set_attr("random_min", &lbl.random_min.to_string());
        e.set_attr("random_max", &lbl.random_max.to_string());
        if let Some(cs) = &lbl.checksum {
            let mut c = XmlElem::new("checksum_label");
            c.set_attr("name", &lbl.name);
            c.set_attr("start", &lbl.start.to_string());
            c.set_attr("end", &lbl.end.to_string());
            c.set_attr("color_index", &lbl.color_index.to_string());
            c.set_attr("display_format_index", &lbl.display_format_index.to_string());
            c.set_attr("category", "generic");
            c.set_attr(
                "data_ranges",
                &format!(
                    "[{}]",
                    cs.data_ranges
                        .iter()
                        .map(|(s, e)| format!("[{s}, {e}]"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            let mut crc = XmlElem::new("crc");
            crc.set_attr("polynomial", &bits_to_str(&cs.crc.polynomial));
            crc.set_attr("start_value", &bits_to_str(&cs.crc.start_value));
            crc.set_attr("final_xor", &bits_to_str(&cs.crc.final_xor));
            c.children.push(crc);
            e.children.push(c);
        } else {
            let mut l = XmlElem::new("label");
            l.set_attr("name", &lbl.name);
            l.set_attr("start", &lbl.start.to_string());
            l.set_attr("end", &lbl.end.to_string());
            l.set_attr("color_index", &lbl.color_index.to_string());
            l.set_attr("display_format_index", &lbl.display_format_index.to_string());
            l.set_attr("apply_decoding", "True");
            l.set_attr("auto_created", "False");
            e.children.push(l);
        }
        e
    }
}

impl SimLabel {
    fn default_random_max(&self) -> i64 {
        let len = self.length();
        if len >= 63 {
            i64::MAX
        } else {
            (1i64 << len) - 1
        }
    }
}

fn bits_to_str(bits: &[Bit]) -> String {
    bits.iter().map(|b| char::from(b'0' + b)).collect()
}

fn message_type_id(name: &str) -> String {
    // stable pseudo-id (the loader does not use it)
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:032x}")
}

// re-export used by engine docs
pub use crate::simulator::item::ChecksumSpec;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(s: &str) -> Vec<Bit> {
        s.bytes().map(|b| b - b'0').collect()
    }

    #[test]
    fn test_tree_navigation() {
        let mut c = SimulatorConfiguration::new();
        let m1 = c.add_item(NodeKind::Message(SimMessage::new(bits("1010"), 0)), None);
        let rule = c.add_item(NodeKind::Rule, None);
        let cond = c.add_item(
            NodeKind::RuleCondition {
                cond_type: ConditionType::If,
                condition: "item1.data == 1".to_string(),
            },
            Some(rule),
        );
        let m2 = c.add_item(
            NodeKind::Message(SimMessage::new(bits("0101"), 0)),
            Some(cond),
        );
        let m3 = c.add_item(NodeKind::Message(SimMessage::new(bits("1111"), 0)), None);

        assert_eq!(c.index_of(m1), "1");
        assert_eq!(c.index_of(rule), "2");
        assert_eq!(c.index_of(cond), "2.1");
        assert_eq!(c.index_of(m2), "2.1.1");
        assert_eq!(c.index_of(m3), "3");

        // next(): depth-first
        assert_eq!(c.next(c.root), Some(m1));
        assert_eq!(c.next(m1), Some(rule));
        assert_eq!(c.next(rule), Some(cond));
        assert_eq!(c.next(cond), Some(m2));
        assert_eq!(c.next(m2), Some(m3));
        assert_eq!(c.next(m3), None);

        assert_eq!(c.next_sibling(m1), Some(rule));
        assert_eq!(c.next_sibling(m3), None);
        assert_eq!(c.prev(m3), Some(m2));
    }

    #[test]
    fn test_item_dict_keys() {
        let mut c = SimulatorConfiguration::new();
        let m1 = c.add_item(NodeKind::Message(SimMessage::new(bits("101010101001"), 0)), None);
        c.add_item(
            NodeKind::Label(SimLabel::new("sequence number", 8, 12)),
            Some(m1),
        );
        c.add_item(NodeKind::Label(SimLabel::new("data", 12, 20)), Some(m1));
        let counter = c.add_item(
            NodeKind::Counter {
                start: 3,
                step: 2,
                value: 3,
            },
            None,
        );
        let trigger = c.add_item(
            NodeKind::TriggerCommand {
                command: Some("touch /tmp/x".to_string()),
                pass_transcript: false,
                return_code: 0,
            },
            None,
        );

        assert_eq!(c.lookup_item("item1.sequence_number"), c.labels_of(m1).first().copied());
        assert_eq!(c.lookup_item("item1.data"), c.labels_of(m1).get(1).copied());
        assert_eq!(c.lookup_item("item2.counter_value"), Some(counter));
        assert_eq!(c.lookup_item("item3"), Some(trigger));
        assert_eq!(c.lookup_item("item3.rc"), Some(trigger));

        let ids = c.get_identifiers();
        assert!(ids.contains(&"item1.sequence_number".to_string()));
        assert!(ids.contains(&"item2.counter_value".to_string()));
        assert!(ids.contains(&"item3.rc".to_string()));
        // the bare trigger item key is not a valid expression identifier
        assert!(!ids.contains(&"item3".to_string()));
    }

    #[test]
    fn test_goto_targets() {
        let mut c = SimulatorConfiguration::new();
        let m1 = c.add_item(NodeKind::Message(SimMessage::new(bits("1010"), 0)), None);
        let rule = c.add_item(NodeKind::Rule, None);
        c.add_item(
            NodeKind::RuleCondition {
                cond_type: ConditionType::If,
                condition: String::new(),
            },
            Some(rule),
        );
        c.add_item(
            NodeKind::RuleCondition {
                cond_type: ConditionType::Else,
                condition: String::new(),
            },
            Some(rule),
        );
        let counter = c.add_item(
            NodeKind::Counter {
                start: 1,
                step: 1,
                value: 1,
            },
            None,
        );
        let goto = c.add_item(NodeKind::Goto { goto_target: None }, None);

        // valid targets: messages and rule-IF conditions, not the rule itself,
        // not ELSE conditions, not counters
        c.node_mut(goto).kind = NodeKind::Goto {
            goto_target: Some("item1".to_string()),
        };
        assert_eq!(c.goto_target(goto), Some(m1));

        c.node_mut(goto).kind = NodeKind::Goto {
            goto_target: Some("item2_1".to_string()),
        };
        assert!(c.goto_target(goto).is_some());

        c.node_mut(goto).kind = NodeKind::Goto {
            goto_target: Some("item2_2".to_string()),
        };
        assert!(c.goto_target(goto).is_none());

        c.node_mut(goto).kind = NodeKind::Goto {
            goto_target: Some("item2".to_string()),
        };
        assert!(c.goto_target(goto).is_none());

        c.node_mut(goto).kind = NodeKind::Goto {
            goto_target: Some("item3.counter_value".to_string()),
        };
        assert_eq!(c.lookup_item("item3.counter_value"), Some(counter));
        assert!(c.goto_target(goto).is_none());

        let targets = c.get_valid_goto_targets(goto);
        assert!(targets.contains(&"item1".to_string()));
        assert!(targets.contains(&"item2_1".to_string()));
        assert!(!targets.contains(&"item3.counter_value".to_string()));
    }

    #[test]
    fn test_consolidate_messages() {
        let mut c = SimulatorConfiguration::new();
        let m1 = c.add_item(NodeKind::Message(SimMessage::new(bits("1010"), 0)), None);
        c.add_item(NodeKind::Message(SimMessage::new(bits("1010"), 0)), None);
        c.add_item(NodeKind::Message(SimMessage::new(bits("1010"), 0)), None);
        let m4 = c.add_item(NodeKind::Message(SimMessage::new(bits("0101"), 0)), None);

        c.consolidate_messages();

        let messages = c.get_all_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(c.message(m1).unwrap().repeat, 3);
        assert_eq!(c.message(m4).unwrap().repeat, 1);
    }

    #[test]
    fn test_xml_round_trip() {
        let mut c = SimulatorConfiguration::new();
        let m1 = c.add_item(NodeKind::Message(SimMessage::new(bits("10101010100100000000"), 42)), None);
        {
            let mut lbl = SimLabel::new("sequence number", 12, 20);
            lbl.value_type_index = 2;
            lbl.formula = "item1.sequence_number + 1".to_string();
            c.add_item(NodeKind::Label(lbl), Some(m1));
        }
        {
            let mut lbl = SimLabel::new("checksum", 20, 24);
            let mut crc = GenericCRC::from_polynomial_bits(vec![1, 1, 1, 1, 0]);
            crc.start_value = vec![0, 0, 0, 0];
            crc.final_xor = vec![0, 0, 0, 0];
            lbl.checksum = Some(ChecksumSpec {
                data_ranges: vec![(12, 20)],
                crc,
            });
            c.add_item(NodeKind::Label(lbl), Some(m1));
        }
        c.add_item(
            NodeKind::Counter {
                start: 3,
                step: 2,
                value: 3,
            },
            None,
        );
        c.add_item(NodeKind::Sleep { sleep_time: 0.5 }, None);
        c.participants = vec![
            SimParticipant {
                name: "Alice".to_string(),
                shortname: "A".to_string(),
                address_hex: String::new(),
                color_index: 0,
                relative_rssi: 0,
                id: "alice-id".to_string(),
                simulate: 0,
            },
            SimParticipant {
                name: "Bob".to_string(),
                shortname: "B".to_string(),
                address_hex: String::new(),
                color_index: 1,
                relative_rssi: 1,
                id: "bob-id".to_string(),
                simulate: 2,
            },
        ];
        c.message_mut(m1).unwrap().source = Some(0);
        c.message_mut(m1).unwrap().destination = Some(1);

        let xml = c.save_to_xml().unwrap();
        let mut c2 = SimulatorConfiguration::new();
        c2.load_from_xml(&xml).unwrap();

        assert_eq!(c2.participants, c.participants);
        assert_eq!(c2.get_all_messages().len(), 1);
        let m2 = c2.get_all_messages()[0];
        let msg2 = c2.message(m2).unwrap();
        assert_eq!(msg2.plain_bits, bits("10101010100100000000"));
        assert_eq!(msg2.pause, 42);
        assert_eq!(msg2.source, Some(0));
        assert_eq!(msg2.destination, Some(1));

        let labels = c2.labels_of(m2);
        assert_eq!(labels.len(), 2);
        match &c2.node(labels[0]).kind {
            NodeKind::Label(lbl) => {
                assert_eq!(lbl.name, "sequence number");
                assert_eq!(lbl.value_type_index, 2);
                assert_eq!(lbl.formula, "item1.sequence_number + 1");
            }
            _ => panic!("expected label"),
        }
        match &c2.node(labels[1]).kind {
            NodeKind::Label(lbl) => {
                let cs = lbl.checksum.as_ref().unwrap();
                assert_eq!(cs.data_ranges, vec![(12, 20)]);
                assert_eq!(cs.crc.polynomial, vec![1, 1, 1, 1, 0]);
            }
            _ => panic!("expected label"),
        }

        // counter and sleep survive
        let kinds: Vec<String> = c2
            .get_all_items()
            .iter()
            .map(|id| {
                match &c2.node(*id).kind {
                    NodeKind::Counter { start, step, value } => {
                        format!("counter {start} {step} {value}")
                    }
                    NodeKind::Sleep { sleep_time } => format!("sleep {sleep_time}"),
                    _ => "other".to_string(),
                }
            })
            .collect();
        assert!(kinds.contains(&"counter 3 2 3".to_string()));
        assert!(kinds.contains(&"sleep 0.5".to_string()));

        // round-trip of the round-trip is stable
        let xml2 = c2.save_to_xml().unwrap();
        let mut c3 = SimulatorConfiguration::new();
        c3.load_from_xml(&xml2).unwrap();
        assert_eq!(c3.get_all_items().len(), c2.get_all_items().len());
    }
}
