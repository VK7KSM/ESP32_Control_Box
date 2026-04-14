# ElfRadio Control Box — PC 通信 API 文档

## 1. 概述

ESP32 控制盒通过 USB Serial JTAG（COM3）与 PC 上位机通信。协议帧与 `log::info!()` 日志共用同一 USB 连接，PC 端通过帧头魔术字节 `0xEF 0xBE` 区分协议数据和 ASCII 日志文本。

**串口参数：**
- 端口：COM3（Windows），波特率 115200（USB CDC 忽略实际波特率）
- **关键**：必须在 `open()` 之前设置 `DTR=False, RTS=False`，否则 ESP32 会复位死循环

## 2. 帧格式

```
[0xEF] [0xBE] [Type:1B] [Len:1B] [Payload:0~250B] [XOR:1B]
```

| 字段 | 大小 | 说明 |
|------|------|------|
| Magic | 2B | 固定 `0xEF 0xBE`，不会出现在 ASCII 日志中 |
| Type | 1B | 消息类型 |
| Len | 1B | Payload 长度（0~250） |
| Payload | 0~250B | 载荷 |
| XOR | 1B | `0xEF ^ 0xBE ^ Type ^ Len ^ Payload[0] ^ ... ^ Payload[N-1]` |

## 3. PC → ESP32 消息

### 3.1 HEARTBEAT (0x01)
心跳包，每 500ms 发送一次。ESP32 连续 3 秒未收到心跳则判定 PC 离线，自动释放所有 override。

| 字段 | 值 |
|------|---|
| Type | `0x01` |
| Len | `0x00` |
| XOR | `0xEF ^ 0xBE ^ 0x01 ^ 0x00 = 0x50` |

**完整帧：** `EF BE 01 00 50`

### 3.2 REQUEST_STATE (0x02)
请求 ESP32 立即上报完整状态。

**完整帧：** `EF BE 02 00 53`

### 3.3 RAW_KEY_PRESS (0x10)
注入单个按键（模拟面板/手咪按键按下）。

| Payload[0] | 说明 |
|------------|------|
| 键码 | 见下方键码表 |

**示例** — 按数字键 "4"：`EF BE 10 01 04 AA`

### 3.4 RAW_KEY_RELEASE (0x11)
松键（发送空闲帧到机身）。

**完整帧：** `EF BE 11 00 40`

### 3.5 RAW_KNOB (0x12)
旋钮微调。

| Payload[0] | 说明 |
|------------|------|
| `0x01` | 左侧 CCW（频率-1步） |
| `0x02` | 左侧 CW（频率+1步） |
| `0x81` | 右侧 CCW |
| `0x82` | 右侧 CW |

### 3.6 SET_VOL (0x25)
设置音量（通过拦截上行帧替换 ADC 值）。

| Payload | 说明 |
|---------|------|
| `[0]` side | 0=LEFT, 1=RIGHT（当前版本忽略，两侧同时生效） |
| `[1]` pct | 0~100 百分比；**0xFF = 取消 override，恢复物理旋钮** |

### 3.7 SET_SQL (0x26)
设置静噪，格式同 SET_VOL。

### 3.8 SET_PTT (0x27)
远程 PTT 控制。内置 30 秒硬超时保护，PC 断连自动释放。

| Payload[0] | 说明 |
|------------|------|
| `0x01` | PTT 按下（开始发射） |
| `0x00` | PTT 释放（停止发射） |

### 3.9 POWER_TOGGLE (0x28)
电台开关机（GPIO 8 光耦脉冲 1.2 秒）。

**完整帧：** `EF BE 28 00 77`

### 3.10 SET_FREQ (0x20) / SET_POWER (0x21) / SET_TONE (0x22) / SET_RPT (0x23) / SET_STEP (0x24)
高级设置命令（通过宏引擎自动完成按键序列）。**当前版本返回"宏引擎待实现"错误，后续版本启用。**

### 3.11 CONFIGURE (0x2F)
一键完整配置。**当前版本返回"宏引擎待实现"错误。**

### 3.12 ABORT_MACRO (0x30)
中止正在执行的宏。

**完整帧：** `EF BE 30 00 6F`

## 4. ESP32 → PC 消息

### 4.1 HEARTBEAT_ACK (0x81)
心跳应答，收到 HEARTBEAT 后立即发送。

**完整帧：** `EF BE 81 00 D0`

### 4.2 STATE_REPORT (0x82)
60 字节定长状态快照。每 500ms 或数据变化时自动发送。

