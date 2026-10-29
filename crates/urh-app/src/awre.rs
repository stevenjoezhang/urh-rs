//! AWRE field label service — v2 contract section "AWRE field labels (P4)"
//! (docs/ipc-contract.md).
//!
//! [`SignalRepository::run_awre`] reuses the analysis pipeline
//! ([`crate::analysis`]) on the requested signal/range, then runs
//! `urh::awre`'s `FormatFinder` over the resulting messages (the same flow
//! as the CLI's `participants` command) and expands the per-message-type
//! labels into flat, per-message [`FieldLabel`]s with **bit** positions:
//! the protocol table renders them directly, and the waveform converts bits
//! to samples via `samples_per_symbol`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};
use urh::awre::FormatFinder;
use urh::protocol::Message;

use crate::analysis::{
    analyze_samples, panic_message, validate_range_args, AnalysisParams, ValidatedParams,
};
use crate::{AppError, SignalRepository};

/// One AWRE-detected field within one message (contract: "AWRE field
/// labels"). `bit_start`/`bit_end` are bit indices into that message's
/// `plain_bits`, `bit_end` exclusive. `color_index` is the label's palette
/// slot inside its message type, passed through from the engine so the same
/// field kind keeps its color across messages.
///
/// Labels are deliberately **not** clamped to a message's bit count: the
/// engines assign one label layout per message *type*, and URH's own
/// protocol table renders it across all messages of that type unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldLabel {
    pub message_index: usize,
    /// Engine caption: "preamble", "synchronization", "length", "source
    /// address", "destination address", "sequence number", "type", "data",
    /// "checksum", … (whatever the AWRE engines found).
    pub name: String,
    pub bit_start: usize,
    /// Exclusive.
    pub bit_end: usize,
    pub color_index: usize,
}

/// Run the FormatFinder over analyzed messages and expand its output.
///
/// The finder assigns labels to message *types*; each type carries the
/// indices of the messages it covers, so every (type label, covered message)
/// pair becomes one [`FieldLabel`]. The result is sorted by
/// (message_index, bit_start, bit_end, name) for a stable wire order and
/// deduplicated, so a message referenced twice by the same type does not
/// yield repeated rows.
fn labels_from_messages(messages: Vec<Message>) -> Vec<FieldLabel> {
    let mut finder = FormatFinder::new(messages);
    finder.perform_analysis();

    let mut labels: Vec<FieldLabel> = Vec::new();
    for analyzed in finder.message_types() {
        for &message_index in &analyzed.message_indices {
            for label in &analyzed.message_type.labels {
                labels.push(FieldLabel {
                    message_index,
                    name: label.name.clone(),
                    bit_start: label.start,
                    bit_end: label.end,
                    color_index: label.color_index,
                });
            }
        }
    }
    labels.sort_by(|a, b| {
        (a.message_index, a.bit_start, a.bit_end, &a.name).cmp(&(
            b.message_index,
            b.bit_start,
            b.bit_end,
            &b.name,
        ))
    });
    labels.dedup_by(|a, b| {
        a.message_index == b.message_index
            && a.bit_start == b.bit_start
            && a.bit_end == b.bit_end
            && a.name == b.name
    });
    labels
}

