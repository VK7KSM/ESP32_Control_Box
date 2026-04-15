// ===================================================================
// CLI 被动收听模式（listen 命令）
//
// 无 TUI，打滚动日志行 + 自动保存 RX 录音到 recordings/
//
// 终止方式（任意先触发）：
//   Ctrl+C     — 优雅退出，保存正在进行的录音
//   --duration — 运行时长到期
//   --count    — 录满 N 次信号
//   --idle     — 最后一次信号结束后 N 时间无新活动
//
// 录音状态机（每侧独立但共用同一 RxMonitor 音频流）：
//   Idle → Active（BUSY 出现）
//   Active → Cooling（BUSY 消失，2s 静默计时开始）
//   Cooling → Active（信号复起，取消计时）
//   Cooling → Idle（2s 超时，保存录音）
//   Active → Active（每 300s 自动分段保存）
// ===================================================================

use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

use chrono::Local;
use crossterm::style::Stylize;

use crate::cli::ListenOptions;
use crate::{audio, protocol, serial_link, state};

// ── 录音状态机 ────────────────────────────────────────────────────

enum RecState {
    Idle,
    Active {
        ts:      String,   // 录音开始时间戳（文件名用）"%Y%m%d_%H%M%S"
        start:   Instant,  // 当前片段开始时间（用于 300s 分段）
        seg_start: Instant,// 整段录音开始时间（用于倒计时打印）
        segment: u8,
        side:    String,
        freq:    String,
    },
    Cooling {
        ts:      String,
        silence_start: Instant,
        segment: u8,
        side:    String,
        freq:    String,
    },
}

// ── 公开入口 ──────────────────────────────────────────────────────

