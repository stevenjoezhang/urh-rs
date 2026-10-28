//! Endless sender — port of `urh/dev/EndlessSender.py`.
//!
//! The Python class wraps a `VirtualDevice` in continuous-send mode: it
//! installs a ring buffer, sets `num_sending_repeats = 0` (endless) and
//! forwards `push_data` writes into that buffer while the device's send
//! thread drains it. This port keeps the structure but decouples it from a
//! concrete device via the [`EndlessDevice`] trait. The device's
//! continuous-send thread is modelled by [`EndlessSender::pump`], which the
//! caller (or a real device implementation) drives to drain buffered chunks.

use std::collections::VecDeque;

use crate::Complex32;

/// Default buffer capacity in samples (Python:
/// `settings.CONTINUOUS_BUFFER_SIZE_MB * 10**6 // 8`, default 10 MB).
pub const DEFAULT_BUFFER_SIZE_SAMPLES: usize = 10_000_000 / 8;

/// The `VirtualDevice` surface `EndlessSender` needs (send mode).
pub trait EndlessDevice {
    /// `device.start()`.
    fn start(&mut self);
    /// `device.stop(reason)`.
    fn stop(&mut self, reason: &str);
    /// Send a chunk of samples (the continuous-send thread's
    /// `send_data`/ring-buffer drain).
    fn send_data(&mut self, data: &[Complex32]);
    /// `device.num_sending_repeats`.
    fn num_sending_repeats(&self) -> i64;
    /// `device.num_sending_repeats = n` (0 = endless).
    fn set_num_sending_repeats(&mut self, n: i64);
}

/// Port of `EndlessSender`.
pub struct EndlessSender<D: EndlessDevice> {
    pub device: D,
    buffer: VecDeque<Vec<Complex32>>,
    buffered_samples: usize,
    capacity: usize,
    started: bool,
}

impl<D: EndlessDevice> EndlessSender<D> {
    pub fn with_capacity(device: D, capacity: usize) -> Self {
        EndlessSender {
            device,
            buffer: VecDeque::new(),
            buffered_samples: 0,
            capacity,
            started: false,
        }
    }

    pub fn new(device: D) -> Self {
        Self::with_capacity(device, DEFAULT_BUFFER_SIZE_SAMPLES)
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// `start()`: endless repeat mode + device start.
    pub fn start(&mut self) {
        self.device.set_num_sending_repeats(0);
        self.device.start();
        self.started = true;
    }

    /// `stop()`.
    pub fn stop(&mut self) {
        self.device.stop("EndlessSender stopped.");
        self.started = false;
    }

    /// `push_data(data)`: enqueue samples for the device's continuous-send
    /// loop. When the buffer is full the oldest chunks are dropped (the
    /// Python version blocks on the ring buffer; a real device-backed
    /// implementation would block instead).
    pub fn push_data(&mut self, data: &[Complex32]) {
        while self.buffered_samples + data.len() > self.capacity && !self.buffer.is_empty() {
            if let Some(dropped) = self.buffer.pop_front() {
                self.buffered_samples -= dropped.len();
            }
        }
        self.buffered_samples += data.len();
        self.buffer.push_back(data.to_vec());
    }

    /// Drain buffered chunks to the device — the synchronous stand-in for
    /// the device's continuous-send thread. Returns the number of chunks
    /// sent. Only drains while started.
    pub fn pump(&mut self) -> usize {
        if !self.started {
            return 0;
        }
        let mut sent = 0;
        while let Some(chunk) = self.buffer.pop_front() {
            self.buffered_samples -= chunk.len();
            self.device.send_data(&chunk);
            sent += 1;
        }
        sent
    }

    /// Number of chunks waiting to be sent.
    pub fn pending_chunks(&self) -> usize {
        self.buffer.len()
    }

    /// Number of samples waiting to be sent.
    pub fn pending_samples(&self) -> usize {
        self.buffered_samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockDevice {
        sent: Vec<Vec<Complex32>>,
        started: bool,
        stopped_with: Vec<String>,
        num_sending_repeats: i64,
    }

    impl EndlessDevice for MockDevice {
        fn start(&mut self) {
            self.started = true;
        }
        fn stop(&mut self, reason: &str) {
            self.started = false;
            self.stopped_with.push(reason.to_string());
        }
        fn send_data(&mut self, data: &[Complex32]) {
            self.sent.push(data.to_vec());
        }
        fn num_sending_repeats(&self) -> i64 {
            self.num_sending_repeats
        }
        fn set_num_sending_repeats(&mut self, n: i64) {
            self.num_sending_repeats = n;
        }
    }

    fn samples(n: usize) -> Vec<Complex32> {
        vec![Complex32::new(1.0, 0.0); n]
    }

    #[test]
    fn test_start_sets_endless_repeat() {
        let mut sender = EndlessSender::new(MockDevice::default());
        assert!(!sender.is_started());
        sender.start();
        assert!(sender.is_started());
        assert!(sender.device.started);
        assert_eq!(sender.device.num_sending_repeats(), 0); // endless
        sender.stop();
        assert!(!sender.is_started());
        assert!(!sender.device.started);
        assert_eq!(sender.device.stopped_with, vec!["EndlessSender stopped."]);
    }

    #[test]
    fn test_push_and_pump_semantics() {
        let mut sender = EndlessSender::new(MockDevice::default());
        // Pushing before start buffers, but pump does not send while stopped.
        sender.push_data(&samples(10));
        assert_eq!(sender.pending_chunks(), 1);
        assert_eq!(sender.pump(), 0);
        assert!(sender.device.sent.is_empty());

        sender.start();
        sender.push_data(&samples(10));
        sender.push_data(&samples(20));
        // The chunk buffered before start is still in the ring buffer, so it
        // drains together with the new ones (Python: the device continuously
        // reads the ring buffer once started).
        assert_eq!(sender.pump(), 3);
        assert_eq!(sender.device.sent.len(), 3);
        assert_eq!(sender.device.sent[0].len(), 10);
        assert_eq!(sender.device.sent[1].len(), 10);
        assert_eq!(sender.device.sent[2].len(), 20);
        assert_eq!(sender.pending_samples(), 0);

        // Repeat semantics: pushing the same data again re-sends it.
        sender.push_data(&samples(10));
        sender.push_data(&samples(10));
        assert_eq!(sender.pump(), 2);
        assert_eq!(sender.device.sent.len(), 5);

        // Stop: the device no longer sends, so pending data stays put.
        sender.push_data(&samples(5));
        sender.stop();
        sender.pump();
        assert_eq!(sender.device.sent.len(), 5);
    }

    #[test]
    fn test_buffer_capacity_drops_oldest() {
        let mut sender = EndlessSender::with_capacity(MockDevice::default(), 15);
        sender.start();
        sender.push_data(&samples(10));
        sender.push_data(&samples(10)); // would exceed 15 -> drops first chunk
        assert_eq!(sender.pending_chunks(), 1);
        assert_eq!(sender.pending_samples(), 10);
        sender.push_data(&samples(5)); // fits alongside the 10
        assert_eq!(sender.pending_samples(), 15);
        assert_eq!(sender.pump(), 2);
    }
}
