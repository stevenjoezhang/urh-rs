//! PCAP / PCAPNG IQ replay, ported from `urh/dev/PCAP.py` and `PCAPNG.py`.
//!
//! URH stores IQ in pcap files with linktype 147 (USER0, "reserved for
//! private use"): every packet's payload is a chunk of raw uint8 IQ bytes,
//! normalized here exactly like a `.complex16u` file. Files written with
//! linktype 1 (Ethernet) are accepted by stripping the 14-byte fake Ethernet
//! header, so captures produced by tools that cannot set USER0 still replay.

use std::fs;
use std::path::Path;

use super::{Command, Device, DeviceConfig, DeviceError, IqChunk};
use crate::Complex32;
use crate::iq::{IqArray, IqFormat};

/// Linktype carrying raw IQ in URH convention (PCAP.py NETWORK).
pub const LINKTYPE_USER0: u32 = 147;
/// Linktype whose packets get a 14-byte header stripped (fake Ethernet).
pub const LINKTYPE_ETHERNET: u32 = 1;
const ETHERNET_HEADER_LEN: usize = 14;

/// One captured packet; `data` is the payload after any link-layer header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcapPacket {
    pub ts_sec: u64,
    /// Fractional part in the file's resolution (nano- or microseconds).
    pub ts_frac: u64,
    pub data: Vec<u8>,
}

/// Byte-order / resolution of a pcap global header.
struct PcapMeta {
    big_endian: bool,
    link_type: u32,
}

fn u32_at(bytes: &[u8], off: usize, big_endian: bool) -> Option<u32> {
    let b = bytes.get(off..off + 4)?;
    Some(if big_endian {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    } else {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    })
}

/// Parse a classic .pcap file into packets (both endianisms, µs and ns
/// resolution magics are accepted).
pub fn parse_pcap(bytes: &[u8]) -> Result<Vec<PcapPacket>, DeviceError> {
    let meta = sniff_pcap_meta(bytes)
        .ok_or_else(|| DeviceError::StreamError("not a pcap file (bad magic)".into()))?;
    let mut packets = Vec::new();
    let mut off = 24; // global header size
    while let Some(ts_sec) = u32_at(bytes, off, meta.big_endian) {
        let ts_frac = u32_at(bytes, off + 4, meta.big_endian).ok_or(truncated())?;
        let incl_len = u32_at(bytes, off + 8, meta.big_endian).ok_or(truncated())? as usize;
        let _orig_len = u32_at(bytes, off + 12, meta.big_endian).ok_or(truncated())?;
        let data_off = off + 16;
        let raw = bytes
            .get(data_off..data_off + incl_len)
            .ok_or_else(truncated)?;
        packets.push(PcapPacket {
            ts_sec: ts_sec as u64,
            ts_frac: ts_frac as u64,
            data: strip_link_layer(meta.link_type, raw.to_vec())?,
        });
        off = data_off + incl_len;
    }
    Ok(packets)
}

fn truncated() -> DeviceError {
    DeviceError::StreamError("truncated pcap record".into())
}

fn sniff_pcap_meta(bytes: &[u8]) -> Option<PcapMeta> {
    if bytes.len() < 24 {
        return None;
    }
    let (big_endian, _ns) = match &bytes[0..4] {
        [0xa1, 0xb2, 0xc3, 0xd4] => (true, false), // µs, BE
        [0xa1, 0xb2, 0x3c, 0x4d] => (true, true),  // ns, BE (what PCAP.py writes)
        [0x4d, 0x3c, 0xb2, 0xa1] => (false, true), // ns, LE
        [0xd4, 0xc3, 0xb2, 0xa1] => (false, false), // µs, LE
        _ => return None,
    };
    let link_type = u32_at(bytes, 20, big_endian)?;
    Some(PcapMeta {
        big_endian,
        link_type,
    })
}

fn strip_link_layer(link_type: u32, mut data: Vec<u8>) -> Result<Vec<u8>, DeviceError> {
    match link_type {
        LINKTYPE_USER0 => Ok(data),
        LINKTYPE_ETHERNET => {
            if data.len() < ETHERNET_HEADER_LEN {
                return Ok(Vec::new());
            }
            data.drain(..ETHERNET_HEADER_LEN);
            Ok(data)
        }
        other => Err(DeviceError::StreamError(format!(
            "pcap linktype {other} carries no IQ"
        ))),
    }
}

