// ===================================================================
// 监听模式 — 组合键直接操作，无二级菜单
//
// 单键: M=MAIN  P=PTT  O=开关机  L/R=频率输入  ↑↓=旋钮
// 组合: V+←→=音量  Q+←→=静噪  W+↑↓=功率  T+↑↓=亚音
// 消息区: RX/TX 通信日志（卡片式），操作反馈→底部状态栏通知
// ===================================================================

use crossterm::{
    cursor, terminal, execute,
    event::{self, Event, KeyCode, KeyEventKind, MouseEvent, MouseEventKind, MouseButton,
            EnableMouseCapture, DisableMouseCapture},
    style::Stylize,
};
use crate::tts;
use std::io::{self, Write};
use std::sync::mpsc;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};
use crate::protocol;
use crate::serial_link::{self, SerialEvent};
use crate::state::SharedState;
use crate::audio;

enum Mode {
    Idle,
    FreqInput { side: u8, input: String },
    PowerConfirm,
}

#[derive(Clone, PartialEq)]
enum CardKind { Rx, Tx }

/// 活跃通信卡片（RX 或 TX），跟踪进行中的通信并原地更新状态行
struct ActiveCard {
    count: u16,         // 在 card_history 中的唯一 ID
    start_time: Instant,
    kind: CardKind,
    last_elapsed: u64,  // 上次显示的秒数（防止无意义重绘）
    side: String,
    freq: String,
    power: String,
    tone: String,
    segment: u8,        // RX 分段编号（1起始；TX=0）
    card_ts: String,    // 创建时间戳（用于文件名 %Y%m%d_%H%M%S）
}

/// 历史卡片记录，用于虚拟滚动重绘（不存储行号，行号由滚动状态动态计算）
struct CardRecord {
    count: u16,
    kind: CardKind,
    side: String,
    ts_display: String,   // 顶行时间戳 "HH:MM:SS"
    freq: String,
    power: String,
    tone: String,
    status_line: String,  // 行2 内容（动态更新）
}

/// 跨连接持久化状态（断开重连后保留消息历史和滚动位置）
pub struct MonitorPersistentState {
    pub card_history:  Vec<CardRecord>,
    pub scroll_offset: usize,
    pub notification:  Option<(String, Instant, bool)>,
    pub card_count:    u16,
    pub vol_target:    i32,   // -1 = 未初始化，0-100 = 百分比
    pub sql_target:    i32,
    // TTS 输入框
    pub tts_text:      String,  // 输入框当前内容
    pub tts_cursor:    usize,   // 光标位置（字符索引）
    pub tts_focused:   bool,    // 输入框是否有焦点
    // 降噪
    pub denoise_db:    f32,     // 0.0 = 关闭，10-100 = 强度
    // 配置（从 elfradio-box.cfg 加载）
    pub tts_voice:     String,
}

impl MonitorPersistentState {
    pub fn new() -> Self {
        let cfg = crate::config::load_config();
        Self {
            card_history:  Vec::new(),
            scroll_offset: 0,
            notification:  None,
            card_count:    0,
            vol_target:    -1,
            sql_target:    -1,
            tts_text:      String::new(),
            tts_cursor:    0,
            tts_focused:   false,
            denoise_db:    cfg.denoise_db,
            tts_voice:     cfg.tts_voice,
        }
    }
}

/// 内层事件循环的退出原因
enum LoopExitReason {
    UserEsc,
    SerialDisconnect,
}

// ===== 公开接口 =====

