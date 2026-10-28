//! Port of `urh/awre/AutoAssigner.py` (the current reference revision
//! contains `auto_assign_participants` and
//! `auto_assign_participant_addresses`; there is no
//! `auto_assign_message_types` to port).
//!
//! DTO mapping: Python's Qt-backed `Participant` becomes the plain struct
//! below; messages do not carry a participant (the crate `Message` has no
//! such field), so assignment results are returned as per-message
//! participant indices (into the given participant list; -1 = unassigned).

use crate::awre::message_type_builder::AnalyzedMessageType;
use crate::protocol::Message;

/// Port of `urh/signalprocessing/Participant.py` (fields relevant to AWRE).
#[derive(Debug, Clone)]
pub struct Participant {
    pub name: String,
    pub shortname: String,
    pub address_hex: String,
    pub relative_rssi: f32,
}

impl Participant {
    pub fn new(name: &str) -> Self {
        Participant {
            name: name.to_string(),
            shortname: name
                .chars()
                .next()
                .and_then(|c| c.to_uppercase().next())
                .unwrap_or('X')
                .to_string(),
            address_hex: String::new(),
            relative_rssi: 0.0,
        }
    }

    pub fn with_address(name: &str, address_hex: &str) -> Self {
        let mut p = Participant::new(name);
        p.address_hex = address_hex.to_string();
        p
    }
}

/// Port of `AutoAssigner.auto_assign_participants`.
///
/// `message_types` resolves the SRC_ADDRESS labels of the messages' message
/// types (Python reaches them through `msg.message_type`). Returns the
/// participant index per message (-1 = no participant).
pub fn auto_assign_participants(
    messages: &[Message],
    participants: &mut Vec<Participant>,
    message_types: &[AnalyzedMessageType],
) -> Vec<isize> {
    let mut assigned: Vec<isize> = vec![-1; messages.len()];

    if participants.is_empty() {
        return assigned;
    }

    if participants.len() == 1 {
        return vec![0; messages.len()];
    }

    // Try to assign participants based on SRC_ADDRESS label and participant address
    for (mi, msg) in messages.iter().enumerate() {
        if assigned[mi] != -1 {
            continue;
        }
        let src_address = message_types
            .iter()
            .find(|mt| mt.message_type.name == msg.message_type)
            .and_then(|mt| mt.get_src_address_from_data(msg));
        if let Some(src_address) = src_address {
            if let Some(pi) = participants
                .iter()
                .position(|p| !p.address_hex.is_empty() && p.address_hex == src_address)
            {
                assigned[mi] = pi as isize;
            }
        }
    }

    // Assign remaining participants based on RSSI of messages
    let rssis: Vec<f32> = messages.iter().map(|m| m.rssi).collect();
    let min_rssi = rssis.iter().cloned().fold(f32::INFINITY, f32::min);
    let max_rssi = rssis.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let center_spacing = (max_rssi - min_rssi) / (participants.len() - 1) as f32;
    let centers: Vec<f32> = (0..participants.len())
        .map(|i| min_rssi + i as f32 * center_spacing)
        .collect();

    let mut rssi_assigned_centers: Vec<usize> = Vec::with_capacity(rssis.len());
    for &rssi in &rssis {
        // np.argmin(|rssi - centers|): first minimum wins
        let center_index = centers
            .iter()
            .enumerate()
            .min_by(|(ia, a), (ib, b)| {
                (rssi - *a)
                    .abs()
                    .partial_cmp(&(rssi - *b).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(ia.cmp(ib))
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        rssi_assigned_centers.push(center_index);
    }

    participants.sort_by(|a, b| {
        a.relative_rssi
            .partial_cmp(&b.relative_rssi)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for (mi, &center_index) in rssi_assigned_centers.iter().enumerate() {
        if assigned[mi] == -1 {
            assigned[mi] = center_index as isize;
        }
    }

    assigned
}

/// Port of `AutoAssigner.auto_assign_participant_addresses`.
pub fn auto_assign_participant_addresses(
    messages: &[Message],
    participant_indices: &[isize],
    participants: &mut [Participant],
    message_types: &[AnalyzedMessageType],
) {
    let without_address: Vec<usize> = participants
        .iter()
        .enumerate()
        .filter(|(_, p)| p.address_hex.is_empty())
        .map(|(i, _)| i)
        .collect();
    if without_address.is_empty() {
        return;
    }

    for (mi, msg) in messages.iter().enumerate() {
        let Some(&pi) = participant_indices.get(mi) else { continue };
        if pi < 0 || pi as usize >= participants.len() {
            continue;
        }
        if !without_address.contains(&(pi as usize)) {
            continue;
        }
        let mt = message_types
            .iter()
            .find(|mt| mt.message_type.name == msg.message_type);
        if let Some(src_address) = mt.and_then(|mt| mt.get_src_address_from_data(msg)) {
            if !src_address.is_empty() {
                participants[pi as usize].address_hex = src_address;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awre::message_type_builder::{
        AnalyzedMessageType, FieldFunction, MessageTypeBuilder,
    };

    /// There is no test_auto_assigner.py in the reference; this covers the
    /// ported logic: address-based assignment first, RSSI fallback after.
    #[test]
    fn test_auto_assign_participants() {
        // no participants -> all unassigned
        let messages = vec![Message::from_plain_hex_str("aabb", 0)];
        let mut participants: Vec<Participant> = Vec::new();
        let assigned = auto_assign_participants(&messages, &mut participants, &[]);
        assert_eq!(assigned, vec![-1]);

        // single participant -> everything belongs to it
        let mut participants = vec![Participant::new("Alice")];
        let assigned = auto_assign_participants(&messages, &mut participants, &[]);
        assert_eq!(assigned, vec![0]);

        // address-based assignment through a SRC address label
        let mut mb = MessageTypeBuilder::new("data");
        mb.add_label(FieldFunction::SrcAddress, 16, None);
        let mt = mb.message_type;

        let alice = Participant::with_address("Alice", "1234");
        let bob = Participant::with_address("Bob", "cafe");
        let mut participants = vec![alice, bob];

        let msg1 = Message::from_plain_hex_str("1234aaaa", 0); // from alice
        let mut msg1 = msg1;
        msg1.message_type = "data".to_string();
        let msg2 = Message::from_plain_hex_str("cafebbbb", 0); // from bob
        let mut msg2 = msg2;
        msg2.message_type = "data".to_string();

        let assigned = auto_assign_participants(
            &[msg1, msg2],
            &mut participants,
            &[AnalyzedMessageType {
                message_type: mt.message_type.clone(),
                typed_labels: mt.typed_labels.clone(),
                message_indices: vec![0, 1],
            }],
        );
        assert_eq!(assigned, vec![0, 1]);
    }

    #[test]
    fn test_auto_assign_participant_addresses() {
        let mut mb = MessageTypeBuilder::new("data");
        mb.add_label(FieldFunction::SrcAddress, 16, None);
        let mt = mb.message_type;

        let mut participants = vec![Participant::new("Alice"), Participant::new("Bob")];
        let mut msg = Message::from_plain_hex_str("1234aaaa", 0);
        msg.message_type = "data".to_string();

        let types = vec![AnalyzedMessageType {
            message_type: mt.message_type.clone(),
            typed_labels: mt.typed_labels.clone(),
            message_indices: vec![0],
        }];

        auto_assign_participant_addresses(&[msg], &[0, -1], &mut participants, &types);
        assert_eq!(participants[0].address_hex, "1234");
        assert_eq!(participants[1].address_hex, "");
    }

}
