//! Integration tests for the v2 simulator service
//! (docs/ipc-contract.md, "Simulator").
//!
//! These drive the public flow over files: a hand-crafted profile
//! exercising every logic item (the `load_simulator` listing and a fully
//! deterministic `run_simulation` transcript), the engine's ground-truth
//! fixture `tests/data/testprofile.sim.xml`, and the error paths (missing
//! files, malformed XML, engine failures, the timeout watchdog).

use std::fs;

use urh_app::simulator::{SimRuleInfo, SimTranscriptEntry};
use urh_app::SignalRepository;

/// The engine's ground-truth profile (also used by `tests/simulator_flow.rs`
/// at the repo root), resolved relative to this crate's manifest.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/data/testprofile.sim.xml"
);

/// A profile exercising every logic item: two messages (one received from
/// the external participant, one sent), a counter, a rule (IF with a nested
/// send message, ELSE with a nested sleep), a goto and a trigger command.
/// Item numbering: 1 msg, 2 msg, 3 counter, 4 rule (4.1 IF, 4.2 ELSE,
/// 4.2.1 sleep), 5 goto, 6 trigger command.
const CRAFTED_PROFILE: &str = r#"<?xml version="1.0" ?>
<simulator_config>
  <modulators>
    <modulator carrier_amplitude="1" carrier_freq_hz="40000" carrier_phase_deg="0" gauss_bt="0.5" gauss_filter_width="1" index="0" modulation_type="0" name="Modulator" param_for_one="100" param_for_zero="0" sample_rate="None" samples_per_bit="100"/>
  </modulators>
  <participants>
    <participant address_hex="" color_index="0" id="alice-id" name="Alice" relative_rssi="0" shortname="A" simulate="0"/>
    <participant address_hex="" color_index="1" id="bob-id" name="Bob" relative_rssi="1" shortname="B" simulate="2"/>
  </participants>
  <items>
    <simulator_message destination_id="bob-id" repeat="1">
      <message bits="1010" decoding_index="0" message_type_id="m1" modulator_index="0" participant_id="alice-id" pause="0" timestamp="0"/>
    </simulator_message>
    <simulator_message destination_id="alice-id" repeat="1">
      <message bits="0101" decoding_index="0" message_type_id="m2" modulator_index="0" participant_id="bob-id" pause="0" timestamp="0"/>
    </simulator_message>
    <simulator_counter_action start="3" step="2"/>
    <simulator_rule>
      <simulator_rule_condition condition="item3.counter_value > 2" type="IF">
        <simulator_message destination_id="alice-id" repeat="1">
          <message bits="1111" decoding_index="0" message_type_id="m3" modulator_index="0" participant_id="bob-id" pause="0" timestamp="0"/>
        </simulator_message>
      </simulator_rule_condition>
      <simulator_rule_condition condition="" type="ELSE">
        <simulator_sleep_action sleep_time="0"/>
      </simulator_rule_condition>
    </simulator_rule>
    <simulator_goto_action goto_target="item6"/>
    <simulator_trigger_command_action command="echo hi" pass_transcript="0"/>
  </items>
</simulator_config>
"#;

/// A profile that blocks the engine for 5 seconds inside a sleep action —
/// the timeout watchdog's worst case (no IO happens, so only the worker
/// thread's deadline can end the call).
const SLEEPY_PROFILE: &str = r#"<?xml version="1.0" ?>
<simulator_config>
  <modulators>
    <modulator carrier_amplitude="1" carrier_freq_hz="40000" carrier_phase_deg="0" gauss_bt="0.5" gauss_filter_width="1" index="0" modulation_type="0" name="Modulator" param_for_one="100" param_for_zero="0" sample_rate="None" samples_per_bit="100"/>
  </modulators>
  <participants>
    <participant address_hex="" color_index="1" id="bob-id" name="Bob" relative_rssi="1" shortname="B" simulate="2"/>
  </participants>
  <items>
    <simulator_sleep_action sleep_time="5"/>
  </items>
</simulator_config>
"#;

fn write_profile(dir: &tempfile::TempDir, name: &str, xml: &str) -> String {
    let path = dir.path().join(name);
    fs::write(&path, xml).unwrap();
    path.to_str().unwrap().to_string()
}

fn as_tuples(infos: &[SimRuleInfo]) -> Vec<(usize, &str, &str, &str)> {
    infos
        .iter()
        .map(|info| {
            (
                info.index,
                info.name.as_str(),
                info.kind.as_str(),
                info.summary.as_str(),
            )
        })
        .collect()
}