/// 持久监听模式（直接 CLI 或双击启动调用）。
/// TUI 生命周期与串口会话分离：TUI 常驻，串口断开后自动重连。
pub fn run_monitor_persistent(port_name: &str, shared: SharedState) {
    let mut stdout = io::stdout();
    let _ = terminal::enable_raw_mode();
    let _ = execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide, EnableMouseCapture);

    let mut pstate = MonitorPersistentState::new();

    // 初始 UI（空状态 + 等待通知）
    let (tw, th) = terminal::size().unwrap_or((80, 24));
    draw_title_bar(tw, &shared, None, false);
    draw_separator(tw);
    draw_message_area(&pstate.card_history, 0, tw, th);
    pstate.notification = Some(("等待控制盒连接...".to_string(), Instant::now(), true));
    draw_bottom_bar(tw, th, &Mode::Idle, &shared, &pstate);

    'outer: loop {
        match serial_link::open_port(port_name) {
            Ok(port) => {
                // 先重置共享状态，再绘制 UI（draw_title_bar 读取 radio_alive，必须先清零）
                {
                    let mut s = shared.lock().unwrap();
                    s.radio_alive = false;
                    s.pc_alive    = false;
                }
                let (event_tx, event_rx) = mpsc::channel::<SerialEvent>();
                let (cmd_tx, cmd_rx)     = mpsc::channel::<Vec<u8>>();
                let _rx = serial_link::spawn_rx_thread(port.clone(), shared.clone(), event_tx);
                let _tx = serial_link::spawn_tx_thread(port.clone(), cmd_rx);

                pstate.notification = Some(("控制盒已连接".to_string(), Instant::now(), false));

                // 重绘全 UI（保留历史消息 + 新通知）
                let (tw2, th2) = terminal::size().unwrap_or((80, 24));
                draw_title_bar(tw2, &shared, None, has_scroll_hint(th2, &pstate.card_history));
                draw_separator(tw2);
                draw_message_area(&pstate.card_history, pstate.scroll_offset, tw2, th2);
                draw_bottom_bar(tw2, th2, &Mode::Idle, &shared, &pstate);

                match run_monitor_loop(&mut pstate, &shared, &cmd_tx, &event_rx) {
                    LoopExitReason::UserEsc => {
                        drop(cmd_tx);
                        // 清空 crossterm 事件缓冲区，防止退出 TUI 瞬间的按键残留到主菜单 stdin
                        while event::poll(Duration::ZERO).unwrap_or(false) {
                            let _ = event::read();
                        }
                        break 'outer;
                    }
                    LoopExitReason::SerialDisconnect => {
                        drop(cmd_tx);
                        // TX 线程因 cmd_rx 关闭自动退出，RX 线程因断连已退出
                        // 先重置状态，再全量清屏重绘（防止旧内容残留 + 位置错位导致双行）
                        {
                            let mut s = shared.lock().unwrap();
                            s.radio_alive = false;
                            s.pc_alive    = false;
                        }
                        pstate.notification =
                            Some(("控制盒已断开，等待重连...".to_string(), Instant::now(), true));
                        let (tw3, th3) = terminal::size().unwrap_or((80, 24));
                        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::All),
                                         cursor::MoveTo(0, 0));
                        draw_title_bar(tw3, &shared, None, has_scroll_hint(th3, &pstate.card_history));
                        draw_separator(tw3);
                        draw_message_area(&pstate.card_history, pstate.scroll_offset, tw3, th3);
                        draw_bottom_bar(tw3, th3, &Mode::Idle, &shared, &pstate);
                        let _ = stdout.flush();
                        // continue 回到外层 loop，再次尝试同一端口
                    }
                }
                // 等待旧线程退出、OS 释放串口句柄，再重试
                std::thread::sleep(Duration::from_millis(500));
            }

            Err(_) => {
                // 串口不可用（ESP32 断开或重启中）
                let (tw2, th2) = terminal::size().unwrap_or((80, 24));
                // 保留 "等待重连" 消息，仅在其他消息时更新为 "等待控制盒连接..."
                let need_update = pstate.notification.as_ref()
                    .map_or(true, |(msg, _, _)| {
                        !msg.contains("等待控制盒") && !msg.contains("等待重连")
                    });
                if need_update {
                    pstate.notification =
                        Some(("等待控制盒连接...".to_string(), Instant::now(), true));
                    draw_bottom_bar(tw2, th2, &Mode::Idle, &shared, &pstate);
                }
                // 轮询键盘 200ms，允许 Esc 退出；用 poll(ZERO) 非阻塞消费所有就绪事件
                if event::poll(Duration::from_millis(200)).unwrap_or(false) {
                    while event::poll(Duration::ZERO).unwrap_or(false) {
                        match event::read() {
                            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press
                                              && k.code == KeyCode::Esc => {
                                break 'outer;
                            }
                            Ok(Event::Resize(_, _)) => {
                                // Resize：全量清屏重绘（与 run_monitor_loop resize 处理器一致）
                                let (tw_r, th_r) = terminal::size().unwrap_or((80, 24));
                                let _ = execute!(stdout,
                                    terminal::Clear(terminal::ClearType::All),
                                    cursor::MoveTo(0, 0));
                                draw_title_bar(tw_r, &shared, None,
                                    has_scroll_hint(th_r, &pstate.card_history));
                                draw_separator(tw_r);
                                draw_message_area(&pstate.card_history,
                                    pstate.scroll_offset, tw_r, th_r);
                                draw_bottom_bar(tw_r, th_r, &Mode::Idle,
                                    &shared, &pstate);
                                let _ = stdout.flush();
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    let _ = terminal::disable_raw_mode();
    let _ = execute!(stdout, DisableMouseCapture, cursor::Show, terminal::LeaveAlternateScreen);
}

// ===== 内层事件循环 =====

fn run_monitor_loop(
    pstate:    &mut MonitorPersistentState,
    shared:    &SharedState,
    cmd_tx:    &mpsc::Sender<Vec<u8>>,
    serial_rx: &mpsc::Receiver<SerialEvent>,
) -> LoopExitReason {
    let mut disconnected = false;
    let mut stdout = io::stdout();

    let (mut tw, mut th) = terminal::size().unwrap_or((80, 24));
    let mut mode = Mode::Idle;

    // 组合键修饰状态（mod_v 已移除，←→ 直接控制音量）
    let mut mod_q = false;
    let mut mod_w = false;
    let mut mod_t = false;
    let mut mod_n = false;  // N+←→:降噪

    // PTT 长按状态
    let mut ptt_press_time: Option<Instant> = None;
    let mut ptt_active = false;
    let mut ptt_start: Option<Instant> = None;
    // PTT TX 音频路由（PC 麦克风 → CM108 输出 → 电台 PIN6），Drop 即停止
    let mut tx_capture: Option<audio::TxMicCapture> = None;
    // 文件发射状态（F键）
    let mut file_ptt_stop: Option<Arc<AtomicBool>> = None;
    let mut file_ptt_rx: Option<mpsc::Receiver<Result<Duration, String>>> = None;
    // TTS 发射状态（Tab+输入+Enter）
    let mut tts_stage_rx:  Option<mpsc::Receiver<Result<std::path::PathBuf, String>>> = None;
    let mut tts_ptt_stop:  Option<Arc<AtomicBool>> = None;
    let mut tts_ptt_rx:    Option<mpsc::Receiver<Result<Duration, String>>> = None;

    // 音频监听（CM108 USB Audio INPUT 接电台扬声器输出）
    // RxMonitor 内置 passthrough：CM108 Input → 用户耳机/扬声器（RX 时收听）
    let mut rx_monitor: Option<audio::RxMonitor> = None;
    let cpal_host = cpal::default_host();
    if let Some(dev) = audio::find_device_by_name(&cpal_host, "USB Audio", true) {
        if let Ok(mon) = audio::RxMonitor::new(&dev) {
            rx_monitor = Some(mon);
        }
    }

    // BUSY 侧别跟踪
    let mut last_left_busy  = false;
    let mut last_right_busy = false;
    // 静默计时：所有侧停止接收后 2s 保存录音并完成卡片
    let mut silence_start: Option<Instant> = None;

    // 通信日志
    let mut active_card: Option<ActiveCard> = None;

    // 开关机状态跟踪
    let mut power_toggle_expected: Option<bool> = None;

    // UI 初始化（使用 pstate 中保留的历史和通知）
    draw_title_bar(tw, &shared, None, has_scroll_hint(th, &pstate.card_history));
    draw_separator(tw);
    draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
    draw_bottom_bar(tw, th, &mode, &shared, pstate);

    // MAIN 侧探测：发送一次 P1 键（与 ESP32 固件的探测配合，合计2次P1净效果为零）
    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_PRESS, &[0x10]));
    std::thread::sleep(Duration::from_millis(150));
    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_REL, &[]));

    loop {
        // ===== 文件发射完成检测（每帧检查）=====
        if let Some(ref rx) = file_ptt_rx {
            match rx.try_recv() {
                Ok(result) => {
                    // 文件播放完成（或失败）—— PTT=0 由此处发送（不再由后台线程负责）
                    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
                    let content = match result {
                        Ok(dur)  => format!("│  OK  文件发射完成 ({:.1}s)", dur.as_secs_f32()),
                        Err(ref e) => format!("│  --  文件发射失败: {}", e),
                    };
                    if let Some(ref card) = active_card {
                        update_card_by_count(&mut pstate.card_history, card.count,
                                             pstate.scroll_offset, tw, th, &content);
                    }
                    active_card   = None;
                    file_ptt_rx   = None;
                    file_ptt_stop = None;
                    ptt_active    = false;
                    ptt_start     = None;
                    if let Some(ref mon) = rx_monitor { mon.unmute_passthrough(); }
                    let spg = has_scroll_hint(th, &pstate.card_history);
                    draw_title_bar(tw, &shared, None, spg);
                    draw_bottom_bar(tw, th, &mode, &shared, pstate);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // 后台线程意外退出，强制清理 PTT
                    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
                    if let Some(ref card) = active_card {
                        update_card_by_count(&mut pstate.card_history, card.count,
                                             pstate.scroll_offset, tw, th, "│  --  文件发射线程异常退出");
                    }
                    active_card   = None;
                    file_ptt_rx   = None;
                    file_ptt_stop = None;
                    ptt_active    = false;
                    ptt_start     = None;
                    if let Some(ref mon) = rx_monitor { mon.unmute_passthrough(); }
                    let spg = has_scroll_hint(th, &pstate.card_history);
                    draw_title_bar(tw, &shared, None, spg);
                }
                Err(mpsc::TryRecvError::Empty) => {} // 还在播放中
            }
        }

        // ===== TTS 合成完成检测：触发 PTT 播放 =====
        if let Some(ref rx) = tts_stage_rx {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    tts_stage_rx = None;
                    pstate.notification = Some(("TTS 发射中...".to_string(), Instant::now(), true));
                    // 发 PTT=1
                    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[1]));
                    ptt_active = true;
                    ptt_start  = Some(Instant::now());
                    if let Some(ref mon) = rx_monitor { mon.mute_passthrough(); }

                    // 创建 TX 卡片
                    let s = shared.lock().unwrap();
                    let (t_side, t_freq, t_power, t_tone) = if s.left_main {
                        ("LEFT".to_string(), s.left.freq.clone(), s.left.power.clone(), s.left.tone_str().to_string())
                    } else if s.right_main {
                        ("RIGHT".to_string(), s.right.freq.clone(), s.right.power.clone(), s.right.tone_str().to_string())
                    } else {
                        ("MAIN?".to_string(), s.left.freq.clone(), s.left.power.clone(), s.left.tone_str().to_string())
                    };
                    drop(s);
                    let ts  = chrono::Local::now().format("%H:%M:%S").to_string();
                    let tsf = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
                    pstate.card_count += 1;
                    let cnt = pstate.card_count;
                    pstate.card_history.push(CardRecord {
                        count: cnt, kind: CardKind::Tx,
                        side: t_side.clone(), ts_display: ts,
                        freq: t_freq.clone(), power: t_power.clone(),
                        tone: t_tone.clone(),
                        status_line: format!("│ {}  TTS发射中...  00:00",
                            " TX ".on_dark_red().white().bold()),
                    });
                    active_card = Some(ActiveCard {
                        count: cnt, start_time: Instant::now(),
                        kind: CardKind::Tx, last_elapsed: u64::MAX,
                        side: t_side, freq: t_freq, power: t_power, tone: t_tone,
                        segment: 0, card_ts: tsf,
                    });
                    pstate.scroll_offset = 0;
                    draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
                    draw_title_bar(tw, &shared, ptt_start, has_scroll_hint(th, &pstate.card_history));

                    // spawn 播放线程
                    let path_str = path.to_string_lossy().to_string();
                    let stop = Arc::new(AtomicBool::new(false));
                    let stop2 = stop.clone();
                    let (done_tx, done_rx) = mpsc::channel();
                    std::thread::spawn(move || {
                        for _ in 0..20 { // 20×50ms = 1s，可被 stop_flag 中断
                            if stop2.load(Ordering::Relaxed) {
                                let _ = done_tx.send(Ok(Duration::ZERO));
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        let r = audio::play_audio_file_to_cm108(&path_str, 30, stop2);
                        let _ = done_tx.send(r);
                    });
                    tts_ptt_stop = Some(stop);
                    tts_ptt_rx   = Some(done_rx);
                }
                Ok(Err(e)) => {
                    tts_stage_rx = None;
                    pstate.notification = Some((format!("TTS 失败: {}", e), Instant::now(), false));
                    draw_bottom_bar(tw, th, &mode, &shared, pstate);
                }
                Err(mpsc::TryRecvError::Empty) => {} // 合成中
                Err(mpsc::TryRecvError::Disconnected) => {
                    tts_stage_rx = None;
                    pstate.notification = Some(("TTS 合成线程异常退出".to_string(), Instant::now(), false));
                    draw_bottom_bar(tw, th, &mode, &shared, pstate);
                }
            }
        }

        // ===== TTS 播放完成检测 =====
        if let Some(ref rx) = tts_ptt_rx {
            match rx.try_recv() {
                Ok(result) => {
                    let content = match result {
                        Ok(dur)    => format!("│  OK  TTS发射完成 ({:.1}s)", dur.as_secs_f32()),
                        Err(ref e) => format!("│  --  TTS发射失败: {}", e),
                    };
                    if let Some(ref card) = active_card {
                        update_card_by_count(&mut pstate.card_history, card.count,
                                             pstate.scroll_offset, tw, th, &content);
                    }
                    active_card  = None;
                    tts_ptt_rx   = None;
                    tts_ptt_stop = None;
                    ptt_active   = false;
                    ptt_start    = None;
                    pstate.notification = None;
                    if let Some(ref mon) = rx_monitor { mon.unmute_passthrough(); }
                    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
                    draw_title_bar(tw, &shared, None, has_scroll_hint(th, &pstate.card_history));
                    draw_bottom_bar(tw, th, &mode, &shared, pstate);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
                    if let Some(ref card) = active_card {
                        update_card_by_count(&mut pstate.card_history, card.count,
                                             pstate.scroll_offset, tw, th, "│  --  TTS播放线程异常退出");
                    }
                    active_card  = None;
                    tts_ptt_rx   = None;
                    tts_ptt_stop = None;
                    ptt_active   = false;
                    ptt_start    = None;
                    if let Some(ref mon) = rx_monitor { mon.unmute_passthrough(); }
                    draw_title_bar(tw, &shared, None, has_scroll_hint(th, &pstate.card_history));
                }
                Err(mpsc::TryRecvError::Empty) => {} // 播放中
            }
        }

        // ===== 串口事件 =====
        while let Ok(evt) = serial_rx.try_recv() {
            match evt {
                SerialEvent::StateUpdated => {
                    let s = shared.lock().unwrap();

                    let left_busy  = s.left.is_busy;
                    let right_busy = s.right.is_busy;
                    let radio_alive = s.radio_alive;

                    // 各侧跳变检测
                    let left_started  = left_busy  && !last_left_busy;
                    let right_started = right_busy && !last_right_busy;
                    let was_any_busy  = last_left_busy || last_right_busy;
                    let now_any_busy  = left_busy || right_busy;

                    // 新 RX 信号开始
                    let new_rx = (left_started || right_started) && !ptt_active;
                    let (rx_side, rx_freq, rx_power, rx_tone) = if new_rx {
                        if left_started {
                            ("LEFT".to_string(), s.left.freq.clone(),
                             s.left.power.clone(), s.left.tone_str().to_string())
                        } else {
                            ("RIGHT".to_string(), s.right.freq.clone(),
                             s.right.power.clone(), s.right.tone_str().to_string())
                        }
                    } else {
                        (String::new(), String::new(), String::new(), String::new())
                    };

                    last_left_busy  = left_busy;
                    last_right_busy = right_busy;
                    drop(s);

                    // 开关机状态确认检测
                    if let Some(expected) = power_toggle_expected {
                        if radio_alive == expected {
                            let confirmed_msg = if expected { "电台已开机" } else { "电台已关机" };
                            pstate.notification = Some((confirmed_msg.to_string(), Instant::now(), false));
                            power_toggle_expected = None;
                        }
                    }

                    if new_rx {
                        // 若有录音在进行，先停止旧录音并完成旧卡片
                        if let Some(ref mon) = rx_monitor {
                            if mon.is_recording() {
                                let samples = mon.stop_recording();
                                let dur = samples.len() as f32 / mon.sample_rate() as f32;
                                if let Some(ref card) = active_card {
                                    let content = finalize_recording_content(
                                        &card.kind, &card.card_ts, &card.freq,
                                        &card.segment, mon.sample_rate(), &samples, dur);
                                    update_card_by_count(
                                        &mut pstate.card_history, card.count,
                                        pstate.scroll_offset, tw, th, &content);
                                }
                            }
                        }
                        active_card = None;
                        silence_start = None;

                        // 开始新录音
                        if let Some(ref mon) = rx_monitor {
                            mon.start_recording();
                        }

                        // 创建 RX 卡片（无高度限制，全部存入历史）
                        let ts  = chrono::Local::now().format("%H:%M:%S").to_string();
                        let tsf = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
                        pstate.card_count += 1;
                        let cnt = pstate.card_count;
                        let init_status = format!("│ {}  接收中...  00:00",
                            " RX ".on_dark_blue().white().bold());
                        pstate.card_history.push(CardRecord {
                            count: cnt, kind: CardKind::Rx,
                            side: rx_side.clone(), ts_display: ts.clone(),
                            freq: rx_freq.clone(), power: rx_power.clone(),
                            tone: rx_tone.clone(), status_line: init_status,
                        });
                        active_card = Some(ActiveCard {
                            count: cnt,
                            start_time: Instant::now(),
                            kind: CardKind::Rx,
                            last_elapsed: u64::MAX,
                            side: rx_side, freq: rx_freq, power: rx_power,
                            tone: rx_tone, segment: 1, card_ts: tsf,
                        });
                        pstate.scroll_offset = 0;  // 自动回到最新
                        draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
                    }

                    // 所有侧停止接收：启动静默计时
                    if was_any_busy && !now_any_busy && !ptt_active {
                        silence_start = Some(Instant::now());
                    }
                    if now_any_busy {
                        silence_start = None;
                    }

                    // 活跃卡片状态行每秒更新
                    if let Some(ref mut card) = active_card {
                        let elapsed = card.start_time.elapsed().as_secs();
                        if elapsed != card.last_elapsed {
                            card.last_elapsed = elapsed;
                            let mins = elapsed / 60;
                            let secs_d = elapsed % 60;
                            let content = match card.kind {
                                CardKind::Rx => format!("│ {}  接收中...  {:02}:{:02}",
                                    " RX ".on_dark_blue().white().bold(), mins, secs_d),
                                CardKind::Tx => format!("│ {}  发射中...  {:02}:{:02}",
                                    " TX ".on_dark_red().white().bold(), mins, secs_d),
                            };
                            update_card_by_count(
                                &mut pstate.card_history, card.count,
                                pstate.scroll_offset, tw, th, &content);

                            // RX 300s 分段
                            if card.kind == CardKind::Rx && elapsed >= 300 {
                                if let Some(ref mon) = rx_monitor {
                                    let samples = mon.stop_recording();
                                    let dur = samples.len() as f32 / mon.sample_rate() as f32;
                                    let seg_content = finalize_recording_content(
                                        &CardKind::Rx, &card.card_ts, &card.freq,
                                        &card.segment, mon.sample_rate(), &samples, dur);
                                    let seg_line = format!("│ [段#{}] {}",
                                        card.segment,
                                        seg_content.trim_start_matches('│').trim_start());
                                    update_card_by_count(
                                        &mut pstate.card_history, card.count,
                                        pstate.scroll_offset, tw, th, &seg_line);
                                    mon.start_recording();
                                }
                                // 新建下一段卡片
                                card.segment += 1;
                                let seg = card.segment;
                                let ts2  = chrono::Local::now().format("%H:%M:%S").to_string();
                                let tsf2 = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
                                let side2  = card.side.clone();
                                let freq2  = card.freq.clone();
                                let power2 = card.power.clone();
                                let tone2  = card.tone.clone();
                                let header = format!("RX[段#{}]", seg);
                                pstate.card_count += 1;
                                let cnt2 = pstate.card_count;
                                let init2 = format!("│ {}  接收中...  00:00",
                                    " RX ".on_dark_blue().white().bold());
                                pstate.card_history.push(CardRecord {
                                    count: cnt2, kind: CardKind::Rx,
                                    side: side2.clone(), ts_display: ts2.clone(),
                                    freq: freq2.clone(), power: power2.clone(),
                                    tone: tone2.clone(), status_line: init2,
                                });
                                *card = ActiveCard {
                                    count: cnt2,
                                    start_time: Instant::now(),
                                    kind: CardKind::Rx,
                                    last_elapsed: u64::MAX,
                                    side: side2, freq: freq2, power: power2, tone: tone2,
                                    segment: seg, card_ts: tsf2,
                                };
                                pstate.scroll_offset = 0;
                                draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
                                // draw_card_frame 已在 draw_message_area 中完成
                                let _ = header; // 消除 unused 警告（header 已用于 push 前构造）
                            }
                        }
                    }

                    draw_title_bar(tw, &shared, ptt_start, has_scroll_hint(th, &pstate.card_history));
                    draw_bottom_bar(tw, th, &mode, &shared, pstate);
                }

                SerialEvent::Error(msg) => {
                    // "PTT timeout" 是 ESP32 看门狗正常保护，不在消息区显示
                    if msg.trim() != "PTT timeout" {
                        // 错误信息显示为通知（不占用消息区卡片行）
                        pstate.notification = Some((msg.clone(), Instant::now(), false));
                        draw_bottom_bar(tw, th, &mode, &shared, pstate);
                    }
                }

                SerialEvent::MacroDone(r) => {
                    let msg = match r { 0 => "宏完成", 1 => "宏超时", 2 => "宏中止", _ => "宏失败" };
                    pstate.notification = Some((msg.to_string(), Instant::now(), false));
                    draw_bottom_bar(tw, th, &mode, &shared, pstate);
                }

                SerialEvent::Disconnected => {
                    disconnected = true;
                    break;
                }
                _ => {}
            }
        }

        // ===== 串口断开：返回给调用者处理重连 =====
        if disconnected {
            return LoopExitReason::SerialDisconnect;
        }

        // ===== RX 静默超时（2s 后保存录音并完成卡片）=====
        if let Some(start) = silence_start {
            if start.elapsed() > Duration::from_secs(2) {
                if let Some(ref mon) = rx_monitor {
                    if mon.is_recording() {
                        let samples = mon.stop_recording();
                        let dur = samples.len() as f32 / mon.sample_rate() as f32;
                        if let Some(ref card) = active_card {
                            let content = finalize_recording_content(
                                &card.kind, &card.card_ts, &card.freq,
                                &card.segment, mon.sample_rate(), &samples, dur);
                            update_card_by_count(
                                &mut pstate.card_history, card.count,
                                pstate.scroll_offset, tw, th, &content);
                        }
                    }
                }
                active_card = None;
                silence_start = None;
            }
        }

        // ===== PTT 长按延迟启动（300ms 后发射）=====
        if let Some(t) = ptt_press_time {
            if !ptt_active && t.elapsed() >= Duration::from_millis(300) {
                // 边缘保护：先停止任何进行中的 RX 录音并完成卡片
                if let Some(ref mon) = rx_monitor {
                    if mon.is_recording() {
                        let samples = mon.stop_recording();
                        let dur = samples.len() as f32 / mon.sample_rate() as f32;
                        if let Some(ref card) = active_card {
                            let content = finalize_recording_content(
                                &card.kind, &card.card_ts, &card.freq,
                                &card.segment, mon.sample_rate(), &samples, dur);
                            update_card_by_count(
                                &mut pstate.card_history, card.count,
                                pstate.scroll_offset, tw, th, &content);
                        }
                    }
                }
                active_card = None;
                silence_start = None;

                // 开始 TX 录音
                if let Some(ref mon) = rx_monitor {
                    mon.start_recording();
                }

                let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[1]));
                ptt_active = true;
                ptt_start = Some(Instant::now());

                // 静音 RX passthrough（防止 PC 扬声器被 PC 麦克风拾取形成回路）
                if let Some(ref mon) = rx_monitor { mon.mute_passthrough(); }
                // 启动 PC 麦克风 → CM108 输出路由
                tx_capture = audio::TxMicCapture::new().ok();

                // 创建 TX 卡片（快照 MAIN 侧信息）
                // MAIN 未知时显示"MAIN?"，不猜测错误侧别
                let s = shared.lock().unwrap();
                let (side, tx_freq, tx_power, tx_tone) = if s.left_main {
                    ("LEFT".to_string(), s.left.freq.clone(), s.left.power.clone(), s.left.tone_str().to_string())
                } else if s.right_main {
                    ("RIGHT".to_string(), s.right.freq.clone(), s.right.power.clone(), s.right.tone_str().to_string())
                } else {
                    ("MAIN?".to_string(), s.left.freq.clone(), s.left.power.clone(), s.left.tone_str().to_string())
                };
                drop(s);
                let ts  = chrono::Local::now().format("%H:%M:%S").to_string();
                let tsf = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
                pstate.card_count += 1;
                let cnt = pstate.card_count;
                let init_status = format!("│ {}  发射中...  00:00",
                    " TX ".on_dark_red().white().bold());
                pstate.card_history.push(CardRecord {
                    count: cnt, kind: CardKind::Tx,
                    side: side.clone(), ts_display: ts.clone(),
                    freq: tx_freq.clone(), power: tx_power.clone(),
                    tone: tx_tone.clone(), status_line: init_status,
                });
                active_card = Some(ActiveCard {
                    count: cnt,
                    start_time: Instant::now(),
                    kind: CardKind::Tx,
                    last_elapsed: u64::MAX,
                    side, freq: tx_freq, power: tx_power, tone: tx_tone,
                    segment: 0, card_ts: tsf,
                });
                pstate.scroll_offset = 0;
                draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
                draw_title_bar(tw, &shared, ptt_start, has_scroll_hint(th, &pstate.card_history));
            }
        }

        // ===== PTT 30 秒看门狗（与 ESP32 PTT_TIMEOUT_US=30s 同步）=====
        if ptt_active {
            if let Some(start) = ptt_start {
                if start.elapsed() >= Duration::from_secs(30) {
                    // 普通PTT：保存麦克风录音并更新卡片
                    let mut card_updated = false;
                    if let Some(ref mon) = rx_monitor {
                        if mon.is_recording() {
                            let samples = mon.stop_recording();
                            let dur = samples.len() as f32 / mon.sample_rate() as f32;
                            if let Some(ref card) = active_card {
                                let content = finalize_recording_content(
                                    &card.kind, &card.card_ts, &card.freq,
                                    &card.segment, mon.sample_rate(), &samples, dur);
                                update_card_by_count(
                                    &mut pstate.card_history, card.count,
                                    pstate.scroll_offset, tw, th, &content);
                                card_updated = true;
                            }
                        }
                    }

                    // 文件发射或无录音PTT：显示超时中止提示
                    if !card_updated {
                        if let Some(ref card) = active_card {
                            update_card_by_count(
                                &mut pstate.card_history, card.count,
                                pstate.scroll_offset, tw, th,
                                "│  !!  PTT超时自动中止（30s看门狗）");
                        }
                    }

                    active_card = None;

                    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
                    // 停止 PC 麦克风 → CM108 路由，恢复 RX passthrough
                    tx_capture = None;
                    // 通知文件/TTS 播放线程停止（若有）
                    if let Some(ref stop) = file_ptt_stop {
                        stop.store(true, Ordering::Relaxed);
                    }
                    file_ptt_stop = None;
                    file_ptt_rx   = None;
                    if let Some(ref stop) = tts_ptt_stop {
                        stop.store(true, Ordering::Relaxed);
                    }
                    tts_ptt_stop = None;
                    tts_ptt_rx   = None;
                    if let Some(ref mon) = rx_monitor { mon.unmute_passthrough(); }
                    ptt_active = false;
                    ptt_start = None;
                    ptt_press_time = None;
                    draw_title_bar(tw, &shared, None, has_scroll_hint(th, &pstate.card_history));
                    draw_bottom_bar(tw, th, &mode, &shared, pstate);
                }
            }
        }

        // ===== 键盘/终端事件 =====
        if !event::poll(Duration::from_millis(50)).unwrap_or(false) { continue; }
        let raw_evt = match event::read() { Ok(e) => e, _ => continue };

        // 终端 resize：清屏重绘（所有行号从 tw/th 重新计算）
        if let Event::Resize(new_w, new_h) = raw_evt {
            tw = new_w; th = new_h;
            let _ = execute!(io::stdout(), terminal::Clear(terminal::ClearType::All));
            draw_title_bar(tw, &shared, ptt_start, has_scroll_hint(th, &pstate.card_history));
            draw_separator(tw);
            draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
            draw_bottom_bar(tw, th, &mode, &shared, pstate);
            continue;
        }

        // 鼠标点击：切换 TTS 输入框焦点
        if let Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, .. }) = raw_evt {
            if matches!(mode, Mode::Idle) {
                let input_rows = [th.saturating_sub(5), th.saturating_sub(4)];
                let new_focused = input_rows.contains(&row);
                if new_focused != pstate.tts_focused {
                    pstate.tts_focused = new_focused;
                    draw_bottom_bar(tw, th, &mode, &shared, pstate);
                }
            }
            continue;
        }

        let evt = match raw_evt { Event::Key(k) => k, _ => continue };

        // 释放事件 — 修饰键清除 / PTT 松开
        if evt.kind == KeyEventKind::Release {
            match evt.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => mod_q = false,
                KeyCode::Char('w') | KeyCode::Char('W') => mod_w = false,
                KeyCode::Char('t') | KeyCode::Char('T') => mod_t = false,
                KeyCode::Char('n') | KeyCode::Char('N') => mod_n = false,
                KeyCode::Char('p') | KeyCode::Char('P') => {
                    if ptt_active {
                        // 保存 TX 录音，完成卡片
                        if let Some(ref mon) = rx_monitor {
                            if mon.is_recording() {
                                let samples = mon.stop_recording();
                                let dur = samples.len() as f32 / mon.sample_rate() as f32;
                                if let Some(ref card) = active_card {
                                    let content = finalize_recording_content(
                                        &card.kind, &card.card_ts, &card.freq,
                                        &card.segment, mon.sample_rate(), &samples, dur);
                                    update_card_by_count(
                                        &mut pstate.card_history, card.count,
                                        pstate.scroll_offset, tw, th, &content);
                                }
                            }
                        }
                        active_card = None;

                        let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
                        // 停止 PC 麦克风 → CM108 路由，恢复 RX passthrough
                        tx_capture = None;
                        if let Some(ref mon) = rx_monitor { mon.unmute_passthrough(); }
                        ptt_active = false;
                        ptt_start = None;
                        draw_title_bar(tw, &shared, None, has_scroll_hint(th, &pstate.card_history));
                    }
                    ptt_press_time = None;
                }
                _ => {}
            }
            continue;
        }

        if evt.kind != KeyEventKind::Press { continue; }

        match &mut mode {
            Mode::Idle => {
                // ── TTS 焦点优先处理：有焦点时完全隔离，continue 跳过快捷键 ──
                if pstate.tts_focused {
                    // 将字符索引转换为字节偏移（操作 String 需要字节位置）
                    let char_to_byte = |s: &str, idx: usize| -> usize {
                        s.char_indices().nth(idx).map(|(b, _)| b).unwrap_or(s.len())
                    };
                    let tts_len = pstate.tts_text.chars().count();

                    match evt.code {
                        KeyCode::Tab => {
                            pstate.tts_focused = false;
                            draw_bottom_bar(tw, th, &mode, &shared, pstate);
                        }
                        KeyCode::Esc => {
                            pstate.tts_focused = false;
                            pstate.tts_text.clear();
                            pstate.tts_cursor = 0;
                            draw_bottom_bar(tw, th, &mode, &shared, pstate);
                        }
                        KeyCode::Enter => {
                            if !pstate.tts_text.is_empty() && !ptt_active {
                                let text = std::mem::take(&mut pstate.tts_text);
                                pstate.tts_cursor = 0;
                                pstate.tts_focused = false;
                                pstate.notification = Some(("TTS 合成中...".to_string(), Instant::now(), true));
                                draw_bottom_bar(tw, th, &mode, &shared, pstate);
                                let voice = pstate.tts_voice.clone();
                                let (stage_tx, stage_rx) = mpsc::channel();
                                std::thread::spawn(move || {
                                    let _ = stage_tx.send(tts::synthesize(&text, &voice));
                                });
                                tts_stage_rx = Some(stage_rx);
                            } else if !pstate.tts_text.is_empty() && ptt_active {
                                pstate.notification = Some(("发射中，请稍候".to_string(), Instant::now(), false));
                                draw_bottom_bar(tw, th, &mode, &shared, pstate);
                            }
                        }
                        // 光标移动
                        KeyCode::Left => {
                            if pstate.tts_cursor > 0 {
                                pstate.tts_cursor -= 1;
                                draw_bottom_bar(tw, th, &mode, &shared, pstate);
                            }
                        }
                        KeyCode::Right => {
                            if pstate.tts_cursor < tts_len {
                                pstate.tts_cursor += 1;
                                draw_bottom_bar(tw, th, &mode, &shared, pstate);
                            }
                        }
                        KeyCode::Home => {
                            pstate.tts_cursor = 0;
                            draw_bottom_bar(tw, th, &mode, &shared, pstate);
                        }
                        KeyCode::End => {
                            pstate.tts_cursor = tts_len;
                            draw_bottom_bar(tw, th, &mode, &shared, pstate);
                        }
                        // 删除：Backspace 删光标前一字符，Delete 删光标处字符
                        KeyCode::Backspace => {
                            if pstate.tts_cursor > 0 {
                                pstate.tts_cursor -= 1;
                                let byte = char_to_byte(&pstate.tts_text, pstate.tts_cursor);
                                pstate.tts_text.remove(byte);
                                draw_bottom_bar(tw, th, &mode, &shared, pstate);
                            }
                        }
                        KeyCode::Delete => {
                            if pstate.tts_cursor < tts_len {
                                let byte = char_to_byte(&pstate.tts_text, pstate.tts_cursor);
                                pstate.tts_text.remove(byte);
                                // 光标不动，但 tts_len 已减 1，保证不越界
                                let new_len = pstate.tts_text.chars().count();
                                pstate.tts_cursor = pstate.tts_cursor.min(new_len);
                                draw_bottom_bar(tw, th, &mode, &shared, pstate);
                            }
                        }
                        // 字符插入（在光标处插入，光标右移）
                        KeyCode::Char(c) => {
                            if !mod_n && !mod_q && !mod_w && !mod_t {
                                let byte = char_to_byte(&pstate.tts_text, pstate.tts_cursor);
                                pstate.tts_text.insert(byte, c);
                                pstate.tts_cursor += 1;
                                draw_bottom_bar(tw, th, &mode, &shared, pstate);
                            }
                        }
                        _ => {} // ↑↓ PageUp 等无关功能键静默忽略
                    }
                    continue; // 完全绕过后续快捷键处理
                }
                // ── 正常快捷键处理（tts_focused=false）──────────────────────
                match evt.code {
                    // 修饰键（互斥，mod_v 已移除）
                    KeyCode::Char('q') | KeyCode::Char('Q') => { mod_w=false; mod_t=false; mod_n=false; mod_q=true; },
                    KeyCode::Char('w') | KeyCode::Char('W') => { mod_q=false; mod_t=false; mod_n=false; mod_w=true; },
                    KeyCode::Char('t') | KeyCode::Char('T') => { mod_q=false; mod_w=false; mod_n=false; mod_t=true; },
                    KeyCode::Char('n') | KeyCode::Char('N') => { mod_q=false; mod_w=false; mod_t=false; mod_n=true; },

                    // 方向键（N+←→=降噪，Q+←→=静噪，裸←→=音量）
                    KeyCode::Left => {
                        if mod_n {
                            pstate.denoise_db = (pstate.denoise_db - 10.0).max(0.0);
                            if let Some(ref mon) = rx_monitor { mon.set_denoise_db(pstate.denoise_db); }
                        } else if mod_q {
                            if pstate.sql_target < 0 {
                                let s = shared.lock().unwrap().left.sql_pct() as i32;
                                pstate.sql_target = if s > 0 { s } else { 30 };
                            }
                            pstate.sql_target = (pstate.sql_target - 5).max(0);
                            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_SQL, &[0, pstate.sql_target as u8]));
                        } else {
                            // 裸←：音量 -5
                            if pstate.vol_target < 0 {
                                let v = shared.lock().unwrap().left.vol_pct() as i32;
                                pstate.vol_target = if v > 0 { v } else { 50 };
                            }
                            pstate.vol_target = (pstate.vol_target - 5).max(0);
                            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_VOL, &[0, pstate.vol_target as u8]));
                        }
                        draw_bottom_bar(tw, th, &mode, &shared, pstate);
                    }
                    KeyCode::Right => {
                        if mod_n {
                            pstate.denoise_db = (pstate.denoise_db + 10.0).min(100.0);
                            if let Some(ref mon) = rx_monitor { mon.set_denoise_db(pstate.denoise_db); }
                        } else if mod_q {
                            if pstate.sql_target < 0 {
                                let s = shared.lock().unwrap().left.sql_pct() as i32;
                                pstate.sql_target = if s > 0 { s } else { 30 };
                            }
                            pstate.sql_target = (pstate.sql_target + 5).min(100);
                            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_SQL, &[0, pstate.sql_target as u8]));
                        } else {
                            // 裸→：音量 +5
                            if pstate.vol_target < 0 {
                                let v = shared.lock().unwrap().left.vol_pct() as i32;
                                pstate.vol_target = if v > 0 { v } else { 50 };
                            }
                            pstate.vol_target = (pstate.vol_target + 5).min(100);
                            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_VOL, &[0, pstate.vol_target as u8]));
                        }
                        draw_bottom_bar(tw, th, &mode, &shared, pstate);
                    }
                    KeyCode::Up => {
                        if mod_w {
                            let key = if shared.lock().unwrap().left_main { 0x21u8 } else { 0xA1u8 };
                            send_key(cmd_tx, key);
                        } else if mod_t {
                            send_key(cmd_tx, 0x12);
                        } else {
                            let step = if shared.lock().unwrap().left_main { 0x02u8 } else { 0x82u8 };
                            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KNOB, &[step]));
                        }
                    }
                    KeyCode::Down => {
                        if mod_w {
                            let key = if shared.lock().unwrap().left_main { 0x21u8 } else { 0xA1u8 };
                            send_key(cmd_tx, key);
                        } else if mod_t {
                            send_key(cmd_tx, 0x12);
                        } else {
                            let step = if shared.lock().unwrap().left_main { 0x01u8 } else { 0x81u8 };
                            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KNOB, &[step]));
                        }
                    }

                    // Tab：切换 TTS 输入框焦点
                    KeyCode::Tab => {
                        pstate.tts_focused = !pstate.tts_focused;
                        draw_bottom_bar(tw, th, &mode, &shared, pstate);
                    }

                    // Esc：若 TTS 输入框有内容/焦点则先清除，否则退出监听
                    KeyCode::Esc => {
                        if pstate.tts_focused || !pstate.tts_text.is_empty() {
                            pstate.tts_focused = false;
                            pstate.tts_text.clear();
                            pstate.tts_cursor = 0;
                            draw_bottom_bar(tw, th, &mode, &shared, pstate);
                        } else {
                            // ── 退出前无条件清理所有 PTT 活动 ─────────────────
                            if let Some(ref stop) = file_ptt_stop { stop.store(true, Ordering::Relaxed); }
                            if let Some(ref stop) = tts_ptt_stop  { stop.store(true, Ordering::Relaxed); }
                            tts_stage_rx = None;
                            tx_capture   = None;
                            if let Some(ref mon) = rx_monitor { mon.unmute_passthrough(); }
                            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
                            // ─────────────────────────────────────────────────
                            return LoopExitReason::UserEsc;
                        }
                    }

                    // Enter：TTS 输入框中若有内容则触发合成
                    KeyCode::Enter => {
                        if !pstate.tts_text.is_empty() && !ptt_active {
                            let text = std::mem::take(&mut pstate.tts_text);
                            pstate.tts_focused = false;
                            pstate.notification = Some(("TTS 合成中...".to_string(), Instant::now(), true));
                            draw_bottom_bar(tw, th, &mode, &shared, pstate);
                            let voice = pstate.tts_voice.clone();
                            let (stage_tx, stage_rx) = mpsc::channel();
                            std::thread::spawn(move || {
                                let _ = stage_tx.send(tts::synthesize(&text, &voice));
                            });
                            tts_stage_rx = Some(stage_rx);
                        } else if !pstate.tts_text.is_empty() && ptt_active {
                            pstate.notification = Some(("发射中，请稍候".to_string(), Instant::now(), false));
                            draw_bottom_bar(tw, th, &mode, &shared, pstate);
                        }
                    }

                    // 单键操作
                    KeyCode::Char('m') | KeyCode::Char('M') => { send_key(cmd_tx, 0x10); }
                    KeyCode::Char('p') | KeyCode::Char('P') => {
                        ptt_press_time = Some(Instant::now());
                    }
                    KeyCode::Char('o') | KeyCode::Char('O') => {
                        mode = Mode::PowerConfirm;
                        draw_bottom_bar(tw, th, &mode, &shared, pstate);
                    }
                    KeyCode::Char('l') | KeyCode::Char('L') => {
                        if !mod_q && !pstate.tts_focused {
                            mode = Mode::FreqInput { side: 0, input: String::new() };
                            draw_bottom_bar(tw, th, &mode, &shared, pstate);
                        }
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        if !mod_q && !pstate.tts_focused {
                            mode = Mode::FreqInput { side: 1, input: String::new() };
                            draw_bottom_bar(tw, th, &mode, &shared, pstate);
                        }
                    }
                    KeyCode::PageUp => {
                        let (_, msg_h) = msg_area_bounds(th);
                        let per_page = cards_per_page_count(msg_h);
                        let max_offset = pstate.card_history.len().saturating_sub(1);
                        pstate.scroll_offset = (pstate.scroll_offset + per_page.max(1)).min(max_offset);
                        draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
                    }
                    KeyCode::PageDown => {
                        let (_, msg_h) = msg_area_bounds(th);
                        let per_page = cards_per_page_count(msg_h);
                        pstate.scroll_offset = pstate.scroll_offset.saturating_sub(per_page.max(1));
                        draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
                    }
                    KeyCode::Char('f') | KeyCode::Char('F') if !ptt_active => {
                        // 弹出原生文件选择框（Windows 原生对话框，不需要退出 raw mode）
                        let picked = rfd::FileDialog::new()
                            .add_filter("音频文件", &["wav", "mp3", "ogg", "flac", "aac", "m4a"])
                            .set_title("选择音频文件发射（最长30秒）")
                            .pick_file();

                        // 对话框关闭后终端可能被遮挡，强制全量重绘
                        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::All));
                        draw_title_bar(tw, &shared, None, has_scroll_hint(th, &pstate.card_history));
                        draw_separator(tw);
                        draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
                        draw_bottom_bar(tw, th, &mode, &shared, pstate);

                        if let Some(path) = picked {
                            let path_str = path.to_string_lossy().to_string();
                            let stop = Arc::new(AtomicBool::new(false));
                            let stop_c = stop.clone();
                            let (done_tx, done_rx) = mpsc::channel::<Result<Duration, String>>();
                            // 注意：不再 clone cmd_tx（避免后台线程持有 sender 导致 TX 线程无法退出）
                            // PTT=0 统一由主循环的 file_ptt_rx done handler 发送

                            // PTT=1 先从主线程发出
                            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[1]));
                            if let Some(ref mon) = rx_monitor { mon.mute_passthrough(); }

                            // 后台线程：前导延迟（可中断）后播放，完成后通知主循环
                            std::thread::spawn(move || {
                                for _ in 0..20 { // 20×50ms = 1s，可被 stop_flag 中断
                                    if stop_c.load(Ordering::Relaxed) {
                                        let _ = done_tx.send(Ok(Duration::ZERO));
                                        return;
                                    }
                                    std::thread::sleep(Duration::from_millis(50));
                                }
                                let result = audio::play_audio_file_to_cm108(&path_str, 30, stop_c);
                                let _ = done_tx.send(result);
                                // PTT=0 由主循环 done handler 负责，不在此处发送
                            });

                            file_ptt_stop = Some(stop);
                            file_ptt_rx   = Some(done_rx);
                            ptt_active    = true;
                            ptt_start     = Some(Instant::now());

                            // 创建 TX 卡片（快照当前 MAIN 侧信息）
                            // MAIN 未知时显示"MAIN?"，不猜测错误侧别
                            let s = shared.lock().unwrap();
                            let (f_side, f_freq, f_power, f_tone) = if s.left_main {
                                ("LEFT".to_string(), s.left.freq.clone(), s.left.power.clone(), s.left.tone_str().to_string())
                            } else if s.right_main {
                                ("RIGHT".to_string(), s.right.freq.clone(), s.right.power.clone(), s.right.tone_str().to_string())
                            } else {
                                ("MAIN?".to_string(), s.left.freq.clone(), s.left.power.clone(), s.left.tone_str().to_string())
                            };
                            drop(s);
                            let ts  = chrono::Local::now().format("%H:%M:%S").to_string();
                            let tsf = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
                            pstate.card_count += 1;
                            let cnt = pstate.card_count;
                            let init_status = format!("│ {}  文件发射中...  00:00",
                                " TX ".on_dark_red().white().bold());
                            pstate.card_history.push(CardRecord {
                                count: cnt, kind: CardKind::Tx,
                                side: f_side.clone(), ts_display: ts.clone(),
                                freq: f_freq.clone(), power: f_power.clone(),
                                tone: f_tone.clone(), status_line: init_status,
                            });
                            active_card = Some(ActiveCard {
                                count: cnt,
                                start_time: Instant::now(),
                                kind: CardKind::Tx,
                                last_elapsed: u64::MAX,
                                side: f_side, freq: f_freq, power: f_power, tone: f_tone,
                                segment: 0, card_ts: tsf,
                            });
                            pstate.scroll_offset = 0;
                            draw_message_area(&pstate.card_history, pstate.scroll_offset, tw, th);
                            draw_title_bar(tw, &shared, ptt_start, has_scroll_hint(th, &pstate.card_history));
                        }
                    }
                    _ => {}
                }
            }

            Mode::FreqInput { side, input } => {
                match evt.code {
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        input.push(c);
                        if input.len() >= 6 {
                            let side_val = *side;
                            let digits: Vec<u8> = input.chars().map(|c| c as u8 - b'0').collect();
                            let freq_str = input.clone();
                            let tx = cmd_tx.clone();
                            let sh = shared.clone();
                            std::thread::spawn(move || {
                                send_freq_sequence(&tx, &sh, side_val, &digits);
                            });
                            // 操作反馈：格式化为 MHz，右对齐显示在状态栏
                            let mhz = format!("{}.{}", &freq_str[..3], &freq_str[3..]);
                            let side_label = if side_val == 0 { "左侧频率" } else { "右侧频率" };
                            pstate.notification = Some((
                                format!("{}→{} MHz", side_label, mhz),
                                Instant::now(), false));
                            mode = Mode::Idle;
                        }
                        draw_bottom_bar(tw, th, &mode, &shared, pstate);
                    }
                    KeyCode::Backspace => { input.pop(); }
                    KeyCode::Esc => { mode = Mode::Idle; }
                    _ => {}
                }
                draw_bottom_bar(tw, th, &mode, &shared, pstate);
            }

            Mode::PowerConfirm => {
                match evt.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        // 根据当前 radio_alive 判断是开机还是关机命令
                        let was_alive = shared.lock().unwrap().radio_alive;
                        let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_POWER_TOGGLE, &[]));
                        let expect_alive = !was_alive;
                        let msg = if was_alive { "关机命令已发送" } else { "开机命令已发送" };
                        pstate.notification = Some((msg.to_string(), Instant::now(), true));  // persistent
                        power_toggle_expected = Some(expect_alive);
                        mode = Mode::Idle;
                        draw_bottom_bar(tw, th, &mode, &shared, pstate);
                    }
                    _ => {
                        mode = Mode::Idle;
                        draw_bottom_bar(tw, th, &mode, &shared, pstate);
                    }
                }
            }
        }
    }

    #[allow(unreachable_code)]
    LoopExitReason::UserEsc
}

