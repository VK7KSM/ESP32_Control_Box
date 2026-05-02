// ===================================================================
// SoftAP HTTP 服务器（C 功能 SoftAP + 网页配网）
//
// 路由：
//   GET  /                 → 主页 HTML（embedded from web_config）
//   GET  /api/status       → JSON 当前状态（版本/WiFi 状态/配置项/SoftAP 客户端数）
//   GET  /api/scan         → 触发 WiFi 扫描，等 ≤3s 返回 JSON 扫描结果
//   POST /api/wifi         → 提交 SSID/密码 → 写 NVS（原子）+ 清 boot_mode + esp_restart
//   POST /api/config       → 提交 ble_name/brightness/dim_timeout/ntp_enabled/manual_time + esp_restart
//   POST /api/reset        → 清所有 NVS namespace + esp_restart
//   POST /api/exit_softap  → 清 boot_mode + esp_restart 回 STA
//
// Watchdog：每 30s 检查 LAST_REQUEST_US，10 分钟无请求 → 自动退出 SoftAP（仅当 boot_mode 触发；
// 无凭据触发不超时，必须有效配置后才退出）
//
// HTTP server 启动需要 wifi netif 已就绪。由 wifi.rs::ap_main 在 wifi.start() 后调用本模块。
// ===================================================================

use crate::nvs_cfg;
use crate::state::SharedState;
use crate::web_config;
use embedded_svc::http::Headers;
use embedded_svc::http::Method;
use embedded_svc::io::{Read, Write};
use esp_idf_svc::http::server::{Configuration as HttpConfig, EspHttpServer};
use esp_idf_svc::sys::esp_timer_get_time;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// 全局 SharedState 引用，供 bump_last_request / watchdog 访问 state.softap_clients
/// 用 Mutex<Option<>> 而非 OnceLock 以保证 esp 工具链兼容
static SHARED_STATE_REF: Mutex<Option<SharedState>> = Mutex::new(None);

/// 上次 HTTP 请求时间（秒 since boot），用于 watchdog 超时检测
/// 用 AtomicU32 而非 AtomicU64 — xtensa-esp32s3 不支持 64-bit 原子
/// 单位用秒（u32 上限 ~136 年，远超 SoftAP 用途）
static LAST_REQUEST_S: AtomicU32 = AtomicU32::new(0);

/// SoftAP 超时（秒，仅 boot_mode 触发：10 分钟无 HTTP 请求 → esp_restart 回 STA）
const SOFTAP_TIMEOUT_S: u32 = 10 * 60;

/// softap_clients UI 显示阈值（秒，2 倍网页心跳间隔避免边界闪烁）
/// 60s 无请求 → state.softap_clients = 0（屏幕 IP 变蓝色）
const CLIENT_IDLE_THRESHOLD_S: u32 = 120;

/// Watchdog 检查间隔（秒）
const WATCHDOG_INTERVAL_S: u64 = 30;

/// Captive portal probe URL 列表（让手机连 SoftAP 后 OS 自动检测并弹"登录此 WiFi"）
/// 各 OS 探测 URL：Android(/generate_204) iOS/macOS(/hotspot-detect.html) Windows(/connecttest.txt)
/// 我们对这些 URL 返回 302 重定向到 http://192.168.4.1/，OS 识别为 captive portal
const CAPTIVE_PORTAL_PROBE_URLS: &[&str] = &[
    "/generate_204",                  // Android (CaptivePortalLogin)
    "/gen_204",                       // Android (older)
    "/hotspot-detect.html",           // iOS / macOS
    "/library/test/success.html",     // macOS
    "/connecttest.txt",               // Windows 10+
    "/ncsi.txt",                      // Windows older
];

/// Captive portal 重定向目标 URL（注：必须是 IP，因为没 DNS）
const REDIRECT_URL: &str = "http://192.168.4.1/";

/// HTTP server 任务栈（默认 4KB 太小，serde_json + WiFi 扫描需要 10KB）
const HTTP_STACK_SIZE: usize = 10_240;

