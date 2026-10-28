//! Sound card backend — port of `urh.dev.native.SoundCard.py` on the
//! **cpal** crate (replacing pyaudio). This is the only native backend that
//! does *not* go through `libloading`: cpal is linked statically into the
//! crate.
//!
//! Why it does not implement [`crate::devices::Device`]: SoundCard.py has no
//! frequency/bandwidth/gain parameters at all (`DEVICE_METHODS = dict()`,
//! `device_parameters` is only sample rate + identifier) and no device
//! discovery by serial — it is driven by a plain sample rate and an
//! input/output flag. Squeezing it into the SDR-shaped trait would require
//! inventing unsupported semantics, so it keeps its own API mirroring the
//! Python class method-for-method.
//!
//! IQ mapping: SoundCard.py opens pyaudio with `channels=2`, `paFloat32`,
//! and `bytes_to_iq` reshapes the interleaved floats into I/Q pairs. The
//! default Rust mode ([`SoundCardMode::Real`]) instead captures mono audio
//! and maps each real sample to `Complex32 { re: sample, im: 0.0 }` — the
//! "audio as real signal" view; [`SoundCardMode::StereoIQ`] reproduces the
//! exact Python interleaved-stereo behavior. TX always takes the real part
//! (and the imaginary part in stereo mode).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::devices::{DeviceError, IqChunk};

/// Default sample rate (SoundCard.SAMPLE_RATE).
pub const DEFAULT_SAMPLE_RATE: f64 = 48000.0;
/// SoundCard.CHUNK_SIZE (frames per pyaudio buffer).
pub const CHUNK_SIZE: usize = 1024;

/// How captured audio maps to complex samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundCardMode {
    /// Mono capture: each real f32 sample becomes `Complex32 { re, im: 0 }`.
    Real,
    /// Stereo capture, exact SoundCard.py behavior: interleaved L/R floats
    /// are interpreted as I/Q pairs (`bytes_to_iq` reshape(-1, 2)).
    StereoIQ,
}

/// A sound card device opened for input or output at a fixed sample rate.
pub struct SoundCardDevice {
    device: cpal::Device,
    sample_rate: f64,
    is_input: bool,
    mode: SoundCardMode,
    stream: Option<cpal::Stream>,
    /// TX sample queue shared with the output-stream callback.
    tx_queue: Arc<Mutex<VecDeque<f32>>>,
    tx_running: Arc<AtomicBool>,
    name: String,
}

impl SoundCardDevice {
    /// Enumerate all input and output device names of the default host
    /// (pyaudio's device listing). Guaranteed not to panic even without
    /// audio hardware — returns an empty list instead.
    pub fn list_devices() -> Vec<String> {
        // cpal enumeration itself is panic-free in practice, but the
        // "must not panic without hardware" guarantee is load-bearing for
        // callers, so guard the whole body.
        std::panic::catch_unwind(|| {
            let host = cpal::default_host();
            let mut names = Vec::new();
            if let Ok(inputs) = host.input_devices() {
                names.extend(inputs.filter_map(|d| d.name().ok()));
            }
            if let Ok(outputs) = host.output_devices() {
                names.extend(outputs.filter_map(|d| d.name().ok()));
            }
            names
        })
        .unwrap_or_default()
    }

    /// Open a sound card. `device` selects the cpal device by name (exact
    /// match first, then substring); `None` uses the default input/output
    /// device. `sample_rate` is the requested rate, `is_input` selects
    /// direction, matching SoundCard.py's input/output stream setup.
    pub fn open(
        device: Option<&str>,
        sample_rate: f64,
        is_input: bool,
    ) -> Result<Self, DeviceError> {
        Self::open_with_mode(device, sample_rate, is_input, SoundCardMode::Real)
    }

    /// Like [`open`] but with an explicit IQ mapping mode.
    pub fn open_with_mode(
        device: Option<&str>,
        sample_rate: f64,
        is_input: bool,
        mode: SoundCardMode,
    ) -> Result<Self, DeviceError> {
        let host = cpal::default_host();
        let (selected, name) = match device.filter(|s| !s.is_empty()) {
            Some(wanted) => {
                let devices: Vec<cpal::Device> = if is_input {
                    host.input_devices().map_err(no_host)?.collect()
                } else {
                    host.output_devices().map_err(no_host)?.collect()
                };
                let exact = devices
                    .iter()
                    .find(|d| d.name().map(|n| n == wanted).unwrap_or(false));
                let partial = devices
                    .iter()
                    .find(|d| d.name().map(|n| n.contains(wanted)).unwrap_or(false));
                match exact.or(partial) {
                    Some(d) => (d.clone(), d.name().unwrap_or_else(|_| wanted.to_string())),
                    None => {
                        return Err(DeviceError::OpenFailed(format!(
                            "sound device {wanted:?} not found"
                        )))
                    }
                }
            }
            None => {
                let d = if is_input {
                    host.default_input_device()
                } else {
                    host.default_output_device()
                }
                .ok_or_else(|| {
                    DeviceError::OpenFailed("no default sound device available".into())
                })?;
                let n = d.name().unwrap_or_default();
                (d, n)
            }
        };

        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            DEFAULT_SAMPLE_RATE
        };

