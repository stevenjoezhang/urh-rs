//! Port of `urh/awre/engines/AddressEngine.py`.
//!
//! Deviations from Python:
//! - `addresses_by_participant` mixes candidate sets, single assigned
//!   addresses, `None` and the broadcast entry (key -1) in one dict; here it
//!   is an `AddrVal` enum and "deleted" entries are simply absent.
//! - Python iterates dicts/sets in insertion order where we use sorted
//!   (BTreeMap/BTreeSet) order; this only affects tie-breaking.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::awre::common_range::{CommonRange, RangeType};
use crate::awre::engines::{
    bit_start, find_common_ranges_by_cluster, find_longest_common_sub_sequences,
    ignore_already_labeled, rng_matches, rng_lt, Clusters,
};
use crate::awre::util;

/// Value of `addresses_by_participant[p]` during `find`.
enum AddrVal {
    /// candidate addresses (Python: list / set / dict)
    Set(BTreeSet<Vec<u8>>),
    /// one assigned address (Python: raw bytes) or the broadcast address
    Single(Vec<u8>),
}

/// An address candidate found in a message. All address ranges use
/// `range_type = hex` and hexvector coordinates.
#[derive(Clone)]
struct AddrRange {
    rng: CommonRange,
}

impl AddrRange {
    fn value(&self) -> &[u8] {
        self.rng.values.first().map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Python set membership of CommonRange: equal by (bit_start, bit_end,
    /// field_type), which for these "Generic" hex ranges means (start, length).
    fn same_pos(a: &AddrRange, b: &AddrRange) -> bool {
        a.rng.start == b.rng.start && a.rng.length == b.rng.length
    }
}

pub struct AddressEngine<'a> {
    /// message data behind synchronization (hexvectors: nibble values)
    msg_vectors: &'a [Vec<u8>],
    /// participant index per message (-1 = no participant)
    participant_indices: &'a [isize],
    known_addresses_by_participant: HashMap<usize, Vec<u8>>,
    /// already labeled ranges, converted to hex coordinates
    already_labeled: Vec<(usize, usize)>,
    src_field_present: bool,
    minimum_score: f64,
    message_indices_by_participant: BTreeMap<isize, Vec<usize>>,
}

impl<'a> AddressEngine<'a> {
    pub fn new(
        msg_vectors: &'a [Vec<u8>],
        participant_indices: &'a [isize],
        known_participant_addresses: Option<&HashMap<usize, Vec<u8>>>,
        already_labeled: &[(usize, usize)],
        src_field_present: bool,
    ) -> Self {
        assert_eq!(msg_vectors.len(), participant_indices.len());

        // convert already labeled ranges to hex coordinates
        let already_labeled = already_labeled
            .iter()
            .map(|&(start, end)| ((start + 3) / 4, (end + 3) / 4))
            .collect();

        let mut message_indices_by_participant: BTreeMap<isize, Vec<usize>> = BTreeMap::new();
        for (i, &participant_index) in participant_indices.iter().enumerate() {
            message_indices_by_participant
                .entry(participant_index)
                .or_default()
                .push(i);
        }

        AddressEngine {
            msg_vectors,
            participant_indices,
            known_addresses_by_participant: known_participant_addresses
                .cloned()
                .unwrap_or_default(),
            already_labeled,
            src_field_present,
            minimum_score: 0.1,
            message_indices_by_participant,
        }
    }

    fn cross_swap_check(rng1: &AddrRange, rng2: &AddrRange) -> bool {
        (rng1.rng.start == rng2.rng.start + rng1.rng.length
            || rng1.rng.start + rng1.rng.length == rng2.rng.start)
            && rng1.value() == rng2.value()
    }

    fn ack_check(rng1: &AddrRange, rng2: &AddrRange) -> bool {
        rng1.rng.start == rng2.rng.start
            && rng1.rng.length == rng2.rng.length
            && rng1.value() != rng2.value()
    }

    pub fn find(&self) -> Vec<CommonRange> {
        let mut addresses_by_participant: BTreeMap<isize, AddrVal> = self
            .known_addresses_by_participant
            .iter()
            .map(|(&p, addr)| (p as isize, AddrVal::Single(addr.clone())))
            .collect();
        for (p, addrs) in self.find_addresses() {
            match addresses_by_participant.entry(p) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(AddrVal::Set(addrs));
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    if let AddrVal::Set(s) = o.get_mut() {
                        s.extend(addrs);
                    }
                }
            }
        }

        // Find the address candidates by participant in messages and create
        // common ranges over matching positions
        let addresses_by_entry: Vec<Vec<Vec<u8>>> = addresses_by_participant
            .values()
            .map(|v| match v {
                AddrVal::Set(s) => s.iter().cloned().collect(),
                AddrVal::Single(a) => vec![a.clone()],
            })
            .collect();

        let already_labeled_cols: Vec<usize> = self
            .already_labeled
            .iter()
            .flat_map(|&(start, end)| start..end)
            .collect();

        let mut ranges_by_participant: BTreeMap<isize, Vec<AddrRange>> = BTreeMap::new();
        for (i, msg_vector) in self.msg_vectors.iter().enumerate() {
            let participant = self.participant_indices[i];
            for address in addresses_by_entry.iter().flatten() {
                for index in util::find_occurrences(
                    msg_vector,
                    address,
                    Some(&already_labeled_cols),
                    false,
                ) {
                    let common_ranges = ranges_by_participant.entry(participant).or_default();
                    match common_ranges
                        .iter_mut()
                        .find(|ar| rng_matches(&ar.rng, index, address))
                    {
                        Some(ar) => {
                            ar.rng.message_indices.insert(i);
                        }
                        None => {
                            let mut rng = CommonRange::new(index, address.len());
                            rng.values = vec![address.clone()];
                            rng.message_indices.insert(i);
                            rng.range_type = RangeType::Hex;
                            common_ranges.push(AddrRange { rng });
                        }
                    }
                }
            }
        }

