//! Simulator service — v2 contract section "Simulator (v2, P5)"
//! (docs/ipc-contract.md).
//!
//! # What this module implements
//!
//! v2 is **read + run**: [`SignalRepository::load_simulator`] parses a
//! `.simulator` XML (the engine's config format,
//! [`SimulatorConfiguration::load_from_xml`]) and lists its logic items as
//! [`SimRuleInfo`]s; [`SignalRepository::run_simulation`] runs the engine
//! over the file and returns the message transcript as
//! [`SimTranscriptEntry`]s. Editing/saving simulator configs and wiring live
//! signals as simulation sources are out of scope. Neither method touches
//! repository state or takes a lock (contract lock discipline: one-shot
//! calls).
//!
//! # The simulation source (documented interpretation)
//!
//! The engine's only input surface is [`SimulatorIO::receive`] (the sniffer
//! stand-in); there is no device layer in v2. This service therefore runs
//! the profile against **itself**: every message whose source participant
//! does not simulate (an "external" participant, e.g. Alice in
//! `tests/data/testprofile.sim.xml`) contributes its configured template
//! bits to a message-source queue, in item order. The engine's waits are
//! served from that queue, one template per `receive` call; once it is
//! exhausted the source goes silent and the run stops (see the run settings
//! below). This is the finite-queue pattern the engine's ground-truth test
//! `tests/simulator_flow.rs` drives — here the queue is derived from the
//! profile instead of hand-written inputs.
//!
//! # Run settings
//!
//! `SimulatorSettings` is not persisted in `.simulator` files (it lives in
//! the project manager in the original), so this module pins it:
//! `num_repeats = 1` (one pass over the items) and
//! `error_handling_index = 1` — a silent source *stops* the run instead of
//! resending the last message ten times. A silent source is a normal
//! end-of-input, not an error: the run succeeds and returns the transcript
//! collected so far. Everything else keeps the engine defaults.
//!
//! # Timeout mechanism (and its honest tradeoff)
//!
//! `Simulator::run` is monolithic (no public step API) and can block for a
//! long time inside `thread::sleep` (sleep actions) or an unconditional
//! GOTO loop, so a deadline cannot be checked from the calling thread. The
//! run therefore happens on a dedicated worker thread and the caller blocks
//! on the result channel with `recv_timeout(timeout)`:
//!
//! - on completion the result is returned;
//! - after the deadline the caller returns
//!   `internal "simulation timed out after Ns"` — and the worker thread is
//!   **leaked**: nothing can interrupt a runaway `run()`. Two mitigations
//!   keep the leak rare and bounded: the IO goes silent at the deadline, so
//!   runs that wait on their source stop on their own shortly after (and
//!   the thread exits), and a truly stuck run only holds one thread plus
//!   its config copy until process exit.
//!
//! Engine panics (e.g. a `modulator_index` outside the configured
//! modulators, or labels longer than the message bits — both reachable via
//! hand-written XML) are caught with `catch_unwind` and reported as
//! `internal` with the panic message.
//!
//! # `SimRuleInfo` mapping
//!
//! The logic items of a profile are its rules and actions — everything that
//! is not a message, label or rule condition (messages are the traffic, and
//! the transcript covers them; labels and conditions are internals of their
//! parent items). Items are listed in pre-order (document order), nested
//! actions included, and `index` is the 0-based position in that listing.
//!
//! | `NodeKind`       | `kind`              | `name`              | `summary`                            |
//! |------------------|---------------------|---------------------|--------------------------------------|
//! | `Rule`           | `"rule"`            | `"Rule"`            | `IF <cond> / ELSE IF <cond> / ELSE`  |
//! | `Goto`           | `"goto"`            | `"Goto"`            | `goto <target item name>`            |
//! | `Sleep`          | `"sleep"`           | `"Sleep"`           | `sleep for <science_time>`           |
//! | `Counter`        | `"counter"`         | `"Counter"`         | `counter start=<s> step=<t>`         |
//! | `TriggerCommand` | `"trigger-command"` | `"Trigger command"` | `trigger command: <cmd>`             |
//!
//! Summaries mirror the engine's own log phrasing (`"GOTO item N"`,
//! `"Sleep for 1.50 s"` via [`science_time`]) where one exists. Degenerate
//! forms: a goto without a target summarizes as `goto (no target)`, a
//! trigger command without one as `trigger command (none)`, a condition
//! without an expression renders as its bare type (`ELSE`), and a rule
//! without conditions as `rule (no conditions)`.
//!
//! # Engine quirk: XML entities in conditions (pre-existing, upstream)
//!
//! The engine's XML writer escapes attribute values (`<` as `&lt;`, `>` as
//! `&gt;`, …) but its reader consumes quick-xml's raw attribute bytes
//! without decoding them. A condition like `item3.counter_value < 6` is
//! therefore *stored* as `&lt;` and *read back* as the literal text
//! `&lt;`, which the expression parser rejects at run time (`internal`).
//! In files: `<` and `&` have no working spelling (unescaped they are not
//! legal XML), while `>`, `"` hand-written unescaped load fine. The listing
//! is unaffected — the condition simply renders verbatim in the summary.
//! The decode belongs in the engine's loader, not in this service.
//!
//! # `SimTranscriptEntry` mapping
//!
//! The engine's `TranscriptEntry` carries the participants and bits but no
//! timestamps and no send/receive flag, so both are derived:
//!
//! - `action` is `"send"` when the entry's source participant simulates
//!   (the engine played that participant and transmitted the message) and
//!   `"receive"` when it is external (the engine matched an incoming
//!   message against that item).
//! - `timestamp` is the wall-clock time in seconds since the run started,
//!   correlated through the IO layer: a sent entry takes the time of an
//!   `io.send` call, a received one the time of the `io.receive` delivery
//!   that fed it. Resends and failed receive attempts make this an
//!   approximation; an entry that cannot be correlated inherits the
//!   previous entry's timestamp, and the sequence is monotonically
//!   non-decreasing by construction.
//! - `source`/`destination` are the participants' shortnames — the labels
//!   the engine's own transcript renders (`A->B`) — falling back to the
//!   full name, then the participant id, when the shortname is empty.
//! - `bits` is the plain bit string `"0101…"`.
//!
//! # Error codes
//!
//! Missing/unreadable files are `io`; XML that `load_from_xml` rejects is
//! `invalid-argument` (the parse error is included); engine failures
//! ([`SimError`]), timeouts and panics are `internal`.

