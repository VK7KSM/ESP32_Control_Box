#!/usr/bin/env python3
"""
ElfRadio ESP32 协议功能测试
协议: [0xAA][0x55][Type][LenLo][LenHi][Payload][CRC16-CCITT]
"""
import serial, struct, time, sys, threading

PORT = 'COM8'

# PC→ESP32 命令
CMD_HEARTBEAT    = 0x01
CMD_GET_STATE    = 0x02
CMD_RAW_KEY_PRESS= 0x10
CMD_RAW_KEY_REL  = 0x11
CMD_RAW_KNOB     = 0x12
CMD_SET_VOL      = 0x25
CMD_SET_SQL      = 0x26
CMD_SET_PTT      = 0x27

# ESP32→PC 报告
RPT_HEARTBEAT_ACK= 0x81
RPT_STATE_REPORT = 0x82
RPT_ERROR        = 0x85

def crc16(data):
    crc = 0xFFFF
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = (crc << 1) ^ 0x1021 if crc & 0x8000 else crc << 1
    return crc & 0xFFFF

def encode(typ, payload=b''):
    hdr = bytes([0xAA, 0x55, typ, len(payload) & 0xFF, (len(payload) >> 8) & 0xFF]) + payload
    crc = crc16(hdr)
    return hdr + bytes([crc & 0xFF, (crc >> 8) & 0xFF])