        let mut num_messages_by_participant: BTreeMap<isize, usize> = BTreeMap::new();
        for &participant in self.participant_indices {
            *num_messages_by_participant.entry(participant).or_insert(0) += 1;
        }

        // Look for cross swapped values between participant clusters.
        // Scores are mutated in place like in Python (the ack check divisor
        // reads possibly already updated scores).
        let participants: Vec<isize> = ranges_by_participant.keys().copied().collect();
        for w in participants.windows(2) {
            let (p1, p2) = (w[0], w[1]);
            let n1 = num_messages_by_participant.get(&p1).copied().unwrap_or(1) as f64;
            let n2 = num_messages_by_participant.get(&p2).copied().unwrap_or(1) as f64;

            for i1 in 0..ranges_by_participant[&p1].len() {
                for i2 in 0..ranges_by_participant[&p2].len() {
                    let (rng1, rng2) = {
                        let r1 = &ranges_by_participant[&p1][i1];
                        let r2 = &ranges_by_participant[&p2][i2];
                        if !(ranges_by_participant[&p2].iter().any(|r| AddrRange::same_pos(r, r1))
                            && ranges_by_participant[&p1].iter().any(|r| AddrRange::same_pos(r, r2)))
                        {
                            continue;
                        }
                        (r1.clone(), r2.clone())
                    };

                    if Self::cross_swap_check(&rng1, &rng2) {
                        let add = rng2.rng.message_indices.len() as f64 / n2;
                        ranges_by_participant.get_mut(&p1).unwrap()[i1].rng.score += add;
                        let add = rng1.rng.message_indices.len() as f64 / n1;
                        ranges_by_participant.get_mut(&p2).unwrap()[i2].rng.score += add;
                    } else if Self::ack_check(&rng1, &rng2) {
                        // add previous score in divisor to add bonus to
                        // ranges that apply to all messages
                        let prev = rng1.rng.score;
                        let add = rng2.rng.message_indices.len() as f64 / (n2 + prev);
                        ranges_by_participant.get_mut(&p1).unwrap()[i1].rng.score += add;
                        let prev = rng2.rng.score;
                        let add = rng1.rng.message_indices.len() as f64 / (n1 + prev);
                        ranges_by_participant.get_mut(&p2).unwrap()[i2].rng.score += add;
                    }
                }
            }
        }

        if ranges_by_participant.len() == 1 && !self.src_field_present {
            let single_participants: Vec<isize> =
                ranges_by_participant.keys().copied().collect();
            for &p in &single_participants {
                let Some(known) = self.known_addresses_by_participant.get(&(p as usize)) else {
                    continue;
                };
                let mut sorted: Vec<AddrRange> =
                    ranges_by_participant[&p].clone();
                sorted.sort_by(|a, b| rng_lt(&a.rng, &b.rng));
                for rng in sorted {
                    if rng.value() == known.as_slice() {
                        // only one participant in this iteration and address
                        // already known -> highscore
                        let idx = ranges_by_participant[&p]
                            .iter()
                            .position(|r| AddrRange::same_pos(r, &rng))
                            .unwrap();
                        ranges_by_participant.get_mut(&p).unwrap()[idx].rng.score = 1.0;
                        break; // take only the first (leftmost) range
                    }
                }
            }
        }

        let mut high_scored_ranges_by_participant: BTreeMap<isize, Vec<CommonRange>> =
            BTreeMap::new();

        let address_length = self.estimate_address_length(&ranges_by_participant);

        // Get highscored ranges by participant
        for (&participant, common_ranges) in ranges_by_participant.iter() {
            let mut sorted_ranges: Vec<AddrRange> = common_ranges
                .iter()
                .filter(|cr| cr.rng.score > self.minimum_score)
                .cloned()
                .collect();
            sorted_ranges.sort_by(|a, b| {
                b.rng.score
                    .partial_cmp(&a.rng.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| rng_lt(&a.rng, &b.rng))
            });

            if sorted_ranges.is_empty() {
                addresses_by_participant.insert(participant, AddrVal::Set(BTreeSet::new()));
                continue;
            }

            // keep only candidate addresses with the estimated length
            let kept: BTreeSet<Vec<u8>> = match addresses_by_participant.get(&participant) {
                Some(AddrVal::Set(s)) => s
                    .iter()
                    .filter(|a| a.len() == address_length)
                    .cloned()
                    .collect(),
                Some(AddrVal::Single(a)) => {
                    if a.len() == address_length {
                        BTreeSet::from([a.clone()])
                    } else {
                        BTreeSet::new()
                    }
                }
                None => BTreeSet::new(),
            };
            addresses_by_participant.insert(participant, AddrVal::Set(kept));

            let mut high_scored = Vec::new();
            for mut rng in sorted_ranges
                .into_iter()
                .map(|ar| ar.rng)
                .filter(|r| r.length == address_length)
            {
                rng.score = rng.score.min(1.0);
                high_scored.push(rng);
            }
            high_scored_ranges_by_participant.insert(participant, high_scored);
        }

        // Now we find the most probable address for all participants
        self.assign_participant_addresses(
            &mut addresses_by_participant,
            &high_scored_ranges_by_participant,
        );