impl SignalRepository {
    /// Detect protocol field labels with AWRE (contract: "AWRE field
    /// labels", v2 P4).
    ///
    /// `range` and `params` have exactly the meaning they have for
    /// [`SignalRepository::analyze`](crate::analysis). An empty analysis
    /// (no messages) yields an empty label list; analysis failures
    /// propagate unchanged.
    ///
    /// The FormatFinder predates the GUI and is known to panic on some
    /// real-world inputs (unequal message lengths trip a slice bounds check
    /// in `src/awre/util.rs::check_crc_for_messages`); such panics surface
    /// as `internal` errors instead of aborting the process (contract
    /// requirement 3).
    ///
    /// Blocking call: analysis plus the finder both run inline (several
    /// seconds on large signals); the Tauri shell must use `spawn_blocking`.
    pub fn run_awre(
        &self,
        signal_id: &str,
        range: Option<(u64, u64)>,
        params: &AnalysisParams,
    ) -> Result<Vec<FieldLabel>, AppError> {
        let signal = self.lookup(signal_id)?;
        validate_range_args(signal.sample_count() as u64, range)?;
        let params = ValidatedParams::validate(params)?;

        // AWRE has no job registry in the v2 contract; the never-set flag
        // simply supplies the shared pipeline's cancellation checkpoints.
        let flag = AtomicBool::new(false);
        let signal_for_job = signal.clone();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let outcome = analyze_samples(&signal_for_job, range, &params, &flag)?;
            if outcome.analyzer.messages.is_empty() {
                return Ok(Vec::<FieldLabel>::new());
            }
            Ok(labels_from_messages(outcome.analyzer.messages.clone()))
        }));
        match result {
            Ok(Ok(labels)) => Ok(labels),
            Ok(Err(err)) => Err(err),
            Err(panic) => Err(AppError::internal(format!(
                "AWRE field analysis panicked: {}",
                panic_message(panic.as_ref())
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::Signal;
    use urh::iq::IqArray;

    const ASK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/ask.complex");
    const FSK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/fsk.complex");
    const HOMEMATIC: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/homematic.complex32s"
    );
    const ELEKTROMATEN: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/elektromaten.complex16s"
    );

    fn open(repo: &SignalRepository, path: &str) -> String {
        repo.open(path, None).unwrap().signal_id
    }

    /// CLI `participants` golden: single-message files carry no labels.
    #[test]
    fn ask_and_fsk_have_no_labels() {
        let repo = SignalRepository::new();
        for path in [ASK, FSK] {
            let id = open(&repo, path);
            let labels = repo
                .run_awre(&id, None, &AnalysisParams::default())
                .unwrap();
            assert!(labels.is_empty(), "{path}: {labels:?}");
        }
    }

    /// CLI `participants` golden for homematic.complex32s: one message type
    /// covering both messages, with a 32-bit preamble followed by a 32-bit
    /// synchronization field.
    #[test]
    fn homematic_golden_labels() {
        let repo = SignalRepository::new();
        let id = open(&repo, HOMEMATIC);
        let labels = repo
            .run_awre(&id, None, &AnalysisParams::default())
            .unwrap();
        assert_eq!(
            labels,
            vec![
                FieldLabel {
                    message_index: 0,
                    name: "preamble".to_string(),
                    bit_start: 0,
                    bit_end: 32,
                    color_index: 0,
                },
                FieldLabel {
                    message_index: 0,
                    name: "synchronization".to_string(),
                    bit_start: 32,
                    bit_end: 64,
                    color_index: 1,
                },
                FieldLabel {
                    message_index: 1,
                    name: "preamble".to_string(),
                    bit_start: 0,
                    bit_end: 32,
                    color_index: 0,
                },
                FieldLabel {
                    message_index: 1,
                    name: "synchronization".to_string(),
                    bit_start: 32,
                    bit_end: 64,
                    color_index: 1,
                },
            ]
        );
    }

    /// The known FormatFinder panic (unequal message lengths in
    /// `check_crc_for_messages`, `src/awre/util.rs:413`) must surface as an
    /// `internal` error, not an abort.
    #[test]
    fn elektromaten_panic_becomes_internal_error() {
        let repo = SignalRepository::new();
        let id = open(&repo, ELEKTROMATEN);
        let err = repo
            .run_awre(&id, None, &AnalysisParams::default())
            .unwrap_err();
        assert_eq!(err.code, "internal", "{err}");
        assert!(err.message.contains("panicked"), "{err}");
    }

    #[test]
    fn empty_messages_produce_no_labels() {
        assert_eq!(labels_from_messages(Vec::new()), Vec::new());
    }

    #[test]
    fn analysis_errors_propagate() {
        let repo = SignalRepository::new();
        // Estimation failure (all-zero signal) comes through unchanged.
        let id = repo.alloc_id("sig-");
        let signal = Signal::new(
            id.clone(),
            "<memory>",
            IqArray::from_vec(vec![urh::Complex32::default(); 4096]),
            1e6,
        );
        repo.register(id.clone(), signal);
        let err = repo
            .run_awre(&id, None, &AnalysisParams::default())
            .unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{err}");

        // Validation errors share the analyze() codes.
        let ask = open(&repo, ASK);
        let err = repo
            .run_awre(&ask, Some((10, 5)), &AnalysisParams::default())
            .unwrap_err();
        assert_eq!(err.code, "invalid-range", "{err}");
        let err = repo
            .run_awre("sig-99", None, &AnalysisParams::default())
            .unwrap_err();
        assert_eq!(err.code, "unknown-signal");
    }
}
