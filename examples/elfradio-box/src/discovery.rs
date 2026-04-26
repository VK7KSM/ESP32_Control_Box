// ===================================================================
// LAN 设备发现 — UDP 4534 客户端
//
// 协议：
//   - 广播 "ELFRADIO?" 到 255.255.255.255:4534
//   - 收 "ELFRADIO! <serial> <version> <rigtype>" 应答
// ===================================================================

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const PORT: u16 = 4534;
const MAGIC_REQ: &[u8] = b"ELFRADIO?";
const MAGIC_REPLY_PREFIX: &[u8] = b"ELFRADIO!";

#[derive(Debug, Clone)]
pub struct Discovered {
    pub ip:      String,    // "192.168.1.42"
    pub serial:  String,    // "76F824"
    pub version: String,    // "0.1.0"
    pub rigtype: String,    // "TH-9800"
}

/// 在 LAN 上扫描控制盒，最多等 wait_ms 毫秒，返回所有发现的设备
pub fn scan(wait_ms: u64) -> std::io::Result<Vec<Discovered>> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;

    let dest = SocketAddrV4::new(Ipv4Addr::BROADCAST, PORT);
    socket.send_to(MAGIC_REQ, dest)?;

    let mut found: Vec<Discovered> = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let mut buf = [0u8; 256];

    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, src)) => {
                let data = &buf[..n];
                if data.len() < MAGIC_REPLY_PREFIX.len()
                    || &data[..MAGIC_REPLY_PREFIX.len()] != MAGIC_REPLY_PREFIX {
                    continue;
                }
                let text = match std::str::from_utf8(data) {
                    Ok(s) => s.trim(),
                    Err(_) => continue,
                };
                // "ELFRADIO! <serial> <version> <rigtype>"
                let parts: Vec<&str> = text.splitn(4, ' ').collect();
                if parts.len() < 4 { continue; }
                let ip_str = match src.ip() {
                    std::net::IpAddr::V4(v4) => v4.to_string(),
                    std::net::IpAddr::V6(v6) => v6.to_string(),
                };
                let dev = Discovered {
                    ip:      ip_str,
                    serial:  parts[1].to_string(),
                    version: parts[2].to_string(),
                    rigtype: parts[3].to_string(),
                };
                // 去重（按 serial）
                if !found.iter().any(|d| d.serial == dev.serial) {
                    found.push(dev);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut
                      || e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(found)
}