/// POST 请求体最大长度（含网页提交的 SSID + PSK + 配置项 JSON）
const MAX_POST_LEN: usize = 1024;

/// 启动 HTTP server + watchdog 线程
/// `no_credentials`：true = 开箱无凭据触发的 SoftAP（不超时），false = 双击触发（10min 超时）
pub fn start_http_server(state: SharedState, no_credentials: bool) {
    // 存全局 state 引用，供 bump_last_request + watchdog 更新 softap_clients
    *SHARED_STATE_REF.lock().unwrap_or_else(|e| e.into_inner()) = Some(state.clone());

    // 初始化 LAST_REQUEST_S 为当前秒数（避免启动瞬间被判定为超时）
    let now_s = (unsafe { esp_timer_get_time() } as u64 / 1_000_000) as u32;
    LAST_REQUEST_S.store(now_s, Ordering::Relaxed);

    let cfg = HttpConfig {
        stack_size: HTTP_STACK_SIZE,
        ..Default::default()
    };

    let mut server = match EspHttpServer::new(&cfg) {
        Ok(s) => s,
        Err(e) => {
            ::log::error!("[SoftAP] HTTP server 创建失败: {:?}", e);
            return;
        }
    };

    // ===== GET / — 主页 HTML =====
    if let Err(e) = server.fn_handler("/", Method::Get, |req| -> Result<(), Box<dyn std::error::Error>> {
        bump_last_request();
        let mut resp = req.into_ok_response()?;
        resp.write_all(web_config::HTML.as_bytes())?;
        Ok(())
    }) {
        ::log::error!("[SoftAP] 注册 GET / 失败: {:?}", e);
    }

    // ===== GET /logo.png — 网页顶部 logo 图片（替代 ⬢ Unicode 装饰字符）=====
    if let Err(e) = server.fn_handler("/logo.png", Method::Get, |req| -> Result<(), Box<dyn std::error::Error>> {
        bump_last_request();
        let mut resp = req.into_response(200, Some("OK"), &[
            ("Content-Type", "image/png"),
            ("Cache-Control", "public, max-age=86400"),  // 浏览器缓存 1 天，避免每次请求
        ])?;
        resp.write_all(web_config::LOGO_PNG)?;
        Ok(())
    }) {
        ::log::error!("[SoftAP] 注册 GET /logo.png 失败: {:?}", e);
    }

    // ===== GET /favicon.ico — 浏览器 tab 标签 favicon（与 logo 同图）=====
    // 浏览器看到 <link rel="icon" type="image/png" href="/logo.png"> 会用 logo.png 作为 favicon
    // 但很多浏览器仍 fallback 请求 /favicon.ico；不注册会 404 + 日志噪音
    if let Err(e) = server.fn_handler("/favicon.ico", Method::Get, |req| -> Result<(), Box<dyn std::error::Error>> {
        bump_last_request();
        let mut resp = req.into_response(200, Some("OK"), &[
            ("Content-Type", "image/png"),
            ("Cache-Control", "public, max-age=86400"),
        ])?;
        resp.write_all(web_config::LOGO_PNG)?;
        Ok(())
    }) {
        ::log::error!("[SoftAP] 注册 GET /favicon.ico 失败: {:?}", e);
    }

    // ===== GET /api/status — JSON 当前状态 =====
    let state_status = state.clone();
    if let Err(e) = server.fn_handler("/api/status", Method::Get, move |req| -> Result<(), Box<dyn std::error::Error>> {
        bump_last_request();
        let body = build_status_json(&state_status, no_credentials);
        let mut resp = req.into_response(200, Some("OK"), &[
            ("Content-Type", "application/json"),
            ("Cache-Control", "no-store"),
        ])?;
        resp.write_all(body.as_bytes())?;
        Ok(())
    }) {
        ::log::error!("[SoftAP] 注册 GET /api/status 失败: {:?}", e);
    }

    // ===== GET /api/scan — 触发 WiFi 扫描 =====
    let state_scan = state.clone();
    if let Err(e) = server.fn_handler("/api/scan", Method::Get, move |req| -> Result<(), Box<dyn std::error::Error>> {
        bump_last_request();
        // 标记请求扫描，wifi.rs 检测后调用 do_scan
        // 注意：SoftAP 模式下 wifi.rs 在 ap_main，不会调用 connect_loop/idle_loop 中的 do_scan
        // 解决：直接在 handler 里用 ESP-IDF 裸 API 触发 scan + 读结果
        let body = run_wifi_scan_inline();
        let mut resp = req.into_response(200, Some("OK"), &[
            ("Content-Type", "application/json"),
        ])?;
        resp.write_all(body.as_bytes())?;
        let _ = state_scan; // 占位避免未使用警告
        Ok(())
    }) {
        ::log::error!("[SoftAP] 注册 GET /api/scan 失败: {:?}", e);
    }

    // ===== POST /api/wifi — 提交 WiFi 凭据 =====
    if let Err(e) = server.fn_handler("/api/wifi", Method::Post, move |mut req| -> Result<(), Box<dyn std::error::Error>> {
        bump_last_request();
        let body = match read_post_body(&mut req) {
            Ok(b) => b,
            Err(msg) => {
                let mut resp = req.into_status_response(400)?;
                resp.write_all(msg.as_bytes())?;
                return Ok(());
            }
        };
        match serde_json::from_slice::<WifiCreds>(&body) {
            Ok(creds) => {
                ::log::info!("[SoftAP] POST /api/wifi: ssid=\"{}\" psk_len={}",
                    creds.ssid, creds.psk.len());
                match nvs_cfg::write_wifi_and_clear_boot_mode(creds.ssid, creds.psk) {
                    Ok(_) => {
                        let mut resp = req.into_ok_response()?;
                        resp.write_all(b"{\"ok\":true,\"action\":\"restart\"}")?;
                        // 等响应送出后重启
                        std::thread::spawn(|| {
                            std::thread::sleep(Duration::from_millis(500));
                            unsafe { esp_idf_svc::sys::esp_restart(); }
                        });
                    }
                    Err(msg) => {
                        let mut resp = req.into_status_response(500)?;
                        resp.write_all(format!("{{\"ok\":false,\"error\":\"{}\"}}", msg).as_bytes())?;
                    }
                }
            }
            Err(e) => {
                let mut resp = req.into_status_response(400)?;
                resp.write_all(format!("{{\"ok\":false,\"error\":\"JSON parse: {}\"}}", e).as_bytes())?;
            }
        }
        Ok(())
    }) {
        ::log::error!("[SoftAP] 注册 POST /api/wifi 失败: {:?}", e);
    }

    // ===== POST /api/config — 提交其他配置项 =====
    if let Err(e) = server.fn_handler("/api/config", Method::Post, move |mut req| -> Result<(), Box<dyn std::error::Error>> {
        bump_last_request();
        let body = match read_post_body(&mut req) {
            Ok(b) => b,
            Err(msg) => {
                let mut resp = req.into_status_response(400)?;
                resp.write_all(msg.as_bytes())?;
                return Ok(());
            }
        };
        match serde_json::from_slice::<ConfigData>(&body) {
            Ok(data) => {
                let mut errors: Vec<String> = Vec::new();
                if let Some(name) = data.ble_name {
                    if let Err(e) = nvs_cfg::write_string(nvs_cfg::NS_CFG, nvs_cfg::KEY_BLE_NAME, name) {
                        errors.push(format!("ble_name: {}", e));
                    }
                }
                if let Some(b) = data.brightness {
                    if let Err(e) = nvs_cfg::write_u8(nvs_cfg::NS_CFG, nvs_cfg::KEY_BRIGHTNESS, b) {
                        errors.push(format!("brightness: {}", e));
                    }
                }
                if let Some(t) = data.dim_timeout {
                    if let Err(e) = nvs_cfg::write_u16(nvs_cfg::NS_CFG, nvs_cfg::KEY_DIM_TIMEOUT, t) {
                        errors.push(format!("dim_timeout: {}", e));
                    }
                }
                if let Some(n) = data.ntp_enabled {
                    if let Err(e) = nvs_cfg::write_u8(nvs_cfg::NS_CFG, nvs_cfg::KEY_NTP_ENABLED, if n { 1 } else { 0 }) {
                        errors.push(format!("ntp_enabled: {}", e));
                    }
                }
                if let Some(t) = data.manual_time_us {
                    if let Err(e) = nvs_cfg::write_u64(nvs_cfg::NS_CFG, nvs_cfg::KEY_MANUAL_TIME, t) {
                        errors.push(format!("manual_time: {}", e));
                    }
                }
                if errors.is_empty() {
                    let mut resp = req.into_ok_response()?;
                    resp.write_all(b"{\"ok\":true,\"action\":\"restart\"}")?;
                    std::thread::spawn(|| {
                        std::thread::sleep(Duration::from_millis(500));
                        unsafe { esp_idf_svc::sys::esp_restart(); }
                    });
                } else {
                    let msg = errors.join("; ");
                    let mut resp = req.into_status_response(500)?;
                    resp.write_all(format!("{{\"ok\":false,\"error\":\"{}\"}}", msg).as_bytes())?;
                }
            }
            Err(e) => {
                let mut resp = req.into_status_response(400)?;
                resp.write_all(format!("{{\"ok\":false,\"error\":\"JSON parse: {}\"}}", e).as_bytes())?;
            }
        }
        Ok(())
    }) {
        ::log::error!("[SoftAP] 注册 POST /api/config 失败: {:?}", e);
    }

    // ===== POST /api/reset — 清所有 NVS =====
    if let Err(e) = server.fn_handler("/api/reset", Method::Post, |req| -> Result<(), Box<dyn std::error::Error>> {
        bump_last_request();
        ::log::warn!("[SoftAP] POST /api/reset：清除所有 NVS namespace");
        match nvs_cfg::erase_all() {
            Ok(_) => {
                let mut resp = req.into_ok_response()?;
                resp.write_all(b"{\"ok\":true,\"action\":\"restart\"}")?;
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(500));
                    unsafe { esp_idf_svc::sys::esp_restart(); }
                });
            }
            Err(msg) => {
                let mut resp = req.into_status_response(500)?;
                resp.write_all(format!("{{\"ok\":false,\"error\":\"{}\"}}", msg).as_bytes())?;
            }
        }
        Ok(())
    }) {
        ::log::error!("[SoftAP] 注册 POST /api/reset 失败: {:?}", e);
    }

    // ===== POST /api/exit_softap — 清 boot_mode 回 STA（不动 wifi 凭据）=====
    if let Err(e) = server.fn_handler("/api/exit_softap", Method::Post, |req| -> Result<(), Box<dyn std::error::Error>> {
        bump_last_request();
        ::log::info!("[SoftAP] POST /api/exit_softap：用户主动退出");
        match nvs_cfg::erase_boot_mode() {
            Ok(_) => {
                let mut resp = req.into_ok_response()?;
                resp.write_all(b"{\"ok\":true,\"action\":\"restart\"}")?;
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(500));
                    unsafe { esp_idf_svc::sys::esp_restart(); }
                });
            }
            Err(msg) => {
                let mut resp = req.into_status_response(500)?;
                resp.write_all(format!("{{\"ok\":false,\"error\":\"{}\"}}", msg).as_bytes())?;
            }
        }
        Ok(())
    }) {
        ::log::error!("[SoftAP] 注册 POST /api/exit_softap 失败: {:?}", e);
    }

    // ===== Captive Portal probe handlers — 让手机自动弹"登录此 WiFi"通知 =====
    // 各 OS 连 SoftAP 后会自动发探测请求，我们返回 302 重定向 → OS 识别为 captive portal
    // 配合 dns_server.rs 的 DNS hijack（所有域名 → 192.168.4.1），手机自动开浏览器到配网页
    for url in CAPTIVE_PORTAL_PROBE_URLS {
        let url_owned = url.to_string();
        if let Err(e) = server.fn_handler::<Box<dyn std::error::Error>, _>(url, Method::Get, |req| {
            bump_last_request();
            // 302 重定向到 http://192.168.4.1/，让浏览器自动跳到配网页
            let _ = req.into_response(302, Some("Found"), &[
                ("Location", REDIRECT_URL),
                ("Content-Length", "0"),
            ])?;
            Ok(())
        }) {
            ::log::error!("[SoftAP] 注册 captive portal {} 失败: {:?}", url_owned, e);
        }
    }

    // ===== 修 A：404 wildcard handler — 拦截所有未匹配 URL，返回 302 重定向 =====
    // 让任何 OS 厂商私有 captive portal probe URL（如 Samsung /connectivity_check、小米 /redirect 等）
    // 都能被识别为 captive portal，触发 OS 弹"登录此 WiFi"通知
    // esp-idf-svc EspHttpServer 没暴露 register_err_handler，用 RawHandle 取 raw httpd_handle_t 调裸 API
    {
        use esp_idf_svc::handle::RawHandle;
        let raw_handle = server.handle();
        unsafe {
            let r = esp_idf_svc::sys::httpd_register_err_handler(
                raw_handle,
                esp_idf_svc::sys::httpd_err_code_t_HTTPD_404_NOT_FOUND,
                Some(captive_404_handler),
            );
            if r == esp_idf_svc::sys::ESP_OK {
                ::log::info!("[SoftAP] 404 wildcard handler 已注册（所有未匹配 URL → 302 重定向）");
            } else {
                ::log::warn!("[SoftAP] httpd_register_err_handler(404) 返回 0x{:X}", r);
            }
        }
    }

    // 让 server 永久存活（不 drop 否则 HTTP server 会停）
    core::mem::forget(server);
    ::log::info!("[SoftAP] HTTP server 已启动 (port 80, stack {}KB, captive portal handlers={} + 404 wildcard)",
        HTTP_STACK_SIZE / 1024, CAPTIVE_PORTAL_PROBE_URLS.len());

    // 启动 watchdog 线程（仅 boot_mode 触发的 SoftAP 才超时）
    // 栈 4096：::log::info! 宏内部 vsnprintf 占 ~1KB + FreeRTOS overhead + Rust runtime，
    // 2048 实测崩溃在第一行 log 输出（栈被 0xa5 填充全部覆盖）
    if !no_credentials {
        std::thread::Builder::new()
            .name("softap_wd".into())
            .stack_size(4096)
            .spawn(move || softap_watchdog_main())
            .ok();
    }
}

