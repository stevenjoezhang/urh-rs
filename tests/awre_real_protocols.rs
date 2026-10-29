//! Ports of the real-protocol AWRE tests (`tests/awre/test_awre_real_protocols.py`,
//! `tests/awre/test_address_engine.py` and `tests/awre/test_generated_protocols.py`),
//! driven through `.proto.xml` files loaded with `urh_core::proto_xml`.
//!
//! Expected field positions and participant addresses were taken from the
//! Python reference (Cython extensions in ~/urh-ref), which is run with the
//! same setup as `AWRETestCase.get_format_finder_from_protocol_file`:
//! load XML -> set every message's type to a fresh "empty" type ->
//! `FormatFinder(messages)` (no participants) -> clear known addresses -> run.

use std::collections::HashMap;

use urh_core::awre::format_finder::FormatFinder;
use urh_core::awre::message_type_builder::FieldFunction;
use urh_core::proto_xml::load_protocol;
use urh_core::protocol::Message;

const DATA_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/");

/// `AWRETestCase.get_format_finder_from_protocol_file` equivalent: loads the
/// XML, clears the messages' message types (so pre-assigned labels cannot
/// interfere) and returns the FormatFinder with cleared known addresses.
/// Like the Python constructor, participants come from the messages
/// (`msg.participant`, i.e. the XML `participant_id`s), and are NOT
/// auto-assigned.
fn format_finder_from_protocol_file(name: &str) -> (FormatFinder, Vec<Message>) {
    let data = load_protocol(format!("{DATA_DIR}{name}")).expect("load proto xml");
    let messages: Vec<Message> = data
        .to_messages()
        .into_iter()
        .map(|mut m| {
            m.message_type = "empty".to_string();
            m
        })
        .collect();
    let participants = data.to_participants();
    let participant_indices = data.participant_indices();
    let mut ff = FormatFinder::with_participants(
        messages.clone(),
        Vec::new(),
        participants,
        participant_indices,
    );
    ff.known_participant_addresses.clear();
    (ff, messages)
}

/// `util.convert_numbers_to_hex_string` for the nibble vectors stored in
/// `known_participant_addresses`.
fn nibbles_to_hex(nibbles: &[u8]) -> String {
    nibbles
        .iter()
        .map(|&n| char::from_digit(n as u32, 16).unwrap_or('?'))
        .collect()
}

fn known_addresses_hex(ff: &FormatFinder) -> HashMap<usize, String> {
    ff.known_participant_addresses
        .iter()
        .map(|(k, v)| (*k, nibbles_to_hex(v)))
        .collect()
}

/// Assert (start, length) of the first label with the given function.
fn assert_field(
    mt: &urh_core::awre::message_type_builder::AnalyzedMessageType,
    function: FieldFunction,
    start: usize,
    length: usize,
) {
    let lbl = mt
        .get_first_label_with_type(function)
        .unwrap_or_else(|| panic!("no {function:?} label in {:?}", mt.message_type.name));
    assert_eq!(
        (lbl.label.start, lbl.label.length()),
        (start, length),
        "{:?} in {:?}",
        function,
        mt.message_type.name
    );
}

#[test]
fn test_35_messages() {
    // Python counterpart: tests/awre/test_address_engine.py
    // (test_address_engine_performance uses this file; the full field layout
    // below is the Python ground truth obtained by running the reference
    // FormatFinder on this file with the same cleared setup)
    let (mut ff, messages) = format_finder_from_protocol_file("35_messages.proto.xml");
    assert_eq!(messages.len(), 35);

    ff.perform_analysis();

    let mts = ff.message_types();
    assert_eq!(mts.len(), 1);
    assert_eq!(mts[0].message_type.name, "empty");
    assert_eq!(mts[0].message_indices.len(), 35);

    assert_field(mts[0], FieldFunction::Preamble, 0, 8);
    assert_field(mts[0], FieldFunction::Sync, 8, 16);
    assert_field(mts[0], FieldFunction::Length, 24, 8);
    assert_field(mts[0], FieldFunction::SrcAddress, 32, 16);
    assert_field(mts[0], FieldFunction::DstAddress, 48, 16);
    assert_field(mts[0], FieldFunction::SequenceNumber, 64, 8);

    let addresses = known_addresses_hex(&ff);
    let values: Vec<&String> = addresses.values().collect();
    assert_eq!(addresses.len(), 2, "{addresses:?}");
    assert!(values.contains(&&"dead".to_string()), "{addresses:?}");
    assert!(values.contains(&&"beef".to_string()), "{addresses:?}");
}

