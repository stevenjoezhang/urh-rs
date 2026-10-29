//! Binary waveform frame codec (contract: docs/ipc-contract.md, "Binary
//! frame"). The frontend decodes this layout directly in TypeScript, so the
//! byte positions and little-endian field order are frozen.

use crate::signal::Extrema;
use crate::WaveformMode;

/// Magic bytes at offset 0.
pub(crate) const MAGIC: [u8; 4] = *b"URH1";
/// Frame format version (contract: currently always 1).
pub const FRAME_VERSION: u16 = 1;
/// Payload dtype tag: 1 = IEEE-754 f32, the only dtype in v1.
pub const DTYPE_F32: u16 = 1;
/// Reserved flags field value (contract: always 0 in v1).
pub const FLAGS_NONE: u32 = 0;
/// Frame header size in bytes; the payload starts here, so every `f32` in the
/// payload is 4-byte aligned.
pub const HEADER_LEN: usize = 24;

/// Encode one waveform response frame.
///
/// - `request_id` is opaque to the backend and echoed verbatim (the frontend
///   drops stale responses by it).
/// - Iq payload: per bin `[min_i, max_i, min_q, max_q]` (16 bytes per bin).
/// - Amplitude payload: per bin `[min_mag, max_mag]` (8 bytes per bin).
pub(crate) fn encode_frame(request_id: u32, bins: &[Extrema], mode: WaveformMode) -> Vec<u8> {
    let per_bin = match mode {
        WaveformMode::Iq => 16,
        WaveformMode::Amplitude => 8,
    };
    let mut frame = Vec::with_capacity(HEADER_LEN + per_bin * bins.len());
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&FRAME_VERSION.to_le_bytes());
    frame.extend_from_slice(&DTYPE_F32.to_le_bytes());
    frame.extend_from_slice(&request_id.to_le_bytes());
    frame.extend_from_slice(&(bins.len() as u32).to_le_bytes());
    frame.extend_from_slice(&mode.as_u32().to_le_bytes());
    frame.extend_from_slice(&FLAGS_NONE.to_le_bytes());
    for bin in bins {
        match mode {
            WaveformMode::Iq => {
                frame.extend_from_slice(&bin.min_i.to_le_bytes());
                frame.extend_from_slice(&bin.max_i.to_le_bytes());
                frame.extend_from_slice(&bin.min_q.to_le_bytes());
                frame.extend_from_slice(&bin.max_q.to_le_bytes());
            }
            WaveformMode::Amplitude => {
                frame.extend_from_slice(&bin.min_m.to_le_bytes());
                frame.extend_from_slice(&bin.max_m.to_le_bytes());
            }
        }
    }
    frame
}