        // Eliminate participants for which we could not assign an address
        // (Python stores None and deletes those entries afterwards; after
        // assign_participant_addresses every remaining entry is Single)
        addresses_by_participant.retain(|_, v| matches!(v, AddrVal::Single(_)));

        // Now we can separate SRC and DST
        let mut separated: BTreeMap<isize, Vec<CommonRange>> = BTreeMap::new();
        for (&participant, ranges) in high_scored_ranges_by_participant.iter() {
            let address: Option<Vec<u8>> = match addresses_by_participant.get(&participant) {
                Some(AddrVal::Single(a)) => Some(a.clone()),
                _ => None,
            };

            let Some(address) = address else {
                separated.insert(participant, Vec::new());
                continue;
            };

            let mut result: Vec<CommonRange> = Vec::new();
            for mut rng in ranges.iter().cloned() {
                rng.field_type = if rng.values.first().map(|v| v.as_slice()) == Some(address.as_slice()) {
                    "source address".to_string()
                } else {
                    "destination address".to_string()
                };

                if result.is_empty() {
                    result.push(rng);
                } else {
                    let subset = result
                        .iter()
                        .find(|r| rng.message_indices.is_subset(&r.message_indices));
                    if let Some(subset) = subset {
                        if rng.field_type == subset.field_type {
                            // avoid adding same address type twice
                            continue;
                        }
                        if rng.length != subset.length
                            || (rng.start != subset.end() + 1 && rng.end() + 1 != subset.start)
                        {
                            // ensure addresses are next to each other
                            continue;
                        }
                    }
                    result.push(rng);
                }
            }
            separated.insert(participant, result);
        }

        self.find_broadcast_fields(
            &mut separated,
            &mut addresses_by_participant,
        );

        let mut result: Vec<CommonRange> = separated.into_values().flatten().collect();

        // If we did not find a SRC address, lower the score a bit, so DST
        // fields do not win later e.g. against length fields in case of tie
        if !result.iter().any(|rng| rng.field_type == "source address") {
            for rng in result.iter_mut() {
                rng.score *= 0.95;
            }
        }

