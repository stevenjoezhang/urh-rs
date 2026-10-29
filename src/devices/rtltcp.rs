//! RTL-TCP network protocol, ported from `urh/dev/native/RTLSDRTCP.py`.
//!
//! Pure TCP protocol, no local hardware or vendor library needed: a 12-byte
//! handshake (`"RTL0"`, tuner type, IF gain, RF gain, all big-endian) followed
//! by 1-byte-command / 4-byte-big-endian-value control messages interleaved
//! with a raw uint8 IQ stream on the same connection.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use super::{Command, Device, DeviceConfig, DeviceError, IqChunk};
use crate::Complex32;
use crate::iq::{IqArray, IqFormat};

/// Control command codes of the rtl_tcp protocol (RTL_TCP_CONSTS in Python).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RtlTcpCommand {
    SetFrequency = 0x01,
    SetSampleRate = 0x02,
    SetTunerGainMode = 0x03,
    SetTunerGain = 0x04,
    SetFreqCorrection = 0x05,
    SetTunerIfGain = 0x06,
    SetTestMode = 0x07,
    SetAgcMode = 0x08,
    SetDirectSampling = 0x09,
    SetOffsetTuning = 0x0A,
    SetRtlXtalFreq = 0x0B,
    SetTunerXtalFreq = 0x0C,
    SetGainByIndex = 0x0D,
    SetBiasTee = 0x0E,
    /// Only pinkavaj/rtl-sdr implements this; osmocom silently ignores it.
    SetBandwidth = 0x40,
}

/// Tuner type reported by the rtl_tcp handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tuner {
    E4000,
    FC0012,
    FC0013,
    FC2580,
    R820T,
    R828D,
    Unknown,
}

impl Tuner {
    fn from_number(n: u32) -> Tuner {
        match n {
            1 => Tuner::E4000,
            2 => Tuner::FC0012,
            3 => Tuner::FC0013,
            4 => Tuner::FC2580,
            5 => Tuner::R820T,
            6 => Tuner::R828D,
            _ => Tuner::Unknown,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Tuner::E4000 => "E4000",
            Tuner::FC0012 => "FC0012",
            Tuner::FC0013 => "FC0013",
            Tuner::FC2580 => "FC2580",
            Tuner::R820T => "R820T",
            Tuner::R828D => "R828D",
            Tuner::Unknown => "Unknown",
        }
    }
}

/// Map a generic [`Command`] to its rtl_tcp protocol byte, where one exists.
pub fn rtl_tcp_command(cmd: Command) -> Option<u8> {
    Some(match cmd {
        Command::SetFrequency => RtlTcpCommand::SetFrequency as u8,
        Command::SetSampleRate => RtlTcpCommand::SetSampleRate as u8,
        Command::SetBandwidth => RtlTcpCommand::SetBandwidth as u8,
        Command::SetRfGain => RtlTcpCommand::SetTunerGain as u8,
        Command::SetIfGain => RtlTcpCommand::SetTunerIfGain as u8,
        Command::SetFrequencyCorrection => RtlTcpCommand::SetFreqCorrection as u8,
        Command::SetDirectSamplingMode => RtlTcpCommand::SetDirectSampling as u8,
        Command::SetBiasTeeEnabled => RtlTcpCommand::SetBiasTee as u8,
        Command::Stop
        | Command::SetBbGain
        | Command::SetChannelIndex
        | Command::SetAntennaIndex => return None,
    })
}

const MAX_DATA_SIZE: usize = 65536;

