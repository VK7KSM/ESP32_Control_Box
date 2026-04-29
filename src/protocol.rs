// ===================================================================
// TH-9800 AA FD 串口协议解析器
//
// 下行 (主机→面板): AA FD [Len] [Payload: Len字节] [XOR校验]
//   Len=02: CmdID + Status → LCD 图标/状态
//   Len=03: 心跳包 (忽略)
//   Len=06: 信道号 ASCII (MR 模式)  side 字节=01/02 区分左右
//   Len=08: 未知 (忽略)
//   Len=09: 频率/文本 ASCII
//       ★ side 字节始终为 0x01，用 flag bit7 区分:
//         flag bit7=0 (如 0x40) = 非MAIN侧的频率
//         flag bit7=1 (如 0xC0) = MAIN侧的频率
//         结合 CmdID=0x14 的 is_main 状态确定实际左右
//
// 上行 (面板→主机): 固定 17 字节 AA FD 0C [12B Payload] 00 [SUM]
//   Payload[1] = PTT (0x00=发射, 0xFF=待机)
//   Payload[6:8] = 音量 16-bit ADC (小端序)
//   Payload[9:11] = 静噪 16-bit ADC (小端序)
// ===================================================================

use crate::state::{PowerLevel, RadioState};
use esp_idf_svc::sys::esp_timer_get_time;

/// 根据 6 位频率（kHz 整数）推算末三位（100Hz/10Hz/1Hz），级联尝试所有标准步进网格
/// TH-9800 协议只传 6 位 ASCII（精度 1kHz），面板 LCD 的末三位由本地计算
fn compute_sub_khz(freq_khz: u32) -> &'static str {
    // 第一步：2.5kHz 族（覆盖 2.5/5/7.5/10/12.5/15/25/30/50/100 kHz 步进）
    let rem = (freq_khz * 10) % 25;
    let off = (25 - rem) % 25;
    if off == 0 { return "000"; }
    if off == 5 { return "500"; }
    // 第二步：6.25kHz 步进
    let rem = (freq_khz * 100) % 625;
    let off = (625 - rem) % 625;
    match off {
        0  => return "000",
        25 => return "250",
        50 => return "500",
        75 => return "750",
        _  => {}
    }
    // 第三步：8.33kHz 步进（= 25/3 kHz，LCD 截断到 10Hz 显示 .000/.330/.660）
    let rem = (freq_khz * 3) % 25;
    match rem {
        0  => return "000",
        24 => return "330",
        23 => return "660",
        _  => {}
    }
    "000" // fallback
}

// ===== 下行帧解析器 =====

#[derive(PartialEq)]
enum DnState { WaitAA, WaitFD, WaitLen, Payload, Checksum }

pub struct DownParser {
    buf: [u8; 32],
    len: usize,
    payload_len: usize,
    state: DnState,
    /// 每个 side (0=左, 1=右) 是否刚收到 Len=06 信道号包
    got_channel: [bool; 2],
}

impl DownParser {
    pub fn new() -> Self {
        Self {
            buf: [0; 32],
            len: 0,
            payload_len: 0,
            state: DnState::WaitAA,
            got_channel: [false; 2],
        }
    }

    fn reset(&mut self) {
        self.len = 0;
        self.payload_len = 0;
        self.state = DnState::WaitAA;
    }

