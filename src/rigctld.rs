// ===================================================================
// Hamlib rigctld 文本协议服务器（TCP 4532）
//
// 协议参考: tests/rigctl_parse.c + doc/man1/rigctld.1
//
// 命令格式: 单字符（如 'f' 'F 145000000'）或反斜杠长名（如 '\set_freq 145000000'）
// 响应格式:
//   - 短模式: 直接返回值，每行一个；命令无返回值时回 "RPRT 0\n"，错误回 "RPRT -<err>\n"
//   - 扩展模式（'+' 前缀或 backslash 命令）: "set_freq: 145000000\nFreq: 145000000\nRPRT 0\n"
//
// 当前实现 WSJT-X / fldigi / JTDX 必需的最小子集：
//   f F m M t T v V s S j J _ \dump_state \chk_vfo \get_powerstat \set_powerstat q Q
//
// F set_freq 在本版用 stub（回 RPRT 0 不实际改频率），完整实现见 Task #9。
// ===================================================================

use crate::state::{BandState, RadioState, SharedState, WifiState};
use esp_idf_svc::sys::esp_timer_get_time;
use std::io::{BufRead, BufReader, Write};
use std::io::ErrorKind;
use std::time::Duration;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const PORT: u16 = 4532;
const MAX_CLIENTS: usize = 4;
const RIG_STEP_HZ: u64 = 2_500;
const RIG_TRACK_INTERVAL_US: u64 = 5_000_000;
const SAT_RX_SQL_OPEN_ADC: u16 = 20;
const SAT_SETUP_RETRY_US: u64 = 10_000_000;
const SAT_SETUP_SNAPSHOT_US: u64 = 800_000;
const SAT_SETUP_MAX_ATTEMPTS: u8 = 20;

#[derive(Clone, Copy, PartialEq)]
enum ToneMode {
    Off,
    Enc,
    EncDec,
    Dcs,
}

struct StepCandidate {
    role: &'static str,
    target: u64,
    current_hz: u64,
    delta_hz: i64,
    is_left: bool,
    is_sat_tx: bool,
}

fn make_sat_step_candidate(
    role: &'static str,
    target: Option<u64>,
    band: &BandState,
    is_left: bool,
    last_step_us: u64,
    now_us: u64,
) -> Option<StepCandidate> {
    if now_us.saturating_sub(last_step_us) < RIG_TRACK_INTERVAL_US {
        return None;
    }
    let target = target?;
    let current = freq_str_to_hz(band.freq.as_str()).unwrap_or(0);
    if current == 0 {
        return None;
    }
    let delta_hz = target as i64 - current as i64;
    if delta_hz.unsigned_abs() < RIG_STEP_HZ / 2 {
        return None;
    }
    Some(StepCandidate {
        role,
        target,
        current_hz: current,
        delta_hz,
        is_left,
        is_sat_tx: role == "TX",
    })
}

// Hamlib RIG_OK = 0, errors negative
const RPRT_OK: &str = "RPRT 0\n";
const RPRT_EINVAL: &str = "RPRT -1\n";       // 参数错
const RPRT_EPROTO: &str = "RPRT -8\n";       // 协议错
#[allow(dead_code)]
const RPRT_ENIMPL: &str = "RPRT -11\n";      // 未实现

// ===== TH-9800 菜单 #30 TONE F：50 组标准 CTCSS 频率 =====
// Hamlib 0.1Hz 单位 → 电台菜单显示文本
const CTCSS_TONES_TENTH_HZ: [u32; 50] = [
    670, 693, 719, 744, 770, 797, 825, 854, 885, 915,
    948, 974, 1000, 1035, 1072, 1109, 1148, 1188, 1230, 1273,
    1318, 1365, 1413, 1462, 1514, 1567, 1598, 1622, 1655, 1679,
    1713, 1738, 1773, 1799, 1835, 1862, 1899, 1928, 1966, 1995,
    2035, 2065, 2107, 2181, 2257, 2291, 2336, 2418, 2503, 2541,
];
const CTCSS_TONE_STRS: [&str; 50] = [
    "67.0",  "69.3",  "71.9",  "74.4",  "77.0",  "79.7",  "82.5",  "85.4",  "88.5",  "91.5",
    "94.8",  "97.4",  "100.0", "103.5", "107.2", "110.9", "114.8", "118.8", "123.0", "127.3",
    "131.8", "136.5", "141.3", "146.2", "151.4", "156.7", "159.8", "162.2", "165.5", "167.9",
    "171.3", "173.8", "177.3", "179.9", "183.5", "186.2", "189.9", "192.8", "196.6", "199.5",
    "203.5", "206.5", "210.7", "218.1", "225.7", "229.1", "233.6", "241.8", "250.3", "254.1",
];

// ===== TH-9800 菜单 #28 STEP：12 个步进选项 =====
const STEP_STRS: [&str; 12] = [
    "2.5", "5", "6.25", "7.5", "8.33", "10", "12.5", "15", "25", "30", "50", "100",
];

pub fn start_rigctld_thread(state: SharedState) {
    std::thread::Builder::new()
        .name("rigctld".into())
        .stack_size(4096)
        .spawn(move || rigctld_main(state))
        .expect("rigctld 线程启动失败");
}

/// 后台频率步进线程：将 state.rigctld_target_hz 与 MAIN 侧实际频率逐步逼近
/// 每次循环注入一帧旋钮 CW/CCW，每帧间隔 200ms（让 relay_up_thread 有时间消费 + 电台响应）
pub fn start_freq_stepper_thread(state: SharedState) {
    std::thread::Builder::new()
        .name("freq_stepper".into())
        .stack_size(3072)
        .spawn(move || freq_stepper_main(state))
        .expect("freq_stepper 线程启动失败");
}

fn freq_stepper_main(state: SharedState) {
    log::info!("[FreqStepper] 启动");

    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let now_us = unsafe { esp_timer_get_time() } as u64;

        let no_clients = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.rigctld_clients == 0
        };
        if no_clients {
            std::thread::sleep(std::time::Duration::from_millis(800));
            continue;
        }

        let sat_setup = {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            prepare_sat_setup_snapshot(&mut s, now_us);
            adopt_stable_sat_targets(&mut s, now_us);
            if s.rigctld_clients > 0
                && s.rigctld_sat_active
                && s.rigctld_sat_split_enabled
                && s.rigctld_setup_snapshot_ready
                && !s.rigctld_setup_running
                && !s.macro_running
                && s.rigctld_setup_attempts < SAT_SETUP_MAX_ATTEMPTS
                && now_us >= s.rigctld_sat_retry_after_us
                && (!s.rigctld_rx_initial_done || !s.rigctld_tx_initial_done || !s.rigctld_rx_step_ready || !s.rigctld_tx_step_ready)
            {
                match (s.rigctld_setup_rx_hz, s.rigctld_setup_tx_hz) {
                    (Some(rx), Some(tx)) => {
                        s.rigctld_setup_running = true;
                        s.rigctld_setup_attempts = s.rigctld_setup_attempts.saturating_add(1);
                        let session_id = s.rigctld_session_id;
                        Some((rx, tx, s.rigctld_sat_rx_is_left, s.rigctld_sat_tx_is_left, session_id))
                    }
                    _ => None,
                }
            } else {
                None
            }
        };

        if let Some((rx, tx, rx_is_left, tx_is_left, session_id)) = sat_setup {
            sat_setup_one_stage(&state, rx, tx, rx_is_left, tx_is_left, session_id);
            continue;
        }

        let setup_target = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            if s.rigctld_clients > 0
                && s.rigctld_setup_running
                && !s.rigctld_initial_freq_done
                && !s.macro_running
            {
                s.rigctld_target_hz
            } else {
                None
            }
        };

        if let Some(target) = setup_target {
            log::info!("[FreqStepper] 执行 rigctld 初始设置 target={}", target);
            rigctld_initial_setup(&state, target);
            continue;
        }

        let correction_target = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            if s.rigctld_clients > 0
                && s.rigctld_sat_active
                && s.rigctld_rx_initial_done
                && s.rigctld_tx_initial_done
                && s.rigctld_rx_step_ready
                && s.rigctld_tx_step_ready
                && !s.rigctld_setup_running
                && !s.macro_running
                && s.key_override.is_none()
                && s.knob_inject.is_none()
                && !s.ptt_override
                && !s.left.is_tx
                && !s.right.is_tx
                && !s.left.is_set
                && !s.right.is_set
                && !side_is_main(&s, s.rigctld_sat_rx_is_left)
            {
                Some(s.rigctld_sat_rx_is_left)
            } else {
                None
            }
        };
        if let Some(rx_is_left) = correction_target {
            log::warn!("[SatSession] MAIN 偏离 RX {}，自动切回", side_name(rx_is_left));
            inject_key_wait(&state, 0x10);
            continue;
        }

        // 读快照：无 rigctld client、宏运行、STEP 未就绪时都不追频
        let step = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            if s.rigctld_clients == 0 {
                continue;
            }
            if s.macro_running || s.rigctld_setup_running || s.knob_inject.is_some() || s.ptt_override || s.left.is_tx || s.right.is_tx {
                continue;
            }
            if s.rigctld_sat_active {
                if !s.rigctld_rx_step_ready || !s.rigctld_tx_step_ready {
                    continue;
                }
                let rx_candidate = make_sat_step_candidate(
                    "RX",
                    s.rigctld_rx_target_hz,
                    if s.rigctld_sat_rx_is_left { &s.left } else { &s.right },
                    s.rigctld_sat_rx_is_left,
                    s.rigctld_rx_last_step_us,
                    now_us,
                );
                let tx_candidate = make_sat_step_candidate(
                    "TX",
                    s.rigctld_tx_target_hz,
                    if s.rigctld_sat_tx_is_left { &s.left } else { &s.right },
                    s.rigctld_sat_tx_is_left,
                    s.rigctld_tx_last_step_us,
                    now_us,
                );
                match (rx_candidate, tx_candidate) {
                    (Some(rx), Some(tx)) => Some(if rx.delta_hz.unsigned_abs() >= tx.delta_hz.unsigned_abs() { rx } else { tx }),
                    (Some(rx), None) => Some(rx),
                    (None, Some(tx)) => Some(tx),
                    (None, None) => continue,
                }
            } else {
                if !s.rigctld_step_ready {
                    continue;
                }
                if now_us.saturating_sub(s.rigctld_last_step_us) < RIG_TRACK_INTERVAL_US {
                    continue;
                }
                let target = match s.rigctld_target_hz {
                    Some(t) => t,
                    None => continue,
                };
                let band = if s.right.is_main { &s.right } else { &s.left };
                let current = freq_str_to_hz(band.freq.as_str()).unwrap_or(0);
                let delta_hz = target as i64 - current as i64;
                if current == 0 || delta_hz.unsigned_abs() < RIG_STEP_HZ / 2 {
                    continue;
                }
                Some(StepCandidate {
                    role: "MAIN",
                    target,
                    current_hz: current,
                    delta_hz,
                    is_left: s.left.is_main,
                    is_sat_tx: false,
                })
            }
        };

        let Some(step) = step else { continue; };
        let (cw, ccw) = if step.is_left { (0x02u8, 0x01u8) } else { (0x82u8, 0x81u8) };
        let step_byte = if step.delta_hz > 0 { cw } else { ccw };

        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.knob_inject.is_none() && !s.macro_running && !s.rigctld_setup_running && !s.ptt_override && !s.left.is_tx && !s.right.is_tx {
            s.knob_inject = Some(step_byte);
            if s.rigctld_sat_active {
                if step.is_sat_tx {
                    s.rigctld_tx_last_step_us = now_us;
                } else {
                    s.rigctld_rx_last_step_us = now_us;
                }
            } else {
                s.rigctld_last_step_us = now_us;
            }
        }
    }
}


