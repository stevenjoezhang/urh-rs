//! Protocol XML (`.proto.xml`) load/save — port of the `to_xml`/`from_xml`
//! layer of `ProtocolAnalyzer` / `Message` / `MessageType` / `ProtocoLabel` /
//! `Participant` (see `urh/signalprocessing/*.py`).
//!
//! The XML tag and attribute names (`protocol`, `messages`, `message`,
//! `message_types`, `message_type`, `label`, `participants`, `participant`,
//! `decodings`, `decoding`, `ruleset`) and their semantics (label `end` is
//! *exclusive* on disk, inclusive in the Python constructor) match the
//! original, so files written by URH itself are readable and vice versa.
//!
//! DTO mapping vs. Python (see module report):
//! - the crate's `Message` has no participant and references its message type
//!   only by name, so `ProtocolData` keeps `participant_id` /
//!   `message_type_id` per message and resolves them on conversion;
//! - label display/fuzz attributes the crate's `ProtocolLabel` does not model
//!   are preserved verbatim in `XmlProtocolLabel` so a load/save round trip
//!   is lossless.

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::awre::auto_assigner::Participant;
use crate::protocol::{Message, MessageType, ProtocolLabel};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtoXmlError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("XML error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("invalid XML structure: {0}")]
    Structure(String),
}

/// A protocol label as stored in XML. `end` is exclusive (Python
/// `ProtocoLabel.to_xml` writes `end + 1` of its inclusive end).
#[derive(Debug, Clone, PartialEq)]
pub struct XmlProtocolLabel {
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub color_index: usize,
    pub apply_decoding: bool,
    pub show: bool,
    pub display_format_index: usize,
    pub display_bit_order_index: usize,
    pub display_endianness: String,
    pub fuzz_me: bool,
    pub fuzz_values: Vec<String>,
    pub auto_created: bool,
}

impl XmlProtocolLabel {
    fn from_protocol_label(lbl: &ProtocolLabel) -> Self {
        XmlProtocolLabel {
            name: lbl.name.clone(),
            start: lbl.start,
            end: lbl.end,
            color_index: lbl.color_index,
            apply_decoding: lbl.apply_decoding,
            show: true,
            display_format_index: 0,
            display_bit_order_index: 0,
            display_endianness: "big".to_string(),
            fuzz_me: false,
            fuzz_values: Vec::new(),
            auto_created: lbl.auto_created,
        }
    }

    fn to_protocol_label(&self) -> ProtocolLabel {
        let mut lbl = ProtocolLabel::new(&self.name, self.start, self.end.saturating_sub(1), self.color_index);
        lbl.apply_decoding = self.apply_decoding;
        lbl.auto_created = self.auto_created;
        lbl
    }
}

/// A message type as stored in XML.
#[derive(Debug, Clone, PartialEq)]
pub struct XmlMessageType {
    pub name: String,
    pub id: String,
    pub assigned_by_ruleset: bool,
    pub assigned_by_logic_analyzer: bool,
    pub labels: Vec<XmlProtocolLabel>,
}

/// A participant as stored in XML.
#[derive(Debug, Clone, PartialEq)]
pub struct XmlParticipant {
    pub name: String,
    pub shortname: String,
    pub address_hex: String,
    pub color_index: usize,
    pub id: String,
    pub relative_rssi: i32,
    pub simulate: bool,
}

/// A message as stored in XML (`bits` is the plain bit string).
#[derive(Debug, Clone, PartialEq)]
pub struct XmlMessage {
    pub bits: String,
    pub pause: u64,
    pub timestamp: Option<f64>,
    pub modulator_index: usize,
    pub message_type_id: Option<String>,
    pub participant_id: Option<String>,
    pub decoding_index: Option<usize>,
}

/// The full content of a `.proto.xml` file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProtocolData {
    pub decodings: Vec<String>,
    pub participants: Vec<XmlParticipant>,
    pub messages: Vec<XmlMessage>,
    pub message_types: Vec<XmlMessageType>,
}

impl ProtocolData {
    /// Convert to the crate's protocol DTOs. Messages get the *name* of the
    /// message type matched by id (falling back to `"Default"`), like
    /// `Message.from_xml` resolving `message_type_id` against the loaded
    /// message types. Labels of the message type are not copied onto the
    /// messages (they live on the type in the crate's model too).
    pub fn to_messages(&self) -> Vec<Message> {
        self.messages
            .iter()
            .map(|m| {
                let mut msg = Message::from_plain_bits_str(&m.bits, m.pause);
                msg.rssi = 0.0;
                if let Some(id) = &m.message_type_id {
                    if let Some(mt) = self.message_types.iter().find(|mt| &mt.id == id) {
                        msg.message_type = mt.name.clone();
                    }
                }
                msg
            })
            .collect()
    }

