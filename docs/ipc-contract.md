# GUI IPC & API contract

> **v2** — adds the P1-remainder … P5 surfaces (spectrogram, analysis, AWRE,
> devices/capture, generator, project, simulator) on top of the v1 slice.
> Sections marked **(v2)** are new; everything else is unchanged v1 behavior.

This is the *implementation contract* for the P0/P1 vertical slice described in
[gui-architecture.md](gui-architecture.md). The Rust service layer
(`crates/urh-app`), the Tauri shell (`apps/desktop/src-tauri`) and the React
frontend (`apps/desktop`) all code against this document. Any change here is a
cross-layer change and must update all three plus this file.

## Model types

```rust
// crates/urh-app — canonical definitions (serde-derived for Tauri JSON IPC)

pub struct SignalMeta {
    pub signal_id: String,   // "sig-<n>", unique per repository instance
    pub path: String,        // absolute path as opened
    pub display_name: String,// file name only
    pub format: String,      // "complex" | "complex16s" | "complex16u" | "complex32s" | "complex32u"
    pub sample_count: u64,   // complex samples in the signal
    pub sample_rate: f64,    // Hz; metadata only, default 1e6 (URH default)
    pub duration_secs: f64,  // sample_count / sample_rate
}

pub enum WaveformMode { Iq = 1, Amplitude = 2 }   // 3=Phase, 4=Frequency reserved
```

Integer range convention: **half-open sample intervals `[start, end)`**, integer
sample indices everywhere in state; time is always *derived* (index / rate).
`u64` sample counts are transferred as JSON numbers — exact up to 2^53
(9e15 samples ≈ 72 PB of float32 IQ, unreachable), so no string/BigInt
adaptation is needed for v1. If that ever changes, switch `start`/`end` to
decimal strings in one atomic change to this contract.

## Service API (`SignalRepository`)

```rust
impl SignalRepository {                       // all &self, Send + Sync
    fn new() -> Self;
    fn open(&self, path: &str, sample_rate: Option<f64>) -> Result<SignalMeta, AppError>;
    fn close(&self, signal_id: &str) -> Result<(), AppError>;
    fn meta(&self, signal_id: &str) -> Result<SignalMeta, AppError>;
    fn list(&self) -> Vec<SignalMeta>;
    fn set_sample_rate(&self, signal_id: &str, rate: f64) -> Result<SignalMeta, AppError>;
    fn waveform_envelope(&self, signal_id: &str, start: u64, end: u64, bins: u32,
                         mode: WaveformMode, request_id: u32) -> Result<Vec<u8>, AppError>;
}
```

`AppError { code: &'static str, message: String }` with codes:
`"unknown-signal"`, `"file-not-found"`, `"invalid-range"`, `"invalid-argument"`,
`"io"`, `"internal"`. Frontends must branch on `code`, never parse `message`.

Envelope semantics — these are **hard requirements**:

1. Per-bin min/max are the *exact* min/max over the raw samples of that bin's
   sub-range. Peaks must never be lost to decimation ("every Nth point" is
   forbidden). Implement via precomputed per-block min/max (block = 256
   samples, for I, Q and |s|) with raw-scanned partial block edges, falling
   back to a direct raw scan when the bin width is small.
2. If `bins >= end - start`, return exactly one bin per sample with `min == max ==
   sample value` (degenerate envelope = raw waveform; the renderer uses the
   same path). The frame's `n_bins` field is the *clamped* count
   (`end - start`), not the requested count; decoders must use `n_bins` from
   the header, never the requested value.
3. Validation: `start < end <= sample_count`, `1 <= bins <= 65536`; violations
   return `invalid-range`, not panics. Unknown ids return `unknown-signal`.
4. Queries must not hold a global lock while computing (clone an `Arc` to the
   signal, compute lock-free).
5. `request_id` is opaque to the backend and echoed verbatim in the frame
   header. The frontend uses it to drop stale responses.

MVP storage: whole file in memory via `urh::iq::IqArray::from_file`
(10M samples = 80 MB — acceptable for the slice). The repository API is shaped
so a paged/mmap backend can replace it later (ADR-002).

## Binary frame (waveform responses)

All multi-byte fields little-endian. Payload starts at offset 24, so every
`f32` is 4-byte aligned.

| offset | size | field |
|---|---|---|
| 0  | 4 | magic `b"URH1"` |
| 4  | 2 | version = 1 |
| 6  | 2 | dtype = 1 (IEEE-754 f32) |
| 8  | 4 | request_id (u32, echo) |
| 12 | 4 | n_bins (u32) |
| 16 | 4 | mode (u32: 1 = Iq, 2 = Amplitude) |
| 20 | 4 | flags = 0 (reserved) |
| 24 | 16·n | Iq payload: per bin `[min_i, max_i, min_q, max_q]` |
| 24 | 8·n  | Amplitude payload: per bin `[min_mag, max_mag]` |

