// ===================================================================
// 终端 UI 工具（颜色标签、文本格式化）
// 仿 MMDVM elfRadio 风格
// ===================================================================

use crossterm::style::{self, Stylize, StyledContent};
use std::io::{self, Write};

/// 状态标签（彩色背景 + 白色粗体文字）
pub fn tag_ok() -> StyledContent<&'static str>  { " OK ".on_dark_green().white().bold() }
pub fn tag_err() -> StyledContent<&'static str> { " ERR".on_dark_red().white().bold() }
pub fn tag_warn() -> StyledContent<&'static str>{ " !! ".on_dark_yellow().white().bold() }
pub fn tag_info() -> StyledContent<&'static str>{ " .. ".on_dark_blue().white().bold() }
pub fn tag_rx() -> StyledContent<&'static str>  { " RX ".on_dark_blue().white().bold() }
pub fn tag_tx() -> StyledContent<&'static str>  { " TX ".on_dark_green().white().bold() }
pub fn tag_rec() -> StyledContent<&'static str> { "● REC".on_red().white().bold() }
pub fn tag_stt() -> StyledContent<&'static str> { "STT ".on_dark_green().white().bold() }

/// 打印带标签的消息行
pub fn print_ok(msg: &str) {
    let mut out = io::stdout();
    let _ = crossterm::execute!(out, style::PrintStyledContent(tag_ok()));
    println!("  {}", msg);
}

pub fn print_err(msg: &str) {
    let mut out = io::stdout();
    let _ = crossterm::execute!(out, style::PrintStyledContent(tag_err()));
    println!("  {}", msg);
}

pub fn print_info(msg: &str) {
    let mut out = io::stdout();
    let _ = crossterm::execute!(out, style::PrintStyledContent(tag_info()));
    println!("  {}", msg);
}

pub fn print_warn(msg: &str) {
    let mut out = io::stdout();
    let _ = crossterm::execute!(out, style::PrintStyledContent(tag_warn()));
    println!("  {}", msg);
}

/// 绘制分隔线（全宽）
pub fn draw_separator(width: u16) {
    println!("{}", "─".repeat(width as usize));
}

/// 绘制标题分隔线（带标题文字）
pub fn draw_titled_separator(title: &str, ts: &str, width: u16) {
    let prefix = format!("─── {} ── {} ", title, ts);
    let remaining = (width as usize).saturating_sub(display_width(&prefix));
    println!("{}{}", prefix, "─".repeat(remaining));
}

/// 计算字符串显示宽度（CJK 字符占 2 列）
pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| {
        if c as u32 > 0x7F { 2 } else { 1 }
    }).sum()
}

/// ASCII Art Banner
pub fn print_banner() {
    let banner = r#"
    ┌──────────────────────────────────────────────────────────┐
    │           _  __ ____            _ _                       │
    │       ___| |/ _|  _ \ __ _  __| (_) ___                  │
    │      / _ \ | |_| |_) / _` |/ _` | |/ _ \                │
    │     |  __/ |  _|  _ < (_| | (_| | | (_) |                │
    │      \___|_|_| |_| \_\__,_|\__,_|_|\___/                │
    │                                        = BOX =           │
    │   业余无线电控制盒上位机    v0.1.0                        │
    └──────────────────────────────────────────────────────────┘"#;
    use crossterm::style::Stylize;
    println!("{}", banner.cyan());
}
