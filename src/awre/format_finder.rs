//! Port of `urh/awre/FormatFinder.py`.
//!
//! Deviations from Python (see report):
//! - Messages reference their message type by *name* (crate `Message` has no
//!   typed back-reference), so existing message types are supplied via
//!   `with_types` and grouped by name instead of by object identity.
//! - Participants are not stored on messages; the constructor takes the
//!   per-message participant indices explicitly (or auto-assigns them).
//! - `FoundRange` wraps `CommonRange` / `ChecksumRange` where Python relies
//!   on subclassing.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::awre::auto_assigner::{auto_assign_participants, Participant};
use crate::awre::common_range::CommonRange;
use crate::awre::engines::address::AddressEngine;
use crate::awre::engines::checksum::ChecksumRange;
use crate::awre::engines::length::LengthEngine;
use crate::awre::engines::sequence_number::SequenceNumberEngine;
use crate::awre::engines::checksum::ChecksumEngine;
use crate::awre::engines::{bit_start, bit_end, length_in_bits, rng_eq, rng_lt, rng_overlaps};
use crate::awre::message_type_builder::{
    AnalyzedMessageType, ChecksumAnnotation, FieldFunction, TypedLabel,
};
use crate::awre::preprocessor::Preprocessor;
use crate::awre::util;
use crate::protocol::{Message, ProtocolLabel};

/// A field found by an engine: either a plain `CommonRange` or a
/// `ChecksumRange` (which carries the CRC data range information).
#[derive(Debug, Clone)]
pub enum FoundRange {
    Common(CommonRange),
    Checksum(ChecksumRange),
}

impl FoundRange {
    pub fn rng(&self) -> &CommonRange {
        match self {
            FoundRange::Common(r) => r,
            FoundRange::Checksum(r) => &r.range,
        }
    }

    fn rng_mut(&mut self) -> &mut CommonRange {
        match self {
            FoundRange::Common(r) => r,
            FoundRange::Checksum(r) => &mut r.range,
        }
    }

    fn bit_start(&self) -> usize {
        bit_start(self.rng())
    }

    fn bit_end(&self) -> usize {
        bit_end(self.rng())
    }

    fn field_type(&self) -> &str {
        &self.rng().field_type
    }

    fn score(&self) -> f64 {
        self.rng().score
    }

    /// Python set semantics: CommonRange hashes on (start, length,
    /// field_type), ChecksumRange additionally on data ranges and crc, so
    /// ranges only collapse inside their variant.
    fn same(a: &FoundRange, b: &FoundRange) -> bool {
        match (a, b) {
            (FoundRange::Common(x), FoundRange::Common(y)) => rng_eq(x, y),
            (FoundRange::Checksum(x), FoundRange::Checksum(y)) => ChecksumRange::eq_range(x, y),
            _ => false,
        }
    }
}

/// Port of `CommonRange.CommonRangeContainer`: the raw equivalent of a
/// message type — a container of common ranges.
#[derive(Debug, Clone)]
pub struct CommonRangeContainer {
    /// sorted by bit_start
    pub ranges: Vec<FoundRange>,
    pub message_indices: BTreeSet<usize>,
}

impl CommonRangeContainer {
    pub fn new(mut ranges: Vec<FoundRange>, message_indices: BTreeSet<usize>) -> Self {
        ranges.sort_by(|a, b| rng_lt(a.rng(), b.rng()));
        CommonRangeContainer {
            ranges,
            message_indices,
        }
    }

    pub fn ranges_overlap(&self) -> bool {
        self.ranges
            .iter()
            .any(|a| self.ranges.iter().any(|b| rng_overlaps(a.rng(), b.rng())))
    }

    pub fn has_same_ranges(&self, ranges: &[FoundRange]) -> bool {
        self.ranges.len() == ranges.len()
            && self
                .ranges
                .iter()
                .zip(ranges.iter())
                .all(|(a, b)| FoundRange::same(a, b))
    }