// ===== 辅助函数 =====

/// 发送按键 press + 延时 + release
fn send_key(cmd_tx: &mpsc::Sender<Vec<u8>>, key: u8) {
    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_PRESS, &[key]));
    std::thread::sleep(Duration::from_millis(100));
    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_REL, &[]));
}

/// 频率输入序列：切 MAIN + 逐位数字
fn send_freq_sequence(cmd_tx: &mpsc::Sender<Vec<u8>>, shared: &SharedState, side: u8, digits: &[u8]) {
    let need_switch = {
        let s = shared.lock().unwrap();
        (side == 0 && !s.left_main) || (side == 1 && !s.right_main)
    };
    if need_switch {
        let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_PRESS, &[0x10]));
        std::thread::sleep(Duration::from_millis(200));
        let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_REL, &[]));
        std::thread::sleep(Duration::from_millis(500));
    }
    for &d in digits {
        let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_PRESS, &[d]));
        std::thread::sleep(Duration::from_millis(300));
        let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_REL, &[]));
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 计算录音保存后卡片第3行的内容
fn finalize_recording_content(
    kind: &CardKind,
    card_ts: &str,
    freq: &str,
    segment: &u8,
    sample_rate: u32,
    samples: &[f32],
    dur: f32,
) -> String {
    if dur < 0.3 {
        return "│  --  信号过短（< 0.3s），未保存".to_string();
    }
    let prefix = match kind { CardKind::Rx => "RX", CardKind::Tx => "TX" };
    let seg_suffix = if *segment > 0 {
        format!("_seg{}", segment)
    } else {
        String::new()
    };
    let fname = format!("recordings/{}_{}_{}{}_{}s.wav",
        prefix, card_ts, freq.replace('.', ""), seg_suffix, dur as u32);
    if audio::save_wav_48k(samples, sample_rate, &fname).is_ok() {
        format!("│  OK  {} ({:.1}s)", fname, dur)
    } else {
        format!("│  --  录音保存失败 ({:.1}s)", dur)
    }
}

