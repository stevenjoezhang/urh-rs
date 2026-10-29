//! Spectrogram tile service — v2 contract section "Spectrogram tiles
//! (P1-remainder)" (docs/ipc-contract.md).
//!
//! Renders STFT tiles of a signal as u8 heat-map frames: the v1 24-byte
//! binary header with dtype 2 (u8) and mode 3 (spectrogram), followed by an
//! `n_bins x freq_bins` row-major payload where row `t` is the time bin and
//! column `f` the frequency bin (DC first). dB values are normalized against
//! a per-(signal, window) reference computed over the *whole* signal, so
//! tiles of the same signal/window are mutually comparable.
//!
//! Memory bounds: STFT frames are computed in bounded chunks (a few MiB of
//! scratch per chunk regardless of signal size), the reference level is
//! cached per (signal, window), and the last
//! [`TILE_CACHE_CAPACITY`] tiles per signal are kept in an LRU. Those caches
//! live in a process-wide static because `SignalRepository` cannot gain
//! fields (contract note) — see [`cache_state`].

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::envelope;
use crate::signal::Signal;
use crate::{AppError, SignalRepository};

/// Payload dtype tag for u8 spectrogram tiles (v1 had only f32 = 1).
pub const DTYPE_U8: u16 = 2;
/// Frame mode value for spectrogram tiles (1 = Iq, 2 = Amplitude in v1).
pub const MODE_SPECTROGRAM: u32 = 3;
/// STFT window sizes the contract allows; hop is always half the window.
pub const ALLOWED_WINDOW_SIZES: [u32; 3] = [256, 1024, 4096];
/// Tiles kept per signal (contract: "the last 8 tiles per signal are cached").
const TILE_CACHE_CAPACITY: usize = 8;
/// Signals with caches kept at once. Bounds the worst-case memory of the
/// tile caches: (payload bytes of up to 8 tiles) x this many signals.
const MAX_SIGNAL_CACHES: usize = 16;
/// Roughly how many samples one STFT chunk covers (~4 MiB of scratch);
/// larger windows simply process fewer frames per chunk.
const CHUNK_SAMPLES: usize = 1_048_576;

// ---------------------------------------------------------------------------
// Frame codec (u8 variant of the v1 layout)
// ---------------------------------------------------------------------------

/// Encode one spectrogram tile frame: the frozen v1 header (magic, version,
/// dtype, request_id, n_bins, mode, flags — all little-endian) followed by
/// the raw u8 payload. `request_id` is opaque and echoed verbatim; it is not
/// part of the tile cache key, so a cached payload is re-stamped with the
/// current request's id by simply encoding a fresh header here.
fn encode_tile_frame(request_id: u32, n_bins: u32, freq_bins: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(envelope::HEADER_LEN + payload.len());
    frame.extend_from_slice(&envelope::MAGIC);
    frame.extend_from_slice(&envelope::FRAME_VERSION.to_le_bytes());
    frame.extend_from_slice(&DTYPE_U8.to_le_bytes());
    frame.extend_from_slice(&request_id.to_le_bytes());
    frame.extend_from_slice(&n_bins.to_le_bytes());
    frame.extend_from_slice(&MODE_SPECTROGRAM.to_le_bytes());
    frame.extend_from_slice(&envelope::FLAGS_NONE.to_le_bytes());
    frame.extend_from_slice(payload);
    debug_assert_eq!(
        frame.len(),
        envelope::HEADER_LEN + n_bins as usize * freq_bins as usize
    );
    frame
}

// ---------------------------------------------------------------------------
// Chunked STFT
// ---------------------------------------------------------------------------

