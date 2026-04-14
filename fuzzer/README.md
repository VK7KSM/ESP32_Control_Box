# fuzzer — TH-9800 协议探针固件

用于逆向分析 TYT TH-9800 电台机身与面板之间的串行通信协议。以字节级透传方式捕获双向数据帧，辅助识别 AA FD 帧结构、命令 ID 和状态编码。

---

## 功能

- **双向字节透传**：实时转发 UART1（机身）↔ UART2（面板）的所有数据
- **AA FD 帧解析**：识别帧边界、提取 Len/CmdID/Payload/校验
- **状态变化检测**：仅在帧内容变化时输出日志，减少刷屏噪音
- **日志输出**：通过 USB CDC（CH343）实时打印十六进制帧内容

---

## 编译与烧录

```bash
# 在 fuzzer 目录编译
cd fuzzer && cargo build

# 烧录（不加 -M）
espflash flash target/xtensa-esp32s3-espidf/debug/elfradio-fuzzer -p COM3

# 读取日志（DTR=0 防复位）
python -m serial.tools.miniterm --dtr 0 --rts 0 COM3 115200
```

---

## 逆向成果

通过本工具完成的协议逆向结果记录在根目录：

- `TH-9800 面板通信协议逆向分析总结报告.md`：完整的下行/上行协议帧字典
- `PC_API.md`：ESP32 与 PC 上位机的通信 API

---

## 注意事项

- 需要控制盒硬件（ESP32-S3）串联在 TH-9800 机身与面板的 RJ-12 接口之间
- 电平转换模块（1N4148 二极管）将 5V TTL 转为 3.3V LVTTL
- **上电顺序**：先接 ESP32（USB），再给电台上电（13.8V），防止 RJ-12 电源反灌损坏电路
