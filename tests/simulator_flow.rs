//! Ground-truth comparison for the simulator flow scenario, ported from
//! `~/urh-ref/tests/test_simulator.py::TestSimulator::test_simulation_flow`.
//!
//! The Python scenario runs the full URH simulator over network SDR devices:
//! "Alice" (external) sends modulated messages to the simulator, which plays
//! "Bob" (echoing each sequence number + 1). The expected outputs below were
//! captured by running the *original Python simulator* on the same profile
//! (`testprofile.sim.xml`) with a driver script that recorded the transcript,
//! per-message send/receive bit sequences, counter values and log
//! (/tmp/sim_groundtruth.json, reproduced in the constants below).
//!
//! The Rust test feeds the same input bit sequences into the engine through
//! the [`SimulatorIO`] trait (the sniffer/demodulation stand-in) and compares:
//! the transcript (bit and hex), every message's sent/received bits, the
//! modulated sample streams (against direct `SimModulator::modulate` calls
//! with the expected response bits, standing in for the Python test's
//! demodulation of the sent samples), counter values and log messages.

use std::collections::VecDeque;
use std::fs;

use urh_core::simulator::config::SimulatorConfiguration;
use urh_core::simulator::engine::{Simulator, SimulatorIO, SimulatorSettings};
use urh_core::simulator::item::NodeKind;
use urh_core::{Bit, Complex32};

const PROFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/testprofile.sim.xml");

// --- Ground truth captured from the original Python simulator run ---

/// Input messages Alice sends (preamble + sync + seq + data + checksum),
/// exactly as computed by the Python test with the profile's CRC label.
const INPUT_BITS: [&str; 3] = [
    "10101010100100000010110011011100",
    "10101010100100000100110011011010",
    "10101010100100000110110011011000",
];

/// Bits of the simulator's responses (Bob), as demodulated in the Python
/// test run: preamble + sync + (received sequence number + 1).
const RESPONSE_BITS: [&str; 3] = [
    "10101010100100000011",
    "10101010100100000101",
    "10101010100100000111",
];

/// Transcript (bit view) from the Python run.
const TRANSCRIPT_BITS: [&str; 7] = [
    "1 (A->B): 10101010100100000010110011011100",
    "2 (B->A): 10101010100100000011",
    "3 (A->B): 10101010100100000100110011011010",
    "4 (B->A): 10101010100100000101",
    "5 (A->B): 10101010100100000110110011011000",
    "6 (B->A): 10101010100100000111",
    "",
];

/// Transcript (hex view) from the Python run.
const TRANSCRIPT_HEX: [&str; 7] = [
    "1 (A->B): aa902cdc",
    "2 (B->A): aa903",
    "3 (A->B): aa904cda",
    "4 (B->A): aa905",
    "5 (A->B): aa906cd8",
    "6 (B->A): aa907",
    "",
];

/// Per simulator message: (index, source, destination, send_recv bits).
const MESSAGES: [(&str, &str, &str, &str); 6] = [
    ("1", "A", "B", "10101010100100000010110011011100"),
    ("2", "B", "A", "10101010100100000011"),
    ("3", "A", "B", "10101010100100000100110011011010"),
    ("4", "B", "A", "10101010100100000101"),
    ("5", "A", "B", "10101010100100000110110011011000"),
    ("6", "B", "A", "10101010100100000111"),
];

/// Pauses of the six simulator messages (msg 6 carries the 1e6 sample pause).
const MESSAGE_PAUSES: [u64; 6] = [0, 0, 0, 0, 0, 1_000_000];

// --- IO mock ---

struct FlowIO {
    incoming: VecDeque<Vec<Bit>>,
    sent: Vec<Vec<Complex32>>,
}

impl SimulatorIO for FlowIO {
    fn receive(&mut self) -> Option<Vec<Bit>> {
        self.incoming.pop_front()
    }
    fn send(&mut self, samples: &[Complex32]) {
        self.sent.push(samples.to_vec());
    }
}

fn bits(s: &str) -> Vec<Bit> {
    s.bytes().map(|b| b - b'0').collect()
}

fn bits_str(bits: &[Bit]) -> String {
    bits.iter().map(|b| char::from(b'0' + b)).collect()
}