        Ok(Self {
            device: selected,
            sample_rate,
            is_input,
            mode,
            stream: None,
            tx_queue: Arc::new(Mutex::new(VecDeque::new())),
            tx_running: Arc::new(AtomicBool::new(false)),
            name,
        })
    }

    /// The cpal device name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The effective sample rate.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Negotiate a stream config: prefer the exact requested sample rate,
    /// fall back to the device default config (its rate may differ — the
    /// deviation from SoundCard.py, where pyaudio hard-fails on an
    /// unsupported rate, is documented on [`Self::start_rx`]).
    fn negotiate_config(
        device: &cpal::Device,
        is_input: bool,
        sample_rate: f64,
        channels: usize,
    ) -> Result<cpal::StreamConfig, DeviceError> {
        let configs = if is_input {
            device.default_input_config()
        } else {
            device.default_output_config()
        };
        let default_cfg = configs.map_err(|e| DeviceError::StreamError(e.to_string()))?;
        let default = default_cfg.config();
        let target = sample_rate.round().clamp(1.0, f64::from(u32::MAX)) as u32;
        if default.sample_rate.0 == target && default.channels as usize == channels {
            return Ok(default);
        }
        // scan supported configs for an exact rate match
        let supported: Vec<cpal::SupportedStreamConfig> = if is_input {
            device
                .supported_input_configs()
                .map(|it| it.map(|r| r.with_max_sample_rate()).collect())
        } else {
            device
                .supported_output_configs()
                .map(|it| it.map(|r| r.with_max_sample_rate()).collect())
        }
        .unwrap_or_default();
        let mut best: Option<cpal::StreamConfig> = None;
        for cfg in supported {
            let c = cfg.config();
            if c.channels as usize != channels {
                continue;
            }
            if c.sample_rate.0 == target {
                return Ok(c);
            }
            // remember the closest supported rate as fallback
            if best
                .as_ref()
                .map(|b| (b.sample_rate.0 as i64 - target as i64).abs()
                    > (c.sample_rate.0 as i64 - target as i64).abs())
                .unwrap_or(true)
            {
                best = Some(c);
            }
        }
        Ok(best.unwrap_or(cpal::StreamConfig {
            channels: channels as u16,
            sample_rate: cpal::SampleRate(target),
            buffer_size: default.buffer_size,
        }))
    }

    /// Start receiving audio as IQ chunks (`prepare_sync_receive` +
    /// `receive_sync` loop): builds a cpal input stream and forwards each
    /// buffer through `sink`.
    ///
    /// Sample-rate deviation from SoundCard.py: pyaudio fails hard when the
    /// requested rate is unsupported; we first try the exact rate and fall
    /// back to the closest supported one so RX still works. Use
    /// [`Self::sample_rate`] / the returned effective rate to check.
    pub fn start_rx(&mut self, sink: std::sync::mpsc::Sender<IqChunk>) -> Result<f64, DeviceError> {
        if !self.is_input {
            return Err(DeviceError::Unsupported(
                "SoundCardDevice opened for output; cannot start_rx",
            ));
        }
        self.stop_stream();
        let channels = match self.mode {
            SoundCardMode::Real => 1usize,
            SoundCardMode::StereoIQ => 2,
        };
        let config = Self::negotiate_config(&self.device, true, self.sample_rate, channels)?;
        let effective_rate = f64::from(config.sample_rate.0);
        self.sample_rate = effective_rate;

        let mode = self.mode;
        // f32 interleaved input data -> Complex32 chunks.
        let stream = self
            .device
            .build_input_stream(
                &config,
                move |data: &[f32], _| {
                    // sound card callbacks must not panic; all indexing below
                    // is bounds-checked
                    let chunk: IqChunk = match mode {
                        SoundCardMode::Real => data
                            .iter()
                            .map(|&x| crate::Complex32::new(x, 0.0))
                            .collect(),
                        SoundCardMode::StereoIQ => data
                            .chunks_exact(2)
                            .map(|p| crate::Complex32::new(p[0], p[1]))
                            .collect(),
                    };
                    let _ = sink.send(chunk); // receiver gone -> drop silently
                },
                |_err| {}, // pyaudio also does not surface stream errors
                None,
            )
            .map_err(|e| DeviceError::StreamError(e.to_string()))?;
        stream
            .play()
            .map_err(|e| DeviceError::StreamError(e.to_string()))?;
        self.stream = Some(stream);
        Ok(effective_rate)
    }

    /// Send samples to the output device (`prepare_sync_send` +
    /// `send_sync`): plays `samples` and blocks until they were handed to
    /// the sound card. The real part is taken; in [`SoundCardMode::StereoIQ`]
    /// the imaginary part is interleaved, matching `iq_to_bytes`.
    ///
    /// Mirroring SoundCard.py's zero-padding (`ljust(CHUNK_SIZE * 8)`), the
    /// stream keeps playing zeros briefly after the samples so the final
    /// buffer is not truncated mid-chunk.
    pub fn send(&mut self, samples: &[crate::Complex32]) -> Result<(), DeviceError> {
        if self.is_input {
            return Err(DeviceError::Unsupported(
                "SoundCardDevice opened for input; cannot send",
            ));
        }
        self.stop_stream();
        let channels = match self.mode {
            SoundCardMode::Real => 1usize,
            SoundCardMode::StereoIQ => 2,
        };
        let config = Self::negotiate_config(&self.device, false, self.sample_rate, channels)?;
        self.sample_rate = f64::from(config.sample_rate.0);

        let queue = Arc::clone(&self.tx_queue);
        queue.lock().unwrap().clear();
        {
            let mut q = queue.lock().unwrap();
            match self.mode {
                SoundCardMode::Real => q.extend(samples.iter().map(|c| c.re)),
                SoundCardMode::StereoIQ => {
                    q.extend(samples.iter().flat_map(|c| [c.re, c.im]))
                }
            }
            // pad with zeros up to one full chunk (send_sync's ljust)
            while q.len() < CHUNK_SIZE * channels {
                q.push_back(0.0);
            }
        }

        let running = Arc::clone(&self.tx_running);
        running.store(true, Ordering::SeqCst);

        let stream = self
            .device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    // must not panic: lock poisoning is the only panic path,
                    // handled below; queue indexing is bounds-checked
                    let mut q = match queue.lock() {
                        Ok(q) => q,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    for slot in data.iter_mut() {
                        *slot = q.pop_front().unwrap_or(0.0);
                    }
                },
                |_err| {},
                None,
            )
            .map_err(|e| DeviceError::StreamError(e.to_string()))?;
        stream
            .play()
            .map_err(|e| DeviceError::StreamError(e.to_string()))?;
        self.stream = Some(stream);

        // Block until all samples were consumed by the callback (plus a
        // small drain margin), then stop — SoundCard.send_sync is
        // synchronous too.
        let total = samples.len().max(CHUNK_SIZE);
        let dur = Duration::from_secs_f64(total as f64 / self.sample_rate.max(1.0) + 0.1);
        std::thread::sleep(dur);
        self.stop_stream();
        Ok(())
    }

    /// Stop the running stream (`shutdown_device`: stop_stream + close).
    pub fn stop(&mut self) {
        self.stop_stream();
    }

    fn stop_stream(&mut self) {
        if let Some(stream) = self.stream.take() {
            drop(stream); // cpal Stream stops on drop
        }
        self.tx_running.store(false, Ordering::SeqCst);
    }
}

fn no_host(e: cpal::DevicesError) -> DeviceError {
    DeviceError::OpenFailed(format!("could not enumerate sound devices: {e}"))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_devices_never_panics() {
        // May be empty on machines without audio hardware; on this Mac it
        // will usually contain the built-in devices.
        let devices = SoundCardDevice::list_devices();
        for d in &devices {
            assert!(!d.is_empty());
        }
    }

    #[test]
    fn open_unknown_device_fails_cleanly() {
        let result = SoundCardDevice::open(Some("definitely-not-a-sound-card-42"), 48000.0, true);
        match result {
            Err(DeviceError::OpenFailed(_)) => {}
            _ => panic!("expected OpenFailed"),
        }
    }

    #[test]
    fn default_sample_rate_constant() {
        assert_eq!(DEFAULT_SAMPLE_RATE, 48000.0);
        assert_eq!(CHUNK_SIZE, 1024);
    }
}