#[test]
fn test_ack_frames_with_crc() {
    // Python counterpart: test_address_engine.py::test_protocol_with_acks_and_checksum
    // (address + preamble/sync/length assertions) plus the checksum and
    // address positions from the reference run (see module docs).
    let (mut ff, messages) = format_finder_from_protocol_file("ack_frames_with_crc.proto.xml");
    assert_eq!(messages.len(), 20);

    ff.perform_analysis();

    let addresses = known_addresses_hex(&ff);
    assert_eq!(addresses.len(), 2, "{addresses:?}");
    let mut sorted: Vec<&String> = addresses.values().collect();
    sorted.sort();
    assert_eq!(sorted, [&"1337".to_string(), &"4711".to_string()]);

    let mts = ff.message_types();
    assert_eq!(mts.len(), 3, "expected 3 message types");

    // The ack type: short frames, DST address directly after length, no SRC.
    let ack_mt = mts
        .iter()
        .find(|mt| mt.message_indices.contains(&1))
        .expect("ack message type (containing message 1)");
    assert_eq!(
        ack_mt.message_indices,
        vec![1, 3, 5, 7, 9, 11, 13, 15, 17, 19]
    );
    assert_field(ack_mt, FieldFunction::Preamble, 0, 16);
    assert_field(ack_mt, FieldFunction::Sync, 16, 16);
    assert_field(ack_mt, FieldFunction::Length, 32, 8);
    assert_field(ack_mt, FieldFunction::DstAddress, 40, 16);
    assert!(ack_mt.get_first_label_with_type(FieldFunction::SrcAddress).is_none());
    assert_field(ack_mt, FieldFunction::Checksum, 56, 16);

    // The remaining two types share the SRC/DST layout and differ in the
    // checksum position (long frames: 584, short frames: 136).
    let data_mts: Vec<_> = mts
        .iter()
        .filter(|mt| !std::ptr::eq(*mt, ack_mt))
        .collect();
    assert_eq!(data_mts.len(), 2);
    let mut checksum_starts: Vec<usize> = Vec::new();
    for mt in &data_mts {
        assert_field(mt, FieldFunction::Preamble, 0, 16);
        assert_field(mt, FieldFunction::Sync, 16, 16);
        assert_field(mt, FieldFunction::Length, 32, 8);
        assert_field(mt, FieldFunction::SrcAddress, 40, 16);
        assert_field(mt, FieldFunction::DstAddress, 56, 16);
        let checksum = mt
            .get_first_label_with_type(FieldFunction::Checksum)
            .expect("checksum label");
        checksum_starts.push(checksum.label.start);
    }
    checksum_starts.sort_unstable();
    assert_eq!(checksum_starts, vec![136, 584]);

    // membership ground truth from the Python run
    let long_mt = data_mts
        .iter()
        .find(|mt| mt.message_indices.contains(&2))
        .unwrap();
    assert_eq!(long_mt.message_indices, vec![2, 6, 10, 14, 18]);
    let empty_mt = mts.iter().find(|mt| mt.message_type.name == "empty").unwrap();
    assert_eq!(empty_mt.message_indices, vec![0, 4, 8, 12, 16]);
}

/// Port of test_generated_protocols.py::test_without_preamble_random_data.
#[test]
fn test_without_preamble_random_data() {
    let (mut ff, _messages) = format_finder_from_protocol_file("without_ack_random_data.proto.xml");
    ff.perform_analysis();

    let mts = ff.message_types();
    assert_eq!(mts.len(), 1);
    assert_field(mts[0], FieldFunction::Sync, 0, 16);
    assert_field(mts[0], FieldFunction::Length, 16, 8);
    assert_field(mts[0], FieldFunction::SrcAddress, 24, 8);
    assert_field(mts[0], FieldFunction::SequenceNumber, 32, 8);
    // no preamble in this protocol
    assert!(mts[0].get_first_label_with_type(FieldFunction::Preamble).is_none());

    let addresses = known_addresses_hex(&ff);
    let values: Vec<&String> = addresses.values().collect();
    assert_eq!(addresses.len(), 2, "{addresses:?}");
    assert!(values.contains(&&"24".to_string()), "{addresses:?}");
    assert!(values.contains(&&"ff".to_string()), "{addresses:?}");
}

/// Port of test_generated_protocols.py::test_with_checksum (count and, from
/// the reference run, the label layout of the resulting message types).
#[test]
fn test_with_checksum() {
    let (mut ff, _messages) = format_finder_from_protocol_file("with_checksum.proto.xml");
    ff.perform_analysis();

    assert_eq!(ff.message_types().len(), 3);

    let addresses = known_addresses_hex(&ff);
    assert_eq!(addresses.len(), 2, "{addresses:?}");
    let values: Vec<&String> = addresses.values().collect();
    assert!(values.contains(&&"1337".to_string()), "{addresses:?}");
    assert!(values.contains(&&"dead".to_string()), "{addresses:?}");

    for mt in ff.message_types() {
        assert_field(mt, FieldFunction::Preamble, 0, 16);
        assert_field(mt, FieldFunction::Sync, 16, 16);
        assert_field(mt, FieldFunction::Length, 32, 8);
        // every message type gets a checksum
        assert!(mt.get_first_label_with_type(FieldFunction::Checksum).is_some());
    }
}

