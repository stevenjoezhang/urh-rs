//! Port of `urh/simulator/Simulator.py` — the core simulation engine:
//! message reception -> rule matching -> action execution -> send loop.
//!
//! Qt threading, signals, `ProtocolSniffer` and `EndlessSender` devices are
//! replaced by a synchronous run loop over the [`SimulatorIO`] trait:
//! `receive` stands for the sniffer (returning the next demodulated
//! message's bits, or `None` on timeout) and `send` for the sender's
//! `push_data` of modulated samples.
//!
//! Decoding note: the engine treats decoded bits as identical to plain bits
//! (the NRZ identity decoding used by the reference simulator profiles).
//! `decoding_index` is carried through the model but other decodings are not
//! applied.

use std::thread;
use std::time::Duration;

use thiserror::Error;

use crate::simulator::config::{SimModulator, SimulatorConfiguration};
use crate::simulator::expression::{ExprError, ExpressionParser, Value};
use crate::simulator::item::{
    ConditionType, NodeId, NodeKind, SimLabel, science_time,
};
use crate::simulator::transcript::Transcript;
use crate::{Bit, Complex32};

/// Device/IO abstraction replacing sniffer + sender.
pub trait SimulatorIO {
    /// Stand-in for `ProtocolSniffer` + demodulation: return the next
    /// received message's bits, or `None` on timeout (Python:
    /// `Simulator.receive_message`).
    fn receive(&mut self) -> Option<Vec<Bit>>;
    /// Stand-in for `EndlessSender.push_data(modulated)`. Called once per
    /// repeat, like the Python `send_message` loop.
    fn send(&mut self, samples: &[Complex32]);
}

/// Settings taken from the `ProjectManager` in the Python version.
#[derive(Debug, Clone)]
pub struct SimulatorSettings {
    /// `simulator_num_repeat`; 0 = endless.
    pub num_repeats: usize,
    /// `simulator_retries`.
    pub retries: usize,
    /// `simulator_timeout_ms`.
    pub timeout_ms: u64,
    /// `simulator_error_handling_index`: 0 = resend last message,
    /// 1 = stop simulation, 2 = restart simulation.
    pub error_handling_index: usize,
    /// Seed for random label values (`value_type_index == 4`).
    pub random_seed: u64,
}

impl Default for SimulatorSettings {
    fn default() -> Self {
        SimulatorSettings {
            num_repeats: 1,
            retries: 10,
            timeout_ms: 2500,
            error_handling_index: 0,
            random_seed: 0x2545F4914F6CDD1D,
        }
    }
}

#[derive(Debug, Error)]
pub enum SimError {
    #[error("expression error in {1}: {0}")]
    Expression(#[source] ExprError, String),
    #[error("unknown action at item {0}")]
    UnknownAction(String),
    #[error("label '{0}' produces a non-integer value")]
    NonIntegerLabelValue(String),
    #[error("value {0} too big for label {1}")]
    LabelValueTooBig(i64, String),
}

/// Small deterministic PRNG standing in for `numpy.random.randint`
/// (xorshift64*).
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(if seed == 0 { 0x9E3779B97F4A7C15 } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// `numpy.random.randint(low, high + 1)` — inclusive on both ends.
    fn randint(&mut self, low: i64, high: i64) -> i64 {
        if high <= low {
            return low;
        }
        let span = (high - low + 1) as u64;
        low + (self.next_u64() % span) as i64
    }
}

/// Port of `Simulator` (non-Qt core).
pub struct Simulator {
    pub config: SimulatorConfiguration,
    pub modulators: Vec<SimModulator>,
    pub settings: SimulatorSettings,
    pub transcript: Transcript,
    pub log_messages: Vec<String>,
    current_item: Option<NodeId>,
    current_repeat: usize,
    is_simulating: bool,
    do_restart: bool,
    last_sent_message: Option<NodeId>,
    rng: Rng,
}

impl Simulator {
    pub fn new(config: SimulatorConfiguration, modulators: Vec<SimModulator>) -> Self {
        let settings = SimulatorSettings::default();
        let rng = Rng::new(settings.random_seed);
        Simulator {
            config,
            modulators,
            settings,
            transcript: Transcript::new(),
            log_messages: Vec::new(),
            current_item: None,
            current_repeat: 0,
            is_simulating: false,
            do_restart: false,
            last_sent_message: None,
            rng,
        }
    }

    // -- public accessors (Python properties) --

    pub fn is_simulating(&self) -> bool {
        self.is_simulating
    }