Bin `k` covers samples `[start + k·w, min(start + (k+1)·w, end))` where
`w = ceil((end - start) / n_bins)`; the last bin may be narrower. Bins are
uniform in sample space (not time space).

Frontend decoding must validate magic, version, dtype and `n_bins` against the
actual byte length, and reject mismatches with a specific error.

## Tauri commands (v1)

```
open_signal(path: String, sample_rate: Option<f64>) -> SignalMeta
close_signal(signal_id: String) -> ()
list_signals() -> SignalMeta[]
set_sample_rate(signal_id: String, sample_rate: f64) -> SignalMeta
get_waveform_envelope(signal_id: String, start: u64, end: u64, bins: u32,
                      mode: u32, request_id: u32) -> ArrayBuffer   // raw frame above
```

Errors serialize as `{ code: String, message: String }` (Tauri rejects the
invoke promise with it). `get_waveform_envelope` returns
`tauri::ipc::Response::new(Vec<u8>)` — raw bytes, never base64/JSON arrays.

Command arguments bind **snake_case** on the wire: every command is declared
`#[tauri::command(rename_all = "snake_case")]` (Tauri's default would be
camelCase), and `SignalMeta` serializes with serde's default snake_case field
names. The frontend sends snake_case keys; unknown extra keys are ignored.

## Request discipline (frontend)

View changes increment a monotonically increasing `request_id` (per signal).
Responses whose id no longer matches the current view are dropped on arrival.
No queuing of intermediate states: only the latest view is ever requested
beyond the request already in flight.

---

# v2 additions

## Spectrogram tiles **(v2, P1-remainder)**

```rust
impl SignalRepository {
    fn spectrogram_tile(&self, signal_id: &str, start: u64, end: u64,
                         freq_bins: u32, window_size: u32, request_id: u32)
        -> Result<Vec<u8>, AppError>;
}
```

- `window_size` ∈ {256, 1024, 4096} (STFT window, `urh::spectrogram`); hop is
  `window_size / 2` (fixed). `freq_bins` must be `window_size / 2` (v2 keeps
  them coupled; decoupling later is a contract change).
- The returned frame reuses the v1 binary header with **dtype = 2 (u8)** and
  **mode = 3 (spectrogram)**; `n_bins` is the *time-bin* count
  `floor((end - start) / hop)` (may be 0 → payload empty; that is valid).
- Payload: `n_bins × freq_bins` u8, row-major, row `t` = time bin `t`, column
  `f` = frequency bin `f` (DC first, i.e. low frequencies at row start —
  renderers flip as needed).
- dB mapping: byte = `round(255 · clamp((db − db_min) / (db_ref − db_min), 0, 1))`
  where `db = 20·log10(|STFT| + 1e-12)`, `db_ref` = max dB over the **whole
  signal** for this `window_size` (computed once, cached), `db_min = db_ref − 100`.
  Tiles of the same signal/window are therefore mutually consistent.
- Validation: same rules as envelopes (`start < end <= sample_count`), plus
  `window_size` ∈ allowed set, else `invalid-argument`.
- Tiles are computed with `spawn_blocking`; the last 8 tiles per signal are
  cached (LRU keyed by the full argument tuple).

## Protocol analysis **(v2, P2)**

```rust
pub struct AnalysisParams {          // all fields optional = "auto"
    pub modulation: Option<String>,  // "ASK"|"FSK"|"PSK"|"GFSK"|"OQPSK"
    pub bit_length: Option<u32>,
    pub center: Option<f32>,
    pub noise_threshold: Option<f32>,
    pub tolerance: Option<u32>,
}

pub struct EstimatedParams {
    pub modulation: String,
    pub bit_length: u32,
    pub center: f32,
    pub noise_threshold: f32,
    pub tolerance: u32,
}

pub struct MessageInfo {
    pub index: usize,
    pub bit_count: usize,
    pub pause: u64,            // pause AFTER the message, samples
    pub plain_bits: String,    // "0101…" (undecoded)
    pub start_sample: u64,     // absolute sample index in the signal
    pub end_sample: u64,       // exclusive; derived from bit_sample_pos + pause
}

pub struct AnalysisResult {
    pub params: EstimatedParams,
    pub messages: Vec<MessageInfo>,
}

impl SignalRepository {
    fn analyze(&self, signal_id: &str, range: Option<(u64, u64)>,
               params: &AnalysisParams, job_id: u32) -> Result<AnalysisResult, AppError>;
    fn cancel_analysis(&self, job_id: u32) -> bool;
}
```

- Analysis is the CLI flow (`src/bin/urh-cli.rs::run_analysis`) made
  parameterizable: auto-detect fills every `None`; user-supplied fields are
  used verbatim (mirroring `estimate_with_modulation` for fixed modulation).