    /// 喂入一个字节，返回 true 表示一帧已完整缓冲
    pub fn feed(&mut self, b: u8) -> bool {
        match self.state {
            DnState::WaitAA => {
                if b == 0xAA {
                    self.buf[0] = b;
                    self.len = 1;
                    self.state = DnState::WaitFD;
                }
            }
            DnState::WaitFD => {
                if b == 0xFD {
                    self.buf[1] = b;
                    self.len = 2;
                    self.state = DnState::WaitLen;
                } else {
                    self.reset();
                    if b == 0xAA { self.feed(b); }
                }
            }
            DnState::WaitLen => {
                self.buf[2] = b;
                self.len = 3;
                self.payload_len = b as usize;
                if self.payload_len == 0 || self.payload_len > 20 {
                    self.reset();
                } else {
                    self.state = DnState::Payload;
                }
            }
            DnState::Payload => {
                if self.len < 32 {
                    self.buf[self.len] = b;
                    self.len += 1;
                }
                if self.len == 3 + self.payload_len {
                    self.state = DnState::Checksum;
                }
            }
            DnState::Checksum => {
                if self.len < 32 {
                    self.buf[self.len] = b;
                    self.len += 1;
                }
                // XOR 校验: Len ^ Payload 各字节
                let mut xor = 0u8;
                for i in 2..self.len - 1 { xor ^= self.buf[i]; }
                if self.buf[self.len - 1] == xor {
                    self.state = DnState::WaitAA;
                    return true; // 有效帧
                }
                // 校验失败 → 丢弃，重新同步
                self.reset();
                return false;
            }
        }
        false
    }

    /// 将完整帧解析并应用到 RadioState
    pub fn apply_to_state(&mut self, rs: &mut RadioState) {
        rs.radio_alive = true;
        match self.payload_len {
            2 => {
                self.apply_icon(rs);
                // TX 判定（实测字节确认）：发射时 0x1D 发功率格数，0x1C 不发（is_busy 保持 false）
                // 接收时 0x1D 发信号格数，0x1C 发 BUSY=ON
                rs.left.is_tx  = rs.left.s_level > 0 && !rs.left.is_busy;
                rs.right.is_tx = rs.right.s_level > 0 && !rs.right.is_busy;
            }
            3 => {}
            6 => self.apply_channel(rs),
            8 => {}
            9 => self.apply_freq(rs),
            _ => {}
        }
    }

    // ---- Len=02: LCD 图标/状态 ----
    fn apply_icon(&self, rs: &mut RadioState) {
        let cmd = self.buf[3];
        let sts = self.buf[4];
        let is_right = (sts & 0x80) != 0;
        let is_on    = (sts & 0x01) != 0;
        let band = if is_right { &mut rs.right } else { &mut rs.left };

        match cmd {
            // SET 图标状态只作为显示来源，不用于判断手动菜单退出。
            // 手动退出仍由 Len=09 频率帧的 menu_exit_count 判定；DTrac 自动宏在 rigctld.rs 内显式清理。
            0x03 => {}
            // MAIN 标记（互斥：设置一侧为 MAIN 时清除另一侧）
            0x14 => {
                band.is_main = is_on;
                if is_on {
                    let other = if is_right { &mut rs.left } else { &mut rs.right };
                    other.is_main = false;
                }
            }
            // 功率等级 L/M/HIGH
            0x15 => {
                let val = sts & 0x7F;
                log::info!("[0x15] sts={:02X} val={:02X} side={}", sts, val, if is_right {"右"} else {"左"});
                if val == 0x01 || val == 0x41 {
                    band.power = PowerLevel::Low;
                    band.power_confirmed = true;
                } else if val == 0x02 || val == 0x42 {
                    band.power = PowerLevel::Mid;
                    band.power_confirmed = true;
                } else if val == 0x03 || val == 0x43 {
                    // HIGH 档：0x03/0x83 表示高功（部分机型会发）
                    band.power = PowerLevel::High;
                    band.power_confirmed = true;
                }
                // val=0x00/0x80：TX结束的清除帧，不代表切换到高功，忽略
            }
            // + 正偏移（S-Meter 来源已确认为 0x1D，0x16 仅控制偏移方向）
            0x16 => {
                band.shift_plus = is_on;
                band.refresh_shift();
            }
            // - 负偏移
            0x17 => {
                band.shift_minus = is_on;
                band.refresh_shift();
            }
            // DEC: CTCSS 解码器
            0x18 => {
                let old = band.tone_type.clone();
                band.tone_seen_mask |= 0x01;
                band.tone_last_frame_us = unsafe { esp_timer_get_time() } as u64;
                band.tone_dec = is_on;
                if is_on { band.tone_dcs = false; }
                band.refresh_tone_type();
                if old != band.tone_type {
                    log::info!("[Tone] {} DEC={} -> {}", if is_right { "RIGHT" } else { "LEFT" }, is_on, band.tone_type.as_str());
                }
            }
            // ENC: CTCSS 编码器
            0x19 => {
                let old = band.tone_type.clone();
                band.tone_seen_mask |= 0x02;
                band.tone_last_frame_us = unsafe { esp_timer_get_time() } as u64;
                band.tone_enc = is_on;
                if is_on { band.tone_dcs = false; }
                band.refresh_tone_type();
                if old != band.tone_type {
                    log::info!("[Tone] {} ENC={} -> {}", if is_right { "RIGHT" } else { "LEFT" }, is_on, band.tone_type.as_str());
                }
            }
            // BUSY 图标（来自 0x1C，bit7=侧, bit0=ON/OFF）
            0x1C => { band.is_busy = is_on; }
            // 条形值（S-Meter/TX功率条）: bit7=侧（已由 band 选取），bits[3:0]=格数(0-9)
            0x1D => { band.s_level = (sts & 0x0F) as u32; }
            // DCS
            0x20 => {
                let old = band.tone_type.clone();
                band.tone_seen_mask |= 0x04;
                band.tone_last_frame_us = unsafe { esp_timer_get_time() } as u64;
                band.tone_dcs = is_on;
                if is_on {
                    band.tone_enc = false;
                    band.tone_dec = false;
                }
                band.refresh_tone_type();
                if old != band.tone_type {
                    log::info!("[Tone] {} DCS={} -> {}", if is_right { "RIGHT" } else { "LEFT" }, is_on, band.tone_type.as_str());
                }
            }
            // AM 模式（CmdID=0x10）
            // 实测：航空 AM 波段（125.000 MHz）时发送，FM 时不发送
            // bit7=侧：sts=0x00=左侧AM，sts=0x80=右侧AM
            // mode 已由 apply_freq 按频率范围直接赋值，此处不再打印高频日志
            0x10 => {
                // 已知：cmd=0x10 不参与 mode 决策；高频日志会阻塞中继，故禁用
            }
            // CmdID=0x25/0x26: 经实测不是 TX 指示，跳过
            // TX 状态由上行帧 PTT 字段决定
            _ => {}
        }
    }