use std::collections::VecDeque;
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use urh::simulator::config::{SimModulator, SimParticipant, SimulatorConfiguration};
use urh::simulator::engine::{Simulator, SimulatorIO, SimulatorSettings};
use urh::simulator::item::{science_time, NodeKind};
use urh::simulator::transcript::TranscriptEntry;
use urh::{Bit, Complex32};

use crate::{AppError, SignalRepository};

// ---------------------------------------------------------------------------
// Wire types (contract: "Simulator")
// ---------------------------------------------------------------------------

/// One logic item (rule or action) of a loaded `.simulator` profile
/// (contract: "Simulator"). See the module docs for the `kind`/`summary`
/// mapping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimRuleInfo {
    /// 0-based position in the listing of logic items (pre-order).
    pub index: usize,
    /// Display name of the item kind (`"Rule"`, `"Goto"`, ...).
    pub name: String,
    /// Machine-readable item kind (`"rule"`, `"goto"`, `"sleep"`,
    /// `"counter"`, `"trigger-command"`).
    pub kind: String,
    /// Human-readable one-liner derived from the item's configuration.
    pub summary: String,
}

/// One entry of a simulation run's message transcript (contract:
/// "Simulator"). See the module docs for how `action` and `timestamp` are
/// derived from the engine's transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimTranscriptEntry {
    /// Seconds since the run started (wall clock, IO-correlated; monotonic
    /// non-decreasing).
    pub timestamp: f64,
    /// Source participant label (shortname, e.g. `"A"`).
    pub source: String,
    /// Destination participant label (shortname, e.g. `"B"`).
    pub destination: String,
    /// Plain bits of the message, `"0101…"`.
    pub bits: String,
    /// `"send"` (the engine transmitted it) or `"receive"` (the engine
    /// matched it against an incoming message).
    pub action: String,
}