pub struct RtlTcpDevice {
    stream: TcpStream,
    pub tuner: Tuner,
    /// IF gain (0.1 dB) from the handshake.
    pub if_gain: u16,
    /// RF gain (0.1 dB) from the handshake.
    pub rf_gain: u16,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RtlTcpDevice {
    /// Connect to an rtl_tcp server and consume the 12-byte handshake.
    pub fn connect(host: &str, port: u16) -> Result<Self, DeviceError> {
        let mut stream = TcpStream::connect((host, port))?;
        let mut init = [0u8; 12];
        stream.read_exact(&mut init)?;
        if &init[0..4] != b"RTL0" {
            return Err(DeviceError::OpenFailed(
                "not a valid rtl_tcp server (bad handshake magic)".into(),
            ));
        }
        let tuner = Tuner::from_number(u32::from_be_bytes([
            init[4], init[5], init[6], init[7],
        ]));
        let if_gain = u16::from_be_bytes([init[8], init[9]]);
        let rf_gain = u16::from_be_bytes([init[10], init[11]]);
        Ok(RtlTcpDevice {
            stream,
            tuner,
            if_gain,
            rf_gain,
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        })
    }

    /// Send a raw protocol message: 1-byte command + 4-byte big-endian value.
    pub fn set_raw(&mut self, cmd: u8, value: u32) -> Result<(), DeviceError> {
        let mut msg = [0u8; 5];
        msg[0] = cmd;
        msg[1..5].copy_from_slice(&value.to_be_bytes());
        self.stream.write_all(&msg)?;
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Decode a raw uint8 IQ buffer into normalized samples, reusing the
    /// crate's uint8 semantics from `iq.rs` ((v - 128) / 128).
    pub fn bytes_to_iq(buffer: &[u8]) -> Vec<Complex32> {
        IqArray::from_bytes(buffer, IqFormat::Complex16U).into_vec()
    }
}

impl Device for RtlTcpDevice {
    fn name(&self) -> &'static str {
        "RTL-TCP"
    }

    fn device_list() -> Vec<String> {
        // No discovery protocol exists; the server address is configuration.
        Vec::new()
    }

    fn set_parameter(&mut self, cmd: Command, value: f64) -> Result<(), DeviceError> {
        let rtl_cmd = rtl_tcp_command(cmd)
            .ok_or(DeviceError::Unsupported("command not supported by rtl_tcp"))?;
        // Values are integers (Hz, 0.1 dB, ppm, ...); negative gains travel as
        // their two's-complement bit pattern, like Python's to_bytes(4, "big").
        let value = value.round() as i64 as i32 as u32;
        self.set_raw(rtl_cmd, value)
    }

    fn start_rx(
        &mut self,
        config: &DeviceConfig,
        sink: std::sync::mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(DeviceError::StreamError("already receiving".into()));
        }
        // Initial parameter set, in the order the Python reference sends it.
        // Gain goes last, otherwise the tuner does not consider it.
        self.set_raw(RtlTcpCommand::SetFrequency as u8, config.frequency as i64 as u32)?;
        self.set_raw(
            RtlTcpCommand::SetSampleRate as u8,
            config.sample_rate as i64 as u32,
        )?;
        self.set_raw(RtlTcpCommand::SetBandwidth as u8, config.bandwidth as i64 as u32)?;
        self.set_raw(
            RtlTcpCommand::SetFreqCorrection as u8,
            config.frequency_correction as i64 as u32,
        )?;
        self.set_raw(
            RtlTcpCommand::SetDirectSampling as u8,
            config.direct_sampling_mode as u32,
        )?;
        self.set_raw(
            RtlTcpCommand::SetBiasTee as u8,
            config.bias_tee_enabled as i32 as u32,
        )?;
        self.set_raw(RtlTcpCommand::SetTunerGain as u8, config.rf_gain as i64 as u32)?;

        let mut reader = self.stream.try_clone()?;
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::SeqCst);
        self.handle = Some(thread::spawn(move || {
            let mut buf = vec![0u8; MAX_DATA_SIZE];
            loop {
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: server closed the stream
                    Ok(n) => {
                        if sink.send(RtlTcpDevice::bytes_to_iq(&buf[..n])).is_err() {
                            break; // receiver gone
                        }
                    }
                    Err(_) => break, // socket closed by stop()
                }
            }
            running.store(false, Ordering::SeqCst);
        }));
        Ok(())
    }

    fn send(&mut self, _config: &DeviceConfig, _samples: &[Complex32]) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported("RTL-TCP is receive-only"))
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.running.store(false, Ordering::SeqCst);
        // Unblock the reader thread's blocking read.
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }
}

