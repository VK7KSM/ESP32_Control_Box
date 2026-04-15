// ===================================================================
// CLI 一次性命令实现
// 所有命令：连接串口 → 等状态 → 执行 → 打印结果 → 退出
// ===================================================================

use std::sync::mpsc;
use std::time::{Duration, Instant};
use crossterm::style::Stylize;

use crate::protocol;
use crate::serial_link::SerialEvent;
use crate::state::SharedState;
use crate::audio;
use crate::tts;

// ===== 收听选项 =====

#[derive(Debug)]
pub struct ListenOptions {
    pub duration:     Option<std::time::Duration>,  // --duration N[s|m|h]
    pub count:        Option<u32>,                  // --count N（录完 N 次后停止）
    pub idle_timeout: Option<std::time::Duration>,  // --idle N[s|m]（无信号 N 时间后停止）
    pub audio:        bool,                         // --audio（开启直通播放）
}

// ===== 命令枚举 =====

#[derive(Debug)]
pub enum CliCommand {
    Help,
    Monitor,
    SetFreq { side: u8, freq: String },          // side: 0=L 1=R, freq: 6位数字字符串
    SetVol(u8),                                   // 0-100%
    SetSql(u8),                                   // 0-100%
    SetPower { side: u8, level: String },         // level: "low"/"mid"/"high"
    MainSwitch(u8),                               // 0=L 1=R
    Tone,                                         // 亚音循环一次
    Knob { up: bool, n: u8 },                     // MAIN 侧旋钮步进
    PowerToggle,                                  // 开关机
    Ptt(u64),                                     // PTT 发射 N 秒（含安全关闭）
    PttOff,                                       // 强制关闭 PTT
    PttTx(String),                                // PTT + 播放音频文件
    Tts { text: String, voice: Option<String> }, // TTS 合成 + PTT 发射

    // ── 固件管理（不需要 OTG 串口）──────────────────────────────
    Flash {
        yes:        bool,           // --yes 跳过所有交互确认
        flash_port: Option<String>, // --flash-port 指定 UART 烧录口
    },
    FlashCheck,                     // flash --check 仅查询版本

    // ── 被动收听（长期运行，无 TUI）──────────────────────────────
    Listen(ListenOptions),
}

// ===== 参数解析 =====

pub struct CliArgs {
    pub port: Option<String>,
    pub command: CliCommand,
}