/// 键盘宏：MAIN 侧按 6 位数字键直接输入频率
/// 频率格式：MHz×3 位 + kHz×3 位（如 145.500 MHz → "145500"）
/// 假设当前在 VFO 模式（若在 MR 模式需先切 VFO，本版未实现）
fn inject_freq_keyboard(state: &SharedState, target_hz: u64, session_id: Option<u32>) -> bool {
    let mhz = target_hz / 1_000_000;
    let khz = (target_hz % 1_000_000) / 1_000;
    let digits = format!("{:03}{:03}", mhz, khz);
    log::info!("[FreqStepper] 键盘输入: \"{}\"", digits);

    for ch in digits.chars() {
        if let Some(id) = session_id {
            if !session_alive(state, id) {
                log::warn!("[FreqStepper] session #{} 已失效，取消键盘输入", id);
                return false;
            }
        }
        let key = (ch as u8).saturating_sub(b'0');
        if key > 9 { continue; }
        // 等待前一次注入被消费
        let wait_start = std::time::Instant::now();
        let mut warned = false;
        loop {
            if let Some(id) = session_id {
                if !session_alive(state, id) {
                    log::warn!("[FreqStepper] session #{} 等待按键时失效", id);
                    return false;
                }
            }
            let busy = {
                let s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.key_override.is_some() || s.key_release
            };
            if !busy { break; }
            if !warned && wait_start.elapsed() >= Duration::from_secs(1) {
                log::warn!("[FreqStepper] 等待按键注入被 relay_up_thread 消费超过 1s");
                warned = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // 按下
        state.lock().unwrap_or_else(|e| e.into_inner()).key_override = Some(key);
        std::thread::sleep(std::time::Duration::from_millis(250));
        // 松开
        state.lock().unwrap_or_else(|e| e.into_inner()).key_release = true;
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    log::info!("[FreqStepper] 键盘输入完成");
    true
}

fn rigctld_main(state: SharedState) {
    let active = Arc::new(AtomicUsize::new(0));
    log::info!("[Rigctld] acceptor 启动");

    loop {
        let connected = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.wifi_state == WifiState::Connected
        };
        if !connected {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }

        let listener = match TcpListener::bind(("0.0.0.0", PORT)) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("[Rigctld] bind 0.0.0.0:{} 失败: {}，5s 后重试", PORT, e);
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };
        log::info!("[Rigctld] 监听 0.0.0.0:{}", PORT);

        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    let cur = active.load(Ordering::SeqCst);
                    if cur >= MAX_CLIENTS {
                        log::warn!("[Rigctld] 拒绝 {}：已达最大并发数 {}", peer, MAX_CLIENTS);
                        drop(stream);
                        continue;
                    }
                    active.fetch_add(1, Ordering::SeqCst);
                    let first_client = {
                        let s = state.lock().unwrap_or_else(|e| e.into_inner());
                        s.rigctld_clients == 0
                    };
                    if first_client {
                        {
                            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                            s.rigctld_initial_freq_done = false;
                            s.rigctld_step_ready = false;
                            s.rigctld_setup_running = false;
                            s.rigctld_target_hz = None;
                            s.rigctld_last_step_us = 0;
                            s.rigctld_sat_retry_after_us = 0;
                            s.rigctld_setup_retry_after_us = 0;
                            s.rigctld_setup_attempts = 0;
                        }
                        begin_sat_session(&state);
                        log::info!("[Rigctld] 首次连接，等待 DTrac 频率/split 命令后执行初始设置");
                    }
                    {
                        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                        s.rigctld_clients = s.rigctld_clients.saturating_add(1);
                    }
                    let st = state.clone();
                    let act = active.clone();
                    log::info!("[Rigctld] 接受连接：{}", peer);
                    let spawn_result = std::thread::Builder::new()
                        .name(format!("rigctld_{}", peer.port()))
                        .stack_size(4096)
                        .spawn(move || {
                            handle_client(stream, &st);
                            act.fetch_sub(1, Ordering::SeqCst);
                            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
                            s.rigctld_clients = s.rigctld_clients.saturating_sub(1);
                            if s.rigctld_clients == 0 {
                                s.rigctld_target_hz = None;
                                s.rigctld_setup_running = false;
                                s.ptt_override = false;
                                clear_sat_session(&mut s);
                            }
                            log::info!("[Rigctld] 连接 {} 已关闭，剩余 {} 客户端", peer, s.rigctld_clients);
                        });
                    if let Err(e) = spawn_result {
                        active.fetch_sub(1, Ordering::SeqCst);
                        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                        s.rigctld_clients = s.rigctld_clients.saturating_sub(1);
                        if s.rigctld_clients == 0 {
                            s.rigctld_target_hz = None;
                            s.rigctld_setup_running = false;
                            s.ptt_override = false;
                            clear_sat_session(&mut s);
                        }
                        log::warn!("[Rigctld] handler 线程启动失败: {}，已回滚 client 计数", e);
                    }
                }
                Err(e) => {
                    let still = { state.lock().unwrap_or_else(|e| e.into_inner()).wifi_state == WifiState::Connected };
                    if !still {
                        log::info!("[Rigctld] WiFi 断开，重新初始化");
                        break;
                    }
                    log::warn!("[Rigctld] accept 错误: {}", e);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    }
}

fn handle_client(stream: TcpStream, state: &SharedState) {
    let _ = stream.set_nodelay(true);
    // 3s 无数据自动断开（DTrac 断开后尽快还原 IP 状态）
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    // ESP-IDF lwip 不可靠支持 TcpStream::try_clone()。改用单一 stream + BufReader 包装，
    // 通过 BufReader::get_mut() 写回原 stream（buffered read + raw write 共用同一句柄）
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut last_line_us = unsafe { esp_timer_get_time() } as u64;

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) => match e.kind() {
                ErrorKind::TimedOut | ErrorKind::WouldBlock => {
                    let now_us = unsafe { esp_timer_get_time() } as u64;
                    if now_us.saturating_sub(last_line_us) >= 10_000_000 {
                        log::info!("[Rigctld] 10s 无命令，关闭空闲 client");
                        return;
                    }
                    continue;
                }
                _ => return,
            },
        }
        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() { continue; }
        last_line_us = unsafe { esp_timer_get_time() } as u64;

        let resp = match dispatch(trimmed, state) {
            DispatchOut::Reply(s) => s,
            DispatchOut::Quit => return,
        };
        if !resp.is_empty() {
            let inner = reader.get_mut();
            if inner.write_all(resp.as_bytes()).is_err() { return; }
            let _ = inner.flush();
        }
    }
}

enum DispatchOut {
    Reply(String),
    Quit,
}

fn log_dtrac_command(line: &str, state: &SharedState) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }

    let body = trimmed.strip_prefix('+').unwrap_or(trimmed);
    let (cmd, args) = if let Some(rest) = body.strip_prefix("\\") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        (parts.next().unwrap_or(""), parts.next().unwrap_or("").trim())
    } else {
        let mut chars = body.chars();
        match chars.next() {
            Some(c) => (body.get(..c.len_utf8()).unwrap_or(""), chars.as_str().trim()),
            None => return,
        }
    };

    let key_cmd = matches!(cmd,
        "I" | "S" | "C" | "E" | "T" |
        "set_split_freq" | "set_split_vfo" |
        "set_ctcss_tone" | "set_ctcss_sql" | "set_tone" |
        "set_ptt"
    );

    if key_cmd {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        log::info!(
            "[DTracCmd] raw={:?} main={} rx={} pending={:?} target={:?} tx={} pending={:?} target={:?} ctcss={}",
            trimmed,
            if s.right.is_main { "RIGHT" } else { "LEFT" },
            side_name(s.rigctld_sat_rx_is_left),
            s.rigctld_rx_pending_hz,
            s.rigctld_rx_target_hz,
            side_name(s.rigctld_sat_tx_is_left),
            s.rigctld_tx_pending_hz,
            s.rigctld_tx_target_hz,
            s.rigctld_ctcss_tone,
        );
    } else if trimmed.starts_with("\\") || trimmed.starts_with('+') {
        log::info!("[DTracCmd] raw={:?} cmd={:?} args={:?}", trimmed, cmd, args);
    }
}

fn dispatch(line: &str, state: &SharedState) -> DispatchOut {
    log_dtrac_command(line, state);

    // 检测扩展响应前缀: '+' 或反斜杠长名
    let (extended, body): (bool, &str) = if let Some(stripped) = line.strip_prefix('+') {
        (true, stripped.trim())
    } else if line.starts_with('\\') {
        (true, line)
    } else {
        (false, line)
    };

    // 反斜杠长名 (如 \dump_state, \chk_vfo, \set_freq)
    if let Some(rest) = body.strip_prefix('\\') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("").trim();
        return handle_long(cmd, args, extended, state);
    }

    // 单字符命令
    let mut chars = body.chars();
    let c = match chars.next() {
        Some(c) => c,
        None => return DispatchOut::Reply(String::new()),
    };
    let args = chars.as_str().trim();
    handle_short(c, args, extended, state)
}

// ===== 命令分发 =====