pub fn run_listen(port_name: &str, opts: ListenOptions) {
    // [1] stop_flag + Ctrl+C 注册
    let stop_flag = Arc::new(AtomicBool::new(false));
    let sf = stop_flag.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        // 避免多次触发时打印多次
        if !sf.load(Ordering::SeqCst) {
            println!();
            println!("[{}] Ctrl+C — 正在停止，保存录音...", ts_now());
            sf.store(true, Ordering::SeqCst);
        }
    }) {
        eprintln!("[警告] 无法注册 Ctrl+C 处理器: {}，Ctrl+C 将直接终止进程", e);
    }

    // [2] 打开串口，spawn RX / TX 线程
    let port = match serial_link::open_port(port_name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{} {}", " ERR".on_dark_red().white().bold(), e);
            std::process::exit(1);
        }
    };
    let shared = state::new_shared_state();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx)     = std::sync::mpsc::channel();
    let _rx = serial_link::spawn_rx_thread(port.clone(), shared.clone(), event_tx);
    let _tx = serial_link::spawn_tx_thread(port.clone(), cmd_rx);
    // ↑ TX 线程已内置 500ms 自动心跳，无需额外处理

    // [3] 等待首个状态报告（最多 5s）
    print!("[{}] 等待 ESP32 状态...", ts_now());
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let wait_start = Instant::now();
    let mut got_state = false;
    while wait_start.elapsed() < Duration::from_secs(5) {
        let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_GET_STATE, &[]));
        if let Ok(serial_link::SerialEvent::StateUpdated) =
            event_rx.recv_timeout(Duration::from_millis(200))
        {
            got_state = true;
            break;
        }
        if stop_flag.load(Ordering::SeqCst) { std::process::exit(0); }
    }
    if got_state {
        println!(" OK");
    } else {
        println!(" 未收到响应，继续...");
    }

    // [4] 初始化音频（可选）
    let host = cpal::default_host();
    let rx_monitor: Option<audio::RxMonitor> =
        match audio::find_device_by_name(&host, "USB Audio", true) {
            Some(ref dev) => match audio::RxMonitor::new(dev) {
                Ok(m) => {
                    if !opts.audio {
                        m.mute_passthrough();  // 默认不直通播放
                    }
                    println!("[{}] 音频录音就绪{}",
                        ts_now(),
                        if opts.audio { "（已开启直通播放）" } else { "" });
                    Some(m)
                }
                Err(e) => {
                    println!("[{}] {} 无法初始化音频录音: {}  — 仅显示状态",
                        ts_now(), " 警告".on_dark_yellow().white().bold(), e);
                    None
                }
            },
            None => {
                println!("[{}] {} 未找到 CM108（USB Audio）— 仅显示状态，不录音",
                    ts_now(), " 警告".on_dark_yellow().white().bold());
                None
            }
        };

    // [5] 打印启动信息
    {
        let s = shared.lock().unwrap();
        let radio_tag = if s.radio_alive {
            "电台在线".green().to_string()
        } else {
            "电台离线".red().to_string()
        };
        print!("[{}] 开始监听  {}  LEFT:{} RIGHT:{}",
            ts_now(), radio_tag, s.left.freq, s.right.freq);
        if let Some(d) = opts.duration     { print!("  时长上限:{}", fmt_dur(d)); }
        if let Some(n) = opts.count        { print!("  次数上限:{}", n); }
        if let Some(d) = opts.idle_timeout { print!("  空闲超时:{}", fmt_dur(d)); }
        println!();
    }
    println!("{}", "─".repeat(72).dark_grey());

    // [6] 状态变量
    let session_start    = Instant::now();
    let mut last_activity = Instant::now();  // 最后一次信号结束时间（--idle 用）
    let mut total_saved  = 0u32;
    let mut rec_state    = RecState::Idle;
    let mut last_left_busy  = false;
    let mut last_right_busy = false;
    let mut update_counter  = 0u32;  // 定期打印状态行（每 N 次 StateUpdated）

    // [7] 主事件循环
    loop {
        // ── 终止条件检查（每次循环顶部）──────────────────────────
        if stop_flag.load(Ordering::SeqCst) {
            finalize_and_save(&mut rec_state, &rx_monitor, &mut total_saved);
            break;
        }
        if let Some(dur) = opts.duration {
            if session_start.elapsed() >= dur {
                println!("[{}] {} 达到时长上限，自动结束",
                    ts_now(), fmt_dur(dur).yellow().bold());
                finalize_and_save(&mut rec_state, &rx_monitor, &mut total_saved);
                break;
            }
        }
        // count 在 save_recording 内部递增后立即在 Cooling → Idle 转换后检查
        if let Some(n) = opts.count {
            if total_saved >= n {
                println!("[{}] {} 已录完 {} 次信号，自动结束",
                    ts_now(), " ✓".green(), n);
                break;
            }
        }
        if let Some(idle) = opts.idle_timeout {
            // 仅在 Idle 状态（无进行中录音）时检查
            if matches!(rec_state, RecState::Idle) && last_activity.elapsed() >= idle {
                println!("[{}] {} {} 无信号活动，自动结束",
                    ts_now(), fmt_dur(idle).yellow().bold(), "空闲超时".yellow());
                break;
            }
        }

        // ── 接收状态事件（100ms 超时，保证循环响应性）────────────
        let event = event_rx.recv_timeout(Duration::from_millis(100));
        match event {
            Ok(serial_link::SerialEvent::StateUpdated) => {
                let (left_busy, right_busy, left_freq, right_freq, left_s, right_s) = {
                    let s = shared.lock().unwrap();
                    (s.left.is_busy, s.right.is_busy,
                     s.left.freq.clone(), s.right.freq.clone(),
                     s.left.s_level, s.right.s_level)
                };

                let any_busy    = left_busy || right_busy;
                let was_any     = last_left_busy || last_right_busy;
                let just_started = any_busy && !was_any;
                let just_stopped = !any_busy && was_any;

                // ── 录音状态机 ────────────────────────────────────
                let new_state = match rec_state {

                    RecState::Idle if just_started => {
                        // 信号出现 → 开始录音
                        let side = if left_busy { "LEFT" } else { "RIGHT" }.to_string();
                        let freq = if left_busy { left_freq.clone() } else { right_freq.clone() };
                        let ts   = Local::now().format("%Y%m%d_%H%M%S").to_string();
                        let s_val = if left_busy { left_s } else { right_s };
                        if let Some(ref m) = rx_monitor { m.start_recording(); }
                        println!("[{}] {} {} {}  S:{}  录音中...",
                            ts_now(), "←".cyan().bold(), side.clone().cyan(), freq.clone().yellow(), s_val);
                        last_activity = Instant::now();
                        RecState::Active {
                            ts, start: Instant::now(), seg_start: Instant::now(),
                            segment: 1, side, freq,
                        }
                    }

                    RecState::Idle => {
                        // 定期打印一次状态
                        update_counter += 1;
                        if update_counter % 50 == 0 {
                            let s = shared.lock().unwrap();
                            println!("[{}] 监听中  LEFT:{} S{}  RIGHT:{} S{}",
                                ts_now(), s.left.freq, s.left.s_level,
                                s.right.freq, s.right.s_level);
                        }
                        RecState::Idle
                    }

                    RecState::Active { ts, start, seg_start, mut segment, side, freq } => {
                        // 300s 自动分段
                        if start.elapsed() >= Duration::from_secs(300) {
                            let samples = rx_monitor.as_ref()
                                .map(|m| m.stop_recording()).unwrap_or_default();
                            let dur = samples.len() as f32
                                / rx_monitor.as_ref().map(|m| m.sample_rate()).unwrap_or(48000) as f32;
                            save_recording(&samples, &ts, &freq, segment, dur, &mut total_saved,
                                rx_monitor.as_ref().map(|m| m.sample_rate()).unwrap_or(48000));
                            segment += 1;
                            if let Some(ref m) = rx_monitor { m.start_recording(); }
                            println!("[{}] ↻ {} {} 已分段（{}）",
                                ts_now(), side.clone().cyan(), freq.clone().yellow(), format!("段#{}", segment - 1).dark_grey());
                            last_activity = Instant::now();
                            RecState::Active { ts, start: Instant::now(), seg_start, segment, side, freq }
                        } else if just_stopped {
                            // 信号消失 → 进入冷却期
                            RecState::Cooling { ts, silence_start: Instant::now(), segment, side, freq }
                        } else {
                            last_activity = Instant::now();
                            RecState::Active { ts, start, seg_start, segment, side, freq }
                        }
                    }

                    RecState::Cooling { ts, silence_start, segment, side, freq } => {
                        if just_started {
                            // 信号复起 → 继续录音，取消冷却
                            let s_val = if left_busy { left_s } else { right_s };
                            println!("[{}] ← {} {} 信号续发  S:{}",
                                ts_now(), side.clone().cyan(), freq.clone().yellow(), s_val);
                            last_activity = Instant::now();
                            RecState::Active {
                                ts, start: Instant::now(), seg_start: Instant::now(),
                                segment, side, freq,
                            }
                        } else if silence_start.elapsed() >= Duration::from_secs(2) {
                            // 2s 静默超时 → 保存录音
                            let samples = rx_monitor.as_ref()
                                .map(|m| m.stop_recording()).unwrap_or_default();
                            let sample_rate = rx_monitor.as_ref()
                                .map(|m| m.sample_rate()).unwrap_or(48000);
                            let dur = samples.len() as f32 / sample_rate as f32;
                            if dur >= 0.3 {
                                save_recording(&samples, &ts, &freq, segment, dur,
                                    &mut total_saved, sample_rate);
                            } else {
                                println!("[{}] {} 信号过短 ({:.1}s)，未保存",
                                    ts_now(), "✗".dark_grey(), dur);
                            }
                            RecState::Idle
                        } else {
                            RecState::Cooling { ts, silence_start, segment, side, freq }
                        }
                    }
                };

                rec_state = new_state;
                last_left_busy  = left_busy;
                last_right_busy = right_busy;
            }

            Ok(serial_link::SerialEvent::Error(msg)) => {
                // 过滤 PTT 超时（正常保护行为）
                if msg.trim() != "PTT timeout" {
                    println!("[{}] {} ESP32: {}", ts_now(), " ERR".on_dark_red().white().bold(), msg);
                }
            }

            Ok(serial_link::SerialEvent::Disconnected) => {
                println!("[{}] {} 串口断开，退出监听",
                    ts_now(), " 断开".on_dark_red().white().bold());
                finalize_and_save(&mut rec_state, &rx_monitor, &mut total_saved);
                print_summary(session_start, total_saved);
                std::process::exit(1);
            }

            _ => {}  // Timeout 或其他事件，继续循环
        }
    }

    print_summary(session_start, total_saved);
    std::process::exit(0);
}

