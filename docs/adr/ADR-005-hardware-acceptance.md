# ADR-005: Hardware acceptance devices

Status: deferred (to be decided in P3) — recorded 2026-10-29

## Issue

P3 (live capture) needs at least one real device to validate the RX path end
to end and one transceiver to validate TX state handling.

## Direction (not yet binding)

- RX path: RTL-SDR — cheapest, widest user base, apt/brew packages already
  exercised by the CI driver-present job.
- RX+TX path: HackRF One (or rad1o, same backend) — half-duplex transceiver
  covered by the same libhackrf binding.

## Why deferred

Device choice does not block P0-P2 (offline-only). The existing
`#[ignore]` hardware smoke tests already cover the driver layer; what P3
adds is the *application* capture pipeline, and that is when the acceptance
device list must be final.