    /// Participant index per message (-1 = none), matching `participant_id`
    /// against the participants list like `Participant.find_matching`.
    pub fn participant_indices(&self) -> Vec<isize> {
        self.messages
            .iter()
            .map(|m| {
                m.participant_id.as_deref().map_or(-1, |id| {
                    self.participants
                        .iter()
                        .position(|p| p.id == id)
                        .map_or(-1, |i| i as isize)
                })
            })
            .collect()
    }

    /// Convert to the AWRE participant DTO.
    pub fn to_participants(&self) -> Vec<Participant> {
        self.participants
            .iter()
            .map(|p| Participant {
                name: p.name.clone(),
                shortname: p.shortname.clone(),
                address_hex: p.address_hex.clone(),
                relative_rssi: p.relative_rssi as f32,
            })
            .collect()
    }

    /// Convert to the crate's `MessageType` DTOs (labels inclusive-ended).
    pub fn to_message_types(&self) -> Vec<MessageType> {
        self.message_types
            .iter()
            .map(|mt| MessageType {
                name: mt.name.clone(),
                id: mt.id.clone(),
                labels: mt.labels.iter().map(|l| l.to_protocol_label()).collect(),
                assigned_by_ruleset: mt.assigned_by_ruleset,
            })
            .collect()
    }

    /// Build from the crate's DTOs, for saving. `participant_indices[i]`
    /// indexes into `participants` (-1 = no participant), standing in for
    /// Python's `msg.participant` back-reference.
    pub fn from_parts(
        messages: &[Message],
        message_types: &[MessageType],
        participants: &[Participant],
        participant_indices: &[isize],
    ) -> Self {
        let xml_participants: Vec<XmlParticipant> = participants
            .iter()
            .enumerate()
            .map(|(i, p)| XmlParticipant {
                name: p.name.clone(),
                shortname: p.shortname.clone(),
                address_hex: p.address_hex.clone(),
                color_index: i,
                id: format!("participant-{i}"),
                relative_rssi: p.relative_rssi as i32,
                simulate: false,
            })
            .collect();
        let xml_message_types: Vec<XmlMessageType> = message_types
            .iter()
            .map(|mt| XmlMessageType {
                name: mt.name.clone(),
                id: mt.id.clone(),
                assigned_by_ruleset: mt.assigned_by_ruleset,
                assigned_by_logic_analyzer: false,
                labels: mt.labels.iter().map(XmlProtocolLabel::from_protocol_label).collect(),
            })
            .collect();
        let xml_messages: Vec<XmlMessage> = messages
            .iter()
            .zip(participant_indices.iter().chain(std::iter::repeat(&-1)))
            .map(|(m, &pi)| {
                let type_id = message_types
                    .iter()
                    .find(|mt| mt.name == m.message_type)
                    .map(|mt| mt.id.clone());
                XmlMessage {
                    bits: m.plain_bits_str(),
                    pause: m.pause,
                    timestamp: Some(0.0),
                    modulator_index: 0,
                    message_type_id: type_id,
                    participant_id: if pi >= 0 {
                        xml_participants.get(pi as usize).map(|p| p.id.clone())
                    } else {
                        None
                    },
                    decoding_index: Some(0),
                }
            })
            .collect();
        ProtocolData {
            decodings: vec!["Non Return To Zero (NRZ), ".to_string()],
            participants: xml_participants,
            messages: xml_messages,
            message_types: xml_message_types,
        }
    }
}

/// `ProtocolAnalyzer.from_xml_file(read_bits=True)`: read a `.proto.xml`
/// file into a `ProtocolData`.
pub fn load_protocol<P: AsRef<Path>>(path: P) -> Result<ProtocolData, ProtoXmlError> {
    let text = fs::read_to_string(path)?;
    parse_protocol_xml(&text)
}

