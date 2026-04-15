# ElfRadio Control Box — PC 通信 API 文档

## 1. 概述

ESP32 控制盒通过 USB OTG 口（GPIO19/20，枚举为 Espressif CDC-ACM 设备，VID=0x303A）与 PC 上位机通信。协议帧与 `log::info!()` 日志共用同一 USB 连接，PC 端通过帧头魔术字节 `0xAA 0x55` 区分协议数据和 ASCII 日志文本。

**串口参数：**
- 端口：自动检测（VID=0x303A，PID≠0x1001），或手动指定（如 `COM8`）
- 波特率：115200（USB CDC 忽略实际波特率，填什么都一样）
- **关键**：必须在 `open()` 之前设置 `DTR=False, RTS=False`，否则 ESP32 会复位死循环

> **注意**：控制盒有两条 USB 线：
> - **OTG 线**（GPIO19/20）→ VID=0x303A → 本 API 使用的通信口
> - **UART 调试线**（CH343/CH340）→ VID≠0x303A → 固件烧录和日志读取用，不参与本 API

## 2. 帧格式

```
[0xAA][0x55][Type:1B][LenLo:1B][LenHi:1B][Payload:0~N B][CRC16_Lo:1B][CRC16_Hi:1B]
```

| 字段 | 大小 | 说明 |
|------|------|------|
| SYNC0 | 1B | 固定 `0xAA` |
| SYNC1 | 1B | 固定 `0x55` |
| Type | 1B | 消息类型 |
| LenLo | 1B | Payload 长度低 8 位 |
| LenHi | 1B | Payload 长度高 8 位（通常为 0）|
| Payload | 0~N B | 载荷（长度由 LenLo+LenHi 决定）|
| CRC16_Lo | 1B | CRC16-CCITT 校验低字节 |
| CRC16_Hi | 1B | CRC16-CCITT 校验高字节 |

**最短帧**（无 Payload）：7 字节（SYNC0+SYNC1+Type+LenLo+LenHi+CRC_Lo+CRC_Hi）

### 校验算法：CRC16-CCITT

- 多项式：`0x1021`，初值：`0xFFFF`，小端序输出
- **校验范围**：从 `SYNC0（0xAA）` 开始，覆盖 SYNC0、SYNC1、Type、LenLo、LenHi、Payload 全部字节
- CRC 本身不参与校验计算

```python
def crc16_ccitt(data: bytes) -> int:
    crc = 0xFFFF
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            if crc & 0x8000:
                crc = ((crc << 1) ^ 0x1021) & 0xFFFF
            else:
                crc = (crc << 1) & 0xFFFF
    return crc

def encode_frame(typ: int, payload: bytes = b'') -> bytes:
    length = len(payload)
    frame = bytes([0xAA, 0x55, typ, length & 0xFF, length >> 8]) + payload
    crc = crc16_ccitt(frame)
    return frame + bytes([crc & 0xFF, crc >> 8])
```

## 3. PC → ESP32 命令

### 3.1 HEARTBEAT (0x01)

心跳包，告知 ESP32 PC 在线。ESP32 的 TX 线程每 500ms 自动发送，无需手动管理。  
ESP32 连续约 3 秒未收到心跳则判定 PC 离线，自动释放所有 override（VOL/SQL/PTT）。

```python
frame = encode_frame(0x01)
# 帧内容: AA 55 01 00 00 [CRC_Lo CRC_Hi]
```

### 3.2 GET_STATE (0x02)

请求 ESP32 立即上报完整状态（`STATE_REPORT`）。ESP32 正常运行时会每约 200ms 自动发送状态，此命令用于强制立即获取。

```python
frame = encode_frame(0x02)
```

### 3.3 RAW_KEY_PRESS (0x10)

注入单个按键（模拟面板/手咪按键按下）。

| Payload[0] | 说明 |
|------------|------|
| 键码 | 见第 5 节键码表 |

```python
# 按数字键 "4"
frame = encode_frame(0x10, bytes([0x04]))

# 切换 MAIN（P1 键）
frame = encode_frame(0x10, bytes([0x10]))
```

### 3.4 RAW_KEY_RELEASE (0x11)

松键（发送空闲帧到机身）。按键按下后必须发送此帧。

```python
frame = encode_frame(0x11)
```