fn handle_short(c: char, args: &str, ext: bool, state: &SharedState) -> DispatchOut {
    match c {
        'q' | 'Q' => DispatchOut::Quit,
        'f' => DispatchOut::Reply(get_freq(ext, state)),
        'F' => DispatchOut::Reply(set_freq(args, ext, state)),
        'm' => DispatchOut::Reply(get_mode(ext, state)),
        'M' => DispatchOut::Reply(set_mode(args, ext)),
        't' => DispatchOut::Reply(get_ptt(ext, state)),
        'T' => DispatchOut::Reply(set_ptt(args, ext, state)),
        'v' => DispatchOut::Reply(get_vfo(ext, state)),
        'V' => DispatchOut::Reply(set_vfo(args, ext, state)),
        's' => DispatchOut::Reply(get_split_vfo(ext, state)),
        'S' => DispatchOut::Reply(set_split_vfo(args, ext, state)),
        'i' => DispatchOut::Reply(get_split_freq(ext, state)),
        'I' => DispatchOut::Reply(set_split_freq(args, ext, state)),
        'c' => DispatchOut::Reply(get_ctcss_tone(ext, state)),
        'C' => DispatchOut::Reply(set_ctcss_tone(args, ext, state)),
        'e' => DispatchOut::Reply(get_ctcss_sql(ext, state)),
        'E' => DispatchOut::Reply(set_ctcss_sql(args, ext, state)),
        'j' => DispatchOut::Reply(get_rit(ext)),
        'J' => DispatchOut::Reply(set_rit(args, ext)),
        '_' => DispatchOut::Reply(get_info(ext)),
        '?' | 'h' => DispatchOut::Reply("RPRT 0\n".to_string()),
        _    => DispatchOut::Reply(RPRT_EINVAL.to_string()),
    }
}

fn handle_long(name: &str, args: &str, ext: bool, state: &SharedState) -> DispatchOut {
    match name {
        "quit" | "exit" | "q" => DispatchOut::Quit,
        "dump_state"   => DispatchOut::Reply(dump_state()),
        "chk_vfo"      => DispatchOut::Reply(if ext { "ChkVFO: 0\nRPRT 0\n".into() } else { "CHKVFO 0\nRPRT 0\n".into() }),
        "get_powerstat"=> DispatchOut::Reply(get_powerstat(ext, state)),
        "set_powerstat"=> DispatchOut::Reply(set_powerstat(args, ext, state)),
        "get_freq"     => DispatchOut::Reply(get_freq(ext, state)),
        "set_freq"     => DispatchOut::Reply(set_freq(args, ext, state)),
        "get_mode"     => DispatchOut::Reply(get_mode(ext, state)),
        "set_mode"     => DispatchOut::Reply(set_mode(args, ext)),
        "get_ptt"      => DispatchOut::Reply(get_ptt(ext, state)),
        "set_ptt"      => DispatchOut::Reply(set_ptt(args, ext, state)),
        "get_vfo"      => DispatchOut::Reply(get_vfo(ext, state)),
        "set_vfo"      => DispatchOut::Reply(set_vfo(args, ext, state)),
        "get_split_vfo"=> DispatchOut::Reply(get_split_vfo(ext, state)),
        "set_split_vfo"=> DispatchOut::Reply(set_split_vfo(args, ext, state)),
        "get_split_freq"=> DispatchOut::Reply(get_split_freq(ext, state)),
        "set_split_freq"=> DispatchOut::Reply(set_split_freq(args, ext, state)),
        "get_rit"      => DispatchOut::Reply(get_rit(ext)),
        "set_rit"      => DispatchOut::Reply(set_rit(args, ext)),
        "get_info"     => DispatchOut::Reply(get_info(ext)),
        "get_vfo_info"   => DispatchOut::Reply(get_vfo_info(ext, state)),
        "set_tone"       => DispatchOut::Reply(set_ctcss_tone(args, ext, state)),
        "get_tone"       => DispatchOut::Reply(get_ctcss_tone(ext, state)),
        "get_ctcss_tone" => DispatchOut::Reply(get_ctcss_tone(ext, state)),
        "set_ctcss_tone" => DispatchOut::Reply(set_ctcss_tone(args, ext, state)),
        "get_ctcss_sql"  => DispatchOut::Reply(get_ctcss_sql(ext, state)),
        "set_ctcss_sql"  => DispatchOut::Reply(set_ctcss_sql(args, ext, state)),
        _ => DispatchOut::Reply(RPRT_EPROTO.to_string()),
    }
}

// ===== 工具函数 =====

fn side_name(is_left: bool) -> &'static str {
    if is_left { "LEFT" } else { "RIGHT" }
}

fn session_alive(state: &SharedState, session_id: u32) -> bool {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    s.rigctld_clients > 0 && s.rigctld_sat_active && s.rigctld_session_id == session_id
}

fn wait_pending_clear(state: &SharedState, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let busy = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.key_override.is_some() || s.key_release || s.knob_inject.is_some() || s.vol_changed || s.sql_changed
        };
        if !busy {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn clear_pending_injections(s: &mut RadioState) {
    s.key_override = None;
    s.key_release = false;
    s.knob_inject = None;
    s.vol_changed = false;
    s.sql_changed = false;
    s.sql_override_side_is_left = None;
}

fn queue_sql_inject(s: &mut RadioState, is_left: bool, adc: u16) {
    s.sql_override = Some(adc);
    s.sql_override_side_is_left = Some(is_left);
    s.sql_changed = true;
}

fn restore_forced_rx_sql(state: &SharedState) {
    let restore = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        match (s.rigctld_rx_sql_forced_side.take(), s.rigctld_rx_sql_saved_adc.take()) {
            (Some(side), Some(adc)) => {
                clear_pending_injections(&mut s);
                queue_sql_inject(&mut s, side, adc);
                log::info!("[SatSession] 恢复上次 RX {} SQL ADC={}", side_name(side), adc);
                true
            }
            _ => false,
        }
    };
    if restore && !wait_pending_clear(state, Duration::from_secs(2)) {
        log::warn!("[SatSession] 等待旧 SQL 恢复帧发送超时，继续新会话初始化");
    }
}

fn reset_sat_setup_state(s: &mut RadioState) {
    s.rigctld_rx_pending_hz = None;
    s.rigctld_tx_pending_hz = None;
    s.rigctld_rx_pending_since_us = 0;
    s.rigctld_tx_pending_since_us = 0;
    s.rigctld_setup_rx_hz = None;
    s.rigctld_setup_tx_hz = None;
    s.rigctld_setup_snapshot_ready = false;
    s.rigctld_setup_attempts = 0;
    s.rigctld_setup_retry_after_us = 0;
    s.rigctld_rx_target_hz = None;
    s.rigctld_tx_target_hz = None;
    s.rigctld_rx_initial_attempted = false;
    s.rigctld_tx_initial_attempted = false;
    s.rigctld_rx_initial_done = false;
    s.rigctld_tx_initial_done = false;
    s.rigctld_rx_step_ready = false;
    s.rigctld_tx_step_ready = false;
    s.rigctld_sat_retry_after_us = 0;
    s.rigctld_rx_last_step_us = 0;
    s.rigctld_tx_last_step_us = 0;
    s.rigctld_tx_ctcss_tone = 0;
}

fn begin_sat_session(state: &SharedState) {
    restore_forced_rx_sql(state);
    if !wait_pending_clear(state, Duration::from_secs(1)) {
        log::warn!("[SatSession] 新会话开始前仍有旧 pending 注入，清空后继续");
    }
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    clear_pending_injections(&mut s);
    s.rigctld_session_id = s.rigctld_session_id.wrapping_add(1);
    bind_sat_session(&mut s);
}

fn bind_sat_session(s: &mut RadioState) {
    let rx_is_left = if s.left.is_main {
        true
    } else if s.right.is_main {
        false
    } else {
        log::warn!("[SatSession #{}] MAIN 位置未知，暂按 LEFT 作为 RX；等待 MAIN 探测后下次连接会重新采样", s.rigctld_session_id);
        true
    };
    let tx_is_left = !rx_is_left;
    s.rigctld_sat_active = true;
    s.rigctld_sat_split_enabled = false;
    s.rigctld_sat_rx_is_left = rx_is_left;
    s.rigctld_sat_tx_is_left = tx_is_left;
    reset_sat_setup_state(s);
    s.rigctld_setup_running = false;
    log::info!("[SatSession #{}] 绑定本次 DTrac 会话: RX={} TX={}（连接时 MAIN 作为 RX）", s.rigctld_session_id, side_name(rx_is_left), side_name(tx_is_left));
}

fn clear_sat_session(s: &mut RadioState) {
    reset_sat_setup_state(s);
    s.rigctld_sat_active = false;
    s.rigctld_sat_split_enabled = false;
    s.rigctld_setup_running = false;
    s.rigctld_rx_sql_forced_side = None;
    s.rigctld_rx_sql_saved_adc = None;
}

fn clear_side_menu_display(s: &mut RadioState, is_left: bool) {
    let band = if is_left { &mut s.left } else { &mut s.right };
    band.is_set = false;
    band.menu_text.clear();
    band.menu_in_value = false;
    band.menu_exit_count = 0;
    band.display_text.clear();
    s.head_count = s.head_count.wrapping_add(1);
}

fn prepare_sat_setup_snapshot(s: &mut RadioState, now_us: u64) {
    if s.rigctld_setup_snapshot_ready || s.rigctld_setup_running {
        return;
    }
    let (rx, tx, rx_since, tx_since) = match (
        s.rigctld_rx_pending_hz,
        s.rigctld_tx_pending_hz,
        s.rigctld_rx_pending_since_us,
        s.rigctld_tx_pending_since_us,
    ) {
        (Some(rx), Some(tx), rx_since, tx_since) if rx_since != 0 && tx_since != 0 => (rx, tx, rx_since, tx_since),
        _ => return,
    };
    let newest = rx_since.max(tx_since);
    if now_us.saturating_sub(newest) < SAT_SETUP_SNAPSHOT_US {
        return;
    }
    s.rigctld_setup_rx_hz = Some(rx);
    s.rigctld_setup_tx_hz = Some(tx);
    s.rigctld_setup_snapshot_ready = true;
    s.rigctld_rx_target_hz = Some(rx);
    s.rigctld_tx_target_hz = Some(tx);
    s.rigctld_rx_initial_attempted = false;
    s.rigctld_tx_initial_attempted = false;
    s.rigctld_rx_initial_done = false;
    s.rigctld_tx_initial_done = false;
    s.rigctld_rx_step_ready = false;
    s.rigctld_tx_step_ready = false;
    s.rigctld_sat_retry_after_us = 0;
    log::info!(
        "[SatSession #{}] 生成初始化快照 RX {}={} TX {}={}，后续 Doppler 不改写快照",
        s.rigctld_session_id,
        side_name(s.rigctld_sat_rx_is_left), rx,
        side_name(s.rigctld_sat_tx_is_left), tx,
    );
}