/// Feed STFT frames to `emit(frame_index, spectrum)` for frames
/// `0..n_frames`, where frame `t` covers samples `[base + t*hop, base +
/// t*hop + window_size)` of `samples`, zero-padded past the end.
///
/// Frames are computed in chunks through `urh::spectrogram::Spectrogram`
/// (window formula, FFT normalization and padding semantics stay identical
/// to the library's whole-signal `stft()`), so the scratch memory stays
/// bounded no matter how long the signal is. This is the workhorse behind
/// both the per-(signal, window) reference level and the tiles themselves;
/// tiles anchor at an arbitrary `base` because the contract's time bins
/// start at the requested `start` sample, not at a multiple of the hop.
fn for_each_stft_frame(
    samples: &[urh::Complex32],
    window_size: usize,
    base: usize,
    n_frames: usize,
    mut emit: impl FnMut(usize, &[urh::Complex32]),
) {
    if n_frames == 0 {
        return;
    }
    let hop = window_size / 2;
    let chunk_frames = (CHUNK_SAMPLES / window_size).max(64);
    let mut done = 0;
    while done < n_frames {
        let take = chunk_frames.min(n_frames - done);
        let region_start = base + done * hop;
        let region_len = (take - 1) * hop + window_size;
        // Zero-pad the tail: the region may extend past the signal end when
        // a tile's last windows straddle it. `get` keeps a region start
        // beyond the signal (all-padding) from panicking.
        let mut region: Vec<urh::Complex32> = samples
            .get(region_start..)
            .map(|tail| tail[..region_len.min(tail.len())].to_vec())
            .unwrap_or_default();
        region.resize(region_len, urh::Complex32::default());
        let spectrogram = urh::spectrogram::Spectrogram::new(&region, window_size, 0.5);
        debug_assert_eq!(spectrogram.hop_size(), hop);
        for (k, spectrum) in spectrogram.stft().iter().enumerate() {
            emit(done + k, spectrum.as_slice());
        }
        done += take;
    }
}

/// Number of STFT frames covering `n` samples starting at offset 0 (the
/// whole-signal grid): `(n - ws) / hop + 1`, or a single zero-padded frame
/// when the signal is shorter than the window (mirroring
/// `Spectrogram::stft`'s padding).
fn whole_signal_frames(n: usize, window_size: usize) -> usize {
    if n >= window_size {
        (n - window_size) / (window_size / 2) + 1
    } else {
        1
    }
}

fn db_of(magnitude: f32) -> f32 {
    20.0 * (magnitude + 1e-12).log10()
}

/// Reference dB level: the maximum dB over the **whole signal** for this
/// window size (contract). Only the rendered bins `0..window_size/2` are
/// considered, so a bright mirror-image bin above the Nyquist column can
/// never dim the visible tile. Returns 0.0 instead of -inf for degenerate
/// input (e.g. all-NaN samples) so the byte mapping stays well-defined.
fn compute_db_ref(samples: &[urh::Complex32], window_size: usize) -> f32 {
    let n_frames = whole_signal_frames(samples.len(), window_size);
    let mut db_ref = f32::NEG_INFINITY;
    for_each_stft_frame(samples, window_size, 0, n_frames, |_, spectrum| {
        for bin in &spectrum[..window_size / 2] {
            let db = db_of(bin.norm());
            if db > db_ref {
                db_ref = db;
            }
        }
    });
    if db_ref.is_finite() {
        db_ref
    } else {
        0.0
    }
}

/// Map a dB value to a display byte (contract):
/// `round(255 * clamp((db - db_min) / (db_ref - db_min), 0, 1))` with
/// `db_min = db_ref - 100`. A NaN `db` (NaN input samples) saturates to 0
/// via the float-to-int cast.
fn db_to_byte(db: f32, db_ref: f32) -> u8 {
    let db_min = db_ref - 100.0;
    let ratio = ((db - db_min) / (db_ref - db_min)).clamp(0.0, 1.0);
    (255.0 * ratio).round() as u8
}

/// Render the tile payload: `n_bins` rows of `window_size / 2` u8 columns.
fn tile_payload(
    samples: &[urh::Complex32],
    window_size: usize,
    start: usize,
    n_bins: usize,
    db_ref: f32,
) -> Vec<u8> {
    let freq_bins = window_size / 2;
    let mut payload = vec![0u8; n_bins * freq_bins];
    for_each_stft_frame(samples, window_size, start, n_bins, |t, spectrum| {
        let row = &mut payload[t * freq_bins..(t + 1) * freq_bins];
        for (f, bin) in spectrum[..freq_bins].iter().enumerate() {
            row[f] = db_to_byte(db_of(bin.norm()), db_ref);
        }
    });
    payload
}

