// ===================================================================
// PC 上位机通信模块（原生 USB OTG / TinyUSB CDC-ACM）
//
// 日志继续走 CH343/UART0
// 上位机通信走 ESP32-S3 原生 USB OTG 口，枚举成独立 CDC-ACM 串口
// 协议: [0xAA][0x55][Type][LenLo][LenHi][Payload...][CRC16-CCITT]
// ===================================================================

use crate::state::{PowerLevel, RadioState, SharedState};
use crate::uart::{build_key_frame, build_knob_frame};
use esp_idf_svc::sys::*;

const SYNC0: u8 = 0xAA;
const SYNC1: u8 = 0x55;

// PC → ESP32
const CMD_HEARTBEAT: u8 = 0x01;
const CMD_GET_STATE: u8 = 0x02;
const CMD_RAW_KEY_PRESS: u8 = 0x10;
const CMD_RAW_KEY_RELEASE: u8 = 0x11;
const CMD_RAW_KNOB: u8 = 0x12;
const CMD_SET_VOL: u8 = 0x25;
const CMD_SET_SQL: u8 = 0x26;
const CMD_SET_PTT: u8 = 0x27;
const CMD_POWER_TOGGLE: u8 = 0x28;

// ESP32 → PC
const RPT_HEARTBEAT_ACK: u8 = 0x81;
const RPT_STATE: u8 = 0x82;
const RPT_ERROR: u8 = 0x85;

const PTT_TIMEOUT_US: u64 = 30_000_000;
const PC_HEARTBEAT_TIMEOUT_US: u64 = 3_000_000;

// ===== TinyUSB minimal FFI (manual declarations) =====

#[repr(C)]
struct tinyusb_config_t {
    device_descriptor: *const core::ffi::c_void,
    string_descriptor: *const *const i8,
    string_descriptor_count: i32,
    external_phy: bool,
    configuration_descriptor: *const u8,
    self_powered: bool,
    vbus_monitor_io: i32,
}

#[repr(C)]
struct tinyusb_config_cdcacm_t {
    usb_dev: i32,
    cdc_port: i32,
    rx_unread_buf_sz: usize,
    callback_rx: Option<extern "C" fn(i32, *mut core::ffi::c_void)>,
    callback_rx_wanted_char: Option<extern "C" fn(i32, *mut core::ffi::c_void)>,
    callback_line_state_changed: Option<extern "C" fn(i32, *mut core::ffi::c_void)>,
    callback_line_coding_changed: Option<extern "C" fn(i32, *mut core::ffi::c_void)>,
}

const TINYUSB_USBDEV_0: i32 = 0;
const TINYUSB_CDC_ACM_0: i32 = 0;

unsafe extern "C" {
    fn tinyusb_driver_install(config: *const tinyusb_config_t) -> esp_err_t;
    fn tusb_cdc_acm_init(cfg: *const tinyusb_config_cdcacm_t) -> esp_err_t;
    fn tinyusb_cdcacm_read(itf: i32, out_buf: *mut u8, out_buf_sz: usize, rx_data_size: *mut usize) -> esp_err_t;
    fn tinyusb_cdcacm_write_queue(itf: i32, in_buf: *const u8, in_size: usize) -> usize;
    fn tinyusb_cdcacm_write_flush(itf: i32, timeout_ticks: u32) -> esp_err_t;
    fn tusb_cdc_acm_initialized(itf: i32) -> bool;
}

