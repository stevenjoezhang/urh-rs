# ADR-004: Coordinates, ranges and the binary IPC frame

Status: accepted (2026-10-29)

## Decision

- All persistent coordinates are **integer sample indices**; time is always
  derived (index / sample_rate) and never stored.
- All sample ranges are **half-open `[start, end)`** — including selections,
  view state and message bit ranges mapped onto the signal.
- The frontend renders from one shared `screen ↔ sample` transform; rendering
  and hit-testing use the same functions, so overlays cannot drift from data.
- Waveform payloads cross IPC as a raw little-endian binary frame with a
  fixed 24-byte header (magic `URH1`, version, dtype, echoed `request_id`,
  `n_bins`, mode, flags) — defined precisely in
  [../ipc-contract.md](../ipc-contract.md). Never JSON/base64 arrays.
- `u64` sample counts travel as JSON numbers: exact up to 2^53 samples
  (≈72 PB of float32 IQ), unreachable for real files. If that assumption is
  ever invalidated, switch to decimal strings in one atomic contract change.
- Every view change bumps a monotonic `request_id`; stale responses are
  dropped on arrival, and requests are coalesced per animation frame.

## Consequences

- Bit/message ranges from the protocol layer map onto samples through an
  explicit table (P2), keeping the waveform, protocol table and generator on
  one coordinate system.
- The frame format is versioned from day one; `version != 1` is a hard error
  on both sides of the bridge.
