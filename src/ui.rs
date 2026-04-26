// ===================================================================
// UI 绘图函数 + 配色/字形常量
// 从 main.rs 提取，适配 state::BandState
// ===================================================================

use embedded_graphics::image::{Image, ImageRawLE};
use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyleBuilder, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Text};
use profont::{PROFONT_24_POINT, PROFONT_14_POINT, PROFONT_12_POINT, PROFONT_9_POINT};

use crate::state::{BandState, PowerLevel, WifiState};

// ===== 配色 =====
pub const BG:     Rgb565 = Rgb565::BLACK;
pub const AMBER:  Rgb565 = Rgb565::new(31, 50, 0);
pub const CYAN:   Rgb565 = Rgb565::new(0, 58, 31);
pub const GREEN:  Rgb565 = Rgb565::new(4, 63, 4);
pub const RED:    Rgb565 = Rgb565::new(31, 10, 0);
pub const WHITE:  Rgb565 = Rgb565::WHITE;
pub const GRAY:   Rgb565 = Rgb565::new(14, 30, 14);
pub const BORDER: Rgb565 = Rgb565::new(6, 18, 10);
pub const PANEL:  Rgb565 = Rgb565::new(1, 3, 2);
pub const TX_BG:  Rgb565 = Rgb565::new(4, 2, 1);

// ===== 布局常量 =====
const BAR_X: i32 = 36;
const BAR_W: u32 = 106;
const VAL_X: i32 = 146;

// ===== 编译时嵌入的 Logo =====
const LOGO_DATA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo.raw"));
const LOGO_W: u32 = 200;
const LOGO_H: u32 = 166;

// ===== Unifont 16×16 CJK 字形 =====
pub const GLYPH_GAO: [u16; 16] = [0x0200, 0x0100, 0xFFFE, 0x0000, 0x0FE0, 0x0820, 0x0820, 0x0FE0, 0x0000, 0x7FFC, 0x4004, 0x4FE4, 0x4824, 0x4824, 0x4FE4, 0x400C]; // 高
pub const GLYPH_GONG: [u16; 16] = [0x0040, 0x0040, 0x0040, 0xFE40, 0x11FC, 0x1044, 0x1044, 0x1044, 0x1044, 0x1084, 0x1084, 0x1E84, 0xF104, 0x4104, 0x0228, 0x0410]; // 功
#[allow(dead_code)]
pub const GLYPH_ZHONG: [u16; 16] = [0x0100, 0x0100, 0x0100, 0x0100, 0x3FF8, 0x2108, 0x2108, 0x2108, 0x2108, 0x2108, 0x3FF8, 0x2108, 0x0100, 0x0100, 0x0100, 0x0100]; // 中
pub const GLYPH_DI: [u16; 16] = [0x0808, 0x083C, 0x0BE0, 0x1220, 0x1220, 0x3220, 0x3220, 0x53FE, 0x9220, 0x1210, 0x1210, 0x1212, 0x120A, 0x128A, 0x1326, 0x1212]; // 低
pub const GLYPH_MANG: [u16; 16] = [0x1020, 0x1010, 0x1010, 0x1000, 0x1BFE, 0x5480, 0x5080, 0x5080, 0x9080, 0x1080, 0x1080, 0x1080, 0x1080, 0x1080, 0x10FE, 0x1000]; // 忙
pub const GLYPH_SUO: [u16; 16] = [0x1020, 0x1124, 0x3CA4, 0x20A8, 0x4020, 0xBDFC, 0x1104, 0x1124, 0xFD24, 0x1124, 0x1124, 0x1124, 0x1450, 0x1888, 0x1104, 0x0202]; // 锁
pub const GLYPH_JING: [u16; 16] = [0x1040, 0x1040, 0xFE78, 0x1088, 0x7C10, 0x11FC, 0xFE24, 0x0024, 0x7DFE, 0x4424, 0x7C24, 0x45FC, 0x7C24, 0x4420, 0x54A0, 0x4840]; // 静
pub const GLYPH_YIN: [u16; 16] = [0x0200, 0x0100, 0x3FF8, 0x0000, 0x0820, 0x0440, 0xFFFE, 0x0000, 0x1FF0, 0x1010, 0x1010, 0x1FF0, 0x1010, 0x1010, 0x1FF0, 0x1010]; // 音
pub const GLYPH_TIAO: [u16; 16] = [0x0050, 0x7C50, 0x4450, 0x4552, 0x44D4, 0x7C58, 0x1050, 0x1058, 0x10D4, 0x5D52, 0x5050, 0x5050, 0x5092, 0x5C92, 0xE112, 0x020E]; // 跳
pub const GLYPH_GUO: [u16; 16] = [0x0010, 0x2010, 0x1010, 0x17FE, 0x0010, 0x0010, 0xF210, 0x1110, 0x1110, 0x1010, 0x1010, 0x1050, 0x1020, 0x2800, 0x47FE, 0x0000]; // 过
pub const GLYPH_XIN: [u16; 16] = [0x0840, 0x0820, 0x0BFE, 0x1000, 0x1000, 0x31FC, 0x3000, 0x5000, 0x91FC, 0x1000, 0x1000, 0x11FC, 0x1104, 0x1104, 0x11FC, 0x1104]; // 信
pub const GLYPH_DAO: [u16; 16] = [0x0208, 0x2110, 0x1000, 0x17FC, 0x0080, 0x03F8, 0xF208, 0x13F8, 0x1208, 0x13F8, 0x1208, 0x13F8, 0x1208, 0x2800, 0x47FE, 0x0000]; // 道
pub const GLYPH_JIAN: [u16; 16] = [0x2010, 0x2010, 0x3B7C, 0x2114, 0x41FE, 0x7A14, 0xA27C, 0x2710, 0xF97C, 0x2510, 0x25FE, 0x2210, 0x2A10, 0x3500, 0x28FE, 0x0000]; // 键
pub const GLYPH_PAN: [u16; 16] = [0x0200, 0x0400, 0x1FF0, 0x1110, 0x1090, 0xFFFE, 0x1010, 0x1210, 0x2150, 0x4020, 0x3FF8, 0x2448, 0x2448, 0x2448, 0xFFFE, 0x0000]; // 盘