        result
    }

    /// Estimate the address length which is assumed to be the same for all
    /// participants.
    fn estimate_address_length(
        &self,
        ranges_by_participant: &BTreeMap<isize, Vec<AddrRange>>,
    ) -> usize {
        let mut address_lengths: Vec<usize> = Vec::new();

        for common_ranges in ranges_by_participant.values() {
            let mut sorted_ranges: Vec<AddrRange> = common_ranges
                .iter()
                .filter(|cr| cr.rng.score > self.minimum_score)
                .cloned()
                .collect();
            sorted_ranges.sort_by(|a, b| {
                b.rng.score
                    .partial_cmp(&a.rng.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| rng_lt(&a.rng, &b.rng))
            });

            if sorted_ranges.is_empty() {
                continue;
            }

            let mut max_scored: Vec<AddrRange> = sorted_ranges
                .iter()
                .filter(|r| r.rng.score == sorted_ranges[0].rng.score)
                .cloned()
                .collect();

            // Prevent overestimation of address length by looking for substrings
            for rng in max_scored.clone() {
                let same_message_rng: Vec<AddrRange> = sorted_ranges
                    .iter()
                    .filter(|r| {
                        !max_scored.iter().any(|m| AddrRange::same_pos(m, r))
                            && r.rng.score > 0.0
                            && r.rng.message_indices == rng.rng.message_indices
                    })
                    .cloned()
                    .collect();

                if same_message_rng.len() > 1
                    && same_message_rng
                        .iter()
                        .all(|r| contains(rng.value(), r.value()))
                {
                    // remove the longer range and add the smaller ones
                    max_scored.retain(|m| !AddrRange::same_pos(m, &rng));
                    max_scored.extend(same_message_rng);
                }
            }

            let possible_address_lengths: Vec<usize> =
                max_scored.iter().map(|r| r.rng.length).collect();

            // Take the most common one. On tie, take the shorter one.
            let mut distinct = possible_address_lengths.clone();
            distinct.sort_unstable();
            distinct.dedup();
            if let Some(&addr_len) = distinct
                .iter()
                .max_by(|a, b| {
                    let ca = possible_address_lengths.iter().filter(|l| *l == *a).count();
                    let cb = possible_address_lengths.iter().filter(|l| *l == *b).count();
                    ca.cmp(&cb).then(b.cmp(a))
                })
            {
                address_lengths.push(addr_len);
            }
        }

        // Take most common address length of participants, to ensure they all
        // have same address length. On tie, take the shorter one.
        let mut distinct = address_lengths.clone();
        distinct.sort_unstable();
        distinct.dedup();
        distinct
            .iter()
            .max_by(|a, b| {
                let ca = address_lengths.iter().filter(|l| *l == *a).count();
                let cb = address_lengths.iter().filter(|l| *l == *b).count();
                ca.cmp(&cb).then(b.cmp(a))
            })
            .copied()
            .unwrap_or(0)
    }

    fn assign_participant_addresses(
        &self,
        addresses_by_participant: &mut BTreeMap<isize, AddrVal>,
        high_scored_ranges_by_participant: &BTreeMap<isize, Vec<CommonRange>>,
    ) {
        let mut scored: BTreeMap<isize, Vec<(Vec<u8>, f64)>> = addresses_by_participant
            .keys()
            .map(|&p| (p, Vec::new()))
            .collect();

        for (&participant, addresses) in addresses_by_participant.iter() {
            if let Some(known) = self
                .known_addresses_by_participant
                .get(&(participant as usize))
            {
                scored
                    .get_mut(&participant)
                    .unwrap()
                    .push((known.clone(), 9999999999.0));
                continue;
            }

            let addr_set: BTreeSet<Vec<u8>> = match addresses {
                AddrVal::Set(s) => s.clone(),
                AddrVal::Single(a) => BTreeSet::from([a.clone()]),
            };

            for &i in self
                .message_indices_by_participant
                .get(&participant)
                .into_iter()
                .flatten()
            {
                let value_in_set =
                    |rng: &CommonRange| -> bool {
                        rng.values
                            .first()
                            .map(|v| v.as_slice())
                            .map(|v| addr_set.iter().any(|a| a.as_slice() == v))
                            .unwrap_or(false)
                    };

                let matching: Vec<&CommonRange> = high_scored_ranges_by_participant
                    .get(&participant)
                    .into_iter()
                    .flatten()
                    .filter(|rng| rng.message_indices.contains(&i) && value_in_set(rng))
                    .collect();

                if matching.len() == 1 {
                    let address = matching[0].values.first().cloned().unwrap_or_default();
                    // only one address, so probably a destination and not a
                    // source; Python `d[address] *= 0.9` on a defaultdict
                    // leaves a missing key at 0
                    mul_score(&mut scored, participant, &address, 0.9);

                    // Since this is probably an ACK, the address is probably
                    // SRC of the participant of the previous message
                    if i > 0 && self.participant_indices[i - 1] != participant {
                        let prev_participant = self.participant_indices[i - 1];
                        let prev_matching: Vec<&CommonRange> = high_scored_ranges_by_participant
                            .get(&prev_participant)
                            .into_iter()
                            .flatten()
                            .filter(|rng| {
                                rng.message_indices.contains(&(i - 1)) && value_in_set(rng)
                            })
                            .collect();
                        if prev_matching.len() > 1 {
                            for prev_rng in prev_matching
                                .iter()
                                .filter(|r| {
                                    r.values.first().map(|v| v.as_slice())
                                        == Some(address.as_slice())
                                })
                            {
                                add_score(&mut scored, prev_participant, &address, prev_rng.score);
                            }
                        }
                    }
                } else if matching.len() > 1 {
                    // more than one address, so there must be a source
                    // address included
                    for rng in matching {
                        let address = rng.values.first().cloned().unwrap_or_default();
                        add_score(&mut scored, participant, &address, rng.score);
                    }
                }
            }
        }

        let minimum_score = 0.5;
        let mut taken_addresses: Vec<Vec<u8>> = Vec::new();

        // If all participants have exactly one possible address and they all
        // differ, we can assign them right away
        if !scored.is_empty()
            && scored.values().all(|addresses| addresses.len() == 1)
        {
            let all_addresses: Vec<Vec<u8>> =
                scored.values().map(|a| a[0].0.clone()).collect();
            let mut unique = all_addresses.clone();
            unique.sort();
            unique.dedup();
            if unique.len() == all_addresses.len() {
                for (&p, addresses) in scored.iter() {
                    addresses_by_participant
                        .insert(p, AddrVal::Single(addresses[0].0.clone()));
                }
                return;
            }
        }

        for (&participant, addresses) in scored.iter() {
            // sort filtered results descending to prevent randomness for
            // equal scores; max by score with first-wins ties
            let mut candidates: Vec<&(Vec<u8>, f64)> = addresses
                .iter()
                .filter(|(a, s)| !taken_addresses.contains(a) && *s >= minimum_score)
                .collect();
            candidates.sort_by(|a, b| b.0.cmp(&a.0));

            let found = candidates
                .iter()
                .enumerate()
                .max_by(|(ia, a), (ib, b)| {
                    a.1.partial_cmp(&b.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(ib.cmp(ia))
                })
                .map(|(_, c)| c);

            match found {
                None => {
                    // could not assign an address for this participant
                    addresses_by_participant
                        .insert(participant, AddrVal::Set(BTreeSet::new()));
                }
                Some(&(ref address, _)) => {
                    taken_addresses.push(address.clone());
                    addresses_by_participant
                        .insert(participant, AddrVal::Single(address.clone()));
                }
            }
        }
    }

    /// Check for messages that were sent to broadcast:
    /// 1. search for messages that have a SRC address but no DST address
    /// 2. look at other messages that have this SRC field and find the
    ///    corresponding DST position
    /// 3. evaluate the value of the messages without DST from 1 and compare
    ///    these values with each other; if they match, we found the broadcast
    fn find_broadcast_fields(
        &self,
        high_scored_ranges_by_participant: &mut BTreeMap<isize, Vec<CommonRange>>,
        addresses_by_participant: &mut BTreeMap<isize, AddrVal>,
    ) {
        if matches!(addresses_by_participant.get(&-1), Some(AddrVal::Single(_))) {
            // broadcast address is already known
            return;
        }

        // (participant, dst index in that participant's range list, message)
        let mut broadcast_bag: Vec<(isize, usize, usize)> = Vec::new();
        for (&participant, common_ranges) in high_scored_ranges_by_participant.iter() {
            let mut src_address_fields: Vec<usize> = common_ranges
                .iter()
                .enumerate()
                .filter(|(_, r)| r.field_type == "source address")
                .map(|(i, _)| i)
                .collect();
            src_address_fields
                .sort_by_key(|&i| (bit_start(&common_ranges[i]), common_ranges[i].length));

            let mut dst_address_fields: Vec<usize> = common_ranges
                .iter()
                .enumerate()
                .filter(|(_, r)| r.field_type == "destination address")
                .map(|(i, _)| i)
                .collect();
            dst_address_fields
                .sort_by_key(|&i| (bit_start(&common_ranges[i]), common_ranges[i].length));

            let msg_with_dst: BTreeSet<usize> = dst_address_fields
                .iter()
                .flat_map(|&i| common_ranges[i].message_indices.iter().copied())
                .collect();

            for &src_idx in &src_address_fields {
                let msg_without_dst: BTreeSet<usize> = common_ranges[src_idx]
                    .message_indices
                    .iter()
                    .copied()
                    .filter(|i| !msg_with_dst.contains(i))
                    .collect();
                if msg_without_dst.is_empty() {
                    continue;
                }

                let with_dst: BTreeSet<usize> = common_ranges[src_idx]
                    .message_indices
                    .difference(&msg_without_dst)
                    .copied()
                    .collect();

                let Some(&matching_dst) = dst_address_fields
                    .iter()
                    .find(|&&dst_idx| common_ranges[dst_idx]
                        .message_indices
                        .is_superset(&with_dst))
                else {
                    continue;
                };
                for &msg in &msg_without_dst {
                    broadcast_bag.push((participant, matching_dst, msg));
                }
            }
        }

        if broadcast_bag.is_empty() {
            return;
        }

        let mut broadcast_address: Option<Vec<u8>> = None;
        for &(participant, dst_idx, msg_index) in &broadcast_bag {
            let dst = &high_scored_ranges_by_participant[&participant][dst_idx];
            let from = dst.start.min(self.msg_vectors[msg_index].len());
            let to = (dst.end() + 1).min(self.msg_vectors[msg_index].len());
            let value = if from < to {
                self.msg_vectors[msg_index][from..to].to_vec()
            } else {
                Vec::new()
            };
            match &broadcast_address {
                None => broadcast_address = Some(value),
                Some(b) if *b == value => {}
                // address is not common across messages so it can't be a
                // broadcast address
                Some(_) => return,
            }
        }

        let broadcast_address = broadcast_address.unwrap_or_default();
        addresses_by_participant.insert(-1, AddrVal::Single(broadcast_address.clone()));
        for &(participant, dst_idx, msg) in &broadcast_bag {
            let dst = &mut high_scored_ranges_by_participant
                .get_mut(&participant)
                .unwrap()[dst_idx];
            dst.values.push(broadcast_address.clone());
            dst.message_indices.insert(msg);
        }
    }

    /// Port of `find_addresses`: find address candidates from the common
    /// ranges between participant clusters.
    pub fn find_addresses(&self) -> BTreeMap<isize, BTreeSet<Vec<u8>>> {
        let mut result: BTreeMap<isize, BTreeSet<Vec<u8>>> = BTreeMap::new();

        let already_assigned: Vec<usize> = self
            .known_addresses_by_participant
            .keys()
            .copied()
            .collect();
        if already_assigned.len() == self.message_indices_by_participant.len() {
            // skipping find addresses as they are already known
            return result;
        }

        let mut common_ranges_by_participant: BTreeMap<isize, Vec<CommonRange>> = BTreeMap::new();
        for (&participant, message_indices) in self.message_indices_by_participant.iter() {
            // Cluster by length
            let mut length_clusters: Clusters<usize> = Vec::new();
            for &i in message_indices {
                let len = self.msg_vectors[i].len();
                match length_clusters.iter_mut().find(|(l, _)| *l == len) {
                    Some((_, v)) => v.push(i),
                    None => length_clusters.push((len, vec![i])),
                }
            }

            let common_ranges_by_length = find_common_ranges_by_cluster(
                self.msg_vectors,
                &length_clusters,
                0.95,
                RangeType::Hex,
            );

            let mut ranges = Vec::new();
            for (_, cluster_ranges) in common_ranges_by_length {
                ranges.extend(ignore_already_labeled(
                    &cluster_ranges,
                    &self.already_labeled,
                ));
            }
            common_ranges_by_participant.insert(participant, ranges);
        }

        let participants: Vec<isize> = common_ranges_by_participant.keys().copied().collect();
        if participants.len() < 2 {
            return result;
        }

        // If we already know the address length we do not need to bother
        // with other candidates
        let addr_len: Option<usize> = if !already_assigned.is_empty() {
            Some(self.known_addresses_by_participant[&already_assigned[0]].len())
        } else {
            None
        };

        for w in participants.windows(2) {
            let (p1, p2) = (w[0], w[1]);
            let p1_already_assigned = already_assigned.contains(&(p1 as usize));
            let p2_already_assigned = already_assigned.contains(&(p2 as usize));

            if p1_already_assigned && p2_already_assigned {
                continue;
            }

            // common ranges are not merged yet, so there is only one element
            // in values
            let values1: Vec<Vec<u8>> = common_ranges_by_participant[&p1]
                .iter()
                .filter_map(|cr| cr.values.first().cloned())
                .collect();
            let values2: Vec<Vec<u8>> = common_ranges_by_participant[&p2]
                .iter()
                .filter_map(|cr| cr.values.first().cloned())
                .collect();

            for seq1 in &values1 {
                for seq2 in &values2 {
                    let lcs = find_longest_common_sub_sequences(Some(seq1), Some(seq2));
                    let vals: Vec<Vec<u8>> = if !lcs.is_empty() {
                        lcs
                    } else {
                        vec![seq1.clone(), seq2.clone()]
                    };
                    // Address candidate must be at least 2 values long
                    for val in vals.into_iter().filter(|v| v.len() >= 2) {
                        if let Some(addr_len) = addr_len {
                            if val.len() != addr_len {
                                continue;
                            }
                        }
                        let known = |p: usize| {
                            self.known_addresses_by_participant.get(&p).map(|a| a.as_slice())
                        };
                        if !p1_already_assigned && !p2_already_assigned {
                            result.entry(p1).or_default().insert(val.clone());
                            result.entry(p2).or_default().insert(val.clone());
                        } else if p1_already_assigned && Some(val.as_slice()) != known(p1 as usize) {
                            result.entry(p2).or_default().insert(val.clone());
                        } else if p2_already_assigned && Some(val.as_slice()) != known(p2 as usize) {
                            result.entry(p1).or_default().insert(val.clone());
                        }
                    }
                }
            }
        }

        result
    }
}