// ---------------------------------------------------------------------------
// Process-wide caches
// ---------------------------------------------------------------------------

/// Identity of a tile request apart from the echoed `request_id`.
type TileKey = (u64, u64, u32, u32); // (start, end, freq_bins, window_size)

/// LRU of rendered tile payloads, most recently used at the back.
struct TileLru {
    order: VecDeque<TileKey>,
    tiles: HashMap<TileKey, Vec<u8>>,
}

impl TileLru {
    fn get(&mut self, key: &TileKey) -> Option<Vec<u8>> {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
            self.order.push_back(*key);
        }
        self.tiles.get(key).cloned()
    }

    fn insert(&mut self, key: TileKey, payload: Vec<u8>) {
        if let Some(pos) = self.order.iter().position(|&k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key);
        self.tiles.insert(key, payload);
        while self.order.len() > TILE_CACHE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.tiles.remove(&evicted);
            }
        }
    }
}

/// Per-signal cache: reference levels per window size plus the tile LRU.
struct SignalCache {
    /// Liveness guard for the cache key (the signal's allocation address):
    /// if the signal was closed and its memory freed (or reused by another
    /// allocation), the `Weak` stops upgrading and `purge_dead` discards the
    /// entry.
    weak: Weak<Signal>,
    db_refs: HashMap<u32, f32>,
    tiles: TileLru,
}

impl SignalCache {
    fn new(signal: &Arc<Signal>) -> Self {
        SignalCache {
            weak: Arc::downgrade(signal),
            db_refs: HashMap::new(),
            tiles: TileLru {
                order: VecDeque::new(),
                tiles: HashMap::new(),
            },
        }
    }
}

/// All signal caches, keyed by `Arc::as_ptr(signal)`.
struct CacheState {
    /// LRU over the cache keys, most recently used at the back.
    order: VecDeque<usize>,
    signals: HashMap<usize, SignalCache>,
}

impl CacheState {
    fn touch(&mut self, key: usize) {
        if let Some(pos) = self.order.iter().position(|&k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key);
    }

    /// Drop entries whose signal is gone (closed and fully freed). With the
    /// entry-count cap this is at most `MAX_SIGNAL_CACHES` pointer upgrades.
    fn purge_dead(&mut self) {
        let dead: Vec<usize> = self
            .order
            .iter()
            .copied()
            .filter(|&k| match self.signals.get(&k) {
                Some(cache) => cache.weak.upgrade().is_none(),
                None => true,
            })
            .collect();
        for key in dead {
            self.order.retain(|&k| k != key);
            self.signals.remove(&key);
        }
    }

    fn evict_over_capacity(&mut self) {
        while self.order.len() > MAX_SIGNAL_CACHES {
            if let Some(key) = self.order.pop_front() {
                self.signals.remove(&key);
            }
        }
    }
}

/// Process-wide tile/reference cache.
///
/// Tradeoff (accepted per the contract's caching note): `SignalRepository`
/// is frozen and cannot gain fields, so the caches are a static keyed by
/// signal allocation address. Consequences, all benign here:
///
/// - the cache is shared by every repository in the process (a GUI has one
///   anyway); entries never outlive their signal because the `Weak` guard
///   fails closed after `close`,
/// - an address reused by a *new* signal cannot collide with a stale entry:
///   the old entry's `Weak` only upgrades while the old allocation is alive,
///   and two live allocations never share an address,
/// - worst-case memory is bounded by `MAX_SIGNAL_CACHES` signals x
///   `TILE_CACHE_CAPACITY` tile payloads; a full-range tile of an N-sample
///   signal is N bytes (n_bins x freq_bins = span), so pathological
///   full-range tiles of very large signals are the only way to approach
///   that bound — the UI's viewport-sized requests stay far below it.
static CACHES: OnceLock<Mutex<CacheState>> = OnceLock::new();

