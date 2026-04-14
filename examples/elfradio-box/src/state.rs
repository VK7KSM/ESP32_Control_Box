// ===================================================================
// 电台状态解码（从 ESP32 STATE_REPORT 60 字节载荷）
// ===================================================================

use std::sync::{Arc, Mutex};

/// 单侧波段状态
#[derive(Clone, Debug)]
pub struct BandState {
    pub freq: String,       // "438.500.000"
    pub mode: String,       // "FM" / "AM"
    pub power: String,      // "HIGH" / "MID" / "LOW"
    pub s_level: u8,        // 0-9
    pub vol_raw: u16,       // ADC 原始值
    pub sql_raw: u16,       // ADC 原始值
    pub is_tx: bool,
    pub is_busy: bool,
    pub tone_enc: bool,
    pub tone_dec: bool,
    pub tone_dcs: bool,
    pub shift_plus: bool,
    pub shift_minus: bool,
    pub is_set: bool,
    pub channel: String,    // "VFO" / "012"
}

impl BandState {
    pub fn vol_pct(&self) -> u32 {
        if self.vol_raw <= 20 { return 0; }
        let v = (self.vol_raw as u32).min(960);
        (v - 20) * 100 / 940
    }

    pub fn sql_pct(&self) -> u32 {
        if self.sql_raw <= 20 { return 0; }
        let v = (self.sql_raw as u32).min(1000);
        (v - 20) * 100 / 980
    }

    pub fn tone_str(&self) -> &str {
        if self.tone_dcs { "DCS" }
        else if self.tone_enc && self.tone_dec { "T/R" }
        else if self.tone_enc { "ENC" }
        else if self.tone_dec { "DEC" }
        else { "" }
    }

    pub fn shift_str(&self) -> &str {
        if self.shift_plus { "+Shft" }
        else if self.shift_minus { "-Shft" }
        else { "" }
    }
}

impl Default for BandState {
    fn default() -> Self {
        Self {
            freq: "---.---".into(),
            mode: "FM".into(),
            power: "HIGH".into(),
            s_level: 0,
            vol_raw: 0,
            sql_raw: 0,
            is_tx: false,
            is_busy: false,
            tone_enc: false,
            tone_dec: false,
            tone_dcs: false,
            shift_plus: false,
            shift_minus: false,
            is_set: false,
            channel: "VFO".into(),
        }
    }
}

/// 电台整体状态
#[derive(Clone, Debug)]
pub struct RadioState {
    pub radio_alive: bool,
    pub pc_alive: bool,
    pub left_main: bool,
    pub right_main: bool,
    pub macro_running: bool,
    pub ptt_override: bool,
    pub left: BandState,
    pub right: BandState,
    pub body_count: u32,
    pub head_count: u32,
    pub pc_count: u8,
}

impl Default for RadioState {
    fn default() -> Self {
        Self {
            radio_alive: false,
            pc_alive: false,
            left_main: false,
            right_main: false,
            macro_running: false,
            ptt_override: false,
            left: BandState::default(),
            right: BandState::default(),
            body_count: 0,
            head_count: 0,
            pc_count: 0,
        }
    }
}

pub type SharedState = Arc<Mutex<RadioState>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(Mutex::new(RadioState::default()))
}

/// 解码 25 字节波段状态
fn decode_band(data: &[u8]) -> BandState {
    let freq = String::from_utf8_lossy(&data[0..12])
        .trim_end_matches('\0').to_string();
    let mode = String::from_utf8_lossy(&data[12..14])
        .trim_end_matches('\0').to_string();
    let power = match data[14] {
        0 => "HIGH",
        1 => "MID",
        3 => "LOW",
        _ => "?",
    }.to_string();
    let s_level = data[15];
    let vol_raw = u16::from_le_bytes([data[16], data[17]]);
    let sql_raw = u16::from_le_bytes([data[18], data[19]]);
    let bf = data[20];
    let channel = String::from_utf8_lossy(&data[21..25])
        .trim_end_matches('\0').to_string();

    BandState {
        freq, mode, power, s_level, vol_raw, sql_raw,
        is_tx:       bf & 0x01 != 0,
        is_busy:     bf & 0x02 != 0,
        tone_enc:    bf & 0x04 != 0,
        tone_dec:    bf & 0x08 != 0,
        tone_dcs:    bf & 0x10 != 0,
        shift_plus:  bf & 0x20 != 0,
        shift_minus: bf & 0x40 != 0,
        is_set:      bf & 0x80 != 0,
        channel,
    }
}

/// 解码 60 字节 STATE_REPORT 载荷
pub fn decode_state_report(payload: &[u8]) -> Option<RadioState> {
    if payload.len() < 60 { return None; }

    let flags = payload[0];
    Some(RadioState {
        radio_alive:  flags & 0x01 != 0,
        pc_alive:     flags & 0x02 != 0,
        left_main:    flags & 0x04 != 0,
        right_main:   flags & 0x08 != 0,
        macro_running: flags & 0x10 != 0,
        ptt_override: flags & 0x20 != 0,
        left:  decode_band(&payload[1..26]),
        right: decode_band(&payload[26..51]),
        body_count: u32::from_le_bytes([payload[51], payload[52], payload[53], payload[54]]),
        head_count: u32::from_le_bytes([payload[55], payload[56], payload[57], payload[58]]),
        pc_count: payload[59],
    })
}