    // ---- Len=06: 信道号（MR 模式）----
    // ★ buf[3] side 字节可能始终为 0x01，与 Len=09 相同。
    //   改用 flag bit7（buf[4]）判断左右，与 apply_freq 保持一致。
    fn apply_channel(&mut self, rs: &mut RadioState) {
        let flag     = self.buf[4];
        let is_right = (flag & 0x80) != 0;
        let side_idx = if is_right { 1usize } else { 0usize };
        let band     = if is_right { &mut rs.right } else { &mut rs.left };

        band.channel.clear();
        let _ = band.channel.push_str("Ch:");
        let mut has_digit = false;
        for i in 6..usize::min(self.len.saturating_sub(1), 9) {
            let c = self.buf[i];
            if c >= b'0' && c <= b'9' {
                let _ = band.channel.push(c as char);
                has_digit = true;
            } else if c == b' ' {
                let _ = band.channel.push('0');  // 前导空格→'0'，保持3位固定宽度
            } else if c >= b'!' && c < 0x7F {
                let _ = band.channel.push(c as char);
            }
        }
        if !has_digit {
            band.channel.clear();
            let _ = band.channel.push_str("VFO");
        }

        band.menu_in_value = false;  // 新菜单项被选中，退出值编辑状态
        self.got_channel[side_idx] = true;
    }