    pub fn has_same_ranges_as_container(&self, other: &CommonRangeContainer) -> bool {
        self.has_same_ranges(&other.ranges)
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

pub struct FormatFinder {
    pub messages: Vec<Message>,
    pub participants: Vec<Participant>,
    /// participant index per message (-1 = no participant)
    pub participant_indices: Vec<isize>,
    /// participant index -> address as nibble values
    pub known_participant_addresses: HashMap<usize, Vec<u8>>,
    pub preamble_starts: Vec<usize>,
    pub preamble_lengths: Vec<usize>,
    pub sync_ends: Vec<usize>,
    /// plain bits behind the synchronization
    pub bitvectors: Vec<Vec<u8>>,
    /// nibble vectors behind the synchronization
    pub hexvectors: Vec<Vec<u8>>,
    pub existing_message_types: Vec<AnalyzedMessageType>,
    pub current_iteration: usize,
}

impl FormatFinder {
    pub const MIN_MESSAGES_PER_CLUSTER: usize = 2;

    /// `FormatFinder(messages)` — no participants; existing message types
    /// are derived from the messages' type names.
    pub fn new(messages: Vec<Message>) -> Self {
        let n = messages.len();
        Self::with_types_and_participants(messages, Vec::new(), Vec::new(), vec![-1; n])
    }

    pub fn with_types(messages: Vec<Message>, types: Vec<AnalyzedMessageType>) -> Self {
        let n = messages.len();
        Self::with_types_and_participants(messages, types, Vec::new(), vec![-1; n])
    }

    /// Explicit participant assignment: `participant_indices[i]` indexes
    /// into `participants` (-1 = no participant for that message).
    pub fn with_participants(
        messages: Vec<Message>,
        types: Vec<AnalyzedMessageType>,
        participants: Vec<Participant>,
        participant_indices: Vec<isize>,
    ) -> Self {
        assert_eq!(messages.len(), participant_indices.len());
        Self::with_types_and_participants(messages, types, participants, participant_indices)
    }

    /// Python `FormatFinder(messages, participants)`: participants are
    /// auto-assigned to the messages (by SRC address label / RSSI).
    pub fn with_auto_assignment(
        messages: Vec<Message>,
        types: Vec<AnalyzedMessageType>,
        mut participants: Vec<Participant>,
    ) -> Self {
        let participant_indices = auto_assign_participants(&messages, &mut participants, &types);
        Self::with_types_and_participants(messages, types, participants, participant_indices)
    }

    fn with_types_and_participants(
        messages: Vec<Message>,
        types: Vec<AnalyzedMessageType>,
        participants: Vec<Participant>,
        participant_indices: Vec<isize>,
    ) -> Self {
        // group the messages by their message type name
        let mut existing_message_types: Vec<AnalyzedMessageType> = Vec::new();
        for (i, msg) in messages.iter().enumerate() {
            match existing_message_types
                .iter_mut()
                .find(|mt| mt.message_type.name == msg.message_type)
            {
                Some(mt) => mt.message_indices.push(i),
                None => {
                    let mut mt = types
                        .iter()
                        .find(|t| t.message_type.name == msg.message_type)
                        .cloned()
                        .unwrap_or_else(|| AnalyzedMessageType::new(&msg.message_type));
                    mt.message_indices = vec![i];
                    existing_message_types.push(mt);
                }
            }
        }

        // Preprocessor on the full bitvectors; already labeled sync words
        // are passed directly (Python reaches them through the message types)
        let full_bitvectors: Vec<Vec<u8>> =
            Self::get_bitvectors_from_messages(&messages, None);
        let existing_sync_words = Self::get_existing_sync_words(
            &full_bitvectors,
            &messages,
            &existing_message_types,
        );
        let preprocessor = Preprocessor::new(full_bitvectors, existing_sync_words);
        let (preamble_starts, mut preamble_lengths, sync_len) = preprocessor.preprocess();

        let mut sync_ends: Vec<usize> = (0..messages.len())
            .map(|i| preamble_starts[i] + preamble_lengths[i] + sync_len)
            .collect();

        let n = if sync_len >= 8 {
            8
        } else if sync_len >= 4 {
            4
        } else if sync_len >= 1 {
            1
        } else {
            0
        };

        for i in 0..sync_ends.len() {
            // In doubt it is better to under estimate the sync end
            if n > 0 {
                let steps = ((sync_ends[i] - preamble_starts[i]) / n).max(1);
                sync_ends[i] = n * steps + preamble_starts[i];
            } else {
                sync_ends[i] = preamble_starts[i];
            }

            if sync_ends[i] - preamble_starts[i] < preamble_lengths[i] {
                preamble_lengths[i] = sync_ends[i] - preamble_starts[i];
            }
        }

        let bitvectors = Self::get_bitvectors_from_messages(&messages, Some(&sync_ends));
        let hexvectors: Vec<Vec<u8>> = util::get_hexvectors(&bitvectors)
            .into_iter()
            .map(|v| v.into_iter().map(|n| n as u8).collect())
            .collect();

        let mut known_participant_addresses = HashMap::new();
        for (pi, p) in participants.iter().enumerate() {
            if !p.address_hex.is_empty() {
                known_participant_addresses.insert(
                    pi,
                    p.address_hex
                        .chars()
                        .filter_map(|c| c.to_digit(16).map(|d| d as u8))
                        .collect(),
                );
            }
        }

        FormatFinder {
            messages,
            participants,
            participant_indices,
            known_participant_addresses,
            preamble_starts,
            preamble_lengths,
            sync_ends,
            bitvectors,
            hexvectors,
            existing_message_types,
            current_iteration: 0,
        }
    }

    /// Message types sorted by name (Python `message_types` property).
    pub fn message_types(&self) -> Vec<&AnalyzedMessageType> {
        let mut result: Vec<&AnalyzedMessageType> =
            self.existing_message_types.iter().collect();
        result.sort_by(|a, b| a.message_type.name.cmp(&b.message_type.name));
        result
    }

    /// Port of `Preprocessor.__get_existing_sync_words`.
    fn get_existing_sync_words(
        bitvectors: &[Vec<u8>],
        messages: &[Message],
        message_types: &[AnalyzedMessageType],
    ) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        for (i, bitvector) in bitvectors.iter().enumerate() {
            let sync_label = message_types
                .iter()
                .find(|mt| mt.message_type.name == messages[i].message_type)
                .and_then(|mt| mt.get_first_label_with_type(FieldFunction::Sync));
            if let Some(lbl) = sync_label {
                let end = lbl.label.end.min(bitvector.len());
                if lbl.label.start < end {
                    result.push(bitvector[lbl.label.start..end].to_vec());
                }
            }
        }
        result
    }

    pub fn get_bitvectors_from_messages(
        messages: &[Message],
        sync_ends: Option<&[usize]>,
    ) -> Vec<Vec<u8>> {
        messages
            .iter()
            .enumerate()
            .map(|(i, msg)| {
                let start = sync_ends.map_or(0, |e| e[i]);
                msg.plain_bits[start.min(msg.plain_bits.len())..].to_vec()
            })
            .collect()
    }

    /// Python `run(max_iterations=10)` — the full analysis pipeline.
    pub fn perform_analysis(&mut self) {
        self.perform_analysis_with_max_iterations(10);
    }

    pub fn perform_analysis_with_max_iterations(&mut self, max_iterations: usize) {
        self.current_iteration = 0;
        while self.perform_iteration() && self.current_iteration < max_iterations {
            self.current_iteration += 1;
        }

        if !self.message_types().is_empty() {
            let covered: HashSet<usize> = self
                .existing_message_types
                .iter()
                .flat_map(|mt| mt.message_indices.iter().copied())
                .collect();
            let without_message_type: Vec<usize> = (0..self.bitvectors.len())
                .filter(|i| !covered.contains(i))
                .collect();

            // add to default (first) message type
            let first_name = self.message_types()[0].message_type.name.clone();
            let first_id = self.message_types()[0].message_type.id.clone();
            if let Some(mt) = self
                .existing_message_types
                .iter_mut()
                .find(|mt| mt.message_type.id == first_id && mt.message_type.name == first_name)
            {
                mt.message_indices.extend(without_message_type);
            }
        }
    }

    /// Perform a field inference iteration for all message types.
    /// Returns whether a new field was found.
    pub fn perform_iteration(&mut self) -> bool {
        let mut new_field_found = false;

        // snapshot: Python iterates a copy of the dict while deleting keys
        let snapshot: Vec<(String, String)> = self
            .existing_message_types
            .iter()
            .map(|mt| (mt.message_type.id.clone(), mt.message_type.name.clone()))
            .collect();

        for (type_id, _type_name) in snapshot {
            let Some(pos) = self
                .existing_message_types
                .iter()
                .position(|mt| mt.message_type.id == type_id)
            else {
                continue;
            };

            let message_type = self.existing_message_types[pos].clone();
            let mut new_fields_for_message_type =
                self.perform_iteration_for_message_type(&message_type);

            new_fields_for_message_type.extend(Self::get_preamble_and_sync(
                &self.preamble_starts,
                &self.preamble_lengths,
                &self.sync_ends,
                &self.existing_message_types[pos].message_indices,
            ));

            // set semantics (dedup)
            new_fields_for_message_type = dedup_ranges(new_fields_for_message_type);

            Self::remove_overlapping_fields(
                &mut new_fields_for_message_type,
                &message_type,
            );

            let containers =
                Self::create_common_range_containers(&new_fields_for_message_type, None);

            // Store addresses of participants if we found a SRC address field
            let mut participants_with_unknown_address: BTreeSet<isize> =
                self.participant_indices.iter().copied().collect();
            for &p in self.known_participant_addresses.keys() {
                participants_with_unknown_address.remove(&(p as isize));
            }
            participants_with_unknown_address.remove(&(-1));

            if !participants_with_unknown_address.is_empty() {
                'outer: for container in &containers {
                    let Some(src_range) = container
                        .ranges
                        .iter()
                        .find(|rng| rng.field_type() == "source address")
                    else {
                        continue;
                    };
                    for &msg_index in &src_range.rng().message_indices {
                        if participants_with_unknown_address.is_empty() {
                            break 'outer;
                        }
                        let p = self.participant_indices[msg_index];
                        let pu = p as usize;
                        if p >= 0 && !self.known_participant_addresses.contains_key(&pu) {
                            let hex_vector = &self.hexvectors[msg_index];
                            let end = (src_range.rng().end() + 1).min(hex_vector.len());
                            let start = src_range.rng().start.min(end);
                            self.known_participant_addresses
                                .insert(pu, hex_vector[start..end].to_vec());
                            participants_with_unknown_address.remove(&p);
                        }
                    }
                }
            }

            new_field_found |= !containers.is_empty();

            if containers.len() == 1 {
                let mt = &mut self.existing_message_types[pos];
                for rng in &containers[0].ranges {
                    Self::add_range_to_message_type(rng, mt);
                }
            } else if containers.len() > 1 {
                self.existing_message_types.remove(pos);

                for (i, container) in containers.iter().enumerate() {
                    let mut new_message_type = message_type.clone();
                    new_message_type.message_indices = Vec::new();

                    if i > 0 {
                        new_message_type.message_type.name = format!(
                            "Message Type {}.{}",
                            self.current_iteration + 1,
                            i
                        );
                        new_message_type.message_type.id =
                            format!("{}-{}", message_type.message_type.id, i);
                    }

                    for rng in &container.ranges {
                        Self::add_range_to_message_type(rng, &mut new_message_type);
                    }

                    new_message_type
                        .message_indices
                        .extend(container.message_indices.iter().copied());

                    self.existing_message_types.push(new_message_type);
                }
            }
        }

        new_field_found
    }

