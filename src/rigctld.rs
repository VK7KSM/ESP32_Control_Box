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

use crate::state::{SharedState, WifiState};
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

#[derive(Clone, Copy, PartialEq)]
enum ToneMode {
    Off,
    Enc,
    EncDec,
    Dcs,
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

        // 读快照：无 rigctld client、宏运行、STEP 未就绪时都不追频
        let (target, current_hz, main_is_left, last_step_us) = {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            if s.rigctld_clients == 0 {
                s.rigctld_target_hz = None;
                continue;
            }
            if s.macro_running || s.rigctld_setup_running || !s.rigctld_step_ready {
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
            (target, current, s.left.is_main, s.rigctld_last_step_us)
        };

        if current_hz == 0 {
            continue;
        }

        let delta_hz = target as i64 - current_hz as i64;
        if delta_hz.unsigned_abs() < RIG_STEP_HZ / 2 {
            state.lock().unwrap_or_else(|e| e.into_inner()).rigctld_target_hz = None;
            continue;
        }

        if now_us.saturating_sub(last_step_us) < RIG_TRACK_INTERVAL_US {
            continue;
        }

        let (cw, ccw) = if main_is_left { (0x02u8, 0x01u8) } else { (0x82u8, 0x81u8) };
        let step_byte = if delta_hz > 0 { cw } else { ccw };

        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.knob_inject.is_none() && !s.macro_running && s.rigctld_step_ready {
            s.knob_inject = Some(step_byte);
            s.rigctld_last_step_us = now_us;
            log::info!("[FreqStepper] 追踪步进 target={} current={} delta={}Hz", target, current_hz, delta_hz);
        }
    }
}

