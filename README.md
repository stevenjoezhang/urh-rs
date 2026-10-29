# urh

[![CI](https://github.com/stevenjoezhang/urh-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/stevenjoezhang/urh-rs/actions/workflows/ci.yml)

A Rust port of the signal-processing core of the [Universal Radio Hacker (URH)](https://github.com/jopohl/urh). URH is archived; this project transplants its core module by module, with behavior verified bit-for-bit / sample-for-sample against measured output of the original Python implementation. The Qt GUI is not (yet) ported.

[Universal Radio Hacker (URH)](https://github.com/jopohl/urh) 的 Rust 转写。URH 已归档；本项目逐模块移植其信号处理核心，行为与原版逐位/逐样本对齐（以原版 Python 实测输出为真值验收）。GUI 不在范围内。

**License**: Apache-2.0, same as upstream URH. This is a derivative work of jopohl/urh; the original copyright and license apply to the ported algorithms and design (see `LICENSE`).

## 模块

| 模块 | 内容 | 参照 (URH) |
|---|---|---|
| `iq` | IQ 容器、`.complex*` 全格式文件 I/O、幅度/dB | `IQArray.py`, `Signal.py`, `util.pyx` |
| `filter` | FIR/IIR、滑动平均、直流校正 | `signal_functions.pyx`, `Filter.py` |
| `modulation` | ASK/FSK/PSK/GFSK/OQPSK 调制与解调（含 costas loop） | `signal_functions.pyx`, `Modulator.py` |
| `encoding` | 编码链（Manchester/差分/Morse/CC1101 白化…）+ CRC 全家 | `Encoding.py`, `GenericCRC.py`, `util.pyx` |
| `protocol` | 消息/消息类型/协议分析 | `Message.py`, `ProtocolAnalyzer.py` |
| `autointerpret` | 自动参数估计（调制/bit_length/center/noise） | `auto_interpretation.pyx`, `ainterpretation/` |
| `awre` | 协议结构自动推断：preamble/sync/长度/序号/地址/校验字段 | `awre/` 全部 |
| `devices` | 设备后端：12 个（见下） | `dev/` |
| `proto_xml` | `.proto.xml` 协议文件加载/保存（quick-xml） | `ProtocolAnalyzerContainer.py` |
| `spectrogram` | STFT 频谱图（手写 radix-2 FFT） | `Spectrogram.py` |
| `simulator` | 仿真规则引擎：表达式解析、规则-动作树、消息收发循环、配置 XML | `simulator/`（GUI 场景项除外） |
| `continuous_modulator` / `endless_sender` | 连续调制发送 / 循环发送 | `ContinuousModulator.py`, `EndlessSender.py` |

另有 `tests/simulator_flow.rs`：与原版 `test_simulation_flow` 的 Python 实测输出（消息序列/transcript/样本流）逐条对照一致。

## 设备后端

- **直接绑厂商库（运行时 dlopen，无 SDK 也能编译）**：HackRF（含 rad1o）、RTL-SDR、AirSpy、BladeRF、LimeSDR、PlutoSDR (libiio)、USRP (libuhd)、SDRPlay
- **声卡**：cpal/CoreAudio（实信号 / 立体声 IQ 两模式）
- **RTL-TCP**：纯 Rust 网络协议实现（mock 服务器字节级测试）
- **VirtualDevice**：`.complex*` 文件回放当信号源；**PCAP/PCAPNG**：解析 + 回放
- 4 个 `#[ignore]` 硬件冒烟测试留给有设备的环境

## CLI

```sh
cargo run --bin urh-cli -- info|analyze|participants <file.complex> [--sample-rate N] [--modulation ASK|FSK|...]
```

## 测试与行为一致性

`cargo test` — 单元 + 集成测试。验收原则：**与原版实测对照，不与直觉对照**：
- `tests/cli_compare.rs`：对 ask/fsk/ASK_mod/ask_short.complex 跑原版 Python（`URH_REF_SRC` 指向的原版源码 + venv）与 Rust CLI，断言调制/bit_length/消息比特长序列全等，center/noise/pause 容差内（实测逐位一致）
- `tests/awre_real_protocols.rs`：35_messages / ack_frames_with_crc / with_checksum / without_preamble 四个真实协议，字段推断结果（位置/长度/地址值/成员归属）与原版 FormatFinder 实测一致
- 原版上游 bug（`get_crc_datarange` 索引、`median_filter` 窗口、`bit_end` 溢出回绕等）原样保留并注释，保证行为可对照

## 状态

macOS 阶段完成。有意未移植：Qt GUI（ui/models/controller）、GNU Radio 后端（dev/gr）。simulator 规则引擎核心已移植（表达式解析器/规则-动作/引擎/配置 XML，GUI 场景项除外）。

## GUI（进行中）

原版 Qt GUI 不做像素级复刻，而是以 Tauri 2 + React + WebGL2 重建桌面应用，DSP 全部保留在本 crate。架构方案见 [docs/gui-architecture.md](docs/gui-architecture.md)，跨层 IPC 契约见 [docs/ipc-contract.md](docs/ipc-contract.md)，决策记录见 [docs/adr/](docs/adr/)。

```sh
cargo test -p urh-app       # 应用服务层（波形 envelope 等）
cd apps/desktop && npm run build && npm test   # 前端（需 node）
cargo check -p urh-desktop  # Tauri 壳（Linux 需 webkit2gtk-4.1-dev）
```

## 原版参考环境（可选，用于跑 Python 真值对照）

任选目录 clone 原版：`git clone https://github.com/jopohl/urh <urh-ref>`，然后
`python3 -m venv /tmp/urh-venv && /tmp/urh-venv/bin/pip install numpy psutil PyQt6 cython setuptools && cd <urh-ref> && /tmp/urh-venv/bin/python setup.py build_ext --inplace`。
`tests/cli_compare.rs` 通过 `URH_REF_SRC` 环境变量定位原版源码（默认 `../urh-ref/src`），环境缺失时自动跳过。CI 中该步骤仅在 Linux 上执行。