fn adopt_stable_sat_targets(s: &mut RadioState, now_us: u64) {
    if let Some(rx) = s.rigctld_rx_pending_hz {
        if s.rigctld_rx_target_hz != Some(rx)
            && s.rigctld_rx_pending_since_us != 0
            && now_us.saturating_sub(s.rigctld_rx_pending_since_us) >= RIG_TRACK_INTERVAL_US
        {
            s.rigctld_rx_target_hz = Some(rx);
            log::info!("[SatSession] RX {} target 稳定 5s，采纳 {}{}",
                side_name(s.rigctld_sat_rx_is_left), rx,
                if s.rigctld_setup_snapshot_ready { "（后续 Doppler 更新）" } else { "（等待初始化快照）" });
        }
    }
    if let Some(tx) = s.rigctld_tx_pending_hz {
        if s.rigctld_tx_target_hz != Some(tx)
            && s.rigctld_tx_pending_since_us != 0
            && now_us.saturating_sub(s.rigctld_tx_pending_since_us) >= RIG_TRACK_INTERVAL_US
        {
            s.rigctld_tx_target_hz = Some(tx);
            log::info!("[SatSession] TX {} target 稳定 5s，采纳 {}{}",
                side_name(s.rigctld_sat_tx_is_left), tx,
                if s.rigctld_setup_snapshot_ready { "（后续 Doppler 更新）" } else { "（等待初始化快照）" });
        }
    }
}


fn mode_band(state: &SharedState) -> crate::state::BandState {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    if s.rigctld_sat_active {
        if s.rigctld_sat_rx_is_left { s.left.clone() } else { s.right.clone() }
    } else if s.right.is_main { s.right.clone() } else { s.left.clone() }
}

fn tx_band(state: &SharedState) -> crate::state::BandState {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    if s.rigctld_sat_active {
        if s.rigctld_sat_tx_is_left { s.left.clone() } else { s.right.clone() }
    } else if s.right.is_main { s.right.clone() } else { s.left.clone() }
}

/// 把 freq 字符串如 "433.550.500" 解析为 Hz（u64）
/// 兼容 "433.550" / "433.550.000" / "433"
fn freq_str_to_hz(s: &str) -> Option<u64> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() { return None; }
    // 找到第一个 '.' 之前是 MHz 整数；其后是 kHz 子部分
    // 协议格式: "MHZ.kkk.uuu" → MHz = MHZ整数; sub = kkk*1000 + uuu (Hz)
    let parts: Vec<&str> = cleaned.split('.').collect();
    let mhz: u64 = parts.get(0)?.parse().ok()?;
    let mut hz: u64 = mhz * 1_000_000;
    if let Some(p1) = parts.get(1) {
        // p1 是 kHz 部分（最多 3 位数字，代表 0-999 kHz），但每位代表 100kHz/10kHz/1kHz
        let p1_padded = format!("{:0<3}", p1);
        let khz: u64 = p1_padded[..3].parse().ok()?;
        hz += khz * 1_000;
    }
    if let Some(p2) = parts.get(2) {
        // p2 是 100Hz/10Hz/1Hz 部分（最多 3 位）
        let p2_padded = format!("{:0<3}", p2);
        let sub: u64 = p2_padded[..3].parse().ok()?;
        hz += sub;
    }
    Some(hz)
}

/// 解析 Hamlib/DTrac 频率参数：支持 Hz 整数、Hz 浮点、MHz 浮点；最终只保留 kHz 精度。
fn parse_freq_arg(s: &str) -> Option<u64> {
    if s.is_empty() { return None; }
    if let Ok(v) = s.parse::<u64>() {
        return Some((v / 1_000) * 1_000);
    }
    let f = s.parse::<f64>().ok()?;
    if !f.is_finite() || f <= 0.0 { return None; }
    let hz = if f < 2_000.0 { f * 1_000_000.0 } else { f };
    Some(((hz as u64) / 1_000) * 1_000)
}

/// 从 Hamlib set_freq 参数中找出频率 token，兼容可选 VFO 前缀。
fn parse_freq_args(args: &str) -> Option<(&str, u64)> {
    let mut parsed = None;
    for token in args.split_whitespace() {
        if let Some(hz) = parse_freq_arg(token) {
            parsed = Some((token, hz));
        }
    }
    parsed
}

fn side_is_main(s: &RadioState, is_left: bool) -> bool {
    if is_left { s.left.is_main } else { s.right.is_main }
}

fn wait_main_side(state: &SharedState, target_left: bool, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            if side_is_main(&s, target_left) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ensure_main_side(state: &SharedState, target_left: bool) -> bool {
    {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        if side_is_main(&s, target_left) {
            return true;
        }
    }
    log::info!("[SatSession] 切 MAIN 到 {}", side_name(target_left));
    inject_key_wait(state, 0x10);
    let ok = wait_main_side(state, target_left, Duration::from_secs(3));
    if !ok {
        log::warn!("[SatSession] 等待 MAIN 切到 {} 超时", side_name(target_left));
    }
    ok
}

fn side_tone_mode(s: &RadioState, is_left: bool) -> ToneMode {
    let band = if is_left { &s.left } else { &s.right };
    if band.tone_dcs {
        ToneMode::Dcs
    } else if band.tone_enc && band.tone_dec {
        ToneMode::EncDec
    } else if band.tone_enc {
        ToneMode::Enc
    } else {
        ToneMode::Off
    }
}

fn sat_clear_rx_tone_if_needed(state: &SharedState, rx_is_left: bool) -> bool {
    let mode = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        side_tone_mode(&s, rx_is_left)
    };
    if mode == ToneMode::Off {
        log::info!("[SatSession] RX {} 亚音已是 OFF", side_name(rx_is_left));
        return true;
    }
    log::info!("[SatSession] RX {} 检测到残留亚音 {}，切到 OFF", side_name(rx_is_left), tone_mode_name(mode));
    if !ensure_main_side(state, rx_is_left) {
        log::warn!("[SatSession] RX {} 残留亚音清理失败：无法切 MAIN", side_name(rx_is_left));
        return false;
    }
    let ok = inject_tone_mode(state, ToneMode::Off);
    let final_mode = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        side_tone_mode(&s, rx_is_left)
    };
    if final_mode == ToneMode::Off {
        log::info!("[SatSession] RX {} 残留亚音清理完成：{}", side_name(rx_is_left), tone_mode_name(final_mode));
        true
    } else {
        log::warn!("[SatSession] RX {} 残留亚音清理未确认：{}", side_name(rx_is_left), tone_mode_name(final_mode));
        ok && final_mode == ToneMode::Off
    }
}

fn sat_apply_tx_ctcss(state: &SharedState, mode: ToneMode) {
    let (tx_is_left, tone) = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        (s.rigctld_sat_tx_is_left, s.rigctld_tx_ctcss_tone)
    };
    if !ensure_main_side(state, tx_is_left) {
        log::warn!("[SatSession] TX CTCSS 设置失败：无法切到 TX {}", side_name(tx_is_left));
        return;
    }
    if tone == 0 {
        let ok = inject_tone_mode(state, ToneMode::Off);
        if !ok {
            log::warn!("[SatSession] TX {} TONE OFF 未确认", side_name(tx_is_left));
        }
        return;
    }
    if let Some(idx) = CTCSS_TONES_TENTH_HZ.iter().position(|&t| t == tone) {
        log::info!("[SatSession] TX {} 设置 CTCSS {}Hz", side_name(tx_is_left), tone as f32 / 10.0);
        let tone_ok = inject_menu_set(state, 30, CTCSS_TONE_STRS[idx], false);
        if tone_ok {
            if !inject_tone_mode(state, mode) {
                log::warn!("[SatSession] TX CTCSS TONE 模式未确认到 {}", tone_mode_name(mode));
            }
        } else {
            log::warn!("[SatSession] TX CTCSS 频率未验证，跳过 TONE 模式按键，避免误取消亚音");
        }
        log::info!("[SatSession] TX CTCSS 设置{}", if tone_ok { "完成" } else { "未验证" });
    } else {
        log::warn!("[SatSession] TX CTCSS {} 不在 TH-9800 标准表中", tone);
    }
}

fn sat_tx_ready_for_ctcss(s: &RadioState) -> bool {
    s.rigctld_sat_active
        && s.rigctld_tx_initial_done
        && s.rigctld_tx_step_ready
        && !s.rigctld_setup_running
        && !s.macro_running
}

fn sat_open_rx_squelch(state: &SharedState, rx_is_left: bool) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    if s.rigctld_rx_sql_forced_side == Some(rx_is_left) {
        log::info!("[SatSession #{}] RX {} SQL=0 已在本会话打开，跳过重复注入", s.rigctld_session_id, side_name(rx_is_left));
        return;
    }
    let saved = if rx_is_left { s.left.sql } else { s.right.sql };
    s.rigctld_rx_sql_forced_side = Some(rx_is_left);
    s.rigctld_rx_sql_saved_adc = Some(saved);
    queue_sql_inject(&mut s, rx_is_left, SAT_RX_SQL_OPEN_ADC);
    if rx_is_left {
        s.left.sql = SAT_RX_SQL_OPEN_ADC;
    } else {
        s.right.sql = SAT_RX_SQL_OPEN_ADC;
    }
    s.head_count = s.head_count.wrapping_add(1);
    log::info!("[SatSession #{}] RX {} 静噪设为 0%，保存原 ADC={}", s.rigctld_session_id, side_name(rx_is_left), saved);
}

struct SatSideSetupResult {
    freq_input_done: bool,
    step_verified: bool,
}

fn sat_setup_side(state: &SharedState, is_left: bool, target_hz: u64, role: &str, session_id: u32) -> SatSideSetupResult {
    if !ensure_main_side(state, is_left) {
        return SatSideSetupResult { freq_input_done: false, step_verified: false };
    }
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.macro_running = true;
        s.key_override = None;
        s.key_release = false;
        s.knob_inject = None;
    }
    log::info!("[SatSession] {} {} 键盘输入频率 {}", role, side_name(is_left), target_hz);
    if !inject_freq_keyboard(state, target_hz, Some(session_id)) {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.macro_running = false;
        return SatSideSetupResult { freq_input_done: false, step_verified: false };
    }
    std::thread::sleep(Duration::from_millis(1200));
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        clear_side_menu_display(&mut s, is_left);
        s.macro_running = false;
    }

    log::info!("[SatSession] {} {} 设置 STEP=2.5kHz", role, side_name(is_left));
    let step_verified = inject_menu_set(state, 28, "2.5", false);
    SatSideSetupResult { freq_input_done: true, step_verified }
}