impl Drop for RtlTcpDevice {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A minimal rtl_tcp server: sends the handshake, records all 5-byte
    /// commands it receives, then streams `iq_bytes` and closes (EOF).
    /// Returns (bound addr, port, command receiver channel).
    fn spawn_mock_server(iq_bytes: Vec<u8>, n_init_commands: usize) -> (String, u16, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Handshake: RTL0, tuner R820T (5), IF gain 100, RF gain 200.
            let mut init = Vec::new();
            init.extend_from_slice(b"RTL0");
            init.extend_from_slice(&5u32.to_be_bytes());
            init.extend_from_slice(&100u16.to_be_bytes());
            init.extend_from_slice(&200u16.to_be_bytes());
            sock.write_all(&init).unwrap();

            for _ in 0..n_init_commands {
                let mut msg = [0u8; 5];
                sock.read_exact(&mut msg).unwrap();
                cmd_tx.send(msg.to_vec()).unwrap();
            }

            sock.write_all(&iq_bytes).unwrap();
            // Close the write side -> client sees EOF.
            sock.shutdown(Shutdown::Write).unwrap();
            // Drain any further commands until the client disconnects.
            let mut buf = [0u8; 64];
            loop {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for msg in buf[..n].chunks_exact(5) {
                            let _ = cmd_tx.send(msg.to_vec());
                        }
                    }
                }
            }
        });
        ("127.0.0.1".to_string(), port, cmd_rx)
    }

    #[test]
    fn bytes_to_iq_normalizes_uint8() {
        let iq = RtlTcpDevice::bytes_to_iq(&[0, 255, 128, 127]);
        assert_eq!(iq.len(), 2);
        assert_eq!(iq[0].re, -1.0);
        assert_eq!(iq[0].im, 127.0 / 128.0);
        assert_eq!(iq[1].re, 0.0);
        assert!((iq[1].im + 1.0 / 128.0).abs() < 1e-6);
        // Odd trailing byte is dropped, like IQArray::convert_array_to_iq.
        assert_eq!(RtlTcpDevice::bytes_to_iq(&[1, 2, 3]).len(), 1);
    }

    #[test]
    fn connect_handshake_and_streaming() {
        // 1.5 IQ sample pairs + one stray byte; last pair split across the
        // client's read boundary is irrelevant since we concatenate chunks.
        let iq_bytes: Vec<u8> = vec![0, 255, 128, 127, 10];
        // 7 commands from start_rx's init sequence + 2 explicit set_parameter.
        let (host, port, cmd_rx) = spawn_mock_server(iq_bytes, 9);

        let mut dev = RtlTcpDevice::connect(&host, port).unwrap();
        assert_eq!(dev.tuner, Tuner::R820T);
        assert_eq!(dev.tuner.name(), "R820T");
        assert_eq!(dev.if_gain, 100);
        assert_eq!(dev.rf_gain, 200);

        dev.set_parameter(Command::SetFrequency, 433_920_000.0).unwrap();
        // Negative RF gain (tenths of dB) travels as two's complement.
        dev.set_parameter(Command::SetRfGain, -10.0).unwrap();

        let (tx, rx) = mpsc::channel();
        let config = DeviceConfig {
            frequency: 433_500_000.0,
            sample_rate: 1_000_000.0,
            bandwidth: 1_000_000.0,
            frequency_correction: 1.0,
            direct_sampling_mode: 0,
            bias_tee_enabled: false,
            rf_gain: 297.0,
            ..Default::default()
        };
        dev.start_rx(&config, tx).unwrap();

        // Collect until EOF closes the sender.
        let mut got: Vec<Complex32> = Vec::new();
        while let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_secs(5)) {
            got.extend(chunk);
        }
        assert!(!dev.is_running());

        // The stray odd byte is dropped -> 2 samples.
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].re, -1.0);
        assert_eq!(got[0].im, 127.0 / 128.0);
        assert_eq!(got[1].re, 0.0);
        assert!((got[1].im + 1.0 / 128.0).abs() < 1e-6);

        // Close our socket so the mock server's drain loop exits and its
        // command channel closes.
        dev.stop().unwrap();

        // Command wire format: explicit set_parameter first (FIFO from server).
        let mut cmds = Vec::new();
        while let Ok(c) = cmd_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            cmds.push(c);
        }
        assert_eq!(cmds.len(), 9);
        assert_eq!(&cmds[0][..], &[0x01, 0x19, 0xDD, 0x18, 0x00]); // freq 433920000
        assert_eq!(&cmds[1][..], &[0x04, 0xFF, 0xFF, 0xFF, 0xF6]); // gain -10
        // Then start_rx's init sequence, gain last (Python order).
        assert_eq!(cmds[2][0], 0x01);
        assert_eq!(&cmds[2][1..], &433_500_000u32.to_be_bytes());
        assert_eq!(cmds[3][0], 0x02);
        assert_eq!(&cmds[3][1..], &1_000_000u32.to_be_bytes());
        assert_eq!(cmds[4][0], 0x40); // bandwidth
        assert_eq!(cmds[5][0], 0x05); // freq correction
        assert_eq!(cmds[6][0], 0x09); // direct sampling
        assert_eq!(cmds[7][0], 0x0E); // bias tee
        assert_eq!(cmds[8][0], 0x04); // tuner gain, last
        assert_eq!(&cmds[8][1..], &297u32.to_be_bytes());
    }

    #[test]
    fn connect_rejects_bad_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"NOPE_NOPE_NOPE").unwrap();
        });
        assert!(RtlTcpDevice::connect("127.0.0.1", port).is_err());
    }

    #[test]
    fn unsupported_commands_rejected() {
        let (host, port, _cmd_rx) = spawn_mock_server(Vec::new(), 0);
        let mut dev = RtlTcpDevice::connect(&host, port).unwrap();
        assert!(matches!(
            dev.set_parameter(Command::SetBbGain, 1.0),
            Err(DeviceError::Unsupported(_))
        ));
        assert!(matches!(
            dev.set_parameter(Command::SetChannelIndex, 1.0),
            Err(DeviceError::Unsupported(_))
        ));
        assert!(dev
            .send(&DeviceConfig::default(), &[Complex32::new(1.0, 0.0)])
            .is_err());
    }

    #[test]
    fn stop_ends_stream() {
        let iq_bytes = vec![128u8; 100_000_000]; // enough to keep the stream busy
        let (host, port, _cmd_rx) = spawn_mock_server(iq_bytes, 7);
        let mut dev = RtlTcpDevice::connect(&host, port).unwrap();
        let (tx, rx) = mpsc::channel();
        dev.start_rx(&DeviceConfig::default(), tx).unwrap();
        assert!(rx.recv().is_ok());
        dev.stop().unwrap();
        assert!(!dev.is_running());
        let mut n = 0;
        while let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_secs(5)) {
            n += chunk.len();
        }
        assert!(n < 2_000_000); // did not consume the whole stream
    }
}
