# GUI IPC & API contract (v1)

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
