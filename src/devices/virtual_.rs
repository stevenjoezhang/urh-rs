//! Virtual device: replay a local IQ file as a signal source, or write
//! transmitted samples to a file (the `file` role of `VirtualDevice.py`).
//!
//! The Python `VirtualDevice` is a Qt signal-emitting wrapper that dispatches
//! to grc/native backends; the Rust port keeps the useful core of that idea —
//! a device that is not backed by hardware — without the GUI plumbing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::{Command, Device, DeviceConfig, DeviceError, IqChunk};
use crate::Complex32;
use crate::iq::IqArray;

/// Chunk size used for RX replay (samples), matching the 2**15-ish batch
/// sizes URH pipes between processes.
const DEFAULT_CHUNK_SIZE: usize = 32768;

pub struct VirtualDevice {
    /// IQ file read during `start_rx`; format inferred from the extension.
    pub source_path: String,
    /// File written by `send`; format inferred from the extension.
    pub target_path: String,
    /// Sleep between chunks to simulate real-time reception.
    pub realtime: bool,
    /// Samples per chunk pushed into the sink.
    pub chunk_size: usize,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl VirtualDevice {
    /// A device that replays `source_path` on RX and writes `send` output to
    /// `target_path`.
    pub fn new(source_path: impl Into<String>, target_path: impl Into<String>) -> Self {
        VirtualDevice {
            source_path: source_path.into(),
            target_path: target_path.into(),
            realtime: false,
            chunk_size: DEFAULT_CHUNK_SIZE,
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// RX-only virtual device replaying `path`.
    pub fn replay(path: impl Into<String>) -> Self {
        Self::new(path, String::new())
    }

    /// TX-only virtual device writing to `path`.
    pub fn sender(path: impl Into<String>) -> Self {
        Self::new(String::new(), path)
    }

    pub fn with_realtime(mut self, realtime: bool) -> Self {
        self.realtime = realtime;
        self
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

impl Device for VirtualDevice {
    fn name(&self) -> &'static str {
        "VirtualDevice"
    }

    fn device_list() -> Vec<String> {
        Vec::new()
    }

    fn set_parameter(&mut self, _cmd: Command, _value: f64) -> Result<(), DeviceError> {
        // Nothing to configure on a file.
        Ok(())
    }

    fn start_rx(
        &mut self,
        config: &DeviceConfig,
        sink: std::sync::mpsc::Sender<IqChunk>,
    ) -> Result<(), DeviceError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(DeviceError::StreamError("already receiving".into()));
        }
        let samples = IqArray::from_file(&self.source_path)?;
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::SeqCst);
        let chunk_size = self.chunk_size.max(1);
        let realtime = self.realtime;
        let sample_rate = config.sample_rate;

        self.handle = Some(thread::spawn(move || {
            for chunk in samples.as_slice().chunks(chunk_size) {
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                if realtime && sample_rate > 0.0 {
                    let secs = chunk.len() as f64 / sample_rate;
                    thread::sleep(Duration::from_secs_f64(secs.min(1.0)));
                }
                if sink.send(chunk.to_vec()).is_err() {
                    break; // receiver gone
                }
            }
            running.store(false, Ordering::SeqCst);
        }));
        Ok(())
    }

    fn send(&mut self, _config: &DeviceConfig, samples: &[Complex32]) -> Result<(), DeviceError> {
        if self.target_path.is_empty() {
            return Err(DeviceError::Unsupported(
                "VirtualDevice has no target file configured",
            ));
        }
        IqArray::from_vec(samples.to_vec()).to_file(&self.target_path)?;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn temp_path(name: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("urh-virtual-{}-{}", std::process::id(), name));
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn replay_chunks_and_stop() {
        let path = temp_path("replay.complex");
        let samples: Vec<Complex32> = (0..100_000)
            .map(|i| Complex32::new(i as f32 * 0.001, -i as f32 * 0.001))
            .collect();
        IqArray::from_vec(samples.clone()).to_file(&path).unwrap();

        let mut dev = VirtualDevice::replay(&path);
        let (tx, rx) = mpsc::channel();
        dev.start_rx(&DeviceConfig::default(), tx).unwrap();

        let mut got: Vec<Complex32> = Vec::new();
        while let Ok(chunk) = rx.recv() {
            got.extend(chunk);
            if got.len() >= 100_000 {
                break;
            }
        }
        dev.stop().unwrap();
        assert_eq!(got.len(), 100_000);
        for (a, b) in got.iter().zip(samples.iter()) {
            assert_eq!(a, b);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stop_interrupts_replay() {
        let path = temp_path("stop.complex");
        // Large file so replay cannot finish before we stop it.
        let samples = vec![Complex32::new(0.5, -0.5); 5_000_000];
        IqArray::from_vec(samples).to_file(&path).unwrap();

        let mut dev = VirtualDevice::replay(&path).with_realtime(true);
        let (tx, rx) = mpsc::channel();
        dev.start_rx(&DeviceConfig::default(), tx).unwrap();
        // With realtime pacing, 5M samples cannot stream out instantly.
        assert!(rx.recv().is_ok());
        dev.stop().unwrap();
        assert!(!dev.is_running());
        // Channel drains and closes once the worker observes the flag.
        let mut leftover = 0;
        while let Ok(chunk) = rx.recv_timeout(Duration::from_secs(5)) {
            leftover += chunk.len();
        }
        assert!(leftover < 5_000_000);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn send_writes_file() {
        let path = temp_path("send.complex16u");
        let mut dev = VirtualDevice::sender(&path);
        let samples = vec![Complex32::new(0.0, 1.0), Complex32::new(-1.0, 0.0)];
        dev.send(&DeviceConfig::default(), &samples).unwrap();
        let back = IqArray::from_file(&path).unwrap();
        assert_eq!(back.len(), 2);
        // uint8 quantization: stored byte = (x + 1) * 127, read back as
        // (byte - 128) / 128.
        assert!((back[0].re - (-1.0 / 128.0)).abs() < 1e-6);
        assert!((back[0].im - 126.0 / 128.0).abs() < 1e-6);
        assert!((back[1].re - (-1.0)).abs() < 1e-6);
        assert!((back[1].im - (-1.0 / 128.0)).abs() < 1e-6);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn send_without_target_is_unsupported() {
        let mut dev = VirtualDevice::new(String::new(), String::new());
        assert!(dev
            .send(&DeviceConfig::default(), &[Complex32::new(1.0, 0.0)])
            .is_err());
    }

    #[test]
    fn start_rx_missing_file_errors() {
        let mut dev = VirtualDevice::replay("/nonexistent/definitely.complex");
        let (tx, _rx) = mpsc::channel();
        assert!(dev.start_rx(&DeviceConfig::default(), tx).is_err());
    }
}