### 3.5 RAW_KNOB (0x12)

旋钮微调（模拟机头旋钮转动）。

| Payload[0] | 说明 |
|------------|------|
| `0x01` | 左侧 CCW（左旋，频率 -1 步）|
| `0x02` | 左侧 CW（右旋，频率 +1 步）|
| `0x81` | 右侧 CCW |
| `0x82` | 右侧 CW |

```python
# 左侧旋钮 CW +1 步
frame = encode_frame(0x12, bytes([0x02]))

# 右侧旋钮 CCW -1 步
frame = encode_frame(0x12, bytes([0x81]))
```

### 3.6 SET_VOL (0x25)

设置音量（通过拦截上行帧替换 ADC 值实现）。

| Payload | 说明 |
|---------|------|
| `[0]` side | `0`=LEFT, `1`=RIGHT（当前版本以 MAIN 侧为准，side 参数保留）|
| `[1]` pct | `0`~`100` 百分比；`0xFF` = 取消 override，恢复物理旋钮 |

```python
# 设置音量 60%
frame = encode_frame(0x25, bytes([0, 60]))

# 取消音量 override，恢复物理旋钮
frame = encode_frame(0x25, bytes([0, 0xFF]))
```

### 3.7 SET_SQL (0x26)

设置静噪，格式同 SET_VOL。

```python
# 设置静噪 25%
frame = encode_frame(0x26, bytes([0, 25]))
```

### 3.8 SET_PTT (0x27)

远程 PTT 控制。内置 30 秒硬超时（与 ESP32 固件看门狗同步），PC 断连自动释放。

| Payload[0] | 说明 |
|------------|------|
| `0x01` | PTT 按下（开始发射）|
| `0x00` | PTT 释放（停止发射）|

```python
frame_on  = encode_frame(0x27, bytes([1]))  # PTT 开
frame_off = encode_frame(0x27, bytes([0]))  # PTT 关
```

### 3.9 POWER_TOGGLE (0x28)

电台开关机（触发 GPIO8 → PC817 光耦脉冲 1.2 秒）。

```python
frame = encode_frame(0x28)
```

## 4. ESP32 → PC 消息

### 4.1 HEARTBEAT_ACK (0x81)

心跳应答，收到 HEARTBEAT 后立即发送。

```python
# 接收到此帧表示 ESP32 在线且 pc_alive=true
# 帧内容: AA 55 81 00 00 [CRC_Lo CRC_Hi]
```

### 4.2 STATE_REPORT (0x82)

60 字节定长状态快照。当 `pc_alive=true` 时约每 200ms 自动发送一次（数据变化时触发）。  
也可通过 GET_STATE (0x02) 强制请求。

```
偏移   字段              大小  说明
0      flags              1    bit0=radio_alive, bit1=pc_alive, bit2=left_main,
                               bit3=right_main, bit4=macro_running, bit5=ptt_override
1-25   left_band         25   见下方波段布局
26-50  right_band        25   同 left_band 布局
51-54  body_count         4   u32 LE：下行帧（主机→面板）计数
55-58  head_count         4   u32 LE：上行帧（面板→主机）计数
59     pc_count           1   u8：PC 帧接收计数（mod 256）
```

**单侧波段布局（25 字节）：**

```
偏移   字段              大小  说明
+0     freq              12   ASCII 频率 "438.500.000\0"（不足补 0x00）
+12    mode               2   "FM" 或 "AM"
+14    power              1   0=HIGH, 1=MID, 3=LOW
+15    s_level            1   0-9 信号/TX 功率格数
+16    vol                2   u16 LE ADC 原始值（vol_pct = (raw-20)*100/940，范围 20~960）
+18    sql                2   u16 LE ADC 原始值（sql_pct = (raw-20)*100/980，范围 20~1000）
+20    flags              1   bit0=is_tx, bit1=is_busy, bit2=tone_enc, bit3=tone_dec,
                               bit4=tone_dcs, bit5=shift_plus, bit6=shift_minus, bit7=is_set_menu
+21    channel            4   ASCII "VFO\0" 或 "012\0" 等
```

### 4.3 ERROR (0x85)

错误信息。Payload 为 UTF-8 ASCII 字符串。

```python
# 示例：PTT 超时保护触发
# payload = b"PTT timeout"
```