- `range = Some((start, end))` restricts analysis to that half-open interval;
  returned `start_sample`/`end_sample` are **absolute** (offset added back).
- Message sample extent: `start_sample = bit_sample_pos[0]`,
  `end_sample = bit_sample_pos.last()` (clamped to the analyzed extent). In
  this port the trailing `bit_sample_pos` entry already folds in the pause —
  mid-signal messages end `[..., pause_start, pause_end]`, the final message
  ends `[..., data_end + pause]` — so pause must NOT be added again. Bit
  positions are absolute within the analyzed range; add the range offset
  when present.
- Runs on `spawn_blocking` under a job registry: `job_id` identifies the job,
  `cancel_analysis(job_id)` sets a cancellation flag → the job returns
  `Err(AppError { code: "cancelled" })` at the next checkpoint. A second
  `analyze` with the same `job_id` while running is rejected with
  `invalid-argument`. Concurrent jobs on *different* signals are allowed.
- New `AppError` code: `"cancelled"`.

## AWRE field labels **(v2, P4)**

```rust
pub struct FieldLabel {
    pub message_index: usize,  // message the label belongs to
    pub name: String,          // "preamble" | "sync" | "length" | …
    pub bit_start: usize,      // bit index within the message
    pub bit_end: usize,        // exclusive
    pub color_index: usize,
}

impl SignalRepository {
    fn run_awre(&self, signal_id: &str, range: Option<(u64, u64)>,
                params: &AnalysisParams) -> Result<Vec<FieldLabel>, AppError>;
}
```

Runs `urh::awre` (FormatFinder) over the analysis messages of that
signal/range; labels carry **bit** positions (protocol table renders them
directly; the waveform converts bits → samples via `samples_per_symbol`).

## Device capture **(v2, P3)**

```rust
pub struct DeviceInfo {
    pub name: String,        // e.g. "RTL-SDR", "SoundCard", "mock", "rtl-tcp"
    pub backend: String,     // "native-dlopen" | "cpal" | "network" | "mock"
    pub available: bool,     // driver lib loadable / device openable
    pub detail: String,      // "" or reason unavailable
}

pub struct CaptureConfig {
    pub device: String,          // DeviceInfo.name
    pub sample_rate: f64,
    pub frequency: f64,
    pub bandwidth: f64,
    pub gain: f64,
    pub record: bool,            // append samples into a repository signal
    // rtl-tcp only:
    pub host: Option<String>,
    pub port: Option<u16>,
}

pub struct CaptureHandle {
    pub capture_id: String,      // "cap-<n>"
    pub signal_id: Option<String>, // Some("rec-<n>") iff record = true
}

pub struct CaptureStats {
    pub dropped_preview_batches: u64,
    pub recorded_samples: u64,
    pub overflow_detected: bool, // record queue overflowed → data loss
}

impl SignalRepository {           // DeviceManager lives inside the repo
    fn list_devices(&self) -> Vec<DeviceInfo>;
    fn start_capture(&self, cfg: &CaptureConfig) -> Result<CaptureHandle, AppError>;
    fn stop_capture(&self, capture_id: &str) -> Result<CaptureStats, AppError>;
    fn capture_stats(&self, capture_id: &str) -> Result<CaptureStats, AppError>;
}
```

Events (Tauri `emit`, frontend listens):

- `capture://batch` — payload `{ capture_id: String, seq: u64, body: String }`.
  Tauri 2 events are always serde_json-serialized, so the raw IQ bytes ride
  as **base64** of the interleaved complex f32 batch (`body`); the frontend
  decodes and validates (length > 0, multiple of 8). `seq` detects gaps
  (dropped preview batches). Preview batches are ≥ 4096 samples.
- `capture://stats` — payload `{ capture_id: String, dropped_preview_batches:
  u64, recorded_samples: u64, overflow_detected: bool }` (flattened), emitted
  at ~1 Hz while capturing.

Queue discipline (hard requirements):

1. Preview channel: bounded queue (default 16 batches); on overflow the
   **oldest** batch is dropped and `dropped_preview_batches` increments.
   Preview may lose data silently — it is display-only.
2. Record channel: bounded queue (default 4096 batches); on overflow the
   capture sets `overflow_detected = true` and **stops** (never silently
   drops recorded samples).
3. Recording accumulates in a buffer owned by the capture; on `stop_capture`
   the buffer is registered in the repository (`signal_id = "rec-<n>"`,
   `format = "complex"`), so the normal waveform / spectrogram / analysis
   paths work on the result. Live preview **during** capture comes from the
   batch events, not from the repository.
4. `mock` device is always available: synthesizes a repeating ASK pattern
  (bits `10101100`, 100 samples/bit) at the configured sample rate — it is
  the test fixture *and* the no-hardware demo.