fn cache_state() -> &'static Mutex<CacheState> {
    CACHES.get_or_init(|| {
        Mutex::new(CacheState {
            order: VecDeque::new(),
            signals: HashMap::new(),
        })
    })
}

fn signal_key(signal: &Arc<Signal>) -> usize {
    Arc::as_ptr(signal) as usize
}

/// Fetch a cached reference level for `(signal key, window_size)`, or `None`.
/// The cache lock is only held for the map access, never for computation.
/// After `purge_dead`, any entry still present belongs to a live signal at
/// exactly the caller's address (see the ABA note on [`cache_state`]).
fn fetch_db_ref(key: usize, window_size: u32) -> Option<f32> {
    let mut state = cache_state()
        .lock()
        .expect("spectrogram cache lock poisoned");
    state.purge_dead();
    state.touch(key);
    state
        .signals
        .get_mut(&key)?
        .db_refs
        .get(&window_size)
        .copied()
}

/// Store a computed reference level. Overwrites a concurrent computation of
/// the same value (db_ref is a deterministic function of the signal and
/// window, so the race is harmless).
fn store_db_ref(key: usize, signal: &Arc<Signal>, window_size: u32, db_ref: f32) {
    let mut state = cache_state()
        .lock()
        .expect("spectrogram cache lock poisoned");
    state.purge_dead();
    state.touch(key);
    state
        .signals
        .entry(key)
        .or_insert_with(|| SignalCache::new(signal))
        .db_refs
        .insert(window_size, db_ref);
    state.evict_over_capacity();
}

/// Fetch a cached tile payload, if this exact request was rendered recently.
fn fetch_tile(key: usize, tile: &TileKey) -> Option<Vec<u8>> {
    let mut state = cache_state()
        .lock()
        .expect("spectrogram cache lock poisoned");
    state.purge_dead();
    state.touch(key);
    state.signals.get_mut(&key)?.tiles.get(tile)
}

fn store_tile(key: usize, signal: &Arc<Signal>, tile: TileKey, payload: Vec<u8>) {
    let mut state = cache_state()
        .lock()
        .expect("spectrogram cache lock poisoned");
    state.purge_dead();
    state.touch(key);
    state
        .signals
        .entry(key)
        .or_insert_with(|| SignalCache::new(signal))
        .tiles
        .insert(tile, payload);
    state.evict_over_capacity();
}

// ---------------------------------------------------------------------------
// Service API
// ---------------------------------------------------------------------------