> **注意**：`MACRO_PROGRESS (0x83)` 和 `MACRO_DONE (0x84)` 在协议中预留但**当前版本未实现**。

## 5. 键码表

### 手咪按键

| 按键 | 键码 | 说明 |
|------|------|------|
| 0-9 | `0x00`-`0x09` | 数字键（频率输入/DTMF）|
| A-D (DTMF) | `0x0A`-`0x0D` | 手咪 DTMF 字母键 |
| P1 (BAND) | `0x10` | **切换 MAIN 左右**（推荐使用此键，行为稳定）|
| P2 (VFO/MR) | `0x11` | VFO/MR 切换 |
| P3 (TONE) | `0x12` | 亚音模式循环 |
| P4 (LOW) | `0x13` | 功率循环 |
| UP | `0x14` | 增大 |
| DOWN | `0x15` | 减小 |

### 面板左侧按键

| 按键 | 键码 | 说明 |
|------|------|------|
| SET | `0x20` | 进入/退出 SET 菜单 |
| LOW | `0x21` | 左侧功率循环 |
| V/M | `0x22` | 左侧 VFO/MR |
| HM | `0x23` | 左侧 Hyper Memory |
| SCN | `0x24` | 左侧扫描 |
| DIAL 短按 | `0x25` | 切换 MAIN 到左侧（注：DIAL 在目标侧已是 MAIN 时会触发频率确认流程，建议改用 P1=0x10）|
| VOL 短按 | `0x26` | 左侧单/双接收切换 |

### 面板右侧按键

| 按键 | 键码 | 说明 |
|------|------|------|
| LOW | `0xA1` | 右侧功率循环 |
| V/M | `0xA2` | 右侧 VFO/MR |
| HM | `0xA3` | 右侧 Hyper Memory |
| SCN | `0xA4` | 右侧扫描 |
| DIAL 短按 | `0xA5` | 切换 MAIN 到右侧（同上，建议用 P1=0x10）|

### Hyper Memory 面板键

| 按键 | 键码 |
|------|------|
| A/B/C | `0x27`/`0x28`/`0x29` |
| D/E/F | `0xAA`/`0xAB`/`0xAC` |

## 6. 安全机制

| 机制 | 说明 |
|------|------|
| PC 心跳超时 | 约 3 秒无心跳 → `pc_alive=false`，释放 VOL/SQL/PTT 所有 override |
| PTT 30s 超时 | 硬编码上限，到期强制释放 PTT（与上位机倒计时同步）|
| PTT 断连保护 | PC 断连时立即释放 PTT |
| VOL/SQL 取消 | 发送 pct=0xFF 手动恢复物理旋钮 |

## 7. PC 端开发指南（Python）

### 依赖

```bash
pip install pyserial
```

### 串口连接（必须严格遵守！）

```python
import serial

ser = serial.Serial()
ser.port = 'COM8'        # 替换为实际串口号，或用 auto_detect() 自动检测
ser.baudrate = 115200    # USB CDC 忽略实际波特率，填什么都一样
ser.timeout = 0.1
ser.dtr = False          # 必须在 open() 之前！否则 ESP32 复位死循环！
ser.rts = False          # 必须在 open() 之前！
ser.open()
```

### 帧编解码

```python
def crc16_ccitt(data: bytes) -> int:
    """CRC16-CCITT: poly=0x1021, init=0xFFFF, little-endian output"""
    crc = 0xFFFF
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            if crc & 0x8000:
                crc = ((crc << 1) ^ 0x1021) & 0xFFFF
            else:
                crc = (crc << 1) & 0xFFFF
    return crc

def encode_frame(typ: int, payload: bytes = b'') -> bytes:
    """编码发送帧：AA 55 Type LenLo LenHi [Payload] CRC_Lo CRC_Hi"""
    length = len(payload)
    frame = bytes([0xAA, 0x55, typ, length & 0xFF, length >> 8]) + payload
    crc = crc16_ccitt(frame)
    return frame + bytes([crc & 0xFF, crc >> 8])
```

### 接收与解复用