    /// Perform a field inference iteration for messages of the given message
    /// type. Returns the newly found fields.
    fn perform_iteration_for_message_type(
        &self,
        message_type: &AnalyzedMessageType,
    ) -> Vec<FoundRange> {
        let indices = &message_type.message_indices;

        // We can take an arbitrary sync end to correct the already labeled
        // fields for this message type, because if the existing labels would
        // have different sync positions, they would not belong to the same
        // message type in the first place
        let sync_end = indices.first().map_or(0, |&i| self.sync_ends[i]);
        let already_labeled: Vec<(usize, usize)> = message_type
            .typed_labels
            .iter()
            .filter(|tl| tl.label.start >= sync_end)
            .map(|tl| (tl.label.start - sync_end, tl.label.end - sync_end))
            .collect();

        let bvs: Vec<Vec<u8>> = indices.iter().map(|&i| self.bitvectors[i].clone()).collect();
        let hexvs: Vec<Vec<u8>> =
            indices.iter().map(|&i| self.hexvectors[i].clone()).collect();
        let pidx: Vec<isize> = indices
            .iter()
            .map(|&i| self.participant_indices[i])
            .collect();

        let mut found: Vec<FoundRange> = Vec::new();

        if message_type
            .get_first_label_with_type(FieldFunction::Length)
            .is_none()
        {
            let engine = LengthEngine::new(&bvs, already_labeled.clone());
            for rng in engine.find(8, 0.1) {
                found.push(FoundRange::Common(rng));
            }
        }

        if message_type
            .get_first_label_with_type(FieldFunction::SrcAddress)
            .is_none()
        {
            let engine = AddressEngine::new(
                &hexvs,
                &pidx,
                Some(&self.known_participant_addresses),
                &already_labeled,
                false,
            );
            for rng in engine.find() {
                found.push(FoundRange::Common(rng));
            }
        } else if message_type
            .get_first_label_with_type(FieldFunction::DstAddress)
            .is_none()
        {
            let engine = AddressEngine::new(
                &hexvs,
                &pidx,
                Some(&self.known_participant_addresses),
                &already_labeled,
                true, // src_field_present
            );
            for rng in engine.find() {
                found.push(FoundRange::Common(rng));
            }
        }

        if message_type
            .get_first_label_with_type(FieldFunction::SequenceNumber)
            .is_none()
        {
            let engine = SequenceNumberEngine::new(&bvs, 8, 0.75, &already_labeled);
            for rng in engine.find() {
                found.push(FoundRange::Common(rng));
            }
        }

        if message_type
            .get_first_label_with_type(FieldFunction::Checksum)
            .is_none()
        {
            // If checksum was not found in first iteration, it will also not
            // be found in next one
            if self.current_iteration == 0 {
                let engine = ChecksumEngine::new(&bvs, 8, 0.9, &already_labeled);
                for rng in engine.find() {
                    found.push(FoundRange::Checksum(rng));
                }
            }
        }

        let retransformed =
            Self::retransform_message_indices(found, indices, &self.sync_ends);
        Self::merge_common_ranges(retransformed)
    }