// ===== UI 绘制 =====

fn print_at(row: u16, text: &str) {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::MoveTo(0, row));
    print!("{}", text);
    let _ = stdout.flush();
}

/// 更新卡片状态行（行2，原地覆盖，无闪烁）
fn update_card_status_line(row: u16, tw: u16, content: &str) {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::MoveTo(0, row));
    print!("{}", content);
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::UntilNewLine));
    // 右边框
    let _ = execute!(stdout, cursor::MoveTo(tw.saturating_sub(1), row));
    print!("{}", "│".dark_cyan());
    let _ = stdout.flush();
}

/// 同步更新行2内容到终端 + card_history（供 resize 重绘用）
/// 通过 count 查找卡片（不依赖存储的行号）
fn update_card_by_count(
    history: &mut Vec<CardRecord>,
    count: u16,
    scroll_offset: usize,
    tw: u16, th: u16,
    content: &str,
) {
    // 更新历史记录
    if let Some(rec) = history.iter_mut().find(|r| r.count == count) {
        rec.status_line = content.to_string();
    }
    // 若该卡片在当前视口内，直接更新屏幕行
    if let Some(row) = find_card_screen_row(history, count, scroll_offset, tw, th) {
        update_card_status_line(row + 2, tw, content);
    }
}

/// 判断是否需要在标题栏显示 PgUp/PgDn 翻页提示（有超过一页的消息时）
fn has_scroll_hint(th: u16, history: &[CardRecord]) -> bool {
    let (_, msg_h) = msg_area_bounds(th);
    history.len() > cards_per_page_count(msg_h)
}