/// 404 wildcard handler — 任何未注册的 URL 都返回 302 重定向到 http://192.168.4.1/
/// 让 OS 识别为 captive portal 并自动弹"登录此 WiFi"通知
/// extern "C" fn 由 ESP-IDF httpd 在 handler thread 调用（不在 Rust 线程，无 Send/Sync 要求）
/// 函数内仅调 ESP-IDF httpd_resp_* 裸 API，不持锁、不阻塞
extern "C" fn captive_404_handler(
    req: *mut esp_idf_svc::sys::httpd_req_t,
    _err: esp_idf_svc::sys::httpd_err_code_t,
) -> esp_idf_svc::sys::esp_err_t {
    // C 字符串字面量（Rust 1.77+），尾随 \0 自动添加，*const c_char 兼容
    let status: &core::ffi::CStr = c"302 Found";
    let location_field: &core::ffi::CStr = c"Location";
    let location_value: &core::ffi::CStr = c"http://192.168.4.1/";
    let content_type: &core::ffi::CStr = c"text/plain";
    let empty_body: &core::ffi::CStr = c"";
    unsafe {
        esp_idf_svc::sys::httpd_resp_set_status(req, status.as_ptr());
        esp_idf_svc::sys::httpd_resp_set_type(req, content_type.as_ptr());
        esp_idf_svc::sys::httpd_resp_set_hdr(req, location_field.as_ptr(), location_value.as_ptr());
        esp_idf_svc::sys::httpd_resp_send(req, empty_body.as_ptr(), 0);
    }
    bump_last_request();  // 算一次 HTTP 请求活动
    esp_idf_svc::sys::ESP_OK
}

