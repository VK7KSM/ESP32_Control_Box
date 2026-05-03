// ===================================================================
// 时间同步模块（NTP + 手动 + RTC + 屏幕显示三色）
//
// 职责：
//   - 应用 POSIX TZ 字符串到 newlib（setenv("TZ",...) + tzset()）
//   - 启动 EspSntp NTP 客户端（仅 STA 模式 + cfg_ntp_enabled=true）
//   - 应用手动设置时间到 RTC（settimeofday）
//   - 提供 current_clock(state) → (HH:MM:SS, ClockColor) 给 UI
//   - 提供 current_unix_sec() → main.rs 主循环每秒 tick 触发用
//
// 三色规则：
//   橙 = 已校准（last_ntp_sync_us > 0 AND age ≤ 24h）
//   蓝 = 已漂移（last_ntp_sync_us > 0 AND age > 24h）
//   灰 = 从未同步（last_ntp_sync_us == 0）→ 显示 "--:--:--"
// ===================================================================

use crate::state::{SharedState, WifiState};
use core::fmt::Write as _;
use core::time::Duration;
use esp_idf_svc::sntp::{EspSntp, OperatingMode, SntpConf, SyncMode};
use esp_idf_svc::sys::{
    esp_timer_get_time, gettimeofday, localtime_r, settimeofday, setenv, timeval, tm, tzset,
};

/// 时钟颜色（与 ui.rs 配色对应：Orange→AMBER, Blue→CYAN, Gray→GRAY）
#[derive(Copy, Clone, Debug)]
pub enum ClockColor {
    Orange,  // 已校准 ≤24h
    Blue,    // 已漂移 >24h
    Gray,    // 从未同步
}

/// 24 小时阈值（微秒）
const DAY_US: u64 = 24 * 3600 * 1_000_000;

/// 应用 POSIX TZ 字符串。必须在第一次 localtime_r 之前调用。
/// 失败（如 setenv 返回非 0）只 log warn，不 panic（newlib tzset 容错回退 UTC）
pub fn apply_tz(tz_posix: &str) {
    use std::ffi::CString;
    let key = CString::new("TZ").unwrap();
    let val = match CString::new(tz_posix) {
        Ok(v) => v,
        Err(_) => {
            ::log::warn!("[Time] TZ 字符串包含 NUL，跳过");
            return;
        }
    };
    let r = unsafe { setenv(key.as_ptr(), val.as_ptr(), 1) };
    if r != 0 {
        ::log::warn!("[Time] setenv(TZ) 失败 ret={}", r);
    }
    unsafe { tzset(); }
    ::log::info!("[Time] TZ 已应用：{}", tz_posix);
}

/// 应用手动设置时间到 RTC，并把 last_ntp_sync_us 视为"刚同步"。
/// `manual_us` = unix epoch 微秒（网页 JS new Date(...).getTime()*1000）
pub fn apply_manual_time(state: &SharedState, manual_us: u64) {
    if manual_us == 0 {
        ::log::warn!("[Time] apply_manual_time: manual_us=0，跳过");
        return;
    }
    let tv = timeval {
        tv_sec:  (manual_us / 1_000_000) as _,
        tv_usec: (manual_us % 1_000_000) as _,
    };
    let r = unsafe { settimeofday(&tv, core::ptr::null()) };
    if r != 0 {
        ::log::error!("[Time] settimeofday 失败 ret={}", r);
        return;
    }
    let now_uptime_us = unsafe { esp_timer_get_time() } as u64;
    if let Ok(mut s) = state.lock() {
        s.last_ntp_sync_us = now_uptime_us;
        s.head_count = s.head_count.wrapping_add(1);  // 触发 UI 立即重绘
    }
    ::log::info!("[Time] 手动时间已写入 RTC：unix_us={}", manual_us);
}

/// 取当前 unix 时间秒（i64）。NTP/手动同步前返回 boot epoch（1970+uptime）。
/// 主循环用它做"秒变化"边沿检测来触发每秒 redraw。
pub fn current_unix_sec() -> i64 {
    let mut tv: timeval = unsafe { core::mem::zeroed() };
    unsafe { gettimeofday(&mut tv, core::ptr::null_mut()); }
    tv.tv_sec as i64
}

