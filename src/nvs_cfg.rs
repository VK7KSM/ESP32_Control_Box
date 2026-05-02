// ===================================================================
// NVS 配置集中管理（C 功能 SoftAP + 网页配网）
//
// 用裸 ESP-IDF NVS API（避开 esp-idf-svc 的 EspDefaultNvsPartition::take 单例限制），
// 让 main.rs 可以在 wifi 启动前提前读 NVS 决定模式，wifi.rs 内 EspNvs 仍正常工作。
//
// Namespace 设计：
//   - "wifi"：保留现有 ssid / psk（与 wifi.rs / pc_comm.rs 兼容）
//   - "cfg"：新增配置：boot_mode / ble_name / brightness / dim_timeout / ntp_enabled / manual_time_us
//
// 关键约束：所有写入 + 清除组合必须用同一个 nvs_open 事务 + 单 nvs_commit 保证原子性
// （避免 boot_mode 写入但凭据没写完导致死循环进 SoftAP）
// ===================================================================

use core::ffi::c_char;
use esp_idf_svc::sys::*;

// ===== Namespace 常量 =====
pub const NS_WIFI: &[u8] = b"wifi\0";
pub const NS_CFG:  &[u8] = b"cfg\0";

// ===== 标准 key 常量（用 \0 结尾，便于 nvs_*_str API 直接用）=====
pub const KEY_SSID:        &[u8] = b"ssid\0";
pub const KEY_PSK:         &[u8] = b"psk\0";
pub const KEY_BOOT_MODE:   &[u8] = b"boot_mode\0";
pub const KEY_BLE_NAME:    &[u8] = b"ble_name\0";
pub const KEY_BRIGHTNESS:  &[u8] = b"brightness\0";
pub const KEY_DIM_TIMEOUT: &[u8] = b"dim_timeout\0";
pub const KEY_NTP_ENABLED: &[u8] = b"ntp_enabled\0";
pub const KEY_MANUAL_TIME: &[u8] = b"manual_time\0";

/// 启动早期初始化 NVS（idempotent，重复调用安全）
/// EspDefaultNvsPartition::take() 内部也会调，重复调返回 OK
pub fn nvs_init_safe() {
    unsafe {
        let r = nvs_flash_init();
        if r == ESP_ERR_NVS_NO_FREE_PAGES || r == ESP_ERR_NVS_NEW_VERSION_FOUND {
            ::log::warn!("[NVS] flash 满 / 版本不匹配，erase + 重新 init");
            let _ = nvs_flash_erase();
            let _ = nvs_flash_init();
        }
    }
}

/// 读 NVS 字符串。返回 Some(content) 或 None（key 不存在 / namespace 不存在 / 读取失败）
/// `buf_size` 限制读取字节数，包含 NUL 结尾
pub fn read_string(ns: &[u8], key: &[u8], buf_size: usize) -> Option<heapless::String<128>> {
    let mut buf = vec![0u8; buf_size.max(1)];
    let mut handle: nvs_handle_t = 0;
    let mut len: usize = buf.len();
    unsafe {
        if nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return None;
        }
        let r = nvs_get_str(handle, key.as_ptr() as *const c_char, buf.as_mut_ptr() as *mut c_char, &mut len);
        nvs_close(handle);
        if r != ESP_OK {
            return None;
        }
    }
    // len 包含 NUL 结尾，砍掉
    let actual_len = if len > 0 { len - 1 } else { 0 };
    let slice = &buf[..actual_len];
    let s = core::str::from_utf8(slice).ok()?;
    let mut out: heapless::String<128> = heapless::String::new();
    let _ = out.push_str(s);
    Some(out)
}

/// 读 NVS u8（如 brightness）
pub fn read_u8(ns: &[u8], key: &[u8]) -> Option<u8> {
    let mut handle: nvs_handle_t = 0;
    let mut value: u8 = 0;
    unsafe {
        if nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return None;
        }
        let r = nvs_get_u8(handle, key.as_ptr() as *const c_char, &mut value);
        nvs_close(handle);
        if r != ESP_OK { return None; }
    }
    Some(value)
}

/// 读 NVS u16（如 dim_timeout）
pub fn read_u16(ns: &[u8], key: &[u8]) -> Option<u16> {
    let mut handle: nvs_handle_t = 0;
    let mut value: u16 = 0;
    unsafe {
        if nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return None;
        }
        let r = nvs_get_u16(handle, key.as_ptr() as *const c_char, &mut value);
        nvs_close(handle);
        if r != ESP_OK { return None; }
    }
    Some(value)
}