/// `ProtocolAnalyzer.to_xml_file`: write a `ProtocolData` as pretty-printed
/// XML, tag-compatible with the reference writer.
pub fn save_protocol<P: AsRef<Path>>(data: &ProtocolData, path: P) -> Result<(), ProtoXmlError> {
    let mut buf = Vec::new();
    let mut w = quick_xml::Writer::new(&mut buf);
    use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};

    let indent = |w: &mut quick_xml::Writer<&mut Vec<u8>>, depth: usize| -> Result<(), ProtoXmlError> {
        for _ in 0..depth {
            w.write_event(Event::Text(BytesText::new("   ")))?;
        }
        Ok(())
    };

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    w.write_event(Event::Decl(quick_xml::events::BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    w.write_event(Event::Start(BytesStart::new("protocol")))?;

    // decodings
    indent(&mut w, 1)?;
    w.write_event(Event::Start(BytesStart::new("decodings")))?;
    for d in &data.decodings {
        indent(&mut w, 2)?;
        w.write_event(Event::Start(BytesStart::new("decoding")))?;
        w.write_event(Event::Text(BytesText::from_escaped(esc(d))))?;
        w.write_event(Event::End(BytesEnd::new("decoding")))?;
    }
    indent(&mut w, 1)?;
    w.write_event(Event::End(BytesEnd::new("decodings")))?;

    // participants
    indent(&mut w, 1)?;
    w.write_event(Event::Start(BytesStart::new("participants")))?;
    for p in &data.participants {
        indent(&mut w, 2)?;
        let mut tag = BytesStart::new("participant");
        tag.push_attribute(("name", esc(&p.name).as_str()));
        tag.push_attribute(("shortname", esc(&p.shortname).as_str()));
        tag.push_attribute(("address_hex", esc(&p.address_hex).as_str()));
        tag.push_attribute(("color_index", p.color_index.to_string().as_str()));
        tag.push_attribute(("id", p.id.as_str()));
        tag.push_attribute(("relative_rssi", p.relative_rssi.to_string().as_str()));
        tag.push_attribute(("simulate", if p.simulate { "1" } else { "0" }));
        w.write_event(Event::Empty(tag))?;
    }
    indent(&mut w, 1)?;
    w.write_event(Event::End(BytesEnd::new("participants")))?;

    // messages
    indent(&mut w, 1)?;
    w.write_event(Event::Start(BytesStart::new("messages")))?;
    for m in &data.messages {
        indent(&mut w, 2)?;
        let mut tag = BytesStart::new("message");
        if !m.bits.is_empty() {
            tag.push_attribute(("bits", m.bits.as_str()));
        }
        if let Some(id) = &m.message_type_id {
            tag.push_attribute(("message_type_id", id.as_str()));
        }
        tag.push_attribute(("modulator_index", m.modulator_index.to_string().as_str()));
        tag.push_attribute(("pause", m.pause.to_string().as_str()));
        if let Some(ts) = m.timestamp {
            tag.push_attribute(("timestamp", format!("{ts}").as_str()));
        }
        if let Some(i) = m.decoding_index {
            tag.push_attribute(("decoding_index", i.to_string().as_str()));
        }
        if let Some(id) = &m.participant_id {
            tag.push_attribute(("participant_id", id.as_str()));
        }
        w.write_event(Event::Empty(tag))?;
    }
    indent(&mut w, 1)?;
    w.write_event(Event::End(BytesEnd::new("messages")))?;

    // message types
    indent(&mut w, 1)?;
    w.write_event(Event::Start(BytesStart::new("message_types")))?;
    for mt in &data.message_types {
        indent(&mut w, 2)?;
        let mut tag = BytesStart::new("message_type");
        tag.push_attribute(("name", esc(&mt.name).as_str()));
        tag.push_attribute(("id", mt.id.as_str()));
        tag.push_attribute((
            "assigned_by_ruleset",
            if mt.assigned_by_ruleset { "1" } else { "0" },
        ));
        tag.push_attribute((
            "assigned_by_logic_analyzer",
            if mt.assigned_by_logic_analyzer { "1" } else { "0" },
        ));
        w.write_event(Event::Start(tag))?;
        for lbl in &mt.labels {
            indent(&mut w, 3)?;
            let mut tag = BytesStart::new("label");
            tag.push_attribute(("name", esc(&lbl.name).as_str()));
            tag.push_attribute(("start", lbl.start.to_string().as_str()));
            tag.push_attribute(("end", lbl.end.to_string().as_str()));
            tag.push_attribute(("color_index", lbl.color_index.to_string().as_str()));
            tag.push_attribute(("apply_decoding", if lbl.apply_decoding { "True" } else { "False" }));
            tag.push_attribute(("show", if lbl.show { "2" } else { "0" }));
            tag.push_attribute(("display_format_index", lbl.display_format_index.to_string().as_str()));
            tag.push_attribute(("display_bit_order_index", lbl.display_bit_order_index.to_string().as_str()));
            tag.push_attribute(("display_endianness", lbl.display_endianness.as_str()));
            tag.push_attribute(("fuzz_me", if lbl.fuzz_me { "2" } else { "0" }));
            tag.push_attribute(("fuzz_values", lbl.fuzz_values.join(",").as_str()));
            tag.push_attribute(("auto_created", if lbl.auto_created { "True" } else { "False" }));
            w.write_event(Event::Empty(tag))?;
        }
        let mut tag = BytesStart::new("ruleset");
        tag.push_attribute(("mode", "0"));
        indent(&mut w, 3)?;
        w.write_event(Event::Empty(tag))?;
        indent(&mut w, 2)?;
        w.write_event(Event::End(BytesEnd::new("message_type")))?;
    }
    indent(&mut w, 1)?;
    w.write_event(Event::End(BytesEnd::new("message_types")))?;

    w.write_event(Event::End(BytesEnd::new("protocol")))?;

    let mut f = fs::File::create(path)?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    // pretty print like the reference (one event per line)
    s = s.replace("<?xml version = \"1.0\" encoding = \"UTF-8\"?>", "<?xml version=\"1.0\"?>");
    s = s.replace("<?xml version=\"1.0\" encoding=\"UTF-8\"?>", "<?xml version=\"1.0\"?>");
    writeln!(f, "{}", s.trim_end())?;
    Ok(())
}

fn parse_protocol_xml(text: &str) -> Result<ProtocolData, ProtoXmlError> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut data = ProtocolData::default();
    // path of open elements, e.g. ["protocol", "messages"]
    let mut stack: Vec<String> = Vec::new();

    loop {
        let ev = match reader.read_event() {
            Ok(e) => e,
            Err(e) => return Err(e.into()),
        };
        match ev {
            Event::Start(e) => {
                let name = e.name().as_ref().to_vec();
                let name = String::from_utf8_lossy(&name).into_owned();
                let attrs = attrs_map(&e)?;
                match (stack.last().map(|s| s.as_str()), name.as_str()) {
                    (Some("protocol"), "decodings") | (Some("protocol"), "participants")
                    | (Some("protocol"), "messages") | (Some("protocol"), "message_types") => {}
                    (Some("message_types"), "message_type") => {
                        let mt = XmlMessageType {
                            name: attrs.get("name").cloned().unwrap_or_else(|| "blank".into()),
                            id: attrs.get("id").cloned().unwrap_or_default(),
                            assigned_by_ruleset: attrs.get("assigned_by_ruleset").map_or(false, |v| v == "1"),
                            assigned_by_logic_analyzer: attrs
                                .get("assigned_by_logic_analyzer")
                                .map_or(false, |v| v == "1"),
                            labels: Vec::new(),
                        };
                        data.message_types.push(mt);
                    }
                    (Some("participants"), "participant") => {
                        let p = XmlParticipant {
                            name: attrs.get("name").cloned().unwrap_or_else(|| "Empty".into()),
                            shortname: attrs.get("shortname").cloned().unwrap_or_else(|| "X".into()),
                            address_hex: attrs.get("address_hex").cloned().unwrap_or_default(),
                            color_index: attrs.get("color_index").and_then(|v| v.parse().ok()).unwrap_or(0),
                            id: attrs.get("id").cloned().unwrap_or_default(),
                            relative_rssi: attrs.get("relative_rssi").and_then(|v| v.parse().ok()).unwrap_or(0),
                            simulate: attrs.get("simulate").and_then(|v| v.parse::<i32>().ok()).map_or(false, |v| v != 0),
                        };
                        data.participants.push(p);
                    }
                    (Some("messages"), "message") => {
                        // handled by Empty below or here when non-empty tag
                        let m = xml_message_from_attrs(&attrs)?;
                        data.messages.push(m);
                    }
                    _ => {}
                }
                stack.push(name);
            }
            Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let attrs = attrs_map(&e)?;
                match (stack.last().map(|s| s.as_str()), name.as_str()) {
                    (Some("messages"), "message") => {
                        let m = xml_message_from_attrs(&attrs)?;
                        data.messages.push(m);
                    }
                    (Some("participants"), "participant") => {
                        let p = XmlParticipant {
                            name: attrs.get("name").cloned().unwrap_or_else(|| "Empty".into()),
                            shortname: attrs.get("shortname").cloned().unwrap_or_else(|| "X".into()),
                            address_hex: attrs.get("address_hex").cloned().unwrap_or_default(),
                            color_index: attrs.get("color_index").and_then(|v| v.parse().ok()).unwrap_or(0),
                            id: attrs.get("id").cloned().unwrap_or_default(),
                            relative_rssi: attrs.get("relative_rssi").and_then(|v| v.parse().ok()).unwrap_or(0),
                            simulate: attrs.get("simulate").and_then(|v| v.parse::<i32>().ok()).map_or(false, |v| v != 0),
                        };
                        data.participants.push(p);
                    }
                    (Some("message_type"), "label") => {
                        let lbl = xml_label_from_attrs(&attrs);
                        if let Some(mt) = data.message_types.last_mut() {
                            mt.labels.push(lbl);
                        }
                    }
                    (Some("message_type"), "ruleset") => {
                        // rulesets are not modeled; ignored like other
                        // crate-unmodeled attributes
                    }
                    _ => {}
                }
            }
            Event::Text(t) => {
                let text = t.unescape()?.into_owned();
                if stack.last().map(|s| s.as_str()) == Some("decoding") {
                    data.decodings.push(text);
                }
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(data)
}

fn attrs_map(e: &quick_xml::events::BytesStart) -> Result<std::collections::HashMap<String, String>, ProtoXmlError> {
    let mut map = std::collections::HashMap::new();
    for a in e.attributes() {
        let a = a.map_err(quick_xml::Error::from)?;
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let value = String::from_utf8_lossy(&a.value).into_owned();
        map.insert(key, value);
    }
    Ok(map)
}

fn xml_message_from_attrs(attrs: &std::collections::HashMap<String, String>) -> Result<XmlMessage, ProtoXmlError> {
    let bits = attrs.get("bits").cloned().unwrap_or_default();
    if bits.is_empty() {
        return Err(ProtoXmlError::Structure("message without bits".into()));
    }
    if !bits.chars().all(|c| c == '0' || c == '1') {
        return Err(ProtoXmlError::Structure(format!("invalid bits string: {bits}")));
    }
    Ok(XmlMessage {
        bits,
        pause: attrs.get("pause").and_then(|v| v.parse().ok()).unwrap_or(0),
        timestamp: attrs.get("timestamp").and_then(|v| v.parse().ok()),
        modulator_index: attrs.get("modulator_index").and_then(|v| v.parse().ok()).unwrap_or(0),
        message_type_id: attrs.get("message_type_id").cloned(),
        participant_id: attrs.get("participant_id").cloned(),
        decoding_index: attrs.get("decoding_index").and_then(|v| v.parse().ok()),
    })
}

fn xml_label_from_attrs(attrs: &std::collections::HashMap<String, String>) -> XmlProtocolLabel {
    let int = |k: &str, d: usize| attrs.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let bool_py = |k: &str, d: bool| {
        attrs
            .get(k)
            .map_or(d, |v| v == "True" || v == "1" || v == "2")
    };
    XmlProtocolLabel {
        name: attrs.get("name").cloned().unwrap_or_default(),
        start: int("start", 0),
        end: int("end", 0),
        color_index: int("color_index", 0),
        apply_decoding: bool_py("apply_decoding", true),
        show: bool_py("show", true),
        display_format_index: int("display_format_index", 0),
        display_bit_order_index: int("display_bit_order_index", 0),
        display_endianness: attrs.get("display_endianness").cloned().unwrap_or_else(|| "big".into()),
        fuzz_me: bool_py("fuzz_me", false),
        fuzz_values: attrs
            .get("fuzz_values")
            .map(|v| v.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect())
            .unwrap_or_default(),
        auto_created: bool_py("auto_created", false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA_DIR: &str = "/Users/mimi/urh-ref/tests/data/";

    fn load(name: &str) -> ProtocolData {
        load_protocol(format!("{DATA_DIR}{name}")).expect("load")
    }

    #[test]
    fn test_load_35_messages() {
        let data = load("35_messages.proto.xml");
        assert_eq!(data.messages.len(), 35);
        assert_eq!(data.participants.len(), 2);
        assert_eq!(data.participants[0].name, "Alice");
        assert_eq!(data.participants[0].address_hex, "dead");
        assert_eq!(data.participants[1].address_hex, "beef");
        assert_eq!(data.message_types.len(), 1);
        let mt = &data.message_types[0];
        assert_eq!(mt.name, "protocol_with_one_message_type");
        assert_eq!(mt.labels.len(), 6);
        // label end is exclusive on disk
        let preamble = &mt.labels[0];
        assert_eq!(preamble.name, "preamble");
        assert_eq!(preamble.start, 0);
        assert_eq!(preamble.end, 8);
        let sync = &mt.labels[1];
        assert_eq!(sync.name, "synchronization");
        assert_eq!((sync.start, sync.end), (8, 24));

        // message <-> participant / message type ids resolve
        let messages = data.to_messages();
        assert_eq!(messages.len(), 35);
        assert_eq!(messages[0].message_type, "protocol_with_one_message_type");
        assert_eq!(messages[0].plain_bits.len(), 136);
        assert_eq!(data.participant_indices()[0], 0);
        assert_eq!(data.participant_indices()[1], 1);

        let mts = data.to_message_types();
        assert_eq!(mts[0].labels[0].start, 0);
        assert_eq!(mts[0].labels[0].end, 8); // crate end is exclusive too
    }

    #[test]
    fn test_load_all_reference_files() {
        for name in [
            "35_messages.proto.xml",
            "ack_frames_with_crc.proto.xml",
            "four_broken.proto.xml",
            "four_participants.proto.xml",
            "homematic.proto.xml",
            "no_preamble24.proto.xml",
            "one_address_one_mt.proto.xml",
            "only_one_address.proto.xml",
            "protocol.proto.xml",
            "protocol_wsp.proto.xml",
            "rwe.proto.xml",
            "three_syncs.proto.xml",
            "with_checksum.proto.xml",
            "without_ack_random_data.proto.xml",
            "without_ack_random_data2.proto.xml",
        ] {
            let data = load(name);
            assert!(!data.messages.is_empty(), "{name}: no messages");
            for m in &data.messages {
                assert!(!m.bits.is_empty(), "{name}: empty bits");
            }
        }
    }

    #[test]
    fn test_roundtrip_save_load() {
        let data = load("35_messages.proto.xml");
        let path = std::env::temp_dir().join("urh_rs_proto_xml_roundtrip.proto.xml");
        save_protocol(&data, &path).expect("save");
        let reloaded = load_protocol(&path).expect("reload");

        assert_eq!(reloaded.participants, data.participants);
        assert_eq!(reloaded.messages, data.messages);
        // ruleset elements are dropped on save; compare types without them
        assert_eq!(reloaded.message_types.len(), data.message_types.len());
        for (a, b) in reloaded.message_types.iter().zip(&data.message_types) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.id, b.id);
            assert_eq!(a.labels, b.labels);
        }
        // URH's own reader must accept our writer output: re-run the AWRE
        // ground truth equivalent in tests/awre_real_protocols.rs.
    }

    #[test]
    fn test_from_parts_and_back() {
        let mut mt = MessageType::new("data");
        mt.add_protocol_label_start_length(0, 8, Some("preamble"), None);
        let participants = vec![
            Participant::with_address("Alice", "dead"),
            Participant::with_address("Bob", "beef"),
        ];
        let messages = vec![
            Message::from_plain_bits_str("10101010", 100).with_message_type("data"),
            Message::from_plain_bits_str("11110000", 0).with_message_type("data"),
        ];
        let data = ProtocolData::from_parts(&messages, &[mt], &participants, &[0, 1]);
        assert_eq!(data.participants.len(), 2);
        assert_eq!(data.messages[0].bits, "10101010");
        assert_eq!(data.messages[0].pause, 100);
        assert_eq!(data.participant_indices(), vec![0, 1]);

        let path = std::env::temp_dir().join("urh_rs_proto_xml_from_parts.proto.xml");
        save_protocol(&data, &path).expect("save");
        let reloaded = load_protocol(&path).expect("reload");
        let msgs = reloaded.to_messages();
        assert_eq!(msgs[0].plain_bits_str(), "10101010");
        assert_eq!(msgs[0].pause, 100);
        assert_eq!(msgs[0].message_type, "data");
        assert_eq!(reloaded.to_message_types()[0].labels[0].name, "preamble");
    }
}