    /// Remove all fields which overlap with fields of the existing message type.
    fn remove_overlapping_fields(common_ranges: &mut Vec<FoundRange>, message_type: &AnalyzedMessageType) {
        if message_type.typed_labels.is_empty() {
            return;
        }

        common_ranges.retain(|rng| {
            !message_type.typed_labels.iter().any(|lbl| {
                // Python: any(i in range(rng.bit_start, rng.bit_end)
                //            for i in range(lbl.start, lbl.end))  (half-open;
                // crate label `end` is exclusive like Python's)
                std::cmp::max(lbl.label.start, rng.bit_start())
                    < std::cmp::min(rng.bit_end(), lbl.label.end)
            })
        });
    }

    /// Merge common ranges if possible.
    fn merge_common_ranges(common_ranges: Vec<FoundRange>) -> Vec<FoundRange> {
        let mut merged_ranges: Vec<FoundRange> = Vec::new();
        for common_range in common_ranges {
            match merged_ranges
                .iter_mut()
                .find(|rng| {
                    rng.bit_start() == common_range.bit_start()
                        && rng.bit_end() == common_range.bit_end()
                        && rng.field_type() == common_range.field_type()
                }) {
                Some(same_range) => {
                    same_range
                        .rng_mut()
                        .values
                        .extend(common_range.rng().values.iter().cloned());
                    same_range
                        .rng_mut()
                        .message_indices
                        .extend(common_range.rng().message_indices.iter().copied());
                }
                None => merged_ranges.push(common_range),
            }
        }
        merged_ranges
    }

    /// Port of `add_range_to_message_type`.
    fn add_range_to_message_type(common_range: &FoundRange, message_type: &mut AnalyzedMessageType) {
        let field_type = FieldFunction::from_caption(common_range.field_type())
            .unwrap_or(FieldFunction::Custom);
        let rng = common_range.rng();

        let label = ProtocolLabel::new(
            common_range.field_type(),
            bit_start(rng),
            bit_end(rng),
            message_type.len(),
        );
        let mut typed = TypedLabel {
            label,
            function: field_type,
            checksum: None,
        };

        if field_type == FieldFunction::Checksum {
            if let FoundRange::Checksum(cr) = common_range {
                typed.checksum = Some(ChecksumAnnotation {
                    data_ranges: vec![(
                        cr.data_range_start + rng.sync_end,
                        cr.data_range_end + rng.sync_end,
                    )],
                    checksum: cr.crc.clone(),
                });
            }
        }

        message_type.add_typed_label(typed, true);
    }

    /// Port of `create_common_range_containers`.
    pub fn create_common_range_containers(
        label_set: &[FoundRange],
        num_messages: Option<usize>,
    ) -> Vec<CommonRangeContainer> {
        let message_indices: Vec<usize> = match num_messages {
            None => {
                let mut s: BTreeSet<usize> = BTreeSet::new();
                for rng in label_set {
                    s.extend(rng.rng().message_indices.iter().copied());
                }
                s.into_iter().collect()
            }
            Some(n) => (0..n).collect(),
        };

        let mut result: Vec<CommonRangeContainer> = Vec::new();
        for i in message_indices {
            let mut labels: Vec<FoundRange> = Vec::new();
            for rng in label_set {
                if rng.rng().message_indices.contains(&i) && !labels.iter().any(|l| FoundRange::same(l, rng)) {
                    labels.push(rng.clone());
                }
            }
            labels.sort_by(|a, b| rng_lt(a.rng(), b.rng()));

            match result
                .iter_mut()
                .find(|container| container.has_same_ranges(&labels))
            {
                Some(container) => {
                    container.message_indices.insert(i);
                }
                None => {
                    result.push(CommonRangeContainer::new(labels, BTreeSet::from([i])));
                }
            }
        }

        Self::handle_overlapping_conflict(result)
    }

