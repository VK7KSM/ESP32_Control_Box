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
use crate::state::SharedState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use esp_idf_svc::sys::*;

const BUTTON_GPIO: i32 = 2;
const POWER_GPIO: i32 = 8;
const LONG_PRESS_DURATION: Duration = Duration::from_secs(3);
const SHORT_PRESS_MIN: Duration = Duration::from_millis(50);  // 去抖下限
const POLL_INTERVAL: Duration = Duration::from_millis(20);  // 50Hz 轮询自带去抖
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

    loop {
        let pressed = unsafe { gpio_get_level(BUTTON_GPIO) } == 0;
        let now = Instant::now();

        match (pressed, press_start) {
            (true, None) => {
                press_start = Some(now);
                already_triggered_long = false;
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
                        power_toggle_worker(st);
                        release_power_lock();
                    });
                }
            }
            (false, Some(start)) => {
                // 释放：分类短按 vs 已触发的长按
                let duration = now - start;
                if !already_triggered_long
                    && duration >= SHORT_PRESS_MIN
                    && duration < LONG_PRESS_DURATION
                {
                    // === 短按：唤醒屏幕 + 重启 BLE 广播 ===
                    ::log::info!("[Button] 短按 ({} ms) → 唤醒屏幕 + 重启 BLE 广播",
                        duration.as_millis());
                    {
                        let mut s = state.lock().unwrap();
                        s.head_count = s.head_count.wrapping_add(1);
                    }
                    ble::request_advertising_restart();
                }
                // 释放后无论何种类型都重置
                press_start = None;
                already_triggered_long = false;
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
fn probe_radio_alive(state: &SharedState) -> bool {
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
fn power_toggle_worker(state: SharedState) {
    let (was_alive, body_before) = {
        let s = state.lock().unwrap();
        (s.radio_alive, s.body_count)
    };
    ::log::info!("[PowerToggle] 开始：was_alive={} body={} (期望 {})",
        was_alive, body_before,
        if was_alive { "三次探针无响应 (关机)" } else { "body_count++ (开机)" });

    for attempt in 0..MAX_ATTEMPTS {
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
            return;
        }

        if attempt < MAX_ATTEMPTS - 1 {
            ::log::warn!("[PowerToggle] 第 {} 次未成功，{}s 后重试",
                attempt + 1, RETRY_GAP.as_secs());
            std::thread::sleep(RETRY_GAP);
        }
    }
    ::log::error!("[PowerToggle] 共 {} 次尝试后仍无法确认状态变化（硬件故障？）", MAX_ATTEMPTS);
}
