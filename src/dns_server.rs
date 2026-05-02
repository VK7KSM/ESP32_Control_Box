// ===================================================================
// SoftAP Captive Portal DNS 劫持（C 功能 SoftAP 一部分）
//
// 监听 UDP 端口 53，所有 DNS 查询返回 IPv4 应答指向 192.168.4.1。
// 让手机连上 SoftAP 后系统检测到无互联网（任意域名都返回 192.168.4.1）→
// 弹"登录此 WiFi"通知 → 自动打开浏览器 → 直接看到配网页（无需手动输入 IP）。
//
// 这是机场/酒店/路由器配网的行业标准做法（Captive Portal）。
// 实现简化：仅处理 type=A 的查询，其他类型也返回 A=192.168.4.1（手机普遍只查 A）。
// ===================================================================

use std::net::UdpSocket;
use std::time::Duration;

const DNS_PORT: u16 = 53;
const SOFTAP_IP: [u8; 4] = [192, 168, 4, 1];
const MAX_DNS_PACKET: usize = 512;

/// 启动 DNS server 后台线程
pub fn start_dns_server() {
    let _ = esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
        pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core1),
        ..Default::default()
    }.set();
    // 栈 8192：lwip UdpSocket::bind/recv_from/send_to 调用栈深 ~2KB，
    // 加 build_dns_response Vec 分配 + Rust runtime + log 输出，4096 边界不安全
    std::thread::Builder::new()
        .name("dns_hijack".into())
        .stack_size(8192)
        .spawn(|| dns_main())
        .expect("dns_server 线程启动失败");
}

fn dns_main() {
    let socket = match UdpSocket::bind(("0.0.0.0", DNS_PORT)) {
        Ok(s) => s,
        Err(e) => {
            ::log::error!("[DNS] bind 0.0.0.0:53 失败: {:?}", e);
            return;
        }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_secs(60)));

    ::log::info!("[DNS] Captive Portal DNS server 已启动 (UDP 53 → 192.168.4.1)");

    let mut buf = [0u8; MAX_DNS_PACKET];
    let mut query_count: u32 = 0;
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, peer)) => {
                if len < 12 {
                    continue; // DNS header 至少 12 字节
                }
                query_count = query_count.wrapping_add(1);
                // 诊断：首次 + 每 10 个查询 log 一次（确认 DNS hijack 真的被触发）
                if query_count == 1 || query_count % 10 == 0 {
                    ::log::info!("[DNS] 收到第 {} 个查询 from {} ({} bytes) → 返回 192.168.4.1",
                        query_count, peer, len);
                }
                let response = build_dns_response(&buf[..len]);
                if !response.is_empty() {
                    let _ = socket.send_to(&response, peer);
                }
            }
            Err(_e) => {
                // 超时或临时错误，继续
            }
        }
    }
}

/// 构造 DNS 响应包
/// 输入是客户端 query 包；输出是 answer 包（对所有 type=A/AAAA 等查询返回 A=192.168.4.1）
fn build_dns_response(query: &[u8]) -> Vec<u8> {
    if query.len() < 12 {
        return Vec::new();
    }

    let mut resp: Vec<u8> = Vec::with_capacity(query.len() + 16);

    // ===== Header（12 字节） =====
    // ID（保留 query 的 ID）
    resp.push(query[0]);
    resp.push(query[1]);
    // Flags: QR=1（response）, OP=0, AA=1, TC=0, RD=保留, RA=1, Z=0, RCODE=0
    // = 0b1000_0101_1000_0000 = 0x8580
    resp.push(0x85);
    resp.push(0x80);
    // QDCOUNT（保留）
    resp.push(query[4]);
    resp.push(query[5]);
    // ANCOUNT = 1
    resp.push(0x00);
    resp.push(0x01);
    // NSCOUNT = 0
    resp.push(0x00);
    resp.push(0x00);
    // ARCOUNT = 0
    resp.push(0x00);
    resp.push(0x00);

    // ===== Question section（拷贝原 query 的 question） =====
    // 找到 question 末尾（query 中 QNAME 以 0x00 结尾，后跟 QTYPE 2B + QCLASS 2B）
    let mut idx = 12;
    while idx < query.len() && query[idx] != 0 {
        let label_len = query[idx] as usize;
        // 防御指针压缩 / 越界
        if label_len & 0xC0 != 0 {
            return Vec::new();
        }
        idx += 1 + label_len;
        if idx >= query.len() {
            return Vec::new();
        }
    }
    if idx >= query.len() {
        return Vec::new();
    }
    let qname_end = idx; // 0x00 位置
    let question_end = qname_end + 1 + 4; // +1 for 0x00, +4 for QTYPE+QCLASS
    if question_end > query.len() {
        return Vec::new();
    }
    resp.extend_from_slice(&query[12..question_end]);

    // ===== Answer section =====
    // NAME: 指针压缩指向 question 中的 QNAME（offset=12 = 0xC00C）
    resp.push(0xC0);
    resp.push(0x0C);
    // TYPE = A (1)
    resp.push(0x00);
    resp.push(0x01);
    // CLASS = IN (1)
    resp.push(0x00);
    resp.push(0x01);
    // TTL = 60 秒
    resp.push(0x00);
    resp.push(0x00);
    resp.push(0x00);
    resp.push(0x3C);
    // RDLENGTH = 4
    resp.push(0x00);
    resp.push(0x04);
    // RDATA = 192.168.4.1
    resp.extend_from_slice(&SOFTAP_IP);

    resp
}