/// 更新 LAST_REQUEST_S（每个 handler 开头调用）
/// 同时设 state.softap_clients=1（屏幕 IP 变橙色，UI 反映"客户端连接中"）
fn bump_last_request() {
    let now_s = (unsafe { esp_timer_get_time() } as u64 / 1_000_000) as u32;
    LAST_REQUEST_S.store(now_s, Ordering::Relaxed);
    // 仅当从 0 → 1 才更新 + head_count++（避免每次请求都触发 redraw）
    if let Some(state) = SHARED_STATE_REF.lock().ok().and_then(|g| g.clone()) {
        if let Ok(mut s) = state.lock() {
            if s.softap_clients == 0 {
                s.softap_clients = 1;
                s.head_count = s.head_count.wrapping_add(1);
            }
        }
    }
}

/// Watchdog：每 30s 检查 (1) softap_clients idle 阈值（120s 无请求 → IP 变蓝）；
/// (2) 10min 超时（仅 boot_mode 触发，esp_restart 回 STA；boot_mode 已在 ap_main 清，重启自然 STA）
fn softap_watchdog_main() {
    ::log::info!("[SoftAP-WD] watchdog 启动（每{}s 检查 softap_clients/超时）", WATCHDOG_INTERVAL_S);
    loop {
        std::thread::sleep(Duration::from_secs(WATCHDOG_INTERVAL_S));
        let now_s = (unsafe { esp_timer_get_time() } as u64 / 1_000_000) as u32;
        let last = LAST_REQUEST_S.load(Ordering::Relaxed);
        let elapsed = now_s.saturating_sub(last);

        // 检查 1：CLIENT_IDLE_THRESHOLD_S（120s）无请求 → softap_clients=0（屏幕 IP 变蓝）
        if elapsed >= CLIENT_IDLE_THRESHOLD_S {
            if let Some(state) = SHARED_STATE_REF.lock().ok().and_then(|g| g.clone()) {
                if let Ok(mut s) = state.lock() {
                    if s.softap_clients != 0 {
                        s.softap_clients = 0;
                        s.head_count = s.head_count.wrapping_add(1);
                    }
                }
            }
        }

        // 检查 2：SOFTAP_TIMEOUT_S（10min）无请求 → esp_restart（boot_mode 已在 ap_main 清）
        if elapsed >= SOFTAP_TIMEOUT_S {
            ::log::warn!(
                "[SoftAP-WD] 超过 10min 无 HTTP 请求（{}s），esp_restart 回 STA",
                elapsed
            );
            std::thread::sleep(Duration::from_millis(100));
            unsafe { esp_idf_svc::sys::esp_restart(); }
        }
    }
}