/// 消息区范围：(msg_top, msg_h)
/// Row 0=title, Row 1=separator, Row 2=空行 → 消息区从 Row 3 开始
/// 底部保留：th-5=输入框顶, th-4=输入框内容, th-3=分隔线, th-2=快捷键, th-1=状态行
fn msg_area_bounds(th: u16) -> (u16, u16) {
    let msg_top = 3u16;
    let msg_h   = th.saturating_sub(8); // th - 3（顶部）- 5（底部含输入框）
    (msg_top, msg_h)
}

/// 每页最多显示的完整卡片数（保留1行给滚动指示器，防止与卡片底边框重叠）
fn cards_per_page_count(msg_h: u16) -> usize {
    (msg_h.saturating_sub(1) / 4) as usize
}

/// 查找指定 count 卡片在当前视口中的屏幕起始行（若不在视口则返回 None）
fn find_card_screen_row(
    history: &[CardRecord], count: u16,
    scroll_offset: usize, _tw: u16, th: u16,
) -> Option<u16> {
    let (msg_top, msg_h) = msg_area_bounds(th);
    let per_page = cards_per_page_count(msg_h);
    if per_page == 0 { return None; }
    let total    = history.len();
    let end_idx  = total.saturating_sub(scroll_offset);
    let start_idx = end_idx.saturating_sub(per_page);
    history[start_idx..end_idx]
        .iter()
        .position(|r| r.count == count)
        .map(|pos| msg_top + (pos as u16 * 4))
}