fn draw_cjk_16<D: DrawTarget<Color = Rgb565>>(display: &mut D, x: i32, y: i32, glyph: &[u16; 16], color: Rgb565)
where D::Error: core::fmt::Debug,
{
    for row in 0..16usize {
        let bits = glyph[row];
        for col in 0..16usize {
            if bits & (0x8000 >> col) != 0 {
                Pixel(Point::new(x + col as i32, y + row as i32), color)
                    .draw(display).unwrap();
            }
        }
    }
}

/// 绘制 CJK 徽章（圆角底色 + 文字，右对齐）
fn draw_cjk_badge<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    right_x: i32, y: i32,
    glyphs: &[&[u16; 16]], bg: Rgb565,
) where D::Error: core::fmt::Debug,
{
    let n = glyphs.len() as i32;
    let w = (n * 16 + 2) as u32;
    let h = 16u32;
    let x = right_x - w as i32;
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(x, y), Size::new(w, h)),
        Size::new(2, 2),
    ).into_styled(PrimitiveStyleBuilder::new().fill_color(bg).build())
        .draw(display).unwrap();
    for (i, glyph) in glyphs.iter().enumerate() {
        draw_cjk_16(display, x + 1 + (i as i32) * 16, y, glyph, WHITE);
    }
}

/// 获取功率等级对应的字形
fn power_glyphs(power: PowerLevel) -> &'static [&'static [u16; 16]] {
    match power {
        PowerLevel::High => &[&GLYPH_GAO, &GLYPH_GONG],
        PowerLevel::Mid => &[&GLYPH_ZHONG, &GLYPH_GONG],
        PowerLevel::Low => &[&GLYPH_DI, &GLYPH_GONG],
    }
}

