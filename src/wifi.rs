// ===================================================================
// WiFi STA 客户端 + 扫描器
//   - 启动后立即 wifi.start()，关闭省电模式（避免与路由器 4-way handshake 冲突）
//   - 立即做一次扫描，结果推到 SharedState（PC 配网时按编号选择）
//   - NVS 有凭据 → 连接，断开自动重连
//   - 无凭据 → 待机，每 30s 自动 re-scan，PC 请求时按需 scan
//   - PC 通过 SharedState.scan_request 触发扫描；扫描完 scan_seq++ 由 pc_comm 推送
// ===================================================================

use crate::nvs_cfg;
use crate::state::{SharedState, WifiAp, WifiState};
use core::fmt::Write as _;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys::*;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi,
};

const NVS_NS: &str = "wifi";

/// SoftAP 模式参数（main.rs 启动判定后传给 wifi 线程）
#[derive(Clone, Copy)]
pub struct SoftApMode {
    pub enabled: bool,        // true = 进 SoftAP；false = 走 STA
    pub no_credentials: bool, // true = 开箱无凭据触发（不超时）；false = 双击触发（10min 超时由 softap.rs 管）
}

fn read_creds(nvs_part: EspDefaultNvsPartition) -> Option<(heapless::String<32>, heapless::String<64>)> {
    let nvs: EspNvs<NvsDefault> = match EspNvs::new(nvs_part, NVS_NS, true) {
        Ok(n) => n,
        Err(e) => {
            ::log::warn!("[WiFi] 打开 NVS namespace 失败: {:?}", e);
            return None;
        }
    };
    let mut ssid_buf = [0u8; 33];
    let mut psk_buf = [0u8; 65];
    let ssid_str = nvs.get_str("ssid", &mut ssid_buf).ok().flatten()?;
    let psk_str  = nvs.get_str("psk",  &mut psk_buf).ok().flatten().unwrap_or("");
    let mut ssid: heapless::String<32> = heapless::String::new();
    let mut psk:  heapless::String<64> = heapless::String::new();
    let _ = ssid.push_str(ssid_str);
    let _ = psk.push_str(psk_str);
    if ssid.is_empty() { return None; }
    Some((ssid, psk))
}

pub fn start_wifi_thread(modem: Modem, state: SharedState, softap: SoftApMode) {
    // 绑到 CPU 1，与 ESP-IDF wifi task 同核；释放 CPU 0 给 UART/LCD/IDLE0
    let _ = esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
        pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core1),
        ..Default::default()
    }.set();
    std::thread::Builder::new()
        .name("wifi".into())
        .stack_size(8192)
        .spawn(move || wifi_main(modem, state, softap))
        .expect("wifi 线程启动失败");
}

fn auth_to_u8(am: AuthMethod) -> u8 {
    match am {
        AuthMethod::None             => 0,
        AuthMethod::WEP              => 1,
        AuthMethod::WPA              => 2,
        AuthMethod::WPA2Personal     => 3,
        AuthMethod::WPAWPA2Personal  => 4,
        AuthMethod::WPA2Enterprise   => 5,
        AuthMethod::WPA3Personal     => 6,
        AuthMethod::WPA2WPA3Personal => 7,
        _ => 255,
    }
}

fn do_scan(wifi: &mut BlockingWifi<EspWifi<'static>>, state: &SharedState) {
    ::log::info!("[WiFi] 开始扫描...");
    { let mut s = state.lock().unwrap(); s.scanning = true; }
    match wifi.scan() {
        Ok(aps) => {
            let mut s = state.lock().unwrap();
            s.scan_results.clear();
            for ap in aps.iter().take(16) {
                let mut ssid: heapless::String<32> = heapless::String::new();
                let _ = ssid.push_str(ap.ssid.as_str());
                let item = WifiAp {
                    ssid,
                    rssi: ap.signal_strength,
                    auth: auth_to_u8(ap.auth_method.unwrap_or(AuthMethod::None)),
                };
                let _ = s.scan_results.push(item);
            }
            s.scan_seq = s.scan_seq.wrapping_add(1);
            s.scanning = false;
            s.scan_request = false;
            ::log::info!("[WiFi] 扫描完成，找到 {} 个 AP", s.scan_results.len());
        }
        Err(e) => {
            ::log::warn!("[WiFi] 扫描失败: {:?}", e);
            let mut s = state.lock().unwrap();
            s.scanning = false;
            s.scan_request = false;
        }
    }
}

