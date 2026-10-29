# urh-rs vs 原版 Python URH 功能对齐差距清单

> 2026-10-29 四路并行审计汇总（设备层 / 分析解码 / Generator+Simulator / 文件格式+CLI+编辑）。
> 每条给出 Python 参照与 Rust 现状；`文件:行号` 均为审计时快照。

## 结论速览

三个反复出现的模式：

1. **"写了但没接线"** 是最大类别：encoding.rs（14 种 codec 全部移植）、filter.rs（FIR）、
   AutoAssigner、ChecksumSpec、config.rs 的 simulator 编辑 API——库层都在，app/GUI 层没有暴露。
2. **应用层比绑定层薄**：设备 TX 在 6 个后端的绑定层已实现，但 urh-app 无任何发送命令；
   CaptureConfig 只有 9 个字段，if_gain/bb_gain/bias_tee 等全部静默为零。
3. **格式与模型缺口**：wav/.coco/.sub/归档读写、原版 .project XML 不兼容、
   Message 缺 participant/timestamp/RSSI、FieldLabel 丢 CRC 规格。

---

## P0 必须对齐（核心功能缺失）

| # | 缺口 | Python 参照 | Rust 现状 |
|---|---|---|---|
| 1 | **解码链接入协议管线**（最高杠杆，codec 已全部就绪） | `Encoding.py` + `ProtocolAnalyzer.set_decoder_for_messages`（ProtocolAnalyzer.py:221）、`Message.decoded_*` | `src/encoding.rs` 完整但 `protocol.rs:12` 自注未接线；`MessageInfo` 只有 plain_bits；generator/simulator 也不用 decoding |
| 2 | **应用层 TX（发送）** | `VirtualDevice` Mode.send（VirtualDevice.py:105-120）+ repeats/连续发送/进度（Device.py:209-275, SendConfig.py） | `Device::send` 绑定层已有（hackrf/bladerf/limesdr/plutosdr/usrp/soundcard/file），urh-app 零暴露；Generator 只能存文件 |
| 3 | **信号编辑全套** | crop/mute/delete/复制粘贴/从选区新建/undo-redo（Signal.py:508-660, EditSignalAction.py, EditableGraphicView.py） | 完全缺失；`signal.rs` 是不可变查询对象；编辑后需重建 LOD |
| 4 | **CaptureConfig 参数透传** | DeviceSettingsWidget 完整参数 + config.py per-device 默认（增益 25/50/75 百分位） | if_gain/bb_gain/bias_tee/channel_index/antenna_index/freq_correction/direct_sampling 全为零；HackRF 默认 LNA=0/VGA=0；LimeSDR 天线 0="None" |
| 5 | **wav 读写** | `Signal.py:114` / `IQArray.py:267` | 无（此前误以为已支持） |
| 6 | **FieldLabel 保留 CRC 规格 + field_type** | `MessageType.py:88-109`, `ChecksumLabel.py` | AWRE 引擎有 `ChecksumSpec`，输出到 `FieldLabel` 时丢弃（多项式/data range/WSP） |
| 7 | **Message participant/timestamp/RSSI** | `Message.py` + `AutoAssigner.py` + ProtocolAnalyzer.py:269-272 | `run_awre` 不带 `with_auto_assignment`；Message 无 participant 字段；RSSI/时间戳占位 |
| 8 | **CLI RX/TX 模式** | `urh_cli.py:490-491` 平铺参数（-rx/-tx/-e 编码/消息文件） | `urh-cli` 仅离线分析；库层设备/capture 都在，只差暴露 |

## P1 应该对齐（常用能力缺口）

