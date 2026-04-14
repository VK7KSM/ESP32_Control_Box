//! ElfRadio UI 桌面预览模拟器 v4.1
//!
//! 运行: cargo run（在 C:\eh\sim\ 目录下）
//! 输出: sim_output.png

use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{
    Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, RoundedRectangle,
};
use embedded_graphics::text::{Alignment, Text};
use embedded_graphics_simulator::{OutputSettingsBuilder, SimulatorDisplay};
use profont::{PROFONT_12_POINT, PROFONT_14_POINT, PROFONT_24_POINT, PROFONT_9_POINT};

// ===== 配色 =====
const BG: Rgb565 = Rgb565::BLACK;
const AMBER: Rgb565 = Rgb565::new(31, 50, 0);
const CYAN: Rgb565 = Rgb565::new(0, 58, 31);
const GREEN: Rgb565 = Rgb565::new(4, 63, 4);
const RED: Rgb565 = Rgb565::new(31, 10, 0);
const WHITE: Rgb565 = Rgb565::WHITE;
const GRAY: Rgb565 = Rgb565::new(14, 30, 14);
const BORDER: Rgb565 = Rgb565::new(6, 18, 10);
const PANEL: Rgb565 = Rgb565::new(1, 3, 2);
const TX_BG: Rgb565 = Rgb565::new(4, 2, 1);

// ===== 布局常量 =====
const BAR_X: i32 = 36;
const BAR_W: u32 = 106;
const VAL_X: i32 = 146;

// ===== Unifont 16×16 位图（从 GNU Unifont 16.0.02 提取，运行时缩放至 12×12） =====
const GLYPH_GAO: [u16; 16] = [0x0200, 0x0100, 0xFFFE, 0x0000, 0x0FE0, 0x0820, 0x0820, 0x0FE0, 0x0000, 0x7FFC, 0x4004, 0x4FE4, 0x4824, 0x4824, 0x4FE4, 0x400C]; // 高
const GLYPH_GONG: [u16; 16] = [0x0040, 0x0040, 0x0040, 0xFE40, 0x11FC, 0x1044, 0x1044, 0x1044, 0x1044, 0x1084, 0x1084, 0x1E84, 0xF104, 0x4104, 0x0228, 0x0410]; // 功
const GLYPH_ZHONG: [u16; 16] = [0x0100, 0x0100, 0x0100, 0x0100, 0x3FF8, 0x2108, 0x2108, 0x2108, 0x2108, 0x2108, 0x3FF8, 0x2108, 0x0100, 0x0100, 0x0100, 0x0100]; // 中
const GLYPH_DI: [u16; 16] = [0x0808, 0x083C, 0x0BE0, 0x1220, 0x1220, 0x3220, 0x3220, 0x53FE, 0x9220, 0x1210, 0x1210, 0x1212, 0x120A, 0x128A, 0x1326, 0x1212]; // 低
const GLYPH_MANG: [u16; 16] = [0x1020, 0x1010, 0x1010, 0x1000, 0x1BFE, 0x5480, 0x5080, 0x5080, 0x9080, 0x1080, 0x1080, 0x1080, 0x1080, 0x1080, 0x10FE, 0x1000]; // 忙
const GLYPH_SUO: [u16; 16] = [0x1020, 0x1124, 0x3CA4, 0x20A8, 0x4020, 0xBDFC, 0x1104, 0x1124, 0xFD24, 0x1124, 0x1124, 0x1124, 0x1450, 0x1888, 0x1104, 0x0202]; // 锁
const GLYPH_JING: [u16; 16] = [0x1040, 0x1040, 0xFE78, 0x1088, 0x7C10, 0x11FC, 0xFE24, 0x0024, 0x7DFE, 0x4424, 0x7C24, 0x45FC, 0x7C24, 0x4420, 0x54A0, 0x4840]; // 静
const GLYPH_YIN: [u16; 16] = [0x0200, 0x0100, 0x3FF8, 0x0000, 0x0820, 0x0440, 0xFFFE, 0x0000, 0x1FF0, 0x1010, 0x1010, 0x1FF0, 0x1010, 0x1010, 0x1FF0, 0x1010]; // 音
const GLYPH_TIAO: [u16; 16] = [0x0050, 0x7C50, 0x4450, 0x4552, 0x44D4, 0x7C58, 0x1050, 0x1058, 0x10D4, 0x5D52, 0x5050, 0x5050, 0x5092, 0x5C92, 0xE112, 0x020E]; // 跳
const GLYPH_GUO: [u16; 16] = [0x0010, 0x2010, 0x1010, 0x17FE, 0x0010, 0x0010, 0xF210, 0x1110, 0x1110, 0x1010, 0x1010, 0x1050, 0x1020, 0x2800, 0x47FE, 0x0000]; // 过
const GLYPH_XIN: [u16; 16] = [0x0840, 0x0820, 0x0BFE, 0x1000, 0x1000, 0x31FC, 0x3000, 0x5000, 0x91FC, 0x1000, 0x1000, 0x11FC, 0x1104, 0x1104, 0x11FC, 0x1104]; // 信
const GLYPH_DAO: [u16; 16] = [0x0208, 0x2110, 0x1000, 0x17FC, 0x0080, 0x03F8, 0xF208, 0x13F8, 0x1208, 0x13F8, 0x1208, 0x13F8, 0x1208, 0x2800, 0x47FE, 0x0000]; // 道
const GLYPH_JIAN: [u16; 16] = [0x2010, 0x2010, 0x3B7C, 0x2114, 0x41FE, 0x7A14, 0xA27C, 0x2710, 0xF97C, 0x2510, 0x25FE, 0x2210, 0x2A10, 0x3500, 0x28FE, 0x0000]; // 键
const GLYPH_PAN: [u16; 16] = [0x0200, 0x0400, 0x1FF0, 0x1110, 0x1090, 0xFFFE, 0x1010, 0x1210, 0x2150, 0x4020, 0x3FF8, 0x2448, 0x2448, 0x2448, 0xFFFE, 0x0000]; // 盘