/// Parse a .pcapng file (SHB / IDB / EPB blocks per the PCAPNG spec).
pub fn parse_pcapng(bytes: &[u8]) -> Result<Vec<PcapPacket>, DeviceError> {
    let mut packets = Vec::new();
    let mut off = 0usize;
    let mut big_endian = true;
    let mut link_type: Option<u32> = None;
    while off + 8 <= bytes.len() {
        let block_type = u32_at(bytes, off, big_endian).unwrap();
        // A new section header may switch byte order; its block type bytes
        // (0A 0D 0D 0A) are symmetric, the magic at offset 8 is not.
        if block_type == 0x0A0D0D0A {
            big_endian = match bytes.get(off + 8..off + 12) {
                Some([0x1A, 0x2B, 0x3C, 0x4D]) => true,
                Some([0x4D, 0x3C, 0x2B, 0x1A]) => false,
                _ => {
                    return Err(DeviceError::StreamError(
                        "pcapng section header has bad magic".into(),
                    ))
                }
            };
            link_type = None;
        }
        let block_type = u32_at(bytes, off, big_endian).unwrap();
        let total_len = u32_at(bytes, off + 4, big_endian).unwrap() as usize;
        if total_len < 12 || off + total_len > bytes.len() || total_len % 4 != 0 {
            return Err(DeviceError::StreamError(
                "pcapng block has invalid length".into(),
            ));
        }
        let body = &bytes[off..off + total_len];
        match block_type {
            0x00000001 => {
                // IDB: linktype is a u16 at offset 8.
                let lt = body
                    .get(8..10)
                    .map(|b| {
                        if big_endian {
                            u16::from_be_bytes([b[0], b[1]])
                        } else {
                            u16::from_le_bytes([b[0], b[1]])
                        }
                    })
                    .unwrap_or(0) as u32;
                link_type = Some(lt);
            }
            0x00000006 => {
                // EPB: interface, ts_hi, ts_lo, caplen, origlen, data.
                let fields: Vec<u32> = (0..5)
                    .filter_map(|i| u32_at(body, 8 + i * 4, big_endian))
                    .collect();
                if fields.len() < 5 {
                    return Err(truncated());
                }
                let caplen = fields[3] as usize;
                let data_off = off + 8 + 20;
                let raw = bytes
                    .get(data_off..data_off + caplen)
                    .ok_or_else(truncated)?;
                let lt = link_type.unwrap_or(LINKTYPE_USER0);
                packets.push(PcapPacket {
                    ts_sec: ((fields[1] as u64) << 32) | fields[2] as u64,
                    ts_frac: 0,
                    data: strip_link_layer(lt, raw.to_vec())?,
                });
            }
            _ => {} // SHB and options blocks are skipped
        }
        off += total_len;
    }
    if off != bytes.len() {
        return Err(truncated());
    }
    Ok(packets)
}

/// Dispatch on the file magic: pcap vs pcapng.
pub fn parse(filename: &str) -> Result<Vec<PcapPacket>, DeviceError> {
    let bytes = fs::read(filename)?;
    if bytes.len() >= 4 {
        match &bytes[0..4] {
            [0x0a, 0x0d, 0x0d, 0x0a] | [0x0d, 0x0d, 0x0a, 0x0a] => {
                return parse_pcapng(&bytes);
            }
            _ => {}
        }
    }
    parse_pcap(&bytes)
}

/// Convert packets (raw uint8 IQ payloads) into normalized samples.
pub fn packets_to_iq(packets: &[PcapPacket]) -> IqArray {
    let total: usize = packets.iter().map(|p| p.data.len()).sum();
    let mut bytes = Vec::with_capacity(total);
    for p in packets {
        bytes.extend_from_slice(&p.data);
    }
    IqArray::from_bytes(&bytes, IqFormat::Complex16U)
}

// ---------------------------------------------------------------------------
// Writer side (port of PCAP.py / PCAPNG.py builders), kept for roundtrips.
// ---------------------------------------------------------------------------

/// Build the pcap global header exactly like `PCAP.build_global_header`:
/// big-endian, nanosecond resolution, linktype 147.
pub fn build_pcap_global_header() -> Vec<u8> {
    const MAGIC: u32 = 0xA1B23C4D; // nanosecond resolution
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.extend_from_slice(&2u16.to_be_bytes()); // version major
    out.extend_from_slice(&4u16.to_be_bytes()); // version minor
    out.extend_from_slice(&0i32.to_be_bytes()); // thiszone
    out.extend_from_slice(&0u32.to_be_bytes()); // sigfigs
    out.extend_from_slice(&65535u32.to_be_bytes()); // snaplen
    out.extend_from_slice(&LINKTYPE_USER0.to_be_bytes());
    out
}