// ===== POST 请求体读取 =====
fn read_post_body<C>(req: &mut esp_idf_svc::http::server::Request<C>) -> Result<Vec<u8>, String>
where C: esp_idf_svc::http::server::Connection
{
    let len = req.content_len().unwrap_or(0) as usize;
    if len == 0 {
        return Err("empty body".into());
    }
    if len > MAX_POST_LEN {
        return Err(format!("body too large: {} > {}", len, MAX_POST_LEN));
    }
    let mut buf = vec![0u8; len];
    req.read_exact(&mut buf).map_err(|e| format!("read body: {:?}", e))?;
    Ok(buf)
}

// ===== JSON 类型 =====

#[derive(Deserialize)]
struct WifiCreds<'a> {
    ssid: &'a str,
    psk: &'a str,
}

#[derive(Deserialize)]
struct ConfigData<'a> {
    #[serde(borrow)]
    ble_name: Option<&'a str>,
    brightness: Option<u8>,
    dim_timeout: Option<u16>,
    ntp_enabled: Option<bool>,
    manual_time_us: Option<u64>,
}

#[derive(Serialize)]
struct StatusJson<'a> {
    version: &'a str,
    softap_active: bool,
    no_credentials: bool,
    softap_clients: u32,
    ssid_suffix: String,
    cfg: CurrentCfg,
    wifi: WifiSummary,
}

