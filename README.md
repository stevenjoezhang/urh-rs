# urh

[![CI](https://github.com/stevenjoezhang/urh-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/stevenjoezhang/urh-rs/actions/workflows/ci.yml)

**[English](README.md) | [中文](README.CN.md)**

A Rust port of the signal-processing core of the [Universal Radio Hacker (URH)](https://github.com/jopohl/urh). URH is archived; this project transplants its core module by module, with behavior verified bit-for-bit / sample-for-sample against measured output of the original Python implementation. The Qt GUI is not (yet) ported.

**License**: Apache-2.0, same as upstream URH. This is a derivative work of jopohl/urh; the original copyright and license apply to the ported algorithms and design (see `LICENSE`).

## Modules

| Module | Contents | Reference (URH) |
|---|---|---|
| `iq` | IQ container, `.complex*` all-format file I/O, magnitude/dB | `IQArray.py`, `Signal.py`, `util.pyx` |
| `filter` | FIR/IIR, moving average, DC correction | `signal_functions.pyx`, `Filter.py` |
| `modulation` | ASK/FSK/PSK/GFSK/OQPSK modulation and demodulation (incl. costas loop) | `signal_functions.pyx`, `Modulator.py` |
| `encoding` | Encoding chains (Manchester / differential / Morse / CC1101 whitening …) + full CRC family | `Encoding.py`, `GenericCRC.py`, `util.pyx` |
| `protocol` | Messages / message types / protocol analysis | `Message.py`, `ProtocolAnalyzer.py` |
| `autointerpret` | Automatic parameter estimation (modulation/bit_length/center/noise) | `auto_interpretation.pyx`, `ainterpretation/` |
| `awre` | Automatic protocol structure inference: preamble/sync/length/sequence/address/checksum fields | all of `awre/` |
| `devices` | Device backends: 12 (see below) | `dev/` |
| `proto_xml` | `.proto.xml` protocol file load/save (quick-xml) | `ProtocolAnalyzerContainer.py` |
| `spectrogram` | STFT spectrogram (hand-written radix-2 FFT) | `Spectrogram.py` |
| `simulator` | Simulation rule engine: expression parsing, rule-action tree, message send/receive loop, config XML | `simulator/` (except GUI scene items) |
| `continuous_modulator` / `endless_sender` | Continuous modulated sending / endless sending | `ContinuousModulator.py`, `EndlessSender.py` |

There is also `tests/simulator_flow.rs`: message sequences / transcript / sample streams compared entry by entry against the measured output of the original `test_simulation_flow` — all matching.

## Device backends

- **Direct vendor-library bindings (runtime `dlopen`; compiles without any SDK installed)**: HackRF (incl. rad1o), RTL-SDR, AirSpy, BladeRF, LimeSDR, PlutoSDR (libiio), USRP (libuhd), SDRPlay
- **Sound card**: cpal/CoreAudio (real-valued signal / stereo-IQ modes)
- **RTL-TCP**: pure-Rust implementation of the network protocol (byte-level tests against a mock server)
- **VirtualDevice**: `.complex*` file replay as a signal source; **PCAP/PCAPNG**: parsing + replay
- 4 `#[ignore]` hardware smoke tests, for environments that have the devices

## CLI

```sh
cargo run --bin urh-cli -- info|analyze|participants <file.complex> [--sample-rate N] [--modulation ASK|FSK|...]
```

## Tests and behavioral fidelity

`cargo test` — unit + integration tests. Acceptance principle: **verified against measured output of the original, not against intuition**:

- `tests/cli_compare.rs`: runs the original Python (`URH_REF_SRC` points at the original sources + venv) and the Rust CLI on ask/fsk/ASK_mod/ask_short.complex, asserting that modulation/bit_length/full message bit sequences are identical, with center/noise/pause within tolerance (measured: bit-for-bit identical)
- `tests/awre_real_protocols.rs`: four real protocols (35_messages / ack_frames_with_crc / with_checksum / without_preamble); field inference results (positions/lengths/address values/membership) match the original FormatFinder's measured output
- Upstream bugs (`get_crc_datarange` indexing, `median_filter` window, `bit_end` overflow wrap-around, …) are preserved as-is with comments, so behavior stays comparable

## Status

The macOS phase is complete. Deliberately not ported: the Qt GUI (ui/models/controller), the GNU Radio backend (dev/gr). The simulator rule engine core is ported (expression parser / rule-actions / engine / config XML; GUI scene items excepted).

## GUI (work in progress)

The original Qt GUI is not reproduced pixel-for-pixel; the desktop app is instead rebuilt with Tauri 2 + React + WebGL2, keeping all DSP in this crate. The architecture plan is in [docs/gui-architecture.md](docs/gui-architecture.md), the cross-layer IPC contract in [docs/ipc-contract.md](docs/ipc-contract.md), and decision records in [docs/adr/](docs/adr/).

```sh
cargo test -p urh-app       # application service layer (waveform envelope, …)
cd apps/desktop && npm run build && npm test   # frontend (requires node)
cargo check -p urh-desktop  # Tauri shell (Linux needs webkit2gtk-4.1-dev)
```

## Original reference environment (optional, for the Python ground-truth comparison)

Clone the original anywhere: `git clone https://github.com/jopohl/urh <urh-ref>`, then
`python3 -m venv /tmp/urh-venv && /tmp/urh-venv/bin/pip install numpy psutil PyQt6 cython setuptools && cd <urh-ref> && /tmp/urh-venv/bin/python setup.py build_ext --inplace`.
`tests/cli_compare.rs` locates the original sources via the `URH_REF_SRC` environment variable (default `../urh-ref/src`) and skips automatically when they are unavailable. In CI this step runs on Linux only.
