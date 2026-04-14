// ===================================================================
// elfRadio BOX 通信协议
// 帧格式: [0xAA][0x55][Type][LenLo][LenHi][Payload][CRC16-CCITT]
// 日志与协议共用同一串口
// ===================================================================

pub const SYNC0: u8 = 0xAA;
pub const SYNC1: u8 = 0x55;

// PC → ESP32
pub const CMD_HEARTBEAT: u8 = 0x01;
pub const CMD_GET_STATE: u8 = 0x02;
pub const CMD_RAW_KEY_PRESS: u8 = 0x10;
pub const CMD_RAW_KEY_REL: u8 = 0x11;
pub const CMD_RAW_KNOB: u8 = 0x12;
pub const CMD_SET_VOL: u8 = 0x25;
pub const CMD_SET_SQL: u8 = 0x26;
pub const CMD_SET_PTT: u8 = 0x27;
pub const CMD_POWER_TOGGLE: u8 = 0x28;

// ESP32 → PC
pub const RPT_HEARTBEAT_ACK: u8 = 0x81;
pub const RPT_STATE_REPORT: u8 = 0x82;
pub const RPT_ERROR: u8 = 0x85;

pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

pub fn encode_frame(typ: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u16;
    let mut frame = Vec::with_capacity(7 + payload.len());
    frame.push(SYNC0);
    frame.push(SYNC1);
    frame.push(typ);
    frame.push((len & 0xFF) as u8);
    frame.push((len >> 8) as u8);
    frame.extend_from_slice(payload);
    let crc = crc16_ccitt(&frame);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);
    frame
}

pub enum ParseEvent {
    Frame { typ: u8, payload: Vec<u8> },
    LogLine(String),
}

pub struct FrameParser {
    phase: u8, // 0=sync0 1=sync1 2=type 3=lenlo 4=lenhi 5=payload 6=crclo 7=crchi
    typ: u8,
    len: u16,
    crc_lo: u8,
    buf: Vec<u8>,
    text_buf: Vec<u8>,
}

impl FrameParser {
    pub fn new() -> Self {
        Self {
            phase: 0,
            typ: 0,
            len: 0,
            crc_lo: 0,
            buf: Vec::with_capacity(512),
            text_buf: Vec::with_capacity(512),
        }
    }

    pub fn feed(&mut self, b: u8) -> Option<ParseEvent> {
        match self.phase {
            0 => {
                if b == SYNC0 { self.phase = 1; }
                else {
                    self.text_buf.push(b);
                    if b == b'\n' {
                        let line = String::from_utf8_lossy(&self.text_buf).to_string();
                        self.text_buf.clear();
                        return Some(ParseEvent::LogLine(line));
                    }
                }
            }
            1 => {
                if b == SYNC1 { self.phase = 2; }
                else {
                    self.text_buf.push(SYNC0);
                    self.text_buf.push(b);
                    self.phase = 0;
                }
            }
            2 => { self.typ = b; self.phase = 3; }
            3 => { self.len = b as u16; self.phase = 4; }
            4 => {
                self.len |= (b as u16) << 8;
                self.buf.clear();
                if self.len == 0 { self.phase = 6; } else { self.phase = 5; }
            }
            5 => {
                self.buf.push(b);
                if self.buf.len() >= self.len as usize {
                    self.phase = 6;
                }
            }
            6 => { self.crc_lo = b; self.phase = 7; }
            7 => {
                let rx_crc = (self.crc_lo as u16) | ((b as u16) << 8);
                let mut hdr = Vec::with_capacity(5 + self.buf.len());
                hdr.push(SYNC0);
                hdr.push(SYNC1);
                hdr.push(self.typ);
                hdr.push((self.len & 0xFF) as u8);
                hdr.push((self.len >> 8) as u8);
                hdr.extend_from_slice(&self.buf);
                let calc = crc16_ccitt(&hdr);
                self.phase = 0;
                if calc == rx_crc {
                    return Some(ParseEvent::Frame { typ: self.typ, payload: std::mem::take(&mut self.buf) });
                }
            }
            _ => self.phase = 0,
        }
        None
    }
}