// ---------------------------------------------------------------------------
// Public service methods (contract: "Simulator")
// ---------------------------------------------------------------------------

impl SignalRepository {
    /// Parse a `.simulator` XML and list its logic items (contract:
    /// "Simulator"). Messages, labels and rule conditions are not listed —
    /// see the module docs for the mapping and the summary format.
    /// Unreadable files are `io`, XML the engine rejects is
    /// `invalid-argument`.
    pub fn load_simulator(&self, path: &str) -> Result<Vec<SimRuleInfo>, AppError> {
        let config = load_config(path)?;
        Ok(rule_infos(&config))
    }

    /// Run the engine over a `.simulator` profile and return the message
    /// transcript (contract: "Simulator"). The profile runs against itself
    /// as the source (module docs) for at most `timeout_secs` seconds; a
    /// timeout is an `internal` error and leaks the (unstoppable) worker
    /// thread — the documented tradeoff of the monolithic `Simulator::run`.
    /// Config/file errors are the same as
    /// [`load_simulator`](Self::load_simulator); engine failures are
    /// `internal`.
    pub fn run_simulation(
        &self,
        path: &str,
        timeout_secs: u64,
    ) -> Result<Vec<SimTranscriptEntry>, AppError> {
        let config = load_config(path)?;
        let modulators = config.modulators.clone();

        // `Instant + Duration` panics on overflow, so cap absurd timeouts:
        // anything beyond ~49 days means "run forever" in practice.
        let timeout =
            Duration::from_secs(timeout_secs).min(Duration::from_millis(u64::from(u32::MAX)));
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);

        let (tx, rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("urh-simulation".to_string())
            .spawn(move || {
                // Panics inside the engine (bad modulator indices, labels
                // longer than the message, ...) become internal errors
                // instead of a silently dropped channel.
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_to_transcript(config, modulators, deadline)
                }))
                .unwrap_or_else(|panic| {
                    Err(AppError::internal(format!(
                        "simulation thread panicked: {}",
                        panic_message(&panic)
                    )))
                });
                let _ = tx.send(result);
            })
            .map_err(|err| {
                AppError::internal(format!("failed to spawn simulation thread: {err}"))
            })?;
        // The handle is dropped (the worker detaches) in every path: on
        // success it is about to exit anyway, and on timeout it must be
        // leaked because `run()` cannot be interrupted.
        drop(worker);

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            // The worker may have finished within a scheduler hiccup of the
            // deadline; give it one last non-blocking chance before
            // declaring the timeout (and leaking it).
            Err(mpsc::RecvTimeoutError::Timeout) => match rx.try_recv() {
                Ok(result) => result,
                Err(_) => Err(AppError::internal(format!(
                    "simulation timed out after {timeout_secs}s"
                ))),
            },
            // Unreachable while `catch_unwind` guards the worker's send (a
            // disconnect would mean the thread died without running it),
            // but must not panic if it ever happens.
            Err(mpsc::RecvTimeoutError::Disconnected) => match rx.try_recv() {
                Ok(result) => result,
                Err(_) => Err(AppError::internal(
                    "simulation worker terminated without a result".to_string(),
                )),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Config loading and rule listing
// ---------------------------------------------------------------------------

/// Read and parse a `.simulator` XML (shared by both service methods).
fn load_config(path: &str) -> Result<SimulatorConfiguration, AppError> {
    let xml = fs::read_to_string(path).map_err(|err| AppError {
        code: AppError::CODE_IO,
        message: format!("{path}: {err}"),
    })?;
    let mut config = SimulatorConfiguration::new();
    config.load_from_xml(&xml).map_err(|err| {
        AppError::invalid_argument(format!("{path}: invalid simulator XML: {err}"))
    })?;
    Ok(config)
}

/// List the profile's logic items in pre-order (see the module docs).
fn rule_infos(config: &SimulatorConfiguration) -> Vec<SimRuleInfo> {
    let mut infos = Vec::new();
    for id in config.get_all_items() {
        let (kind, name) = match &config.node(id).kind {
            NodeKind::Rule => ("rule", "Rule"),
            NodeKind::Goto { .. } => ("goto", "Goto"),
            NodeKind::Sleep { .. } => ("sleep", "Sleep"),
            NodeKind::Counter { .. } => ("counter", "Counter"),
            NodeKind::TriggerCommand { .. } => ("trigger-command", "Trigger command"),
            // Messages, labels and rule conditions are not logic items.
            _ => continue,
        };
        infos.push(SimRuleInfo {
            index: infos.len(),
            name: name.to_string(),
            kind: kind.to_string(),
            summary: summarize_item(config, id),
        });
    }
    infos
}

/// Build an item's summary line (see the module docs for the formats).
fn summarize_item(config: &SimulatorConfiguration, id: usize) -> String {
    match &config.node(id).kind {
        NodeKind::Rule => {
            let mut parts = Vec::new();
            for child in &config.node(id).children {
                if let NodeKind::RuleCondition {
                    cond_type,
                    condition,
                } = &config.node(*child).kind
                {
                    if condition.is_empty() {
                        parts.push(cond_type.as_str().to_string());
                    } else {
                        parts.push(format!("{} {}", cond_type.as_str(), condition));
                    }
                }
            }
            if parts.is_empty() {
                "rule (no conditions)".to_string()
            } else {
                parts.join(" / ")
            }
        }
        NodeKind::Goto { goto_target } => match goto_target {
            Some(target) if !target.is_empty() => format!("goto {target}"),
            _ => "goto (no target)".to_string(),
        },
        NodeKind::Sleep { sleep_time } => format!("sleep for {}", science_time(*sleep_time)),
        NodeKind::Counter { start, step, .. } => {
            format!("counter start={start} step={step}")
        }
        NodeKind::TriggerCommand { command, .. } => match command {
            Some(command) if !command.trim().is_empty() => {
                format!("trigger command: {command}")
            }
            _ => "trigger command (none)".to_string(),
        },
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Simulation run
// ---------------------------------------------------------------------------

/// What a transcript entry's source participant did (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// The engine played the source participant and transmitted.
    Send,
    /// The source participant is external; the engine received.
    Receive,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Send => "send",
            Action::Receive => "receive",
        }
    }
}