/// Build one pcap record with absolute nanosecond timestamps.
pub fn build_pcap_packet(ts_sec: u32, ts_nsec: u32, data: &[u8]) -> Vec<u8> {
    let l = data.len() as u32;
    let mut out = Vec::with_capacity(16 + data.len());
    out.extend_from_slice(&ts_sec.to_be_bytes());
    out.extend_from_slice(&ts_nsec.to_be_bytes());
    out.extend_from_slice(&l.to_be_bytes());
    out.extend_from_slice(&l.to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// Write IQ bytes as a single-packet .pcap file (linktype 147).
pub fn write_pcap(filename: &str, iq_bytes: &[u8]) -> Result<(), DeviceError> {
    let mut out = build_pcap_global_header();
    out.extend_from_slice(&build_pcap_packet(0, 0, iq_bytes));
    fs::write(Path::new(filename), out)?;
    Ok(())
}

/// Write IQ bytes as a single-packet .pcapng file (SHB + IDB + one EPB),
/// mirroring the block layout of `PCAPNG.py`.
pub fn write_pcapng(filename: &str, iq_bytes: &[u8]) -> Result<(), DeviceError> {
    let mut out = Vec::new();

    // Section Header Block.
    const SHB: u32 = 0x0A0D0D0A;
    const SHB_LEN: u32 = 28;
    out.extend_from_slice(&SHB.to_be_bytes());
    out.extend_from_slice(&SHB_LEN.to_be_bytes());
    out.extend_from_slice(&0x1A2B3C4Du32.to_be_bytes()); // byte-order magic
    out.extend_from_slice(&1u16.to_be_bytes()); // version major
    out.extend_from_slice(&0u16.to_be_bytes()); // version minor
    out.extend_from_slice(&u64::MAX.to_be_bytes()); // section length: -1
    out.extend_from_slice(&SHB_LEN.to_be_bytes());

    // Interface Description Block, linktype 147.
    const IDB: u32 = 0x00000001;
    const IDB_LEN: u32 = 20;
    out.extend_from_slice(&IDB.to_be_bytes());
    out.extend_from_slice(&IDB_LEN.to_be_bytes());
    out.extend_from_slice(&(LINKTYPE_USER0 as u16).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // reserved
    out.extend_from_slice(&0u32.to_be_bytes()); // snaplen
    out.extend_from_slice(&IDB_LEN.to_be_bytes());

    // Enhanced Packet Block (timestamp resolution 1e-6, as in PCAPNG.py).
    const EPB: u32 = 0x00000006;
    let caplen = iq_bytes.len() as u32;
    let padded = (iq_bytes.len() + 3) / 4 * 4;
    let total: u32 = 32 + padded as u32;
    out.extend_from_slice(&EPB.to_be_bytes());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // interface id
    out.extend_from_slice(&0u32.to_be_bytes()); // timestamp high
    out.extend_from_slice(&0u32.to_be_bytes()); // timestamp low
    out.extend_from_slice(&caplen.to_be_bytes());
    out.extend_from_slice(&caplen.to_be_bytes()); // original len
    out.extend_from_slice(iq_bytes);
    out.resize(out.len() + (padded - iq_bytes.len()), 0);
    out.extend_from_slice(&total.to_be_bytes());

    fs::write(Path::new(filename), out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Device: replay a pcap/pcapng file as a signal source.
// ---------------------------------------------------------------------------

pub struct PcapSource {
    pub path: String,
    running: bool,
}

impl PcapSource {
    pub fn new(path: impl Into<String>) -> Self {
        PcapSource {
            path: path.into(),
            running: false,
        }
    }
}

impl Device for PcapSource {
    fn name(&self) -> &'static str {
        "PCAP"
    }

    fn device_list() -> Vec<String> {
        Vec::new()
    }

    fn set_parameter(&mut self, _cmd: Command, _value: f64) -> Result<(), DeviceError> {
        Ok(())
    }

    fn start_rx(
        &mut self,
        _config: &DeviceConfig,
        sink: std::sync::mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        if self.running {
            return Err(DeviceError::StreamError("already receiving".into()));
        }
        let packets = parse(&self.path)?;
        self.running = true;
        // One captured packet == one chunk, preserving URH's packetization.
        for p in &packets {
            let chunk = IqArray::from_bytes(&p.data, IqFormat::Complex16U).into_vec();
            if sink.send(chunk).is_err() {
                break;
            }
        }
        self.running = false;
        Ok(())
    }

    fn send(&mut self, _config: &DeviceConfig, samples: &[Complex32]) -> Result<(), DeviceError> {
        let bytes = IqArray::from_vec(samples.to_vec()).to_bytes(IqFormat::Complex16U);
        if self.path.to_ascii_lowercase().ends_with("ng")
            || self.path.to_ascii_lowercase().ends_with(".pcapng")
        {
            write_pcapng(&self.path, &bytes)
        } else {
            write_pcap(&self.path, &bytes)
        }
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.running = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("urh-pcap-{}-{}", std::process::id(), name));
        p.to_string_lossy().into_owned()
    }

    fn build_pcap_bytes(link_type: u32, little_endian: bool, records: &[(&[u8], u32, u32)]) -> Vec<u8> {
        let put = |out: &mut Vec<u8>, v: u32| {
            if little_endian {
                out.extend_from_slice(&v.to_le_bytes());
            } else {
                out.extend_from_slice(&v.to_be_bytes());
            }
        };
        let put16 = |out: &mut Vec<u8>, v: u16| {
            if little_endian {
                out.extend_from_slice(&v.to_le_bytes());
            } else {
                out.extend_from_slice(&v.to_be_bytes());
            }
        };
        let mut out = Vec::new();
        // µs magic, matching the endianness under test.
        if little_endian {
            out.extend_from_slice(&0xA1B2C3D4u32.to_le_bytes());
        } else {
            out.extend_from_slice(&0xA1B2C3D4u32.to_be_bytes());
        }
        put16(&mut out, 2); // version major
        put16(&mut out, 4); // version minor
        put(&mut out, 0); // thiszone
        put(&mut out, 0); // sigfigs
        put(&mut out, 65535); // snaplen
        put(&mut out, link_type);
        for (data, ts_sec, ts_frac) in records {
            put(&mut out, *ts_sec);
            put(&mut out, *ts_frac);
            put(&mut out, data.len() as u32);
            put(&mut out, data.len() as u32);
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn parse_user0_pcap() {
        // Two packets of uint8 IQ + one packet with a stray odd byte.
        let bytes = build_pcap_bytes(
            LINKTYPE_USER0,
            false,
            &[
                (&[0, 255, 128, 127], 1, 100),
                (&[10, 200, 30], 2, 200),
            ],
        );
        let packets = parse_pcap(&bytes).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].ts_sec, 1);
        assert_eq!(packets[0].ts_frac, 100);
        assert_eq!(packets[0].data, vec![0, 255, 128, 127]);
        let iq = packets_to_iq(&packets);
        assert_eq!(iq.len(), 3); // odd trailing byte dropped
        assert_eq!(iq[0].re, -1.0);
        assert_eq!(iq[0].im, 127.0 / 128.0);
        assert_eq!(iq[1].re, 0.0);
        assert!((iq[1].im + 1.0 / 128.0).abs() < 1e-6);
    }

    #[test]
    fn parse_little_endian_pcap() {
        let bytes = build_pcap_bytes(LINKTYPE_USER0, true, &[(&[128, 128], 5, 0)]);
        let packets = parse_pcap(&bytes).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data, vec![128, 128]);
    }

    #[test]
    fn parse_ethernet_pcap_strips_header() {
        let mut pkt = vec![0u8; 14]; // fake Ethernet header
        pkt.extend_from_slice(&[0u8, 255u8, 128u8, 127u8]);
        let bytes = build_pcap_bytes(LINKTYPE_ETHERNET, false, &[(&pkt, 0, 0)]);
        let packets = parse_pcap(&bytes).unwrap();
        assert_eq!(packets[0].data, vec![0, 255, 128, 127]);
    }

    #[test]
    fn unknown_linktype_rejected() {
        let bytes = build_pcap_bytes(105, false, &[(&[1, 2], 0, 0)]); // IEEE 802.11
        assert!(parse_pcap(&bytes).is_err());
    }

    #[test]
    fn truncated_record_rejected() {
        let mut bytes = build_pcap_bytes(LINKTYPE_USER0, false, &[(&[1, 2, 3, 4], 0, 0)]);
        bytes.truncate(bytes.len() - 2); // cut into the record data
        assert!(parse_pcap(&bytes).is_err());
    }

    #[test]
    fn bad_magic_rejected() {
        assert!(parse_pcap(b"not a pcap file at all......").is_err());
    }

    #[test]
    fn pcap_roundtrip_and_device_replay() {
        let path = temp_path("replay.pcap");
        let iq_bytes = vec![0u8, 255u8, 128u8, 127u8, 64u8, 192u8];
        write_pcap(&path, &iq_bytes).unwrap();

        // The writer must produce exactly what PCAP.py produces.
        let expected = {
            let mut e = Vec::new();
            e.extend_from_slice(&0xA1B23C4Du32.to_be_bytes());
            e.extend_from_slice(&2u16.to_be_bytes());
            e.extend_from_slice(&4u16.to_be_bytes());
            e.extend_from_slice(&0i32.to_be_bytes());
            e.extend_from_slice(&0u32.to_be_bytes());
            e.extend_from_slice(&65535u32.to_be_bytes());
            e.extend_from_slice(&147u32.to_be_bytes());
            e.extend_from_slice(&build_pcap_packet(0, 0, &iq_bytes));
            e
        };
        assert_eq!(fs::read(&path).unwrap(), expected);

        let mut dev = PcapSource::new(&path);
        let (tx, rx) = std::sync::mpsc::channel();
        dev.start_rx(&DeviceConfig::default(), tx).unwrap();
        let mut chunks: Vec<IqChunk> = Vec::new();
        while let Ok(c) = rx.recv() {
            chunks.push(c);
        }
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 3);
        assert_eq!(chunks[0][0].re, -1.0);
        assert_eq!(chunks[0][1].re, 0.0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pcapng_roundtrip() {
        let path = temp_path("replay.pcapng");
        let iq_bytes = vec![0u8, 255u8, 128u8, 127u8, 64u8, 192u8, 7u8];
        write_pcapng(&path, &iq_bytes).unwrap();
        // Dispatch must pick pcapng from the magic.
        let packets = parse(&path).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data, iq_bytes);
        let iq = packets_to_iq(&packets);
        assert_eq!(iq.len(), 3); // odd byte dropped

        // Replay via the Device trait.
        let mut dev = PcapSource::new(&path);
        let (tx, rx) = std::sync::mpsc::channel();
        dev.start_rx(&DeviceConfig::default(), tx).unwrap();
        let chunks: Vec<IqChunk> = rx.iter().collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 3);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pcapng_byte_order_swap() {
        // Little-endian pcapng: SHB block type is byte-symmetric, the
        // byte-order magic at offset 8 appears reversed on the wire.
        let mut out = Vec::new();
        out.extend_from_slice(&0x0A0D0D0Au32.to_le_bytes()); // symmetric
        out.extend_from_slice(&28u32.to_le_bytes());
        out.extend_from_slice(&0x1A2B3C4Du32.to_le_bytes()); // wire: 4D 3C 2B 1A
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&u64::MAX.to_le_bytes());
        out.extend_from_slice(&28u32.to_le_bytes());
        // IDB, linktype 147
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&20u32.to_le_bytes());
        out.extend_from_slice(&147u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&20u32.to_le_bytes());
        // EPB with 4 bytes payload (no padding needed)
        out.extend_from_slice(&6u32.to_le_bytes());
        out.extend_from_slice(&36u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&[0, 255, 128, 127]);
        out.extend_from_slice(&36u32.to_le_bytes());

        let packets = parse_pcapng(&out).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data, vec![0, 255, 128, 127]);
    }

    #[test]
    fn device_send_writes_pcap() {
        let path = temp_path("send.pcap");
        let mut dev = PcapSource::new(&path);
        let samples = vec![Complex32::new(0.0, 1.0), Complex32::new(-1.0, 0.0)];
        dev.send(&DeviceConfig::default(), &samples).unwrap();
        let packets = parse(&path).unwrap();
        assert_eq!(packets.len(), 1);
        let iq = packets_to_iq(&packets);
        assert_eq!(iq.len(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replay_missing_file_errors() {
        let mut dev = PcapSource::new("/nonexistent/definitely.pcap");
        let (tx, _rx) = std::sync::mpsc::channel();
        assert!(dev.start_rx(&DeviceConfig::default(), tx).is_err());
    }
}