```
偏移  字段              大小  说明
0     flags              1    bit0=radio_alive, bit1=pc_alive, bit2=left_main,
                              bit3=right_main, bit4=macro_running, bit5=ptt_override
1-25  left_band         25    见下
26-50 right_band        25    同 left_band 布局
51-54 body_count         4    u32 LE 下行帧计数
55-58 head_count         4    u32 LE 上行帧计数
59    pc_count           1    u8 PC 帧计数 mod 256
```

**单侧波段布局（25 字节）：**
```
偏移  字段              大小  说明
+0    freq              12   ASCII 频率 "438.500.000\0"（不足补 0x00）
+12   mode               2   "FM" 或 "AM"
+14   power              1   0=HIGH, 1=MID, 3=LOW
+15   s_level            1   0-9 信号/功率格数
+16   vol                2   u16 LE ADC 原始值
+18   sql                2   u16 LE ADC 原始值
+20   flags              1   bit0=tx, bit1=busy, bit2=enc, bit3=dec,
                              bit4=dcs, bit5=shift+, bit6=shift-, bit7=set_menu
+21   channel            4   ASCII "VFO\0" 或 "012\0"
```

### 4.3 MACRO_PROGRESS (0x83)
宏执行进度。Payload: `[step, total]`。

### 4.4 MACRO_DONE (0x84)
宏执行完成。Payload: `[result]`。
- 0 = 成功, 1 = 超时, 2 = 用户中止, 3 = 安全检查失败

### 4.5 ERROR (0x85)
错误信息。Payload 为 ASCII 错误描述。

## 5. 键码表

### 手咪按键（作用于 MAIN 侧）

| 按键 | 键码 | 说明 |
|------|------|------|
| 0-9 | `0x00`-`0x09` | 数字键（频率输入/DTMF） |
| A-D (DTMF) | `0x0A`-`0x0D` | 手咪 DTMF 字母键 |
| P1 (BAND) | `0x10` | 切换 MAIN 左右（默认功能） |
| P2 (VFO/MR) | `0x11` | VFO/MR 切换（默认功能） |
| P3 (TONE) | `0x12` | 亚音模式循环（默认功能） |
| P4 (LOW) | `0x13` | 功率循环（默认功能） |
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
| DIAL 短按 | `0x25` | **切换 MAIN 到左侧** |
| VOL 短按 | `0x26` | 左侧单/双接收切换 |

### 面板右侧按键

| 按键 | 键码 | 说明 |
|------|------|------|
| LOW | `0xA1` | 右侧功率循环 |
| V/M | `0xA2` | 右侧 VFO/MR |
| HM | `0xA3` | 右侧 Hyper Memory |
| SCN | `0xA4` | 右侧扫描 |
| DIAL 短按 | `0xA5` | **切换 MAIN 到右侧** |

### Hyper Memory 面板键

| 按键 | 键码 |
|------|------|
| A/B/C | `0x27`/`0x28`/`0x29` |
| D/E/F | `0xAA`/`0xAB`/`0xAC` |

## 6. 安全机制

| 机制 | 说明 |
|------|------|
| PC 心跳超时 | 3 秒无心跳 → pc_alive=false，释放所有 override |
| PTT 30s 超时 | 硬编码上限，到期强制释放 PTT |
| PTT 断连保护 | PC 断连时立即释放 PTT |
| VOL/SQL 断连 | PC 断连时取消 override，恢复物理旋钮 |
| VOL/SQL 取消 | 发送 pct=0xFF 手动取消 override |

## 7. PC 端开发指南（Windows 11）

### 依赖
```
pip install pyserial
```

### 串口连接（必须严格遵守！）
```python
import serial

ser = serial.Serial()
ser.port = 'COM3'
ser.baudrate = 115200
ser.dtr = False   # 必须在 open() 之前！否则 ESP32 复位死循环！
ser.rts = False   # 必须在 open() 之前！
ser.open()
```

### 发送帧
```python
def send_frame(ser, typ, payload=b''):
    length = len(payload)
    xor = 0xEF ^ 0xBE ^ typ ^ length
    for b in payload:
        xor ^= b
    frame = bytes([0xEF, 0xBE, typ, length]) + payload + bytes([xor])
    ser.write(frame)
```

