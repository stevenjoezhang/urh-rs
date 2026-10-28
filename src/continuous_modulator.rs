//! Continuous modulator — port of `urh/signalprocessing/ContinuousModulator.py`.
//!
//! The Python version runs a `multiprocessing.Process` that modulates a list
//! of messages round after round into a ring buffer, so a device can drain it
//! without the producer running out of RAM. This port keeps the modulation
//! loop and the abort/current-message-index bookkeeping; the ring buffer and
//! the OS process are replaced by a [`ContinuousSendSink`] trait (a blocking
//! `push` stands in for "wait until the ring buffer has space, then push").

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::simulator::config::SimModulator;
use crate::{Bit, Complex32};

/// Default ring-buffer capacity in samples (Python:
/// `settings.CONTINUOUS_BUFFER_SIZE_MB * 1e6 // 8` with the default 10 MB).
pub const DEFAULT_BUFFER_SIZE_SAMPLES: usize = 10_000_000 / 8;

/// Consumer of modulated samples. `push` may block until the consumer has
/// space (the Python ring-buffer `will_fit` wait).
pub trait ContinuousSendSink: Send {
    fn push(&mut self, samples: Vec<Complex32>);
}

/// Simple collector for tests / in-memory use.
#[derive(Default)]
pub struct CollectorSink {
    pub samples: Vec<Complex32>,
    /// Length of each pushed chunk (one entry per push).
    pub chunk_lengths: Vec<usize>,
}

