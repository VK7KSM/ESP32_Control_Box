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

pub const SYNC0: u8 = 0xAA;
pub const SYNC1: u8 = 0x55;

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
const CMD_SET_WIFI_CRED: u8 = 0x29;
const CMD_WIFI_SCAN:     u8 = 0x2A;

// ESP32 → PC
pub const RPT_HEARTBEAT_ACK: u8 = 0x81;
pub const RPT_STATE:         u8 = 0x82;
pub const RPT_ERROR:         u8 = 0x85;
pub const RPT_WIFI_SCAN:     u8 = 0x86;

pub const PTT_TIMEOUT_US: u64 = 30_000_000;
pub const PC_HEARTBEAT_TIMEOUT_US: u64 = 3_000_000;

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

/// 推送 WiFi 扫描结果（payload 可达 ~600 字节，使用 Vec 而非栈）
/// 格式: [count:u8] [{ssid_len:u8, ssid[N], rssi:i8, auth:u8}]*
fn send_wifi_scan_frame(items: &[crate::state::WifiAp]) {
    unsafe { if !tusb_cdc_acm_initialized(TINYUSB_CDC_ACM_0) { return; } }
    let mut payload: Vec<u8> = Vec::with_capacity(1 + items.len() * 35);
    let count = items.len().min(16) as u8;
    payload.push(count);
    for ap in items.iter().take(16) {
        let ssid = ap.ssid.as_bytes();
        let slen = ssid.len().min(32);
        payload.push(slen as u8);
        payload.extend_from_slice(&ssid[..slen]);
        payload.push(ap.rssi as u8);
        payload.push(ap.auth);
    }
    let len = payload.len() as u16;
    let mut frame: Vec<u8> = Vec::with_capacity(5 + payload.len() + 2);
    frame.extend_from_slice(&[SYNC0, SYNC1, RPT_WIFI_SCAN, (len & 0xFF) as u8, (len >> 8) as u8]);
    frame.extend_from_slice(&payload);
    let crc = crc16_ccitt(&frame);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);
    unsafe {
        let _ = tinyusb_cdcacm_write_queue(TINYUSB_CDC_ACM_0, frame.as_ptr(), frame.len());
        let _ = tinyusb_cdcacm_write_flush(TINYUSB_CDC_ACM_0, 50);
    }
}

/// 构造 60 字节 STATE_REPORT payload（不含帧头帧尾）
pub fn make_state_payload(rs: &RadioState) -> [u8; 60] {
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
    p
}

fn send_state_report(rs: &RadioState) {
    let p = make_state_payload(rs);
    send_frame(RPT_STATE, &p);
}

/// 构造 WIFI_SCAN payload 字节流（不含帧头帧尾）
pub fn make_scan_payload(items: &[crate::state::WifiAp]) -> Vec<u8> {
    let mut payload: Vec<u8> = Vec::with_capacity(1 + items.len() * 35);
    let count = items.len().min(16) as u8;
    payload.push(count);
    for ap in items.iter().take(16) {
        let ssid = ap.ssid.as_bytes();
        let slen = ssid.len().min(32);
        payload.push(slen as u8);
        payload.extend_from_slice(&ssid[..slen]);
        payload.push(ap.rssi as u8);
        payload.push(ap.auth);
    }
    payload
}

/// 编码完整帧 (SYNC + Type + Len + Payload + CRC) 到 Vec
pub fn encode_frame_vec(typ: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u16;
    let mut frame: Vec<u8> = Vec::with_capacity(5 + payload.len() + 2);
    frame.extend_from_slice(&[SYNC0, SYNC1, typ, (len & 0xFF) as u8, (len >> 8) as u8]);
    frame.extend_from_slice(payload);
    let crc = crc16_ccitt(&frame);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);
    frame
}