/// 取当前时钟显示文本 + 颜色
/// Fix C：总是格式化 wall clock（即使未同步也显示时间在走，让用户看到时钟工作中）
/// 未同步（last_ntp_sync_us==0）→ "HH:MM:SS"（boot epoch+uptime localized to TZ）+ Gray
/// 已同步 ≤24h → "HH:MM:SS" + Orange
/// 已同步 >24h → "HH:MM:SS" + Blue
pub fn current_clock(state: &SharedState) -> (heapless::String<8>, ClockColor) {
    let last_sync = state.lock().map(|s| s.last_ntp_sync_us).unwrap_or(0);
    let mut s: heapless::String<8> = heapless::String::new();

    // 总是从 gettimeofday 取 wall clock 并格式化
    let mut tv: timeval = unsafe { core::mem::zeroed() };
    unsafe { gettimeofday(&mut tv, core::ptr::null_mut()); }
    let mut t: tm = unsafe { core::mem::zeroed() };
    let sec = tv.tv_sec;
    unsafe { localtime_r(&sec, &mut t); }
    let _ = write!(s, "{:02}:{:02}:{:02}", t.tm_hour, t.tm_min, t.tm_sec);

    // 颜色按同步状态区分
    let color = if last_sync == 0 {
        ClockColor::Gray  // 未同步：显示 wall clock 但用灰色提示用户未校准
    } else {
        let now_us = unsafe { esp_timer_get_time() } as u64;
        let age_us = now_us.saturating_sub(last_sync);
        if age_us > DAY_US { ClockColor::Blue } else { ClockColor::Orange }
    };
    (s, color)
}

/// 启动 NTP 后台线程（绑 CPU 1，栈 4096）
/// 线程职责：等 WiFi 第一次 Connected → 创建 EspSntp（永不 drop）→ 死循环 sleep
/// EspSntp 内部 60s poll 自动重试，WiFi 断开重连时自愈，无需手动 stop/start
pub fn start_ntp_thread(state: SharedState) {
    let _ = esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
        pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core1),
        ..Default::default()
    }.set();
    std::thread::Builder::new()
        .name("ntp_sync".into())
        .stack_size(4096)
        .spawn(move || ntp_main(state))
        .expect("ntp_sync 线程启动失败");
}

fn ntp_main(state: SharedState) {
    ::log::info!("[NTP] 线程启动，等待 WiFi 连接...");

    // 等 WiFi 第一次 Connected + cfg_ntp_enabled
    loop {
        let (connected, ntp_on) = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            (matches!(s.wifi_state, WifiState::Connected), s.cfg_ntp_enabled)
        };
        if connected && ntp_on { break; }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    // 取配置 NTP 服务器（cfg_ntp_server，默认 au.pool.ntp.org）
    let server_owned: String = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.cfg_ntp_server.is_empty() {
            "au.pool.ntp.org".to_string()
        } else {
            s.cfg_ntp_server.as_str().to_string()
        }
    };

    // ESP-IDF 默认 SNTP_MAX_SERVERS=1（lwipopts），SntpConf::servers 是定长数组。
    // 用 ..Default::default() 自动适配数组大小，再用切片 fill 覆盖所有 slot
    let mut conf = SntpConf {
        operating_mode: OperatingMode::Poll,
        sync_mode: SyncMode::Immediate,  // 首次同步立即跳到正确时间（vs Smooth 渐进）
        ..Default::default()
    };
    for slot in conf.servers.iter_mut() {
        *slot = &server_owned;
    }

    let st_for_cb = state.clone();
    let _sntp = match EspSntp::new_with_callback(&conf, move |dur: Duration| {
        let now_uptime_us = unsafe { esp_timer_get_time() } as u64;
        if let Ok(mut s) = st_for_cb.lock() {
            s.last_ntp_sync_us = now_uptime_us;
            s.head_count = s.head_count.wrapping_add(1);  // 首次同步触发立即重绘
        }
        ::log::info!("[NTP] 同步成功 unix_us={}", dur.as_micros());
    }) {
        Ok(s) => s,
        Err(e) => {
            ::log::error!("[NTP] EspSntp 启动失败 {:?}，clock 将永显 --:--:--", e);
            return;
        }
    };

    ::log::info!("[NTP] EspSntp 已启动，服务器={}（内部 60s poll 自愈，永不 drop）", server_owned);
    // EspSntp owned in this stack frame; thread never exits → never drops → SNTP 永远工作
    loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
}