fn sat_setup_one_stage(state: &SharedState, rx_hz: u64, tx_hz: u64, rx_is_left: bool, tx_is_left: bool, session_id: u32) {
    if !session_alive(state, session_id) { return; }

    let stage = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.rigctld_rx_initial_done {
            Some(("RX_FREQ", rx_is_left, rx_hz))
        } else if !s.rigctld_rx_step_ready {
            Some(("RX_STEP", rx_is_left, rx_hz))
        } else if !s.rigctld_tx_initial_done {
            Some(("TX_FREQ", tx_is_left, tx_hz))
        } else if !s.rigctld_tx_step_ready {
            Some(("TX_STEP", tx_is_left, tx_hz))
        } else {
            None
        }
    };

    let Some((stage, is_left, target_hz)) = stage else {
        let main_ok = ensure_main_side(state, rx_is_left);
        if main_ok && session_alive(state, session_id) {
            sat_open_rx_squelch(state, rx_is_left);
            let _ = wait_pending_clear(state, Duration::from_secs(2));
        }
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.rigctld_session_id == session_id {
            s.rigctld_setup_running = false;
            log::info!("[SatSession #{}] 初始设置完成，MAIN 已恢复 RX {}", session_id, side_name(rx_is_left));
        }
        return;
    };

    log::info!("[SatSession #{}] 单阶段初始化 {} {}", session_id, stage, side_name(is_left));
    let ok = match stage {
        "RX_FREQ" | "TX_FREQ" => sat_setup_frequency_only(state, is_left, target_hz, stage, session_id),
        "RX_STEP" | "TX_STEP" => sat_setup_step_only(state, is_left, stage, session_id),
        _ => false,
    };

    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    if s.rigctld_session_id != session_id {
        s.rigctld_setup_running = false;
        s.macro_running = false;
        return;
    }

    let now = unsafe { esp_timer_get_time() } as u64;
    match stage {
        "RX_FREQ" => s.rigctld_rx_initial_done = ok,
        "RX_STEP" => s.rigctld_rx_step_ready = ok,
        "TX_FREQ" => s.rigctld_tx_initial_done = ok,
        "TX_STEP" => s.rigctld_tx_step_ready = ok,
        _ => {}
    }
    s.rigctld_setup_running = false;
    s.rigctld_rx_last_step_us = now;
    s.rigctld_tx_last_step_us = now;

    if ok {
        log::info!("[SatSession #{}] {} 完成", session_id, stage);
    } else {
        s.rigctld_sat_retry_after_us = now + SAT_SETUP_RETRY_US;
        log::warn!("[SatSession #{}] {} 失败，仅重试当前阶段，{}s 后允许重试", session_id, stage, SAT_SETUP_RETRY_US / 1_000_000);
    }
}

fn sat_setup_frequency_only(state: &SharedState, is_left: bool, target_hz: u64, role: &str, session_id: u32) -> bool {
    if !ensure_main_side(state, is_left) {
        return false;
    }
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.macro_running = true;
        s.key_override = None;
        s.key_release = false;
        s.knob_inject = None;
    }
    log::info!("[SatSession] {} {} 键盘输入频率 {}", role, side_name(is_left), target_hz);
    let ok = inject_freq_keyboard(state, target_hz, Some(session_id));
    std::thread::sleep(Duration::from_millis(800));
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    clear_side_menu_display(&mut s, is_left);
    s.macro_running = false;
    ok
}

fn sat_setup_step_only(state: &SharedState, is_left: bool, role: &str, _session_id: u32) -> bool {
    if !ensure_main_side(state, is_left) {
        return false;
    }
    log::info!("[SatSession] {} {} 设置 STEP=2.5kHz", role, side_name(is_left));
    inject_menu_set(state, 28, "2.5", false)
}

fn sat_initial_setup(state: &SharedState, rx_hz: u64, tx_hz: u64, rx_is_left: bool, tx_is_left: bool, session_id: u32) {
    if !session_alive(state, session_id) { return; }
    let rx_tone_ok = sat_clear_rx_tone_if_needed(state, rx_is_left);
    if !rx_tone_ok {
        log::warn!("[SatSession #{}] RX {} 残留亚音清理未确认，继续初始化但不提前打开 SQL", session_id, side_name(rx_is_left));
    }
    if !session_alive(state, session_id) { return; }
    let rx = sat_setup_side(state, rx_is_left, rx_hz, "RX", session_id);
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.rigctld_session_id != session_id {
            s.rigctld_setup_running = false;
            s.macro_running = false;
            return;
        }
        s.rigctld_rx_initial_attempted = rx.freq_input_done;
        s.rigctld_rx_initial_done = rx.freq_input_done;
        s.rigctld_rx_step_ready = rx.step_verified;
        if !rx.freq_input_done {
            s.rigctld_tx_initial_attempted = false;
            s.rigctld_setup_running = false;
            s.rigctld_sat_retry_after_us = unsafe { esp_timer_get_time() } as u64 + SAT_SETUP_RETRY_US;
            log::warn!("[SatSession #{}] RX 初始频率输入失败，{}s 后才允许重试", session_id, SAT_SETUP_RETRY_US / 1_000_000);
            return;
        }
        if !rx.step_verified {
            s.rigctld_setup_running = false;
            s.rigctld_sat_retry_after_us = unsafe { esp_timer_get_time() } as u64 + SAT_SETUP_RETRY_US;
            log::warn!("[SatSession #{}] RX 频率已写入但 STEP 未验证，暂停自动重试 {}s", session_id, SAT_SETUP_RETRY_US / 1_000_000);
            return;
        }
    }
    if !session_alive(state, session_id) { return; }

    let tx = sat_setup_side(state, tx_is_left, tx_hz, "TX", session_id);
    let tx_ok = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.rigctld_session_id != session_id {
            s.rigctld_setup_running = false;
            s.macro_running = false;
            return;
        }
        s.rigctld_tx_initial_attempted = tx.freq_input_done;
        s.rigctld_tx_initial_done = tx.freq_input_done;
        s.rigctld_tx_step_ready = tx.step_verified;
        let now = unsafe { esp_timer_get_time() } as u64;
        s.rigctld_rx_last_step_us = now;
        s.rigctld_tx_last_step_us = now;
        if tx.freq_input_done && !tx.step_verified {
            s.rigctld_setup_running = false;
            s.rigctld_sat_retry_after_us = now + SAT_SETUP_RETRY_US;
            log::warn!("[SatSession #{}] TX 频率已写入但 STEP 未验证，暂停自动重试 {}s", session_id, SAT_SETUP_RETRY_US / 1_000_000);
        }
        tx.freq_input_done && tx.step_verified
    };

    if tx_ok && session_alive(state, session_id) {
        let tone = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.rigctld_tx_ctcss_tone
        };
        if tone > 0 {
            log::warn!("[SatSession #{}] TX CTCSS {}Hz 已记录但暂不执行，避免菜单宏触发栈溢出重启", session_id, tone as f32 / 10.0);
        }
    }

    let main_tx_ok = tx_ok && wait_main_side(state, tx_is_left, Duration::from_secs(1));
    if main_tx_ok && session_alive(state, session_id) {
        sat_open_rx_squelch(state, rx_is_left);
        let sql_flushed = wait_pending_clear(state, Duration::from_secs(2));
        if !sql_flushed {
            log::warn!("[SatSession #{}] RX {} SQL=0 注入等待超时", session_id, side_name(rx_is_left));
        }
    }

    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    if s.rigctld_session_id != session_id {
        s.rigctld_setup_running = false;
        s.macro_running = false;
        return;
    }
    s.rigctld_setup_running = false;
    if main_tx_ok {
        log::info!("[SatSession #{}] 初始设置完成，MAIN 保持 TX {}", session_id, side_name(tx_is_left));
    } else {
        log::warn!("[SatSession #{}] 初始设置未完成：rx_ok={} tx_ok={} main_tx_ok={}", session_id, rx.step_verified, tx_ok, main_tx_ok);
    }
}
fn rigctld_initial_setup(state: &SharedState, target: u64) {
    log::info!("[RigctldSetup] 设置初始频率 {}", target);
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.macro_running = true;
        s.key_override = None;
        s.key_release = false;
        s.knob_inject = None;
    }
    if !inject_freq_keyboard(state, target, None) {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.rigctld_setup_running = false;
        s.macro_running = false;
        return;
    }
    std::thread::sleep(Duration::from_millis(1200));
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        let main_is_left = s.left.is_main;
        clear_side_menu_display(&mut s, main_is_left);
        s.rigctld_initial_freq_done = true;
        s.macro_running = false;
    }

    let has_pending_ctcss = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.rigctld_ctcss_tone > 0
    };
    if has_pending_ctcss {
        log::warn!("[RigctldSetup] CTCSS 已记录但暂不执行，STEP 后退出 SET 菜单");
    }
    log::info!("[RigctldSetup] 初始频率完成，设置 STEP=2.5kHz，完成后退出到频率页");
    let step_ok = inject_menu_set(state, 28, "2.5", false);
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.rigctld_step_ready = step_ok;
        s.rigctld_setup_running = false;
        s.rigctld_last_step_us = unsafe { esp_timer_get_time() } as u64;
    }
    if step_ok {
        log::info!("[RigctldSetup] STEP 设置验证通过，进入 DTrac 限速追踪");
    } else {
        log::warn!("[RigctldSetup] STEP 设置未验证通过，暂停 DTrac 追踪");
    }
}


fn get_freq(ext: bool, state: &SharedState) -> String {
    // split 会话中 get_freq 返回 RX 频率；普通模式返回 MAIN 目标/实际频率。
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let hz = if s.rigctld_sat_active {
        match s.rigctld_rx_pending_hz.or(s.rigctld_rx_target_hz) {
            Some(t) => t,
            None => {
                let band = if s.rigctld_sat_rx_is_left { &s.left } else { &s.right };
                freq_str_to_hz(band.freq.as_str()).unwrap_or(0)
            }
        }
    } else {
        match s.rigctld_target_hz {
            Some(t) => t,
            None => {
                let band = if s.right.is_main { &s.right } else { &s.left };
                freq_str_to_hz(band.freq.as_str()).unwrap_or(0)
            }
        }
    };
    drop(s);
    if ext {
        format!("get_freq:\nFreq: {}\nRPRT 0\n", hz)
    } else {
        format!("{}\n", hz)
    }
}