// ===== 开机画面 =====
pub fn draw_splash<D: DrawTarget<Color = Rgb565>>(fb: &mut D)
where D::Error: core::fmt::Debug,
{
    fb.clear(Rgb565::BLACK).unwrap();

    let logo_x = ((240 - LOGO_W) / 2) as i32;
    let logo_y = 30_i32;
    let logo_raw = ImageRawLE::<Rgb565>::new(LOGO_DATA, LOGO_W);
    Image::new(&logo_raw, Point::new(logo_x, logo_y))
        .draw(fb).unwrap();

    let title_style = MonoTextStyleBuilder::new()
        .font(&PROFONT_24_POINT)
        .text_color(AMBER)
        .build();
    Text::with_alignment("elfRadio Box",
        Point::new(120, logo_y + LOGO_H as i32 + 35), title_style, Alignment::Center)
        .draw(fb).unwrap();

    let ver_style = MonoTextStyleBuilder::new()
        .font(&PROFONT_14_POINT)
        .text_color(CYAN)
        .build();
    Text::with_alignment("V 0.1.0",
        Point::new(120, logo_y + LOGO_H as i32 + 60), ver_style, Alignment::Center)
        .draw(fb).unwrap();

    log::info!("开机画面已显示");
}

// ===== 主界面绘制 =====
pub fn draw_main_ui<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    left: &BandState,
    right: &BandState,
    radio_alive: bool,
    pc_alive: bool,
    wifi_state: &WifiState,
    wifi_ip: &str,
    rigctld_clients: u32,
) where D::Error: core::fmt::Debug,
{
    display.clear(BG).unwrap();

    // 顶栏
    Rectangle::new(Point::new(0, 0), Size::new(240, 22))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(PANEL).build())
        .draw(display).unwrap();
    hline(display, 22, BORDER);

    Text::new("TYT TH-9800", Point::new(6, 16),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(WHITE).build())
        .draw(display).unwrap();
    Text::with_alignment("V0.1.0", Point::new(234, 16),
        MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(CYAN).build(),
        Alignment::Right).draw(display).unwrap();

    // 波段1
    draw_band(display, 23, left);
    hline(display, 159, BORDER);

    // 波段2
    draw_band(display, 160, right);

    // 底栏
    hline(display, 296, BORDER);
    Rectangle::new(Point::new(0, 297), Size::new(240, 22))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(PANEL).build())
        .draw(display).unwrap();

    // 左下角：WiFi 状态 / IP（rigctld 客户端连接时 IP 显示橙色）
    let (wifi_text, wifi_color): (&str, Rgb565) = match wifi_state {
        WifiState::Connected    => (wifi_ip, if rigctld_clients > 0 { AMBER } else { CYAN }),
        WifiState::Connecting   => ("WiFi:connect..", AMBER),
        WifiState::NoCredentials => ("WiFi:no setup", GRAY),
        WifiState::Failed       => ("WiFi:fail", GRAY),
        WifiState::Disabled     => ("WiFi:OFF", GRAY),
    };
    Text::new(wifi_text, Point::new(6, 314),
        MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(wifi_color).build())
        .draw(display).unwrap();

    // PC 状态
    if pc_alive {
        Text::new("PC OK", Point::new(120, 314),
            MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(GREEN).build())
            .draw(display).unwrap();
    } else {
        Text::new("PC --", Point::new(120, 314),
            MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(GRAY).build())
            .draw(display).unwrap();
    }

    // Radio 状态
    if radio_alive {
        Text::with_alignment("Radio OK", Point::new(234, 314),
            MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(AMBER).build(),
            Alignment::Right).draw(display).unwrap();
    } else {
        Text::with_alignment("Radio --", Point::new(234, 314),
            MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(GRAY).build(),
            Alignment::Right).draw(display).unwrap();
    }
}