/// Test-only decoder mirroring the frontend's validation duties (magic,
/// version, dtype, `n_bins` vs actual length). Not part of the shipped API.
#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    pub(crate) struct DecodedFrame {
        pub(crate) request_id: u32,
        pub(crate) n_bins: u32,
        pub(crate) mode: u32,
        pub(crate) flags: u32,
        /// Flat payload: `stride` f32 values per bin.
        pub(crate) values: Vec<f32>,
        /// f32 values per bin (4 for Iq, 2 for Amplitude).
        pub(crate) stride: usize,
    }

    pub(crate) fn decode(frame: &[u8]) -> DecodedFrame {
        assert!(frame.len() >= HEADER_LEN, "frame shorter than header");
        assert_eq!(&frame[0..4], MAGIC.as_slice(), "bad magic");
        assert_eq!(
            u16::from_le_bytes([frame[4], frame[5]]),
            FRAME_VERSION,
            "bad version"
        );
        assert_eq!(
            u16::from_le_bytes([frame[6], frame[7]]),
            DTYPE_F32,
            "bad dtype"
        );
        let request_id = u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]);
        let n_bins = u32::from_le_bytes([frame[12], frame[13], frame[14], frame[15]]);
        let mode = u32::from_le_bytes([frame[16], frame[17], frame[18], frame[19]]);
        let flags = u32::from_le_bytes([frame[20], frame[21], frame[22], frame[23]]);
        let stride = if mode == WaveformMode::Iq.as_u32() {
            4
        } else {
            2
        };
        let payload = &frame[HEADER_LEN..];
        assert_eq!(
            payload.len() / 4,
            stride * n_bins as usize,
            "payload length does not match n_bins"
        );
        let values = payload
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        DecodedFrame {
            request_id,
            n_bins,
            mode,
            flags,
            values,
            stride,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::decode;
    use super::*;

    fn bin(min_i: f32, max_i: f32, min_q: f32, max_q: f32, min_m: f32, max_m: f32) -> Extrema {
        Extrema {
            min_i,
            max_i,
            min_q,
            max_q,
            min_m,
            max_m,
        }
    }

    #[test]
    fn test_iq_frame_exact_bytes() {
        let bins = vec![
            bin(-0.5, 0.25, -0.75, 0.125, 0.1, 0.9),
            bin(-1.0, 1.0, -2.0, 2.0, 0.0, 3.0),
        ];
        let frame = encode_frame(0xDEAD_BEEF, &bins, WaveformMode::Iq);

        let mut expected = Vec::new();
        expected.extend_from_slice(b"URH1");
        expected.extend_from_slice(&1u16.to_le_bytes()); // version
        expected.extend_from_slice(&1u16.to_le_bytes()); // dtype
        expected.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // request_id
        expected.extend_from_slice(&2u32.to_le_bytes()); // n_bins
        expected.extend_from_slice(&1u32.to_le_bytes()); // mode = Iq
        expected.extend_from_slice(&0u32.to_le_bytes()); // flags
        for v in [
            -0.5f32, 0.25, -0.75, 0.125, // bin 0: min_i, max_i, min_q, max_q
            -1.0f32, 1.0, -2.0, 2.0, // bin 1
        ] {
            expected.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(frame, expected);
        assert_eq!(frame.len(), HEADER_LEN + 16 * 2);
    }

    #[test]
    fn test_amplitude_frame_exact_bytes() {
        let bins = vec![
            bin(0.0, 0.0, 0.0, 0.0, 0.1, 0.9),
            bin(0.0, 0.0, 0.0, 0.0, 0.0, 3.0),
            bin(0.0, 0.0, 0.0, 0.0, 2.5, 2.5),
        ];
        let frame = encode_frame(7, &bins, WaveformMode::Amplitude);

        let mut expected = Vec::new();
        expected.extend_from_slice(b"URH1");
        expected.extend_from_slice(&1u16.to_le_bytes());
        expected.extend_from_slice(&1u16.to_le_bytes());
        expected.extend_from_slice(&7u32.to_le_bytes());
        expected.extend_from_slice(&3u32.to_le_bytes());
        expected.extend_from_slice(&2u32.to_le_bytes()); // mode = Amplitude
        expected.extend_from_slice(&0u32.to_le_bytes());
        for v in [0.1f32, 0.9, 0.0, 3.0, 2.5, 2.5] {
            expected.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(frame, expected);
        assert_eq!(frame.len(), HEADER_LEN + 8 * 3);
    }

    #[test]
    fn test_request_id_echo_and_header_fields() {
        let bins = vec![bin(0.0, 0.0, 0.0, 0.0, 0.0, 0.0)];
        for rid in [0u32, 1, 65_535, u32::MAX] {
            for mode in [WaveformMode::Iq, WaveformMode::Amplitude] {
                let frame = encode_frame(rid, &bins, mode);
                let decoded = decode(&frame);
                assert_eq!(decoded.request_id, rid);
                assert_eq!(decoded.n_bins, 1);
                assert_eq!(decoded.mode, mode.as_u32());
                assert_eq!(decoded.flags, 0);
                assert_eq!(decoded.stride, if mode == WaveformMode::Iq { 4 } else { 2 });
                assert_eq!(decoded.values.len(), decoded.stride);
            }
        }
    }

    #[test]
    fn test_empty_bin_list_is_a_header_only_frame() {
        // Zero bins never occur through the repository (n >= 1 after
        // validation), but the codec itself degrades gracefully.
        let frame = encode_frame(42, &[], WaveformMode::Iq);
        assert_eq!(frame.len(), HEADER_LEN);
        let decoded = decode(&frame);
        assert_eq!(decoded.n_bins, 0);
        assert!(decoded.values.is_empty());
    }
}