/// 解析命令行参数。失败返回 Err（包含错误信息），用户应打印帮助后退出。
pub fn parse_args() -> Result<CliArgs, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 无参数：进入 TUI 监听模式（双击启动的默认行为）
    if args.is_empty() {
        return Err("__interactive__".into());
    }

    // help 不需要串口
    if args[0] == "help" || args[0] == "--help" || args[0] == "-h" {
        return Ok(CliArgs { port: None, command: CliCommand::Help });
    }

    let mut port: Option<String> = None;
    let mut i = 0usize;

    // 解析全局 --port 选项
    if i < args.len() && (args[i] == "--port" || args[i] == "-p") {
        i += 1;
        if i >= args.len() {
            return Err("--port 后需要指定串口名称，如 COM8".into());
        }
        port = Some(args[i].clone());
        i += 1;
    }

    if i >= args.len() {
        // 无子命令 → 交互模式（不属于 CLI 路径）
        return Err("__interactive__".into());
    }

    let cmd_str = args[i].to_lowercase();
    i += 1;

    let command = match cmd_str.as_str() {
        "monitor" => CliCommand::Monitor,

        "set-freq" | "freq" => {
            if i + 1 >= args.len() {
                return Err("用法: set-freq <L|R> <NNNNNN>  例如: set-freq L 433550".into());
            }
            let side = parse_side(&args[i])?;
            i += 1;
            let freq = args[i].clone();
            i += 1;
            if freq.len() != 6 || !freq.chars().all(|c| c.is_ascii_digit()) {
                return Err(format!("频率必须是 6 位纯数字，如 433550，收到: {}", freq));
            }
            CliCommand::SetFreq { side, freq }
        }

        "set-vol" | "vol" => {
            if i >= args.len() {
                return Err("用法: set-vol <0-100>".into());
            }
            let v: u8 = args[i].parse().map_err(|_| format!("音量必须是 0-100 之间的整数，收到: {}", args[i]))?;
            i += 1;
            if v > 100 { return Err("音量范围 0-100".into()); }
            CliCommand::SetVol(v)
        }

        "set-sql" | "sql" => {
            if i >= args.len() {
                return Err("用法: set-sql <0-100>".into());
            }
            let v: u8 = args[i].parse().map_err(|_| format!("静噪必须是 0-100 之间的整数，收到: {}", args[i]))?;
            i += 1;
            if v > 100 { return Err("静噪范围 0-100".into()); }
            CliCommand::SetSql(v)
        }

        "set-power" | "power" => {
            if i + 1 >= args.len() {
                return Err("用法: set-power <L|R> <low|mid|high>".into());
            }
            let side = parse_side(&args[i])?;
            i += 1;
            let level = args[i].to_lowercase();
            i += 1;
            if !["low", "mid", "high"].contains(&level.as_str()) {
                return Err(format!("功率等级必须是 low/mid/high，收到: {}", level));
            }
            CliCommand::SetPower { side, level }
        }

        "main" => {
            if i >= args.len() {
                return Err("用法: main <L|R>".into());
            }
            let side = parse_side(&args[i])?;
            i += 1;
            CliCommand::MainSwitch(side)
        }

        "tone" => CliCommand::Tone,

        "knob" => {
            if i >= args.len() {
                return Err("用法: knob <up|down> [N]".into());
            }
            let up = match args[i].to_lowercase().as_str() {
                "up" | "cw" => true,
                "down" | "ccw" => false,
                other => return Err(format!("旋钮方向必须是 up/down，收到: {}", other)),
            };
            i += 1;
            let n: u8 = if i < args.len() {
                let v = args[i].parse().unwrap_or(1);
                i += 1;
                v.max(1).min(50)
            } else { 1 };
            CliCommand::Knob { up, n }
        }

        "power-toggle" | "toggle" | "onoff" => CliCommand::PowerToggle,

        "ptt" => {
            if i >= args.len() {
                return Err("用法: ptt <秒数>  例如: ptt 10".into());
            }
            let secs: u64 = args[i].parse().map_err(|_| format!("PTT 时长必须是整数秒，收到: {}", args[i]))?;
            i += 1;
            if secs == 0 || secs > 30 {
                return Err("PTT 时长必须在 1-30 秒之间（ESP32 看门狗硬限制 30 秒）".into());
            }
            CliCommand::Ptt(secs)
        }

        "ptt-off" => CliCommand::PttOff,

        "ptt-tx" => {
            if i >= args.len() {
                return Err("用法: ptt-tx <file>".into());
            }
            let path = args[i].clone();
            i += 1;
            CliCommand::PttTx(path)
        }

        "tts" => {
            // 用法: tts [--voice VOICE] <文字>
            let mut voice: Option<String> = None;
            if i < args.len() && args[i] == "--voice" {
                i += 1;
                if i >= args.len() {
                    return Err("--voice 后需要声线名称，如 zh-TW-HsiaoChenNeural".into());
                }
                voice = Some(args[i].clone());
                i += 1;
            }
            if i >= args.len() {
                return Err("用法: tts [--voice 声线] <文字>  例: tts \"你好世界\"".into());
            }
            let text = args[i..].join(" ");
            i = args.len();
            CliCommand::Tts { text, voice }
        }

        "flash" | "firmware" | "update" => {
            let mut yes        = false;
            let mut flash_port: Option<String> = None;
            let mut check_only = false;
            while i < args.len() {
                match args[i].as_str() {
                    "--yes" | "-y"    => { yes = true; i += 1; }
                    "--check"         => { check_only = true; i += 1; }
                    "--flash-port"    => {
                        i += 1;
                        if i >= args.len() { return Err("--flash-port 后需要串口名，如 COM9".into()); }
                        flash_port = Some(args[i].clone());
                        i += 1;
                    }
                    _ => { i += 1; }  // 忽略未知选项
                }
            }
            if check_only { CliCommand::FlashCheck }
            else          { CliCommand::Flash { yes, flash_port } }
        }

        "listen" | "rx" | "monitor-rx" => {
            let mut duration:     Option<std::time::Duration> = None;
            let mut count:        Option<u32>                 = None;
            let mut idle_timeout: Option<std::time::Duration> = None;
            let mut audio = false;
            while i < args.len() {
                match args[i].as_str() {
                    "--duration" | "-d" => {
                        i += 1;
                        if i >= args.len() { return Err("--duration 后需要时长，如 30m / 2h / 3600s".into()); }
                        duration = Some(parse_duration(&args[i])?);
                        i += 1;
                    }
                    "--count" | "-n" => {
                        i += 1;
                        if i >= args.len() { return Err("--count 后需要整数".into()); }
                        count = Some(args[i].parse::<u32>()
                            .map_err(|_| format!("--count 必须是正整数，收到: {}", args[i]))?);
                        i += 1;
                    }
                    "--idle" | "-i" => {
                        i += 1;
                        if i >= args.len() { return Err("--idle 后需要时长，如 10m".into()); }
                        idle_timeout = Some(parse_duration(&args[i])?);
                        i += 1;
                    }
                    "--audio" => { audio = true; i += 1; }
                    _ => { i += 1; }
                }
            }
            CliCommand::Listen(ListenOptions { duration, count, idle_timeout, audio })
        }

        other => return Err(format!("未知命令: {}，运行 'elfradio-box help' 查看帮助", other)),
    };

    let _ = i; // 忽略剩余参数
    Ok(CliArgs { port, command })
}