/// Which IO event a transcript entry is correlated with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoEventKind {
    /// `io.send` — the engine transmitted a modulated message.
    Send,
    /// `io.receive` delivered bits that the engine matched to an item.
    Receive,
}

/// The message-source IO: replays the profile's receive-message templates
/// (module docs) and keeps a wall-clock timeline of send/receive events for
/// transcript timestamping. The modulated samples themselves are not kept —
/// the v2 transcript is bits-only.
struct TranscriptIO {
    queue: VecDeque<Vec<Bit>>,
    start: Instant,
    deadline: Instant,
    events: Vec<(IoEventKind, f64)>,
}

impl TranscriptIO {
    fn new(queue: VecDeque<Vec<Bit>>, start: Instant, deadline: Instant) -> Self {
        TranscriptIO {
            queue,
            start,
            deadline,
            events: Vec::new(),
        }
    }

    fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Times of the recorded events of one kind, in order.
    fn event_times(&self, kind: IoEventKind) -> impl Iterator<Item = f64> + '_ {
        self.events
            .iter()
            .filter_map(move |(k, time)| (*k == kind).then_some(*time))
    }
}

impl SimulatorIO for TranscriptIO {
    fn receive(&mut self) -> Option<Vec<Bit>> {
        // Watchdog assist: a run still waiting on its source after the
        // deadline goes silent, which ends the run through the
        // stop-on-error setting instead of leaking the worker thread.
        if Instant::now() >= self.deadline {
            return None;
        }
        let bits = self.queue.pop_front()?;
        self.events.push((IoEventKind::Receive, self.elapsed()));
        Some(bits)
    }

    fn send(&mut self, _samples: &[Complex32]) {
        self.events.push((IoEventKind::Send, self.elapsed()));
    }
}

