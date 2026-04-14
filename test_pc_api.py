#!/usr/bin/env python3
"""
ElfRadio PC API 直接测试脚本
通过 COM8 (TinyUSB CDC-ACM) 逐项验证固件功能
"""
import serial, struct, time, sys

PORT = 'COM8'

# ===== 协议帧编码 =====
def crc16(data):
    crc = 0xFFFF
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) if crc & 0x8000 else (crc << 1)
            crc &= 0xFFFF
    return crc

def make_frame(typ, payload=b''):
    frame = bytes([0xAA, 0x55, typ, len(payload) & 0xFF, (len(payload) >> 8) & 0xFF]) + payload
    c = crc16(frame)
    return frame + bytes([c & 0xFF, (c >> 8) & 0xFF])

def send(ser, typ, payload=b''):
    f = make_frame(typ, payload)
    ser.write(f)
    return f

# ===== 接收解析 =====
def recv_frames(ser, timeout=1.0):
    """接收并解析所有帧，返回 [(typ, payload), ...]"""
    frames = []
    buf = bytearray()
    end = time.time() + timeout
    while time.time() < end:
        data = ser.read(ser.in_waiting or 1)
        if not data:
            continue
        buf.extend(data)
        # 解析帧
        while len(buf) >= 7:
            idx = buf.find(b'\xAA\x55')
            if idx < 0:
                buf.clear()
                break
            if idx > 0:
                buf = buf[idx:]
            if len(buf) < 5:
                break
            plen = buf[3] | (buf[4] << 8)
            total = 5 + plen + 2
            if len(buf) < total:
                break
            # 校验
            frame_data = bytes(buf[:5+plen])
            rx_crc = buf[5+plen] | (buf[5+plen+1] << 8)
            calc = crc16(frame_data)
            if calc == rx_crc:
                frames.append((buf[2], bytes(buf[5:5+plen])))
            buf = buf[total:]
    return frames

def decode_state(p):
    """解码 60 字节 STATE_REPORT"""
    if len(p) < 60:
        return None
    flags = p[0]
    def band(d):
        freq = d[0:12].rstrip(b'\x00').decode('ascii', errors='replace')
        mode = d[12:14].rstrip(b'\x00').decode('ascii', errors='replace')
        pwr = {0:'HIGH',1:'MID',3:'LOW'}.get(d[14],'?')
        s = d[15]
        bf = d[20]
        ch = d[21:25].rstrip(b'\x00').decode('ascii', errors='replace')
        return {'freq':freq, 'mode':mode, 'power':pwr, 's':s,
                'tx':bool(bf&1), 'busy':bool(bf&2), 'ch':ch}
    return {
        'radio_alive': bool(flags & 0x01),
        'pc_alive': bool(flags & 0x02),
        'left_main': bool(flags & 0x04),
        'right_main': bool(flags & 0x08),
        'left': band(p[1:26]),
        'right': band(p[26:51]),
    }

def get_state(ser):
    """发送心跳+GET_STATE，返回解码后的状态"""
    send(ser, 0x01)  # heartbeat
    time.sleep(0.1)
    send(ser, 0x02)  # get_state
    frames = recv_frames(ser, 0.5)
    for typ, payload in frames:
        if typ == 0x82 and len(payload) >= 60:
            return decode_state(payload)
    return None

def key_press(ser, key, hold_ms=150, release_ms=150):
    """按键注入：按下 → 等 hold_ms → 松开 → 等 release_ms"""
    send(ser, 0x10, bytes([key]))
    time.sleep(hold_ms / 1000)
    send(ser, 0x11)
    time.sleep(release_ms / 1000)

def knob_step(ser, step):
    """旋钮微调"""
    send(ser, 0x12, bytes([step]))

# ===================================================================
# 测试用例
# ===================================================================

def connect():
    ser = serial.Serial()
    ser.port = PORT
    ser.baudrate = 115200
    ser.timeout = 0.5
    ser.dtr = False
    ser.rts = False
    ser.open()
    time.sleep(0.2)
    # 清空缓冲区
    ser.read(ser.in_waiting or 1024)
    return ser

def test_heartbeat(ser):
    print("\n=== 测试1: 心跳 ===")
    send(ser, 0x01)
    frames = recv_frames(ser, 0.5)
    ack = [f for f in frames if f[0] == 0x81]
    if ack:
        print("  ✓ 收到 HEARTBEAT_ACK")
        return True
    else:
        print("  ✗ 未收到 HEARTBEAT_ACK")
        return False

def test_get_state(ser):
    print("\n=== 测试2: 获取状态 ===")
    st = get_state(ser)
    if st:
        print(f"  ✓ Radio: {'OK' if st['radio_alive'] else '--'}")
        print(f"  ✓ MAIN: {'LEFT' if st['left_main'] else 'RIGHT'}")
        print(f"  ✓ LEFT:  {st['left']['mode']} {st['left']['freq']} MHz {st['left']['power']} Ch:{st['left']['ch']}")
        print(f"  ✓ RIGHT: {st['right']['mode']} {st['right']['freq']} MHz {st['right']['power']} Ch:{st['right']['ch']}")
        return st
    else:
        print("  ✗ 未收到状态报告")
        return None