// ===== 单个波段面板 (136px) =====
pub fn draw_band<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    y: i32,
    state: &BandState,
) where D::Error: core::fmt::Debug,
{
    let bg_color = if state.is_tx { TX_BG } else { BG };
    Rectangle::new(Point::new(0, y), Size::new(240, 136))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(bg_color).build())
        .draw(display).unwrap();

    let s9 = MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(GRAY).build();
    let _s9c = MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(CYAN).build();
    let s9a = MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(AMBER).build();
    let s9g = MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(GREEN).build();

    // 行1: 标题行
    let band_style = MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(CYAN).build();
    Text::new(state.label, Point::new(6, y + 12), band_style).draw(display).unwrap();

    if state.is_main {
        let badge_x = 6 + (state.label.len() as i32) * 8 - 1 + 7;
        RoundedRectangle::with_equal_corners(
            Rectangle::new(Point::new(badge_x, y + 1), Size::new(30, 13)),
            Size::new(2, 2),
        ).into_styled(PrimitiveStyleBuilder::new().fill_color(AMBER).build())
            .draw(display).unwrap();
        let main_txt = MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(Rgb565::BLACK).build();
        Text::new("MAIN", Point::new(badge_x + 3, y + 11), main_txt).draw(display).unwrap();
    }

    // 右侧: MT + channel
    let mut title_rx = 234i32;
    let ch_disp = state.channel.as_str();
    Text::with_alignment(ch_disp, Point::new(title_rx, y + 12), s9g, Alignment::Right)
        .draw(display).unwrap();
    title_rx -= (ch_disp.len() as i32) * 6 + 2;

    if state.is_mt {
        Text::new("MT", Point::new(title_rx - 12, y + 12), s9).draw(display).unwrap();
    }

    // 行2: 频率面板
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(4, y + 16), Size::new(232, 40)),
        Size::new(4, 4),
    ).into_styled(PrimitiveStyleBuilder::new()
        .fill_color(PANEL).stroke_color(BORDER).stroke_width(1).build())
        .draw(display).unwrap();

    if !state.is_set && state.display_text.is_empty() {
        if !state.shift.is_empty() {
            Text::new(state.shift.as_str(), Point::new(8, y + 28), s9a).draw(display).unwrap();
        }
        if !state.tone_type.is_empty() {
            let mut tone_buf = [0u8; 16];
            let tone_str = if state.tone_freq.is_empty() {
                state.tone_type.as_str()
            } else {
                let t = state.tone_type.as_bytes();
                let f = state.tone_freq.as_bytes();
                let mut i = 0;
                for &b in t { tone_buf[i] = b; i += 1; }
                tone_buf[i] = b' '; i += 1;
                for &b in f { tone_buf[i] = b; i += 1; }
                core::str::from_utf8(&tone_buf[..i]).unwrap_or("")
            };
            Text::with_alignment(tone_str, Point::new(230, y + 28), s9a, Alignment::Right)
                .draw(display).unwrap();
        }
    }

    // 频率拆分: "438.500" → freq_main, freq_fine 从完整 freq 字符串拆分
    let freq_str = state.freq.as_str();
    // 尝试拆分为主频率（前 7 字符如 "438.500"）和细分
    let (freq_main, freq_fine) = if freq_str.len() > 7 {
        (&freq_str[..7], &freq_str[7..])
    } else {
        (freq_str, "")
    };

    if state.is_set {
        Text::new("SET", Point::new(8, y + 28), s9a).draw(display).unwrap();

        let menu_name_buf = menu_name_from_state(state);
        let menu_name = menu_name_buf.as_str();
        Text::new(menu_name, Point::new(8, y + 48),
            MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(AMBER).build())
            .draw(display).unwrap();

        if state.menu_in_value {
            let menu_value = translate_menu_value(state.menu_text.as_str());
            if !menu_value.is_empty() {
                let value_x = (12 + menu_name.len() as i32 * 9).min(116);
                Text::new(menu_value.as_str(), Point::new(value_x, y + 48),
                    MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build())
                    .draw(display).unwrap();
            }
        }
    } else if !state.display_text.is_empty() && is_frequency_entry_text(state.display_text.as_str()) {
        Text::new(state.mode.as_str(), Point::new(8, y + 48),
            MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build())
            .draw(display).unwrap();

        Text::new(state.display_text.as_str(), Point::new(40, y + 48),
            MonoTextStyleBuilder::new().font(&PROFONT_24_POINT).text_color(AMBER).build())
            .draw(display).unwrap();

        Text::with_alignment("MHz", Point::new(230, y + 48),
            MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build(),
            Alignment::Right).draw(display).unwrap();
    } else if !state.display_text.is_empty() {
        Text::new(state.display_text.as_str(), Point::new(8, y + 48),
            MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(AMBER).build())
            .draw(display).unwrap();
    } else {
        // 正常模式：mode + freq + MHz
        Text::new(state.mode.as_str(), Point::new(8, y + 48),
            MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build())
            .draw(display).unwrap();

        Text::new(freq_main, Point::new(40, y + 48),
            MonoTextStyleBuilder::new().font(&PROFONT_24_POINT).text_color(AMBER).build())
            .draw(display).unwrap();

        if !freq_fine.is_empty() {
            // 去掉开头的点号（如果有）
            let fine = freq_fine.trim_start_matches('.');
            Text::new(fine, Point::new(156, y + 48),
                MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(GRAY).build())
                .draw(display).unwrap();
        }

        Text::with_alignment("MHz", Point::new(230, y + 48),
            MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build(),
            Alignment::Right).draw(display).unwrap();
    }

    // 行3: S表行
    let y3 = y + 64;
    let (ind_label, ind_color) = if state.is_tx { ("TX", RED) } else { ("RX", GREEN) };
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(6, y3), Size::new(26, 16)),
        Size::new(2, 2),
    ).into_styled(PrimitiveStyleBuilder::new().fill_color(ind_color).build())
        .draw(display).unwrap();
    Text::new(ind_label, Point::new(9, y3 + 13),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(Rgb565::BLACK).build())
        .draw(display).unwrap();

    draw_s_meter(display, BAR_X, y3 + 2, state.s_level);

    let s_label = ["S0","S1","S2","S3","S4","S5","S6","S7","S8","S9"]
        .get(state.s_level as usize).copied().unwrap_or("S9");
    Text::new(s_label, Point::new(VAL_X, y3 + 12), s9).draw(display).unwrap();

    if state.power_confirmed {
        draw_cjk_badge(display, 234, y3, power_glyphs(state.power), CYAN);
    }

    // 行4: VOL
    let y4 = y + 88;
    Text::new("VOL", Point::new(6, y4 + 13),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(AMBER).build())
        .draw(display).unwrap();
    draw_bar(display, y4, state.vol_pct(), AMBER);

    let mut badge_rx = 234i32;
    if state.is_lock {
        draw_cjk_badge(display, badge_rx, y4, &[&GLYPH_JIAN, &GLYPH_PAN, &GLYPH_SUO], AMBER);
        badge_rx -= 48;
    }
    if state.is_busy {
        draw_cjk_badge(display, badge_rx, y4, &[&GLYPH_XIN, &GLYPH_DAO, &GLYPH_MANG], RED);
    }

    // 行5: SQL
    let y5 = y + 112;
    Text::new("SQL", Point::new(6, y5 + 13),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build())
        .draw(display).unwrap();
    draw_bar(display, y5, state.sql_pct(), CYAN);

    badge_rx = 234;
    if state.is_skip {
        draw_cjk_badge(display, badge_rx, y5, &[&GLYPH_TIAO, &GLYPH_GUO], CYAN);
        badge_rx -= 34;
    }
    if state.is_mute {
        draw_cjk_badge(display, badge_rx, y5, &[&GLYPH_JING, &GLYPH_YIN], RED);
    }
}