    pub fn current_repeat(&self) -> usize {
        self.current_repeat
    }

    /// `simulation_is_finished`.
    pub fn simulation_is_finished(&self) -> bool {
        if self.settings.num_repeats == 0 {
            return false;
        }
        self.current_repeat >= self.settings.num_repeats
    }

    /// `read_log_messages`.
    pub fn read_log_messages(&mut self) -> Vec<String> {
        let result = self.log_messages.clone();
        self.log_messages.clear();
        result
    }

    // -- run loop --

    /// `start()` + `simulate()` + `stop()`, synchronous.
    pub fn run(&mut self, io: &mut dyn SimulatorIO) -> Result<(), SimError> {
        self.reset();
        self.transcript.clear();
        self.initialize_counters();
        self.log_message("Simulation is running");

        while self.is_simulating && !self.simulation_is_finished() {
            let next = self.step(io)?;
            self.current_item = next;
            if self.do_restart {
                self.restart();
            }
        }

        self.stop("Finished");
        Ok(())
    }

    /// One iteration of the `simulate` while-loop.
    fn step(&mut self, io: &mut dyn SimulatorIO) -> Result<Option<NodeId>, SimError> {
        let Some(id) = self.current_item else {
            self.current_repeat += 1;
            self.transcript.start_new_round();
            return Ok(Some(self.config.root));
        };

        let kind = self.config.node(id).kind.clone();
        match kind {
            NodeKind::Root | NodeKind::Label(_) => Ok(self.config.next(id)),
            NodeKind::Message(_) => {
                self.process_message(id, io)?;
                Ok(self.config.next(id))
            }
            NodeKind::Goto { .. } => {
                let target = self.config.goto_target(id);
                if let Some(t) = target {
                    self.log_message(format!("GOTO item {}", self.config.index_of(t)));
                }
                Ok(target)
            }
            NodeKind::TriggerCommand { command, .. } => {
                let command = self.fill_counter_values(command.as_deref().unwrap_or(""));
                self.log_message(format!("Calling {command}"));
                // External program execution is not supported by urh-core.
                self.log_message(
                    "External program execution is not supported by urh-core".to_string(),
                );
                Ok(self.config.next(id))
            }
            NodeKind::Rule => {
                let children = self.config.node(id).children.clone();
                let mut applying = None;
                for child in children {
                    if self.condition_applies(child)? {
                        applying = Some(child);
                        break;
                    }
                }
                if let Some(cond) = applying {
                    let (cond_type, condition, logging_active, cond_children) = {
                        let n = self.config.node(cond);
                        match &n.kind {
                            NodeKind::RuleCondition {
                                cond_type,
                                condition,
                            } => (*cond_type, condition.clone(), n.logging_active, n.children.clone()),
                            _ => (ConditionType::If, String::new(), true, Vec::new()),
                        }
                    };
                    if logging_active && cond_type != ConditionType::Else && !condition.is_empty()
                    {
                        self.log_message(format!(
                            "Rule condition {} ({}) applied",
                            self.config.index_of(cond),
                            condition
                        ));
                    }
                    if !cond_children.is_empty() {
                        return Ok(Some(cond_children[0]));
                    }
                }
                Ok(self.config.next_sibling(id))
            }
            NodeKind::RuleCondition { cond_type, .. } => {
                let parent = self.config.node(id).parent;
                if cond_type == ConditionType::If {
                    Ok(parent)
                } else {
                    Ok(parent.and_then(|p| self.config.next_sibling(p)))
                }
            }
            NodeKind::Sleep { sleep_time } => {
                self.log_message(format!("Sleep for {}", science_time(sleep_time)));
                if sleep_time > 0.0 {
                    thread::sleep(Duration::from_secs_f64(sleep_time));
                }
                Ok(self.config.next(id))
            }
            NodeKind::Counter { step, value, .. } => {
                let new_value = value.wrapping_add(step);
                if let NodeKind::Counter { value, .. } = &mut self.config.node_mut(id).kind {
                    *value = new_value;
                }
                self.log_message(format!("Increase counter by {step} to {new_value}"));
                Ok(self.config.next(id))
            }
        }
    }