    /// Handle overlapping conflicts for a list of containers.
    pub fn handle_overlapping_conflict(
        containers: Vec<CommonRangeContainer>,
    ) -> Vec<CommonRangeContainer> {
        let mut result: Vec<CommonRangeContainer> = Vec::new();
        for container in containers {
            let conflicted_handled = if container.ranges_overlap() {
                Self::handle_container_overlapping_conflict(&container)
            } else {
                container
            };

            match result
                .iter_mut()
                .find(|c| c.has_same_ranges_as_container(&conflicted_handled))
            {
                Some(same_rng_container) => {
                    same_rng_container
                        .message_indices
                        .extend(conflicted_handled.message_indices.iter().copied());
                }
                None => result.push(conflicted_handled),
            }
        }
        result
    }

    /// Handle the overlapping conflict for a single container:
    /// 1. choose the range with the highest score
    /// 2. if multiple ranges overlap choose the ranges that maximize the
    ///    overall (cumulated) score
    fn handle_container_overlapping_conflict(
        container: &CommonRangeContainer,
    ) -> CommonRangeContainer {
        // partition the container into overlapping partitions
        let mut partitions: Vec<Vec<FoundRange>> = Vec::new();
        for cur_rng in container.ranges.clone() {
            match partitions.last_mut() {
                None => partitions.push(vec![cur_rng]),
                Some(last_partition) => {
                    let last_rng = &last_partition[last_partition.len() - 1];
                    if rng_overlaps(last_rng.rng(), cur_rng.rng()) {
                        last_partition.push(cur_rng);
                    } else {
                        partitions.push(vec![cur_rng]);
                    }
                }
            }
        }

        let mut result: Vec<FoundRange> = Vec::new();
        for partition in partitions {
            let mut possible_solutions: Vec<Vec<FoundRange>> = Vec::new();
            for (i, rng) in partition.iter().enumerate() {
                // append every range to this solution that does not overlap
                // with the current rng
                let mut solution = vec![rng.clone()];
                solution.extend(
                    partition[i + 1..]
                        .iter()
                        .filter(|r| !rng_overlaps(rng.rng(), r.rng()))
                        .cloned(),
                );
                possible_solutions.push(solution);
            }

            // Take the solution that maximizes the score. In case of tie,
            // choose the solution with shorter total length; if there is
            // still a tie prefer the solution that contains a length field;
            // if nothing else helps break the tie by names of the field types.
            let mut best: Option<(Vec<FoundRange>, (f64, i64, bool, String))> = None;
            for solution in possible_solutions {
                let key = solution_key(&solution);
                if best.is_none() || key_greater(&key, &best.as_ref().unwrap().1) {
                    best = Some((solution, key));
                }
            }
            result.extend(best.unwrap().0);
        }

        CommonRangeContainer::new(result, container.message_indices.clone())
    }

    /// Retransform the found message indices of an engine to the original
    /// index space based on the message indices of the message type; sets
    /// the sync_end so bit_start/bit_end match the original space.
    pub fn retransform_message_indices(
        common_ranges: Vec<FoundRange>,
        message_type_indices: &[usize],
        sync_ends: &[usize],
    ) -> Vec<FoundRange> {
        let mut result = Vec::new();
        for common_range in common_ranges {
            let message_indices: Vec<usize> = common_range
                .rng()
                .message_indices
                .iter()
                .map(|&i| message_type_indices[i])
                .collect();

            // if we have different sync_ends we need to create a new common
            // range for each different sync_length
            let mut sync_ends_grouped: Vec<(usize, BTreeSet<usize>)> = Vec::new();
            for &mi in &message_indices {
                let se = sync_ends[mi];
                match sync_ends_grouped.iter_mut().find(|(s, _)| *s == se) {
                    Some((_, set)) => {
                        set.insert(mi);
                    }
                    None => sync_ends_grouped.push((se, BTreeSet::from([mi]))),
                }
            }

            for (sync_end, indices) in sync_ends_grouped {
                let mut rng = common_range.clone();
                rng.rng_mut().sync_end = sync_end;
                rng.rng_mut().message_indices = indices;
                result.push(rng);
            }
        }
        result
    }

    /// Get preamble and sync common ranges based on the data.
    fn get_preamble_and_sync(
        preamble_starts: &[usize],
        preamble_lengths: &[usize],
        sync_ends: &[usize],
        message_type_indices: &[usize],
    ) -> Vec<FoundRange> {
        let mut result: Vec<CommonRange> = Vec::new();

        for &i in message_type_indices {
            let mut preamble = CommonRange::new(preamble_starts[i], preamble_lengths[i]);
            preamble.field_type = "preamble".to_string();
            preamble.message_indices.insert(i);

            match result
                .iter_mut()
                .find(|rng| rng_eq(rng, &preamble))
            {
                Some(existing) => {
                    existing.message_indices.insert(i);
                }
                None => {
                    if preamble_lengths[i] > 0 {
                        result.push(preamble);
                    }
                }
            }

            let preamble_end = preamble_starts[i] + preamble_lengths[i];
            let sync_end = sync_ends[i];
            let mut sync = CommonRange::new(preamble_end, sync_end - preamble_end);
            sync.field_type = "synchronization".to_string();
            sync.message_indices.insert(i);

            match result.iter_mut().find(|rng| rng_eq(rng, &sync)) {
                Some(existing) => {
                    existing.message_indices.insert(i);
                }
                None => {
                    if sync_end - preamble_end > 0 {
                        result.push(sync);
                    }
                }
            }
        }

        result.into_iter().map(FoundRange::Common).collect()
    }
}