fn mark(state: &SharedState, ws: WifiState, ip: &str) {
    if let Ok(mut s) = state.lock() {
        s.wifi_state = ws;
        s.wifi_ip.clear();
        let _ = s.wifi_ip.push_str(ip);
    }
}

fn wifi_main(modem: Modem, state: SharedState, softap: SoftApMode) {
    // STA 模式延迟 3 秒让 BLE controller 先 init 抢内部 SRAM（~30KB）。WiFi init 也吃约 16KB
    // RESERVE_INTERNAL 内存，必须 BLE 在前 WiFi 在后；之前是反过来导致 BLE_INIT: Malloc failed
    // SoftAP 模式 BLE 不启动（main.rs 已 gate），无需等待，立即启动
    if !softap.enabled {
        ::log::info!("[WiFi] 等待 3 秒让 BLE controller 先初始化完成...");
        std::thread::sleep(std::time::Duration::from_secs(3));
    } else {
        ::log::info!("[WiFi] SoftAP 模式（no_credentials={}），跳过 BLE 等待", softap.no_credentials);
    }

    let sysloop = match EspSystemEventLoop::take() {
        Ok(s) => s,
        Err(e) => { ::log::error!("[WiFi] sysloop 失败: {:?}", e); mark(&state, WifiState::Failed, ""); return; }
    };
    let nvs_part = match EspDefaultNvsPartition::take() {
        Ok(n) => n,
        Err(e) => { ::log::error!("[WiFi] NVS 失败: {:?}", e); mark(&state, WifiState::Failed, ""); return; }
    };

    let esp_wifi = match EspWifi::new(modem, sysloop.clone(), Some(nvs_part.clone())) {
        Ok(w) => w,
        Err(e) => { ::log::error!("[WiFi] EspWifi::new 失败: {:?}", e); mark(&state, WifiState::Failed, ""); return; }
    };
    let mut wifi = match BlockingWifi::wrap(esp_wifi, sysloop) {
        Ok(w) => w,
        Err(e) => { ::log::error!("[WiFi] BlockingWifi::wrap 失败: {:?}", e); mark(&state, WifiState::Failed, ""); return; }
    };

    if softap.enabled {
        // ===== SoftAP 模式 =====
        ap_main(&mut wifi, &state, softap);
        // ap_main 不返回（外部 esp_restart() 退出）
        return;
    }

    // ===== STA 模式（保留原有完整路径）=====
    // 必须先 set_configuration 一个空 STA 配置，否则 start() 后 scan 会失败
    if let Err(e) = wifi.set_configuration(&Configuration::Client(ClientConfiguration::default())) {
        ::log::error!("[WiFi] 初始 set_configuration 失败: {:?}", e);
        mark(&state, WifiState::Failed, "");
        return;
    }
    if let Err(e) = wifi.start() {
        ::log::error!("[WiFi] start 失败: {:?}", e);
        mark(&state, WifiState::Failed, "");
        return;
    }

    // 关闭 WiFi 省电模式（默认 WIFI_PS_MIN_MODEM 与某些路由器在 4-way handshake 时冲突）
    unsafe {
        let r = esp_wifi_set_ps(wifi_ps_type_t_WIFI_PS_NONE);
        if r == ESP_OK {
            ::log::info!("[WiFi] 已关闭省电模式 (WIFI_PS_NONE)");
        } else {
            ::log::warn!("[WiFi] esp_wifi_set_ps 返回 {}", r);
        }
    }

    ::log::info!("[WiFi] 已启动 STA 模式");

    // 启动后不主动扫描，避免无上位机时 WiFi scan 抢占 UART 中继；等待 PC 显式请求

    // 主循环：根据 NVS 凭据决定行为
    loop {
        let creds = read_creds(nvs_part.clone());
        match creds {
            Some((ssid, psk)) => {
                // 有凭据 → 尝试连接
                connect_loop(&mut wifi, &state, &ssid, &psk);
                // connect_loop 仅在凭据丢失/被清除时返回；正常工作不返回
            }
            None => {
                // 无凭据 → 进入扫描+待机循环，等 PC 写入凭据后重启或本地重检
                idle_loop(&mut wifi, &state);
            }
        }
    }
}

