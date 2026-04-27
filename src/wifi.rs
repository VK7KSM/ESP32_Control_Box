// ===================================================================
// WiFi STA 客户端 + 扫描器
//   - 启动后立即 wifi.start()，关闭省电模式（避免与路由器 4-way handshake 冲突）
//   - 立即做一次扫描，结果推到 SharedState（PC 配网时按编号选择）
//   - NVS 有凭据 → 连接，断开自动重连
//   - 无凭据 → 待机，每 30s 自动 re-scan，PC 请求时按需 scan
//   - PC 通过 SharedState.scan_request 触发扫描；扫描完 scan_seq++ 由 pc_comm 推送
// ===================================================================

use crate::state::{SharedState, WifiAp, WifiState};
use core::fmt::Write as _;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys::*;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};

const NVS_NS: &str = "wifi";

fn read_creds(nvs_part: EspDefaultNvsPartition) -> Option<(heapless::String<32>, heapless::String<64>)> {
    let nvs: EspNvs<NvsDefault> = match EspNvs::new(nvs_part, NVS_NS, true) {
        Ok(n) => n,
        Err(e) => {
            log::warn!("[WiFi] 打开 NVS namespace 失败: {:?}", e);
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

pub fn start_wifi_thread(modem: Modem, state: SharedState) {
    std::thread::Builder::new()
        .name("wifi".into())
        .stack_size(8192)
        .spawn(move || wifi_main(modem, state))
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
    log::info!("[WiFi] 开始扫描...");
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
            log::info!("[WiFi] 扫描完成，找到 {} 个 AP", s.scan_results.len());
        }
        Err(e) => {
            log::warn!("[WiFi] 扫描失败: {:?}", e);
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

fn wifi_main(modem: Modem, state: SharedState) {
    let sysloop = match EspSystemEventLoop::take() {
        Ok(s) => s,
        Err(e) => { log::error!("[WiFi] sysloop 失败: {:?}", e); mark(&state, WifiState::Failed, ""); return; }
    };
    let nvs_part = match EspDefaultNvsPartition::take() {
        Ok(n) => n,
        Err(e) => { log::error!("[WiFi] NVS 失败: {:?}", e); mark(&state, WifiState::Failed, ""); return; }
    };

    let esp_wifi = match EspWifi::new(modem, sysloop.clone(), Some(nvs_part.clone())) {
        Ok(w) => w,
        Err(e) => { log::error!("[WiFi] EspWifi::new 失败: {:?}", e); mark(&state, WifiState::Failed, ""); return; }
    };
    let mut wifi = match BlockingWifi::wrap(esp_wifi, sysloop) {
        Ok(w) => w,
        Err(e) => { log::error!("[WiFi] BlockingWifi::wrap 失败: {:?}", e); mark(&state, WifiState::Failed, ""); return; }
    };

    // 必须先 set_configuration 一个空 STA 配置，否则 start() 后 scan 会失败
    if let Err(e) = wifi.set_configuration(&Configuration::Client(ClientConfiguration::default())) {
        log::error!("[WiFi] 初始 set_configuration 失败: {:?}", e);
        mark(&state, WifiState::Failed, "");
        return;
    }
    if let Err(e) = wifi.start() {
        log::error!("[WiFi] start 失败: {:?}", e);
        mark(&state, WifiState::Failed, "");
        return;
    }

    // 关闭 WiFi 省电模式（默认 WIFI_PS_MIN_MODEM 与某些路由器在 4-way handshake 时冲突）
    unsafe {
        let r = esp_wifi_set_ps(wifi_ps_type_t_WIFI_PS_NONE);
        if r == ESP_OK {
            log::info!("[WiFi] 已关闭省电模式 (WIFI_PS_NONE)");
        } else {
            log::warn!("[WiFi] esp_wifi_set_ps 返回 {}", r);
        }
    }

    log::info!("[WiFi] 已启动 STA 模式");

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
        log::error!("[WiFi] set_configuration 失败: {:?}", e);
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
        log::info!("[WiFi] 连接 SSID=\"{}\" auth={:?}", ssid, auth);
        match wifi.connect() {
            Ok(_) => {}
            Err(e) => {
                fail_count += 1;
                log::warn!("[WiFi] connect 失败({}): {:?}，10s 后重试", fail_count, e);
                if fail_count >= 6 { mark(state, WifiState::Failed, ""); }
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            }
        }
        if let Err(e) = wifi.wait_netif_up() {
            log::warn!("[WiFi] wait_netif_up 失败: {:?}", e);
            let _ = wifi.disconnect();
            std::thread::sleep(std::time::Duration::from_secs(10));
            continue;
        }
        let ip_info = match wifi.wifi().sta_netif().get_ip_info() {
            Ok(i) => i,
            Err(e) => {
                log::warn!("[WiFi] get_ip_info 失败: {:?}", e);
                let _ = wifi.disconnect();
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            }
        };
        let mut ip_str: heapless::String<16> = heapless::String::new();
        let _ = write!(ip_str, "{}", ip_info.ip);
        log::info!("[WiFi] ★ 已连接，IP = {}", ip_str.as_str());
        fail_count = 0;
        mark(state, WifiState::Connected, ip_str.as_str());

        // 保活循环：每 5s 检查 is_connected 和 scan_request
        // is_connected 仅在连续 3 次返回 false 才认定断开（避免瞬态抖动把 UI 切到 Connecting）
        let mut consecutive_disconnect: u8 = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let want_scan = { state.lock().map(|s| s.scan_request).unwrap_or(false) };
            if want_scan {
                log::info!("[WiFi] PC 请求扫描，临时断开");
                let _ = wifi.disconnect();
                do_scan(wifi, state);
                break;
            }
            match wifi.is_connected() {
                Ok(true) => { consecutive_disconnect = 0; }
                _ => {
                    consecutive_disconnect = consecutive_disconnect.saturating_add(1);
                    log::warn!("[WiFi] is_connected=false (连续 {} 次)", consecutive_disconnect);
                    if consecutive_disconnect >= 3 {
                        log::warn!("[WiFi] 确认断开，重连");
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