### 接收与解复用
```python
import threading

class Receiver:
    """从串口读取，分离协议帧和日志文本"""
    
    def __init__(self, ser):
        self.ser = ser
        self.state = 'IDLE'  # IDLE, GOT_EF, TYPE, LEN, PAYLOAD, XOR
        self.typ = 0
        self.length = 0
        self.buf = bytearray()
        self.text_buf = bytearray()
    
    def process_byte(self, b):
        """处理一个字节，返回 (frame_type, payload) 或 None"""
        if self.state == 'IDLE':
            if b == 0xEF:
                self.state = 'GOT_EF'
            else:
                self.text_buf.append(b)
                if b == 0x0A:  # 换行符，输出日志行
                    print(self.text_buf.decode('utf-8', errors='replace'), end='', file=sys.stderr)
                    self.text_buf.clear()
        elif self.state == 'GOT_EF':
            if b == 0xBE:
                self.state = 'TYPE'
            else:
                self.text_buf.append(0xEF)
                self.text_buf.append(b)
                self.state = 'IDLE'
        elif self.state == 'TYPE':
            self.typ = b
            self.state = 'LEN'
        elif self.state == 'LEN':
            self.length = b
            self.buf.clear()
            self.state = 'PAYLOAD' if b > 0 else 'XOR'
        elif self.state == 'PAYLOAD':
            self.buf.append(b)
            if len(self.buf) >= self.length:
                self.state = 'XOR'
        elif self.state == 'XOR':
            xor = 0xEF ^ 0xBE ^ self.typ ^ self.length
            for x in self.buf:
                xor ^= x
            self.state = 'IDLE'
            if xor == b:
                return (self.typ, bytes(self.buf))
        return None
```

### 解码 STATE_REPORT
```python
import struct

def decode_state(payload):
    """解码 60 字节 STATE_REPORT"""
    flags = payload[0]
    result = {
        'radio_alive': bool(flags & 0x01),
        'pc_alive':    bool(flags & 0x02),
        'left_main':   bool(flags & 0x04),
        'right_main':  bool(flags & 0x08),
        'macro_running': bool(flags & 0x10),
        'ptt_override':  bool(flags & 0x20),
    }
    
    def decode_band(data):
        freq = data[0:12].rstrip(b'\x00').decode('ascii', errors='replace')
        mode = data[12:14].rstrip(b'\x00').decode('ascii', errors='replace')
        power = {0: 'HIGH', 1: 'MID', 3: 'LOW'}.get(data[14], '?')
        s_level = data[15]
        vol = struct.unpack('<H', data[16:18])[0]
        sql = struct.unpack('<H', data[18:20])[0]
        bf = data[20]
        channel = data[21:25].rstrip(b'\x00').decode('ascii', errors='replace')
        return {
            'freq': freq, 'mode': mode, 'power': power,
            's_level': s_level, 'vol': vol, 'sql': sql,
            'is_tx': bool(bf & 0x01), 'is_busy': bool(bf & 0x02),
            'tone_enc': bool(bf & 0x04), 'tone_dec': bool(bf & 0x08),
            'tone_dcs': bool(bf & 0x10), 'shift_plus': bool(bf & 0x20),
            'shift_minus': bool(bf & 0x40), 'is_set': bool(bf & 0x80),
            'channel': channel,
        }
    
    result['left'] = decode_band(payload[1:26])
    result['right'] = decode_band(payload[26:51])
    result['body_count'] = struct.unpack('<I', payload[51:55])[0]
    result['head_count'] = struct.unpack('<I', payload[55:59])[0]
    result['pc_count'] = payload[59]
    return result
```

