// ===================================================================
// TCP 4533 — elfradio-box CRC16 协议（与 USB CDC 字节级一致）
//
// 设计：
//   - WiFi 连接成功后绑定 0.0.0.0:4533
//   - 每个 client 一个 handler 线程，最多 4 并发
//   - 复用 pc_comm.rs 的 PcParser + dispatch_command
//   - state.pc_alive 由共享心跳时间戳控制（USB / TCP 任一通道有心跳即在线）
// ===================================================================

use crate::pc_comm::{
    dispatch_command, encode_frame_vec, make_scan_payload, make_state_payload, PcParser,
    RPT_STATE, RPT_WIFI_SCAN, PC_HEARTBEAT_TIMEOUT_US,
};
use crate::state::{SharedState, WifiState};
use esp_idf_svc::sys::esp_timer_get_time;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const PORT: u16 = 4533;
const MAX_CLIENTS: usize = 4;

pub fn start_pc_tcp_thread(state: SharedState, power_pin_num: i32) {
    std::thread::Builder::new()
        .name("pc_tcp_acceptor".into())
        .stack_size(4096)
        .spawn(move || acceptor_main(state, power_pin_num))
        .expect("pc_tcp 线程启动失败");
}

fn acceptor_main(state: SharedState, power_pin_num: i32) {
    let active = Arc::new(AtomicUsize::new(0));
    log::info!("[PC-TCP] acceptor 启动");

    loop {
        let connected = {
            let s = state.lock().unwrap();
            s.wifi_state == WifiState::Connected
        };
        if !connected {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }

        let listener = match TcpListener::bind(("0.0.0.0", PORT)) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("[PC-TCP] bind 0.0.0.0:{} 失败: {}，5s 后重试", PORT, e);
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };
        log::info!("[PC-TCP] 监听 0.0.0.0:{}", PORT);
        listener.set_nonblocking(false).ok();

        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    let cur = active.load(Ordering::SeqCst);
                    if cur >= MAX_CLIENTS {
                        log::warn!("[PC-TCP] 拒绝 {}：已达最大并发数 {}", peer, MAX_CLIENTS);
                        drop(stream);
                        continue;
                    }
                    active.fetch_add(1, Ordering::SeqCst);
                    let st = state.clone();
                    let act = active.clone();
                    log::info!("[PC-TCP] 接受连接：{}（活跃 {}）", peer, cur + 1);
                    std::thread::Builder::new()
                        .name(format!("pc_tcp_{}", peer.port()))
                        .stack_size(8192)
                        .spawn(move || {
                            handle_client(stream, st, power_pin_num);
                            act.fetch_sub(1, Ordering::SeqCst);
                            log::info!("[PC-TCP] 连接 {} 已关闭", peer);
                        })
                        .ok();
                }
                Err(e) => {
                    // WiFi 断开会导致 accept 失败
                    let still = { state.lock().unwrap().wifi_state == WifiState::Connected };
                    if !still {
                        log::info!("[PC-TCP] WiFi 断开，重新初始化 listener");
                        break;
                    }
                    log::warn!("[PC-TCP] accept 错误: {}", e);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    }
}

fn handle_client(mut stream: TcpStream, state: SharedState, power_pin_num: i32) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(50)));
    let _ = stream.set_nodelay(true);

    let mut parser = PcParser::new();
    let mut last_report_us: u64 = 0;
    let mut last_pushed_scan_seq: u32 = 0;
    let mut buf = [0u8; 256];

    // 进入会话后，给每个 client 一个独立的"本通道心跳"时间戳。
    // 6 秒无任何字节 → 断开（TCP 比 USB 容忍度高一倍）
    let mut last_byte_us = unsafe { esp_timer_get_time() } as u64;

    loop {
        let now_us = unsafe { esp_timer_get_time() } as u64;

        // --- 读字节 ---
        match stream.read(&mut buf) {
            Ok(0) => {
                log::info!("[PC-TCP] client 主动关闭");
                return;
            }
            Ok(n) => {
                last_byte_us = now_us;
                for &b in &buf[..n] {
                    if let Some(cmd) = parser.feed(b) {
                        let frames = dispatch_command(cmd, &state, power_pin_num, now_us);
                        for (typ, payload) in frames {
                            let frame = encode_frame_vec(typ, &payload);
                            if stream.write_all(&frame).is_err() {
                                log::info!("[PC-TCP] 写失败，断开");
                                return;
                            }
                        }
                        let _ = stream.flush();
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut
                       || e.kind() == std::io::ErrorKind::WouldBlock => {
                // 正常超时，继续走周期性推送
            }
            Err(e) => {
                log::info!("[PC-TCP] 读错误: {}", e);
                return;
            }
        }

        // --- TCP 通道层心跳超时（6 秒）---
        if now_us.saturating_sub(last_byte_us) > 2 * PC_HEARTBEAT_TIMEOUT_US {
            log::info!("[PC-TCP] 6s 无数据，断开");
            return;
        }

        // --- 周期性 STATE_REPORT（200ms，仅在 PC 在线时）---
        let maybe_state = {
            let s = state.lock().unwrap();
            if s.pc_alive && now_us.saturating_sub(last_report_us) > 200_000 {
                Some(make_state_payload(&s))
            } else {
                None
            }
        };
        if let Some(p) = maybe_state {
            let frame = encode_frame_vec(RPT_STATE, &p);
            if stream.write_all(&frame).is_err() { return; }
            let _ = stream.flush();
            last_report_us = now_us;
        }

        // --- WiFi 扫描结果推送 ---
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
            let payload = make_scan_payload(&items);
            let frame = encode_frame_vec(RPT_WIFI_SCAN, &payload);
            if stream.write_all(&frame).is_err() { return; }
            let _ = stream.flush();
        }
    }
}