// ===== CRC16-CCITT =====
fn crc16_ccitt(data: &[u8]) -> u16 {
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

pub fn init_pc_comm() {
    unsafe {
        let tusb_cfg = tinyusb_config_t {
            device_descriptor: core::ptr::null(),
            string_descriptor: core::ptr::null(),
            string_descriptor_count: 0,
            external_phy: false,
            configuration_descriptor: core::ptr::null(),
            self_powered: false,
            vbus_monitor_io: -1,  // GPIO_NUM_NC: 不监控 VBUS
        };
        let ret = tinyusb_driver_install(&tusb_cfg);
        if ret != ESP_OK {
            log::error!("[PC通信] tinyusb_driver_install 失败: {}", ret);
            return;
        }

        let acm_cfg = tinyusb_config_cdcacm_t {
            usb_dev: TINYUSB_USBDEV_0,
            cdc_port: TINYUSB_CDC_ACM_0,
            rx_unread_buf_sz: 64,
            callback_rx: None,
            callback_rx_wanted_char: None,
            callback_line_state_changed: None,
            callback_line_coding_changed: None,
        };
        let ret = tusb_cdc_acm_init(&acm_cfg);
        if ret != ESP_OK {
            log::error!("[PC通信] tusb_cdc_acm_init 失败: {}", ret);
            return;
        }
    }
    log::info!("[PC通信] TinyUSB CDC-ACM 通信口就绪（原生 USB OTG）");
}

fn send_frame(typ: u8, payload: &[u8]) {
    unsafe {
        if !tusb_cdc_acm_initialized(TINYUSB_CDC_ACM_0) {
            return;
        }
    }

    // 用栈上数组代替 Vec，避免频繁堆分配（最大 payload 60 + header 5 + crc 2 = 67）
    let len = payload.len() as u16;
    let total = 5 + payload.len() + 2;
    if total > 128 { return; }  // 安全上限
    let mut frame = [0u8; 128];
    frame[0] = SYNC0;
    frame[1] = SYNC1;
    frame[2] = typ;
    frame[3] = (len & 0xFF) as u8;
    frame[4] = (len >> 8) as u8;
    frame[5..5 + payload.len()].copy_from_slice(payload);
    let crc = crc16_ccitt(&frame[..5 + payload.len()]);
    frame[5 + payload.len()] = (crc & 0xFF) as u8;
    frame[5 + payload.len() + 1] = (crc >> 8) as u8;

    unsafe {
        let _ = tinyusb_cdcacm_write_queue(TINYUSB_CDC_ACM_0, frame.as_ptr(), total);
        let _ = tinyusb_cdcacm_write_flush(TINYUSB_CDC_ACM_0, 10); // 10 ticks ≈ 10ms，非永久阻塞
    }
}

fn send_ack() {
    send_frame(RPT_HEARTBEAT_ACK, &[]);
}

fn send_error(msg: &str) {
    let bytes = msg.as_bytes();
    let len = bytes.len().min(240);
    send_frame(RPT_ERROR, &bytes[..len]);
}

fn send_state_report(rs: &RadioState) {
    let mut p = [0u8; 60];
    let mut flags: u8 = 0;
    if rs.radio_alive { flags |= 0x01; }
    if rs.pc_alive { flags |= 0x02; }
    if rs.left.is_main { flags |= 0x04; }
    if rs.right.is_main { flags |= 0x08; }
    if rs.macro_running { flags |= 0x10; }
    if rs.ptt_override { flags |= 0x20; }
    p[0] = flags;

    fn encode_band(band: &crate::state::BandState, out: &mut [u8]) {
        let fb = band.freq.as_bytes();
        let flen = fb.len().min(12);
        out[0..flen].copy_from_slice(&fb[..flen]);
        let mb = band.mode.as_bytes();
        let mlen = mb.len().min(2);
        out[12..12 + mlen].copy_from_slice(&mb[..mlen]);
        out[14] = match band.power {
            PowerLevel::High => 0,
            PowerLevel::Mid => 1,
            PowerLevel::Low => 3,
        };
        out[15] = band.s_level as u8;
        out[16] = (band.vol & 0xFF) as u8;
        out[17] = (band.vol >> 8) as u8;
        out[18] = (band.sql & 0xFF) as u8;
        out[19] = (band.sql >> 8) as u8;
        let mut bf: u8 = 0;
        if band.is_tx { bf |= 0x01; }
        if band.is_busy { bf |= 0x02; }
        if band.tone_enc { bf |= 0x04; }
        if band.tone_dec { bf |= 0x08; }
        if band.tone_dcs { bf |= 0x10; }
        if band.shift_plus { bf |= 0x20; }
        if band.shift_minus { bf |= 0x40; }
        if band.is_set { bf |= 0x80; }
        out[20] = bf;
        let cb = band.channel.as_bytes();
        let clen = cb.len().min(4);
        out[21..21 + clen].copy_from_slice(&cb[..clen]);
    }

    encode_band(&rs.left, &mut p[1..26]);
    encode_band(&rs.right, &mut p[26..51]);
    p[51] = (rs.body_count & 0xFF) as u8;
    p[52] = ((rs.body_count >> 8) & 0xFF) as u8;
    p[53] = ((rs.body_count >> 16) & 0xFF) as u8;
    p[54] = ((rs.body_count >> 24) & 0xFF) as u8;
    p[55] = (rs.head_count & 0xFF) as u8;
    p[56] = ((rs.head_count >> 8) & 0xFF) as u8;
    p[57] = ((rs.head_count >> 16) & 0xFF) as u8;
    p[58] = ((rs.head_count >> 24) & 0xFF) as u8;
    p[59] = (rs.pc_count & 0xFF) as u8;
    send_frame(RPT_STATE, &p);
}

enum ParseState { WaitSync0, WaitSync1, WaitType, WaitLenLo, WaitLenHi, Payload, WaitCrcLo, WaitCrcHi }

pub enum PcCommand {
    Heartbeat,
    GetState,
    RawKeyPress(u8),
    RawKeyRelease,
    RawKnob(u8),
    SetVol(u8),
    SetSql(u8),
    SetPtt(bool),
    PowerToggle,
}

pub struct PcParser {
    state: ParseState,
    typ: u8,
    len: u16,
    payload: [u8; 256],
    pos: usize,
    crc_lo: u8,
}

impl PcParser {
    pub fn new() -> Self {
        Self { state: ParseState::WaitSync0, typ: 0, len: 0, payload: [0; 256], pos: 0, crc_lo: 0 }
    }

    pub fn feed(&mut self, b: u8) -> Option<PcCommand> {
        match self.state {
            ParseState::WaitSync0 => if b == SYNC0 { self.state = ParseState::WaitSync1; },
            ParseState::WaitSync1 => if b == SYNC1 { self.state = ParseState::WaitType; } else { self.state = ParseState::WaitSync0; },
            ParseState::WaitType => { self.typ = b; self.state = ParseState::WaitLenLo; }
            ParseState::WaitLenLo => { self.len = b as u16; self.state = ParseState::WaitLenHi; }
            ParseState::WaitLenHi => {
                self.len |= (b as u16) << 8;
                self.pos = 0;
                if self.len == 0 { self.state = ParseState::WaitCrcLo; }
                else if self.len as usize <= self.payload.len() { self.state = ParseState::Payload; }
                else { self.state = ParseState::WaitSync0; }
            }
            ParseState::Payload => {
                self.payload[self.pos] = b;
                self.pos += 1;
                if self.pos >= self.len as usize { self.state = ParseState::WaitCrcLo; }
            }
            ParseState::WaitCrcLo => { self.crc_lo = b; self.state = ParseState::WaitCrcHi; }
            ParseState::WaitCrcHi => {
                let rx_crc = (self.crc_lo as u16) | ((b as u16) << 8);
                // CRC 验证：用栈上数组代替 Vec
                let plen = self.len as usize;
                let mut frame = [0u8; 261]; // 5 header + max 256 payload
                frame[0] = SYNC0; frame[1] = SYNC1; frame[2] = self.typ;
                frame[3] = (self.len & 0xFF) as u8; frame[4] = (self.len >> 8) as u8;
                frame[5..5 + plen].copy_from_slice(&self.payload[..plen]);
                let calc = crc16_ccitt(&frame[..5 + plen]);
                self.state = ParseState::WaitSync0;
                if calc == rx_crc { return self.decode(); }
            }
        }
        None
    }

    fn decode(&self) -> Option<PcCommand> {
        match self.typ {
            CMD_HEARTBEAT => Some(PcCommand::Heartbeat),
            CMD_GET_STATE => Some(PcCommand::GetState),
            CMD_RAW_KEY_PRESS if self.len >= 1 => Some(PcCommand::RawKeyPress(self.payload[0])),
            CMD_RAW_KEY_RELEASE => Some(PcCommand::RawKeyRelease),
            CMD_RAW_KNOB if self.len >= 1 => Some(PcCommand::RawKnob(self.payload[0])),
            CMD_SET_VOL if self.len >= 2 => Some(PcCommand::SetVol(self.payload[1])),
            CMD_SET_SQL if self.len >= 2 => Some(PcCommand::SetSql(self.payload[1])),
            CMD_SET_PTT if self.len >= 1 => Some(PcCommand::SetPtt(self.payload[0] != 0)),
            CMD_POWER_TOGGLE => Some(PcCommand::PowerToggle),
            _ => None,
        }
    }
}

pub fn pc_comm_thread(
    uart_host: &esp_idf_svc::hal::uart::UartDriver<'_>,
    state: SharedState,
    power_pin_num: i32,
) {
    let mut parser = PcParser::new();
    let mut rx_buf = [0u8; 64];
    let init_us = unsafe { esp_timer_get_time() } as u64;
    let mut last_hb_us = init_us;
    let mut last_report_us = init_us;

    log::info!("[PC通信] TinyUSB CDC-ACM 线程启动");

    loop {
        let now_us = unsafe { esp_timer_get_time() } as u64;

        unsafe {
            if tusb_cdc_acm_initialized(TINYUSB_CDC_ACM_0) {
                let mut rx_size: usize = 0;
                if tinyusb_cdcacm_read(TINYUSB_CDC_ACM_0, rx_buf.as_mut_ptr(), rx_buf.len(), &mut rx_size) == ESP_OK && rx_size > 0 {
                    for i in 0..rx_size {
                        if let Some(cmd) = parser.feed(rx_buf[i]) {
                            if matches!(cmd, PcCommand::Heartbeat) { last_hb_us = now_us; }
                            handle_command(cmd, uart_host, &state, power_pin_num, now_us);
                        }
                    }
                }
            }
        }

        let ptt_expired = {
            let mut s = state.lock().unwrap();
            if s.ptt_override && (now_us - s.ptt_start_us) > PTT_TIMEOUT_US {
                s.ptt_override = false;
                true
            } else {
                false
            }
        }; // 锁在此释放，再调 write_flush 不阻塞中继线程
        if ptt_expired { send_error("PTT timeout"); }

        if (now_us - last_hb_us) > PC_HEARTBEAT_TIMEOUT_US {
            let mut s = state.lock().unwrap();
            if s.pc_alive {
                s.pc_alive = false;
                s.ptt_override = false;
                s.vol_override = None;
                s.sql_override = None;
                s.key_override = None;
                s.key_release = false;
                s.knob_inject = None;
                s.vol_changed = false;
                s.sql_changed = false;
                s.macro_running = false;
            }
        }

        // 仅在 PC 已连接（收到心跳）时才发送周期性报告，避免无人接收时 write_flush 空转
        let maybe_snapshot = {
            let s = state.lock().unwrap();
            if s.pc_alive && (now_us - last_report_us) > 200_000 {
                Some(s.clone())
            } else {
                None
            }
        };
        if let Some(snapshot) = maybe_snapshot {
            send_state_report(&snapshot);
            last_report_us = now_us;
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn handle_command(cmd: PcCommand, uart_host: &esp_idf_svc::hal::uart::UartDriver<'_>, state: &SharedState, power_pin_num: i32, now_us: u64) {
    match cmd {
        PcCommand::Heartbeat => {
            let mut s = state.lock().unwrap();
            s.pc_alive = true;
            s.pc_count = s.pc_count.wrapping_add(1);
            drop(s);
            send_ack();
        }
        PcCommand::GetState => {
            let snapshot = state.lock().unwrap().clone();
            send_state_report(&snapshot);
        }
        PcCommand::RawKeyPress(key) => {
            let mut s = state.lock().unwrap();
            s.key_override = Some(key);
        }
        PcCommand::RawKeyRelease => {
            let mut s = state.lock().unwrap();
            s.key_release = true;
        }
        PcCommand::RawKnob(step) => {
            let mut s = state.lock().unwrap();
            s.knob_inject = Some(step);
        }
        PcCommand::SetVol(pct) => {
            let mut s = state.lock().unwrap();
            if pct == 0xFF { s.vol_override = None; }
            else { s.vol_override = Some((20 + (pct as u32) * 940 / 100) as u16); s.vol_changed = true; }
        }
        PcCommand::SetSql(pct) => {
            let mut s = state.lock().unwrap();
            if pct == 0xFF { s.sql_override = None; }
            else { s.sql_override = Some((20 + (pct as u32) * 980 / 100) as u16); s.sql_changed = true; }
        }
        PcCommand::SetPtt(on) => {
            let mut s = state.lock().unwrap();
            s.ptt_override = on;
            if on { s.ptt_start_us = now_us; }
        }
        PcCommand::PowerToggle => {
            log::info!("[PC通信] 收到开关机指令，GPIO{} 脉冲 1.2s", power_pin_num);
            unsafe { gpio_set_level(power_pin_num, 1); }
            std::thread::sleep(std::time::Duration::from_millis(1200));
            unsafe { gpio_set_level(power_pin_num, 0); }
            log::info!("[PC通信] GPIO{} 脉冲结束", power_pin_num);
        }
    }
}