/// Run the engine over `config` and project the transcript (module docs).
fn run_to_transcript(
    config: SimulatorConfiguration,
    modulators: Vec<SimModulator>,
    deadline: Instant,
) -> Result<Vec<SimTranscriptEntry>, AppError> {
    let queue = source_templates(&config);
    let mut io = TranscriptIO::new(queue, Instant::now(), deadline);

    let mut sim = Simulator::new(config, modulators);
    sim.settings = SimulatorSettings {
        num_repeats: 1,
        error_handling_index: 1,
        ..SimulatorSettings::default()
    };
    sim.run(&mut io)
        .map_err(|err| AppError::internal(format!("simulation failed: {err}")))?;
    Ok(transcript_entries(&sim, &io))
}

/// The message-source queue: the template bits of every receive message
/// (source participant set and not simulated), in item order.
fn source_templates(config: &SimulatorConfiguration) -> VecDeque<Vec<Bit>> {
    config
        .get_all_items()
        .into_iter()
        .filter_map(|id| config.message(id))
        .filter(|msg| msg.source.is_some() && !config.participant_simulates(msg.source))
        .map(|msg| msg.plain_bits.clone())
        .collect()
}

/// Project the engine's transcript into wire entries, timestamping them
/// from the IO event timeline (module docs).
fn transcript_entries(sim: &Simulator, io: &TranscriptIO) -> Vec<SimTranscriptEntry> {
    let mut sends = io.event_times(IoEventKind::Send);
    let mut receives = io.event_times(IoEventKind::Receive);

    let mut entries = Vec::new();
    let mut last = 0.0f64;
    for entry in sim.transcript.entries() {
        let action = action_for(sim, entry);
        let correlated = match action {
            Action::Send => sends.next(),
            Action::Receive => receives.next(),
        }
        .unwrap_or(last);
        // Monotonic by construction: resends can pair an entry with an event
        // that predates the previous entry's timestamp.
        let timestamp = correlated.max(last);
        last = timestamp;
        entries.push(SimTranscriptEntry {
            timestamp,
            source: participant_label(
                &entry.source_shortname,
                &entry.source_id,
                &sim.config.participants,
            ),
            destination: participant_label(
                &entry.destination_shortname,
                &entry.destination_id,
                &sim.config.participants,
            ),
            bits: bits_to_string(&entry.bits),
            action: action.as_str().to_string(),
        });
    }
    entries
}

/// Classify an entry by its source participant: simulated participants are
/// played by the engine (send), external ones arrive through the source
/// (receive).
fn action_for(sim: &Simulator, entry: &TranscriptEntry) -> Action {
    let simulates = sim
        .config
        .participants
        .iter()
        .any(|p| p.id == entry.source_id && p.simulates());
    if simulates {
        Action::Send
    } else {
        Action::Receive
    }
}

/// Display label for a transcript participant: the shortname (what the
/// engine's transcript renders), else the full name, else the id.
fn participant_label(shortname: &str, id: &str, participants: &[SimParticipant]) -> String {
    if !shortname.is_empty() {
        return shortname.to_string();
    }
    participants
        .iter()
        .find(|p| p.id == id)
        .map(|p| {
            if p.name.is_empty() {
                p.id.clone()
            } else {
                p.name.clone()
            }
        })
        .unwrap_or_default()
}

/// Plain bits as a `"0101…"` string (the engine transcript's own mapping).
fn bits_to_string(bits: &[Bit]) -> String {
    bits.iter().map(|b| char::from(b'0' + b)).collect()
}