    /// `SimulatorRuleCondition.condition_applies` for a rule's child.
    fn condition_applies(&mut self, cond_id: NodeId) -> Result<bool, SimError> {
        let (cond_type, condition) = match &self.config.node(cond_id).kind {
            NodeKind::RuleCondition {
                cond_type,
                condition,
            } => (*cond_type, condition.clone()),
            _ => return Ok(false),
        };
        if cond_type == ConditionType::Else {
            return Ok(true);
        }
        let node = ExpressionParser::new(&self.config)
            .validate_condition(&condition)
            .map_err(|e| SimError::Expression(e, condition.clone()))?;
        let value = ExpressionParser::new(&self.config)
            .evaluate_node(&node)
            .map_err(|e| SimError::Expression(e, condition.clone()))?;
        Ok(value.is_truthy())
    }

    // -- message processing --

    /// `process_message`.
    fn process_message(
        &mut self,
        msg_id: NodeId,
        io: &mut dyn SimulatorIO,
    ) -> Result<(), SimError> {
        let index = self.config.index_of(msg_id);
        let (source, destination, repeat, modulator_index, pause) = {
            let msg = self.config.message(msg_id).unwrap();
            (
                msg.source,
                msg.destination,
                msg.repeat,
                msg.modulator_index,
                msg.pause,
            )
        };

        if source.is_none() {
            return Ok(());
        }

        let mut new_bits = self.generate_message_from_template(msg_id)?;

        if self.config.participant_simulates(source) {
            // We have to send a message.
            // Fill checksum labels first.
            for lbl_id in self.config.labels_of(msg_id) {
                let lbl = match &self.config.node(lbl_id).kind {
                    NodeKind::Label(lbl) => lbl.clone(),
                    _ => continue,
                };
                if let Some(cs) = &lbl.checksum {
                    let checksum = cs.calculate(&new_bits);
                    let lbl_len = lbl.end.saturating_sub(lbl.start);
                    for i in 0..lbl_len {
                        new_bits[lbl.start + i] = checksum.get(i).copied().unwrap_or(0);
                    }
                }
            }

            self.transcript_append(source, destination, &new_bits, &index);
            self.send_message(&new_bits, pause, repeat, modulator_index, io);
            self.log_message(format!("Sending message {index}"));
            if let Some(msg) = self.config.message_mut(msg_id) {
                msg.send_recv.push(new_bits);
            }
            self.last_sent_message = Some(msg_id);
        } else {
            // We have to receive a message.
            self.log_message(format!("Waiting for message {index}..."));
            let mut retry = 0usize;

            while self.is_simulating
                && !self.simulation_is_finished()
                && retry < self.settings.retries
            {
                let received = io.receive();
                if !self.is_simulating {
                    return Ok(());
                }
                let received_bits = match received {
                    Some(bits) => bits,
                    None => {
                        self.log_message("Receive timeout".to_string());
                        match self.settings.error_handling_index {
                            0 => {
                                self.resend_last_message(io);
                                retry += 1;
                                continue;
                            }
                            1 => {
                                self.stop("");
                                return Ok(());
                            }
                            _ => {
                                self.do_restart = true;
                                return Ok(());
                            }
                        }
                    }
                };

                self.log_message(format!("  Received {} data bits", received_bits.len()));
                self.log_message("  Check whether received data matches");

                match self.check_message(&received_bits, &new_bits, msg_id, retry) {
                    Ok(()) => {
                        if let Some(msg) = self.config.message_mut(msg_id) {
                            msg.send_recv.push(received_bits.clone());
                        }
                        self.transcript_append(source, destination, &received_bits, &index);
                        self.log_message(format!("Received message {index}: "));
                        return Ok(());
                    }
                    Err(error_msg) => {
                        if !error_msg.is_empty() {
                            self.log_message(error_msg);
                        }
                    }
                }

                retry += 1;
            }

            if retry == self.settings.retries {
                self.log_message(format!("Message {index} not received"));
                self.stop("");
            }
        }
        Ok(())
    }

    /// `check_message`: Ok(()) on match, Err(log message) on mismatch.
    fn check_message(
        &self,
        received_bits: &[Bit],
        expected_bits: &[Bit],
        msg_id: NodeId,
        retry: usize,
    ) -> Result<(), String> {
        let index = self.config.index_of(msg_id);
        if received_bits.is_empty() {
            return Err(format!("Failed to decode message {index}"));
        }

        for lbl_id in self.config.labels_of(msg_id) {
            let lbl = match &self.config.node(lbl_id).kind {
                NodeKind::Label(lbl) => lbl,
                _ => continue,
            };
            // Skip live (1) and random (4) value types.
            if lbl.value_type_index == 1 || lbl.value_type_index == 4 {
                continue;
            }

            let start = lbl.start.min(received_bits.len());
            let end = lbl.end.min(received_bits.len());
            let (actual, expected) = if let Some(cs) = &lbl.checksum {
                let expected = cs.calculate(received_bits);
                (received_bits[start..end].to_vec(), expected)
            } else {
                let exp_start = lbl.start.min(expected_bits.len());
                let exp_end = lbl.end.min(expected_bits.len());
                (
                    received_bits[start..end].to_vec(),
                    expected_bits[exp_start..exp_end].to_vec(),
                )
            };

            if actual != expected {
                return Err(format!(
                    "Attempt for message {index} [{}/{}] Mismatch for label: {}",
                    retry + 1,
                    self.settings.retries,
                    lbl.name
                ));
            }
        }
        Ok(())
    }