fn dedup_ranges(ranges: Vec<FoundRange>) -> Vec<FoundRange> {
    let mut result: Vec<FoundRange> = Vec::new();
    for rng in ranges {
        if !result.iter().any(|r| FoundRange::same(r, &rng)) {
            result.push(rng);
        }
    }
    result
}

fn solution_key(solution: &[FoundRange]) -> (f64, i64, bool, String) {
    let score: f64 = solution.iter().map(|r| r.score()).sum();
    let total_len: i64 = solution.iter().map(|r| length_in_bits(r.rng()) as i64).sum();
    let has_length = solution.iter().any(|r| r.field_type() == "length");
    let first_chars: String = solution
        .iter()
        .map(|r| r.field_type().chars().next().unwrap_or('\0'))
        .collect();
    (score, -total_len, has_length, first_chars)
}

fn key_greater(a: &(f64, i64, bool, String), b: &(f64, i64, bool, String)) -> bool {
    a.0.partial_cmp(&b.0)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(a.1.cmp(&b.1))
        .then(a.2.cmp(&b.2))
        .then(a.3.cmp(&b.3))
        == std::cmp::Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::str2bits;

    fn make_range(
        start: usize,
        length: usize,
        value: &str,
        score: f64,
        field_type: &str,
        message_indices: &[usize],
    ) -> FoundRange {
        let mut rng = CommonRange::new(start, length);
        rng.values = vec![str2bits(value)];
        rng.score = score;
        rng.field_type = field_type.to_string();
        rng.message_indices = message_indices.iter().copied().collect();
        FoundRange::Common(rng)
    }

    fn unwrap_common(r: &FoundRange) -> &CommonRange {
        match r {
            FoundRange::Common(c) => c,
            FoundRange::Checksum(_) => panic!("expected common range"),
        }
    }

    /// test_format_finder.py::test_create_message_types_1
    #[test]
    fn test_create_message_types_1() {
        let rng1 = make_range(0, 8, "1".repeat(8).as_str(), 1.0, "Length", &[0, 1, 2]);
        let rng2 = make_range(8, 8, "1".repeat(8).as_str(), 1.0, "Address", &[0, 1, 2]);

        let message_types =
            FormatFinder::create_common_range_containers(&[rng1.clone(), rng2.clone()], None);
        assert_eq!(message_types.len(), 1);

        let expected = CommonRangeContainer::new(
            vec![rng1, rng2],
            [0, 1, 2].into_iter().collect(),
        );
        assert!(message_types[0].has_same_ranges_as_container(&expected));
    }

    /// test_format_finder.py::test_create_message_types_2
    #[test]
    fn test_create_message_types_2() {
        let rng1 = make_range(0, 8, "1".repeat(8).as_str(), 1.0, "Length", &[0, 2, 4, 6, 8, 12]);
        let rng2 = make_range(8, 8, "1".repeat(8).as_str(), 1.0, "Address", &[1, 2, 3, 4, 5, 12]);
        let rng3 = make_range(16, 8, "1".repeat(8).as_str(), 1.0, "Seq", &[1, 3, 5, 7, 12]);

        let message_types = FormatFinder::create_common_range_containers(
            &[rng1.clone(), rng2.clone(), rng3.clone()],
            None,
        );
        let expected1 = CommonRangeContainer::new(vec![rng1.clone()], [0, 6, 8].into_iter().collect());
        let expected2 = CommonRangeContainer::new(
            vec![rng1.clone(), rng2.clone()],
            [2, 4].into_iter().collect(),
        );
        let expected3 = CommonRangeContainer::new(
            vec![rng1.clone(), rng2.clone(), rng3.clone()],
            [12].into_iter().collect(),
        );
        let expected4 = CommonRangeContainer::new(
            vec![rng2.clone(), rng3.clone()],
            [1, 3, 5].into_iter().collect(),
        );
        let expected5 = CommonRangeContainer::new(vec![rng3.clone()], [7].into_iter().collect());

        assert_eq!(message_types.len(), 5);

        for expected in &[&expected1, &expected2, &expected3, &expected4, &expected5] {
            assert!(
                message_types
                    .iter()
                    .any(|mt| mt.has_same_ranges_as_container(expected)),
                "missing {:?}",
                expected.ranges.iter().map(|r| r.field_type().to_string()).collect::<Vec<_>>()
            );
        }
    }

    /// test_format_finder.py::test_retransform_message_indices
    #[test]
    fn test_retransform_message_indices() {
        let sync_ends = [12, 12, 12, 14, 14];

        let rng = make_range(0, 8, "1".repeat(8).as_str(), 1.0, "length", &[0, 1, 2, 3, 4]);
        let retransformed_ranges =
            FormatFinder::retransform_message_indices(vec![rng], &[0, 1, 2, 3, 4], &sync_ends);

        // two different sync ends
        assert_eq!(retransformed_ranges.len(), 2);

        // Python compares with CommonRange equality (bit_start/bit_end/field_type)
        let expected1 = make_range(12, 8, "1".repeat(8).as_str(), 1.0, "length", &[0, 1, 2]);
        let expected2 = make_range(14, 8, "1".repeat(8).as_str(), 1.0, "length", &[3, 4]);

        for expected in [&expected1, &expected2] {
            assert!(retransformed_ranges
                .iter()
                .any(|r| rng_eq(unwrap_common(r), unwrap_common(expected))));
        }
    }

    /// test_format_finder.py::test_handle_no_overlapping_conflict
    #[test]
    fn test_handle_no_overlapping_conflict() {
        let rng1 = make_range(0, 8, "1".repeat(8).as_str(), 1.0, "Length", &[0, 1, 2]);
        let rng2 = make_range(8, 8, "1".repeat(8).as_str(), 1.0, "Address", &[0, 1, 2]);

        let container =
            CommonRangeContainer::new(vec![rng1.clone(), rng2.clone()], [0, 1, 2].into_iter().collect());

        // no conflict
        let result = FormatFinder::handle_overlapping_conflict(vec![container]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 2);
        assert!(result[0].ranges.iter().any(|r| FoundRange::same(r, &rng1)));
        assert_eq!(
            result[0].message_indices,
            [0, 1, 2].into_iter().collect()
        );
        assert!(result[0].ranges.iter().any(|r| FoundRange::same(r, &rng2)));
    }

    /// test_format_finder.py::test_handle_easy_overlapping_conflict
    #[test]
    fn test_handle_easy_overlapping_conflict() {
        // Easy conflict: First Label has higher score
        let rng1 = make_range(8, 8, "1".repeat(8).as_str(), 1.0, "Length", &[0, 1, 2]);
        let rng2 = make_range(8, 8, "1".repeat(8).as_str(), 0.8, "Address", &[0, 1, 2]);

        let container =
            CommonRangeContainer::new(vec![rng1.clone(), rng2.clone()], [0, 1, 2].into_iter().collect());
        let result = FormatFinder::handle_overlapping_conflict(vec![container]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 1);
        assert!(result[0].ranges.iter().any(|r| FoundRange::same(r, &rng1)));
        assert_eq!(result[0].message_indices, [0, 1, 2].into_iter().collect());
    }

    /// test_format_finder.py::test_handle_medium_overlapping_conflict
    #[test]
    fn test_handle_medium_overlapping_conflict() {
        let rng1 = make_range(8, 8, "1".repeat(8).as_str(), 1.0, "Length", &[]);
        let rng2 = make_range(4, 10, "1".repeat(8).as_str(), 0.8, "Address", &[]);
        let rng3 = make_range(15, 20, "1".repeat(8).as_str(), 1.0, "Seq", &[]);
        let rng4 = make_range(60, 80, "1".repeat(8).as_str(), 0.8, "Type", &[]);
        let rng5 = make_range(70, 90, "1".repeat(8).as_str(), 0.9, "Data", &[]);

        let container = CommonRangeContainer::new(
            vec![rng1.clone(), rng2.clone(), rng3.clone(), rng4.clone(), rng5.clone()],
            [0].into_iter().collect(),
        );
        let result = FormatFinder::handle_overlapping_conflict(vec![container]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 3);
        assert!(result[0].ranges.iter().any(|r| FoundRange::same(r, &rng1)));
        assert!(result[0].ranges.iter().any(|r| FoundRange::same(r, &rng3)));
        assert!(result[0].ranges.iter().any(|r| FoundRange::same(r, &rng5)));
    }
}