/// 绘制完整 16×16 CJK 字符（不缩放，无损）
fn draw_cjk_16(display: &mut SimulatorDisplay<Rgb565>, x: i32, y: i32, glyph: &[u16; 16], color: Rgb565) {
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

/// 绘制 CJK 徽章（圆角底色 + 16×16 完整文字，右对齐，16px 高填满行）
fn draw_cjk_badge(
    display: &mut SimulatorDisplay<Rgb565>,
    right_x: i32, y: i32,
    glyphs: &[&[u16; 16]], bg: Rgb565,
) {
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

struct BandState {
    label: &'static str,
    is_main: bool,
    freq_main: &'static str,
    freq_fine: &'static str,
    mode: &'static str,
    power_glyphs: &'static [&'static [u16; 16]],
    s_level: u32,
    vol_pct: u32,
    sql_pct: u32,
    is_tx: bool,
    channel: &'static str,
    tone_type: &'static str,
    tone_freq: &'static str,
    shift: &'static str,
    is_busy: bool,
    is_skip: bool,
    is_mute: bool,
    is_lock: bool,
    is_mt: bool,
    is_pref: bool,
}

fn main() {
    let mut display: SimulatorDisplay<Rgb565> = SimulatorDisplay::new(Size::new(240, 320));
    draw_main_ui(&mut display);

    let output_settings = OutputSettingsBuilder::new().scale(1).build();
    display
        .to_rgb_output_image(&output_settings)
        .save_png("sim_output.png")
        .expect("无法保存 sim_output.png");
    println!("已生成 sim_output.png (240x320)");
}

fn draw_main_ui(display: &mut SimulatorDisplay<Rgb565>) {
    display.clear(BG).unwrap();

    // ---- 顶栏 (22px: y=0~21) ----
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

    // ---- 波段1 (y=23, 136px) ----
    let left_state = BandState {
        label: "LEFT", is_main: true,
        freq_main: "438.500", freq_fine: "000", mode: "FM",
        power_glyphs: &[&GLYPH_GAO, &GLYPH_GONG],
        s_level: 7, vol_pct: 75, sql_pct: 40,
        is_tx: false, channel: "VFO",
        tone_type: "ENC", tone_freq: "88.5",
        shift: "+Shft",
        is_busy: true, is_skip: false, is_mute: false,
        is_lock: false, is_mt: false, is_pref: false,
    };
    draw_band(display, 23, &left_state);
    hline(display, 159, BORDER);

    // ---- 波段2 (y=160, 136px) ----
    let right_state = BandState {
        label: "RIGHT", is_main: false,
        freq_main: "145.350", freq_fine: "000", mode: "AM",
        power_glyphs: &[&GLYPH_DI, &GLYPH_GONG],
        s_level: 3, vol_pct: 60, sql_pct: 30,
        is_tx: false, channel: "Ch:012",
        tone_type: "DCS", tone_freq: "023",
        shift: "-Shft",
        is_busy: false, is_skip: true, is_mute: true,
        is_lock: true, is_mt: true, is_pref: true,
    };
    draw_band(display, 160, &right_state);

    // ---- 底栏 (y=296 线, y=297~318 = 22px) ----
    hline(display, 296, BORDER);
    Rectangle::new(Point::new(0, 297), Size::new(240, 22))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(PANEL).build())
        .draw(display).unwrap();

    Text::new("00:00:00", Point::new(6, 314),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build())
        .draw(display).unwrap();
    let status_style = MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(GRAY).build();
    Text::new("PC --", Point::new(120, 314), status_style).draw(display).unwrap();
    Text::with_alignment("Radio --", Point::new(234, 314), status_style, Alignment::Right)
        .draw(display).unwrap();
}

