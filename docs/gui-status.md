# GUI implementation status

Where the desktop GUI (branch `gui/full-app`) stands against
[gui-architecture.md](gui-architecture.md) and [ipc-contract.md](ipc-contract.md).
"What works" is verified by tests + compile checks; see the caveat at the
end for what that does *not* cover.

## Phase status

| Phase | Scope | Status |
|---|---|---|
| P0 architecture | workspace, Tauri+React shell, typed IPC, canvas | ✅ (PR #2 vertical slice) |
| P1 offline waveform | repository, LOD min/max envelope, zoom/drag | ✅ (PR #2 vertical slice) |
| P1 remainder | selection, spectrogram tiles | ✅ |
| P2 protocol analysis | AnalysisJob, param editing, protocol table, bidirectional selection | ✅ |
| P3 real-time devices | DeviceManager, record/preview queues, FFT waterfall | ✅ (mock + rtl-tcp; hardware untested) |
| P4 generation & advanced | Generator, AWRE labels, Project save/load | ✅ (device send deferred, see below) |
| P5 simulator & release | Simulator load/run, packaging, cross-platform CI | ✅ (simulator is read+run; editing out of scope) |

## Deliberately out of scope (v2)

- **Device send / TX**: generating a signal and transmitting it over a
  device is not wired. The design keeps sending strictly explicit-start
  (contract "Generator"); a future send session would reuse the capture
  session machinery.
- **Simulator editing**: `.simulator` configs can be loaded, listed and run
  (with transcript); creating or editing rules in the GUI is not
  implemented. The Rust engine (`urh::simulator`) supports it — the missing
  piece is UI + save paths only.
- **Live view of a recording in progress**: during capture, preview comes
  from `capture://batch` events; the recorded signal is registered in the
  repository only on `stop_capture` (avoids O(n) snapshot copies per
  refresh). Waveform/spectrogram/analysis work on the finished recording.
- **Encoding chains in the generator**: generated bits are modulated raw
  (no Manchester/whitening step in the generate path).
- **Project UI collects params/view for the active signal only** — the
  `.urhproj` format and bridge types support per-signal maps for richer
  future use.
- **CSP is null** (carried over from the P0 slice; tightening it is an
  open checklist item — requires exercising the app on a real desktop).
- **Sound card backend** is listed by `list_devices` but reported
  unavailable (the cpal soundcard feature is not enabled for `urh-app`).

## Known issues

- `urh::awre::util::check_crc_for_messages` panics on protocols with
  unequal message lengths (e.g. elektromaten); `run_awre` contains it via
  `catch_unwind` and reports `internal`. Fix belongs in the root crate
  (upstream-parity question first).
- `stop_capture` hangs if a vendor `stop()` call hangs (inherent to the
  synchronous `Device` trait; not observable with the mock device).
- `rtl-tcp` connect has no timeout below the OS default, so connecting to
  a dead host blocks `start_capture` for the TCP timeout period.
- Live waterfall columns keep the dB reference they were drawn with; a
  later, louder signal does not re-color earlier columns.

## Verification caveat

This machine is headless: everything above is verified by `cargo test`
(urh-app) + vitest (frontend) + `cargo check -p urh-desktop` (CI matrix:
ubuntu/windows/macos). **The app has not been launched on a real desktop.**
Smoke-test locally with `cd apps/desktop && npm install && npm run tauri dev`
— the `mock` device (no hardware needed) exercises capture, live waveform,
waterfall and recording end to end.