| # | 缺口 | 说明 |
|---|---|---|
| 9 | Generator fuzzing（三种模式 + 值生成策略） | Successive/Concurrent/Exhaustive + range/boundaries/random（FuzzingTableModel.py:121-159）；XML 属性透传已有，生成逻辑全无 |
| 10 | Generator 多消息/消息列表 + 调制器完整参数 | 现在 GeneratorParams 只有 5 个字段、单段 bits、carrier 硬编码（generator.rs:41-48） |
| 11 | `.coco` / `.sub`（Flipper RAW）/ 归档（zip/tar.gz）读写 | Signal.py:175-207 / IQArray.py:260,275 / FileOperator.py:212-280 |
| 12 | 滤波应用入口 | `filter.rs` FIR 已移植，无 `filter_range` 等价 API（Signal.py:642） |
| 13 | `AnalysisParams` 补 pause_threshold / message_length_divisor | divisor 补零逻辑已移植（protocol.rs:680）未接线 |
| 14 | NetworkSDRInterface（外部程序当 SDR） | 收发两端整体缺失（VirtualDevice.py:279-289） |
| 15 | Simulator 规则编辑 IPC | 核心 config.rs `add_items/delete_items/move_items` 已实现，app 层未暴露；编辑结果也存不回去 |
| 16 | 协议导出：.bin / .pcapng（含 decoded ASCII）/ plain bits / .ft .fta 频谱导出 | FileOperator.py:44-48,116-123 |
| 17 | per-device 参数范围表 + 吸附策略统一 | 只有 hackrf/rtlsdr/sdrplay 做了 nearest-value 吸附；无 DEVICE_CONFIG 等价表供前端 |
| 18 | 原版 .project XML 导入 | 现为自有 JSON schema，互不兼容 |
| 19 | bladerf/limesdr `send` 应用 channel/antenna；USRP subdevice 进配置 | 绑定层遗漏 |
| 20 | 消息对齐 alignment_offset + 载波频率估计 | ProtocolAnalyzer.py:570-632,579-598 |

## P2 可选 / 低优先级

- External program codec 与 TriggerCommand 执行（进程 spawn，需安全设计；当前有意报 MissingExternalProgram）
- Ruleset 求值（message type 动态归类；`assigned_by_ruleset` 目前是占位）
- DC 偏移校正开关（Device.py:348）
- 设备运行中消息通道 / 错误码表（DeviceError 字符串已够用）
- 噪声阈值百分比模式、参数缓存
- simulator live input（运行时交互输入）
- 带宽-采样率锁定（纯 UI 概念）
- `bandwidth_is_adjustable` 元信息暴露给前端

## 确认已对齐（无需动）

- AWRE 四引擎（Length/SeqNum/Address/Checksum）+ Preprocessor + MessageTypeBuilder
- AutoInterpretation 全函数集（Haar 小波调制识别、center/noise/tolerance 估计）
- GenericCRC（guess_all + 标准表）、解调（afp_demod/grab_pulse_lens）
- 全部 12 种非 external 编码 codec 的库实现
- 9 个 native 后端绑定 + RX capture + 设备枚举/identifier（本轮刚完成）
- rtl-tcp 参数覆盖（freq/sr/bw/gain/if_gain/fcorr/directSampling/biasTee）
- .complex/.complex16u/s/.complex32u/s/.cs8/.cu8/.cs16/.cu16 读写
- .proto.xml 读写（participants/message types/fuzz 属性透传）
- simulator 表达式解析器 + transcript + .simulator.xml 只读加载运行
- rad1o 不传 bias tee（与原版一致）；airspy 带宽不可调（一致）

## 建议实施顺序（依赖关系驱动）

1. **解码链接线**（#1）：protocol/analysis/generator/simulator 一处受益四处
2. **CaptureConfig 参数透传 + per-device 默认表**（#4）：不修这个，真机接收质量都是哑巴档
3. **app 层 TX + Generator 多消息/调制参数**（#2+#10）：发送闭环
4. **wav + .coco + .sub 格式**（#5+#11）：纯 io 模块增量
5. **信号编辑 + 滤波入口**（#3+#12）：需要 signal.rs 从不可变改为可变 + LOD 重建
6. **FieldLabel CRC 规格 + participant/RSSI/timestamp**（#6+#7）：AWRE 输出增肥
7. **CLI RX/TX**（#8）、fuzzing（#9）、NetworkSDR（#14）随后