```python
class FrameParser:
    """逐字节解析串口数据流，分离协议帧和日志文本"""

    def __init__(self):
        self.phase = 0      # 0=sync0 1=sync1 2=type 3=lenlo 4=lenhi 5=payload 6=crclo 7=crchi
        self.typ = 0
        self.length = 0
        self.crc_lo = 0
        self.buf = bytearray()
        self.text_buf = bytearray()

    def feed(self, b: int):
        """处理一个字节。返回 (typ, payload_bytes) 或 None（帧未完整）"""
        if self.phase == 0:
            if b == 0xAA:
                self.phase = 1
            else:
                self.text_buf.append(b)
                if b == 0x0A:  # 换行，输出一行日志
                    line = self.text_buf.decode('utf-8', errors='replace')
                    self.text_buf.clear()
                    return ('log', line)
        elif self.phase == 1:
            if b == 0x55:
                self.phase = 2
            else:
                self.text_buf.extend([0xAA, b])
                self.phase = 0
        elif self.phase == 2:
            self.typ = b
            self.phase = 3
        elif self.phase == 3:
            self.length = b          # LenLo
            self.phase = 4
        elif self.phase == 4:
            self.length |= b << 8    # LenHi
            self.buf.clear()
            self.phase = 5 if self.length > 0 else 6
        elif self.phase == 5:
            self.buf.append(b)
            if len(self.buf) >= self.length:
                self.phase = 6
        elif self.phase == 6:
            self.crc_lo = b
            self.phase = 7
        elif self.phase == 7:
            rx_crc = self.crc_lo | (b << 8)
            header = bytes([0xAA, 0x55, self.typ,
                            self.length & 0xFF, self.length >> 8])
            calc_crc = crc16_ccitt(header + bytes(self.buf))
            self.phase = 0
            if calc_crc == rx_crc:
                return (self.typ, bytes(self.buf))
            # CRC 不匹配，静默丢弃
        return None
```

### 解码 STATE_REPORT

```python
import struct

def decode_state(payload: bytes) -> dict:
    """解码 60 字节 STATE_REPORT 载荷"""
    if len(payload) < 60:
        return {}

    flags = payload[0]
    result = {
        'radio_alive':   bool(flags & 0x01),
        'pc_alive':      bool(flags & 0x02),
        'left_main':     bool(flags & 0x04),
        'right_main':    bool(flags & 0x08),
        'macro_running': bool(flags & 0x10),
        'ptt_override':  bool(flags & 0x20),
    }

    def decode_band(data: bytes) -> dict:
        freq    = data[0:12].rstrip(b'\x00').decode('ascii', errors='replace')
        mode    = data[12:14].rstrip(b'\x00').decode('ascii', errors='replace')
        power   = {0: 'HIGH', 1: 'MID', 3: 'LOW'}.get(data[14], '?')
        s_level = data[15]
        vol_raw = struct.unpack('<H', data[16:18])[0]
        sql_raw = struct.unpack('<H', data[18:20])[0]
        bf      = data[20]
        channel = data[21:25].rstrip(b'\x00').decode('ascii', errors='replace')
        # 百分比换算：ADC 范围 20~960（vol）/ 20~1000（sql）
        vol_pct = max(0, min(100, (vol_raw - 20) * 100 // 940)) if vol_raw > 20 else 0
        sql_pct = max(0, min(100, (sql_raw - 20) * 100 // 980)) if sql_raw > 20 else 0
        return {
            'freq': freq, 'mode': mode, 'power': power,
            's_level': s_level,
            'vol_raw': vol_raw, 'vol_pct': vol_pct,
            'sql_raw': sql_raw, 'sql_pct': sql_pct,
            'is_tx':      bool(bf & 0x01),
            'is_busy':    bool(bf & 0x02),
            'tone_enc':   bool(bf & 0x04),
            'tone_dec':   bool(bf & 0x08),
            'tone_dcs':   bool(bf & 0x10),
            'shift_plus': bool(bf & 0x20),
            'shift_minus':bool(bf & 0x40),
            'is_set_menu':bool(bf & 0x80),
            'channel': channel,
        }

    result['left']       = decode_band(payload[1:26])
    result['right']      = decode_band(payload[26:51])
    result['body_count'] = struct.unpack('<I', payload[51:55])[0]
    result['head_count'] = struct.unpack('<I', payload[55:59])[0]
    result['pc_count']   = payload[59]
    return result
```

### 最小可运行 CLI 示例