fn add_score(map: &mut BTreeMap<isize, Vec<(Vec<u8>, f64)>>, participant: isize, address: &[u8], score: f64) {
    let entry = map.get_mut(&participant).unwrap();
    match entry.iter_mut().find(|(a, _)| a.as_slice() == address) {
        Some(e) => e.1 += score,
        None => entry.push((address.to_vec(), score)),
    }
}

/// Python `d[address] *= factor` on a defaultdict(int): a missing key is
/// inserted as 0 and stays 0.
fn mul_score(map: &mut BTreeMap<isize, Vec<(Vec<u8>, f64)>>, participant: isize, address: &[u8], factor: f64) {
    let entry = map.get_mut(&participant).unwrap();
    match entry.iter_mut().find(|(a, _)| a.as_slice() == address) {
        Some(e) => e.1 *= factor,
        None => entry.push((address.to_vec(), 0.0)),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awre::auto_assigner::Participant;
    use crate::awre::format_finder::FormatFinder;
    use crate::awre::message_type_builder::{FieldFunction, MessageTypeBuilder};
    use crate::awre::protocol_generator::{test_util, ProtocolGenerator};
    use crate::awre::util;
    use crate::protocol::Message;

    fn alice() -> Participant {
        Participant::with_address("Alice", "1234")
    }

    fn bob() -> Participant {
        Participant::with_address("Bob", "cafe")
    }

    /// test_address_engine.py::test_one_participant
    #[test]
    fn test_one_participant() {
        let mut mb = MessageTypeBuilder::new("simple_address_test");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        mb.add_label(FieldFunction::SrcAddress, 16, None);

        let num_messages_by_data_length = [(8usize, 5usize), (16, 10), (32, 15)];
        let a = alice();
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![a.clone()],
            &[],
            &["0x9a9d"],
            false,
            true,
        );
        for &(data_length, num_messages) in &num_messages_by_data_length {
            for i in 0..num_messages {
                let data = pg.decimal_to_bits(22 * i as u64, data_length);
                pg.generate_message(None, &data, Some(&a), None);
            }
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let ff = FormatFinder::with_participants(
            messages,
            vec![],
            vec![a],
            pg.participant_indices.clone(),
        );

        let address_engine = AddressEngine::new(&ff.hexvectors, &ff.participant_indices, None, &[], false);
        let address_dict = address_engine.find_addresses();

        assert_eq!(address_dict.len(), 0);
    }

    /// test_address_engine.py::test_two_participants
    #[test]
    fn test_two_participants() {
        let mut mb = MessageTypeBuilder::new("address_two_participants");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        mb.add_label(FieldFunction::SrcAddress, 16, None);
        mb.add_label(FieldFunction::DstAddress, 16, None);

        let num_messages = 50;
        let (a, b) = (alice(), bob());
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone()],
            vec![a.clone(), b.clone()],
            &[],
            &["0x9a9d"],
            false,
            true,
        );

        for i in 0..num_messages {
            let (source, destination, data_length) = if i % 2 == 0 {
                (&a, &b, 8)
            } else {
                (&b, &a, 16)
            };
            let data = pg.decimal_to_bits(4 * i as u64, data_length);
            pg.generate_message(None, &data, Some(source), Some(destination));
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let mut ff = FormatFinder::with_participants(
            messages,
            vec![],
            vec![a.clone(), b.clone()],
            pg.participant_indices.clone(),
        );

        let address_engine = AddressEngine::new(&ff.hexvectors, &ff.participant_indices, None, &[], false);
        let address_dict = address_engine.find_addresses();
        assert_eq!(address_dict.len(), 2);
        for participant in 0..2 {
            let addresses: Vec<String> = address_dict[&(participant as isize)]
                .iter()
                .map(|v| test_util::nibbles_to_hex(v))
                .collect();
            assert!(addresses.contains(&"1234".to_string()));
            assert!(addresses.contains(&"cafe".to_string()));
        }

        ff.known_participant_addresses.clear();
        assert_eq!(ff.known_participant_addresses.len(), 0);

        ff.perform_iteration();

        assert_eq!(ff.known_participant_addresses.len(), 2);
        let values: Vec<Vec<u8>> = ff
            .known_participant_addresses
            .values()
            .cloned()
            .collect();
        assert!(values.iter().any(|v| *v == vec![1, 2, 3, 4]));
        assert!(values.iter().any(|v| *v == vec![12, 10, 15, 14]));

        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 1);
        let mt = message_types[0];
        let dst_addr = mt.get_first_label_with_type(FieldFunction::DstAddress).unwrap();
        assert_eq!(dst_addr.label.start, 32);
        assert_eq!(dst_addr.label.end - dst_addr.label.start, 16);
        let src_addr = mt.get_first_label_with_type(FieldFunction::SrcAddress).unwrap();
        assert_eq!(src_addr.label.start, 48);
        assert_eq!(src_addr.label.end - src_addr.label.start, 16);
    }

    fn generate_two_participants_with_acks(include_type: bool) -> (ProtocolGenerator, Participant, Participant) {
        let mut mb = MessageTypeBuilder::new("data");
        mb.add_label(FieldFunction::Preamble, 8, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        if include_type {
            mb.add_label(FieldFunction::Type, 8, None);
        }
        mb.add_label(FieldFunction::DstAddress, 16, None);
        mb.add_label(FieldFunction::SrcAddress, 16, None);

        let mut mb_ack = MessageTypeBuilder::new("ack");
        mb_ack.add_label(FieldFunction::Preamble, 8, None);
        mb_ack.add_label(FieldFunction::Sync, 16, None);
        mb_ack.add_label(FieldFunction::Length, 8, None);
        mb_ack.add_label(FieldFunction::DstAddress, 16, None);

        let num_messages = 50;
        let (a, b) = (alice(), bob());
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone(), mb_ack.message_type.clone()],
            vec![a.clone(), b.clone()],
            &[],
            &["0x6768", "0x6768"],
            false,
            true,
        );

        let mut rng = test_util::Rng::new(0);
        for i in 0..num_messages {
            let (source, destination, data_length) = if i % 2 == 0 {
                (&a, &b, 8usize)
            } else {
                (&b, &a, 16)
            };
            let value = rng.next_below(1 << (data_length - 1));
            let data = pg.decimal_to_bits(value, data_length);
            pg.generate_message(None, &data, Some(source), Some(destination));
            // ack in the other direction
            pg.generate_message(Some(1), "", Some(destination), Some(source));
        }

        (pg, a, b)
    }

    /// test_address_engine.py::test_two_participants_with_ack_messages
    #[test]
    fn test_two_participants_with_ack_messages() {
        let (pg, a, b) = generate_two_participants_with_acks(false);

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let mut ff = FormatFinder::with_participants(
            messages,
            vec![],
            vec![a, b],
            pg.participant_indices.clone(),
        );

        let address_engine = AddressEngine::new(&ff.hexvectors, &ff.participant_indices, None, &[], false);
        let address_dict = address_engine.find_addresses();
        assert_eq!(address_dict.len(), 2);
        for participant in 0..2 {
            let addresses: Vec<String> = address_dict[&(participant as isize)]
                .iter()
                .map(|v| test_util::nibbles_to_hex(v))
                .collect();
            assert!(addresses.contains(&"1234".to_string()));
            assert!(addresses.contains(&"cafe".to_string()));
        }

        ff.known_participant_addresses.clear();
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 2);
        let mt = message_types[1];
        let dst_addr = mt.get_first_label_with_type(FieldFunction::DstAddress).unwrap();
        assert_eq!(dst_addr.label.start, 32);
        assert_eq!(dst_addr.label.end - dst_addr.label.start, 16);
        let src_addr = mt.get_first_label_with_type(FieldFunction::SrcAddress).unwrap();
        assert_eq!(src_addr.label.start, 48);
        assert_eq!(src_addr.label.end - src_addr.label.start, 16);

        let mt = message_types[0];
        let dst_addr = mt.get_first_label_with_type(FieldFunction::DstAddress).unwrap();
        assert_eq!(dst_addr.label.start, 32);
        assert_eq!(dst_addr.label.end - dst_addr.label.start, 16);
    }

    /// test_address_engine.py::test_two_participants_with_ack_messages_and_type
    #[test]
    fn test_two_participants_with_ack_messages_and_type() {
        let (pg, a, b) = generate_two_participants_with_acks(true);

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let mut ff = FormatFinder::with_participants(
            messages,
            vec![],
            vec![a, b],
            pg.participant_indices.clone(),
        );

        let address_engine = AddressEngine::new(&ff.hexvectors, &ff.participant_indices, None, &[], false);
        let address_dict = address_engine.find_addresses();
        assert_eq!(address_dict.len(), 2);
        for participant in 0..2 {
            let addresses: Vec<String> = address_dict[&(participant as isize)]
                .iter()
                .map(|v| test_util::nibbles_to_hex(v))
                .collect();
            assert!(addresses.contains(&"1234".to_string()));
            assert!(addresses.contains(&"cafe".to_string()));
        }

        ff.known_participant_addresses.clear();
        ff.perform_iteration();
        let message_types = ff.message_types();
        assert_eq!(message_types.len(), 2);
        let mt = message_types[1];
        let dst_addr = mt.get_first_label_with_type(FieldFunction::DstAddress).unwrap();
        assert_eq!(dst_addr.label.start, 40);
        assert_eq!(dst_addr.label.end - dst_addr.label.start, 16);
        let src_addr = mt.get_first_label_with_type(FieldFunction::SrcAddress).unwrap();
        assert_eq!(src_addr.label.start, 56);
        assert_eq!(src_addr.label.end - src_addr.label.start, 16);

        let mt = message_types[0];
        let dst_addr = mt.get_first_label_with_type(FieldFunction::DstAddress).unwrap();
        assert_eq!(dst_addr.label.start, 32);
        assert_eq!(dst_addr.label.end - dst_addr.label.start, 16);
    }

    /// test_address_engine.py::test_three_participants_with_ack
    #[test]
    fn test_three_participants_with_ack() {
        let alice = Participant::with_address("Alice", "1337");
        let bob = Participant::with_address("Bob", "4711");
        let carl = Participant::with_address("Carl", "cafe");

        let mut mb = MessageTypeBuilder::new("data");
        mb.add_label(FieldFunction::Preamble, 16, None);
        mb.add_label(FieldFunction::Sync, 16, None);
        mb.add_label(FieldFunction::Length, 8, None);
        mb.add_label(FieldFunction::SrcAddress, 16, None);
        mb.add_label(FieldFunction::DstAddress, 16, None);
        mb.add_label(FieldFunction::SequenceNumber, 16, None);

        let mut mb_ack = MessageTypeBuilder::new("ack");
        mb_ack.add_label(FieldFunction::Preamble, 16, None);
        mb_ack.add_label(FieldFunction::Sync, 16, None);
        mb_ack.add_label(FieldFunction::Length, 8, None);
        mb_ack.add_label(FieldFunction::DstAddress, 16, None);

        let preamble = "10".repeat(8);
        let participants = vec![alice.clone(), bob.clone(), carl.clone()];
        let mut pg = ProtocolGenerator::new(
            vec![mb.message_type.clone(), mb_ack.message_type.clone()],
            participants.clone(),
            &[&preamble, &preamble],
            &["0x9a7d", "0x9a7d"],
            false,
            true,
        );

        let mut rng = test_util::Rng::new(0);
        let mut i: usize = 0;
        while pg.messages.len() < 20 {
            let source = &participants[i % participants.len()];
            let destination = &participants[(i + 1) % participants.len()];
            let data_bytes = if i % 2 == 0 { 8 } else { 16 };
            let data = rng.random_bits(data_bytes * 8);
            pg.generate_message(None, &data, Some(source), Some(destination));
            pg.generate_message(Some(1), "", Some(destination), Some(source));
            i += 1;
        }

        let mut messages = pg.messages.clone();
        test_util::clear_message_types(&mut messages);
        let mut ff = FormatFinder::with_participants(
            messages,
            vec![],
            participants,
            pg.participant_indices.clone(),
        );
        ff.known_participant_addresses.clear();
        assert_eq!(ff.known_participant_addresses.len(), 0);
        ff.perform_analysis();

        // Since there are ACKS in this protocol, the engine must be able to
        // assign the correct participant addresses IN CORRECT ORDER!
        assert_eq!(
            test_util::nibbles_to_hex(&ff.known_participant_addresses[&0]),
            "1337"
        );
        assert_eq!(
            test_util::nibbles_to_hex(&ff.known_participant_addresses[&1]),
            "4711"
        );
        assert_eq!(
            test_util::nibbles_to_hex(&ff.known_participant_addresses[&2]),
            "cafe"
        );
    }

    /// test_address_engine.py::test_paper_example
    #[test]
    fn test_paper_example() {
        let alice = Participant::new("Alice");
        let bob = Participant::new("Bob");
        let _participants = vec![alice, bob];
        let messages = vec![
            Message::from_plain_hex_str("aabb1234", 0),
            Message::from_plain_hex_str("aabb6789", 0),
            Message::from_plain_hex_str("bbaa4711", 0),
            Message::from_plain_hex_str("bbaa1337", 0),
        ];

        let bitvectors = FormatFinder::get_bitvectors_from_messages(&messages, None);
        let hexvectors: Vec<Vec<u8>> = util::get_hexvectors(&bitvectors)
            .into_iter()
            .map(|v| v.into_iter().map(|n| n as u8).collect())
            .collect();
        let participant_indices = [0, 0, 1, 1];
        let _address_engine = AddressEngine::new(
            &hexvectors,
            &participant_indices,
            None,
            &[],
            false,
        );
    }

    /// test_address_engine.py::test_find_common_sub_sequence
    #[test]
    fn test_find_common_sub_sequence() {
        let str1 = "0612345678";
        let str2 = "0756781234";

        let seq1: Vec<u8> = str1.chars().map(|c| c as u8 - b'0').collect();
        let seq2: Vec<u8> = str2.chars().map(|c| c as u8 - b'0').collect();

        let indices = util::find_longest_common_sub_sequence_indices(&seq1, &seq2);
        assert_eq!(indices.len(), 2);
        for (start, end) in indices {
            let s = &str1[start..end];
            assert!(s == "5678" || s == "1234");
            assert!(str1.contains(s));
            assert!(str2.contains(s));
        }
    }

    /// test_address_engine.py::test_find_first_occurrence
    #[test]
    fn test_find_first_occurrence() {
        let str1 = format!("{}{}{}", "00".repeat(100), "1234500012345", "00".repeat(100));
        let str2 = "12345";

        let seq1: Vec<u8> = str1.chars().map(|c| c as u8 - b'0').collect();
        let seq2: Vec<u8> = str2.chars().map(|c| c as u8 - b'0').collect();

        let indices = util::find_occurrences(&seq1, &seq2, None, false);
        assert_eq!(indices.len(), 2);
        let index = indices[0];
        assert_eq!(&str1[index..index + str2.len()], str2);

        // Test with ignoring indices
        let ignore: Vec<usize> = (0..205).collect();
        let indices = util::find_occurrences(&seq1, &seq2, Some(&ignore), false);
        assert_eq!(indices.len(), 1);

        // Test with ignoring indices
        let ignore: Vec<usize> = (0..210).collect();
        let indices = util::find_occurrences(&seq1, &seq2, Some(&ignore), false);
        assert_eq!(indices.len(), 0);

        let ones = vec![1u8; 10];
        assert!(util::find_occurrences(&seq1, &ones, None, false).is_empty());
    }
}
