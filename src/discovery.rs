// ===================================================================
// LAN 设备发现 — UDP 4534
//
// 协议：
//   - PC 上位机广播 "ELFRADIO?" 到 255.255.255.255:4534
//   - ESP32 收到后回 "ELFRADIO! <serial> <version> <rigtype>" 给请求源 IP
//
// serial   = efuse MAC 后 6 hex（如 "76F824"）
// version  = "0.1.0"
// rigtype  = "TH-9800"
//
// 仅在 WiFi 已连接时绑定 socket；WiFi 断开则关闭 socket，重连后再绑定。
// ===================================================================

use crate::state::{SharedState, WifiState};
use esp_idf_svc::sys::*;
use std::net::UdpSocket;

const PORT: u16 = 4534;
const VERSION: &str = "0.1.0";
const RIGTYPE: &str = "TH-9800";
const MAGIC_REQ: &[u8] = b"ELFRADIO?";

pub fn start_discovery_thread(state: SharedState) {
    std::thread::Builder::new()
        .name("discovery".into())
        .stack_size(4096)
        .spawn(move || discovery_main(state))
        .expect("discovery 线程启动失败");
}

fn read_serial() -> String {
    let mut mac: [u8; 8] = [0; 8];
    unsafe {
        esp_efuse_mac_get_default(mac.as_mut_ptr());
    }
    format!("{:02X}{:02X}{:02X}", mac[3], mac[4], mac[5])
}

fn discovery_main(state: SharedState) {
    let serial = read_serial();
    let reply = format!("ELFRADIO! {} {} {}", serial, VERSION, RIGTYPE);
    log::info!("[Discovery] 启动，serial={} version={} rigtype={}", serial, VERSION, RIGTYPE);

    loop {
        // 等到 WiFi 连上
        let connected = {
            let s = state.lock().unwrap();
            s.wifi_state == WifiState::Connected
        };
        if !connected {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }

        // 绑定 0.0.0.0:4534
        let socket = match UdpSocket::bind(("0.0.0.0", PORT)) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("[Discovery] bind 0.0.0.0:{} 失败: {}，5s 后重试", PORT, e);
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };
        // set_broadcast 接收方不强制需要，但保留
        let _ = socket.set_broadcast(true);
        // 设置 1s 读超时，便于检查 WiFi 状态
        let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(1)));
        log::info!("[Discovery] 监听 UDP 0.0.0.0:{}", PORT);

        let mut buf = [0u8; 64];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, src)) => {
                    if &buf[..n.min(MAGIC_REQ.len())] == MAGIC_REQ {
                        log::info!("[Discovery] 收到 ELFRADIO? 来自 {}", src);
                        if let Err(e) = socket.send_to(reply.as_bytes(), src) {
                            log::warn!("[Discovery] 回复失败: {}", e);
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut
                          || e.kind() == std::io::ErrorKind::WouldBlock => {
                    // 检查 WiFi 是否还连着
                    let still = {
                        let s = state.lock().unwrap();
                        s.wifi_state == WifiState::Connected
                    };
                    if !still {
                        log::info!("[Discovery] WiFi 断开，关闭 socket");
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("[Discovery] recv 错误: {}，重新绑定", e);
                    break;
                }
            }
        }
        // socket drop → 关闭，回到外层 loop 等 WiFi 重连
    }
}