fn set_freq(args: &str, ext: bool, state: &SharedState) -> String {
    let (hz_str, hz) = match parse_freq_args(args) {
        Some(v) => v,
        None => {
            log::warn!("[Rigctld] set_freq 参数无法解析: {:?}", args);
            return RPRT_EINVAL.to_string();
        }
    };
    let target = ((hz + RIG_STEP_HZ / 2) / RIG_STEP_HZ) * RIG_STEP_HZ;

    if target < 26_000_000 || target > 1_300_000_000 {
        log::warn!("[Rigctld] set_freq 频率越界: {}", target);
        return RPRT_EINVAL.to_string();
    }

    let now_us = unsafe { esp_timer_get_time() } as u64;
    let mut sat_log: Option<bool> = None;
    let mut throttled_log = false;
    let start_setup = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.rigctld_sat_active {
            let changed = s.rigctld_rx_pending_hz != Some(target);
            let due = s.rigctld_rx_pending_since_us == 0
                || now_us.saturating_sub(s.rigctld_rx_pending_since_us) >= RIG_TRACK_INTERVAL_US;
            if changed || due {
                s.rigctld_rx_pending_hz = Some(target);
                s.rigctld_rx_pending_since_us = now_us;
                sat_log = Some(s.rigctld_sat_rx_is_left);
            }
            if s.rigctld_setup_snapshot_ready {
                s.rigctld_rx_target_hz = Some(target);
            }
            s.rigctld_target_hz = None;
            false
        } else {
            let changed = s.rigctld_target_hz != Some(target);
            let due = now_us.saturating_sub(s.rigctld_last_step_us) >= RIG_TRACK_INTERVAL_US;
            s.rigctld_target_hz = Some(target);
            throttled_log = changed || due;
            if throttled_log {
                s.rigctld_last_step_us = now_us;
            }
            if !s.rigctld_initial_freq_done && !s.rigctld_setup_running {
                s.rigctld_setup_running = true;
                true
            } else {
                false
            }
        }
    };

    if let Some(rx_is_left) = sat_log {
        log::info!("[SatSession] F/set_freq → RX {} pending={}（5s 节流采样）", side_name(rx_is_left), target);
    } else if start_setup {
        log::info!("[Rigctld] 首个 set_freq={}，交由 freq_stepper 执行初始频率+STEP 设置", target);
    } else if throttled_log {
        log::info!("[Rigctld] set_freq target={}（5s 节流采样）", target);
    }

    if ext { format!("set_freq: {}\nFreq: {}\nRPRT 0\n", hz_str, target) }
    else   { RPRT_OK.to_string() }
}

fn get_mode(ext: bool, state: &SharedState) -> String {
    let band = mode_band(state);
    let mode = match band.mode.as_str() {
        "AM" => "AM",
        _ => "FM",  // 默认 FM
    };
    let bw = if mode == "AM" { 8000 } else { 12500 };
    if ext { format!("get_mode:\nMode: {}\nPassband: {}\nRPRT 0\n", mode, bw) }
    else   { format!("{}\n{}\n", mode, bw) }
}

fn set_mode(_args: &str, ext: bool) -> String {
    // TH-9800 无独立 AM/FM 切换键；按频段自动选择，这里只 ack
    if ext { "set_mode:\nRPRT 0\n".to_string() } else { RPRT_OK.to_string() }
}

fn get_ptt(ext: bool, state: &SharedState) -> String {
    let band = tx_band(state);
    let ptt = if band.is_tx { 1 } else { 0 };
    if ext { format!("get_ptt:\nPTT: {}\nRPRT 0\n", ptt) }
    else   { format!("{}\n", ptt) }
}

fn sat_ptt_ready(s: &RadioState) -> bool {
    if !s.rigctld_sat_active || !s.rigctld_sat_split_enabled {
        return true;
    }
    if s.macro_running || s.rigctld_setup_running {
        return false;
    }
    let tx_band = if s.rigctld_sat_tx_is_left { &s.left } else { &s.right };
    if !tx_band.is_main || tx_band.is_set {
        return false;
    }
    let target = match s.rigctld_tx_target_hz {
        Some(t) => t,
        None => return false,
    };
    let current = match freq_str_to_hz(tx_band.freq.as_str()) {
        Some(v) => v,
        None => return false,
    };
    let diff = if current > target { current - target } else { target - current };
    diff <= RIG_STEP_HZ
}

fn set_ptt(args: &str, ext: bool, state: &SharedState) -> String {
    let v = args.split_whitespace().next().unwrap_or("");
    let on = match v {
        "0" => false,
        "1" => true,
        _ => return RPRT_EINVAL.to_string(),
    };
    let now_us = unsafe { esp_timer_get_time() } as u64;
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if on && !sat_ptt_ready(&s) {
            s.ptt_override = false;
            log::warn!(
                "[SatSession] 拒绝 PTT：MAIN={} TX={} target={:?} actual={} macro={} setup={}",
                if s.right.is_main { "RIGHT" } else { "LEFT" },
                side_name(s.rigctld_sat_tx_is_left),
                s.rigctld_tx_target_hz,
                if s.rigctld_sat_tx_is_left { s.left.freq.as_str() } else { s.right.freq.as_str() },
                s.macro_running,
                s.rigctld_setup_running,
            );
            return RPRT_EPROTO.to_string();
        }
        s.ptt_override = on;
        if on { s.ptt_start_us = now_us; }
    }
    log::info!("[Rigctld] set_ptt: {}", if on { "ON" } else { "OFF" });
    if ext { format!("set_ptt: {}\nRPRT 0\n", v) } else { RPRT_OK.to_string() }
}

fn get_vfo(ext: bool, state: &SharedState) -> String {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let v = if s.rigctld_sat_active {
        "VFOA"
    } else if s.left.is_main {
        "VFOA"
    } else {
        "VFOB"
    };
    if ext { format!("get_vfo:\nVFO: {}\nRPRT 0\n", v) }
    else   { format!("{}\n", v) }
}

fn set_vfo(args: &str, ext: bool, state: &SharedState) -> String {
    let target_left = match args.split_whitespace().next().unwrap_or("") {
        "VFOA" | "Main" | "main" => true,
        "VFOB" | "Sub" | "sub"   => false,
        _ => return RPRT_EINVAL.to_string(),
    };
    {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.rigctld_sat_active {
            log::info!("[SatSession #{}] set_vfo 在卫星会话内只 ACK，不物理切 MAIN", s.rigctld_session_id);
            return if ext { "set_vfo:\nRPRT 0\n".to_string() } else { RPRT_OK.to_string() };
        }
        let already = (target_left && s.left.is_main) || (!target_left && s.right.is_main);
        if already {
            return if ext { "set_vfo:\nRPRT 0\n".into() } else { RPRT_OK.to_string() };
        }
    }
    // 注入 P1 (0x10) 切换 MAIN
    state.lock().unwrap_or_else(|e| e.into_inner()).key_override = Some(0x10);
    log::info!("[Rigctld] set_vfo: 注入 P1 切 MAIN");
    if ext { "set_vfo:\nRPRT 0\n".to_string() } else { RPRT_OK.to_string() }
}

fn get_split_vfo(ext: bool, state: &SharedState) -> String {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let split = if s.rigctld_sat_split_enabled { 1 } else { 0 };
    let tx_vfo = if s.rigctld_sat_tx_is_left { "VFOA" } else { "VFOB" };
    if ext { format!("get_split_vfo:\nSplit: {}\nTX VFO: {}\nRPRT 0\n", split, tx_vfo) }
    else   { format!("{}\n{}\n", split, tx_vfo) }
}

fn set_split_vfo(args: &str, ext: bool, state: &SharedState) -> String {
    let mut tokens = args.split_whitespace();
    let split = match tokens.next().unwrap_or("") {
        "0" => false,
        "1" => true,
        _ => return RPRT_EINVAL.to_string(),
    };
    let tx_vfo = tokens.next().unwrap_or("VFOB");
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.rigctld_sat_active {
            log::warn!("[SatSession] set_split_vfo 时会话未绑定，等待下一次连接重新采样 MAIN");
            return if ext { "set_split_vfo:\nRPRT 0\n".into() } else { RPRT_OK.to_string() };
        }
        s.rigctld_sat_split_enabled = split;
    }
    let (rx_is_left, tx_is_left) = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        (s.rigctld_sat_rx_is_left, s.rigctld_sat_tx_is_left)
    };
    log::info!("[SatSession] set_split_vfo split={} RX={} TX={} (DTrac TX VFO {}, 保留连接时物理映射)", split, side_name(rx_is_left), side_name(tx_is_left), tx_vfo);
    if ext { "set_split_vfo:\nRPRT 0\n".into() } else { RPRT_OK.to_string() }
}

fn get_split_freq(ext: bool, state: &SharedState) -> String {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let hz = match s.rigctld_tx_pending_hz.or(s.rigctld_tx_target_hz) {
        Some(t) => t,
        None => {
            let band = if s.rigctld_sat_tx_is_left { &s.left } else { &s.right };
            freq_str_to_hz(band.freq.as_str()).unwrap_or(0)
        }
    };
    if ext { format!("get_split_freq:\nTx freq: {}\nRPRT 0\n", hz) }
    else   { format!("{}\n", hz) }
}

fn set_split_freq(args: &str, ext: bool, state: &SharedState) -> String {
    let (hz_str, hz) = match parse_freq_args(args) {
        Some(v) => v,
        None => {
            log::warn!("[Rigctld] set_split_freq 参数无法解析: {:?}", args);
            return RPRT_EINVAL.to_string();
        }
    };
    let target = ((hz + RIG_STEP_HZ / 2) / RIG_STEP_HZ) * RIG_STEP_HZ;
    if target < 26_000_000 || target > 1_300_000_000 {
        log::warn!("[Rigctld] set_split_freq 频率越界: {}", target);
        return RPRT_EINVAL.to_string();
    }
    let tx_is_left;
    let changed;
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.rigctld_sat_active {
            log::warn!("[SatSession] set_split_freq 时会话未绑定，等待下一次连接重新采样 MAIN");
            return if ext { format!("set_split_freq: {}\nTx freq: {}\nRPRT 0\n", hz_str, target) } else { RPRT_OK.to_string() };
        }
        s.rigctld_sat_split_enabled = true;
        changed = s.rigctld_tx_pending_hz != Some(target);
        if changed {
            s.rigctld_tx_pending_hz = Some(target);
            s.rigctld_tx_pending_since_us = unsafe { esp_timer_get_time() } as u64;
            if s.rigctld_setup_snapshot_ready {
                s.rigctld_tx_target_hz = Some(target);
            }
        }
        s.rigctld_target_hz = None;
        tx_is_left = s.rigctld_sat_tx_is_left;
    }
    if changed {
        log::info!("[SatSession] I/set_split_freq → TX {} pending={}（稳定 5s 后采纳）", side_name(tx_is_left), target);
    }
    if ext { format!("set_split_freq: {}\nTx freq: {}\nRPRT 0\n", hz_str, target) }
    else   { RPRT_OK.to_string() }
}


fn get_rit(ext: bool) -> String {
    if ext { "get_rit:\nRIT: 0\nRPRT 0\n".into() } else { "0\n".into() }
}

fn set_rit(_args: &str, ext: bool) -> String {
    if ext { "set_rit:\nRPRT 0\n".into() } else { RPRT_OK.to_string() }
}