/// 处理 PC 命令的纯逻辑层（无 USB 依赖），返回需要发回的帧列表 [(typ, payload), ...]
/// 调用者负责把这些帧通过相应通道（USB / TCP）发出。
/// PowerToggle / SetWifiCred 内部 sleep 或 esp_restart()，会阻塞调用线程（USB pc_comm 或 TCP handler）。
pub fn dispatch_command(
    cmd: PcCommand,
    state: &SharedState,
    power_pin_num: i32,
    now_us: u64,
) -> Vec<(u8, Vec<u8>)> {
    let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
    match cmd {
        PcCommand::Heartbeat => {
            let mut s = state.lock().unwrap();
            s.pc_alive = true;
            s.pc_count = s.pc_count.wrapping_add(1);
            s.pc_last_hb_us = now_us;
            drop(s);
            out.push((RPT_HEARTBEAT_ACK, Vec::new()));
        }
        PcCommand::GetState => {
            let snap = state.lock().unwrap().clone();
            out.push((RPT_STATE, make_state_payload(&snap).to_vec()));
        }
        PcCommand::RawKeyPress(key) => {
            state.lock().unwrap().key_override = Some(key);
        }
        PcCommand::RawKeyRelease => {
            state.lock().unwrap().key_release = true;
        }
        PcCommand::RawKnob(step) => {
            state.lock().unwrap().knob_inject = Some(step);
        }
        PcCommand::SetVol(pct) => {
            let mut s = state.lock().unwrap();
            if pct == 0xFF { s.vol_override = None; }
            else { s.vol_override = Some((20 + (pct as u32) * 940 / 100) as u16); s.vol_changed = true; }
        }
        PcCommand::SetSql(pct) => {
            let mut s = state.lock().unwrap();
            if pct == 0xFF {
                s.sql_override = None;
                s.sql_override_side_is_left = None;
            }
            else {
                let side_is_left = !s.right.is_main;
                s.sql_override = Some((20 + (pct as u32) * 980 / 100) as u16);
                s.sql_override_side_is_left = Some(side_is_left);
                s.sql_changed = true;
            }
        }
        PcCommand::SetPtt(on) => {
            let mut s = state.lock().unwrap();
            if on && s.rigctld_ptt_blocked_until_tx_real {
                s.ptt_override = false;
                log::warn!("[PTT保护] TX placeholder active, block PC PTT until real TX I applied");
            } else {
                s.ptt_override = on;
                if on { s.ptt_start_us = now_us; }
            }
        }
        PcCommand::PowerToggle => {
            log::info!("[PC通信] 收到开关机指令，GPIO{} 脉冲 1.2s", power_pin_num);
            unsafe { gpio_set_level(power_pin_num, 1); }
            std::thread::sleep(std::time::Duration::from_millis(1200));
            unsafe { gpio_set_level(power_pin_num, 0); }
            log::info!("[PC通信] GPIO{} 脉冲结束", power_pin_num);
        }
        PcCommand::WifiScan => {
            state.lock().unwrap().scan_request = true;
            log::info!("[PC通信] 收到 WiFi 扫描请求");
        }
        PcCommand::SetWifiCred { ssid, psk } => {
            log::info!("[PC通信] 收到 WiFi 配网：SSID=\"{}\" PSK={}字节", ssid.as_str(), psk.len());
            match write_wifi_creds_raw(ssid.as_str(), psk.as_str()) {
                Ok(()) => {
                    log::info!("[PC通信] WiFi 凭据已写入 NVS，1 秒后重启");
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                    unsafe { esp_restart(); }
                }
                Err(e) => {
                    log::error!("[PC通信] 写 NVS 失败: {}", e);
                    let msg = format!("WiFi NVS 写入失败: {}", e);
                    out.push((RPT_ERROR, msg.as_bytes().to_vec()));
                }
            }
        }
    }
    out
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
    SetWifiCred { ssid: heapless::String<32>, psk: heapless::String<64> },
    WifiScan,
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
            CMD_SET_WIFI_CRED if self.len >= 2 => {
                // [ssid_len:1][ssid][psk_len:1][psk]
                let plen = self.len as usize;
                let p = &self.payload[..plen];
                let slen = p[0] as usize;
                if slen > 32 || slen + 2 > plen { return None; }
                let psk_len_off = 1 + slen;
                let pl = p[psk_len_off] as usize;
                if pl > 64 || psk_len_off + 1 + pl > plen { return None; }
                let ssid_bytes = &p[1..1+slen];
                let psk_bytes  = &p[psk_len_off+1..psk_len_off+1+pl];
                let mut ssid: heapless::String<32> = heapless::String::new();
                let mut psk:  heapless::String<64> = heapless::String::new();
                let _ = ssid.push_str(core::str::from_utf8(ssid_bytes).ok()?);
                let _ = psk.push_str(core::str::from_utf8(psk_bytes).ok()?);
                Some(PcCommand::SetWifiCred { ssid, psk })
            }
            CMD_WIFI_SCAN => Some(PcCommand::WifiScan),
            _ => None,
        }
    }
}