impl SignalRepository {
    /// Render one spectrogram tile as a binary frame (contract:
    /// "Spectrogram tiles", v2 P1-remainder).
    ///
    /// `window_size` must be one of [`ALLOWED_WINDOW_SIZES`] and `freq_bins`
    /// must be `window_size / 2` (they stay coupled in v2); the range
    /// follows the envelope rules (`start < end <= sample_count`). The
    /// payload holds `floor((end - start) / hop)` time bins (possibly zero —
    /// a header-only frame is valid) of `freq_bins` u8 columns each, DC
    /// first, normalized against the whole-signal reference level for this
    /// window so tiles are mutually consistent.
    ///
    /// This is a blocking call; on large signals the first tile per window
    /// pays for the whole-signal reference scan (later ones hit the cache).
    /// The Tauri shell must run it on `spawn_blocking`.
    pub fn spectrogram_tile(
        &self,
        signal_id: &str,
        start: u64,
        end: u64,
        freq_bins: u32,
        window_size: u32,
        request_id: u32,
    ) -> Result<Vec<u8>, AppError> {
        // Argument validation first (contract requirement 3), then lookup,
        // then range — the same order the envelope path uses.
        if !ALLOWED_WINDOW_SIZES.contains(&window_size) {
            return Err(AppError::invalid_argument(format!(
                "window_size must be one of {ALLOWED_WINDOW_SIZES:?}, got {window_size}"
            )));
        }
        if freq_bins != window_size / 2 {
            return Err(AppError::invalid_argument(format!(
                "freq_bins must be window_size / 2 = {}, got {freq_bins}",
                window_size / 2
            )));
        }
        // Clone the Arc and compute lock-free (contract requirement 4).
        let signal = self.lookup(signal_id)?;
        let sample_count = signal.sample_count() as u64;
        if start >= end || end > sample_count {
            return Err(AppError::invalid_range(format!(
                "viewport must satisfy start < end <= {sample_count}, got [{start}, {end})"
            )));
        }

        let ws = window_size as usize;
        let hop = ws / 2;
        let key = signal_key(&signal);
        let tile_key = (start, end, freq_bins, window_size);

        if let Some(payload) = fetch_tile(key, &tile_key) {
            return Ok(encode_tile_frame(
                request_id,
                (payload.len() / freq_bins as usize) as u32,
                freq_bins,
                &payload,
            ));
        }

        // Whole-signal reference level, cached per (signal, window).
        let db_ref = match fetch_db_ref(key, window_size) {
            Some(v) => v,
            None => {
                let v = compute_db_ref(signal.samples(), ws);
                store_db_ref(key, &signal, window_size, v);
                v
            }
        };

        let n_bins = ((end - start) / hop as u64) as usize;
        let payload = tile_payload(signal.samples(), ws, start as usize, n_bins, db_ref);
        store_tile(key, &signal, tile_key, payload.clone());
        Ok(encode_tile_frame(
            request_id,
            n_bins as u32,
            freq_bins,
            &payload,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::Signal;
    use urh::iq::IqArray;

    const ASK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/ask.complex");

    fn open(repo: &SignalRepository, path: &str) -> String {
        repo.open(path, None).unwrap().signal_id
    }

    /// A complex exponential at DFT bin `bin` (samples `e^{2πi·bin·n/ws}`).
    fn tone(n: usize, ws: usize, bin: usize) -> Vec<urh::Complex32> {
        (0..n)
            .map(|i| {
                let phase = 2.0 * std::f64::consts::PI * bin as f64 * i as f64 / ws as f64;
                urh::Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn db_to_byte_mapping() {
        let db_ref = -20.0;
        assert_eq!(db_to_byte(db_ref, db_ref), 255);
        assert_eq!(db_to_byte(db_ref + 5.0, db_ref), 255);
        // db_min = db_ref - 100 maps to 0, as does anything below it.
        assert_eq!(db_to_byte(db_ref - 100.0, db_ref), 0);
        assert_eq!(db_to_byte(db_ref - 250.0, db_ref), 0);
        // Halfway maps to round(255/2) = 128.
        assert_eq!(db_to_byte(db_ref - 50.0, db_ref), 128);
        // NaN input saturates to 0 instead of panicking.
        assert_eq!(db_to_byte(f32::NAN, db_ref), 0);
    }

    #[test]
    fn tile_frame_exact_bytes() {
        let frame = encode_tile_frame(0xDEAD_BEEF, 2, 3, &[1, 2, 3, 4, 5, 6]);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"URH1");
        expected.extend_from_slice(&1u16.to_le_bytes()); // version
        expected.extend_from_slice(&2u16.to_le_bytes()); // dtype = u8
        expected.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // request_id
        expected.extend_from_slice(&2u32.to_le_bytes()); // n_bins (time bins)
        expected.extend_from_slice(&3u32.to_le_bytes()); // mode = spectrogram
        expected.extend_from_slice(&0u32.to_le_bytes()); // flags
        expected.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(frame, expected);
        assert_eq!(frame.len(), envelope::HEADER_LEN + 6);
    }

    #[test]
    fn tile_matches_direct_spectrogram() {
        // Two tones of different frequency and amplitude.
        let samples: Vec<urh::Complex32> = (0..5000)
            .map(|i| {
                let a = 2.0 * std::f64::consts::PI * 100.0 * i as f64 / 256.0;
                let b = 2.0 * std::f64::consts::PI * 5.0 * i as f64 / 256.0;
                urh::Complex32::new(
                    a.cos() as f32 + 0.3 * b.cos() as f32,
                    a.sin() as f32 + 0.3 * b.sin() as f32,
                )
            })
            .collect();
        let ws = 256usize;
        let db_ref = compute_db_ref(&samples, ws);

        // Full-range tile must equal the library's whole-signal STFT bin for
        // bin (validates chunking, padding and normalization parity).
        let n = samples.len();
        let n_frames = whole_signal_frames(n, ws);
        let direct = urh::spectrogram::Spectrogram::new(&samples, ws, 0.5).stft();
        let payload = tile_payload(&samples, ws, 0, n_frames, db_ref);
        assert_eq!(payload.len(), n_frames * ws / 2);
        for t in 0..n_frames {
            for f in 0..ws / 2 {
                let want = db_to_byte(db_of(direct[t][f].norm()), db_ref);
                assert_eq!(payload[t * ws / 2 + f], want, "mismatch at ({t}, {f})");
            }
        }

        // Tiles anchored at arbitrary (non-hop-aligned) offsets: frame t
        // must equal a single-window STFT starting at `start + t*hop`...
        for start in [777usize, 1, 129] {
            let tile_bins = 9;
            let payload = tile_payload(&samples, ws, start, tile_bins, db_ref);
            for t in 0..tile_bins {
                let offset = start + t * (ws / 2);
                let window: Vec<urh::Complex32> = samples[offset..offset + ws].to_vec();
                let single = urh::spectrogram::Spectrogram::new(&window, ws, 0.5).stft();
                assert_eq!(single.len(), 1);
                for f in 0..ws / 2 {
                    let want = db_to_byte(db_of(single[0][f].norm()), db_ref);
                    assert_eq!(payload[t * ws / 2 + f], want, "tile@{start} ({t}, {f})");
                }
            }
        }

        // ... including windows that straddle the signal end and are
        // zero-padded (and, in the limit, a window fully past it).
        let start = n - 3 * (ws / 2);
        let tile_bins = 5;
        let payload = tile_payload(&samples, ws, start, tile_bins, db_ref);
        for t in 0..tile_bins {
            let offset = start + t * (ws / 2);
            let mut window: Vec<urh::Complex32> = samples[offset.min(n)..n].to_vec();
            window.resize(ws, urh::Complex32::default());
            let single = urh::spectrogram::Spectrogram::new(&window, ws, 0.5).stft();
            for f in 0..ws / 2 {
                let want = db_to_byte(db_of(single[0][f].norm()), db_ref);
                assert_eq!(payload[t * ws / 2 + f], want, "padded tile ({t}, {f})");
            }
        }
    }

    #[test]
    fn db_ref_matches_full_signal_max() {
        let samples = tone(3000, 256, 42);
        let ws = 256usize;
        let direct = urh::spectrogram::Spectrogram::new(&samples, ws, 0.5).stft();
        let mut want = f32::NEG_INFINITY;
        for frame in &direct {
            for bin in &frame[..ws / 2] {
                want = want.max(db_of(bin.norm()));
            }
        }
        assert_eq!(compute_db_ref(&samples, ws), want);
    }

    #[test]
    fn tone_peaks_at_expected_column() {
        // Bin 64 of a 256-point STFT (DC first, no fftshift — the frontend
        // flips when rendering).
        let repo = SignalRepository::new();
        let id = repo.alloc_id("sig-");
        let signal = Signal::new(
            id.clone(),
            "<memory>",
            IqArray::from_vec(tone(4096, 256, 64)),
            1e6,
        );
        repo.register(id.clone(), signal);
        let frame = repo.spectrogram_tile(&id, 0, 4096, 128, 256, 7).unwrap();
        let payload = &frame[envelope::HEADER_LEN..];
        // Every time bin's brightest column must be 64.
        for row in payload.chunks_exact(128) {
            let (best, _) = row
                .iter()
                .enumerate()
                .max_by_key(|&(f, &v)| (v, std::cmp::Reverse(f)))
                .unwrap();
            assert_eq!(best, 64);
        }
        // And a DC tone peaks at column 0.
        let id2 = repo.alloc_id("sig-");
        let signal = Signal::new(
            id2.clone(),
            "<memory>",
            IqArray::from_vec(tone(4096, 256, 0)),
            1e6,
        );
        repo.register(id2.clone(), signal);
        let frame = repo.spectrogram_tile(&id2, 0, 4096, 128, 256, 8).unwrap();
        let payload = &frame[envelope::HEADER_LEN..];
        let (best, _) = payload[..128]
            .iter()
            .enumerate()
            .max_by_key(|&(f, &v)| (v, std::cmp::Reverse(f)))
            .unwrap();
        assert_eq!(best, 0);
    }

    #[test]
    fn zero_time_bins_is_header_only() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK); // 13710 samples
                                   // A range shorter than the hop (128) renders zero time bins.
        let frame = repo.spectrogram_tile(&id, 0, 100, 128, 256, 9).unwrap();
        assert_eq!(frame.len(), envelope::HEADER_LEN);
        assert_eq!(
            u32::from_le_bytes([frame[12], frame[13], frame[14], frame[15]]),
            0
        );
    }

    #[test]
    fn tile_lru_eviction_and_refresh() {
        let mut lru = TileLru {
            order: VecDeque::new(),
            tiles: HashMap::new(),
        };
        let key = |i: u64| (i, i + 10, 128, 256);
        for i in 0..9 {
            lru.insert(key(i), vec![i as u8]);
        }
        // Capacity 8: the first key was evicted.
        assert!(lru.get(&key(0)).is_none());
        assert_eq!(lru.get(&key(1)), Some(vec![1u8]));
        // Touching key(2) makes key(3) the least recently used.
        assert!(lru.get(&key(2)).is_some());
        lru.insert(key(9), vec![9]);
        assert!(lru.get(&key(3)).is_none());
        assert!(lru.get(&key(2)).is_some());
        assert_eq!(lru.order.len(), 8);
        assert_eq!(lru.tiles.len(), 8);
    }

    #[test]
    fn validation_errors() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        let err = repo
            .spectrogram_tile(&id, 0, 13710, 128, 512, 1)
            .unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{err}");
        let err = repo
            .spectrogram_tile(&id, 0, 13710, 100, 256, 1)
            .unwrap_err();
        assert_eq!(err.code, "invalid-argument", "{err}");
        for (start, end) in [(5u64, 5u64), (10u64, 5u64), (0u64, 13711u64)] {
            let err = repo
                .spectrogram_tile(&id, start, end, 128, 256, 1)
                .unwrap_err();
            assert_eq!(err.code, "invalid-range", "range ({start},{end}): {err}");
        }
        let err = repo
            .spectrogram_tile("sig-99", 0, 100, 128, 256, 1)
            .unwrap_err();
        assert_eq!(err.code, "unknown-signal");
    }

    #[test]
    fn tiles_are_cached_and_deterministic() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        let a = repo
            .spectrogram_tile(&id, 1000, 5000, 128, 256, 11)
            .unwrap();
        let b = repo
            .spectrogram_tile(&id, 1000, 5000, 128, 256, 12)
            .unwrap();
        // Same arguments: identical payload, echoed request id differs.
        assert_eq!(&a[envelope::HEADER_LEN..], &b[envelope::HEADER_LEN..]);
        assert_ne!(&a[8..12], &b[8..12]);
        // Payload size invariant: n_bins x freq_bins.
        let n_bins = u32::from_le_bytes([a[12], a[13], a[14], a[15]]) as usize;
        assert_eq!(n_bins, (5000 - 1000) / 128);
        assert_eq!(a.len(), envelope::HEADER_LEN + n_bins * 128);
    }

    #[test]
    fn full_range_tile_reaches_reference_brightness() {
        let repo = SignalRepository::new();
        let id = open(&repo, ASK);
        let frame = repo
            .spectrogram_tile(&id, 0, 13710, 2048, 4096, 13)
            .unwrap();
        let payload = &frame[envelope::HEADER_LEN..];
        let n_bins = (13710 / 2048) as usize; // 6
        assert_eq!(payload.len(), n_bins * 2048);
        // The loudest sample of the whole signal falls inside this range, so
        // at least one byte must sit at the top of the scale.
        assert!(payload.contains(&255), "no reference-bright pixel in tile");
    }
}