def decode_band(d):
    freq = d[0:12].rstrip(b'\x00').decode('ascii','replace')
    mode = d[12:14].rstrip(b'\x00').decode('ascii','replace')
    power = {0:'HIGH',1:'MID',3:'LOW'}.get(d[14],'?')
    s_lvl = d[15]
    vol   = struct.unpack('<H', d[16:18])[0]
    sql   = struct.unpack('<H', d[18:20])[0]
    bf    = d[20]
    ch    = d[21:25].rstrip(b'\x00').decode('ascii','replace')
    vol_pct = max(0, min(100, (vol-20)*100//940)) if vol>20 else 0
    sql_pct = max(0, min(100, (sql-20)*100//980)) if sql>20 else 0
    tx    = 'TX' if bf&0x01 else 'RX'
    busy  = '忙' if bf&0x02 else '  '
    tone  = 'DCS' if bf&0x10 else ('T/R' if bf&0x0C==0x0C else ('ENC' if bf&0x04 else ('DEC' if bf&0x08 else '   ')))
    return f"{mode:2} {freq:14} {tx} S{s_lvl} {power:3} VOL:{vol_pct:3}% SQL:{sql_pct:3}% {busy} {tone} Ch:{ch}"

class Parser:
    def __init__(self): self.ph=0; self.typ=0; self.ln=0; self.clo=0; self.buf=bytearray()
    def feed(self, b):
        if self.ph==0:
            if b==0xAA: self.ph=1
        elif self.ph==1:
            self.ph=2 if b==0x55 else 0
        elif self.ph==2: self.typ=b; self.ph=3
        elif self.ph==3: self.ln=b; self.ph=4
        elif self.ph==4: self.ln|=b<<8; self.buf.clear(); self.ph=5 if self.ln else 6
        elif self.ph==5:
            self.buf.append(b)
            if len(self.buf)>=self.ln: self.ph=6
        elif self.ph==6: self.clo=b; self.ph=7
        elif self.ph==7:
            rx=self.clo|(b<<8)
            hdr=bytes([0xAA,0x55,self.typ,self.ln&0xFF,(self.ln>>8)&0xFF])+self.buf
            ok=crc16(hdr)==rx; self.ph=0
            if ok: return (self.typ, bytes(self.buf))
        return None

# --- 连接 ---
print(f"[连接] 打开 {PORT} ...")
port = serial.Serial()
port.port = PORT
port.baudrate = 115200
port.timeout = 0.05
port.open()
print(f"[连接] {PORT} 已打开")

parser = Parser()
last_state = {}
hb_stop = threading.Event()

def hb_thread():
    while not hb_stop.is_set():
        port.write(encode(CMD_HEARTBEAT))
        port.flush()
        time.sleep(0.5)

def rx_thread():
    global last_state
    while not hb_stop.is_set():
        data = port.read(256)
        for b in data:
            r = parser.feed(b)
            if r:
                typ, payload = r
                if typ == RPT_STATE_REPORT and len(payload)>=60:
                    fl = payload[0]
                    lm = '★MAIN' if fl&0x04 else '     '
                    rm = '★MAIN' if fl&0x08 else '     '
                    lband = decode_band(payload[1:26])
                    rband = decode_band(payload[26:51])
                    last_state = {'flags':fl,'left':lband,'right':rband,
                                  'left_raw':payload[1:26],'right_raw':payload[26:51]}
                elif typ == RPT_ERROR:
                    print(f"[ESP32错误] {payload.decode('utf-8','replace')}")

t1 = threading.Thread(target=hb_thread, daemon=True); t1.start()
t2 = threading.Thread(target=rx_thread, daemon=True); t2.start()

# --- 等待连接 ---
print("[等待] 获取初始状态...")
port.write(encode(CMD_GET_STATE))
port.flush()
time.sleep(1.5)

def show_state():
    if not last_state: print("  (未收到状态)"); return
    fl = last_state['flags']
    lm = '★MAIN' if fl&0x04 else '     '
    rm = '★MAIN' if fl&0x08 else '     '
    ra = '在线' if fl&0x01 else '离线'
    pc = '在线' if fl&0x02 else '离线'
    print(f"  电台:{ra}  PC:{pc}")
    print(f"  LEFT {lm} {last_state['left']}")
    print(f"  RIGHT{rm} {last_state['right']}")

print("\n=== 初始状态 ===")
show_state()

# --- 测试 1: VOL 调节 ---
print("\n=== 测试1: LEFT VOL 调节 ===")
print("  发送 CMD_SET_VOL LEFT 30%...")
port.write(encode(CMD_SET_VOL, bytes([0, 30]))); port.flush()
time.sleep(0.8)
print("  等待后状态:")
show_state()

time.sleep(0.5)
print("  发送 CMD_SET_VOL LEFT 60%...")
port.write(encode(CMD_SET_VOL, bytes([0, 60]))); port.flush()
time.sleep(0.8)
show_state()

time.sleep(0.5)
print("  发送 CMD_SET_VOL LEFT 0xFF (取消override)...")
port.write(encode(CMD_SET_VOL, bytes([0, 0xFF]))); port.flush()
time.sleep(0.5)

# --- 测试 2: SQL 调节 ---
print("\n=== 测试2: LEFT SQL 调节 ===")
print("  发送 CMD_SET_SQL LEFT 20%...")
port.write(encode(CMD_SET_SQL, bytes([0, 20]))); port.flush()
time.sleep(0.8)
show_state()

time.sleep(0.5)
print("  发送 CMD_SET_SQL LEFT 0xFF (取消override)...")
port.write(encode(CMD_SET_SQL, bytes([0, 0xFF]))); port.flush()
time.sleep(0.5)

# --- 测试 3: 亚音 P3 键 ---
print("\n=== 测试3: 亚音循环 (P3=0x13) ===")
for i in range(4):
    print(f"  第{i+1}次按 P3 (0x13)...")
    port.write(encode(CMD_RAW_KEY_PRESS, bytes([0x13]))); port.flush()
    time.sleep(0.2)
    port.write(encode(CMD_RAW_KEY_REL)); port.flush()
    time.sleep(0.6)
    show_state()

# --- 测试 4: Watchdog 观察 ---
print("\n=== 测试4: 观察 Watchdog 频率 (等待10秒) ===")
print("  (如果修复成功，COM9 日志应无/少 task_wdt 告警)")
time.sleep(10)

print("\n=== 测试完成 ===")
show_state()
hb_stop.set()
port.close()
print("[完成] 串口已关闭")