#[derive(Serialize)]
struct CurrentCfg {
    ble_name: String,
    brightness: u8,
    dim_timeout: u16,
    ntp_enabled: bool,
}

#[derive(Serialize)]
struct WifiSummary {
    ssid: String,
}

#[derive(Serialize)]
struct ScanResult {
    aps: Vec<ScanApItem>,
    error: Option<String>,
}

#[derive(Serialize)]
struct ScanApItem {
    ssid: String,
    rssi: i8,
    auth: u8,
}

fn build_status_json(state: &SharedState, no_credentials: bool) -> String {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let saved_ssid = nvs_cfg::read_string(nvs_cfg::NS_WIFI, nvs_cfg::KEY_SSID, 33)
        .map(|v| v.as_str().to_string())
        .unwrap_or_default();
    let ble_name = nvs_cfg::read_string(nvs_cfg::NS_CFG, nvs_cfg::KEY_BLE_NAME, 17)
        .map(|v| v.as_str().to_string())
        .unwrap_or_else(|| "elfRadio".to_string());
    let brightness = nvs_cfg::read_u8(nvs_cfg::NS_CFG, nvs_cfg::KEY_BRIGHTNESS).unwrap_or(60);
    let dim_timeout = nvs_cfg::read_u16(nvs_cfg::NS_CFG, nvs_cfg::KEY_DIM_TIMEOUT).unwrap_or(150);
    let ntp_enabled = nvs_cfg::read_u8(nvs_cfg::NS_CFG, nvs_cfg::KEY_NTP_ENABLED).unwrap_or(1) != 0;

    let suffix = nvs_cfg::ssid_suffix();
    let st = StatusJson {
        version: crate::discovery::VERSION,
        softap_active: s.softap_active,
        no_credentials,
        softap_clients: s.softap_clients,
        ssid_suffix: suffix.as_str().to_string(),
        cfg: CurrentCfg { ble_name, brightness, dim_timeout, ntp_enabled },
        wifi: WifiSummary { ssid: saved_ssid },
    };
    serde_json::to_string(&st).unwrap_or_else(|_| "{}".into())
}