```python
#!/usr/bin/env python3
"""
ElfRadio PC Link — 最小 CLI 示例
用法: python pc_link.py [COM8]

注意: 串口必须是 OTG 口（Espressif VID=0x303A），不是 UART 调试口！
"""
import serial, threading, sys, time, struct

PORT = sys.argv[1] if len(sys.argv) > 1 else 'COM8'

# ── 帧编解码 ──────────────────────────────────────────────────────

def crc16_ccitt(data: bytes) -> int:
    crc = 0xFFFF
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc

def encode_frame(typ: int, payload: bytes = b'') -> bytes:
    n = len(payload)
    frame = bytes([0xAA, 0x55, typ, n & 0xFF, n >> 8]) + payload
    crc = crc16_ccitt(frame)
    return frame + bytes([crc & 0xFF, crc >> 8])

# ── 串口连接（DTR 必须在 open() 前置 False！）──────────────────────

ser = serial.Serial()
ser.port      = PORT
ser.baudrate  = 115200
ser.timeout   = 0.1
ser.dtr       = False   # 在 open() 之前！
ser.rts       = False
ser.open()
print(f"已连接 {PORT}，输入命令（h=帮助）:")

# ── 心跳线程（每 500ms 发送）──────────────────────────────────────

def heartbeat_thread():
    while True:
        ser.write(encode_frame(0x01))
        time.sleep(0.5)

# ── 接收线程 ──────────────────────────────────────────────────────

def rx_thread():
    phase = 0; typ = 0; length = 0; crc_lo = 0
    buf = bytearray(); text = bytearray()
    while True:
        data = ser.read(1)
        if not data:
            continue
        b = data[0]
        if phase == 0:
            if b == 0xAA:  phase = 1
            else:
                text.append(b)
                if b == 0x0A:
                    sys.stderr.write(text.decode('utf-8', errors='replace'))
                    sys.stderr.flush(); text.clear()
        elif phase == 1:
            phase = 2 if b == 0x55 else 0
            if phase == 0: text.extend([0xAA, b])
        elif phase == 2:  typ = b; phase = 3
        elif phase == 3:  length = b; phase = 4                    # LenLo
        elif phase == 4:  length |= b << 8; buf.clear()            # LenHi
                          phase = 5 if length > 0 else 6
        elif phase == 5:
            buf.append(b)
            if len(buf) >= length: phase = 6
        elif phase == 6:  crc_lo = b; phase = 7
        elif phase == 7:
            rx_crc = crc_lo | (b << 8)
            hdr = bytes([0xAA, 0x55, typ, length & 0xFF, length >> 8])
            if crc16_ccitt(hdr + bytes(buf)) == rx_crc:
                handle_frame(typ, bytes(buf))
            phase = 0

def handle_frame(typ: int, payload: bytes):
    if typ == 0x81:
        pass  # HEARTBEAT_ACK
    elif typ == 0x82 and len(payload) >= 60:
        show_state(payload)
    elif typ == 0x85:
        msg = payload.decode('utf-8', errors='replace')
        if msg.strip() != 'PTT timeout':   # PTT 超时是正常保护行为，静默过滤
            print(f"[ESP32 错误] {msg}")

def show_state(p: bytes):
    flags = p[0]
    def band(d: bytes):
        freq = d[0:12].rstrip(b'\x00').decode('ascii', errors='replace')
        mode = d[12:14].rstrip(b'\x00').decode('ascii', errors='replace')
        pwr  = {0:'HIGH',1:'MID',3:'LOW'}.get(d[14],'?')
        s    = d[15]; bf = d[20]
        ch   = d[21:25].rstrip(b'\x00').decode('ascii', errors='replace')
        vol_raw, sql_raw = struct.unpack('<HH', d[16:20])
        vol_pct = max(0, min(100, (vol_raw - 20) * 100 // 940)) if vol_raw > 20 else 0
        sql_pct = max(0, min(100, (sql_raw - 20) * 100 // 980)) if sql_raw > 20 else 0
        tx = 'TX' if bf & 0x01 else ('RX' if bf & 0x02 else '  ')
        return f"{mode} {freq} MHz  S{s}  {tx}  {pwr}  Ch:{ch}  VOL:{vol_pct}%  SQL:{sql_pct}%"
    main_l = 'MAIN' if flags & 0x04 else '    '
    main_r = 'MAIN' if flags & 0x08 else '    '
    radio  = 'OK' if flags & 0x01 else '--'
    pc     = 'OK' if flags & 0x02 else '--'
    print(f"\r  LEFT  {main_l}  {band(p[1:26])}")
    print(f"  RIGHT {main_r}  {band(p[26:51])}")
    body = struct.unpack('<I', p[51:55])[0]
    head = struct.unpack('<I', p[55:59])[0]
    print(f"  Radio:{radio}  PC:{pc}  Down:{body}  Up:{head}")
    print()

# ── 主循环 ────────────────────────────────────────────────────────

threading.Thread(target=heartbeat_thread, daemon=True).start()
threading.Thread(target=rx_thread, daemon=True).start()

HELP = """
命令:
  s            请求状态报告
  k<HEX>       按键（如 k10=P1/MAIN切换, k21=LEFT LOW键, k12=P3/亚音）
  kr           松键
  cw/ccw       左侧旋钮 CW/CCW +1步
  rcw/rccw     右侧旋钮 CW/CCW +1步
  vol<0-100>   设置音量（如 vol60）
  sql<0-100>   设置静噪（如 sql25）
  volx/sqlx    取消音量/静噪 override，恢复物理旋钮
  ptton/pttoff PTT 开/关
  radio        电台开关机（GPIO8 脉冲）
  q            退出
"""

while True:
    try:
        line = input("> ").strip().lower()
    except (EOFError, KeyboardInterrupt):
        break
    if not line or line == 'h':
        print(HELP)
    elif line == 's':
        ser.write(encode_frame(0x02))
    elif line.startswith('k') and line != 'kr' and len(line) > 1:
        try:
            ser.write(encode_frame(0x10, bytes([int(line[1:], 16)])))
        except ValueError:
            print("键码格式错误，例: k10 表示键码 0x10")
    elif line == 'kr':
        ser.write(encode_frame(0x11))
    elif line == 'cw':   ser.write(encode_frame(0x12, bytes([0x02])))
    elif line == 'ccw':  ser.write(encode_frame(0x12, bytes([0x01])))
    elif line == 'rcw':  ser.write(encode_frame(0x12, bytes([0x82])))
    elif line == 'rccw': ser.write(encode_frame(0x12, bytes([0x81])))
    elif line.startswith('vol') and line[3:].isdigit() and line != 'volx':
        ser.write(encode_frame(0x25, bytes([0, int(line[3:])])))
    elif line == 'volx':
        ser.write(encode_frame(0x25, bytes([0, 0xFF])))
    elif line.startswith('sql') and line[3:].isdigit() and line != 'sqlx':
        ser.write(encode_frame(0x26, bytes([0, int(line[3:])])))
    elif line == 'sqlx':
        ser.write(encode_frame(0x26, bytes([0, 0xFF])))
    elif line == 'ptton':  ser.write(encode_frame(0x27, bytes([1])))
    elif line == 'pttoff': ser.write(encode_frame(0x27, bytes([0])))
    elif line == 'radio':  ser.write(encode_frame(0x28))
    elif line == 'q':      break
    else:
        print("未知命令，输入 h 查看帮助")

ser.close()
```

### 运行方式

```bash
# 安装依赖
pip install pyserial

# 保存为 pc_link.py，运行（替换为实际串口号）
python pc_link.py COM8
```

## 8. 常见错误

| 现象 | 原因 | 解决 |
|------|------|------|
| 打开串口后 ESP32 无限重启 | DTR 在 `open()` 后设置，已触发复位 | 必须在 `open()` **之前** 设置 `dtr=False` |
| 发送帧后 ESP32 无响应 | `pc_alive=False`，未先发 HEARTBEAT | 先发一帧 HEARTBEAT（0x01），等 500ms |
| 收不到 STATE_REPORT | 连接的是 UART 调试口而非 OTG 口 | 换插 OTG 线（VID=0x303A 的串口）|
| 帧 CRC 校验失败 | 使用旧版 XOR 或 1 字节 Len 格式 | 使用本文档 CRC16-CCITT 格式，Len 为 2 字节 |
| VOL/SQL 设置无效 | `pc_alive=False`，override 被忽略 | 先建立心跳，确认 STATE_REPORT 中 pc_alive=true |
