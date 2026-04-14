// ===================================================================
// 串口连接管理 + RX/TX 线程
//
// 使用 Arc<Mutex<>> 共享串口（Windows USB CDC 的 try_clone 写入可能不可靠）
// ===================================================================

use crate::protocol::{self, ParseEvent, FrameParser};
use crate::state::{self, SharedState};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

pub type SharedPort = Arc<Mutex<Box<dyn serialport::SerialPort>>>;

/// 串口事件（RX 线程 → 主线程）
pub enum SerialEvent {
    StateUpdated,
    LogLine(String),
    MacroDone(u8),
    Error(String),
    Disconnected,
}

/// 枚举可用串口
pub fn list_ports() -> Vec<(String, String)> {
    match serialport::available_ports() {
        Ok(ports) => {
            ports.into_iter().map(|p| {
                let desc = match &p.port_type {
                    serialport::SerialPortType::UsbPort(usb) => {
                        format!("USB VID:{:04X} PID:{:04X} {}",
                            usb.vid, usb.pid, usb.product.as_deref().unwrap_or(""))
                    }
                    _ => "Serial".to_string(),
                };
                (p.port_name, desc)
            }).collect()
        }
        Err(_) => Vec::new(),
    }
}

/// 自动检测串口：只返回 Espressif OTG CDC-ACM 口
/// 排除 USB JTAG/Serial debug 口（VID=0x303A PID=0x1001，打开会触发 ESP32 复位）
/// 不做单一串口 fallback，避免误连 CH343 等非 ESP32 设备
pub fn auto_detect_port() -> Option<String> {
    if let Ok(ports) = serialport::available_ports() {
        for p in &ports {
            if let serialport::SerialPortType::UsbPort(usb) = &p.port_type {
                if usb.vid == 0x303A && usb.pid != 0x1001 {
                    return Some(p.port_name.clone());
                }
            }
        }
    }
    None
}

/// 打开串口，返回 Arc<Mutex> 共享句柄
pub fn open_port(name: &str) -> Result<SharedPort, String> {
    let mut port = serialport::new(name, 115200)
        .timeout(Duration::from_millis(50))
        .open()
        .map_err(|e| format!("打开 {} 失败: {}", name, e))?;

    if let Err(e) = port.write_data_terminal_ready(false) {
        return Err(format!("设置 DTR 失败: {}", e));
    }
    if let Err(e) = port.write_request_to_send(false) {
        return Err(format!("设置 RTS 失败: {}", e));
    }

    Ok(Arc::new(Mutex::new(port)))
}

/// 向串口写帧（加锁 + flush）
pub fn send_frame(port: &SharedPort, frame: &[u8]) {
    if let Ok(mut p) = port.lock() {
        let _ = p.write_all(frame);
        let _ = p.flush();
    }
}

/// RX 线程：读串口 → 解析帧 → 更新状态
pub fn spawn_rx_thread(
    port: SharedPort,
    shared: SharedState,
    event_tx: mpsc::Sender<SerialEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("serial_rx".into())
        .spawn(move || {
            let mut parser = FrameParser::new();
            let mut buf = [0u8; 256];

            loop {
                let n = {
                    let mut p = match port.lock() {
                        Ok(p) => p,
                        Err(_) => { std::thread::sleep(Duration::from_millis(10)); continue; }
                    };
                    match p.read(&mut buf) {
                        Ok(n) => n,
                        Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
                        Err(_) => {
                            let _ = event_tx.send(SerialEvent::Disconnected);
                            break;
                        }
                    }
                };

                for i in 0..n {
                    if let Some(evt) = parser.feed(buf[i]) {
                        match evt {
                            ParseEvent::Frame { typ, payload } => {
                                if !handle_frame(typ, &payload, &shared, &event_tx) { return; }
                            }
                            ParseEvent::LogLine(line) => {
                                if event_tx.send(SerialEvent::LogLine(line)).is_err() { return; }
                            }
                        }
                    }
                }

                if n == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        })
        .expect("Serial RX 线程启动失败")
}

fn handle_frame(typ: u8, payload: &[u8], shared: &SharedState, event_tx: &mpsc::Sender<SerialEvent>) -> bool {
    match typ {
        protocol::RPT_HEARTBEAT_ACK => {}
        protocol::RPT_STATE_REPORT => {
            if let Some(new_state) = state::decode_state_report(payload) {
                let mut s = shared.lock().unwrap();
                *s = new_state;
                drop(s);
                if event_tx.send(SerialEvent::StateUpdated).is_err() { return false; }
            }
        }
        protocol::RPT_ERROR => {
            let msg = String::from_utf8_lossy(payload).to_string();
            if event_tx.send(SerialEvent::Error(msg)).is_err() { return false; }
        }
        _ => {}
    }
    true
}

/// TX 线程：心跳 + 命令发送
pub fn spawn_tx_thread(
    port: SharedPort,
    cmd_rx: mpsc::Receiver<Vec<u8>>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("serial_tx".into())
        .spawn(move || {
            let heartbeat = protocol::encode_frame(protocol::CMD_HEARTBEAT, &[]);
            let mut last_hb = std::time::Instant::now();

            loop {
                match cmd_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(frame) => {
                        send_frame(&port, &frame);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }

                if last_hb.elapsed() >= Duration::from_millis(500) {
                    send_frame(&port, &heartbeat);
                    last_hb = std::time::Instant::now();
                }
            }
        })
        .expect("Serial TX 线程启动失败")
}
