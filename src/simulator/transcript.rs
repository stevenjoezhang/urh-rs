//! Port of `urh/simulator/Transcript.py`.

/// One transcript entry: (source participant id, source shortname,
/// destination participant id, destination shortname, message bits, simulator
/// message index).
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptEntry {
    pub source_id: String,
    pub source_shortname: String,
    pub destination_id: String,
    pub destination_shortname: String,
    pub bits: Vec<crate::Bit>,
    pub msg_index: String,
}

/// Rounds of transcript entries; a new round is started each time the
/// simulation wraps around to the root item.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    rounds: Vec<Vec<TranscriptEntry>>,
}

fn bits_to_string(bits: &[crate::Bit]) -> String {
    bits.iter().map(|b| char::from(b'0' + b)).collect()
}

fn bits_to_hex(bits: &[crate::Bit]) -> String {
    // Python util.convert_bits_to_string pads with zeros to a multiple of 4.
    let mut padded = bits.to_vec();
    while !padded.len().is_multiple_of(4) {
        padded.push(0);
    }
    let mut s = String::new();
    for chunk in padded.chunks(4) {
        let v = chunk
            .iter()
            .fold(0u8, |acc, b| (acc << 1) | (b & 1));
        s.push(char::from_digit(v as u32, 16).unwrap());
    }
    s
}

impl Transcript {
    pub fn new() -> Self {
        Transcript {
            rounds: Vec::new(),
        }
    }

    pub fn append(
        &mut self,
        source_id: &str,
        source_shortname: &str,
        destination_id: &str,
        destination_shortname: &str,
        bits: Vec<crate::Bit>,
        msg_index: &str,
    ) {
        if self.rounds.is_empty() {
            self.rounds.push(Vec::new());
        }
        self.rounds.last_mut().unwrap().push(TranscriptEntry {
            source_id: source_id.to_string(),
            source_shortname: source_shortname.to_string(),
            destination_id: destination_id.to_string(),
            destination_shortname: destination_shortname.to_string(),
            bits,
            msg_index: msg_index.to_string(),
        });
    }

    pub fn start_new_round(&mut self) {
        if self.rounds.is_empty() || !self.rounds.last().unwrap().is_empty() {
            self.rounds.push(Vec::new());
        }
    }

    pub fn clear(&mut self) {
        self.rounds.clear();
    }

    pub fn num_rounds(&self) -> usize {
        self.rounds.len()
    }

    pub fn entries(&self) -> impl Iterator<Item = &TranscriptEntry> {
        self.rounds.iter().flatten()
    }

    /// `get_for_all_participants`.
    pub fn get_for_all_participants(&self, all_rounds: bool, use_bit: bool) -> Vec<String> {
        let mut result = Vec::new();
        if self.rounds.is_empty() {
            return result;
        }
        let range = if all_rounds {
            0..self.rounds.len()
        } else {
            self.rounds.len() - 1..self.rounds.len()
        };
        for i in range {
            for e in &self.rounds[i] {
                let data = if use_bit {
                    bits_to_string(&e.bits)
                } else {
                    bits_to_hex(&e.bits)
                };
                result.push(format!(
                    "{} ({}->{}): {}",
                    e.msg_index, e.source_shortname, e.destination_shortname, data
                ));
            }
            if i != self.rounds.len() - 1 {
                result.push(String::new());
            }
        }
        result
    }

    /// `get_for_participant`: lines of the current round touching the given
    /// participant, prefixed `->` (participant was destination) or `<-`
    /// (participant was source).
    pub fn get_for_participant(&self, participant_id: &str) -> String {
        if self.rounds.is_empty() {
            return String::new();
        }
        let mut result = Vec::new();
        for e in self.rounds.last().unwrap() {
            let s = bits_to_string(&e.bits);
            if participant_id == e.destination_id {
                result.push(format!("->{s}"));
            } else if participant_id == e.source_id {
                result.push(format!("<-{s}"));
            }
        }
        result.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transcript_rounds_and_format() {
        let mut t = Transcript::new();
        t.append("a", "A", "b", "B", vec![1, 0, 1, 1], "1");
        t.start_new_round();
        t.append("b", "B", "a", "A", vec![0, 0, 1, 1], "2");

        assert_eq!(
            t.get_for_all_participants(true, true),
            vec!["1 (A->B): 1011", "", "2 (B->A): 0011"]
        );
        // hex with padding: 1011 -> b, 0011 -> 3
        assert_eq!(
            t.get_for_all_participants(true, false),
            vec!["1 (A->B): b", "", "2 (B->A): 3"]
        );
        // only current round
        assert_eq!(t.get_for_all_participants(false, true), vec!["2 (B->A): 0011"]);
    }

    #[test]
    fn test_get_for_participant() {
        let mut t = Transcript::new();
        t.append("a", "A", "b", "B", vec![1, 1], "1");
        t.append("b", "B", "a", "A", vec![0, 1], "2");
        assert_eq!(t.get_for_participant("b"), "->11\n<-01");
        assert_eq!(t.get_for_participant("zzz"), "");
    }

    #[test]
    fn test_hex_padding_matches_python() {
        // 20 bits -> 5 hex digits ("aa903" ground truth from the Python run)
        let bits: Vec<crate::Bit> = "10101010100100000011"
            .bytes()
            .map(|b| b - b'0')
            .collect();
        assert_eq!(bits_to_hex(&bits), "aa903");
    }

    #[test]
    fn test_start_new_round_skips_empty() {
        let mut t = Transcript::new();
        t.start_new_round();
        t.start_new_round();
        assert_eq!(t.num_rounds(), 1);
    }
}