/// 全量重绘消息区（清空 + 按 scroll_offset 绘制可见卡片 + 滚动指示器）
fn draw_message_area(history: &[CardRecord], scroll_offset: usize, tw: u16, th: u16) {
    let (msg_top, msg_h) = msg_area_bounds(th);
    if msg_h == 0 { return; }
    let indicator_row = msg_top + msg_h - 1;
    let per_page = cards_per_page_count(msg_h);
    let total    = history.len();

    // 清空消息区（含指示器行）
    let mut stdout = io::stdout();
    for r in msg_top..=indicator_row {
        let _ = execute!(stdout, cursor::MoveTo(0, r),
                         terminal::Clear(terminal::ClearType::UntilNewLine));
    }

    if per_page == 0 || total == 0 {
        let _ = stdout.flush();
        return;
    }

    // 计算可见窗口
    let end_idx   = total.saturating_sub(scroll_offset);
    let start_idx = end_idx.saturating_sub(per_page);

    for (i, card) in history[start_idx..end_idx].iter().enumerate() {
        let row = msg_top + (i as u16 * 4);
        let kind_str = match card.kind { CardKind::Tx => "TX", CardKind::Rx => "RX" };
        draw_card_frame(row, tw, card.count, kind_str,
                        &card.side, &card.ts_display,
                        &card.freq, &card.power, &card.tone);
        update_card_status_line(row + 2, tw, &card.status_line);
    }

    // 滚动指示器（仅在有更新消息在下方时显示）
    if scroll_offset > 0 {
        let newer = scroll_offset.min(total);
        let _ = execute!(stdout, cursor::MoveTo(0, indicator_row));
        print!("{}", format!("  ↓ 还有 {} 条新消息  [PgDn 返回最新]", newer).dark_yellow());
    }

    let _ = stdout.flush();
}