    // ---- Len=09: 频率/文本 ASCII ----
    //
    // ★ 实测纠正 (2026-04-04):
    //   flag bit7 是物理左/右指示，与 MAIN 状态无关。
    //   bit7=0 (0x40) → 物理左波段频率
    //   bit7=1 (0xC0) → 物理右波段频率
    //
    //   早期误认为 bit7 表示 MAIN/非MAIN 侧，仅因观测时恰好 RIGHT=MAIN
    //   导致两者重合。实机验证 LEFT=MAIN 时该假设不成立。
    fn apply_freq(&mut self, rs: &mut RadioState) {
        if self.len < 13 { return; }
        let flag = self.buf[4];
        // flag bit7 直接表示物理左(0)/右(1)，与 MAIN 侧无关
        let is_right = (flag & 0x80) != 0;
        let side_idx = if is_right { 1usize } else { 0usize };

        let band = if is_right { &mut rs.right } else { &mut rs.left };

        let raw = &self.buf[6..12];
        let is_freq = raw.iter().all(|&c| c == b' ' || (c >= b'0' && c <= b'9'));


        if is_freq {
            // 解析 6 位 ASCII 为 kHz 整数
            let h = if raw[0] == b' ' { 0u32 } else { (raw[0] - b'0') as u32 };
            let t = if raw[1] == b' ' { 0u32 } else { (raw[1] - b'0') as u32 };
            let u = if raw[2] == b' ' { 0u32 } else { (raw[2] - b'0') as u32 };
            let d3 = (raw[3] - b'0') as u32;
            let d4 = (raw[4] - b'0') as u32;
            let d5 = (raw[5] - b'0') as u32;
            let mhz = h * 100 + t * 10 + u;
            let freq_khz = mhz * 1000 + d3 * 100 + d4 * 10 + d5;

            // 构建频率字符串：XXX.XXX.YYY（含末三位 100Hz/10Hz/1Hz）
            band.freq.clear();
            for &c in &raw[0..3] {
                let _ = band.freq.push(if c == b' ' { b'0' } else { c } as char);
            }
            let _ = band.freq.push('.');
            for &c in &raw[3..6] {
                let _ = band.freq.push(c as char);
            }
            // 追加末三位（由步进网格推算，协议不传输此精度）
            let sub = compute_sub_khz(freq_khz);
            let _ = band.freq.push('.');
            let _ = band.freq.push_str(sub);
            band.last_freq_frame_us = unsafe { esp_timer_get_time() } as u64;

            if !self.got_channel[side_idx] {
                band.channel.clear();
                let _ = band.channel.push_str("VFO");
            }

            // 收到频率帧 → 机头已回到频率区，ESP32 同帧清空文本显示
            band.display_text.clear();
            // 延迟退出菜单模式（CTCSS 显示时频率/文本帧交替，连续 2 帧才算真退出）
            if band.is_set {
                band.menu_exit_count = band.menu_exit_count.saturating_add(1);
                if band.menu_exit_count >= 2 {
                    band.is_set = false;
                    band.menu_text.clear();
                    band.menu_in_value = false;
                    band.menu_exit_count = 0;
                }
            } else {
                band.menu_exit_count = 0;
            }

            // 108-136 MHz = 民用航空 AM 波段（TH-9800 实测），直接按频率赋值，不依赖 cmd=0x10
            band.mode.clear();
            if mhz >= 108 && mhz <= 136 {
                let _ = band.mode.push_str("AM");
            } else {
                let _ = band.mode.push_str("FM");
            }

            self.got_channel[side_idx] = false;
            return;
        }
        // 非频率文本（模式名、菜单名）不更新 freq 字段

        // 功率等级文本检测（LOW键循环时不发 0x15，只发 Len=09 文本帧）
        // 实测文本带前导空格：" MID1 "=20 4D 49 44...  "  LOW "=20 20 4C 4F...  " HIGH "=20 48 49 47...
        // 跳过前导空格后再比较，避免因偏移错误而漏匹配
        let text_start = raw.iter().position(|&c| c != b' ').unwrap_or(6);
        let mut detected_power = false;
        if text_start < 6 {
            let t = &raw[text_start..];
            if t.get(0) == Some(&b'L') && t.get(1) == Some(&b'O') && t.get(2) == Some(&b'W') {
                log::info!("[Len09] → 功率文本: LOW → 低功");
                band.power = PowerLevel::Low;
                band.power_confirmed = true;
                detected_power = true;
            } else if t.get(0) == Some(&b'M') && t.get(1) == Some(&b'I') && t.get(2) == Some(&b'D') {
                log::info!("[Len09] → 功率文本: MID → 中功");
                band.power = PowerLevel::Mid;
                band.power_confirmed = true;
                detected_power = true;
            } else if t.get(0) == Some(&b'H') && t.get(1) == Some(&b'I') && t.get(2) == Some(&b'G') {
                log::info!("[Len09] → 功率文本: HIGH → 高功");
                band.power = PowerLevel::High;
                band.power_confirmed = true;
                detected_power = true;
            }
        }

        // had_channel=true → 上一帧是 Len=06（顶级菜单滚动）
        // had_channel=false → 直接收到 Len=09（已进入值编辑）
        let had_channel = self.got_channel[side_idx];
        self.got_channel[side_idx] = false;

        if detected_power {
            band.display_text.clear();
            return;
        }

        // 非频率 + 稳定显示标志 (flag bit6) + 非功率文本 → 菜单名称或菜单值
        if !is_freq && (flag & 0x40) != 0 {
            let mt_start = raw.iter().position(|&c| c > b' ').unwrap_or(6);
            if mt_start < 6 {
                let mut new_text: heapless::String<12> = heapless::String::new();
                for &c in &raw[mt_start..] {
                    if c > b' ' && c < 0x7F { let _ = new_text.push(c as char); }
                }
                if !new_text.is_empty() {
                    if new_text != band.menu_text {
                        band.menu_text = new_text;
                    }
                    band.is_set = true;
                    band.menu_exit_count = 0;
                    // had_channel=true → 顶级菜单（Len=06+Len=09 成对）；false → 值编辑（仅 Len=09）
                    band.menu_in_value = !had_channel;
                }
            }
        }

        let first = raw.iter().position(|&c| c > b' ');
        let last = raw.iter().rposition(|&c| c > b' ');
        if let (Some(first), Some(last)) = (first, last) {
            band.display_text.clear();
            for &c in &raw[first..=last] {
                if c >= b' ' && c < 0x7F {
                    let _ = band.display_text.push(c as char);
                }
            }
        }
    }
}