impl CollectorSink {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ContinuousSendSink for Box<dyn ContinuousSendSink + Send> {
    fn push(&mut self, samples: Vec<Complex32>) {
        (**self).push(samples);
    }
}

impl ContinuousSendSink for CollectorSink {
    fn push(&mut self, samples: Vec<Complex32>) {
        self.chunk_lengths.push(samples.len());
        self.samples.extend(samples);
    }
}

/// A message to be modulated continuously.
#[derive(Debug, Clone)]
pub struct ContinuousMessage {
    pub bits: Vec<Bit>,
    /// Pause after the message, in samples.
    pub pause: u32,
    pub modulator_index: usize,
}

impl ContinuousMessage {
    pub fn new(bits: Vec<Bit>, pause: u32, modulator_index: usize) -> Self {
        ContinuousMessage {
            bits,
            pause,
            modulator_index,
        }
    }
}

/// Port of `ContinuousModulator`.
pub struct ContinuousModulator {
    messages: Vec<ContinuousMessage>,
    modulators: Vec<SimModulator>,
    /// -1 or 0 = infinite (Python: `num_repeats <= 0` means forever).
    pub num_repeats: i64,
    abort: Arc<AtomicBool>,
    current_message_index: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl ContinuousModulator {
    /// Python `WAIT_TIMEOUT` (kept for API parity; the sink's blocking push
    /// replaces the timed ring-buffer wait).
    pub const WAIT_TIMEOUT_MS: u64 = 100;

    pub fn new(messages: Vec<ContinuousMessage>, modulators: Vec<SimModulator>, num_repeats: i64) -> Self {
        ContinuousModulator {
            messages,
            modulators,
            num_repeats,
            abort: Arc::new(AtomicBool::new(false)),
            current_message_index: Arc::new(AtomicUsize::new(0)),
            handle: None,
        }
    }

    /// `is_running`.
    pub fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// Index of the message currently being modulated (Python
    /// `current_message_index`, a multiprocessing.Value).
    pub fn current_message_index(&self) -> usize {
        self.current_message_index.load(Ordering::SeqCst)
    }

    /// `start()`: spawn the modulation thread with the given sink.
    pub fn start(&mut self, sink: Box<dyn ContinuousSendSink + Send>) {
        self.abort.store(false, Ordering::SeqCst);
        self.current_message_index.store(0, Ordering::SeqCst);
        let messages = self.messages.clone();
        let modulators = self.modulators.clone();
        let num_repeats = self.num_repeats;
        let abort = Arc::clone(&self.abort);
        let current = Arc::clone(&self.current_message_index);
        self.handle = Some(thread::spawn(move || {
            modulate_continuously(&messages, &modulators, num_repeats, &abort, &current, sink);
        }));
    }

    /// `stop(clear_buffer)`: abort the loop and join the thread. The
    /// sink owns the "buffer", so clearing is the caller's business.
    pub fn stop(&mut self, _clear_buffer: bool) {
        self.abort.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Run the modulation loop synchronously (test-friendly).
    pub fn modulate_continuously_into(&self, sink: &mut dyn ContinuousSendSink) {
        modulate_continuously(
            &self.messages,
            &self.modulators,
            self.num_repeats,
            &self.abort,
            &AtomicUsize::new(0),
            SyncSink(sink),
        );
    }
}

/// Adapter so the synchronous entry can reuse the boxed-sink loop body.
struct SyncSink<'a>(&'a mut dyn ContinuousSendSink);

impl ContinuousSendSink for SyncSink<'_> {
    fn push(&mut self, samples: Vec<Complex32>) {
        self.0.push(samples);
    }
}

/// Port of `modulate_continuously`.
fn modulate_continuously(
    messages: &[ContinuousMessage],
    modulators: &[SimModulator],
    num_repeats: i64,
    abort: &AtomicBool,
    current_message_index: &AtomicUsize,
    mut sink: impl ContinuousSendSink,
) {
    let infinite = num_repeats <= 0;
    let mut repeat = 0i64;
    loop {
        if !infinite && repeat >= num_repeats {
            break;
        }
        if abort.load(Ordering::SeqCst) {
            return;
        }

        for (i, message) in messages.iter().enumerate() {
            if abort.load(Ordering::SeqCst) {
                return;
            }
            current_message_index.store(i, Ordering::SeqCst);
            let modulator = &modulators[message.modulator_index];
            let modulated = modulator.modulate(&message.bits, message.pause);
            sink.push(modulated);
        }

        current_message_index.store(0, Ordering::SeqCst);
        repeat += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulator::config::SimModulator;

    fn msg(bits: &str, pause: u32) -> ContinuousMessage {
        ContinuousMessage::new(
            bits.bytes().map(|b| b - b'0').collect(),
            pause,
            0,
        )
    }

    #[test]
    fn test_finite_repeats_and_pause() {
        // 10 bits, 100 samples per symbol -> 1000 samples + pause
        let messages = vec![msg("1010101010", 500), msg("11110000", 0)];
        let cm = ContinuousModulator::new(messages, vec![SimModulator::default()], 3);
        let mut sink = CollectorSink::new();
        cm.modulate_continuously_into(&mut sink);

        // 3 repeats x 2 messages = 6 chunks
        assert_eq!(sink.chunk_lengths.len(), 6);
        // message 1: 10 bits * 100 sps + 500 pause
        assert_eq!(sink.chunk_lengths[0], 10 * 100 + 500);
        assert_eq!(sink.chunk_lengths[1], 8 * 100);
        assert_eq!(sink.chunk_lengths[2], sink.chunk_lengths[0]);
        assert_eq!(sink.chunk_lengths[3], sink.chunk_lengths[1]);
        assert_eq!(sink.samples.len(), 3 * (1500 + 800));
    }

    #[test]
    fn test_start_stop_thread() {
        let messages: Vec<_> = (0..50).map(|i| msg(&"1".repeat(100), 100 * i)).collect();
        let mut cm = ContinuousModulator::new(messages, vec![SimModulator::default()], -1);
        cm.start(Box::new(CollectorSink::new()));
        assert!(cm.is_running());
        cm.stop(true);
        assert!(!cm.is_running());
    }

    #[test]
    fn test_current_message_index_progresses() {
        struct IndexProbe {
            seen: Vec<usize>,
            count: usize,
        }
        impl ContinuousSendSink for IndexProbe {
            fn push(&mut self, _samples: Vec<Complex32>) {
                self.seen.push(self.count);
                self.count += 1;
            }
        }
        let messages = vec![msg("1010", 0), msg("0101", 0)];
        let cm = ContinuousModulator::new(messages, vec![SimModulator::default()], 2);
        let current = Arc::new(AtomicUsize::new(0));
        let mut probe = IndexProbe {
            seen: vec![],
            count: 0,
        };
        modulate_continuously(
            &cm.messages,
            &cm.modulators,
            2,
            &AtomicBool::new(false),
            &current,
            SyncSink(&mut probe),
        );
        assert_eq!(probe.seen.len(), 4);
        // after the loop the index wraps back to 0 like the Python version
        assert_eq!(current.load(Ordering::SeqCst), 0);
    }
}