fn is_frequency_entry_text(s: &str) -> bool {
    let mut digits = 0usize;
    for c in s.chars() {
        if c >= '0' && c <= '9' {
            digits += 1;
        } else if c != '.' && c != '-' && c != ' ' {
            return false;
        }
    }
    digits >= 1
}

fn menu_name_from_state(state: &BandState) -> heapless::String<12> {
    if let Some(num) = channel_menu_number(state.channel.as_str()) {
        if num < MENU_NAMES.len() {
            let mut out = heapless::String::new();
            let _ = out.push_str(MENU_NAMES[num]);
            return out;
        }
    }

    let mut out = heapless::String::new();
    let _ = out.push_str(state.menu_text.as_str());
    out
}

fn channel_menu_number(ch: &str) -> Option<usize> {
    let s = ch.strip_prefix("Ch:")?;
    let mut n = 0usize;
    let mut has_digit = false;
    for b in s.bytes() {
        if b >= b'0' && b <= b'9' {
            n = n * 10 + (b - b'0') as usize;
            has_digit = true;
        }
    }
    if has_digit { Some(n) } else { None }
}

fn translate_menu_value(raw: &str) -> heapless::String<12> {
    let mut compact: heapless::String<12> = heapless::String::new();
    for c in raw.chars() {
        if c > ' ' && c != '.' {
            let _ = compact.push(c.to_ascii_uppercase());
        }
    }

    let mapped = match compact.as_str() {
        "25K" => Some("2.5kHz"),
        "5K" | "50K" => Some("5.0kHz"),
        "625K" => Some("6.25kHz"),
        "75K" => Some("7.5kHz"),
        "833K" => Some("8.33kHz"),
        "10K" => Some("10kHz"),
        "125K" => Some("12.5kHz"),
        "15K" | "150K" => Some("15kHz"),
        "250K" => Some("25kHz"),
        "30K" | "300K" => Some("30kHz"),
        "500K" => Some("50kHz"),
        "100K" | "1000K" => Some("100kHz"),
        "ON" | "BEPON" | "APOON" | "MUTEON" => Some("ON"),
        "OFF" | "BEPOFF" | "APOOFF" | "MUTEOFF" => Some("OFF"),
        _ => None,
    };

    let mut out = heapless::String::new();
    if let Some(v) = mapped {
        let _ = out.push_str(v);
    } else {
        let _ = out.push_str(raw);
    }
    out
}

