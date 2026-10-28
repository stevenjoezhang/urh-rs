# urh-core

[Universal Radio Hacker (URH)](https://github.com/jopohl/urh) 核心信号处理层的 Rust 转写。URH 已归档；本项目逐模块移植其 DSP/协议核心，GUI 不在本阶段范围。

## 模块

| 模块 | 内容 | 参照 (URH) |
|---|---|---|
| `iq` | IQ 容器、`.complex*` 文件 I/O、幅度/dB | `IQArray.py`, `Signal.py`, `util.pyx` |
| `filter` | FIR/IIR | `signal_functions.pyx`, `Filter.py` |
| `modulation` | ASK/FSK/PSK/GFSK/OQPSK 调制与解调 | `signal_functions.pyx`, `Modulator.py` |
| `encoding` | 编码链 + CRC | `Encoding.py`, `GenericCRC.py`, `util.pyx` |
| `protocol` | 消息/消息类型/协议分析 | `Message.py`, `ProtocolAnalyzer.py` |
| `autointerpret` | 自动参数估计 | `auto_interpretation.pyx`, `ainterpretation/` |

## 状态

进行中——macOS 先行，其他平台后续。参考测试数据来自原仓库 `tests/data/`（见 `~/urh-ref`）。

```sh
cargo test
```