    /// `generate_message_from_template`.
    fn generate_message_from_template(
        &mut self,
        msg_id: NodeId,
    ) -> Result<Vec<Bit>, SimError> {
        let template = self.config.message(msg_id).unwrap().clone();
        let mut new_bits = template.plain_bits.clone();

        for lbl_id in self.config.labels_of(msg_id) {
            let lbl = match &self.config.node(lbl_id).kind {
                NodeKind::Label(lbl) => lbl.clone(),
                _ => continue,
            };
            match lbl.value_type_index {
                2 => {
                    // formula
                    let node = ExpressionParser::new(&self.config)
                        .validate_formula(&lbl.formula)
                        .map_err(|e| SimError::Expression(e, lbl.formula.clone()))?;
                    let value = ExpressionParser::new(&self.config)
                        .evaluate_node(&node)
                        .map_err(|e| SimError::Expression(e, lbl.formula.clone()))?;
                    let n = match value {
                        Value::Int(n) => n,
                        _ => {
                            return Err(SimError::NonIntegerLabelValue(lbl.name.clone()));
                        }
                    };
                    self.set_label_value(&mut new_bits, &lbl, n)?;
                }
                3 => {
                    // external program
                    self.log_message(
                        "External program execution is not supported by urh-core".to_string(),
                    );
                    continue;
                }
                4 => {
                    // random value
                    let n = self.rng.randint(lbl.random_min, lbl.random_max);
                    self.set_label_value(&mut new_bits, &lbl, n)?;
                }
                _ => continue,
            }
        }

        Ok(new_bits)
    }

    /// `Simulator.set_label_value`. Like Python, values that do not fit keep
    /// their most significant bits (the Python loop assigns `bits[i]` for
    /// `i < lbl_len`, i.e. the leading characters of the binary string).
    fn set_label_value(
        &mut self,
        bits: &mut [Bit],
        label: &SimLabel,
        decimal_value: i64,
    ) -> Result<(), SimError> {
        if decimal_value < 0 {
            return Err(SimError::LabelValueTooBig(decimal_value, label.name.clone()));
        }
        let lbl_len = label.end.saturating_sub(label.start);
        // Python: "{0:0<lbl_len>b}" — zero-padded on the left.
        let mut binary = format!("{decimal_value:b}");
        if binary.len() < lbl_len {
            let mut padded = "0".repeat(lbl_len - binary.len());
            padded.push_str(&binary);
            binary = padded;
        }
        if binary.len() > lbl_len {
            // Python logs a warning and truncates (keeping the MSBs).
            self.log_message(format!(
                "Value {decimal_value} too big for label {}, bits truncated",
                label.name
            ));
        }
        for i in 0..lbl_len {
            bits[label.start + i] = binary
                .as_bytes()
                .get(i)
                .map(|b| b - b'0')
                .unwrap_or(0);
        }
        Ok(())
    }

    /// `send_message`.
    fn send_message(
        &self,
        bits: &[Bit],
        pause: u64,
        repeat: usize,
        modulator_index: usize,
        io: &mut dyn SimulatorIO,
    ) {
        let modulator = &self.modulators[modulator_index];
        let modulated = modulator.modulate(bits, pause as u32);
        for _ in 0..repeat {
            io.send(&modulated);
        }
    }

    /// `resend_last_message`.
    fn resend_last_message(&mut self, io: &mut dyn SimulatorIO) {
        self.log_message("Resending last message".to_string());
        let Some(lsm) = self.last_sent_message else {
            return;
        };
        let (repeat, modulator_index, pause) = {
            let msg = self.config.message(lsm).unwrap();
            (msg.repeat, msg.modulator_index, msg.pause)
        };
        let bits = self
            .config
            .message(lsm)
            .unwrap()
            .send_recv
            .last()
            .cloned();
        if let Some(bits) = bits {
            self.send_message(&bits, pause, repeat, modulator_index, io);
        }
    }