fn draw_band(display: &mut SimulatorDisplay<Rgb565>, y: i32, state: &BandState) {
    if state.is_tx {
        Rectangle::new(Point::new(0, y), Size::new(240, 136))
            .into_styled(PrimitiveStyleBuilder::new().fill_color(TX_BG).build())
            .draw(display).unwrap();
    }

    let s9 = MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(GRAY).build();
    let s9c = MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(CYAN).build();
    let s9a = MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(AMBER).build();
    let s9g = MonoTextStyleBuilder::new().font(&PROFONT_9_POINT).text_color(GREEN).build();

    // ===== 行1: 标题行 (y+0 ~ y+15, 16px) =====
    let band_style = MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(CYAN).build();
    Text::new(state.label, Point::new(6, y + 12), band_style).draw(display).unwrap();

    // MAIN 徽章 (30×13)
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

    // 右侧: MT + ◄ + channel (GREEN 亮色)
    let mut title_rx = 234i32;
    Text::with_alignment(state.channel, Point::new(title_rx, y + 12), s9g, Alignment::Right)
        .draw(display).unwrap();
    title_rx -= (state.channel.len() as i32) * 6 + 2;

    if state.is_pref {
        Text::new("<", Point::new(title_rx - 6, y + 12), s9c).draw(display).unwrap();
        title_rx -= 8;
    }
    if state.is_mt {
        Text::new("MT", Point::new(title_rx - 12, y + 12), s9).draw(display).unwrap();
    }

    // ===== 行2: 频率面板 (y+16 ~ y+55, 40px) =====
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(4, y + 16), Size::new(232, 40)),
        Size::new(4, 4),
    ).into_styled(PrimitiveStyleBuilder::new()
        .fill_color(PANEL).stroke_color(BORDER).stroke_width(1).build())
        .draw(display).unwrap();

    // 面板上行: 左侧偏移 + 右侧亚音 (PROFONT_9, baseline y+25, AMBER 亮色)
    if !state.shift.is_empty() {
        Text::new(state.shift, Point::new(8, y + 28), s9a).draw(display).unwrap();
    }
    if !state.tone_type.is_empty() {
        let tone_str: String = if state.tone_freq.is_empty() {
            state.tone_type.to_string()
        } else {
            format!("{} {}", state.tone_type, state.tone_freq)
        };
        Text::with_alignment(&tone_str, Point::new(230, y + 28), s9a, Alignment::Right)
            .draw(display).unwrap();
    }

    // 面板下行: FM/AM (PROFONT_14) + 频率 (PROFONT_24) + 细分 + MHz
    // FM/AM x=8, PROFONT_14, baseline y+48, CYAN
    Text::new(state.mode, Point::new(8, y + 48),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build())
        .draw(display).unwrap();

    // 主频率 x=40, PROFONT_24, AMBER
    Text::new(state.freq_main, Point::new(40, y + 48),
        MonoTextStyleBuilder::new().font(&PROFONT_24_POINT).text_color(AMBER).build())
        .draw(display).unwrap();

    // 细分频率 "000" (无小数点), PROFONT_12, GRAY, x=156
    Text::new(state.freq_fine, Point::new(156, y + 48),
        MonoTextStyleBuilder::new().font(&PROFONT_12_POINT).text_color(GRAY).build())
        .draw(display).unwrap();

    // MHz, PROFONT_14, CYAN, 右对齐 x=230
    Text::with_alignment("MHz", Point::new(230, y + 48),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build(),
        Alignment::Right).draw(display).unwrap();

    // ===== 行3: S表行 (y+64 ~ y+79, 16px) =====
    let y3 = y + 64;

    // RX/TX 指示块 (26×16, PROFONT_14)
    let (ind_label, ind_color) = if state.is_tx { ("TX", RED) } else { ("RX", GREEN) };
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(6, y3), Size::new(26, 16)),
        Size::new(2, 2),
    ).into_styled(PrimitiveStyleBuilder::new().fill_color(ind_color).build())
        .draw(display).unwrap();
    Text::new(ind_label, Point::new(9, y3 + 13),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(Rgb565::BLACK).build())
        .draw(display).unwrap();

    // S 表
    draw_s_meter(display, BAR_X, y3 + 2, state.s_level);

    // S 值
    let s_label = ["S0","S1","S2","S3","S4","S5","S6","S7","S8","S9"]
        .get(state.s_level as usize).copied().unwrap_or("S9");
    Text::new(s_label, Point::new(VAL_X, y3 + 12), s9).draw(display).unwrap();

    // 功率徽章 (右对齐 x=234, CYAN 底色, 16px 高填满行)
    draw_cjk_badge(display, 234, y3, state.power_glyphs, CYAN);

    // ===== 行4: VOL (y+88 ~ y+103, 16px) =====
    let y4 = y + 88;
    Text::new("VOL", Point::new(6, y4 + 13),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(AMBER).build())
        .draw(display).unwrap();
    draw_bar(display, y4, state.vol_pct, AMBER);

    // VOL 右侧徽章: 信道忙 + 键盘锁
    let mut badge_rx = 234i32;
    if state.is_lock {
        draw_cjk_badge(display, badge_rx, y4, &[&GLYPH_JIAN, &GLYPH_PAN, &GLYPH_SUO], AMBER);
        badge_rx -= 48;
    }
    if state.is_busy {
        draw_cjk_badge(display, badge_rx, y4, &[&GLYPH_XIN, &GLYPH_DAO, &GLYPH_MANG], RED);
    }

    // ===== 行5: SQL (y+112 ~ y+127, 16px) =====
    let y5 = y + 112;
    Text::new("SQL", Point::new(6, y5 + 13),
        MonoTextStyleBuilder::new().font(&PROFONT_14_POINT).text_color(CYAN).build())
        .draw(display).unwrap();
    draw_bar(display, y5, state.sql_pct, CYAN);

    // SQL 右侧徽章: 静音 + 跳过
    badge_rx = 234;
    if state.is_skip {
        draw_cjk_badge(display, badge_rx, y5, &[&GLYPH_TIAO, &GLYPH_GUO], CYAN);
        badge_rx -= 34;
    }
    if state.is_mute {
        draw_cjk_badge(display, badge_rx, y5, &[&GLYPH_JING, &GLYPH_YIN], RED);
    }
}

fn draw_bar(
    display: &mut SimulatorDisplay<Rgb565>,
    y: i32, percent: u32, fill_color: Rgb565,
) {
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

fn draw_s_meter(display: &mut SimulatorDisplay<Rgb565>, x: i32, y: i32, level: u32) {
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

fn hline(display: &mut SimulatorDisplay<Rgb565>, y: i32, color: Rgb565) {
    Line::new(Point::new(0, y), Point::new(239, y))
        .into_styled(PrimitiveStyle::with_stroke(color, 1))
        .draw(display).unwrap();
}

fn fmt_pct(val: u32, buf: &mut [u8; 4]) -> &str {
    if val >= 100 { "100" }
    else if val >= 10 {
        buf[0] = b'0' + (val / 10) as u8;
        buf[1] = b'0' + (val % 10) as u8;
        core::str::from_utf8(&buf[..2]).unwrap()
    } else {
        buf[0] = b'0' + val as u8;
        core::str::from_utf8(&buf[..1]).unwrap()
    }
}
