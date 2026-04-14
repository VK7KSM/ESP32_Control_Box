# src — ESP32-S3 控制盒固件源码

TYT TH-9800 电台控制盒的 ESP32-S3 固件，以 MITM（中间人）方式串联在电台机身与面板之间，实现软件控制与实时状态显示。

---

## 文件说明

| 文件 | 职责 |
|---|---|
| `main.rs` | 入口：外设初始化、双缓冲帧循环、任务调度 |
| `uart.rs` | 双 UART 透传线程：`relay_down_thread`（机身→面板）、`relay_up_thread`（面板→机身）|
| `protocol.rs` | TH-9800 下行帧解析（AA FD 格式）、上行帧构建、PC 通信帧编解码（CRC16-CCITT）|
| `state.rs` | 共享电台状态（频率、模式、S 表、PTT、VOL/SQL 等），Mutex 保护 |
| `pc_comm.rs` | TinyUSB CDC-ACM PC 通信层：接收 PC 命令、发送状态报告 |
| `ui.rs` | ST7789 屏幕 UI 渲染（双波段面板、S 表、菜单模式、中文徽章）|
| `framebuf.rs` | 双缓冲帧管理：ESP-IDF DMA 异步刷屏，RGB565 字节序转换 |
| `macro_engine.rs` | 宏指令引擎：PC 下发按键序列的顺序执行 |

---

## 硬件架构

```
TH-9800 机身 ──UART1(GPIO17/18)──┐
                                  ESP32-S3
TH-9800 面板 ──UART2(GPIO7/16)───┘
                  │
              ST7789 屏幕 (SPI, GPIO9-14)
                  │
              PC 上位机 (TinyUSB CDC-ACM, GPIO19/20)
```

电平转换：1N4148 二极管模块（5V TTL ↔ 3.3V LVTTL）

---

## 编译与烧录

> **编译环境要求**：Rust `esp` 工具链（通过 `espup` 安装），ESP-IDF v5.3.3，Python 3.12

```bash
# 编译（在项目根目录）
cargo build

# 烧录到 ESP32-S3（串口号以实际为准，⚠️ 不要加 -M 参数）
espflash flash target/xtensa-esp32s3-espidf/debug/elfradio-hwnode -p COM3

# 读取日志（必须用 --dtr 0 防止 ESP32 复位）
python -m serial.tools.miniterm --dtr 0 --rts 0 COM3 115200
```

---

## TH-9800 通信协议要点

- **波特率**：19200, 8N1
- **下行帧格式**：`AA FD [Len] [Payload] [XOR校验]`
- **上行帧格式**：`AA FD 0C [12B Payload] [XOR校验]`（固定 16 字节）
- 协议详细说明见根目录 `TH-9800 面板通信协议逆向分析总结报告.md`

---

## PC 通信协议

ESP32 通过原生 USB OTG 口（TinyUSB CDC-ACM）与 PC 上位机通信：

```
帧格式：[0xAA][0x55][Type][LenLo][LenHi][Payload...][CRC16-CCITT]
```

详细命令表见根目录 `PC_API.md`。PC 上位机见 `examples/elfradio-box/`。