    /// `__fill_counter_values`: substitute `itemN.counter_value` tokens in a
    /// command string with the current counter values. Python splits on the
    /// regex `item[0-9]+\.counter_value`, so a token matches even when glued
    /// to following word characters; a matching token that is not a counter
    /// is dropped (Python appends nothing on the KeyError path).
    fn fill_counter_values(&self, command: &str) -> String {
        const SUFFIX: &str = ".counter_value";
        let bytes = command.as_bytes();
        let mut result = String::new();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'i' && command[i..].starts_with("item") {
                let mut j = i + 4;
                let digit_start = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > digit_start && command[j..].starts_with(SUFFIX) {
                    let end = j + SUFFIX.len();
                    let token = &command[i..end];
                    match self
                        .config
                        .lookup_item(token)
                        .map(|id| &self.config.node(id).kind)
                    {
                        Some(NodeKind::Counter { value, .. }) => {
                            result.push_str(&value.to_string());
                        }
                        _ => {
                            // Not a counter: Python's except path appends
                            // nothing (the token is dropped).
                        }
                    }
                    i = end;
                    continue;
                }
            }
            result.push(bytes[i] as char);
            i += 1;
        }
        result
    }

    // -- state management --

    /// `reset`.
    fn reset(&mut self) {
        self.current_item = Some(self.config.root);
        for msg_id in self.config.get_all_messages() {
            if let Some(msg) = self.config.message_mut(msg_id) {
                msg.send_recv.clear();
            }
        }
        self.last_sent_message = None;
        self.is_simulating = true;
        self.do_restart = false;
        self.current_repeat = 0;
        self.log_messages.clear();
    }

    /// `restart`.
    fn restart(&mut self) {
        self.transcript.start_new_round();
        self.reset();
        self.log_message("Restarting simulation");
    }

