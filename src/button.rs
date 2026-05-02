// ===================================================================
// GPIO 2 物理按钮：长按 3s 触发开关机 / 短按唤醒屏幕 + 重启 BLE 广播
//
// 长按 3s：spawn 一次性 worker → 1.2s 脉冲 GPIO 8 → 主动心跳探针检测 → 重试 ≤2 次
// 短按（50ms~3s 释放）：head_count++ 唤醒屏幕 + ble::request_advertising_restart()
// 短按多次（连续）：未来 SoftAP 触发（C 功能做完后实现）
//
// 关机检测：主动注入 CW+CCW 旋钮探针 3 次（1s 间隔），3 次都无下行响应 → 关机成功
//          复用 s.knob_inject 现有机制，~5s 完成检测（远快于被动等 alive=false）
// 屏幕反馈：触发时 head_count++ 唤醒（main loop 配合 POWER_TOGGLE_IN_PROGRESS 例外允许）
// 并发保护：static AtomicBool POWER_TOGGLE_IN_PROGRESS 防 PC API 重叠
// ===================================================================

use crate::ble;
use crate::nvs_cfg;
use crate::state::{SharedState, StatusMsgColor};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use esp_idf_svc::sys::*;

const BUTTON_GPIO: i32 = 2;
const POWER_GPIO: i32 = 8;
const LONG_PRESS_DURATION: Duration = Duration::from_secs(3);
const SHORT_PRESS_MIN: Duration = Duration::from_millis(50);  // 去抖下限
const POLL_INTERVAL: Duration = Duration::from_millis(20);  // 50Hz 轮询自带去抖
/// 双击窗口：第一次释放后多长时间内再次按下算双击（典型用户双击 200~500ms，300ms 兼顾响应速度与误触）
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(300);
const PULSE_DURATION: Duration = Duration::from_millis(1200);  // GPIO 8 脉冲（沿用 pc_comm.rs 1.2s）
const POST_PULSE_WAIT: Duration = Duration::from_millis(1500);  // 等电台真正完成关机
const PROBE_GAP: Duration = Duration::from_millis(600);  // 单帧注入到响应到达
const PROBE_INTERVAL: Duration = Duration::from_millis(200);  // 探针之间间隔
const PROBES: u8 = 3;
const VERIFY_TIMEOUT_ON: Duration = Duration::from_secs(8);  // 开机检测：电台启动 2-5s 发首帧
const RETRY_GAP: Duration = Duration::from_secs(1);
const MAX_ATTEMPTS: u8 = 3;  // 首次 + 2 次重试

/// 全局电源切换互斥锁（物理按钮 + PC API PowerToggle 共用）
pub static POWER_TOGGLE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// 设置顶栏状态消息（如 "Powering off..."），同时 head_count++ 触发主循环立即重绘
/// clear_after_ms=0 表示持续显示直到下次 set 或 clear；>0 表示主循环到时自动清除
fn set_status(state: &SharedState, text: &str, color: StatusMsgColor, clear_after_ms: u64) {
    let mut s = state.lock().unwrap();
    s.status_msg.clear();
    let _ = s.status_msg.push_str(text);
    s.status_msg_color = color;
    s.status_msg_clear_at_us = if clear_after_ms == 0 {
        0
    } else {
        let now_us = unsafe { esp_timer_get_time() } as u64;
        now_us + clear_after_ms * 1000
    };
    s.head_count = s.head_count.wrapping_add(1);
}

/// 清除顶栏状态消息，恢复默认 "TYT TH-9800" 标题
fn clear_status(state: &SharedState) {
    let mut s = state.lock().unwrap();
    s.status_msg.clear();
    s.status_msg_clear_at_us = 0;
    s.head_count = s.head_count.wrapping_add(1);
}

pub fn try_acquire_power_lock() -> bool {
    POWER_TOGGLE_IN_PROGRESS
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
}

pub fn release_power_lock() {
    POWER_TOGGLE_IN_PROGRESS.store(false, Ordering::Release);
}

pub fn start_button_thread(state: SharedState) {
    let _ = esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
        pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core0),
        ..Default::default()
    }.set();
    std::thread::Builder::new()
        .name("button".into())
        .stack_size(4096)
        .spawn(move || button_main(state))
        .expect("button 线程启动失败");
}