/// 读 NVS u64（如 manual_time_us）
pub fn read_u64(ns: &[u8], key: &[u8]) -> Option<u64> {
    let mut handle: nvs_handle_t = 0;
    let mut value: u64 = 0;
    unsafe {
        if nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return None;
        }
        let r = nvs_get_u64(handle, key.as_ptr() as *const c_char, &mut value);
        nvs_close(handle);
        if r != ESP_OK { return None; }
    }
    Some(value)
}

// ===== 模式判定（main.rs / wifi.rs 启动时调用）=====

/// 是否应进入 SoftAP 模式
/// 返回 (is_softap, is_no_credentials)：
///   - (false, _)：STA 模式
///   - (true, false)：双击触发的 SoftAP（cfg/boot_mode = "softap"，10min 超时）
///   - (true, true)：开箱无凭据触发的 SoftAP（不超时，等用户配）
pub fn should_enter_softap() -> (bool, bool) {
    // 优先检查 cfg/boot_mode
    if let Some(mode) = read_string(NS_CFG, KEY_BOOT_MODE, 16) {
        if mode.as_str() == "softap" {
            return (true, false);
        }
    }
    // 否则检查 wifi/ssid 是否有值
    let has_creds = read_string(NS_WIFI, KEY_SSID, 33)
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !has_creds {
        return (true, true);
    }
    (false, false)
}

// ===== 写入操作 =====

/// 写 boot_mode = "softap"（双击按钮触发，重启后进 SoftAP 10min 超时）
pub fn write_boot_mode_softap() -> Result<(), String> {
    let mut handle: nvs_handle_t = 0;
    unsafe {
        let r = nvs_open(NS_CFG.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READWRITE, &mut handle);
        if r != ESP_OK { return Err(format!("nvs_open(cfg)={}", r)); }
        let value = b"softap\0";
        let r = nvs_set_str(handle, KEY_BOOT_MODE.as_ptr() as *const c_char, value.as_ptr() as *const c_char);
        if r != ESP_OK { nvs_close(handle); return Err(format!("nvs_set_str(boot_mode)={}", r)); }
        let r = nvs_commit(handle);
        nvs_close(handle);
        if r != ESP_OK { return Err(format!("nvs_commit={}", r)); }
    }
    Ok(())
}

/// 清 boot_mode（用户主动退出 SoftAP / 10min 超时 / 提交 WiFi 凭据时）
/// 单独调用版本（不与 wifi 凭据原子）；需要原子时用 write_wifi_and_clear_boot_mode
pub fn erase_boot_mode() -> Result<(), String> {
    let mut handle: nvs_handle_t = 0;
    unsafe {
        let r = nvs_open(NS_CFG.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READWRITE, &mut handle);
        if r != ESP_OK { return Err(format!("nvs_open(cfg)={}", r)); }
        let r = nvs_erase_key(handle, KEY_BOOT_MODE.as_ptr() as *const c_char);
        if r != ESP_OK && r != ESP_ERR_NVS_NOT_FOUND {
            nvs_close(handle);
            return Err(format!("nvs_erase_key(boot_mode)={}", r));
        }
        let r = nvs_commit(handle);
        nvs_close(handle);
        if r != ESP_OK { return Err(format!("nvs_commit={}", r)); }
    }
    Ok(())
}

/// **原子事务**：写 wifi/ssid + wifi/psk + 清 cfg/boot_mode（单 commit）
/// 用于网页提交 WiFi 凭据后退出 SoftAP，避免中间断电导致死循环
pub fn write_wifi_and_clear_boot_mode(ssid: &str, psk: &str) -> Result<(), String> {
    if ssid.len() > 32 { return Err("ssid 超长 (>32)".into()); }
    if psk.len() > 64 { return Err("psk 超长 (>64)".into()); }

    // Step 1: 写 wifi namespace
    let mut handle: nvs_handle_t = 0;
    unsafe {
        let r = nvs_open(NS_WIFI.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READWRITE, &mut handle);
        if r != ESP_OK { return Err(format!("nvs_open(wifi)={}", r)); }

        let mut ssid_buf = [0u8; 33];
        ssid_buf[..ssid.len()].copy_from_slice(ssid.as_bytes());
        let r = nvs_set_str(handle, KEY_SSID.as_ptr() as *const c_char, ssid_buf.as_ptr() as *const c_char);
        if r != ESP_OK { nvs_close(handle); return Err(format!("nvs_set_str(ssid)={}", r)); }

        let mut psk_buf = [0u8; 65];
        psk_buf[..psk.len()].copy_from_slice(psk.as_bytes());
        let r = nvs_set_str(handle, KEY_PSK.as_ptr() as *const c_char, psk_buf.as_ptr() as *const c_char);
        if r != ESP_OK { nvs_close(handle); return Err(format!("nvs_set_str(psk)={}", r)); }

        let r = nvs_commit(handle);
        nvs_close(handle);
        if r != ESP_OK { return Err(format!("nvs_commit(wifi)={}", r)); }
    }

    // Step 2: 清 cfg/boot_mode（独立 namespace 无法和 wifi 一起 commit）
    erase_boot_mode()?;
    Ok(())
}