fn parse_side(s: &str) -> Result<u8, String> {
    match s.to_uppercase().as_str() {
        "L" | "LEFT" => Ok(0),
        "R" | "RIGHT" => Ok(1),
        other => Err(format!("侧边必须是 L 或 R，收到: {}", other)),
    }
}

/// 解析时长字符串：支持 30m / 2h / 90s / 3600（默认秒）
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    if let Some(n) = s.strip_suffix('h') {
        n.parse::<u64>()
            .map(|v| std::time::Duration::from_secs(v * 3600))
            .map_err(|_| format!("时长格式错误: {}（示例: 30m / 2h / 90s）", s))
    } else if let Some(n) = s.strip_suffix('m') {
        n.parse::<u64>()
            .map(|v| std::time::Duration::from_secs(v * 60))
            .map_err(|_| format!("时长格式错误: {}（示例: 30m / 2h / 90s）", s))
    } else if let Some(n) = s.strip_suffix('s') {
        n.parse::<u64>()
            .map(std::time::Duration::from_secs)
            .map_err(|_| format!("时长格式错误: {}（示例: 30m / 2h / 90s）", s))
    } else {
        s.parse::<u64>()
            .map(std::time::Duration::from_secs)
            .map_err(|_| format!("时长格式错误: {}（示例: 30m / 2h / 90s）", s))
    }
}

// ===== 帮助信息 =====