/// 键盘宏：MAIN 侧按 6 位数字键直接输入频率
/// 频率格式：MHz×3 位 + kHz×3 位（如 145.500 MHz → "145500"）
/// 假设当前在 VFO 模式（若在 MR 模式需先切 VFO，本版未实现）
fn inject_freq_keyboard(state: &SharedState, target_hz: u64) {
    let mhz = target_hz / 1_000_000;
    let khz = (target_hz % 1_000_000) / 1_000;
    let digits = format!("{:03}{:03}", mhz, khz);
    log::info!("[FreqStepper] 键盘输入: \"{}\"", digits);

    for ch in digits.chars() {
        let key = (ch as u8).saturating_sub(b'0');
        if key > 9 { continue; }
        // 等待前一次注入被消费
        let wait_start = std::time::Instant::now();
        let mut warned = false;
        loop {
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
                    // 更新连接计数；首次连接只重置 setup 状态，等待首个 set_freq 后再设置初始频率和 STEP
                    {
                        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                        if s.rigctld_clients == 0 {
                            s.rigctld_initial_freq_done = false;
                            s.rigctld_step_ready = false;
                            s.rigctld_setup_running = false;
                            s.rigctld_target_hz = None;
                            s.rigctld_last_step_us = 0;
                            log::info!("[Rigctld] 首次连接，等待 DTrac 首个 set_freq 后执行初始设置");
                        }
                        s.rigctld_clients = s.rigctld_clients.saturating_add(1);
                    }
                    let st = state.clone();
                    let act = active.clone();
                    log::info!("[Rigctld] 接受连接：{}", peer);
                    std::thread::Builder::new()
                        .name(format!("rigctld_{}", peer.port()))
                        .stack_size(8192)
                        .spawn(move || {
                            handle_client(stream, &st);
                            act.fetch_sub(1, Ordering::SeqCst);
                            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
                            s.rigctld_clients = s.rigctld_clients.saturating_sub(1);
                            // 最后一个 client 断开 → 清频率追踪 target，避免 freq_stepper 持续追
                            if s.rigctld_clients == 0 {
                                s.rigctld_target_hz = None;
                                s.key_override = None;
                                s.key_release = false;
                                s.knob_inject = None;
                                s.rigctld_setup_running = false;
                            }
                            log::info!("[Rigctld] 连接 {} 已关闭，剩余 {} 客户端", peer, s.rigctld_clients);
                        })
                        .ok();
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
        log::info!("[Rigctld] ← {}", trimmed);

        let resp = match dispatch(trimmed, state) {
            DispatchOut::Reply(s) => s,
            DispatchOut::Quit => return,
        };
        if !resp.is_empty() {
            log::info!("[Rigctld] → {} bytes", resp.len());
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

fn dispatch(line: &str, state: &SharedState) -> DispatchOut {
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
        's' => DispatchOut::Reply(get_split_vfo(ext)),
        'S' => DispatchOut::Reply(set_split_vfo(args, ext)),
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
        "get_split_vfo"=> DispatchOut::Reply(get_split_vfo(ext)),
        "set_split_vfo"=> DispatchOut::Reply(set_split_vfo(args, ext)),
        "get_rit"      => DispatchOut::Reply(get_rit(ext)),
        "set_rit"      => DispatchOut::Reply(set_rit(args, ext)),
        "get_info"     => DispatchOut::Reply(get_info(ext)),
        "get_vfo_info"   => DispatchOut::Reply(get_vfo_info(ext, state)),
        "get_ctcss_tone" => DispatchOut::Reply(get_ctcss_tone(ext, state)),
        "set_ctcss_tone" => DispatchOut::Reply(set_ctcss_tone(args, ext, state)),
        "get_ctcss_sql"  => DispatchOut::Reply(get_ctcss_sql(ext, state)),
        "set_ctcss_sql"  => DispatchOut::Reply(set_ctcss_sql(args, ext, state)),
        _ => DispatchOut::Reply(RPRT_EPROTO.to_string()),
    }
}

// ===== 工具函数 =====

/// 取 MAIN 侧 BandState。若都不是 MAIN（罕见），返回 left
fn main_band(state: &SharedState) -> crate::state::BandState {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    if s.right.is_main { s.right.clone() }
    else               { s.left.clone() }
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

fn rigctld_initial_setup(state: &SharedState, target: u64) {
    log::info!("[RigctldSetup] 设置初始频率 {}", target);
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.macro_running = true;
        s.key_override = None;
        s.key_release = false;
        s.knob_inject = None;
    }
    inject_freq_keyboard(state, target);
    std::thread::sleep(Duration::from_millis(1200));
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.rigctld_initial_freq_done = true;
        s.macro_running = false;
    }

    let keep_menu_open = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.rigctld_ctcss_tone > 0
    };
    log::info!("[RigctldSetup] 初始频率完成，设置 STEP=2.5kHz，{}", if keep_menu_open { "后续设置亚音，保持 SET 菜单" } else { "无亚音，完成后退出到频率页" });
    let step_ok = inject_menu_set(state, 28, "2.5", keep_menu_open);
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
    // 优先返回 set_freq 异步目标（让客户端立即看到 set_freq 生效，避免重发循环）
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let hz = match s.rigctld_target_hz {
        Some(t) => t,
        None => {
            let band = if s.right.is_main { &s.right } else { &s.left };
            freq_str_to_hz(band.freq.as_str()).unwrap_or(0)
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

    let start_setup = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.rigctld_target_hz = Some(target);
        if !s.rigctld_initial_freq_done && !s.rigctld_setup_running {
            s.rigctld_setup_running = true;
            true
        } else {
            false
        }
    };

    if start_setup {
        log::info!("[Rigctld] 首个 set_freq={}，交由 freq_stepper 执行初始频率+STEP 设置", target);
    } else {
        log::info!("[Rigctld] set_freq target={}（等待限速追踪）", target);
    }

    if ext { format!("set_freq: {}\nFreq: {}\nRPRT 0\n", hz_str, target) }
    else   { RPRT_OK.to_string() }
}

fn get_mode(ext: bool, state: &SharedState) -> String {
    let band = main_band(state);
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
    let band = main_band(state);
    let ptt = if band.is_tx { 1 } else { 0 };
    if ext { format!("get_ptt:\nPTT: {}\nRPRT 0\n", ptt) }
    else   { format!("{}\n", ptt) }
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
        s.ptt_override = on;
        if on { s.ptt_start_us = now_us; }
    }
    log::info!("[Rigctld] set_ptt: {}", if on { "ON" } else { "OFF" });
    if ext { format!("set_ptt: {}\nRPRT 0\n", v) } else { RPRT_OK.to_string() }
}

fn get_vfo(ext: bool, state: &SharedState) -> String {
    // 把 MAIN 侧映射为 VFOA，另一侧为 VFOB
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let v = if s.left.is_main { "VFOA" } else { "VFOB" };
    if ext { format!("get_vfo:\nVFO: {}\nRPRT 0\n", v) }
    else   { format!("{}\n", v) }
}

fn set_vfo(args: &str, ext: bool, state: &SharedState) -> String {
    // VFOA → 切到 LEFT MAIN；VFOB → RIGHT MAIN（注入 P1=0x10 切 MAIN）
    let target_left = match args.split_whitespace().next().unwrap_or("") {
        "VFOA" | "Main" | "main" => true,
        "VFOB" | "Sub" | "sub"   => false,
        _ => return RPRT_EINVAL.to_string(),
    };
    {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
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

fn get_split_vfo(ext: bool) -> String {
    // TH-9800 无 split：恒定 0 + VFOB
    if ext { "get_split_vfo:\nSplit: 0\nTX VFO: VFOB\nRPRT 0\n".into() }
    else   { "0\nVFOB\n".into() }
}

fn set_split_vfo(_args: &str, ext: bool) -> String {
    if ext { "set_split_vfo:\nRPRT 0\n".into() } else { RPRT_OK.to_string() }
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
    let band = main_band(state);
    let hz = freq_str_to_hz(band.freq.as_str()).unwrap_or(0);
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let vfo = if s.left.is_main { "VFOA" } else { "VFOB" };
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
    state.lock().unwrap_or_else(|e| e.into_inner()).rigctld_ctcss_tone = tone;
    let target_idx = CTCSS_TONES_TENTH_HZ.iter().position(|&t| t == tone);
    log::info!("[Rigctld] set_ctcss_tone: {}（{} Hz），idx={:?}", tone, tone as f32 / 10.0, target_idx);
    let st = state.clone();
    std::thread::Builder::new()
        .name("rigctld_ctcss".into())
        .stack_size(8192)
        .spawn(move || {
            if let Some(idx) = target_idx {
                inject_menu_set(&st, 30, CTCSS_TONE_STRS[idx], false);
            }
            inject_tone_mode(&st, if tone > 0 { ToneMode::Enc } else { ToneMode::Off });
        })
        .ok();
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
    state.lock().unwrap_or_else(|e| e.into_inner()).rigctld_ctcss_tone = tone;
    let target_idx = CTCSS_TONES_TENTH_HZ.iter().position(|&t| t == tone);
    let st = state.clone();
    std::thread::Builder::new()
        .name("rigctld_ctcss_sql".into())
        .stack_size(8192)
        .spawn(move || {
            if let Some(idx) = target_idx {
                inject_menu_set(&st, 30, CTCSS_TONE_STRS[idx], false);
            }
            inject_tone_mode(&st, if tone > 0 { ToneMode::EncDec } else { ToneMode::Off });
        })
        .ok();
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
    let ok = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        let band = if s.right.is_main { &s.right } else { &s.left };
        s.radio_alive && !band.is_tx && !band.is_busy
    };
    if !ok {
        log::warn!("[MenuNav] 电台不满足条件（未在线/发射中/信道忙），取消");
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

    let (dial_click, dial_cw, dial_ccw) = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.left.is_main { (0x25u8, 0x02u8, 0x01u8) } else { (0xA5u8, 0x82u8, 0x81u8) }
    };

    log::info!("[MenuNav] 开始：目标菜单 #{} = \"{}\"(idx={})", menu_num, target_val, tgt_idx);

    let already_in_set = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        let band = if s.right.is_main { &s.right } else { &s.left };
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
        let cur_val = wait_for_menu_value(state);
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
        for _ in 0..vsteps {
            inject_knob_wait(state, vdir);
            std::thread::sleep(Duration::from_millis(120));
        }
        std::thread::sleep(Duration::from_millis(200)); // 等待电台 Len=09 响应更新 menu_text
    }

    // === Step 5: 保存值；不保留菜单时再按一次 SET 回到频率显示 ===
    inject_key_wait(state, 0x20);
    std::thread::sleep(Duration::from_millis(500));
    if !keep_menu_open {
        inject_key_wait(state, 0x20);
        std::thread::sleep(Duration::from_millis(500));
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        let band = if s.right.is_main { &mut s.right } else { &mut s.left };
        band.is_set = false;
        band.menu_text.clear();
        band.menu_in_value = false;
        band.menu_exit_count = 0;
        band.display_text.clear();
        s.head_count = s.head_count.wrapping_add(1);
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
fn inject_tone_mode(state: &SharedState, target: ToneMode) {
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
    if !acquired { return; }

    let presses = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        let band = if s.right.is_main { &s.right } else { &s.left };
        let current = if band.tone_dcs {
            ToneMode::Dcs
        } else if band.tone_enc && band.tone_dec {
            ToneMode::EncDec
        } else if band.tone_enc {
            ToneMode::Enc
        } else {
            ToneMode::Off
        };
        tone_mode_presses(current, target)
    };
    log::info!("[MenuNav] inject_tone_mode target={} presses={}", tone_mode_name(target), presses);
    for _ in 0..presses {
        inject_key_wait(state, 0x12);  // P3 = TONE 键循环模式
    }
    state.lock().unwrap_or_else(|e| e.into_inner()).macro_running = false;
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

/// 等待电台确认进入值编辑模式并返回当前值（最多 600ms）
/// 只有 menu_in_value=true 时才返回，保证读到的是"值"而非"菜单名"
fn wait_for_menu_value(state: &SharedState) -> heapless::String<12> {
    let deadline = std::time::Instant::now() + Duration::from_millis(600);
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
    heapless::String::new()  // 超时返回空串
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