// ===== 上行帧解析器 =====

pub struct UpParser {
    buf: [u8; 20],
    len: usize,
    pub state: u8,  // pub: relay_up_thread 需要判断是否在帧同步中
}

impl UpParser {
    pub fn new() -> Self {
        Self { buf: [0; 20], len: 0, state: 0 }
    }

    /// 喂入一个字节，返回 true 表示上行帧已完整缓冲
    ///
    /// 实测帧长度为 16 字节（协议文档描述的 00 后缀不存在）:
    ///   AA FD 0C [P0..P11] SUM  =  3 + 12 + 1 = 16 字节
    pub fn feed(&mut self, b: u8) -> bool {
        match self.state {
            0 => if b == 0xAA { self.buf[0] = b; self.len = 1; self.state = 1; }
            1 => if b == 0xFD { self.buf[1] = b; self.len = 2; self.state = 2; }
                 else { self.state = 0; }
            2 => {
                self.buf[2] = b; self.len = 3;
                if b == 0x0C { self.state = 3; } else { self.state = 0; }
            }
            3 => {
                if self.len < 20 { self.buf[self.len] = b; self.len += 1; }
                if self.len == 16 {   // ← 实测 16 字节，不是 17
                    self.state = 0;
                    self.len = 0;
                    return true;
                }
            }
            _ => self.state = 0,
        }
        false
    }

    /// 获取最近完成的帧副本（16 字节），供 relay_up_thread 修改后转发
    pub fn get_frame(&self) -> [u8; 16] {
        let mut f = [0u8; 16];
        f.copy_from_slice(&self.buf[..16]);
        f
    }

