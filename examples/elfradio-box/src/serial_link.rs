// ===================================================================
// 通信链路抽象层 (Transport)
//   - Usb(SerialPort) — 现有 USB CDC-ACM 通道
//   - Tcp(TcpStream)  — LAN 上的 ESP32（端口 4533，CRC16 协议字节级一致）
//
// RX/TX 线程对 Transport 操作，业务代码不感知底层介质。
// ===================================================================

use crate::protocol::{self, ParseEvent, FrameParser};
use crate::state::{self, SharedState};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// 通信链路：USB 串口或 TCP socket
pub enum Transport {
    Serial(Box<dyn serialport::SerialPort>),
    Tcp(TcpStream),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Serial(p) => p.read(buf),
            Transport::Tcp(s)    => s.read(buf),
        }
    }
}
impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Serial(p) => p.write(buf),
            Transport::Tcp(s)    => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Serial(p) => p.flush(),
            Transport::Tcp(s)    => s.flush(),
        }
    }
}

pub type SharedPort = Arc<Mutex<Transport>>;

/// 选路目标
#[derive(Debug, Clone)]
pub enum TransportTarget {
    Usb(String),       // COM 端口
    Lan(String),       // IP 地址（"192.168.1.42"）
}

impl std::fmt::Display for TransportTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportTarget::Usb(p) => write!(f, "USB:{}", p),
            TransportTarget::Lan(ip) => write!(f, "LAN:{}", ip),
        }
    }
}

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

/// 自动检测 USB 串口：仅 Espressif OTG（VID=0x303A, PID≠0x1001）
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

/// 自动选路：USB 优先，找不到则 LAN UDP 扫描
/// 返回首个可用 target，都没有则 None
pub fn auto_detect_any() -> Option<TransportTarget> {
    if let Some(name) = auto_detect_port() {
        return Some(TransportTarget::Usb(name));
    }
    // LAN 扫描（UDP 4534）
    match crate::discovery::scan(2000) {
        Ok(devs) if !devs.is_empty() => Some(TransportTarget::Lan(devs[0].ip.clone())),
        _ => None,
    }
}

/// 打开 USB 串口
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

    Ok(Arc::new(Mutex::new(Transport::Serial(port))))
}

/// 打开 TCP 4533 连接到 ESP32
pub fn open_tcp(ip: &str) -> Result<SharedPort, String> {
    use std::net::SocketAddr;
    let addr: SocketAddr = format!("{}:4533", ip).parse()
        .map_err(|e| format!("IP 格式错误: {}", e))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| format!("连接 {} 失败: {}", addr, e))?;
    stream.set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(|e| format!("set_read_timeout: {}", e))?;
    let _ = stream.set_nodelay(true);
    Ok(Arc::new(Mutex::new(Transport::Tcp(stream))))
}

/// 按 target 打开链路
pub fn open_target(t: &TransportTarget) -> Result<SharedPort, String> {
    match t {
        TransportTarget::Usb(name) => open_port(name),
        TransportTarget::Lan(ip)   => open_tcp(ip),
    }
}

/// 一次性同步操作：打开 Transport 直接拥有（不进 Arc<Mutex>），用于
/// initial_state_check / WiFi 配网 / WiFi 扫描等单线程过程
pub fn open_oneshot(t: &TransportTarget, read_timeout_ms: u64) -> Result<Transport, String> {
    match t {
        TransportTarget::Usb(name) => {
            let mut port = serialport::new(name, 115200)
                .timeout(Duration::from_millis(read_timeout_ms))
                .open().map_err(|e| format!("打开 {} 失败: {}", name, e))?;
            port.write_data_terminal_ready(false)
                .map_err(|e| format!("DTR: {}", e))?;
            port.write_request_to_send(false)
                .map_err(|e| format!("RTS: {}", e))?;
            Ok(Transport::Serial(port))
        }
        TransportTarget::Lan(ip) => {
            use std::net::SocketAddr;
            let addr: SocketAddr = format!("{}:4533", ip).parse()
                .map_err(|e| format!("IP 格式: {}", e))?;
            let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
                .map_err(|e| format!("连接 {} 失败: {}", addr, e))?;
            stream.set_read_timeout(Some(Duration::from_millis(read_timeout_ms)))
                .map_err(|e| format!("set_read_timeout: {}", e))?;
            let _ = stream.set_nodelay(true);
            Ok(Transport::Tcp(stream))
        }
    }
}

/// 写入帧（加锁 + flush）
pub fn send_frame(port: &SharedPort, frame: &[u8]) {
    if let Ok(mut p) = port.lock() {
        let _ = p.write_all(frame);
        let _ = p.flush();
    }
}

/// RX 线程
pub fn spawn_rx_thread(
    port: SharedPort,
    shared: SharedState,
    event_tx: mpsc::Sender<SerialEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("link_rx".into())
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
                        Ok(0) => {
                            // TCP 端发 FIN 关闭
                            let _ = event_tx.send(SerialEvent::Disconnected);
                            break;
                        }
                        Ok(n) => n,
                        Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut
                                   || e.kind() == std::io::ErrorKind::WouldBlock => 0,
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
        .expect("Link RX 线程启动失败")
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
        .name("link_tx".into())
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
        .expect("Link TX 线程启动失败")
}