#[cfg(test)]
mod partially_labeled_tests {
    use super::*;
    use crate::awre::auto_assigner::Participant;
    use crate::awre::message_type_builder::{
        AnalyzedMessageType, FieldFunction, MessageTypeBuilder, TypedLabel,
    };
    use crate::awre::protocol_generator::{test_util, ProtocolGenerator};
    use crate::protocol::ProtocolLabel;

    fn prepare_example_protocol() -> (ProtocolGenerator, Participant, Participant) {
        let alice = Participant::with_address("Alice", "1234");
        let bob = Participant::with_address("Bob", "cafe");

        let mut mb = MessageTypeBuilder::new("data");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        mb.add_label(FieldFunction::Type, 8, None);
        mb.add_label(FieldFunction::DstAddress, 16, None);
        mb.add_label(FieldFunction::SrcAddress, 16, None);
        let mut mb_ack = MessageTypeBuilder::new("ack");
        mb_ack.add_label(FieldFunction::Preamble, 8, None);
        mb_ack.add_label(FieldFunction::Sync, 16, None);
        mb_ack.add_label(FieldFunction::Length, 8, None);
        mb_ack.add_label(FieldFunction::DstAddress, 16, None);

        let num_messages = 50;
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone(), mb_ack.message_type.clone()],
            vec![alice.clone(), bob.clone()],
            &[],
            &["0x6768", "0x6768"],
            false,
            true,
        );

        let mut rng = test_util::Rng::new(0);
        for i in 0..num_messages {
            let (source, destination, data_length) = if i % 2 == 0 {
                (&alice, &bob, 8usize)
            } else {
                (&bob, &alice, 16)
            };
            let value = rng.next_below(1 << (data_length - 1));
            let data = pg.decimal_to_bits(value, data_length);
            pg.generate_message(None, &data, Some(source), Some(destination));
            pg.generate_message(Some(1), "", Some(destination), Some(source));
        }

        (pg, alice, bob)
    }

    fn prepare_simple_example_protocol() -> (ProtocolGenerator, Participant, Participant) {
        let alice = Participant::with_address("Alice", "1234");
        let bob = Participant::with_address("Bob", "cafe");

        let mut mb = MessageTypeBuilder::new("data");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        mb.add_label(FieldFunction::Type, 8, None);
        mb.add_label(FieldFunction::DstAddress, 16, None);
        mb.add_label(FieldFunction::SrcAddress, 16, None);

        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![alice.clone(), bob.clone()],
            &[],
            &["0x6768"],
            false,
            true,
        );

        let mut rng = test_util::Rng::new(0);
        for _ in 0..10 {
            let data = rng.random_bits(16);
            pg.generate_message(None, &data, Some(&alice), Some(&bob));
            let data = rng.random_bits(8);
            pg.generate_message(None, &data, Some(&bob), Some(&alice));
        }

        (pg, alice, bob)
    }

    fn labels_equal(mt1: &AnalyzedMessageType, mt2: &AnalyzedMessageType) -> bool {
        if mt1.message_type.labels.len() != mt2.message_type.labels.len() {
            return false;
        }
        mt1.message_type
            .labels
            .iter()
            .zip(mt2.message_type.labels.iter())
            .all(|(l1, l2)| {
                l1.start == l2.start && l1.end == l2.end && l1.name == l2.name
            })
    }

    /// test_partially_labeled.py::test_fully_labeled
    #[test]
    fn test_fully_labeled() {
        let (pg, alice, bob) = prepare_example_protocol();
        let messages = pg.messages.clone();
        let types = pg.message_types.clone();

        let mut message_types: Vec<AnalyzedMessageType> = types.clone();
        message_types.sort_by(|a, b| a.message_type.name.cmp(&b.message_type.name));

        let mut ff = FormatFinder::with_participants(
            messages,
            types,
            vec![alice, bob],
            pg.participant_indices.clone(),
        );
        ff.perform_iteration();
        let ff_types = ff.message_types();
        assert_eq!(message_types.len(), ff_types.len());

        for (mt1, mt2) in message_types.iter().zip(ff_types.iter()) {
            assert!(labels_equal(mt1, mt2));
        }
    }

    /// test_partially_labeled.py::test_one_message_type_empty
    #[test]
    fn test_one_message_type_empty() {
        let (pg, alice, bob) = prepare_example_protocol();
        let n_message_types = pg.message_types.len();
        let messages = pg.messages.clone();

        let mut types = pg.message_types.clone();
        let ack_mt = types.iter_mut().find(|mt| mt.message_type.name == "ack").unwrap();
        ack_mt.clear();
        assert_eq!(ack_mt.len(), 0);

        let mut ff = FormatFinder::with_participants(
            messages,
            types,
            vec![alice, bob],
            pg.participant_indices.clone(),
        );
        ff.perform_iteration();
        assert_eq!(n_message_types, ff.message_types().len());

        let ack = ff
            .existing_message_types
            .iter()
            .find(|mt| mt.message_type.name == "ack")
            .unwrap();
        assert_eq!(ack.len(), 4, "{:?}", ack.message_type.labels);
    }

    /// test_partially_labeled.py::test_given_address_information
    #[test]
    fn test_given_address_information() {
        let (pg, alice, bob) = prepare_example_protocol();
        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);

        let mut ff = FormatFinder::with_participants(
            messages,
            vec![],
            vec![alice, bob],
            pg.participant_indices.clone(),
        );
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(2, message_types.len());

        assert!(message_types[0].get_first_label_with_type(FieldFunction::Preamble).is_some());
        assert!(message_types[1].get_first_label_with_type(FieldFunction::Preamble).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::Sync).is_some());
        assert!(message_types[1].get_first_label_with_type(FieldFunction::Sync).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::Length).is_some());
        assert!(message_types[1].get_first_label_with_type(FieldFunction::Length).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::DstAddress).is_some());
        assert!(message_types[1].get_first_label_with_type(FieldFunction::DstAddress).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::SrcAddress).is_none());
        assert!(message_types[1].get_first_label_with_type(FieldFunction::SrcAddress).is_some());
    }

    fn prepare_cleared_simple_ff() -> (FormatFinder, ProtocolGenerator, Participant, Participant) {
        let (pg, alice, bob) = prepare_simple_example_protocol();
        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let ff = FormatFinder::with_participants(
            messages,
            vec![],
            vec![alice.clone(), bob.clone()],
            pg.participant_indices.clone(),
        );
        (ff, pg, alice, bob)
    }

    fn add_plain_label(ff: &mut FormatFinder, start: usize, length: usize) {
        // MessageType.add_protocol_label_start_length: plain, untyped label
        let mt = &mut ff.existing_message_types[0];
        let label = ProtocolLabel::new("Label #0", start, start + length - 1, 0);
        mt.add_typed_label(
            TypedLabel {
                label,
                function: FieldFunction::Custom,
                checksum: None,
            },
            true,
        );
    }

    /// test_partially_labeled.py::test_type_part_already_labeled
    #[test]
    fn test_type_part_already_labeled() {
        let (mut ff, _pg, _a, _b) = prepare_cleared_simple_ff();

        // overlaps type
        add_plain_label(&mut ff, 32, 8);
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(1, message_types.len());

        assert!(message_types[0].get_first_label_with_type(FieldFunction::Preamble).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::Sync).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::Length).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::DstAddress).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::SrcAddress).is_some());
    }

    /// test_partially_labeled.py::test_length_part_already_labeled
    #[test]
    fn test_length_part_already_labeled() {
        let (mut ff, _pg, _a, _b) = prepare_cleared_simple_ff();

        // overlaps length
        add_plain_label(&mut ff, 24, 8);
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(1, message_types.len());

        assert!(message_types[0].get_first_label_with_type(FieldFunction::Preamble).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::Sync).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::Length).is_none());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::DstAddress).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::SrcAddress).is_some());
    }

    /// test_partially_labeled.py::test_address_part_already_labeled
    #[test]
    fn test_address_part_already_labeled() {
        let (mut ff, _pg, _a, _b) = prepare_cleared_simple_ff();

        // overlaps dst address
        add_plain_label(&mut ff, 40, 16);
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(1, message_types.len());

        assert!(message_types[0].get_first_label_with_type(FieldFunction::Preamble).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::Sync).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::Length).is_some());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::DstAddress).is_none());
        assert!(message_types[0].get_first_label_with_type(FieldFunction::SrcAddress).is_some());
    }
}