pub fn print_help() {
    println!("{}", r#"
 ╔══════════════════════════════════════════════════════════════╗
 ║          elfRadio BOX — 命令行控制指南  v0.1.0              ║
 ╚══════════════════════════════════════════════════════════════╝

用法:  elfradio-box [--port <PORT>] <命令> [参数]

全局选项:
  --port <PORT>            指定串口（如 COM8），省略则自动检测

━━━ 基本命令 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  help                     显示本帮助
  monitor                  进入 TUI 实时监听模式（跳过主菜单）

━━━ 频率与调制 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  set-freq <L|R> <NNNNNN>  设置左/右侧频率（6位kHz，无小数点）
                            例: set-freq L 433550  → 433.550 MHz
                            例: set-freq R 145500  → 145.500 MHz
  main <L|R>               将 MAIN 切换到左侧或右侧
  knob <up|down> [N]       MAIN 侧旋钮步进 N 格（默认1，最大50）
  tone                     亚音模式循环切换（发 P3 键一次）

━━━ 音量与静噪 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  set-vol <0-100>           设置音量百分比
  set-sql <0-100>           设置静噪百分比

━━━ 功率 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  set-power <L|R> <level>  设置功率等级
                            level: low / mid / high
                            例: set-power L low

━━━ 发射控制 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ptt <秒数>               PTT 发射 N 秒后自动释放（1-30秒，ESP32 看门狗硬限制）
                            例: ptt 10  → 发射 10 秒
  ptt-off                  立即强制关闭 PTT
  ptt-tx <file>            PTT + 播放音频文件，播完自动松开 PTT
                            支持 WAV/MP3/OGG/FLAC/AAC 等格式，自动截断至30秒
                            例: ptt-tx recording.wav  ptt-tx music.mp3
  tts [--voice 声线] <文字>  文字转语音后 PTT 发射，音频保存到 recordings/
                            默认声线: zh-TW-HsiaoChenNeural（台湾女声）
                            例: tts "你好，这里是 VK7KSM"
                            例: tts --voice zh-CN-XiaoxiaoNeural "Hello"

━━━ 电源 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  power-toggle             电台开/关机（GPIO8 脉冲 1.2 秒）

━━━ 固件管理 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  flash [选项]             从 GitHub 下载最新固件并烧录 ESP32
                            （不需要 OTG 线，只需插入 UART 调试线）
                            --yes              跳过所有确认提示（自动化脚本专用）
                            --flash-port <P>   指定 UART 烧录口（如 COM9），省略则自动检测
  flash --check            仅查询 GitHub 最新版本，不下载不烧录
                            例: flash
                            例: flash --yes
                            例: flash --yes --flash-port COM9

━━━ 被动收听 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  listen [选项]            被动监听电台信号，打滚动日志，自动保存 RX 录音到 recordings/
                            多个终止条件同时有效，任意一个先触发即停止
                            Ctrl+C 随时终止（会先保存正在进行的录音再退出）
                            --duration, -d <N[s|m|h]>  运行时长（如 30m, 1h, 3600s）
                            --count,    -n <N>          录完 N 次信号后停止
                            --idle,     -i <N[s|m]>    最后一次信号结束 N 时间内无新活动则停止
                            --audio                    开启接收音频直通（CM108 → PC 耳机）
                            例: listen                  → 无限监听，Ctrl+C 结束
                            例: listen -d 30m           → 运行 30 分钟后结束
                            例: listen -n 5             → 录到 5 次信号后结束
                            例: listen -d 1h --idle 20m → 最多 1 小时，20 分钟无信号提前结束
                            例: listen --audio -d 2h    → 开音频直通，运行 2 小时

━━━ 示例 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  elfradio-box --port COM8 monitor
  elfradio-box --port COM8 set-freq L 433550
  elfradio-box --port COM8 set-power L low
  elfradio-box --port COM8 ptt 5
  elfradio-box --port COM8 ptt-tx cq.wav
  elfradio-box --port COM8 power-toggle
  elfradio-box flash --check
  elfradio-box flash --yes
  elfradio-box --port COM8 listen -d 30m
  elfradio-box --port COM8 listen -n 3 --audio

  台长呼号: VK7KSM   73!"#.cyan());
    println!();
}

// ===== 命令执行 =====

/// 执行一次性 CLI 命令。需要已连接且已获得状态报告。
pub fn run_command(
    cmd: &CliCommand,
    shared: &SharedState,
    cmd_tx: &mpsc::Sender<Vec<u8>>,
    event_rx: &mpsc::Receiver<SerialEvent>,
) -> Result<(), String> {
    match cmd {
        CliCommand::Help | CliCommand::Monitor
        | CliCommand::Flash { .. } | CliCommand::FlashCheck
        | CliCommand::Listen(_) => {
            // 这些命令在 main.rs 中特殊处理，不应到达这里
            unreachable!()
        }

        CliCommand::SetFreq { side, freq } => {
            let digits: Vec<u8> = freq.chars().map(|c| c as u8 - b'0').collect();
            println!(" {}  设置{}频率 → {} MHz",
                " .. ".on_dark_blue().white().bold(),
                if *side == 0 { "LEFT" } else { "RIGHT" },
                format!("{}.{}.{}", &freq[..3], &freq[3..6], "000"));

            send_freq_sequence(cmd_tx, shared, *side, &digits);

            // 等待电台消化频率输入（主动轮询状态）
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_GET_STATE, &[]));
                wait_state(event_rx, Duration::from_millis(500));
            }
            let s = shared.lock().unwrap();
            let band = if *side == 0 { &s.left } else { &s.right };
            println!(" {}  {}侧当前频率: {} MHz",
                " OK ".on_dark_green().white().bold(),
                if *side == 0 { "LEFT" } else { "RIGHT" },
                band.freq);
            Ok(())
        }

        CliCommand::SetVol(pct) => {
            println!(" {}  设置音量 → {}%", " .. ".on_dark_blue().white().bold(), pct);
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_VOL, &[0, *pct]));
            wait_state(event_rx, Duration::from_secs(3));
            println!(" {}  音量已发送", " OK ".on_dark_green().white().bold());
            Ok(())
        }

        CliCommand::SetSql(pct) => {
            println!(" {}  设置静噪 → {}%", " .. ".on_dark_blue().white().bold(), pct);
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_SQL, &[0, *pct]));
            wait_state(event_rx, Duration::from_secs(3));
            println!(" {}  静噪已发送", " OK ".on_dark_green().white().bold());
            Ok(())
        }

        CliCommand::SetPower { side, level } => {
            let target = level.as_str();
            println!(" {}  设置{}功率 → {}",
                " .. ".on_dark_blue().white().bold(),
                if *side == 0 { "LEFT" } else { "RIGHT" },
                target.to_uppercase());

            // LOW 键键码：LEFT=0x21，RIGHT=0xA1
            let low_key: u8 = if *side == 0 { 0x21 } else { 0xA1 };

            // 最多尝试 5 次 LOW 键循环
            for attempt in 0..5 {
                let current = {
                    let s = shared.lock().unwrap();
                    let band = if *side == 0 { &s.left } else { &s.right };
                    band.power.to_lowercase()
                };
                let current_ref = current.as_str();
                // 映射到规范名称
                let current_norm = if current_ref.contains("high") { "high" }
                    else if current_ref.contains("mid") { "mid" }
                    else if current_ref.contains("low") { "low" }
                    else { "high" }; // 默认假设 HIGH

                if current_norm == target {
                    println!(" {}  功率已是 {}，无需调整", " OK ".on_dark_green().white().bold(), target.to_uppercase());
                    return Ok(());
                }

                if attempt > 0 {
                    println!("     当前功率: {}，继续循环...", current.to_uppercase());
                }

                // 发 LOW 键
                let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_PRESS, &[low_key]));
                std::thread::sleep(Duration::from_millis(200));
                let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_REL, &[]));
                std::thread::sleep(Duration::from_millis(300));

                // 请求状态更新
                let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_GET_STATE, &[]));
                wait_state(event_rx, Duration::from_secs(2));
            }

            let final_power = {
                let s = shared.lock().unwrap();
                let band = if *side == 0 { &s.left } else { &s.right };
                band.power.clone()
            };
            if final_power.to_lowercase().contains(target) {
                println!(" {}  功率已设为 {}", " OK ".on_dark_green().white().bold(), final_power);
                Ok(())
            } else {
                Err(format!("功率循环 5 次后当前为 {}，目标 {} 未达到", final_power, target.to_uppercase()))
            }
        }

        CliCommand::MainSwitch(side) => {
            let target_main = *side == 0; // true=LEFT
            let already = {
                let s = shared.lock().unwrap();
                (target_main && s.left_main) || (!target_main && s.right_main)
            };
            if already {
                println!(" {}  {} 已经是 MAIN，无需切换",
                    " OK ".on_dark_green().white().bold(),
                    if *side == 0 { "LEFT" } else { "RIGHT" });
                return Ok(());
            }
            println!(" {}  切换 MAIN → {}",
                " .. ".on_dark_blue().white().bold(),
                if *side == 0 { "LEFT" } else { "RIGHT" });
            // 发 P1 键切换 MAIN
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_PRESS, &[0x10]));
            std::thread::sleep(Duration::from_millis(200));
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_REL, &[]));
            wait_state(event_rx, Duration::from_secs(2));
            println!(" {}  MAIN 切换完成", " OK ".on_dark_green().white().bold());
            Ok(())
        }

        CliCommand::Tone => {
            println!(" {}  亚音模式循环（P3 键）", " .. ".on_dark_blue().white().bold());
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_PRESS, &[0x12]));
            std::thread::sleep(Duration::from_millis(200));
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KEY_REL, &[]));
            wait_state(event_rx, Duration::from_secs(2));
            println!(" {}  亚音键已发送", " OK ".on_dark_green().white().bold());
            Ok(())
        }

        CliCommand::Knob { up, n } => {
            let is_left_main = shared.lock().unwrap().left_main;
            let step_cw  = if is_left_main { 0x02u8 } else { 0x82u8 };
            let step_ccw = if is_left_main { 0x01u8 } else { 0x81u8 };
            let step = if *up { step_cw } else { step_ccw };
            println!(" {}  旋钮 {} × {} 格（{}侧）",
                " .. ".on_dark_blue().white().bold(),
                if *up { "CW↑" } else { "CCW↓" }, n,
                if is_left_main { "LEFT" } else { "RIGHT" });
            for _ in 0..*n {
                let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_RAW_KNOB, &[step]));
                std::thread::sleep(Duration::from_millis(80));
            }
            wait_state(event_rx, Duration::from_secs(2));
            let freq = {
                let s = shared.lock().unwrap();
                let band = if is_left_main { &s.left } else { &s.right };
                band.freq.clone()
            };
            println!(" {}  旋钮步进完成，当前频率: {} MHz",
                " OK ".on_dark_green().white().bold(), freq);
            Ok(())
        }

        CliCommand::PowerToggle => {
            println!(" {}  开关机指令（GPIO8 脉冲 1.2s）...", " .. ".on_dark_blue().white().bold());
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_POWER_TOGGLE, &[]));
            std::thread::sleep(Duration::from_secs(2));
            println!(" {}  开关机指令已发送", " OK ".on_dark_green().white().bold());
            Ok(())
        }

        CliCommand::Ptt(secs) => {
            println!(" {}  PTT 发射 {} 秒...", " .. ".on_dark_blue().white().bold(), secs);
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[1]));

            // 保持连接 + 等待（每秒打印倒计时）
            let end = Instant::now() + Duration::from_secs(*secs);
            let mut last_remaining = *secs + 1;
            while Instant::now() < end {
                let remaining = end.duration_since(Instant::now()).as_secs() + 1;
                if remaining != last_remaining {
                    println!(" {}  PTT 发射中，剩余 {} 秒...",
                        " TX ".on_dark_red().white().bold(), remaining);
                    last_remaining = remaining;
                }
                drain_events(event_rx);
                std::thread::sleep(Duration::from_millis(200));
            }

            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
            std::thread::sleep(Duration::from_millis(200));
            println!(" {}  PTT 已释放", " OK ".on_dark_green().white().bold());
            Ok(())
        }

        CliCommand::PttOff => {
            println!(" {}  强制关闭 PTT...", " .. ".on_dark_blue().white().bold());
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
            std::thread::sleep(Duration::from_millis(300));
            println!(" {}  PTT 已关闭", " OK ".on_dark_green().white().bold());
            Ok(())
        }

        CliCommand::PttTx(wav_path) => {
            println!(" {}  PTT + 播放音频文件（最长30s）: {}", " .. ".on_dark_blue().white().bold(), wav_path);

            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[1]));
            std::thread::sleep(Duration::from_millis(1000)); // PTT 建立延迟

            match audio::play_wav_to_usb(wav_path) {
                Ok(dur) => {
                    println!(" {}  音频播放完毕 ({:.1}s)", " OK ".on_dark_green().white().bold(), dur.as_secs_f32());
                }
                Err(e) => {
                    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
                    return Err(format!("音频播放失败: {}", e));
                }
            }

            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
            std::thread::sleep(Duration::from_millis(200));
            println!(" {}  PTT 已释放", " OK ".on_dark_green().white().bold());
            Ok(())
        }

        CliCommand::Tts { text, voice } => {
            let v = voice.as_deref().unwrap_or(tts::DEFAULT_VOICE);
            println!(" {}  TTS 合成中... 声线: {}  文字: \"{}\"",
                " .. ".on_dark_blue().white().bold(), v, text);

            // 1. 合成 → 保存到 recordings/
            let path = tts::synthesize(text, v)
                .map_err(|e| format!("TTS 合成失败: {}", e))?;
            println!(" {}  合成完成: {}", " OK ".on_dark_green().white().bold(),
                path.display());

            // 2. 发 PTT=1，等待 1 秒电台建立载波
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[1]));
            std::thread::sleep(Duration::from_millis(1000)); // PTT 建立延迟
            println!(" {}  PTT 发射中...", " TX ".on_dark_red().white().bold());

            // 3. 播放合成音频
            let path_str = path.to_string_lossy().to_string();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            match audio::play_audio_file_to_cm108(&path_str, 30, stop) {
                Ok(dur) => {
                    println!(" {}  发射完毕 ({:.1}s)", " OK ".on_dark_green().white().bold(),
                        dur.as_secs_f32());
                }
                Err(e) => {
                    let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
                    return Err(format!("TTS 发射失败: {}", e));
                }
            }

            // 4. 松 PTT
            let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_SET_PTT, &[0]));
            std::thread::sleep(Duration::from_millis(200));
            println!(" {}  PTT 已释放  录音已保存: {}", " OK ".on_dark_green().white().bold(),
                path.display());
            Ok(())
        }
    }
}

// ===== 内部辅助函数 =====

/// 发送频率输入序列（自动切 MAIN + 逐位数字键）
fn send_freq_sequence(
    cmd_tx: &mpsc::Sender<Vec<u8>>,
    shared: &SharedState,
    side: u8,
    digits: &[u8],
) {
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

/// 等待最多 timeout 时间内收到一个 StateUpdated 事件
fn wait_state(event_rx: &mpsc::Receiver<SerialEvent>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(SerialEvent::StateUpdated) = event_rx.try_recv() { return; }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 清空事件队列（防止积压）
fn drain_events(event_rx: &mpsc::Receiver<SerialEvent>) {
    while event_rx.try_recv().is_ok() {}
}
