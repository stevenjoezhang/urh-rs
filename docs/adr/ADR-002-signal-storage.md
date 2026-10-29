# ADR-002: Signal storage — whole-file in memory for the MVP

Status: accepted (2026-10-29)

## Decision

`SignalRepository::open` loads the entire file via
`urh::iq::IqArray::from_file` (float32 in memory) and precomputes per-block
min/max LOD (block = 256 samples) for I, Q and magnitude. Viewport queries
(`waveform_envelope`) are computed from the in-memory data.

## Rationale

The P1 acceptance target is 10M complex samples ≈ 80 MB resident — well within
a desktop app's budget. Exact per-bin min/max over a precomputed block LOD is
O(viewport) with tiny constants, which is what keeps pan/zoom at frame rate;
paging would not change that.

## Consequences and exit criteria

- Very large files (>1G samples, multi-GB) will exhaust memory; that is out
  of scope for the slice.
- The repository API (`open`/`meta`/`waveform_envelope` with integer sample
  ranges) is shaped so a paged or mmap-backed `SignalStore` can replace the
  in-memory array behind the same interface — the switch becomes necessary
  when we target files that do not fit RAM, not before.
- Integer formats are normalized to float32 on load exactly like URH does;
  the LOD therefore sees the same values the analysis pipeline sees.