def test_main_switch(ser):
    print("\n=== 测试3: MAIN 切换 ===")
    st = get_state(ser)
    if not st:
        print("  ✗ 无法获取状态"); return

    before = 'LEFT' if st['left_main'] else 'RIGHT'
    print(f"  当前 MAIN: {before}")

    # 切到另一侧
    target_key = 0xA5 if st['left_main'] else 0x25
    target_side = 'RIGHT' if st['left_main'] else 'LEFT'
    print(f"  → 按键 0x{target_key:02X} 切换到 {target_side}")
    key_press(ser, target_key)
    time.sleep(0.5)

    st2 = get_state(ser)
    if st2:
        after = 'LEFT' if st2['left_main'] else 'RIGHT'
        if after == target_side:
            print(f"  ✓ MAIN 已切到 {after}")
        else:
            print(f"  ✗ MAIN 仍在 {after}（期望 {target_side}）")

        # 切回去
        restore_key = 0x25 if st['left_main'] else 0xA5
        key_press(ser, restore_key)
        time.sleep(0.5)
        print(f"  → 已恢复 MAIN 到 {before}")
    else:
        print("  ✗ 切换后无法获取状态")

def test_knob(ser):
    print("\n=== 测试4: 旋钮微调 ===")
    st = get_state(ser)
    if not st:
        print("  ✗ 无法获取状态"); return

    main_side = 'left' if st['left_main'] else 'right'
    freq_before = st[main_side]['freq']
    print(f"  MAIN={main_side.upper()} 频率={freq_before}")

    # CW +1 步
    cw = 0x02 if main_side == 'left' else 0x82
    ccw = 0x01 if main_side == 'left' else 0x81
    print(f"  → 旋钮 CW (0x{cw:02X})")
    knob_step(ser, cw)
    time.sleep(0.5)

    st2 = get_state(ser)
    if st2:
        freq_after = st2[main_side]['freq']
        print(f"  频率变为 {freq_after}")
        if freq_after != freq_before:
            print(f"  ✓ 旋钮微调有效")
        else:
            print(f"  ✗ 频率未变化")

        # 恢复：CCW -1 步
        print(f"  → 旋钮 CCW (0x{ccw:02X}) 恢复")
        knob_step(ser, ccw)
        time.sleep(0.5)
    else:
        print("  ✗ 微调后无法获取状态")

def test_power_cycle(ser):
    print("\n=== 测试5: 功率循环 ===")
    st = get_state(ser)
    if not st:
        print("  ✗ 无法获取状态"); return

    # 找 MAIN 侧
    main_side = 'left' if st['left_main'] else 'right'
    pwr_before = st[main_side]['power']
    low_key = 0x21 if main_side == 'left' else 0xA1
    print(f"  MAIN={main_side.upper()} 当前功率={pwr_before}")
    print(f"  → 按 LOW 键 (0x{low_key:02X})")

    key_press(ser, low_key, hold_ms=150, release_ms=400)
    time.sleep(1.0)  # 等待功率提示页显示完

    st2 = get_state(ser)
    if st2:
        pwr_after = st2[main_side]['power']
        print(f"  功率变为 {pwr_after}")
        if pwr_after != pwr_before:
            print(f"  ✓ 功率循环有效")
        else:
            print(f"  ? 功率未变化（可能需要多按几次或等待提示页超时）")
    else:
        print("  ✗ 功率变更后无法获取状态")

def test_freq_digits(ser):
    print("\n=== 测试6: 频率数字输入 ===")
    st = get_state(ser)
    if not st:
        print("  ✗ 无法获取状态"); return

    main_side = 'left' if st['left_main'] else 'right'
    freq_before = st[main_side]['freq']
    print(f"  MAIN={main_side.upper()} 当前频率={freq_before}")

    # 输入 433550（= 433.550 MHz UHF）
    digits = "433550"
    print(f"  → 逐个按数字键: {digits}")
    for i, c in enumerate(digits):
        d = int(c)
        print(f"    [{i+1}/{len(digits)}] 按键 {d}", end='', flush=True)
        key_press(ser, d, hold_ms=150, release_ms=200)
        time.sleep(0.1)
        print(" OK")

    time.sleep(0.5)
    st2 = get_state(ser)
    if st2:
        freq_after = st2[main_side]['freq']
        print(f"  频率变为 {freq_after}")
        if '433' in freq_after and '550' in freq_after:
            print(f"  ✓ 频率设置成功")
        elif freq_after != freq_before:
            print(f"  ? 频率有变但不是预期值")
        else:
            print(f"  ✗ 频率未变化")
    else:
        print("  ✗ 频率输入后无法获取状态")

def test_vol_override(ser):
    print("\n=== 测试7: 音量 Override ===")
    # 设 50%
    print("  → SET_VOL side=0 pct=50")
    send(ser, 0x25, bytes([0, 50]))
    time.sleep(0.5)
    st = get_state(ser)
    if st:
        print(f"  VOL ADC: left={st['left'].get('vol','?')}")
        print(f"  ✓ 音量 override 发送成功")

    # 取消
    print("  → SET_VOL side=0 pct=0xFF (取消)")
    send(ser, 0x25, bytes([0, 0xFF]))
    time.sleep(0.3)

# ===== 主流程 =====
if __name__ == '__main__':
    print(f"ElfRadio PC API 测试 — 端口 {PORT}")
    ser = connect()
    print(f"✓ {PORT} 已连接")

    try:
        test_heartbeat(ser)
        st = test_get_state(ser)
        if st and st['radio_alive']:
            test_main_switch(ser)
            test_knob(ser)
            test_power_cycle(ser)
            test_freq_digits(ser)
            test_vol_override(ser)
        else:
            print("\n⚠ 电台未在线，跳过需要电台的测试")

        print("\n=== 测试完成 ===")
    except KeyboardInterrupt:
        print("\n中断")
    finally:
        ser.close()
        print("串口已关闭")