/// 在 SoftAP 模式下做 WiFi 扫描（wifi.rs::ap_main 不跑 connect_loop/idle_loop，无法走 do_scan）
/// 用 ESP-IDF 裸 API 直接触发 scan
fn run_wifi_scan_inline() -> String {
    use esp_idf_svc::sys::*;
    let mut aps_out: Vec<ScanApItem> = Vec::new();
    let mut error: Option<String> = None;

    unsafe {
        let mut scan_cfg: wifi_scan_config_t = core::mem::zeroed();
        // block=true: scan() 同步等待完成
        let r = esp_wifi_scan_start(&scan_cfg, true);
        if r != ESP_OK {
            error = Some(format!("scan_start={}", r));
        } else {
            let mut count: u16 = 0;
            esp_wifi_scan_get_ap_num(&mut count);
            let count = count.min(16);
            if count > 0 {
                let mut records: Vec<wifi_ap_record_t> = vec![core::mem::zeroed(); count as usize];
                let mut n = count;
                let r = esp_wifi_scan_get_ap_records(&mut n, records.as_mut_ptr());
                if r == ESP_OK {
                    for ap in records.iter().take(n as usize) {
                        let raw_ssid = ap.ssid;
                        let nul = raw_ssid.iter().position(|&b| b == 0).unwrap_or(raw_ssid.len());
                        let ssid_bytes = &raw_ssid[..nul];
                        let ssid = String::from_utf8_lossy(ssid_bytes).to_string();
                        aps_out.push(ScanApItem {
                            ssid,
                            rssi: ap.rssi,
                            auth: ap.authmode as u8,
                        });
                    }
                } else {
                    error = Some(format!("get_ap_records={}", r));
                }
            }
        }
        let _ = scan_cfg;
    }

    serde_json::to_string(&ScanResult { aps: aps_out, error })
        .unwrap_or_else(|_| "{\"aps\":[],\"error\":\"json\"}".into())
}