// ── 辅助函数 ─────────────────────────────────────────────────────

/// 保存录音文件，更新计数，打印日志
fn save_recording(
    samples: &[f32],
    ts: &str,
    freq: &str,
    segment: u8,
    dur: f32,
    count: &mut u32,
    sample_rate: u32,
) {
    // 与 TUI 一致的文件名格式
    let freq_safe = freq.replace('.', "_");
    let path = format!("recordings/RX_{}_{}_{:.0}s_seg{}.wav",
        ts, freq_safe, dur, segment);

    match audio::save_wav_48k(samples, sample_rate, &path) {
        Ok(()) => {
            *count += 1;
            println!("[{}] {} [{:04}] 已保存: {}  ({:.1}s)",
                ts_now(),
                "✓".green().bold(),
                count,
                path.as_str().cyan(),
                dur);
        }
        Err(e) => {
            println!("[{}] {} 录音保存失败: {}", ts_now(), "✗".red(), e);
        }
    }
}

/// 退出前保存正在进行的录音
fn finalize_and_save(
    rec_state: &mut RecState,
    rx_monitor: &Option<audio::RxMonitor>,
    count: &mut u32,
) {
    let state = std::mem::replace(rec_state, RecState::Idle);
    match state {
        RecState::Active { ts, freq, segment, .. }
        | RecState::Cooling { ts, freq, segment, .. } => {
            let samples = rx_monitor.as_ref()
                .map(|m| m.stop_recording()).unwrap_or_default();
            let sample_rate = rx_monitor.as_ref()
                .map(|m| m.sample_rate()).unwrap_or(48000);
            let dur = samples.len() as f32 / sample_rate as f32;
            if dur >= 0.3 {
                println!("[{}] 保存退出时录音 ({:.1}s)...", ts_now(), dur);
                save_recording(&samples, &ts, &freq, segment, dur, count, sample_rate);
            }
        }
        RecState::Idle => {}
    }
}

/// 打印最终统计行
fn print_summary(start: Instant, total: u32) {
    println!("{}", "─".repeat(72).dark_grey());
    println!("[{}] 监听结束  运行时间:{}  共保存录音:{} 次",
        ts_now(),
        format!(" {} ", fmt_dur(start.elapsed())).on_dark_blue().white().bold(),
        format!(" {} ", total).on_dark_green().white().bold());
}

/// 当前时间字符串（用于日志前缀）
fn ts_now() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

/// 格式化时长（人性化）
fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        if m > 0 { format!("{}h{}m", h, m) } else { format!("{}h", h) }
    } else if s >= 60 {
        let m = s / 60;
        let rem = s % 60;
        if rem > 0 { format!("{}m{}s", m, rem) } else { format!("{}m", m) }
    } else {
        format!("{}s", s)
    }
}