fn button_main(state: SharedState) {
    // GPIO 2 INPUT + 内部上拉（按下接 GND → LOW）
    unsafe {
        let cfg = gpio_config_t {
            pin_bit_mask: 1u64 << BUTTON_GPIO,
            mode: gpio_mode_t_GPIO_MODE_INPUT,
            pull_up_en: gpio_pullup_t_GPIO_PULLUP_ENABLE,
            pull_down_en: gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
            intr_type: gpio_int_type_t_GPIO_INTR_DISABLE,
        };
        gpio_config(&cfg);
    }
    ::log::info!("[Button] GPIO {} 监听启动（短按=BLE广播+唤醒屏，长按 {}s=开关机）",
        BUTTON_GPIO, LONG_PRESS_DURATION.as_secs());

    let mut press_start: Option<Instant> = None;
    let mut already_triggered_long = false;  // 避免按住不放期间重复触发长按
    let mut last_countdown_secs: u64 = 0;    // 长按倒计时已显示到第几秒（1/2/3，0=未显示）
    // 双击检测状态：上次释放时刻 + duration（用于第二次按下时计算窗口）
    let mut last_release_time: Option<Instant> = None;
    let mut last_release_was_short_press: bool = false; // 上次释放是合法短按（>=50ms 且 <3s 且非长按已触发）

    loop {
        let pressed = unsafe { gpio_get_level(BUTTON_GPIO) } == 0;
        let now = Instant::now();

        // 双击窗口超时检查：上次合法短按释放后超过 DOUBLE_CLICK_WINDOW 仍未二次按下 → 执行短按动作
        // （此时确认为单按，可触发"唤醒屏幕 + 重启 BLE 广播"）
        if last_release_was_short_press {
            if let Some(t) = last_release_time {
                if now.duration_since(t) > DOUBLE_CLICK_WINDOW {
                    ::log::info!("[Button] 短按确认（无后续双击）→ 唤醒屏幕 + 重启 BLE 广播");
                    {
                        let mut s = state.lock().unwrap();
                        s.head_count = s.head_count.wrapping_add(1);
                    }
                    ble::request_advertising_restart();
                    last_release_was_short_press = false;
                    last_release_time = None;
                }
            }
        }

        match (pressed, press_start) {
            (true, None) => {
                // 检查是否双击：上次释放是合法短按 + 在 DOUBLE_CLICK_WINDOW 内再次按下
                let is_double_click = last_release_was_short_press
                    && last_release_time
                        .map(|t| now.duration_since(t) <= DOUBLE_CLICK_WINDOW)
                        .unwrap_or(false);

                if is_double_click {
                    // === 双击：检查 SoftAP 状态决定进入或退出 ===
                    let in_softap = state.lock().map(|s| s.softap_active).unwrap_or(false);
                    let switch_msg: &str;
                    if in_softap {
                        ::log::info!("[Button] 双击（SoftAP 模式）→ 清 boot_mode + 重启回 STA");
                        match nvs_cfg::erase_boot_mode() {
                            Ok(_) => ::log::info!("[Button] erase_boot_mode 成功"),
                            Err(e) => ::log::error!("[Button] erase_boot_mode 失败: {}", e),
                        }
                        switch_msg = "-> STA";
                    } else {
                        ::log::info!("[Button] 双击（STA 模式）→ 写 boot_mode=softap + 重启进 SoftAP");
                        match nvs_cfg::write_boot_mode_softap() {
                            Ok(_) => ::log::info!("[Button] write_boot_mode_softap 成功"),
                            Err(e) => ::log::error!("[Button] write_boot_mode_softap 失败: {}", e),
                        }
                        switch_msg = "-> SoftAP";
                    }
                    // 屏幕显示"切换中"提示，避免用户以为是崩溃（双击 esp_restart 是预期行为）
                    // sleep 1s 让用户看清提示文字（主循环 50ms tick + 200ms redraw 节流，1s 内会重绘 4-5 次）
                    set_status(&state, switch_msg, StatusMsgColor::Amber, 0);
                    std::thread::sleep(Duration::from_millis(1000));
                    unsafe { esp_restart(); }
                }

                press_start = Some(now);
                already_triggered_long = false;
                last_countdown_secs = 0;
                // 双击检测后清除标志（避免连击 3+ 次）
                last_release_was_short_press = false;
                last_release_time = None;
            }
            (true, Some(start)) if !already_triggered_long && now - start >= LONG_PRESS_DURATION => {
                // 长按 3 秒触发（每次按下只触发一次）
                already_triggered_long = true;
                ::log::info!("[Button] 长按 {}s 达成", LONG_PRESS_DURATION.as_secs());

                // 先 acquire 电源锁（让主循环 wake 判定时能正确看到 POWER_TOGGLE_IN_PROGRESS）
                if !try_acquire_power_lock() {
                    ::log::warn!("[Button] 已有 PowerToggle 进行中，跳过本次触发");
                } else {
                    // 后 head_count++ 唤醒屏幕（power_toggling=true，主循环允许唤醒覆盖 alive=false）
                    {
                        let mut s = state.lock().unwrap();
                        s.head_count = s.head_count.wrapping_add(1);
                    }
                    let st = state.clone();
                    std::thread::spawn(move || {
                        power_toggle_worker(st, false);  // 手动长按，fail 显示 5s
                        release_power_lock();
                    });
                }
            }
            (true, Some(start)) => {
                // 仍按下但未达 3s：在 1s/2s/3s 整秒边界更新倒计时（每秒至多 1 次）
                let elapsed_secs = (now - start).as_secs();
                if !already_triggered_long
                    && elapsed_secs > last_countdown_secs
                    && elapsed_secs >= 1
                    && elapsed_secs <= 3
                {
                    last_countdown_secs = elapsed_secs;
                    let was_alive = state.lock().unwrap().radio_alive;
                    let action = if was_alive { "Off" } else { "On" };
                    let countdown = 4 - elapsed_secs; // 1s→显示3, 2s→2, 3s→1
                    let mut text: heapless::String<32> = heapless::String::new();
                    use core::fmt::Write;
                    let _ = write!(text, "Radio {} {}..", action, countdown);
                    set_status(&state, text.as_str(), StatusMsgColor::Amber, 0);
                }
            }
            (false, Some(start)) => {
                // 释放：分类短按 vs 已触发的长按
                let duration = now - start;
                let is_short_press = !already_triggered_long
                    && duration >= SHORT_PRESS_MIN
                    && duration < LONG_PRESS_DURATION;

                if is_short_press {
                    // 注意：短按"动作"（唤醒+BLE 重启）不在这里执行，留到下面双击窗口超时后再决定
                    // 这是因为本次释放可能是双击的第一次释放，需等 DOUBLE_CLICK_WINDOW 内是否第二次按下
                    // 记录释放时刻供双击检测
                    last_release_time = Some(now);
                    last_release_was_short_press = true;
                } else {
                    // 长按已触发 / 抖动（< 50ms） → 清双击状态，避免与之前合法短按形成误判
                    last_release_was_short_press = false;
                    last_release_time = None;
                }

                // 释放后清除可能残留的倒计时显示（< 1s 短按时 last_countdown_secs=0 跳过；
                // 长按已触发时 worker 接管 status_msg，无需此处清除）
                if last_countdown_secs > 0 && !already_triggered_long {
                    clear_status(&state);
                }
                // 释放后无论何种类型都重置
                press_start = None;
                already_triggered_long = false;
                last_countdown_secs = 0;
            }
            (false, None) => {}
            _ => {}
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 主动心跳探针：注入非 MAIN 侧的 CW+CCW 旋钮帧，看是否触发下行响应
/// 返回 true = 电台仍活着（响应了），false = 电台无响应
/// 复用 s.knob_inject 现有机制（uart.rs::relay_up_thread 既有支持）
/// 单次调用 ~1.2 秒（CW 600ms + CCW 600ms）
pub fn probe_radio_alive(state: &SharedState) -> bool {
    // 选非 MAIN 侧（与心跳设计一致，避免干扰主操作侧）
    let (use_right, body_before) = {
        let s = state.lock().unwrap();
        let use_right = if s.right.is_main {
            false  // RIGHT=MAIN → 步进 LEFT
        } else if s.left.is_main {
            true   // LEFT=MAIN → 步进 RIGHT
        } else {
            false  // MAIN 未知 → 默认步进 LEFT
        };
        (use_right, s.body_count)
    };
    let cw  = if use_right { 0x82u8 } else { 0x02u8 };
    let ccw = if use_right { 0x81u8 } else { 0x01u8 };

    // CW 探针
    state.lock().unwrap().knob_inject = Some(cw);
    std::thread::sleep(PROBE_GAP);
    // CCW 反向（CW+CCW 净零频率变化，对用户无感知）
    state.lock().unwrap().knob_inject = Some(ccw);
    std::thread::sleep(PROBE_GAP);

    state.lock().unwrap().body_count != body_before
}

/// 电源切换 worker
/// 关机检测（was_alive=true）：1.5s 等待 + 主动探针 3 次 → 5s 内确认
/// 开机检测（was_alive=false）：等 ≤8s 看 body_count++（电台启动发首帧）
///
/// `is_auto`：true = 由 main.rs 启动宽限期触发的自动开机（失败显示 30s 让用户有时间看清）；
///            false = 用户长按按钮触发的手动开关机（失败显示 5s）
pub fn power_toggle_worker(state: SharedState, is_auto: bool) {
    let (was_alive, body_before) = {
        let s = state.lock().unwrap();
        (s.radio_alive, s.body_count)
    };
    ::log::info!("[PowerToggle] 开始：was_alive={} body={} (期望 {})",
        was_alive, body_before,
        if was_alive { "三次探针无响应 (关机)" } else { "body_count++ (开机)" });

    for attempt in 0..MAX_ATTEMPTS {
        // 更新顶栏状态消息（v4 计划文字：Radio Off... / Radio On...）
        // 重试期间不在 UI 显示次数（避免增加复杂度，重试详情在 log 里看）
        let status_text: &str = if was_alive { "Radio Off..." } else { "Radio On..." };
        set_status(&state, status_text, StatusMsgColor::Amber, 0);

        ::log::info!("[PowerToggle] 第 {} 次脉冲（GPIO {} → HIGH {}ms → LOW）",
            attempt + 1, POWER_GPIO, PULSE_DURATION.as_millis());
        unsafe {
            gpio_set_level(POWER_GPIO, 1);
            std::thread::sleep(PULSE_DURATION);
            gpio_set_level(POWER_GPIO, 0);
        }

        let success = if was_alive {
            // === 关机检测：主动探针 3 次 ===
            ::log::info!("[PowerToggle] 等 {}ms 后开始 {} 次主动探针检测...",
                POST_PULSE_WAIT.as_millis(), PROBES);
            std::thread::sleep(POST_PULSE_WAIT);
            let mut alive_responded = false;
            for probe in 0..PROBES {
                let responded = probe_radio_alive(&state);
                ::log::info!("[PowerToggle] 探针 {}/{}：{}",
                    probe + 1, PROBES,
                    if responded { "有响应（电台仍开机）" } else { "无响应" });
                if responded {
                    alive_responded = true;
                    break;
                }
                if probe < PROBES - 1 {
                    std::thread::sleep(PROBE_INTERVAL);
                }
            }
            !alive_responded  // 三次都无响应 → 关机成功
        } else {
            // === 开机检测：等 body_count++ ===
            ::log::info!("[PowerToggle] 等 ≤{}s 检测 body_count++...", VERIFY_TIMEOUT_ON.as_secs());
            let deadline = Instant::now() + VERIFY_TIMEOUT_ON;
            let mut detected = false;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let body_now = state.lock().unwrap().body_count;
                if body_now != body_before {
                    detected = true;
                    break;
                }
                if Instant::now() >= deadline { break; }
            }
            detected
        };

        if success {
            if was_alive {
                // 关机确认：worker 内部决策不依赖 alive 字段（基于 3 次探针无响应）
                // 此时 main loop 的 15s 超时尚未触发 alive=false，~15s 后才会转 false 并自动关屏
                ::log::info!("[PowerToggle] 第 {} 次成功（关机确认：三次探针无响应；alive 将在 ~15s 后由主循环转 false）",
                    attempt + 1);
            } else {
                // 开机确认：靠 body_count 增加触发
                let body_now = state.lock().unwrap().body_count;
                ::log::info!("[PowerToggle] 第 {} 次成功（开机确认：body_count {} → {}）",
                    attempt + 1, body_before, body_now);
            }
            // 成功 → 立即清除顶栏状态消息恢复默认标题
            clear_status(&state);
            return;
        }

        if attempt < MAX_ATTEMPTS - 1 {
            ::log::warn!("[PowerToggle] 第 {} 次未成功，{}s 后重试",
                attempt + 1, RETRY_GAP.as_secs());
            std::thread::sleep(RETRY_GAP);
        }
    }
    ::log::error!("[PowerToggle] 共 {} 次尝试后仍无法确认状态变化（硬件故障？）", MAX_ATTEMPTS);
    // 全部失败 → 显示红字（关机/开机分别为 Radio Off FAIL / Radio On FAIL）
    // 自动开机（is_auto=true）失败显示 30s（让用户有时间看到错误信息）
    // 手动长按（is_auto=false）失败显示 5s（用户主动操作时通常已盯着屏幕）
    let fail_text: &str = if was_alive { "Radio Off FAIL" } else { "Radio On FAIL" };
    let fail_duration_ms: u64 = if is_auto { 30000 } else { 5000 };
    set_status(&state, fail_text, StatusMsgColor::Red, fail_duration_ms);
}
