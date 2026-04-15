// ===================================================================
// elfRadio BOX — 业余无线电控制盒上位机
// ===================================================================

mod protocol;
mod serial_link;
mod audio;
mod state;
mod monitor;
mod ui;
mod cli;
mod tts;
mod config;
mod firmware;
mod listen;

use crossterm::style::Stylize;
use std::sync::mpsc;
use std::time::Duration;

/// 用 crossterm 事件读取一行菜单输入，彻底避免 TUI 退出后 Windows Console 输入队列残留。
/// stdin().read_line() 在 crossterm raw mode 开启/关闭切换后会立即返回（队列有残留字符），
/// 此函数始终走 crossterm 事件路径，与 TUI 保持一致。
fn read_menu_line() -> String {
    use crossterm::{event::{self, Event, KeyCode, KeyEventKind}, terminal};
    let _ = terminal::enable_raw_mode();
    let mut buf = String::new();
    loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                match k.code {
                    KeyCode::Enter => break,
                    KeyCode::Char(c) => {
                        buf.push(c);
                        print!("{}", c);
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    KeyCode::Backspace if !buf.is_empty() => {
                        buf.pop();
                        print!("\x08 \x08");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let _ = terminal::disable_raw_mode();
    println!();
    buf
}

fn main() {
    // ── 解析命令行参数 ──────────────────────────────────────────────
    let cli_args = match cli::parse_args() {
        Ok(a) => a,
        Err(e) if e == "__interactive__" => {
            // 无子命令：显示主菜单（双击启动的默认行为）
            run_interactive(None, false);
            return;
        }
        Err(e) => {
            eprintln!("{} {}", " ERR".on_dark_red().white().bold(), e);
            cli::print_help();
            std::process::exit(1);
        }
    };

    // ── help：直接打印，无需串口 ────────────────────────────────────
    if matches!(cli_args.command, cli::CliCommand::Help) {
        cli::print_help();
        return;
    }

    // ── monitor：直接进 TUI（指定端口，跳过主菜单）─────────────────
    if matches!(cli_args.command, cli::CliCommand::Monitor) {
        run_interactive(cli_args.port.as_deref(), true);
        return;
    }

    // ── flash / flash-check：不需要 OTG 串口，只需 UART 调试线 ────
    if let cli::CliCommand::Flash { ref yes, ref flash_port } = cli_args.command {
        ui::print_banner();
        println!();
        run_firmware_update_cli(*yes, flash_port.as_deref());
        return;
    }
    if matches!(cli_args.command, cli::CliCommand::FlashCheck) {
        ui::print_banner();
        println!();
        run_firmware_check_cli();
        return;
    }

    // ── listen：需要 OTG 串口，长期运行，无 TUI ──────────────────────
    if let cli::CliCommand::Listen(opts) = cli_args.command {
        ui::print_banner();
        println!();
        let port_name = resolve_port_or_wait(cli_args.port.as_deref());
        listen::run_listen(&port_name, opts);  // 内部调用 exit()，不会返回
        return;
    }

    // ── 一次性 CLI 命令：连接 → 执行 → 退出 ────────────────────────
    ui::print_banner();
    println!();

    let port_name = match resolve_port(cli_args.port.as_deref()) {
        Some(p) => p,
        None => {
            ui::print_err("未找到 ESP32 串口，请用 --port 指定");
            std::process::exit(1);
        }
    };

    let shared = state::new_shared_state();
    let port = match serial_link::open_port(&port_name) {
        Ok(p) => {
            ui::print_ok(&format!("串口 {} 已连接", port_name));
            p
        }
        Err(e) => {
            ui::print_err(&e);
            std::process::exit(1);
        }
    };

    let (event_tx, event_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let _rx = serial_link::spawn_rx_thread(port.clone(), shared.clone(), event_tx);
    let _tx = serial_link::spawn_tx_thread(port.clone(), cmd_rx);

    // 等待首个状态报告（最多 4 秒）
    ui::print_info("等待 ESP32 状态报告...");
    let mut got_state = false;
    for _ in 0..20 {
        let _ = cmd_tx.send(protocol::encode_frame(protocol::CMD_GET_STATE, &[]));
        std::thread::sleep(Duration::from_millis(200));
        while let Ok(e) = event_rx.try_recv() {
            if matches!(e, serial_link::SerialEvent::StateUpdated) { got_state = true; }
        }
        if got_state { break; }
    }
    if !got_state {
        ui::print_warn("未收到状态报告，仍继续执行...");
    } else {
        let s = shared.lock().unwrap();
        let radio = if s.radio_alive { "在线".green().to_string() } else { "离线".red().to_string() };
        ui::print_ok(&format!("电台: {}  Down:{} Up:{}  Left:{} Right:{}",
            radio, s.body_count, s.head_count, s.left.freq, s.right.freq));
    }

    println!();
    match cli::run_command(&cli_args.command, &shared, &cmd_tx, &event_rx) {
        Ok(()) => {
            // CLI 一次性命令：直接 exit 强制关闭串口（RX 线程阻塞读不会自然退出）
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!(" {}  {}", " ERR".on_dark_red().white().bold(), e);
            std::process::exit(1);
        }
    }
}

// ── 交互主菜单模式 ─────────────────────────────────────────────────

/// 解析并等待串口就绪：指定端口 → 自动检测 → 无限等待直到出现
fn resolve_port_or_wait(forced: Option<&str>) -> String {
    match forced {
        Some(p) => {
            ui::print_ok(&format!("使用指定串口: {}", p));
            p.to_string()
        }
        None => match serial_link::auto_detect_port() {
            Some(name) => {
                ui::print_ok(&format!("自动检测到 ESP32: {}", name));
                name
            }
            None => {
                ui::print_info("未检测到 ESP32，等待连接... (Ctrl+C 退出)");
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    if let Some(name) = serial_link::auto_detect_port() {
                        ui::print_ok(&format!("自动检测到 ESP32: {}", name));
                        break name;
                    }
                }
            }
        },
    }
}

fn run_interactive(forced_port: Option<&str>, direct_tui: bool) {
    ui::print_banner();
    println!();

    let port_name = resolve_port_or_wait(forced_port);

    let shared = state::new_shared_state();

    // 持久监听模式（monitor 命令）：TUI 常驻，自动重连
    if direct_tui {
        monitor::run_monitor_persistent(&port_name, shared);
        println!("再见！ 73 de VK7KSM");
        return;
    }

    // 主菜单循环前做一次状态初始化（最多等 2 秒，恢复老 run_session 的状态报告行为）
    initial_state_check(&port_name, &shared);

    // 主菜单循环（不维护串口连接，连接由 run_monitor_persistent 内部管理）
    loop {
        println!();
        println!("{}", "═".repeat(60).dark_grey());
        println!("  {} - {}", "elfRadio BOX".cyan().bold(), "主菜单".white());
        println!("{}", "═".repeat(60).dark_grey());
        println!();
        println!("  {} 监听模式     实时监听、显示状态、录音、PTT 发射", "[1]".yellow().bold());
        println!("  {} 连接状态     ESP32 / 电台 / 声卡信息", "[2]".yellow().bold());
        println!("  {} 设置         TTS 声线 / 降噪默认值", "[3]".yellow().bold());
        println!("  {} 固件更新     联网检查并烧录最新 ESP32 固件", "[4]".yellow().bold());
        println!();
        println!("  {} 退出", "[0]".yellow().bold());
        println!();
        {
            let s = shared.lock().unwrap();
            let radio = if s.radio_alive { "OK".green().to_string() } else { "--".dark_grey().to_string() };
            let pc    = if s.pc_alive   { "OK".green().to_string() } else { "--".dark_grey().to_string() };
            println!("{}", "─".repeat(60).dark_grey());
            println!("  串口: {} │ Radio: {} │ PC: {}",
                port_name.as_str().yellow(), radio, pc);
        }

        print!("{} ", "请选择 >".dark_cyan());
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        let input = read_menu_line();

        match input.as_str() {
            "1" => {
                // 进入持久 TUI 监听模式（内部管理连接 + 自动重连）
                // 用户按 Esc 后退出 TUI，回到此主菜单
                monitor::run_monitor_persistent(&port_name, shared.clone());
            }
            "2" => {
                println!();
                show_status(&shared, &port_name);
                println!("  按 Enter 返回...");
                let _ = read_menu_line();
            }
            "3" => {
                println!();
                run_settings();
            }
            "4" => {
                println!();
                run_firmware_update();
            }
            "0" | "q" => break,
            _ => {}
        }
    }

    println!("再见！ 73 de VK7KSM");
}

// ── CLI 版固件版本查询（仅查，不烧）────────────────────────────────

fn run_firmware_check_cli() {
    use crossterm::style::Stylize;

    println!("  正在查询 GitHub 最新版本...");
    let info = match firmware::check_latest() {
        Ok(i) => i,
        Err(e) => {
            println!("  {} {}", " ERR".on_dark_red().white().bold(), e);
            std::process::exit(1);
        }
    };

    let cfg = config::load_config();
    let local = if cfg.firmware_version.is_empty() { "（未记录）".to_string() } else { cfg.firmware_version.clone() };

    println!("  本地已记录版本: {}", local.as_str().cyan());
    println!("  GitHub 最新版本: {}", info.tag.as_str().yellow().bold());
    println!("  固件下载地址:   {}", info.firmware_url.as_str().dark_grey());
    println!();
    if cfg.firmware_version == info.tag {
        println!("  {} 当前已是最新版本", " OK ".on_dark_green().white().bold());
        std::process::exit(0);  // exit(0) = 已是最新
    } else {
        println!("  {} 有新版本可用，运行 'flash' 命令升级", " !! ".on_dark_yellow().white().bold());
        std::process::exit(2);  // exit(2) = 有新版本（方便脚本判断）
    }
}

// ── CLI 版固件更新（下载 + 烧录，支持 --yes 非交互）────────────────

fn run_firmware_update_cli(yes: bool, flash_port: Option<&str>) {
    use crossterm::style::Stylize;

    println!("{}", "─".repeat(60).dark_grey());
    println!("  {} 固件更新（CLI 模式）", "elfRadio BOX".cyan().bold());
    println!("{}", "─".repeat(60).dark_grey());
    println!();

    // 1. 读取本地已记录版本
    let mut cfg = config::load_config();
    let local_ver = if cfg.firmware_version.is_empty() {
        "（未记录）".to_string()
    } else {
        cfg.firmware_version.clone()
    };
    println!("  本地已烧录版本: {}", local_ver.as_str().cyan());
    println!();
    print!("  正在检查 GitHub 最新版本...");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();

    // 2. 查询 GitHub
    let info = match firmware::check_latest() {
        Ok(i) => i,
        Err(e) => {
            println!();
            println!("  {} {}", " ERR".on_dark_red().white().bold(), e);
            std::process::exit(1);
        }
    };
    println!(" 完成");
    println!();
    if cfg.firmware_version == info.tag {
        println!("  {} 当前已是最新版本 ({})", " OK ".on_dark_green().white().bold(), info.tag);
        if !yes {
            print!("  仍然烧录？(Y/其他=退出) > ");
            std::io::Write::flush(&mut std::io::stdout()).unwrap();
            let ans = read_menu_line();
            if ans.trim().to_lowercase() != "y" {
                println!("  已取消。");
                std::process::exit(0);
            }
        }
    } else {
        println!("  发现新版本: {}  （当前: {}）",
            info.tag.as_str().yellow().bold(), local_ver.as_str().dark_grey());
        if !yes {
            print!("  是否下载并烧录？(Y/其他=取消) > ");
            std::io::Write::flush(&mut std::io::stdout()).unwrap();
            let ans = read_menu_line();
            if ans.trim().to_lowercase() != "y" {
                println!("  已取消。");
                std::process::exit(0);
            }
        }
    }
    println!("  固件来源: {}", info.firmware_url.as_str().dark_grey());

    // 3. 下载固件
    println!();
    print!("  下载中... 0 KB");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let bin_path = match firmware::download_firmware(&info.firmware_url, |dl, total| {
        let dl_kb   = dl   / 1024;
        let total_s = if total > 0 { format!("{} KB", total / 1024) } else { "? KB".to_string() };
        print!("\r  下载中... {} KB / {}    ", dl_kb, total_s);
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }) {
        Ok(p) => { println!(); println!("  {} 下载完成: {}", " OK ".on_dark_green().white().bold(), p.display()); p }
        Err(e) => {
            println!();
            println!("  {} {}", " ERR".on_dark_red().white().bold(), e);
            std::process::exit(1);
        }
    };

    // 4. 检测 UART 烧录口
    println!();
    print!("  检测 UART 烧录口...");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let port = match flash_port {
        Some(p) => { println!(" 使用指定: {}", p.yellow()); p.to_string() }
        None    => match firmware::find_flash_port() {
            Some(p) => { println!(" 找到: {}", p.as_str().yellow()); p }
            None => {
                println!();
                println!("  {} 未检测到 UART 烧录口", " ERR".on_dark_red().white().bold());
                println!("  请确认控制盒 UART 调试线已插入 PC，或用 --flash-port 指定");
                std::process::exit(1);
            }
        }
    };

    // 5. 二次确认（--yes 时跳过）
    if !yes {
        println!();
        print!("  即将向 {} 烧录固件 {}，ESP32 将重启。继续？(Y/n) > ",
            port.as_str().yellow(), info.tag.as_str().yellow());
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        let confirm = read_menu_line();
        if !confirm.trim().is_empty() && confirm.trim().to_lowercase() != "y" {
            println!("  已取消。");
            std::process::exit(0);
        }
    } else {
        println!("  [--yes] 跳过确认，直接烧录 {} → {}", info.tag.as_str().yellow(), port.as_str().yellow());
    }

    // 6. 烧录
    println!();
    println!("{}", "─".repeat(60).dark_grey());
    println!("  正在连接 ESP32 bootloader...");
    let mut last_pct: usize = 0;
    let result = firmware::flash_firmware(&bin_path, &port, |current, total| {
        if total > 0 {
            let pct = current * 100 / total;
            if pct != last_pct {
                last_pct = pct;
                print!("\r  烧录中... {}% ({}/{} KB)    ",
                    pct, current / 1024, total / 1024);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
    });
    println!();
    println!("{}", "─".repeat(60).dark_grey());
    println!();

    match result {
        Ok(()) => {
            println!("  {} 烧录完成！ESP32 正在重启...", " OK ".on_dark_green().white().bold());
            cfg.firmware_version = info.tag.clone();
            match config::save_config(&cfg) {
                Ok(())  => println!("  已记录版本 {} 到配置文件", info.tag.as_str().cyan()),
                Err(e)  => println!("  警告：版本记录失败: {}", e),
            }
            std::process::exit(0);
        }
        Err(e) => {
            println!("  {} 烧录失败: {}", " ERR".on_dark_red().white().bold(), e);
            std::process::exit(1);
        }
    }
}

// ── 交互菜单版固件更新（不变）──────────────────────────────────────

fn run_firmware_update() {
    use crossterm::style::Stylize;

    println!("{}", "─".repeat(60).dark_grey());
    println!("  {} 固件更新", "elfRadio BOX".cyan().bold());
    println!("{}", "─".repeat(60).dark_grey());
    println!();

    // ── 1. 读取本地已记录版本 ────────────────────────────────────────
    let mut cfg = config::load_config();
    let local_ver = if cfg.firmware_version.is_empty() {
        "（未记录）".to_string()
    } else {
        cfg.firmware_version.clone()
    };
    println!("  本地已烧录版本: {}", local_ver.as_str().cyan());
    println!();
    print!("  正在检查 GitHub 最新版本...");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();

    // ── 2. 查询 GitHub Releases ──────────────────────────────────────
    let info = match firmware::check_latest() {
        Ok(i) => i,
        Err(e) => {
            println!();
            println!("  {} {}", " ERR".on_dark_red().white().bold(), e);
            println!("  按 Enter 返回...");
            let _ = read_menu_line();
            return;
        }
    };
    println!(" 完成");
    println!();
    if cfg.firmware_version == info.tag {
        println!("  {} 当前已是最新版本 ({})", " OK ".on_dark_green().white().bold(), info.tag);
        println!("  仍可选择强制重新烧录。");
    } else {
        println!("  发现新版本: {}  （当前: {}）",
            info.tag.as_str().yellow().bold(),
            local_ver.as_str().dark_grey());
    }
    println!();
    println!("  固件来源: {}", info.firmware_url.as_str().dark_grey());
    println!();
    print!("  是否下载并烧录？(Y/其他=取消) > ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let ans = read_menu_line();
    if ans.trim().to_lowercase() != "y" {
        println!("  已取消。");
        println!("  按 Enter 返回...");
        let _ = read_menu_line();
        return;
    }

    // ── 3. 下载固件 ──────────────────────────────────────────────────
    println!();
    print!("  下载中... 0 KB");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let bin_path = match firmware::download_firmware(&info.firmware_url, |dl, total| {
        let dl_kb   = dl   / 1024;
        let total_s = if total > 0 { format!("{} KB", total / 1024) } else { "? KB".to_string() };
        print!("\r  下载中... {} KB / {}    ", dl_kb, total_s);
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }) {
        Ok(p) => { println!(); println!("  {} 下载完成: {}", " OK ".on_dark_green().white().bold(), p.display()); p }
        Err(e) => {
            println!();
            println!("  {} {}", " ERR".on_dark_red().white().bold(), e);
            println!("  按 Enter 返回...");
            let _ = read_menu_line();
            return;
        }
    };

    // ── 4. 检测 UART 烧录口（VID≠0x303A 的 USB 串口 = CH343/CH340 等 UART 桥）──────
    println!();
    print!("  检测 UART 烧录口...");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let port = match firmware::find_flash_port() {
        Some(p) => { println!(" 找到: {}", p.as_str().yellow()); p }
        None => {
            println!();
            println!("  {} 未检测到 UART 烧录口", " ERR".on_dark_red().white().bold());
            println!("  请确认控制盒 UART 调试线（非 OTG 线）已插入 PC");
            println!("  按 Enter 返回...");
            let _ = read_menu_line();
            return;
        }
    };

    // ── 5. 再次确认 ──────────────────────────────────────────────────
    println!();
    print!("  即将向 {} 烧录固件 {}，ESP32 将重启。继续？(Y/n) > ",
        port.as_str().yellow(), info.tag.as_str().yellow());
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let confirm = read_menu_line();
    if !confirm.trim().is_empty() && confirm.trim().to_lowercase() != "y" {
        println!("  已取消。");
        println!("  按 Enter 返回...");
        let _ = read_menu_line();
        return;
    }

    // ── 6. 烧录 ──────────────────────────────────────────────────────
    println!();
    println!("{}", "─".repeat(60).dark_grey());
    println!("  正在连接 ESP32 bootloader...");
    let mut last_pct: usize = 0;
    let result = firmware::flash_firmware(&bin_path, &port, |current, total| {
        if total > 0 {
            let pct = current * 100 / total;
            if pct != last_pct {
                last_pct = pct;
                print!("\r  烧录中... {}% ({}/{} KB)    ",
                    pct, current / 1024, total / 1024);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
    });
    println!();  // 换行（清掉进度条 \r 行）
    println!("{}", "─".repeat(60).dark_grey());
    println!();

    match result {
        Ok(()) => {
            println!("  {} 烧录完成！ESP32 正在重启...", " OK ".on_dark_green().white().bold());
            // 记录已烧录版本
            cfg.firmware_version = info.tag.clone();
            match config::save_config(&cfg) {
                Ok(()) => println!("  已记录版本 {} 到配置文件", info.tag.as_str().cyan()),
                Err(e) => println!("  警告：版本记录失败: {}", e),
            }
        }
        Err(e) => {
            println!("  {} 烧录失败: {}", " ERR".on_dark_red().white().bold(), e);
        }
    }

    println!();
    println!("  按 Enter 返回...");
    let _ = read_menu_line();
}

fn run_settings() {
    let mut cfg = config::load_config();
    loop {
        println!("{}", "─".repeat(60).dark_grey());
        println!("  {} 设置", "elfRadio BOX".cyan().bold());
        println!("{}", "─".repeat(60).dark_grey());
        println!();
        println!("  {} TTS 声线     当前: {}", "[1]".yellow().bold(), cfg.tts_voice.as_str().cyan());
        println!("  {} 降噪默认值   当前: {}（0=关闭，10-100=强度）",
            "[2]".yellow().bold(),
            format!("{}", cfg.denoise_db).as_str().cyan());
        println!();
        println!("  {} 保存并返回", "[0]".yellow().bold());
        println!();
        print!("{} ", "请选择 >".dark_cyan());
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        let input = read_menu_line();
        match input.as_str() {
            "1" => {
                println!("  常用声线:");
                println!("    zh-TW-HsiaoChenNeural  （台湾女声，默认）");
                println!("    zh-CN-XiaoxiaoNeural   （普通话女声）");
                println!("    zh-CN-YunxiNeural      （普通话男声）");
                println!("    en-US-GuyNeural        （英文男声）");
                print!("  输入声线名称（Enter 保持当前）: ");
                std::io::Write::flush(&mut std::io::stdout()).unwrap();
                let v = read_menu_line();
                if !v.is_empty() {
                    cfg.tts_voice = v;
                    println!("  {} 声线已设为: {}", " OK ".on_dark_green().white().bold(), cfg.tts_voice);
                }
            }
            "2" => {
                print!("  输入降噪强度 0-100（0=关闭，Enter 保持当前）: ");
                std::io::Write::flush(&mut std::io::stdout()).unwrap();
                let v = read_menu_line();
                if !v.is_empty() {
                    match v.parse::<f32>() {
                        Ok(n) if n >= 0.0 && n <= 100.0 => {
                            cfg.denoise_db = n;
                            println!("  {} 降噪强度已设为: {}", " OK ".on_dark_green().white().bold(), n);
                        }
                        _ => println!("  {} 输入无效，必须是 0-100 之间的数字",
                            " ERR".on_dark_red().white().bold()),
                    }
                }
            }
            "0" | "" => {
                match config::save_config(&cfg) {
                    Ok(()) => println!("  {} 配置已保存", " OK ".on_dark_green().white().bold()),
                    Err(e) => println!("  {} {}", " ERR".on_dark_red().white().bold(), e),
                }
                break;
            }
            _ => {}
        }
    }
}

fn resolve_port(forced: Option<&str>) -> Option<String> {
    if let Some(p) = forced { return Some(p.to_string()); }
    serial_link::auto_detect_port()
}

/// 主菜单启动前的一次性状态查询（同步、无线程、函数返回后串口 100% 释放）
fn initial_state_check(port_name: &str, shared: &state::SharedState) {
    ui::print_info("正在获取 ESP32 状态...");
    let mut port = match serialport::new(port_name, 115_200)
        .timeout(Duration::from_millis(100))
        .open() {
        Ok(p) => p,
        Err(_) => {
            ui::print_warn("串口暂时不可用，跳过状态检测");
            return;
        }
    };
    let _ = port.write_data_terminal_ready(false);
    let _ = port.write_request_to_send(false);

    let mut parser = protocol::FrameParser::new();
    let mut buf = [0u8; 256];
    for i in 0..40 {  // 最多等 4 秒（40 × 100ms read timeout）
        // 每 500ms（每 5 次迭代）重发一次 CMD_GET_STATE
        // ESP32 pc_comm 可能在 USB 枚举后 1-3 秒才就绪，需要多次请求
        if i % 5 == 0 {
            // 先发 heartbeat → ESP32 收到后设 pc_alive=true，之后才响应 CMD_GET_STATE
            let hb = protocol::encode_frame(protocol::CMD_HEARTBEAT, &[]);
            if port.write_all(&hb).is_err() { return; }
            let _ = port.flush();
            let frame = protocol::encode_frame(protocol::CMD_GET_STATE, &[]);
            if port.write_all(&frame).is_err() { return; }
            let _ = port.flush();
        }
        match port.read(&mut buf) {
            Ok(n) => {
                for b in &buf[..n] {
                    if let Some(evt) = parser.feed(*b) {
                        if let protocol::ParseEvent::Frame { typ, payload } = evt {
                            if typ == protocol::RPT_STATE_REPORT {
                                if let Some(new_state) = state::decode_state_report(&payload) {
                                    *shared.lock().unwrap() = new_state;
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return,
        }
    }
    // port 在此 drop → 串口自动关闭，无 Arc 无线程泄漏
}

fn show_status(shared: &state::SharedState, port_name: &str) {
    let s = shared.lock().unwrap();
    println!("  ─── 连接状态 ──────────────────────────────");
    println!("  串口:    {} {}", port_name.yellow(),
        if s.pc_alive { "在线".green().to_string() } else { "离线".red().to_string() });
    println!("  电台:    {}", if s.radio_alive { "在线".green().to_string() } else { "离线".red().to_string() });
    println!("  下行帧:  {}", s.body_count);
    println!("  上行帧:  {}", s.head_count);
    println!("  PC帧:    {}", s.pc_count);
    println!();
    println!("  ─── 电台状态 ──────────────────────────────");
    let print_band = |label: &str, b: &state::BandState, is_main: bool| {
        let main_tag = if is_main { " MAIN".yellow().bold().to_string() } else { "     ".to_string() };
        println!("  {}{} {} {} MHz S{} {} {}",
            label, main_tag, b.mode, b.freq, b.s_level,
            if b.is_tx { "TX".red().to_string() } else { "RX".green().to_string() },
            b.power);
        println!("         VOL:{}%  SQL:{}%  {} {} Ch:{}",
            b.vol_pct(), b.sql_pct(), b.tone_str(), b.shift_str(), b.channel);
    };
    print_band("LEFT ", &s.left, s.left_main);
    print_band("RIGHT", &s.right, s.right_main);
    println!();
    println!("  ─── 音频设备 ──────────────────────────────");
    for (name, _) in audio::list_audio_devices() {
        println!("  {}", name);
    }
    println!();
}

fn wait_enter() {
    println!("按 Enter 退出...");
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
}