fn get_info(ext: bool) -> String {
    let info = "TYT TH-9800 via elfRadio Box";
    if ext { format!("get_info:\nInfo: {}\nRPRT 0\n", info) }
    else   { format!("{}\n", info) }
}

fn get_powerstat(ext: bool, state: &SharedState) -> String {
    let on = state.lock().unwrap_or_else(|e| e.into_inner()).radio_alive;
    let v = if on { 1 } else { 0 };
    if ext { format!("get_powerstat:\nPower Status: {}\nRPRT 0\n", v) }
    else   { format!("{}\n", v) }
}

fn set_powerstat(_args: &str, ext: bool, _state: &SharedState) -> String {
    // TH-9800 开关机 = GPIO 脉冲 1.2s（与现有 0x28 同等效果）
    // 此处仅占位，避免误触发；用户应通过 elfradio-box [4] 触发
    if ext { "set_powerstat:\nRPRT 0\n".into() } else { RPRT_OK.to_string() }
}

fn get_vfo_info(ext: bool, state: &SharedState) -> String {
    let band = mode_band(state);
    let hz = freq_str_to_hz(band.freq.as_str()).unwrap_or(0);
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let vfo = if s.rigctld_sat_active || s.left.is_main { "VFOA" } else { "VFOB" };
    let mode = if band.mode.as_str() == "AM" { "AM" } else { "FM" };
    let bw = if mode == "AM" { 8000 } else { 12500 };
    if ext {
        format!("get_vfo_info:\nVFO: {}\nFreq: {}\nMode: {}\nWidth: {}\nSplit: 0\nSatMode: 0\nRPRT 0\n", vfo, hz, mode, bw)
    } else {
        format!("{}\n{}\n{}\n{}\n0\n0\n", vfo, hz, mode, bw)
    }
}

// ===== CTCSS / DCS 命令 =====

fn get_ctcss_tone(ext: bool, state: &SharedState) -> String {
    let tone = state.lock().unwrap_or_else(|e| e.into_inner()).rigctld_ctcss_tone;
    if ext { format!("get_ctcss_tone:\nCTCSS Tone: {}\nRPRT 0\n", tone) }
    else   { format!("{}\n", tone) }
}

fn parse_ctcss_arg(args: &str) -> Option<u32> {
    let raw = args.split_whitespace().next().unwrap_or("");
    if raw == "0" || raw == "0.0" { return Some(0); }
    if let Ok(v) = raw.parse::<u32>() {
        return CTCSS_TONES_TENTH_HZ.iter().any(|&t| t == v).then_some(v);
    }
    let hz = raw.parse::<f32>().ok()?;
    if !hz.is_finite() || hz <= 0.0 { return None; }
    let tenth = (hz * 10.0 + 0.5) as u32;
    CTCSS_TONES_TENTH_HZ.iter().any(|&t| t == tenth).then_some(tenth)
}

fn set_ctcss_tone(args: &str, ext: bool, state: &SharedState) -> String {
    let tone = match parse_ctcss_arg(args) {
        Some(v) => v,
        None => return RPRT_EINVAL.to_string(),
    };
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.rigctld_ctcss_tone = tone;
        if s.rigctld_sat_active {
            s.rigctld_tx_ctcss_tone = tone;
        }
    }
    let target_idx = CTCSS_TONES_TENTH_HZ.iter().position(|&t| t == tone);
    log::info!(
        "[Rigctld] set_ctcss_tone: {}（{} Hz），idx={:?}，仅记录并 ACK，暂不执行菜单宏",
        tone,
        tone as f32 / 10.0,
        target_idx
    );
    if ext { "set_ctcss_tone:\nRPRT 0\n".to_string() } else { RPRT_OK.to_string() }
}

fn get_ctcss_sql(ext: bool, state: &SharedState) -> String {
    // SQL CTCSS = 解码器（DEC），返回与 ENC 相同的已知值
    let tone = state.lock().unwrap_or_else(|e| e.into_inner()).rigctld_ctcss_tone;
    if ext { format!("get_ctcss_sql:\nCTCSS Sql: {}\nRPRT 0\n", tone) }
    else   { format!("{}\n", tone) }
}

fn set_ctcss_sql(args: &str, ext: bool, state: &SharedState) -> String {
    let tone = match parse_ctcss_arg(args) {
        Some(v) => v,
        None => return RPRT_EINVAL.to_string(),
    };
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.rigctld_ctcss_tone = tone;
        if s.rigctld_sat_active {
            s.rigctld_tx_ctcss_tone = tone;
        }
    }
    let target_idx = CTCSS_TONES_TENTH_HZ.iter().position(|&t| t == tone);
    log::info!(
        "[Rigctld] set_ctcss_sql: {}（{} Hz），idx={:?}，仅记录并 ACK，暂不执行菜单宏",
        tone,
        tone as f32 / 10.0,
        target_idx
    );
    if ext { "set_ctcss_sql:\nRPRT 0\n".to_string() } else { RPRT_OK.to_string() }
}

// ===== SET 菜单通用导航 =====

/// 进入 SET 菜单，用手咪数字键直接跳到 menu_num，调整值为 target_val，然后退出。
/// 在独立后台线程中调用。使用 macro_running 防止并发；使用浮点匹配防止子串歧义。
fn inject_menu_set(state: &SharedState, menu_num: u8, target_val: &str, keep_menu_open: bool) -> bool {
    // === Guard 1: 等待宏锁（最多 30s），防止并发执行 ===
    let acquired = {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                if !s.macro_running {
                    s.macro_running = true;
                    s.rigctld_target_hz = None;
                    s.key_override = None;
                    s.key_release = false;
                    s.knob_inject = None;
                    break true;
                }
            }
            if std::time::Instant::now() >= deadline {
                log::warn!("[MenuNav] 等待宏锁超时 30s，跳过 #{}", menu_num);
                break false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    if !acquired { return false; }

    // === Guard 2: 前提条件检查 ===
    let (menu_side_is_left, ok) = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        let menu_side_is_left = s.left.is_main;
        let band = if menu_side_is_left { &s.left } else { &s.right };
        let ok = s.radio_alive && !band.is_tx && !band.is_busy;
        if !ok {
            log::warn!(
                "[MenuNav] Guard2 fail #{} target={} main={} alive={} tx={} busy={} s={} set={} menu=\"{}\" display=\"{}\" in_value={} macro={} ptt={}",
                menu_num,
                target_val,
                side_name(menu_side_is_left),
                s.radio_alive,
                band.is_tx,
                band.is_busy,
                band.s_level,
                band.is_set,
                band.menu_text.as_str(),
                band.display_text.as_str(),
                band.menu_in_value,
                s.macro_running,
                s.ptt_override,
            );
        }
        (menu_side_is_left, ok)
    };
    if !ok {
        state.lock().unwrap_or_else(|e| e.into_inner()).macro_running = false;
        return false;
    }

    // === 预计算目标索引（fail-fast：目标值不在列表中直接返回）===
    let list: &[&str] = if menu_num == 30 { &CTCSS_TONE_STRS } else { &STEP_STRS };
    let tgt_idx = match list.iter().position(|&s| s == target_val) {
        Some(i) => i,
        None => {
            log::warn!("[MenuNav] 目标值 \"{}\" 不在列表中，取消 #{}", target_val, menu_num);
            state.lock().unwrap_or_else(|e| e.into_inner()).macro_running = false;
            return false;
        }
    };

    let (dial_click, dial_cw, dial_ccw) = if menu_side_is_left {
        (0x25u8, 0x02u8, 0x01u8)
    } else {
        (0xA5u8, 0x82u8, 0x81u8)
    };

    log::info!("[MenuNav] 开始：目标菜单 #{} = \"{}\"(idx={})", menu_num, target_val, tgt_idx);

    let already_in_set = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        let band = if menu_side_is_left { &s.left } else { &s.right };
        band.is_set
    };
    if !already_in_set {
        inject_key_wait(state, 0x20);
        std::thread::sleep(Duration::from_millis(500));
    }

    // === Step 2: 用手咪数字键直接跳到目标菜单（无需知道起始位置）===
    // 手咪数字键码：0x00='0', 0x01='1', ..., 0x09='9'
    // 菜单 1-9：按 1 个数字；菜单 10-42：按十位 + 个位 两个数字
    if menu_num >= 10 {
        let tens = (menu_num / 10) as u8;
        inject_key_wait(state, tens);
        std::thread::sleep(Duration::from_millis(300));
    }
    let units = (menu_num % 10) as u8;
    inject_key_wait(state, units);
    std::thread::sleep(Duration::from_millis(400));

    // === Step 3: 短按 MAIN DIAL 进入值编辑 ===
    // TH-9800 数字键只跳到菜单项（顶级层），必须按 DIAL click 才进入值编辑
    inject_key_wait(state, dial_click);
    std::thread::sleep(Duration::from_millis(500));

    // === Step 4: 验证+旋钮调整循环（最多 3 次）===
    // wait_for_menu_value 等待 menu_in_value=true，保证读到的是当前值而非菜单名
    let mut verified = false;
    for attempt in 0u8..3 {
        let cur_val = if menu_num == 30 {
            wait_for_menu_value_with_timeout(state, Duration::from_millis(1500))
        } else {
            wait_for_menu_value(state)
        };
        if cur_val.is_empty() {
            log::warn!("[MenuNav] attempt={}: 菜单值未显示（超时），放弃验证", attempt);
            break;
        }
        let cur_idx = match menu_value_index(menu_num, cur_val.as_str()) {
            Some(idx) => idx,
            None => {
                log::warn!("[MenuNav] attempt={}: 无法识别菜单值 {:?}，放弃验证", attempt, cur_val.as_str());
                break;
            }
        };

        log::info!("[MenuNav] attempt={}: \"{}\"(idx={}) → \"{}\"(idx={})",
                   attempt, cur_val, cur_idx, target_val, tgt_idx);

        if cur_idx == tgt_idx {
            log::info!("[MenuNav] 验证通过");
            verified = true;
            break;
        }

        let dv = tgt_idx as i32 - cur_idx as i32;
        let (vdir, vsteps) = if dv > 0 { (dial_cw, dv as u32) } else { (dial_ccw, (-dv) as u32) };
        let knob_delay_ms = if menu_num == 30 { 250 } else { 120 };
        for _ in 0..vsteps {
            inject_knob_wait(state, vdir);
            std::thread::sleep(Duration::from_millis(knob_delay_ms));
        }
        std::thread::sleep(Duration::from_millis(if menu_num == 30 { 800 } else { 200 }));
    }

    // === Step 5: 保存值；不保留菜单时再按一次 SET 回到频率显示 ===
    inject_key_wait(state, 0x20);
    std::thread::sleep(Duration::from_millis(500));
    if !keep_menu_open {
        inject_key_wait(state, 0x20);
        std::thread::sleep(Duration::from_millis(500));
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        clear_side_menu_display(&mut s, menu_side_is_left);
    }

    state.lock().unwrap_or_else(|e| e.into_inner()).macro_running = false;
    if verified {
        log::info!("[MenuNav] 完成{}", if keep_menu_open { "，保持 SET 菜单" } else { "，已退出到频率页" });
    } else {
        log::warn!("[MenuNav] 未验证到目标值，已退出菜单");
    }
    verified
}