pub fn pc_comm_thread(
    _uart_host: &esp_idf_svc::hal::uart::UartDriver<'_>,
    state: SharedState,
    power_pin_num: i32,
) {
    let mut parser = PcParser::new();
    let mut rx_buf = [0u8; 64];
    let init_us = unsafe { esp_timer_get_time() } as u64;
    let mut last_report_us = init_us;
    let mut last_pushed_scan_seq: u32 = 0;

    log::info!("[PC通信] TinyUSB CDC-ACM 线程启动");

    loop {
        let now_us = unsafe { esp_timer_get_time() } as u64;

        unsafe {
            if tusb_cdc_acm_initialized(TINYUSB_CDC_ACM_0) {
                let mut rx_size: usize = 0;
                if tinyusb_cdcacm_read(TINYUSB_CDC_ACM_0, rx_buf.as_mut_ptr(), rx_buf.len(), &mut rx_size) == ESP_OK && rx_size > 0 {
                    for i in 0..rx_size {
                        if let Some(cmd) = parser.feed(rx_buf[i]) {
                            // dispatch_command 内部已处理 Heartbeat 设置 pc_last_hb_us
                            let frames = dispatch_command(cmd, &state, power_pin_num, now_us);
                            for (typ, payload) in frames {
                                send_frame(typ, &payload);
                            }
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
        };
        if ptt_expired { send_error("PTT timeout"); }

        // 共享心跳超时检查（任意通道有心跳即视为 PC 在线）
        let pc_timed_out = {
            let s = state.lock().unwrap();
            s.pc_alive && (now_us.saturating_sub(s.pc_last_hb_us)) > PC_HEARTBEAT_TIMEOUT_US
        };
        if pc_timed_out {
            let mut s = state.lock().unwrap();
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

        // WiFi 扫描结果推送：检测 scan_seq 变化（仅当 PC 在线）
        let scan_to_send = {
            let s = state.lock().unwrap();
            if s.pc_alive && s.scan_seq != last_pushed_scan_seq && !s.scanning {
                last_pushed_scan_seq = s.scan_seq;
                Some(s.scan_results.clone())
            } else {
                None
            }
        };
        if let Some(items) = scan_to_send {
            log::info!("[PC通信] 推送 WiFi 扫描结果: {} 个 AP", items.len());
            send_wifi_scan_frame(&items);
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// handle_command 已被 dispatch_command 替代

/// 用 ESP-IDF 裸 NVS API 写入 SSID/PSK，namespace="wifi"
/// 与 wifi.rs 中 EspNvs::new 同 namespace；nvs_flash_init 已由 EspDefaultNvsPartition::take 完成
pub fn write_wifi_creds_raw(ssid: &str, psk: &str) -> Result<(), String> {
    use core::ffi::c_char;
    let ns = b"wifi\0";
    let key_ssid = b"ssid\0";
    let key_psk  = b"psk\0";

    let mut handle: nvs_handle_t = 0;
    unsafe {
        let r = nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READWRITE, &mut handle);
        if r != ESP_OK { return Err(format!("nvs_open={}", r)); }

        // 用零结尾的临时 buffer 写入（nvs_set_str 需要 NUL terminator）
        let mut ssid_buf = [0u8; 33];
        ssid_buf[..ssid.len()].copy_from_slice(ssid.as_bytes());
        let r = nvs_set_str(handle, key_ssid.as_ptr() as *const c_char, ssid_buf.as_ptr() as *const c_char);
        if r != ESP_OK { nvs_close(handle); return Err(format!("nvs_set_str(ssid)={}", r)); }

        let mut psk_buf = [0u8; 65];
        psk_buf[..psk.len()].copy_from_slice(psk.as_bytes());
        let r = nvs_set_str(handle, key_psk.as_ptr() as *const c_char, psk_buf.as_ptr() as *const c_char);
        if r != ESP_OK { nvs_close(handle); return Err(format!("nvs_set_str(psk)={}", r)); }

        let r = nvs_commit(handle);
        nvs_close(handle);
        if r != ESP_OK { return Err(format!("nvs_commit={}", r)); }
    }
    Ok(())
}