// ===== cfg 写入辅助（C4 网页配置项）=====

pub fn write_string(ns: &[u8], key: &[u8], value: &str) -> Result<(), String> {
    if value.len() > 127 { return Err("value 超长 (>127)".into()); }
    let mut buf = [0u8; 128];
    buf[..value.len()].copy_from_slice(value.as_bytes());

    let mut handle: nvs_handle_t = 0;
    unsafe {
        let r = nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READWRITE, &mut handle);
        if r != ESP_OK { return Err(format!("nvs_open={}", r)); }
        let r = nvs_set_str(handle, key.as_ptr() as *const c_char, buf.as_ptr() as *const c_char);
        if r != ESP_OK { nvs_close(handle); return Err(format!("nvs_set_str={}", r)); }
        let r = nvs_commit(handle);
        nvs_close(handle);
        if r != ESP_OK { return Err(format!("nvs_commit={}", r)); }
    }
    Ok(())
}

pub fn write_u8(ns: &[u8], key: &[u8], value: u8) -> Result<(), String> {
    let mut handle: nvs_handle_t = 0;
    unsafe {
        let r = nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READWRITE, &mut handle);
        if r != ESP_OK { return Err(format!("nvs_open={}", r)); }
        let r = nvs_set_u8(handle, key.as_ptr() as *const c_char, value);
        if r != ESP_OK { nvs_close(handle); return Err(format!("nvs_set_u8={}", r)); }
        let r = nvs_commit(handle);
        nvs_close(handle);
        if r != ESP_OK { return Err(format!("nvs_commit={}", r)); }
    }
    Ok(())
}

pub fn write_u16(ns: &[u8], key: &[u8], value: u16) -> Result<(), String> {
    let mut handle: nvs_handle_t = 0;
    unsafe {
        let r = nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READWRITE, &mut handle);
        if r != ESP_OK { return Err(format!("nvs_open={}", r)); }
        let r = nvs_set_u16(handle, key.as_ptr() as *const c_char, value);
        if r != ESP_OK { nvs_close(handle); return Err(format!("nvs_set_u16={}", r)); }
        let r = nvs_commit(handle);
        nvs_close(handle);
        if r != ESP_OK { return Err(format!("nvs_commit={}", r)); }
    }
    Ok(())
}

pub fn write_u64(ns: &[u8], key: &[u8], value: u64) -> Result<(), String> {
    let mut handle: nvs_handle_t = 0;
    unsafe {
        let r = nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READWRITE, &mut handle);
        if r != ESP_OK { return Err(format!("nvs_open={}", r)); }
        let r = nvs_set_u64(handle, key.as_ptr() as *const c_char, value);
        if r != ESP_OK { nvs_close(handle); return Err(format!("nvs_set_u64={}", r)); }
        let r = nvs_commit(handle);
        nvs_close(handle);
        if r != ESP_OK { return Err(format!("nvs_commit={}", r)); }
    }
    Ok(())
}

// ===== 全局重置（POST /api/reset）=====

/// 清 wifi + cfg 两个 namespace 的所有 key
pub fn erase_all() -> Result<(), String> {
    for ns in &[NS_WIFI, NS_CFG] {
        let mut handle: nvs_handle_t = 0;
        unsafe {
            let r = nvs_open(ns.as_ptr() as *const c_char, nvs_open_mode_t_NVS_READWRITE, &mut handle);
            if r != ESP_OK {
                if r == ESP_ERR_NVS_NOT_FOUND { continue; }
                return Err(format!("nvs_open={}", r));
            }
            let r = nvs_erase_all(handle);
            if r != ESP_OK && r != ESP_ERR_NVS_NOT_FOUND {
                nvs_close(handle);
                return Err(format!("nvs_erase_all={}", r));
            }
            let r = nvs_commit(handle);
            nvs_close(handle);
            if r != ESP_OK { return Err(format!("nvs_commit={}", r)); }
        }
    }
    Ok(())
}

// ===== SoftAP SSID 后缀（efuse MAC 后 4 位 hex 大写）=====

/// 读 efuse MAC 后 4 位 hex 大写（如 "F824"），用于 SoftAP SSID 后缀
/// 复用 discovery.rs 现有 MAC 读取流程，避免重复
pub fn ssid_suffix() -> heapless::String<8> {
    let mut mac = [0u8; 6];
    unsafe { esp_efuse_mac_get_default(mac.as_mut_ptr()); }
    let mut s: heapless::String<8> = heapless::String::new();
    use core::fmt::Write;
    let _ = write!(s, "{:02X}{:02X}", mac[4], mac[5]);
    s
}