## Generator **(v2, P4)**

```rust
pub struct GeneratorParams {
    pub modulation: String,     // "ASK"|"FSK"|"PSK"|"GFSK"|"OQPSK"
    pub bits: String,           // "0101…" (decoded bits; no encoding chain in v2)
    pub samples_per_bit: u32,
    pub pause_samples: u64,
    pub center: f32,
}

impl SignalRepository {
    fn generate(&self, params: &GeneratorParams) -> Result<SignalMeta, AppError>; // → "gen-<n>" signal
    fn save_signal_as(&self, signal_id: &str, path: &str) -> Result<(), AppError>; // .complex f32
}
```

- `generate` uses `urh::modulation` and registers a transient signal in the
  repository so the standard waveform/spectrogram views render it; the
  frontend offers "save as .complex" via `save_signal_as`.
- Sending over a device is **explicit only**: the UI composes a modulated
  signal, then the user starts a capture-like send session (v2: not
  implemented; documented in docs/gui-status.md).

## Project **(v2, P4)**

```rust
pub struct ProjectSignal { pub path: String, pub sample_rate: f64 }
pub struct ProjectState {
    pub version: u32,               // = 2
    pub signals: Vec<ProjectSignal>,
    pub active_signal_id: Option<String>,
    pub analysis: Vec<(String, AnalysisParams)>,  // signal_id → params
    pub view: Vec<(String, u64, u64, u32)>,       // signal_id, start, end, mode
}

impl SignalRepository {
    fn save_project(&self, path: &str, state: &ProjectState) -> Result<(), AppError>;
    fn load_project(&self, path: &str) -> Result<ProjectState, AppError>;
}
```

`.urhproj` files are pretty-printed JSON. `load_project` only *parses*; the
frontend re-opens files via `open_signal` and re-applies params/views (open
failures are per-signal errors the UI surfaces, not a failed load).

## Simulator **(v2, P5)**

```rust
pub struct SimRuleInfo {
    pub index: usize,
    pub name: String,          // rule display name
    pub kind: String,          // "rule" | "action" | … as in the engine
    pub summary: String,       // human-readable one-liner from the config
}

pub struct SimTranscriptEntry {
    pub timestamp: f64,
    pub source: String,
    pub destination: String,
    pub bits: String,
    pub action: String,        // e.g. "send", "log"
}

impl SignalRepository {
    fn load_simulator(&self, path: &str) -> Result<Vec<SimRuleInfo>, AppError>;
    fn run_simulation(&self, path: &str, timeout_secs: u64) -> Result<Vec<SimTranscriptEntry>, AppError>;
}
```

v2 is **read + run**: load a `.simulator` XML (the engine's config format),
list its rules, run it against its configured source, return the transcript.
Editing/saving simulator configs and wiring live signals as simulation
sources are out of scope (documented in docs/gui-status.md).

## Tauri commands (v2 additions)

```
get_spectrogram_tile(signal_id, start, end, freq_bins, window_size, request_id) -> ArrayBuffer
analyze_signal(signal_id, range_start: Option<u64>, range_end: Option<u64>,
               params: AnalysisParams, job_id: u32) -> AnalysisResult
cancel_analysis(job_id: u32) -> bool
run_awre(signal_id, range_start: Option<u64>, range_end: Option<u64>,
         params: AnalysisParams) -> FieldLabel[]
list_devices() -> DeviceInfo[]
start_capture(config: CaptureConfig) -> CaptureHandle
stop_capture(capture_id: String) -> CaptureStats
capture_stats(capture_id: String) -> CaptureStats
generate(params: GeneratorParams) -> SignalMeta
save_signal_as(signal_id: String, path: String) -> ()
save_project(path: String, state: ProjectState) -> ()
load_project(path: String) -> ProjectState
load_simulator(path: String) -> SimRuleInfo[]
run_simulation(path: String, timeout_secs: u64) -> SimTranscriptEntry[]
```

Same binding rules as v1: every command `#[tauri::command(rename_all =
"snake_case")]`, snake_case JSON keys, errors as `{code, message}`.

## Frontend structure conventions **(v2)**

- `src/bridge/api.ts` + `src/bridge/protocol.ts` are the **only** files that
  touch `invoke`/`listen`; view-models and components import from there.
  New commands get typed wrappers in `api.ts`; new frame types go in
  `protocol.ts` with decoding + validation + unit tests.
- Feature view-models live in `src/app/use*.ts` (React hooks, testable
  without DOM); rendering in `src/render/` (WebGL/canvas, thin); layout in
  `src/components/` (presentational).
- Selection is a `(start, end) | null` sample-interval in the active view
  model; every consumer (zoom, analysis, protocol-table highlight) reads it
  from there. One source of truth.