    /// `stop`.
    fn stop(&mut self, msg: &str) {
        if self.is_simulating {
            let suffix = if msg.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", msg.trim())
            };
            self.log_message(format!("Stop simulation{suffix}"));
        }
        self.is_simulating = false;
        self.do_restart = false;
    }

    /// `__initialize_counters`.
    fn initialize_counters(&mut self) {
        for id in self.config.get_all_items() {
            if let NodeKind::Counter { start, .. } = self.config.node(id).kind.clone() {
                if let NodeKind::Counter { value, .. } = &mut self.config.node_mut(id).kind {
                    *value = start;
                }
            }
        }
    }

    fn transcript_append(
        &mut self,
        source: Option<usize>,
        destination: Option<usize>,
        bits: &[Bit],
        msg_index: &str,
    ) {
        let src = self.config.participant(source);
        let dst = self.config.participant(destination);
        self.transcript.append(
            &src.map(|p| p.id.clone()).unwrap_or_default(),
            &src.map(|p| p.shortname.clone()).unwrap_or_default(),
            &dst.map(|p| p.id.clone()).unwrap_or_default(),
            &dst.map(|p| p.shortname.clone()).unwrap_or_default(),
            bits.to_vec(),
            msg_index,
        );
    }

    /// `log_message` (without timestamps / HTML markup).
    fn log_message(&mut self, message: impl Into<String>) {
        self.log_messages.push(message.into());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulator::config::{SimParticipant, SimModulator};
    use crate::simulator::item::{ChecksumSpec, SimLabel, SimMessage};

    /// IO mock: a queue of incoming bit sequences plus a collector of sent
    /// sample chunks.
    struct MockIO {
        incoming: std::collections::VecDeque<Vec<Bit>>,
        sent: Vec<Vec<Complex32>>,
    }

    impl MockIO {
        fn new(incoming: Vec<Vec<Bit>>) -> Self {
            MockIO {
                incoming: incoming.into(),
                sent: Vec::new(),
            }
        }
    }

    impl SimulatorIO for MockIO {
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

    fn test_config() -> SimulatorConfiguration {
        let mut c = SimulatorConfiguration::new();
        c.participants = vec![
            SimParticipant {
                name: "Alice".to_string(),
                shortname: "A".to_string(),
                address_hex: String::new(),
                color_index: 0,
                relative_rssi: 0,
                id: "alice".to_string(),
                simulate: 0,
            },
            SimParticipant {
                name: "Bob".to_string(),
                shortname: "B".to_string(),
                address_hex: String::new(),
                color_index: 1,
                relative_rssi: 1,
                id: "bob".to_string(),
                simulate: 2,
            },
        ];
        c
    }

    #[test]
    fn test_counter_action() {
        let mut config = test_config();
        config.add_item(
            NodeKind::Counter {
                start: 3,
                step: 2,
                value: 3,
            },
            None,
        );
        let mut sim = Simulator::new(config, vec![SimModulator::default()]);
        let mut io = MockIO::new(vec![]);
        sim.run(&mut io).unwrap();

        let counter = sim
            .config
            .get_all_items()
            .into_iter()
            .find(|id| matches!(sim.config.node(*id).kind, NodeKind::Counter { .. }))
            .unwrap();
        match &sim.config.node(counter).kind {
            NodeKind::Counter { value, .. } => assert_eq!(*value, 5), // 3 + 2 after one pass
            _ => panic!(),
        }
        assert!(sim
            .log_messages
            .iter()
            .any(|m| m == "Increase counter by 2 to 5"));
    }

    #[test]
    fn test_goto_action_forward_jump_skips_item() {
        let mut config = test_config();
        // msg1 (sent), goto -> item4 (sleep), msg2 (skipped), sleep
        let mut m1 = SimMessage::new(bits("1010"), 0);
        m1.source = Some(1); // Bob (simulates)
        m1.destination = Some(0);
        config.add_item(NodeKind::Message(m1), None);
        config.add_item(
            NodeKind::Goto {
                goto_target: Some("item4".to_string()),
            },
            None,
        );
        let mut m2 = SimMessage::new(bits("0101"), 0);
        m2.source = Some(1);
        config.add_item(NodeKind::Message(m2), None);
        config.add_item(NodeKind::Sleep { sleep_time: 0.0 }, None);

        let mut sim = Simulator::new(config, vec![SimModulator::default()]);
        let mut io = MockIO::new(vec![]);
        sim.run(&mut io).unwrap();

        // msg1 sent, msg2 skipped by the goto jump
        assert_eq!(io.sent.len(), 1);
        assert!(sim.log_messages.iter().any(|m| m == "GOTO item 4"));
        assert!(sim.log_messages.iter().any(|m| m == "Sleep for 0.00 ns"));
    }

    #[test]
    fn test_goto_action_backward_loop_terminates_via_rule() {
        let mut config = test_config();
        // msg1 (sent), counter, rule: IF counter < 3 THEN goto msg1 ELSE end
        let mut m1 = SimMessage::new(bits("1010"), 0);
        m1.source = Some(1);
        m1.destination = Some(0);
        config.add_item(NodeKind::Message(m1), None);
        config.add_item(
            NodeKind::Counter {
                start: 1,
                step: 1,
                value: 1,
            },
            None,
        );
        let rule = config.add_item(NodeKind::Rule, None);
        let cond_if = config.add_item(
            NodeKind::RuleCondition {
                cond_type: ConditionType::If,
                condition: "item2.counter_value < 3".to_string(),
            },
            Some(rule),
        );
        config.add_item(
            NodeKind::Goto {
                goto_target: Some("item1".to_string()),
            },
            Some(cond_if),
        );

        let mut settings = SimulatorSettings::default();
        settings.num_repeats = 2;
        let mut sim = Simulator::new(config, vec![SimModulator::default()]);
        sim.settings = settings;
        let mut io = MockIO::new(vec![]);
        sim.run(&mut io).unwrap();

        // Round 1: msg1, counter->2, rule true -> goto -> msg1, counter->3,
        // rule false -> done. Round 2: msg1, counter->4, rule false.
        assert_eq!(io.sent.len(), 3);
        let counter = sim
            .config
            .get_all_items()
            .into_iter()
            .find(|id| matches!(sim.config.node(*id).kind, NodeKind::Counter { .. }))
            .unwrap();
        match &sim.config.node(counter).kind {
            NodeKind::Counter { value, .. } => assert_eq!(*value, 4),
            _ => panic!(),
        }
    }

    #[test]
    fn test_sleep_action_caption() {
        assert!(science_time(1.5).starts_with("1.50 s"));
        assert!(science_time(0.5).starts_with("500.00 ms"));
    }

    #[test]
    fn test_rule_true_false_branches() {
        let mut config = test_config();
        // msg1 is a RECEIVE message (source Alice, external) with a live data
        // label; rule: IF item1.data == 5 THEN message A ELSE message B.
        let mut m1 = SimMessage::new(bits("1111111111111111"), 0);
        m1.source = Some(0); // Alice (external) -> receive
        m1.destination = Some(1);
        let msg1 = config.add_item(NodeKind::Message(m1), None);
        {
            let mut lbl = SimLabel::new("data", 8, 12);
            lbl.value_type_index = 1; // live input: not checked on receive
            config.add_item(NodeKind::Label(lbl), Some(msg1));
        }

        let rule = config.add_item(NodeKind::Rule, None);
        let cond_if = config.add_item(
            NodeKind::RuleCondition {
                cond_type: ConditionType::If,
                condition: "item1.data == 5".to_string(),
            },
            Some(rule),
        );
        let mut ma = SimMessage::new(bits("0000000000000001"), 0);
        ma.source = Some(1); // Bob (simulates) -> send
        config.add_item(NodeKind::Message(ma), Some(cond_if));
        let cond_else = config.add_item(
            NodeKind::RuleCondition {
                cond_type: ConditionType::Else,
                condition: String::new(),
            },
            Some(rule),
        );
        let mut mb = SimMessage::new(bits("0000000000000010"), 0);
        mb.source = Some(1);
        config.add_item(NodeKind::Message(mb), Some(cond_else));

        // received bits 8..12 = 0101 = 5 -> IF branch -> message A sent
        let incoming = bits("111111110101");
        let mut sim = Simulator::new(config.clone(), vec![SimModulator::default()]);
        let mut io = MockIO::new(vec![incoming]);
        sim.run(&mut io).unwrap();
        assert_eq!(io.sent.len(), 1);
        let messages = sim.config.get_all_messages();
        assert_eq!(sim.config.message(messages[1]).unwrap().send_recv.len(), 1);
        assert_eq!(sim.config.message(messages[2]).unwrap().send_recv.len(), 0);
        assert!(sim
            .log_messages
            .iter()
            .any(|m| m == "Rule condition 2.1 (item1.data == 5) applied"));

        // received bits 8..12 = 0011 = 3 -> ELSE branch -> message B
        let mut sim2 = Simulator::new(config, vec![SimModulator::default()]);
        let mut io2 = MockIO::new(vec![bits("111111110011")]);
        sim2.run(&mut io2).unwrap();
        assert_eq!(io2.sent.len(), 1);
        let messages = sim2.config.get_all_messages();
        assert_eq!(sim2.config.message(messages[1]).unwrap().send_recv.len(), 0);
        assert_eq!(sim2.config.message(messages[2]).unwrap().send_recv.len(), 1);
    }

    #[test]
    fn test_receive_retry_and_error_handling_resend() {
        let mut config = test_config();
        let mut m1 = SimMessage::new(bits("1010"), 0);
        m1.source = Some(0); // Alice (external) -> receive
        m1.destination = Some(1);
        let msg1 = config.add_item(NodeKind::Message(m1), None);
        let mut lbl = SimLabel::new("data", 0, 4);
        lbl.value_type_index = 0;
        config.add_item(NodeKind::Label(lbl), Some(msg1));

        let mut settings = SimulatorSettings::default();
        settings.retries = 3;
        settings.error_handling_index = 0;
        let mut sim = Simulator::new(config, vec![SimModulator::default()]);
        sim.settings = settings;

        // First receive times out (None), then a matching message arrives.
        // The timeout triggers "Resend last message" (no last message yet).
        let mut io = MockIO2::new(vec![None, Some(bits("1010"))]);
        sim.run(&mut io).unwrap();
        assert!(sim.log_messages.iter().any(|m| m == "Receive timeout"));
        assert!(sim.log_messages.iter().any(|m| m == "Resending last message"));
        assert_eq!(
            sim.config.message(sim.config.get_all_messages()[0])
                .unwrap()
                .send_recv
                .len(),
            1
        );
    }

    /// Mock that models timeouts via Option layers.
    struct MockIO2 {
        queue: std::collections::VecDeque<Option<Vec<Bit>>>,
        sent: Vec<Vec<Complex32>>,
    }

    impl MockIO2 {
        fn new(queue: Vec<Option<Vec<Bit>>>) -> Self {
            MockIO2 {
                queue: queue.into(),
                sent: Vec::new(),
            }
        }
    }

    impl SimulatorIO for MockIO2 {
        fn receive(&mut self) -> Option<Vec<Bit>> {
            self.queue.pop_front().flatten()
        }
        fn send(&mut self, samples: &[Complex32]) {
            self.sent.push(samples.to_vec());
        }
    }

    #[test]
    fn test_message_not_received_stops() {
        let mut config = test_config();
        let mut m1 = SimMessage::new(bits("1010"), 0);
        m1.source = Some(0);
        config.add_item(NodeKind::Message(m1), None);

        let mut settings = SimulatorSettings::default();
        settings.retries = 2;
        let mut sim = Simulator::new(config, vec![SimModulator::default()]);
        sim.settings = settings;
        let mut io = MockIO2::new(vec![]);
        sim.run(&mut io).unwrap();
        assert!(sim
            .log_messages
            .iter()
            .any(|m| m == "Message 1 not received"));
    }

    #[test]
    fn test_checksum_label_sent_and_verified() {
        // Build a message with a 4-bit CRC over the first 4 bits.
        let mut config = test_config();
        let mut m = SimMessage::new(bits("10100000"), 0);
        m.source = Some(1); // simulator sends
        m.destination = Some(0);
        let msg = config.add_item(NodeKind::Message(m), None);
        {
            let lbl = SimLabel::new("data", 0, 4);
            config.add_item(NodeKind::Label(lbl), Some(msg));
        }
        {
            let mut crc = crate::encoding::GenericCRC::from_polynomial_bits(vec![1, 1, 1, 1, 0]);
            crc.start_value = vec![0, 0, 0, 0];
            crc.final_xor = vec![0, 0, 0, 0];
            let mut lbl = SimLabel::new("checksum", 4, 8);
            lbl.checksum = Some(ChecksumSpec {
                data_ranges: vec![(0, 4)],
                crc,
            });
            config.add_item(NodeKind::Label(lbl), Some(msg));
        }

        let mut sim = Simulator::new(config, vec![SimModulator::default()]);
        let mut io = MockIO::new(vec![]);
        sim.run(&mut io).unwrap();

        let sent = sim
            .config
            .message(sim.config.get_all_messages()[0])
            .unwrap()
            .send_recv[0]
            .clone();
        // checksum over 1010 with polynomial 11110 must equal bits 4..8
        let mut crc = crate::encoding::GenericCRC::from_polynomial_bits(vec![1, 1, 1, 1, 0]);
        crc.start_value = vec![0, 0, 0, 0];
        crc.final_xor = vec![0, 0, 0, 0];
        assert_eq!(bits_str(&sent[4..8]), bits_str(&crc.crc(&bits("1010"))));
    }

    #[test]
    fn test_fill_counter_values() {
        let mut config = test_config();
        config.add_item(
            NodeKind::Counter {
                start: 7,
                step: 1,
                value: 7,
            },
            None,
        );
        let sim = Simulator::new(config, vec![SimModulator::default()]);
        assert_eq!(
            sim.fill_counter_values("echo item1.counter_value items"),
            "echo 7 items"
        );
        assert_eq!(
            sim.fill_counter_values("echo item1.counter_valueitem1.counter_value"),
            "echo 77"
        );
        assert_eq!(sim.fill_counter_values("no counters here"), "no counters here");
    }

    #[test]
    fn test_trigger_command_is_skeleton() {
        let mut config = test_config();
        config.add_item(
            NodeKind::TriggerCommand {
                command: Some("touch /tmp/should-not-run".to_string()),
                pass_transcript: false,
                return_code: 0,
            },
            None,
        );
        let mut sim = Simulator::new(config, vec![SimModulator::default()]);
        let mut io = MockIO::new(vec![]);
        sim.run(&mut io).unwrap();
        assert!(sim
            .log_messages
            .iter()
            .any(|m| m.starts_with("Calling touch")));
        assert!(sim
            .log_messages
            .iter()
            .any(|m| m.contains("not supported")));
        assert!(!std::path::Path::new("/tmp/should-not-run").exists());
    }

    #[test]
    fn test_set_label_value_truncation_matches_python() {
        let mut config = test_config();
        let _ = &mut config;
        let mut sim = Simulator::new(config, vec![SimModulator::default()]);
        let mut bits = vec![0u8; 8];
        let lbl = SimLabel::new("x", 0, 8);
        // 257 = 100000001 -> Python keeps the first 8 chars "10000000" = 128
        sim.set_label_value(&mut bits, &lbl, 257).unwrap();
        assert_eq!(bits_str(&bits), "10000000");
        // 5 -> 00000101
        sim.set_label_value(&mut bits, &lbl, 5).unwrap();
        assert_eq!(bits_str(&bits), "00000101");
    }
}