/// 绘制 4 行卡片框架（一次性，后续只更新行2）
/// 行0: ┌─ [编号+类型标签(有背景色)] ─── HH:MM:SS ──────────────────┐
/// 行1: │ SIDE  FREQ  POWER  TONE                                    │
/// 行2: │ [TX/RX]  状态...  00:00                                    │
/// 行3: └──────────────────────────────────────────────────────────────┘
fn draw_card_frame(row: u16, tw: u16, count: u16, kind: &str, side: &str, ts: &str,
                   freq: &str, power: &str, tone: &str) {
    let mut stdout = io::stdout();
    let is_tx = kind.starts_with("TX");

    // 行0：顶部边框
    // 结构：┌─ [tag(有背景色)] ─── ts ──...──┐
    // 全部是 ASCII + box-drawing（均 1 宽），用 .chars().count() 精确计算
    let border_left = "┌─ ";
    let tag          = format!(" {:03} {} ", count, kind);
    let sep_ts       = format!(" ─── {} ", ts);
    let prefix_vis   = border_left.chars().count()
                     + tag.chars().count()
                     + sep_ts.chars().count();
    let dashes = (tw as usize).saturating_sub(prefix_vis + 1); // +1 for ┐

    let _ = execute!(stdout, cursor::MoveTo(0, row));
    print!("{}", border_left.dark_cyan());
    if is_tx {
        print!("{}", tag.on_dark_red().white().bold());
    } else {
        print!("{}", tag.on_dark_blue().white().bold());
    }
    print!("{}", format!("{}{}┐", sep_ts, "─".repeat(dashes)).dark_cyan());

    // 行1：频率/功率/亚音信息
    let tone_s = if tone.is_empty() { "无亚音".to_string() } else { tone.to_string() };
    let info = format!("│ {}  {}  {}  {}", side, freq, power, tone_s);
    let _ = execute!(stdout, cursor::MoveTo(0, row + 1));
    print!("{}", info.dark_cyan());
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::UntilNewLine));
    let _ = execute!(stdout, cursor::MoveTo(tw.saturating_sub(1), row + 1));
    print!("{}", "│".dark_cyan());

    // 行2：初始状态行
    let _ = execute!(stdout, cursor::MoveTo(0, row + 2));
    if is_tx {
        print!("│ {}  发射中...  00:00", " TX ".on_dark_red().white().bold());
    } else {
        print!("│ {}  接收中...  00:00", " RX ".on_dark_blue().white().bold());
    }
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::UntilNewLine));
    let _ = execute!(stdout, cursor::MoveTo(tw.saturating_sub(1), row + 2));
    print!("{}", "│".dark_cyan());

    // 行3：底部边框
    let _ = execute!(stdout, cursor::MoveTo(0, row + 3));
    print!("{}", format!("└{}┘", "─".repeat(tw.saturating_sub(2) as usize)).dark_cyan());

    let _ = stdout.flush();
}

fn draw_title_bar(tw: u16, shared: &SharedState, ptt_start: Option<Instant>, show_pgud: bool) {
    let s = shared.lock().unwrap();
    let main_side = if s.left_main { "LEFT" } else if s.right_main { "RIGHT" } else { "LEFT" };
    let band = if s.left_main { &s.left } else if s.right_main { &s.right } else { &s.left };
    let mode_s  = band.mode.clone();
    let freq_s  = band.freq.clone();
    let power_s = band.power.clone();
    let s_level = band.s_level;
    let is_busy = s.left.is_busy || s.right.is_busy;
    let is_tx   = s.left.is_tx  || s.right.is_tx;
    let local_ptt = ptt_start.is_some();
    let ptt = local_ptt || s.ptt_override || is_tx;
    drop(s);

    let remaining_secs = ptt_start.map(|t| 30u64.saturating_sub(t.elapsed().as_secs()));

    let status = if ptt {
        match remaining_secs {
            Some(s) if s > 0 => format!("TX 发射中 {:2}s", s),
            _                 => "TX 发射中".to_string(),
        }
    } else if is_busy {
        "RX 接收中".to_string()
    } else {
        "空闲".to_string()
    };

    // 右侧文字：可选 PgUp/PgDn翻页 提示 + Esc:退出 + 状态
    let pgud_prefix = if show_pgud { "PgUp/PgDn翻页  " } else { "" };
    let right_text = format!("{}Esc:退出  OK 监听中  {}", pgud_prefix, status);
    let right_vis: usize = right_text.chars().map(|c| if (c as u32) > 0x7F { 2 } else { 1 }).sum();
    let right_col = (tw as usize).saturating_sub(right_vis);

    let left_plain = format!("elfRadio  {} MAIN  {} {} MHz  {}  S{}",
        main_side, mode_s, freq_s, power_s, s_level);
    let left_vis = left_plain.len();
    let gap = right_col.saturating_sub(left_vis);

    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::MoveTo(0, 0));
    print!("{}  {} MAIN  {} {} MHz  {}  S{}{}",
        "elfRadio".cyan().bold(), main_side, mode_s, freq_s, power_s, s_level,
        " ".repeat(gap));
    // PgUp/PgDn 前缀（暗色）
    if show_pgud {
        print!("{}", pgud_prefix.dark_grey());
    }
    // 先打印 Esc:退出（暗灰色）
    print!("{}", "Esc:退出  ".dark_grey());
    if ptt {
        print!("{}", format!("OK 监听中  {}", status).on_dark_red().white().bold());
    } else if is_busy {
        print!("{}", format!("OK 监听中  {}", status).on_dark_blue().white().bold());
    } else {
        print!("{}", format!("OK 监听中  {}", status).dark_grey());
    }
    let _ = stdout.flush();
}