/// The crafted profile lists its five logic items in document order with
/// the documented kinds and summaries.
#[test]
fn load_simulator_lists_the_crafted_profile() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = write_profile(&dir, "crafted.simulator", CRAFTED_PROFILE);

    let infos = repo.load_simulator(&path).unwrap();
    assert_eq!(
        as_tuples(&infos),
        vec![
            (0, "Counter", "counter", "counter start=3 step=2"),
            (1, "Rule", "rule", "IF item3.counter_value > 2 / ELSE"),
            (2, "Sleep", "sleep", "sleep for 0.00 ns"),
            (3, "Goto", "goto", "goto item6"),
            (
                4,
                "Trigger command",
                "trigger-command",
                "trigger command: echo hi"
            ),
        ]
    );
}

/// The ground-truth fixture contains only messages (no rules/actions), so
/// its listing is empty — the transcript is where its content shows.
#[test]
fn load_simulator_on_the_fixture_lists_no_logic_items() {
    let repo = SignalRepository::new();
    let infos = repo.load_simulator(FIXTURE).unwrap();
    assert!(infos.is_empty());
}

/// Running the crafted profile: Alice's template is replayed as the source
/// (received), Bob answers, the counter makes the rule's IF branch fire and
/// the nested message is sent. Timestamps advance monotonically.
#[test]
fn run_simulation_of_the_crafted_profile() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = write_profile(&dir, "crafted.simulator", CRAFTED_PROFILE);

    let entries = repo.run_simulation(&path, 30).unwrap();
    let projected: Vec<(&str, &str, &str, &str)> = entries
        .iter()
        .map(|e| {
            (
                e.source.as_str(),
                e.destination.as_str(),
                e.bits.as_str(),
                e.action.as_str(),
            )
        })
        .collect();
    assert_eq!(
        projected,
        vec![
            ("A", "B", "1010", "receive"),
            ("B", "A", "0101", "send"),
            ("B", "A", "1111", "send"),
        ]
    );

    assert!(entries[0].timestamp.is_finite() && entries[0].timestamp >= 0.0);
    for pair in entries.windows(2) {
        assert!(
            pair[0].timestamp < pair[1].timestamp,
            "timestamps must advance: {entries:?}"
        );
    }
}

/// The engine's ground-truth profile under the service's message-source
/// mode: the first exchange is the profile's saved receive template for
/// message 1 (replayed verbatim as Alice's input) and Bob's formula answer
/// (`item1.sequence_number+1` = 1). The run then stops gracefully: the
/// profile's saved templates are stale relative to its own formula labels
/// (message 3 expects sequence number 2 — the Python test feeds freshly
/// computed input bits, the root `simulator_flow.rs` test documents them),
/// so the replayed template fails label validation and error handling
/// "stop" ends the simulation with the partial transcript.
#[test]
fn run_simulation_of_the_ground_truth_fixture() {
    let repo = SignalRepository::new();
    let entries: Vec<SimTranscriptEntry> = repo.run_simulation(FIXTURE, 30).unwrap();
    let projected: Vec<(&str, &str, &str, &str)> = entries
        .iter()
        .map(|e| {
            (
                e.source.as_str(),
                e.destination.as_str(),
                e.bits.as_str(),
                e.action.as_str(),
            )
        })
        .collect();
    assert_eq!(
        projected,
        vec![
            ("A", "B", "10101010100100000000110011011110", "receive"),
            ("B", "A", "10101010100100000001", "send"),
        ]
    );

    // Structural invariants: bit strings, actions, monotonic timestamps.
    for entry in &entries {
        assert!(
            entry.bits.bytes().all(|b| b == b'0' || b == b'1'),
            "{entry:?}"
        );
        assert!(
            matches!(entry.action.as_str(), "send" | "receive"),
            "{entry:?}"
        );
        assert!(
            entry.timestamp.is_finite() && entry.timestamp >= 0.0,
            "{entry:?}"
        );
    }
    for pair in entries.windows(2) {
        assert!(
            pair[0].timestamp < pair[1].timestamp,
            "timestamps must advance: {entries:?}"
        );
    }
}

/// A profile that sleeps for 5 s with a 1 s budget: the watchdog returns
/// `internal` with the documented message (and the worker thread leaks
/// until it wakes — unavoidable, see the module docs).
#[test]
fn run_simulation_times_out() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let path = write_profile(&dir, "sleepy.simulator", SLEEPY_PROFILE);

    let started = std::time::Instant::now();
    let err = repo.run_simulation(&path, 1).unwrap_err();
    assert_eq!(err.code, "internal", "{err}");
    assert_eq!(err.message, "simulation timed out after 1s", "{err}");
    // The call must return at the deadline, not after the full sleep.
    assert!(started.elapsed().as_secs() < 4, "{:?}", started.elapsed());

    // The same profile with a generous budget completes (the leaked worker
    // of the previous call is still asleep; both threads coexist).
    let entries = repo.run_simulation(&path, 30).unwrap();
    assert!(entries.is_empty());
}