/// 将 TH-9800 菜单显示文本映射到选项索引。
fn menu_value_index(menu_num: u8, s: &str) -> Option<usize> {
    let text = s.trim();
    if menu_num == 30 {
        let tenth = parse_tenth_hz_menu_value(text)?;
        CTCSS_TONES_TENTH_HZ.iter().position(|&v| v == tenth)
    } else {
        step_value_index(text)
    }
}

fn step_value_index(s: &str) -> Option<usize> {
    let mut text = heapless::String::<8>::new();
    for c in s.chars() {
        if !c.is_whitespace() {
            let _ = text.push(c.to_ascii_uppercase());
        }
    }

    match text.as_str() {
        "25K" => Some(0),
        "5K" | "50K" => Some(1),
        "625K" => Some(2),
        "75K" => Some(3),
        "833K" => Some(4),
        "10K" => Some(5),
        "125K" => Some(6),
        "15K" | "150K" => Some(7),
        "250K" => Some(8),
        "30K" | "300K" => Some(9),
        "500K" => Some(10),
        "100K" | "1000K" => Some(11),
        _ => STEP_STRS.iter().position(|&v| v == text.as_str()),
    }
}

fn parse_tenth_hz_menu_value(s: &str) -> Option<u32> {
    let mut digits = heapless::String::<8>::new();
    let mut has_dot = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            let _ = digits.push(c);
        } else if c == '.' {
            has_dot = true;
        }
    }
    if digits.is_empty() {
        return None;
    }
    let n = digits.parse::<u32>().ok()?;
    if has_dot { Some(n) } else { Some(n) }
}

/// 用手咪 P3(TONE) 键循环，将 MAIN 侧亚音模式切换到目标状态
fn current_tone_mode(state: &SharedState) -> ToneMode {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let band = if s.right.is_main { &s.right } else { &s.left };
    if band.tone_dcs {
        ToneMode::Dcs
    } else if band.tone_enc && band.tone_dec {
        ToneMode::EncDec
    } else if band.tone_enc {
        ToneMode::Enc
    } else {
        ToneMode::Off
    }
}

fn inject_tone_mode(state: &SharedState, target: ToneMode) -> bool {
    let acquired = {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                if !s.macro_running {
                    s.macro_running = true;
                    break true;
                }
            }
            if std::time::Instant::now() >= deadline {
                log::warn!("[MenuNav] inject_tone_mode 等待宏锁超时 30s，跳过");
                break false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    if !acquired { return false; }

    for attempt in 0..4 {
        let current = current_tone_mode(state);
        if current == target {
            log::info!("[MenuNav] inject_tone_mode 已是 {}", tone_mode_name(target));
            state.lock().unwrap_or_else(|e| e.into_inner()).macro_running = false;
            return true;
        }
        let presses = tone_mode_presses(current, target);
        if presses == 0 {
            break;
        }
        log::info!(
            "[MenuNav] inject_tone_mode attempt={} current={} target={} press P3",
            attempt,
            tone_mode_name(current),
            tone_mode_name(target)
        );
        inject_key_wait(state, 0x12);
        std::thread::sleep(Duration::from_millis(800));
    }

    let final_mode = current_tone_mode(state);
    let ok = final_mode == target;
    if !ok {
        log::warn!("[MenuNav] inject_tone_mode 未确认到 {}，当前 {}", tone_mode_name(target), tone_mode_name(final_mode));
    }
    state.lock().unwrap_or_else(|e| e.into_inner()).macro_running = false;
    ok
}

fn tone_mode_presses(current: ToneMode, target: ToneMode) -> u8 {
    fn idx(m: ToneMode) -> u8 {
        match m {
            ToneMode::Off => 0,
            ToneMode::Enc => 1,
            ToneMode::EncDec => 2,
            ToneMode::Dcs => 3,
        }
    }
    (idx(target) + 4 - idx(current)) % 4
}

fn tone_mode_name(m: ToneMode) -> &'static str {
    match m {
        ToneMode::Off => "OFF",
        ToneMode::Enc => "ENC",
        ToneMode::EncDec => "ENC.DEC",
        ToneMode::Dcs => "DCS",
    }
}

// ===== 注入辅助函数 =====

fn wait_key_clear(state: &SharedState) {
    loop {
        let busy = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.key_override.is_some() || s.key_release
        };
        if !busy { break; }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn inject_key_wait(state: &SharedState, key: u8) {
    wait_key_clear(state);
    state.lock().unwrap_or_else(|e| e.into_inner()).key_override = Some(key);
    std::thread::sleep(Duration::from_millis(200));
    // 发送松开帧
    wait_key_clear(state);
    state.lock().unwrap_or_else(|e| e.into_inner()).key_release = true;
    std::thread::sleep(Duration::from_millis(100));
}

fn inject_knob_wait(state: &SharedState, step: u8) {
    loop {
        if state.lock().unwrap_or_else(|e| e.into_inner()).knob_inject.is_none() { break; }
        std::thread::sleep(Duration::from_millis(20));
    }
    state.lock().unwrap_or_else(|e| e.into_inner()).knob_inject = Some(step);
}

fn read_menu_text(state: &SharedState) -> heapless::String<12> {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let band = if s.right.is_main { &s.right } else { &s.left };
    band.menu_text.clone()
}

fn wait_for_menu_value_with_timeout(state: &SharedState, timeout: Duration) -> heapless::String<12> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            let band = if s.right.is_main { &s.right } else { &s.left };
            if band.is_set && band.menu_in_value && !band.menu_text.is_empty() {
                return band.menu_text.clone();
            }
        }
        if std::time::Instant::now() >= deadline { break; }
        std::thread::sleep(Duration::from_millis(20));
    }
    heapless::String::new()
}

fn wait_for_menu_value(state: &SharedState) -> heapless::String<12> {
    wait_for_menu_value_with_timeout(state, Duration::from_millis(600))
}

// ===== \dump_state 模板 =====
// 参考 hamlib v4.6.x dummy backend dump_state 输出
// 关键字段：proto_ver(0)、model(2=NetRigctl)、ITU2、freq ranges、tuning steps、has_func/level
// TH-9800 频率范围：26-33 / 47-54 / 108-180 / 220-260 / 350-512 / 750-1300 MHz
// 简化为典型业余频段
fn dump_state() -> String {
    let mut s = String::new();
    s.push_str("0\n");        // protocol version
    s.push_str("2\n");        // model id (NET rigctl)
    s.push_str("2\n");        // ITU region 2
    // RX frequency ranges: low_hz high_hz mode_mask low_pwr high_pwr vfo ant
    // 终止行: "0 0 0 0 0 0 0\n"
    // 26-33 MHz CB
    s.push_str("26000000.000000 33000000.000000 0x1ff -1 -1 0x16000003 0x3\n");
    // 47-54 MHz 6m
    s.push_str("47000000.000000 54000000.000000 0x1ff -1 -1 0x16000003 0x3\n");
    // 108-180 MHz (2m + air)
    s.push_str("108000000.000000 180000000.000000 0x1ff -1 -1 0x16000003 0x3\n");
    // 220-260 MHz (1.25m)
    s.push_str("220000000.000000 260000000.000000 0x1ff -1 -1 0x16000003 0x3\n");
    // 350-512 MHz (70cm)
    s.push_str("350000000.000000 512000000.000000 0x1ff -1 -1 0x16000003 0x3\n");
    // 750-1300 MHz (23cm)
    s.push_str("750000000.000000 1300000000.000000 0x1ff -1 -1 0x16000003 0x3\n");
    s.push_str("0 0 0 0 0 0 0\n");  // terminator
    // TX frequency ranges (业余三段)
    s.push_str("28000000.000000 29700000.000000 0x1ff 5000 50000 0x16000003 0x3\n");
    s.push_str("50000000.000000 54000000.000000 0x1ff 5000 50000 0x16000003 0x3\n");
    s.push_str("144000000.000000 148000000.000000 0x1ff 5000 50000 0x16000003 0x3\n");
    s.push_str("222000000.000000 225000000.000000 0x1ff 5000 50000 0x16000003 0x3\n");
    s.push_str("420000000.000000 450000000.000000 0x1ff 5000 35000 0x16000003 0x3\n");
    s.push_str("0 0 0 0 0 0 0\n");
    // tuning steps: mode_mask step_hz
    s.push_str("0x1ff 5000\n");
    s.push_str("0x1ff 6250\n");
    s.push_str("0x1ff 10000\n");
    s.push_str("0x1ff 12500\n");
    s.push_str("0x1ff 15000\n");
    s.push_str("0x1ff 20000\n");
    s.push_str("0x1ff 25000\n");
    s.push_str("0x1ff 50000\n");
    s.push_str("0x1ff 100000\n");
    s.push_str("0 0\n");
    // filter list: mode_mask passband_hz
    s.push_str("0x1ff 12500\n");   // FM narrow
    s.push_str("0x1ff 25000\n");   // FM wide
    s.push_str("0x1ff 8000\n");    // AM
    s.push_str("0 0\n");
    s.push_str("0\n");          // max_rit
    s.push_str("0\n");          // max_xit
    s.push_str("0\n");          // max_ifshift
    s.push_str("0\n");          // announces
    // preamps: 终止 0
    s.push_str("0\n");
    // attenuators: 终止 0
    s.push_str("0\n");
    s.push_str("0x0\n");        // has_get_func
    s.push_str("0x0\n");        // has_set_func
    s.push_str("0x40000020\n"); // has_get_level (RFPOWER + STRENGTH 估值)
    s.push_str("0x0\n");        // has_set_level
    s.push_str("0x0\n");        // has_get_parm
    s.push_str("0x0\n");        // has_set_parm
    // 后续可选字段许多客户端不强制要求，hamlib v4 dump_state 还输出 vfo_ops/scan_ops
    s.push_str("0x0\n");        // vfo_ops
    s.push_str("0x0\n");        // scan_ops
    s.push_str("0\n");          // targetable_vfo
    s.push_str("0\n");          // transceive
    s.push_str("vfo_ops=0\n");
    s.push_str("ptt_type=0x1\n");
    s.push_str("targetable_vfo=0x0\n");
    s.push_str("done\n");
    s
}