fn draw_separator(tw: u16) {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::MoveTo(0, 1));
    print!("{}", "\u{2500}".repeat(tw as usize).dark_grey());
    let _ = stdout.flush();
}

fn draw_bottom_bar(tw: u16, th: u16, mode: &Mode, shared: &SharedState,
                   pstate: &MonitorPersistentState) {
    let mut stdout = io::stdout();

    // ── TTS 输入框（th-5: 顶横线，th-4: 内容）──────────────────────
    let _ = execute!(stdout, cursor::MoveTo(0, th.saturating_sub(5)));
    print!("{}", "\u{2500}".repeat(tw as usize).dark_grey());
    let _ = execute!(stdout, cursor::MoveTo(0, th.saturating_sub(4)));
    if pstate.tts_focused {
        // 有焦点：按光标位置计算视口，保证光标始终可见
        // 可用宽度：tw - 4（" > " 前缀 3 列 + 左边 1 列空格）
        let max_vis = (tw as usize).saturating_sub(4);
        let cursor = pstate.tts_cursor;
        let chars: Vec<char> = pstate.tts_text.chars().collect();
        let len = chars.len();

        // 计算从位置 0 到光标的视觉宽度
        let text_to_cursor: String = chars[..cursor.min(len)].iter().collect();
        let width_to_cursor = vis_width(&text_to_cursor);

        // 视口起点：
        //   光标前文字宽度 + 1（光标块）<= max_vis 时，从头开始显示（光标未到右边界）
        //   超出后，从光标向左回推，使光标恰好贴着右边界
        let view_start = if width_to_cursor + 1 <= max_vis {
            0
        } else {
            // 向左回推，找到恰好让光标停在右边界的 view_start
            let target = max_vis.saturating_sub(1); // 光标前可用宽度
            let mut vs = cursor;
            let mut w = 0usize;
            while vs > 0 {
                let cw = vis_width(&chars[vs - 1].to_string());
                if w + cw > target { break; }
                w += cw;
                vs -= 1;
            }
            vs
        };

        // 光标前的可见文字
        let before: String = chars[view_start..cursor.min(len)].iter().collect();
        let before_vis = vis_width(&before);

        // 光标后的文字（截断到剩余宽度）
        let after_max = max_vis.saturating_sub(before_vis + 1);
        let after: String = if cursor < len {
            let after_full: String = chars[cursor..].iter().collect();
            truncate_to_vis(&after_full, after_max)
        } else {
            String::new()
        };

        print!(" > {}{}{}", before.white(), "█".white(), after.white());
    } else if pstate.tts_text.is_empty() {
        // 无焦点空输入：占位提示
        print!(" {}", "输入TTS文字，Tab获焦，Enter发射...".dark_grey());
    } else {
        // 有内容但失焦：暗色显示（从头显示，截断到行宽）
        let display_text = truncate_to_vis(&pstate.tts_text, (tw as usize).saturating_sub(3));
        print!(" {}", display_text.dark_grey());
    }
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::UntilNewLine));

    // ── 分隔线（th-3）──────────────────────────────────────────────
    let _ = execute!(stdout, cursor::MoveTo(0, th.saturating_sub(3)));
    print!("{}", "\u{2500}".repeat(tw as usize).dark_grey());

    // ── 快捷键提示（th-2）─────────────────────────────────────────
    let _ = execute!(stdout, cursor::MoveTo(0, th.saturating_sub(2)));
    let hint = match mode {
        Mode::Idle => {
            format!("{}  {}  {}  {}  {}  {}  {}  {}  {}  {}",
                "L:左频率 R:右频率".yellow().bold(),
                "M:切MAIN".yellow().bold(),
                "P:长按发射".yellow().bold(),
                "F:文件发射".yellow().bold(),
                "O:开关机".yellow().bold(),
                format!("{}:频率", "\u{2191}\u{2193}").dark_cyan(),
                format!("{0}{1}:音量  Q+{0}{1}:静噪", "\u{2190}", "\u{2192}").dark_cyan(),
                format!("W+{0}:功率  T+{0}:亚音", "\u{2191}\u{2193}").dark_cyan(),
                format!("N+{0}{1}:降噪", "\u{2190}", "\u{2192}").dark_cyan(),
                "Tab:TTS输入".dark_cyan(),
            )
        }
        Mode::FreqInput { side, input } => {
            let side_str = if *side == 0 { "LEFT" } else { "RIGHT" };
            let remaining = 6usize.saturating_sub(input.len());
            format!("{} 频率: {}{} (还需{}位)  Esc取消",
                side_str.yellow().bold(), input, "\u{2581}".dark_grey(), remaining)
        }
        Mode::PowerConfirm => {
            format!("{} 确认开关机？ Y/其它键取消", "!!".on_dark_yellow().white().bold())
        }
    };
    print!("{}", hint);
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::UntilNewLine));

    // ── 状态行（th-1）─────────────────────────────────────────────
    let s = shared.lock().unwrap();
    let radio = if s.radio_alive { "OK".green() } else { "--".dark_grey() };
    let pc    = if s.pc_alive   { "OK".green() } else { "--".dark_grey() };
    let lv = s.left.vol_pct();  let ls = s.left.sql_pct();
    let rv = s.right.vol_pct(); let rs = s.right.sql_pct();
    drop(s);

    let dnr_str = if pstate.denoise_db < 0.1 {
        "DNR:--".to_string()
    } else {
        format!("DNR:{:.0}%", pstate.denoise_db)
    };

    // 计算左侧纯文本宽度（用于通知右对齐参考）
    let status_plain = format!(
        "Radio:OK PC:OK  L: VOL {} % / SQL {} %  R: VOL {} % / SQL {} %  {}",
        lv, ls, rv, rs, dnr_str);
    let status_vis = status_plain.len();

    let _ = execute!(stdout, cursor::MoveTo(0, th.saturating_sub(1)));
    print!("Radio:{} PC:{}  L: {} {} / {} {}  R: {} {} / {} {}  {}",
        radio, pc,
        format!("VOL {}", lv).dark_yellow(), "%".dark_yellow(),
        format!("SQL {}", ls).dark_cyan(),   "%".dark_cyan(),
        format!("VOL {}", rv).dark_yellow(), "%".dark_yellow(),
        format!("SQL {}", rs).dark_cyan(),   "%".dark_cyan(),
        if pstate.denoise_db < 0.1 { dnr_str.dark_grey() } else { dnr_str.dark_cyan() },
    );
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::UntilNewLine));

    // 通知右对齐
    let notification = &pstate.notification;
    let show_notif = if let Some((_, ref instant, persistent)) = notification {
        *persistent || instant.elapsed() < Duration::from_secs(3)
    } else {
        false
    };
    if show_notif {
        if let Some((ref msg, _, _)) = notification {
            let notif_str = format!("✓ {}", msg);
            let notif_vis = vis_width(&notif_str);
            let notif_col = (tw as usize).saturating_sub(notif_vis);
            if notif_col > status_vis + 2 {
                let _ = execute!(stdout, cursor::MoveTo(notif_col as u16, th.saturating_sub(1)));
                print!("{}", notif_str.dark_green().bold());
            }
        }
    }

    let _ = stdout.flush();
}

/// 计算字符串视觉宽度（CJK 字符计为 2）
fn vis_width(s: &str) -> usize {
    s.chars().map(|c| {
        let cp = c as u32;
        if (cp >= 0x1100 && cp <= 0x115F)
        || (cp >= 0x2E80 && cp <= 0x303F)
        || (cp >= 0x3040 && cp <= 0x33FF)
        || (cp >= 0x3400 && cp <= 0x4DBF)
        || (cp >= 0x4E00 && cp <= 0x9FFF)
        || (cp >= 0xA000 && cp <= 0xA4CF)
        || (cp >= 0xAC00 && cp <= 0xD7AF)
        || (cp >= 0xF900 && cp <= 0xFAFF)
        || (cp >= 0xFE10 && cp <= 0xFE19)
        || (cp >= 0xFE30 && cp <= 0xFE6F)
        || (cp >= 0xFF00 && cp <= 0xFF60)
        || (cp >= 0xFFE0 && cp <= 0xFFE6)
        || (cp >= 0x20000 && cp <= 0x2FFFD)
        || (cp >= 0x30000 && cp <= 0x3FFFD)
        { 2 } else { 1 }
    }).sum()
}

/// 按视觉宽度截断字符串（不超过 max_vis 列），返回截断后的字符串
fn truncate_to_vis(s: &str, max_vis: usize) -> String {
    let mut result = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = if vis_width(&c.to_string()) == 2 { 2 } else { 1 };
        if used + w > max_vis { break; }
        result.push(c);
        used += w;
    }
    result
}