/// Best-effort message from a caught panic payload.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use urh::simulator::config::SimParticipant;
    use urh::simulator::item::{ConditionType, SimMessage};

    fn bits(s: &str) -> Vec<Bit> {
        s.bytes().map(|b| b - b'0').collect()
    }

    /// Two participants like the flow-test fixture: Alice external, Bob
    /// simulated.
    fn participants() -> Vec<SimParticipant> {
        vec![
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
        ]
    }

    fn message(bits_str: &str, source: Option<usize>, destination: Option<usize>) -> NodeKind {
        let mut msg = SimMessage::new(bits(bits_str), 0);
        msg.source = source;
        msg.destination = destination;
        NodeKind::Message(msg)
    }

    /// A profile exercising every logic item: receive msg, send msg,
    /// counter, rule (IF with a nested send message, ELSE with a nested
    /// sleep), goto, trigger command.
    fn test_config() -> SimulatorConfiguration {
        let mut config = SimulatorConfiguration::new();
        config.participants = participants();
        config.add_item(message("1010", Some(0), Some(1)), None); // item 1 (receive)
        config.add_item(message("0101", Some(1), Some(0)), None); // item 2 (send)
        config.add_item(
            NodeKind::Counter {
                start: 3,
                step: 2,
                value: 3,
            },
            None,
        ); // item 3
        let rule = config.add_item(NodeKind::Rule, None); // item 4
        let cond_if = config.add_item(
            NodeKind::RuleCondition {
                cond_type: ConditionType::If,
                condition: "item3.counter_value < 6".to_string(),
            },
            Some(rule),
        ); // item 4.1
        config.add_item(message("1111", Some(1), Some(0)), Some(cond_if)); // 4.1.1
        let cond_else = config.add_item(
            NodeKind::RuleCondition {
                cond_type: ConditionType::Else,
                condition: String::new(),
            },
            Some(rule),
        ); // item 4.2
        config.add_item(NodeKind::Sleep { sleep_time: 0.0 }, Some(cond_else)); // 4.2.1
        config.add_item(
            NodeKind::Goto {
                goto_target: Some("item6".to_string()),
            },
            None,
        ); // item 5
        config.add_item(
            NodeKind::TriggerCommand {
                command: Some("echo hi".to_string()),
                pass_transcript: false,
                return_code: 0,
            },
            None,
        ); // item 6
        config
    }

    #[test]
    fn rule_infos_lists_logic_items_in_preorder() {
        let config = test_config();
        let infos = rule_infos(&config);

        let summarized: Vec<(usize, &str, &str, &str)> = infos
            .iter()
            .map(|info| {
                (
                    info.index,
                    info.name.as_str(),
                    info.kind.as_str(),
                    info.summary.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summarized,
            vec![
                (0, "Counter", "counter", "counter start=3 step=2"),
                (1, "Rule", "rule", "IF item3.counter_value < 6 / ELSE"),
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

    #[test]
    fn summaries_cover_the_degenerate_forms() {
        let mut config = SimulatorConfiguration::new();
        config.add_item(NodeKind::Rule, None); // no conditions
        config.add_item(NodeKind::Goto { goto_target: None }, None);
        config.add_item(
            NodeKind::Goto {
                goto_target: Some(String::new()),
            },
            None,
        );
        config.add_item(NodeKind::Sleep { sleep_time: 1.5 }, None);
        config.add_item(
            NodeKind::TriggerCommand {
                command: None,
                pass_transcript: true,
                return_code: 0,
            },
            None,
        );
        config.add_item(
            NodeKind::TriggerCommand {
                command: Some("   ".to_string()),
                pass_transcript: false,
                return_code: 0,
            },
            None,
        );
        let summaries: Vec<String> = rule_infos(&config)
            .into_iter()
            .map(|info| info.summary)
            .collect();
        assert_eq!(
            summaries,
            vec![
                "rule (no conditions)".to_string(),
                "goto (no target)".to_string(),
                "goto (no target)".to_string(),
                "sleep for 1.50 s".to_string(),
                "trigger command (none)".to_string(),
                "trigger command (none)".to_string(),
            ]
        );
    }

    #[test]
    fn rule_infos_skip_traffic_and_organizational_items() {
        // A profile made only of traffic (messages) lists nothing:
        // messages and rule conditions are never part of the listing
        // (the populated case is covered by
        // `rule_infos_lists_logic_items_in_preorder`).
        let mut config = SimulatorConfiguration::new();
        config.participants = participants();
        config.add_item(message("1010", Some(1), Some(0)), None);
        config.add_item(message("0101", Some(0), Some(1)), None);
        assert!(rule_infos(&config).is_empty());
    }

    #[test]
    fn source_templates_picks_external_receive_messages_in_order() {
        let mut config = SimulatorConfiguration::new();
        config.participants = participants();
        // item 1: receive (Alice external).
        config.add_item(message("1010", Some(0), Some(1)), None);
        // item 2: send (Bob simulated) — not a source.
        config.add_item(message("0101", Some(1), Some(0)), None);
        // item 3: no source — skipped by the engine entirely.
        config.add_item(message("1111", None, None), None);
        // item 4: nested receive inside a rule.
        let rule = config.add_item(NodeKind::Rule, None);
        let cond = config.add_item(
            NodeKind::RuleCondition {
                cond_type: ConditionType::If,
                condition: String::new(),
            },
            Some(rule),
        );
        config.add_item(message("0001", Some(0), Some(1)), Some(cond));

        let queue = source_templates(&config);
        let templates: Vec<String> = queue
            .into_iter()
            .map(|bits| bits_to_string(&bits))
            .collect();
        assert_eq!(templates, vec!["1010".to_string(), "0001".to_string()]);
    }

    #[test]
    fn transcript_io_replays_the_queue_then_goes_silent() {
        let queue = vec![bits("10"), bits("11")].into();
        let mut io = TranscriptIO::new(queue, Instant::now(), far_future());
        assert_eq!(io.receive().unwrap(), bits("10"));
        assert_eq!(io.receive().unwrap(), bits("11"));
        assert!(io.receive().is_none());
        assert!(io.receive().is_none(), "silent sources stay silent");

        // Sends are recorded even after the source is exhausted.
        io.send(&[Complex32::new(1.0, 0.0)]);
        let times: Vec<f64> = io.event_times(IoEventKind::Receive).collect();
        assert_eq!(times.len(), 2);
        assert!(times.iter().all(|t| *t >= 0.0));
        assert_eq!(io.event_times(IoEventKind::Send).count(), 1);
    }

    #[test]
    fn transcript_io_goes_silent_at_the_deadline() {
        let queue = vec![bits("10")].into();
        // Deadline already in the past: the watchdog assist must refuse to
        // feed even a non-empty queue.
        let mut io = TranscriptIO::new(queue, Instant::now(), Instant::now());
        assert!(io.receive().is_none());
    }

    fn far_future() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    /// End-to-end over a hand-built config: the transcript is projected with
    /// actions, labels, bits and monotonic timestamps.
    #[test]
    fn transcript_entries_are_projected_from_a_run() {
        let config = test_config();
        let modulators = config.modulators.clone();
        let entries = run_to_transcript(config, modulators, far_future()).unwrap();

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
        // Timestamps: finite, non-negative, non-decreasing, and strictly
        // positive after the first entry (each later message follows an IO
        // event that happened after the previous one).
        assert!(entries[0].timestamp.is_finite() && entries[0].timestamp >= 0.0);
        for pair in entries.windows(2) {
            assert!(
                pair[0].timestamp < pair[1].timestamp,
                "timestamps must advance: {:?}",
                entries
            );
        }
        assert!(entries.last().unwrap().timestamp < 1.0);
    }

    #[test]
    fn participant_label_falls_back_to_name_then_id() {
        let participants = vec![
            SimParticipant {
                name: "Alice".to_string(),
                shortname: String::new(),
                address_hex: String::new(),
                color_index: 0,
                relative_rssi: 0,
                id: "alice-id".to_string(),
                simulate: 0,
            },
            SimParticipant {
                name: String::new(),
                shortname: String::new(),
                address_hex: String::new(),
                color_index: 0,
                relative_rssi: 0,
                id: "naked-id".to_string(),
                simulate: 0,
            },
        ];
        assert_eq!(participant_label("A", "alice-id", &participants), "A");
        assert_eq!(participant_label("", "alice-id", &participants), "Alice");
        assert_eq!(participant_label("", "naked-id", &participants), "naked-id");
        assert_eq!(participant_label("", "unknown", &participants), "");
    }

    #[test]
    fn bits_to_string_matches_the_engine_mapping() {
        assert_eq!(bits_to_string(&bits("01001110")), "01001110");
        assert_eq!(bits_to_string(&[]), "");
    }
}