#[test]
fn test_simulation_flow_matches_python() {
    let xml = fs::read_to_string(PROFILE).unwrap();
    let mut config = SimulatorConfiguration::new();
    config.load_from_xml(&xml).unwrap();

    // The Python test asserts the scene contains 6 message items.
    assert_eq!(config.get_all_messages().len(), 6);

    // Alice (simulate=0) is external, Bob (simulate=2) is played by the
    // simulator.
    assert_eq!(config.participants.len(), 2);
    assert!(!config.participants[0].simulates());
    assert!(config.participants[1].simulates());
    assert_eq!(config.participants[0].shortname, "A");
    assert_eq!(config.participants[1].shortname, "B");

    let modulators = config.modulators.clone();
    assert_eq!(modulators.len(), 1);

    let mut io = FlowIO {
        incoming: INPUT_BITS.iter().map(|s| bits(s)).collect(),
        sent: Vec::new(),
    };

    let mut sim = Simulator::new(config, modulators);
    sim.settings = SimulatorSettings {
        num_repeats: 1,
        ..SimulatorSettings::default()
    };
    sim.run(&mut io).unwrap();

    assert!(!sim.is_simulating());

    // --- sent sample streams ---
    // The Python test demodulates the sent samples and checks the response
    // bits; here we verify the samples are exactly the modulation of the
    // expected response bits (pause included: msg 6 has 1e6 pause samples).
    assert_eq!(io.sent.len(), 3, "three responses expected");
    for (i, chunk) in io.sent.iter().enumerate() {
        let pause = MESSAGE_PAUSES[2 * i + 1] as u32;
        let expected = sim.modulators[0].modulate(&bits(RESPONSE_BITS[i]), pause);
        assert_eq!(
            chunk.len(),
            expected.len(),
            "sample count mismatch for response {i}"
        );
        for (a, b) in chunk.iter().zip(expected.iter()) {
            let d = (a - b).norm();
            assert!(d < 1e-5, "sample mismatch in response {i}: {d}");
        }
    }
    // msg 6 pause shows up in the sample count
    assert_eq!(io.sent[2].len(), 20 * 100 + 1_000_000);

    // --- transcript (bit + hex) ---
    let transcript_bits: Vec<String> = sim.transcript.get_for_all_participants(true, true);
    let transcript_hex: Vec<String> = sim.transcript.get_for_all_participants(true, false);
    assert_eq!(transcript_bits, TRANSCRIPT_BITS.to_vec());
    assert_eq!(transcript_hex, TRANSCRIPT_HEX.to_vec());

    // --- per-message send/receive records ---
    let message_ids = sim.config.get_all_messages();
    assert_eq!(message_ids.len(), 6);
    for (i, msg_id) in message_ids.iter().enumerate() {
        let (index, source, destination, expected_bits) = MESSAGES[i];
        let msg = sim.config.message(*msg_id).unwrap();
        assert_eq!(sim.config.index_of(*msg_id), index);
        assert_eq!(
            sim.config.participant(msg.source).map(|p| p.shortname.as_str()),
            Some(source)
        );
        assert_eq!(
            sim.config
                .participant(msg.destination)
                .map(|p| p.shortname.as_str()),
            Some(destination)
        );
        assert_eq!(msg.pause, MESSAGE_PAUSES[i]);
        assert_eq!(msg.send_recv.len(), 1, "message {index}");
        assert_eq!(bits_str(&msg.send_recv[0]), expected_bits);
    }

    // --- counters: the profile contains no counter actions ---
    let counters: Vec<_> = sim
        .config
        .get_all_items()
        .into_iter()
        .filter(|id| matches!(sim.config.node(*id).kind, NodeKind::Counter { .. }))
        .collect();
    assert!(counters.is_empty());

    // --- log sequence (subset of the Python log, without HTML/timestamps) ---
    let expected_log_snippets = [
        "Simulation is running",
        "Waiting for message 1...",
        "  Received 32 data bits",
        "  Check whether received data matches",
        "Received message 1: ",
        "Sending message 2",
        "Waiting for message 3...",
        "Received message 3: ",
        "Sending message 4",
        "Waiting for message 5...",
        "Received message 5: ",
        "Sending message 6",
        "Stop simulation (Finished)",
    ];
    let mut log = sim.log_messages.clone();
    for snippet in expected_log_snippets {
        let pos = log
            .iter()
            .position(|m| m == snippet)
            .unwrap_or_else(|| panic!("log message {snippet:?} missing: {log:?}"));
        log = log.split_off(pos + 1);
    }
}