const MENU_NAMES: [&str; 43] = [
    "", "APO", "ARS", "BEEP", "CLK.SFT", "CWID", "CWID W", "DIMMER",
    "DTMF", "DTMF W", "DW", "HYPER", "LOCK", "LOCKT", "MUTE", "NAME",
    "NAR/WID", "OPN.MSG", "PON.MSG", "PTT.ID", "RF SQL", "RPT.MOD", "SCAN",
    "SCN.M", "SHIFT", "SKIP", "SPLIT", "SQL.TYP", "STEP", "TBST", "TONE F",
    "TONE M", "TOT", "TS MUT", "TS SPD", "VFO.BND", "VFO.LNK", "VOX",
    "VOX.D", "VOX.G", "VOX.T", "W/N.DEV", "WX ALT",
];

// ===== 进度条 =====
fn draw_bar<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, y: i32, percent: u32, fill_color: Rgb565,
) where D::Error: core::fmt::Debug,
{
    let bar_h = 10u32;
    Rectangle::new(Point::new(BAR_X, y + 3), Size::new(BAR_W, bar_h))
        .into_styled(PrimitiveStyleBuilder::new().stroke_color(BORDER).stroke_width(1).build())
        .draw(display).unwrap();

    let fill_w = (BAR_W - 4) * percent.min(100) / 100;
    if fill_w > 0 {
        Rectangle::new(Point::new(BAR_X + 2, y + 5), Size::new(fill_w, bar_h - 4))
            .into_styled(PrimitiveStyleBuilder::new().fill_color(fill_color).build())
            .draw(display).unwrap();
    }

    let mut buf = [0u8; 4];
    let pct = fmt_pct(percent, &mut buf);
    Text::new(pct, Point::new(VAL_X, y + 12),
        MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(GRAY).build())
        .draw(display).unwrap();
}

// ===== S 表 =====
fn draw_s_meter<D: DrawTarget<Color = Rgb565>>(display: &mut D, x: i32, y: i32, level: u32)
where D::Error: core::fmt::Debug,
{
    let total_bars = 9u32;
    let bar_w = 10i32;
    let bar_h = 12i32;
    let gap = 2i32;

    for i in 0..total_bars {
        let bx = x + (i as i32) * (bar_w + gap);
        let color = if i < level {
            if i < 5 { GREEN } else if i < 7 { AMBER } else { RED }
        } else {
            Rgb565::new(2, 6, 3)
        };
        Rectangle::new(Point::new(bx, y), Size::new(bar_w as u32, bar_h as u32))
            .into_styled(PrimitiveStyleBuilder::new().fill_color(color).build())
            .draw(display).unwrap();
    }
}

// ===== 水平线 =====
fn hline<D: DrawTarget<Color = Rgb565>>(display: &mut D, y: i32, color: Rgb565)
where D::Error: core::fmt::Debug,
{
    Line::new(Point::new(0, y), Point::new(239, y))
        .into_styled(embedded_graphics::primitives::PrimitiveStyle::with_stroke(color, 1))
        .draw(display).unwrap();
}

// ===== 百分比格式化 =====
fn fmt_pct(val: u32, buf: &mut [u8; 4]) -> &str {
    if val >= 100 {
        "100"
    } else if val >= 10 {
        buf[0] = b'0' + (val / 10) as u8;
        buf[1] = b'0' + (val % 10) as u8;
        core::str::from_utf8(&buf[..2]).unwrap()
    } else {
        buf[0] = b'0' + val as u8;
        core::str::from_utf8(&buf[..1]).unwrap()
    }
}
