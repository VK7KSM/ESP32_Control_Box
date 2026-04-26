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
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const PORT: u16 = 4532;
const MAX_CLIENTS: usize = 4;

// Hamlib RIG_OK = 0, errors negative
const RPRT_OK: &str = "RPRT 0\n";
const RPRT_EINVAL: &str = "RPRT -1\n";       // 参数错
const RPRT_EPROTO: &str = "RPRT -8\n";       // 协议错
#[allow(dead_code)]
const RPRT_ENIMPL: &str = "RPRT -11\n";      // 未实现

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
    const STEP_HZ: u64 = 12_500;
    const KNOB_THRESHOLD_STEPS: i64 = 5;  // ≤5 步用旋钮，>5 步用键盘宏

    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));

        // 读快照
        let (target, current_hz, main_is_left) = {
            let s = state.lock().unwrap();
            let target = match s.rigctld_target_hz {
                Some(t) => t,
                None => continue,
            };
            let band = if s.right.is_main { &s.right } else { &s.left };
            let current = freq_str_to_hz(band.freq.as_str()).unwrap_or(0);
            (target, current, s.left.is_main)
        };

        if current_hz == 0 {
            // 频率未知（电台离线）：保留 target，等待电台上线
            continue;
        }

        let delta_hz = target as i64 - current_hz as i64;
        let delta_steps = delta_hz / STEP_HZ as i64;

        if delta_hz.unsigned_abs() < STEP_HZ / 2 {
            log::info!("[FreqStepper] target={} 抵达 (current={})", target, current_hz);
            state.lock().unwrap().rigctld_target_hz = None;
            continue;
        }

        if delta_steps.unsigned_abs() <= KNOB_THRESHOLD_STEPS as u64 {
            // === 微调路径：旋钮单步注入 ===
            let (cw, ccw) = if main_is_left { (0x02u8, 0x01u8) } else { (0x82u8, 0x81u8) };
            let step_byte = if delta_hz > 0 { cw } else { ccw };

            let mut s = state.lock().unwrap();
            if s.knob_inject.is_some() { continue; }
            s.knob_inject = Some(step_byte);
        } else {
            // === 大跳路径：键盘宏（6 位数字直接输入）===
            log::info!("[FreqStepper] 大跳 target={} (delta={}步) → 键盘输入", target, delta_steps);
            inject_freq_keyboard(&state, target);
            // 键盘输入后清除 target，避免重复触发；电台收到 6 位完整后会立即跳到目标频率
            state.lock().unwrap().rigctld_target_hz = None;
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
        loop {
            let busy = {
                let s = state.lock().unwrap();
                s.key_override.is_some() || s.key_release
            };
            if !busy { break; }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // 按下
        state.lock().unwrap().key_override = Some(key);
        std::thread::sleep(std::time::Duration::from_millis(250));
        // 松开
        state.lock().unwrap().key_release = true;
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    log::info!("[FreqStepper] 键盘输入完成");
}

fn rigctld_main(state: SharedState) {
    let active = Arc::new(AtomicUsize::new(0));
    log::info!("[Rigctld] acceptor 启动");

    loop {
        let connected = {
            let s = state.lock().unwrap();
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
                    let st = state.clone();
                    let act = active.clone();
                    log::info!("[Rigctld] 接受连接：{}", peer);
                    std::thread::Builder::new()
                        .name(format!("rigctld_{}", peer.port()))
                        .stack_size(8192)
                        .spawn(move || {
                            handle_client(stream, st);
                            act.fetch_sub(1, Ordering::SeqCst);
                            log::info!("[Rigctld] 连接 {} 已关闭", peer);
                        })
                        .ok();
                }
                Err(e) => {
                    let still = { state.lock().unwrap().wifi_state == WifiState::Connected };
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

fn handle_client(stream: TcpStream, state: SharedState) {
    let _ = stream.set_nodelay(true);
    // ESP-IDF lwip 不可靠支持 TcpStream::try_clone()。改用单一 stream + BufReader 包装，
    // 通过 BufReader::get_mut() 写回原 stream（buffered read + raw write 共用同一句柄）
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() { continue; }
        log::info!("[Rigctld] ← {}", trimmed);

        let resp = match dispatch(trimmed, &state) {
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
        "get_vfo_info" => DispatchOut::Reply(get_vfo_info(ext, state)),
        _ => DispatchOut::Reply(RPRT_EPROTO.to_string()),
    }
}

// ===== 工具函数 =====

/// 取 MAIN 侧 BandState。若都不是 MAIN（罕见），返回 left
fn main_band(state: &SharedState) -> crate::state::BandState {
    let s = state.lock().unwrap();
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

// ===== 命令实现 =====

fn get_freq(ext: bool, state: &SharedState) -> String {
    // 优先返回 set_freq 异步目标（让客户端立即看到 set_freq 生效，避免重发循环）
    let s = state.lock().unwrap();
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
    let hz_str = args.split_whitespace().next().unwrap_or("");
    let hz: u64 = match hz_str.parse() {
        Ok(v) => v,
        Err(_) => return RPRT_EINVAL.to_string(),
    };
    const STEP_HZ: u64 = 12_500;
    let target = ((hz + STEP_HZ / 2) / STEP_HZ) * STEP_HZ;

    // 计算与当前频率的步数差，超过 100 步（1.25 MHz）视为跨段，旋钮步进不切实际
    // → 拒绝并返回错误，让客户端使用更直接方式（手动切段或 elfradio-box 键盘输入）
    let band = main_band(state);
    let current = freq_str_to_hz(band.freq.as_str()).unwrap_or(0);
    if current == 0 {
        log::warn!("[Rigctld] set_freq: 当前频率未知，已设 target 但 stepper 不会启动");
        state.lock().unwrap().rigctld_target_hz = Some(target);
        return if ext { format!("set_freq: {}\nFreq: {}\nRPRT 0\n", hz_str, target) } else { RPRT_OK.to_string() };
    }
    let delta = (target as i64 - current as i64).abs() as u64;
    let steps = delta / STEP_HZ;
    // stepper 内部会按 ≤5 步=旋钮 / >5 步=键盘宏 自动选择路径，
    // 故仅在频率明显越界时拒绝（如 1300 MHz 以外）
    if target < 26_000_000 || target > 1_300_000_000 {
        log::warn!("[Rigctld] set_freq 频率越界: {}", target);
        return RPRT_EINVAL.to_string();
    }

    state.lock().unwrap().rigctld_target_hz = Some(target);
    log::info!("[Rigctld] set_freq target={} (current={} delta={}步)", target, current, steps);
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
        let mut s = state.lock().unwrap();
        s.ptt_override = on;
        if on { s.ptt_start_us = now_us; }
    }
    log::info!("[Rigctld] set_ptt: {}", if on { "ON" } else { "OFF" });
    if ext { format!("set_ptt: {}\nRPRT 0\n", v) } else { RPRT_OK.to_string() }
}

fn get_vfo(ext: bool, state: &SharedState) -> String {
    // 把 MAIN 侧映射为 VFOA，另一侧为 VFOB
    let s = state.lock().unwrap();
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
        let s = state.lock().unwrap();
        let already = (target_left && s.left.is_main) || (!target_left && s.right.is_main);
        if already {
            return if ext { "set_vfo:\nRPRT 0\n".into() } else { RPRT_OK.to_string() };
        }
    }
    // 注入 P1 (0x10) 切换 MAIN
    state.lock().unwrap().key_override = Some(0x10);
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
    let on = state.lock().unwrap().radio_alive;
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
    let s = state.lock().unwrap();
    let vfo = if s.left.is_main { "VFOA" } else { "VFOB" };
    let mode = if band.mode.as_str() == "AM" { "AM" } else { "FM" };
    let bw = if mode == "AM" { 8000 } else { 12500 };
    if ext {
        format!("get_vfo_info:\nVFO: {}\nFreq: {}\nMode: {}\nWidth: {}\nSplit: 0\nSatMode: 0\nRPRT 0\n", vfo, hz, mode, bw)
    } else {
        format!("{}\n{}\n{}\n{}\n0\n0\n", vfo, hz, mode, bw)
    }
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