/// SoftAP 主流程
/// SSID = `elfRadio-XXXX`（XXXX = MAC 后 4 位 hex 大写）
/// 无密码（auth=Open，channel=1，max_connections=4）
/// IP 默认 192.168.4.1（ESP-IDF AP netif 默认配置）
fn ap_main(wifi: &mut BlockingWifi<EspWifi<'static>>, state: &SharedState, softap: SoftApMode) {
    // 构造 SSID：elfRadio-XXXX
    let suffix = nvs_cfg::ssid_suffix();
    let mut ssid: heapless::String<32> = heapless::String::new();
    let _ = ssid.push_str("elfRadio-");
    let _ = ssid.push_str(suffix.as_str());
    ::log::info!("[WiFi-AP] 配置 SoftAP：SSID=\"{}\" 无密码 channel=1", ssid.as_str());

    // 用 default() 拿到合理 protocols（ESP-IDF 期望至少一组 802.11 协议），再覆盖关键字段
    let mut ap_cfg = AccessPointConfiguration::default();
    ap_cfg.ssid = ssid.clone();
    ap_cfg.ssid_hidden = false;
    ap_cfg.channel = 1;
    ap_cfg.auth_method = AuthMethod::None;
    ap_cfg.password = heapless::String::new();
    ap_cfg.max_connections = 4;

    // === 关键：APSTA (Mixed) 模式而非纯 AP 模式 ===
    // esp_wifi_scan_start 仅在 STA / APSTA 模式下工作；纯 AP 模式调用返回 -1 (ESP_FAIL)
    // 用空 ClientConfiguration（SSID 空）让 STA 子系统启用但不自动连任何 AP
    // 这样 softap.rs::run_wifi_scan_inline 能正常调 esp_wifi_scan_start
    // 代价：STA 子系统多占 ~5-8KB SRAM；AP 模式可用余量 ~118KB → APSTA 仍 ~108KB 充足
    let client_cfg = ClientConfiguration::default();
    let cfg = Configuration::Mixed(client_cfg, ap_cfg);

    if let Err(e) = wifi.set_configuration(&cfg) {
        ::log::error!("[WiFi-AP] set_configuration(Mixed) 失败: {:?}", e);
        mark(state, WifiState::Failed, "");
        return;
    }
    if let Err(e) = wifi.start() {
        ::log::error!("[WiFi-AP] start 失败: {:?}", e);
        mark(state, WifiState::Failed, "");
        return;
    }

    // === 修 1：强制 AP IP = 192.168.4.1（ESP-IDF v5.x 默认 192.168.71.1，与 v4 计划期望不符） ===
    // === 修 + ：DHCP Option 114 (RFC 8910) captive portal URI，让 Android 11+/iOS 14+ 自动弹登录页 ===
    // 标准做法：dhcps_stop → set_ip_info → dhcps_option(114) → dhcps_start
    unsafe {
        let key = b"WIFI_AP_DEF\0";
        let netif = esp_netif_get_handle_from_ifkey(key.as_ptr() as *const _);
        if netif.is_null() {
            ::log::warn!("[WiFi-AP] esp_netif_get_handle_from_ifkey(WIFI_AP_DEF) 返回 null，IP 仍 192.168.71.1");
        } else {
            let _ = esp_netif_dhcps_stop(netif);
            let mut ip_info: esp_netif_ip_info_t = core::mem::zeroed();
            // ESP-IDF lwip addr 是 32-bit packed network order，等价 LE 字节数组
            ip_info.ip.addr      = u32::from_le_bytes([192, 168, 4, 1]);
            ip_info.gw.addr      = u32::from_le_bytes([192, 168, 4, 1]);
            ip_info.netmask.addr = u32::from_le_bytes([255, 255, 255, 0]);
            let r1 = esp_netif_set_ip_info(netif, &ip_info);

            // DHCP Option 114 (RFC 8910 Captive-Portal URI)：在 DHCP 协议层告诉客户端
            // 这是 captive portal + URL。Android 11+/iOS 14+/Win10 build 2004+ 原生支持，
            // 不依赖 DNS hijack（不被 Private DNS / DoH 绕过），客户端 OS 自动弹"登录此 WiFi"
            // 不支持的老 OS 会忽略此 option，无副作用（仍依赖 DNS hijack + HTTP probe 兜底）
            // ESP-IDF sys bindings 没生成 ESP_NETIF_CAPTIVEPORTAL_URI 常量，用裸数字 114
            const ESP_NETIF_CAPTIVEPORTAL_URI: esp_netif_dhcp_option_id_t = 114;
            let captive_uri: &[u8] = b"http://192.168.4.1/";
            let r_cp = esp_netif_dhcps_option(
                netif,
                esp_netif_dhcp_option_mode_t_ESP_NETIF_OP_SET,
                ESP_NETIF_CAPTIVEPORTAL_URI,
                captive_uri.as_ptr() as *mut _,
                captive_uri.len() as u32,
            );

            let r2 = esp_netif_dhcps_start(netif);
            if r1 != ESP_OK || r2 != ESP_OK {
                ::log::warn!("[WiFi-AP] set_ip_info=0x{:X} dhcps_start=0x{:X}（IP 可能仍 192.168.71.1）", r1, r2);
            } else {
                ::log::info!("[WiFi-AP] AP IP 已强制设为 192.168.4.1（DHCP lease pool 自动调整）");
            }
            if r_cp == ESP_OK {
                ::log::info!("[WiFi-AP] DHCP Option 114 已设 captive portal URI = http://192.168.4.1/");
            } else {
                ::log::warn!("[WiFi-AP] dhcps_option(114) 返回 0x{:X}（OS 不支持，仍可手动访问 192.168.4.1）", r_cp);
            }

            // === 修 B：用 esp_netif_set_dns_info 设 DNS = 192.168.4.1 ===
            // ESP-IDF 5.5.1 dhcps_option(6) 不支持（返回 0x5001），改用 esp_netif_set_dns_info API
            // 这是 ESP-IDF 官方推荐设 netif DNS 的方式：dhcps 自动从 netif 取 DNS info 加入 DHCP ACK option 6
            // 不设 DNS → 手机会用系统全局 DNS（8.8.8.8 等）→ DNS hijack 完全失效（用户实测多设备失败的根因）
            let mut dns_info: esp_netif_dns_info_t = core::mem::zeroed();
            dns_info.ip.u_addr.ip4.addr = u32::from_le_bytes([192, 168, 4, 1]);
            dns_info.ip.type_ = 0;  // 0 = IPADDR_TYPE_V4
            let r_dns = esp_netif_set_dns_info(
                netif,
                esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN,
                &mut dns_info,
            );
            if r_dns == ESP_OK {
                ::log::info!("[WiFi-AP] esp_netif_set_dns_info(MAIN) = 192.168.4.1（手机 DHCP 将收到此 DNS）");
            } else {
                ::log::warn!("[WiFi-AP] set_dns_info 返回 0x{:X}（手机可能用系统 DNS，hijack 失效）", r_dns);
            }
        }
    }

    // === 修 2：进 AP 模式后立即清 boot_mode（避免用户中途断电导致下次开机仍进 SoftAP）===
    // 之后无论用户操作（提交凭据 / 退出按钮 / 双击 / watchdog 超时）, 重启都自然回 STA
    if let Err(e) = nvs_cfg::erase_boot_mode() {
        ::log::warn!("[WiFi-AP] erase_boot_mode 失败: {} （下次重启可能仍进 SoftAP）", e);
    } else {
        ::log::info!("[WiFi-AP] boot_mode 已清除（下次重启回 STA，除非再双击或无凭据）");
    }

    // 标记 SoftAP 激活（UI 自动响应 — E v5.1 已实现：顶栏 WiFi 图标橙 + 底栏 IP 192.168.4.1 蓝）
    {
        let mut s = state.lock().unwrap();
        s.softap_active = true;
        s.softap_clients = 0;
        s.head_count = s.head_count.wrapping_add(1);
    }

    ::log::info!(
        "[WiFi-AP] ★ SoftAP 已启动：SSID=\"{}\" IP=192.168.4.1 (no_credentials={})",
        ssid.as_str(),
        softap.no_credentials
    );

    // C2 阶段在此启动 softap (HTTP) 和 dns_server 线程
    crate::softap::start_http_server(state.clone(), softap.no_credentials);
    crate::dns_server::start_dns_server();

    // SoftAP 主循环：仅维持线程存活，所有退出都通过 esp_restart() 触发（softap.rs / button.rs / 网页）
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn connect_loop(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    state: &SharedState,
    ssid: &str,
    psk: &str,
) {
    let auth = if psk.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal };
    let cfg = Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().unwrap_or_default(),
        password: psk.try_into().unwrap_or_default(),
        auth_method: auth,
        ..Default::default()
    });
    if let Err(e) = wifi.set_configuration(&cfg) {
        ::log::error!("[WiFi] set_configuration 失败: {:?}", e);
        mark(state, WifiState::Failed, "");
        std::thread::sleep(std::time::Duration::from_secs(10));
        return;
    }

    let mut fail_count: u32 = 0;
    loop {
        // 检查扫描请求（连接失败间隙也允许扫描）
        let want_scan = { state.lock().map(|s| s.scan_request).unwrap_or(false) };
        if want_scan {
            let _ = wifi.disconnect();
            do_scan(wifi, state);
        }

        mark(state, WifiState::Connecting, "");
        ::log::info!("[WiFi] 连接 SSID=\"{}\" auth={:?}", ssid, auth);
        match wifi.connect() {
            Ok(_) => {}
            Err(e) => {
                fail_count += 1;
                ::log::warn!("[WiFi] connect 失败({}): {:?}，10s 后重试", fail_count, e);
                if fail_count >= 6 { mark(state, WifiState::Failed, ""); }
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            }
        }
        if let Err(e) = wifi.wait_netif_up() {
            ::log::warn!("[WiFi] wait_netif_up 失败: {:?}", e);
            let _ = wifi.disconnect();
            std::thread::sleep(std::time::Duration::from_secs(10));
            continue;
        }
        let ip_info = match wifi.wifi().sta_netif().get_ip_info() {
            Ok(i) => i,
            Err(e) => {
                ::log::warn!("[WiFi] get_ip_info 失败: {:?}", e);
                let _ = wifi.disconnect();
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            }
        };
        let mut ip_str: heapless::String<16> = heapless::String::new();
        let _ = write!(ip_str, "{}", ip_info.ip);
        ::log::info!("[WiFi] ★ 已连接，IP = {}", ip_str.as_str());
        fail_count = 0;
        mark(state, WifiState::Connected, ip_str.as_str());

        // 保活循环：每 5s 检查 is_connected 和 scan_request
        // is_connected 仅在连续 3 次返回 false 才认定断开（避免瞬态抖动把 UI 切到 Connecting）
        let mut consecutive_disconnect: u8 = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let want_scan = { state.lock().map(|s| s.scan_request).unwrap_or(false) };
            if want_scan {
                ::log::info!("[WiFi] PC 请求扫描，临时断开");
                let _ = wifi.disconnect();
                do_scan(wifi, state);
                break;
            }
            match wifi.is_connected() {
                Ok(true) => { consecutive_disconnect = 0; }
                _ => {
                    consecutive_disconnect = consecutive_disconnect.saturating_add(1);
                    ::log::warn!("[WiFi] is_connected=false (连续 {} 次)", consecutive_disconnect);
                    if consecutive_disconnect >= 3 {
                        ::log::warn!("[WiFi] 确认断开，重连");
                        mark(state, WifiState::Connecting, "");
                        break;
                    }
                }
            }
        }
    }
}

fn idle_loop(wifi: &mut BlockingWifi<EspWifi<'static>>, state: &SharedState) {
    loop {
        // 无凭据时只响应 PC 显式扫描请求，不做后台周期扫描，避免影响 UART 中继
        std::thread::sleep(std::time::Duration::from_secs(1));
        let want_scan = { state.lock().map(|s| s.scan_request).unwrap_or(false) };
        if want_scan {
            do_scan(wifi, state);
        }
        // 检查是否有新凭据写入（PC 凭据写完会 esp_restart，但保险起见也轮询）
        // 实际上 esp_restart 后会重新进入 wifi_main，故此处仅用于 panic-free
    }
}