    /// 诊断日志：每帧打印关键字段（首帧+每 20 帧打印）
    pub fn log_diag(&self, count: u32) {
        if count <= 3 || count % 20 == 0 {
            use core::fmt::Write as FW;
            let mut hex: heapless::String<64> = heapless::String::new();
            for i in 3..15 {  // P[0..P[11]]
                let _ = write!(hex, "{:02X} ", self.buf[i]);
            }
            let ptt      = self.buf[4];
            // ★ 实测: VOL在buf[8-10], SQL在buf[11-13]
            let vol_flag = self.buf[8];
            let vol_raw  = (self.buf[9] as u16) | ((self.buf[10] as u16) << 8);
            let sql_flag = self.buf[11];
            let sql_raw  = (self.buf[12] as u16) | ((self.buf[13] as u16) << 8);
            let sum_byte = self.buf[15];
            let mut sum_calc: u8 = 0;
            for i in 3..15 { sum_calc = sum_calc.wrapping_add(self.buf[i]); }
            log::info!("[上行#{:04}] PTT={:02X} VFLAG={:02X} VOL={:04} SFLAG={:02X} SQL={:04} SUM={:02X}(计算{:02X}) [{}]",
                count, ptt, vol_flag, vol_raw, sql_flag, sql_raw,
                sum_byte, sum_calc, hex.as_str().trim());
        }
    }

    /// 将上行帧应用到 RadioState
    ///
    /// 16 字节结构 (buf 绝对索引):
    ///   [0]=AA [1]=FD [2]=0C
    ///   [3]=P[0]=0x84  [4]=P[1]=PTT     [5]=P[2]=旋钮
    ///   [6]=P[3]=FF    [7]=P[4]=按键标志
    ///   [8]=P[5]=VOL标志 (无按键时) / 键码 (按键时)
    ///   [9]=P[6]=VOL低8  [10]=P[7]=VOL高8
    ///   [11]=P[8]=SQL标志  [12]=P[9]=SQL低8  [13]=P[10]=SQL高8
    ///   [14]=P[11]=00 (固定)  [15]=SUM
    ///
    /// ★ 实测纠正: VOL/SQL 字段比协议文档描述的偏移 -1 字节
    pub fn apply_to_state(&self, rs: &mut RadioState) {
        // ---- 按键检测日志（P[3]=0x00 表示有键按下，P[4]=键码）----
        if self.buf[6] == 0x00 {
            let key = self.buf[7];
            log::info!("[上行按键] key=0x{:02X}", key);
        }
        // 注意：不在此处 log warn，SUM 信息已通过 log_diag 每 20 帧打印一次
        // 大量日志会阻塞 relay 线程导致 UART FIFO 溢出，电台卡死

        // ---- VOL: buf[8]=标志, buf[9:10]=16-bit ADC 小端序 ----
        // 无按键时 buf[8]=VOL_FLAG(0x01=左转动/0x81=右或空闲)
        // 有按键时 buf[8]=键码，VOL 更新跳过（0x01/0x81 不在键码范围内，自然不匹配）
        // 右侧空闲时 VOL=0xFFFF，经 <=1023 过滤后不更新
        let vol_flag = self.buf[8];   // ★ 实测在 buf[8]，不是 buf[9]
        let vol_raw  = (self.buf[9] as u16) | ((self.buf[10] as u16) << 8);

        if vol_raw <= 1023 {
            if vol_flag == 0x01 {
                rs.left.vol = vol_raw;
            } else if vol_flag == 0x81 {
                if adc_changed(rs.right.vol, vol_raw, 2) {
                    rs.right.vol = vol_raw;
                }
            }
        }

        // ---- SQL: buf[11]=标志, buf[12:13]=16-bit ADC 小端序 ----
        let sql_flag = self.buf[11];  // ★ 实测在 buf[11]，不是 buf[12]
        let sql_raw  = (self.buf[12] as u16) | ((self.buf[13] as u16) << 8);

        if sql_raw <= 1023 {
            if sql_flag == 0x02 {
                rs.left.sql = sql_raw;
            } else if sql_flag == 0x82 {
                if adc_changed(rs.right.sql, sql_raw, 2) {
                    rs.right.sql = sql_raw;
                }
            }
        }

        rs.head_count = rs.head_count.wrapping_add(1);
    }
}

/// ADC 死区滤波: 仅当变化超过 threshold 才视为有效变化
fn adc_changed(old: u16, new: u16, threshold: u16) -> bool {
    let diff = if new > old { new - old } else { old - new };
    diff > threshold
}