/// Missing files are `io`; XML the engine rejects is `invalid-argument`
/// with the parse error in the message; the same codes apply to
/// `load_simulator` and `run_simulation`.
#[test]
fn file_and_xml_errors() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();

    // Missing file.
    let missing = dir.path().join("nope.simulator");
    let err = repo.load_simulator(missing.to_str().unwrap()).unwrap_err();
    assert_eq!(err.code, "io", "{err}");
    assert!(err.message.contains("nope.simulator"), "{err}");
    let err = repo
        .run_simulation(missing.to_str().unwrap(), 5)
        .unwrap_err();
    assert_eq!(err.code, "io", "{err}");

    // Not XML at all.
    let junk = write_profile(&dir, "junk.simulator", "this is not xml");
    let err = repo.load_simulator(&junk).unwrap_err();
    assert_eq!(err.code, "invalid-argument", "{err}");
    let err = repo.run_simulation(&junk, 5).unwrap_err();
    assert_eq!(err.code, "invalid-argument", "{err}");

    // Well-formed XML but not a simulator config.
    let wrong_root = write_profile(
        &dir,
        "wrong_root.simulator",
        "<simulator><items></items></simulator>",
    );
    let err = repo.load_simulator(&wrong_root).unwrap_err();
    assert_eq!(err.code, "invalid-argument", "{err}");
    assert!(err.message.contains("simulator_config"), "{err}");

    // A simulator config without the <items> element.
    let no_items = write_profile(
        &dir,
        "no_items.simulator",
        "<simulator_config><participants/></simulator_config>",
    );
    let err = repo.load_simulator(&no_items).unwrap_err();
    assert_eq!(err.code, "invalid-argument", "{err}");
    assert!(err.message.contains("items"), "{err}");

    // An unknown item tag.
    let unknown = write_profile(
        &dir,
        "unknown.simulator",
        "<simulator_config><items><simulator_magic/></items></simulator_config>",
    );
    let err = repo.load_simulator(&unknown).unwrap_err();
    assert_eq!(err.code, "invalid-argument", "{err}");
    assert!(err.message.contains("simulator_magic"), "{err}");

    // A directory instead of a file.
    let err = repo
        .load_simulator(dir.path().to_str().unwrap())
        .unwrap_err();
    assert_eq!(err.code, "io", "{err}");
}

/// Engine failures are `internal` with the engine's message: a rule whose
/// condition does not parse fails at run time (the XML loader does not
/// validate expressions).
#[test]
fn engine_errors_are_internal() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let bad_expression = write_profile(
        &dir,
        "bad_expression.simulator",
        r#"<?xml version="1.0" ?>
<simulator_config>
  <modulators>
    <modulator carrier_amplitude="1" carrier_freq_hz="40000" carrier_phase_deg="0" gauss_bt="0.5" gauss_filter_width="1" index="0" modulation_type="0" name="Modulator" param_for_one="100" param_for_zero="0" sample_rate="None" samples_per_bit="100"/>
  </modulators>
  <participants>
    <participant address_hex="" color_index="0" id="alice-id" name="Alice" relative_rssi="0" shortname="A" simulate="0"/>
  </participants>
  <items>
    <simulator_rule>
      <simulator_rule_condition condition="this is (not an expression" type="IF"/>
    </simulator_rule>
  </items>
</simulator_config>
"#,
    );

    // The listing is fine — the condition is only summarized.
    let infos = repo.load_simulator(&bad_expression).unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, "rule");

    // Running it hits the expression parser.
    let err = repo.run_simulation(&bad_expression, 10).unwrap_err();
    assert_eq!(err.code, "internal", "{err}");
    assert!(err.message.contains("expression"), "{err}");
}

/// A panic-inducing profile (a label longer than the message bits) is
/// caught and reported as `internal` instead of killing the test process.
#[test]
fn engine_panics_are_caught() {
    let repo = SignalRepository::new();
    let dir = tempfile::tempdir().unwrap();
    let oversize_label = write_profile(
        &dir,
        "oversize_label.simulator",
        r#"<?xml version="1.0" ?>
<simulator_config>
  <modulators>
    <modulator carrier_amplitude="1" carrier_freq_hz="40000" carrier_phase_deg="0" gauss_bt="0.5" gauss_filter_width="1" index="0" modulation_type="0" name="Modulator" param_for_one="100" param_for_zero="0" sample_rate="None" samples_per_bit="100"/>
  </modulators>
  <participants>
    <participant address_hex="" color_index="1" id="bob-id" name="Bob" relative_rssi="1" shortname="B" simulate="2"/>
  </participants>
  <items>
    <simulator_message destination_id="" repeat="1">
      <message bits="10" decoding_index="0" message_type_id="m1" modulator_index="0" participant_id="bob-id" pause="0" timestamp="0"/>
      <simulator_label formula="1" value_type_index="2">
        <label end="8" name="data" start="0"/>
      </simulator_label>
    </simulator_message>
  </items>
</simulator_config>
"#,
    );

    let err = repo.run_simulation(&oversize_label, 10).unwrap_err();
    assert_eq!(err.code, "internal", "{err}");
    assert!(err.message.contains("panicked"), "{err}");
}