### 最小可运行 CLI 示例
```python
#!/usr/bin/env python3
"""ElfRadio PC Link — 最小 CLI 示例"""
import serial, threading, sys, time, struct

PORT = 'COM3'

def send_frame(ser, typ, payload=b''):
    length = len(payload)
    xor = 0xEF ^ 0xBE ^ typ ^ length
    for b in payload:
        xor ^= b
    ser.write(bytes([0xEF, 0xBE, typ, length]) + payload + bytes([xor]))

def heartbeat_thread(ser):
    while True:
        send_frame(ser, 0x01)  # HEARTBEAT
        time.sleep(0.5)

def rx_thread(ser):
    state = 'IDLE'
    typ = 0; length = 0; buf = bytearray(); text = bytearray()
    
    while True:
        data = ser.read(1)
        if not data:
            continue
        b = data[0]
        
        if state == 'IDLE':
            if b == 0xEF:
                state = 'GOT_EF'
            else:
                text.append(b)
                if b == 0x0A:
                    sys.stderr.write(text.decode('utf-8', errors='replace'))
                    sys.stderr.flush()
                    text.clear()
        elif state == 'GOT_EF':
            state = 'TYPE' if b == 0xBE else 'IDLE'
            if state == 'IDLE':
                text.extend([0xEF, b])
        elif state == 'TYPE':
            typ = b; state = 'LEN'
        elif state == 'LEN':
            length = b; buf.clear()
            state = 'PAYLOAD' if b > 0 else 'XOR'
        elif state == 'PAYLOAD':
            buf.append(b)
            if len(buf) >= length:
                state = 'XOR'
        elif state == 'XOR':
            xor = 0xEF ^ 0xBE ^ typ ^ length
            for x in buf: xor ^= x
            state = 'IDLE'
            if xor == b:
                handle_frame(typ, bytes(buf))

def handle_frame(typ, payload):
    if typ == 0x81:
        pass  # HEARTBEAT_ACK
    elif typ == 0x82 and len(payload) >= 60:
        show_state(payload)
    elif typ == 0x84:
        r = {0:'OK', 1:'超时', 2:'中止', 3:'安全失败'}.get(payload[0] if payload else 255, '?')
        print(f"[宏完成] {r}")
    elif typ == 0x85:
        print(f"[错误] {payload.decode('utf-8', errors='replace')}")

def show_state(p):
    flags = p[0]
    def band(d):
        freq = d[0:12].rstrip(b'\x00').decode('ascii', errors='replace')
        mode = d[12:14].rstrip(b'\x00').decode('ascii', errors='replace')
        pwr = {0:'HIGH',1:'MID',3:'LOW'}.get(d[14],'?')
        s = d[15]; bf = d[20]
        ch = d[21:25].rstrip(b'\x00').decode('ascii', errors='replace')
        tx = 'TX' if bf & 0x01 else 'RX'
        vol_pct = max(0, min(100, (struct.unpack('<H', d[16:18])[0] - 20) * 100 // 940))
        sql_pct = max(0, min(100, (struct.unpack('<H', d[18:20])[0] - 20) * 100 // 980))
        return f"{mode} {freq} MHz S{s} {tx} {pwr} {ch} VOL:{vol_pct}% SQL:{sql_pct}%"
    
    main_l = 'MAIN' if flags & 0x04 else '    '
    main_r = 'MAIN' if flags & 0x08 else '    '
    radio = 'OK' if flags & 0x01 else '--'
    pc = 'OK' if flags & 0x02 else '--'
    
    print(f"\r  LEFT  {main_l}  {band(p[1:26])}")
    print(f"  RIGHT {main_r}  {band(p[26:51])}")
    print(f"  Radio:{radio}  PC:{pc}  Down:{struct.unpack('<I',p[51:55])[0]}  Up:{struct.unpack('<I',p[55:59])[0]}")
    print()

def main():
    ser = serial.Serial()
    ser.port = PORT
    ser.baudrate = 115200
    ser.timeout = 0.1
    ser.dtr = False  # 必须在 open() 之前！
    ser.rts = False
    ser.open()
    print(f"已连接 {PORT}，输入命令（h=帮助）:")

    threading.Thread(target=heartbeat_thread, args=(ser,), daemon=True).start()
    threading.Thread(target=rx_thread, args=(ser,), daemon=True).start()

    while True:
        try:
            line = input("> ").strip()
        except (EOFError, KeyboardInterrupt):
            break
        
        if not line or line == 'h':
            print("命令: s=状态  k<hex>=按键  kr=松键  cw/ccw/rcw/rccw=旋钮")
            print("       vol<0-100>=音量  sql<0-100>=静噪  volx/sqlx=取消override")
            print("       ptton/pttoff=PTT  radio=开关机  q=退出")
        elif line == 's':
            send_frame(ser, 0x02)
        elif line.startswith('k') and len(line) > 1 and line[1:] != 'r':
            key = int(line[1:], 16)
            send_frame(ser, 0x10, bytes([key]))
        elif line == 'kr':
            send_frame(ser, 0x11)
        elif line == 'cw':
            send_frame(ser, 0x12, bytes([0x02]))
        elif line == 'ccw':
            send_frame(ser, 0x12, bytes([0x01]))
        elif line == 'rcw':
            send_frame(ser, 0x12, bytes([0x82]))
        elif line == 'rccw':
            send_frame(ser, 0x12, bytes([0x81]))
        elif line.startswith('vol') and line[3:].isdigit():
            send_frame(ser, 0x25, bytes([0, int(line[3:])]))
        elif line == 'volx':
            send_frame(ser, 0x25, bytes([0, 0xFF]))
        elif line.startswith('sql') and line[3:].isdigit():
            send_frame(ser, 0x26, bytes([0, int(line[3:])]))
        elif line == 'sqlx':
            send_frame(ser, 0x26, bytes([0, 0xFF]))
        elif line == 'ptton':
            send_frame(ser, 0x27, bytes([1]))
        elif line == 'pttoff':
            send_frame(ser, 0x27, bytes([0]))
        elif line == 'radio':
            send_frame(ser, 0x28)
        elif line == 'q':
            break
        else:
            print("未知命令，输入 h 查看帮助")
    
    ser.close()

if __name__ == '__main__':
    main()
```

### 运行方式
```bash
# 安装依赖
pip install pyserial

# 保存上面的代码为 pc_link.py，然后运行：
python pc_link.py

# 或者指定端口：修改脚本中的 PORT = 'COM3'
```
