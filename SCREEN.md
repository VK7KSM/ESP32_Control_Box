# ElfRadio Control Box 屏幕 UI 规格文档

> 版本: v4.1 | 更新: 2026-02-23 | 已在实机验证通过

## 1. 屏幕硬件

- ST7789 2.0寸 IPS, 240×320 竖屏, RGB565, SPI 40MHz
- 色彩反转: `ColorInversion::Inverted`（IPS 屏特性）

## 2. 全局配色

| 常量名 | RGB565 值 | 用途 |
|---|---|---|
| BG | BLACK | 背景 |
| AMBER | (31,50,0) | 频率、VOL 标签、MAIN 徽章、±Shft/亚音 |
| CYAN | (0,58,31) | 波段标签、SQL 标签、FM/AM/MHz、版本号、功率徽章底色、跳过徽章底色 |
| GREEN | (4,63,4) | RX 指示块、S 表低段、信道号/VFO |
| RED | (31,10,0) | TX 指示块、S 表高段、信道忙徽章底色、静音徽章底色 |
| WHITE | WHITE | 顶栏标题、中文徽章文字 |
| GRAY | (14,30,14) | S 值、进度条数值、细分频率 |
| BORDER | (6,18,10) | 分隔线、面板边框、进度条边框 |
| PANEL | (1,3,2) | 顶栏/底栏/频率面板填充 |
| TX_BG | (4,2,1) | 发射时波段背景 |

## 3. 全局布局

```
y=0~21     顶栏 (22px, PANEL 底色)
y=22       分隔线 BORDER
y=23~158   波段1 (136px)
y=159      分隔线 BORDER
y=160~295  波段2 (136px)
y=296      分隔线 BORDER
y=297~318  底栏 (22px, PANEL 底色)
```

## 4. 顶栏 (y=0~21)

| 元素 | 字体 | 颜色 | 位置 |
|---|---|---|---|
| "TYT TH-9800" | PROFONT_14 | WHITE | x=6, baseline y=16 |
| "V0.1.0" | PROFONT_9 | CYAN | 右对齐 x=234, baseline y=16 |

对应 TH-9800.md: §1 产品概述

## 5. 波段面板 (136px)

每个波段占 136px，内部布局（y 为波段起始偏移）：

```
y+0~15     行1: 标题行 (16px)
y+16~55    行2: 频率面板 (40px)
y+56~63    间隙 (8px)
y+64~79    行3: S 表行 (16px)
y+80~87    间隙 (8px)
y+88~103   行4: VOL 行 (16px)
y+104~111  间隙 (8px)
y+112~127  行5: SQL 行 (16px)
y+128~135  底部留白 (8px)
```

### 5.1 行1: 标题行

| 元素 | 字体 | 颜色 | 位置 | TH-9800.md 对应 |
|---|---|---|---|---|
| 波段标签 "LEFT"/"RIGHT" | PROFONT_12 | CYAN | x=6, baseline y+12 | §3.1/§3.2 左右波段 |
| MAIN 徽章 (30×13) | PROFONT_9 黑字 | AMBER 底色 | 标签右侧+7px, y+1 | §4.1 #10 MAIN 图标 |
| MT 标记 | PROFONT_9 | GRAY | 右侧区域 | §4.1 #12 Memory Tune |
| ◄ 优先标记 | PROFONT_9 | CYAN | MT 右侧 | §4.1 #2 优先存储 |
| 信道 "VFO"/"Ch:xxx" | PROFONT_9 | GREEN | 右对齐 x=234 | §6.4 VFO/MR 模式, §4.1 #1 信道号 |

### 5.2 行2: 频率面板 (圆角矩形 232×40)

面板边框: BORDER 1px, 填充 PANEL, 圆角 4px

**上行 (baseline y+28):**

| 元素 | 字体 | 颜色 | 位置 | TH-9800.md 对应 |
|---|---|---|---|---|
| 偏移 "+Shft"/"-Shft" | PROFONT_9 | AMBER | x=8 | §4.1 #4/#5 偏移方向, §8.1 中继操作 |
| 亚音 "ENC 88.5"/"DCS 023" | PROFONT_9 | AMBER | 右对齐 x=230 | §4.1 #7/#8/#14 CTCSS/DCS, §8.2 亚音操作 |

**下行 (baseline y+48):**

| 元素 | 字体 | 颜色 | 位置 | TH-9800.md 对应 |
|---|---|---|---|---|
| 调制 "FM"/"AM" | PROFONT_14 | CYAN | x=8 | §4.1 #15 AM 模式, §8.8 AM 模式 |
| 主频率 "438.500" | PROFONT_24 | AMBER | x=40 | §4.3 频率显示 |
| 细分频率 "000" | PROFONT_12 | GRAY | x=156 | §4.3 频率精度到 100Hz |
| 单位 "MHz" | PROFONT_14 | CYAN | 右对齐 x=230 | — |

### 5.3 行3: S 表行

| 元素 | 规格 | 颜色 | 位置 | TH-9800.md 对应 |
|---|---|---|---|---|
| RX/TX 指示块 | 26×16 圆角, PROFONT_14 黑字 | RX=GREEN, TX=RED | x=6, y+64 | §4.1 #9 TX 图标, §6.7/§6.8 收发 |
| S 表 (9 段) | 每段 10×12, 间隔 2px | 0-4=GREEN, 5-6=AMBER, 7-8=RED | x=36, y+66 | §4.2 S 表条形图 |
| S 值 "S0"~"S9" | PROFONT_9 | GRAY | x=146, baseline y+76 | — |
| 功率徽章 | CJK 16×16 徽章 | CYAN 底 WHITE 字 | 右对齐 x=234 | §6.9 功率选择, §14.2 功率映射 |

功率徽章映射:

| TH-9800 功率 | 中文 | 字形 |
|---|---|---|
| HIGH | 高功 | GLYPH_GAO + GLYPH_GONG |
| LOW | 低功 | GLYPH_DI + GLYPH_GONG |

### 5.4 行4: VOL 行

| 元素 | 规格 | 颜色 | 位置 | TH-9800.md 对应 |
|---|---|---|---|---|
| "VOL" | PROFONT_14 | AMBER | x=6, baseline y+101 | §6.2 音量调节 |
| 进度条 | 106×10, 边框 BORDER | AMBER 填充 | x=36, y+91 | — |
| 百分比值 | PROFONT_9 | GRAY | x=146 | — |
| 信道忙徽章 | CJK "信道忙" | RED 底 WHITE 字 | 右对齐 x=234 | §4.1 #11 BUSY 图标 |
| 键盘锁徽章 | CJK "键盘锁" | AMBER 底 WHITE 字 | 信道忙左侧 | §4.1 #19 锁定图标 |

### 5.5 行5: SQL 行

| 元素 | 规格 | 颜色 | 位置 | TH-9800.md 对应 |
|---|---|---|---|---|
| "SQL" | PROFONT_14 | CYAN | x=6, baseline y+125 | §6.3 静噪调节 |
| 进度条 | 106×10, 边框 BORDER | CYAN 填充 | x=36, y+115 | — |
| 百分比值 | PROFONT_9 | GRAY | x=146 | — |
| 跳过徽章 | CJK "跳过" | CYAN 底 WHITE 字 | 右对齐 x=234 | §4.1 #3 SKIP 图标, §8.3 跳过信道 |
| 静音徽章 | CJK "静音" | RED 底 WHITE 字 | 跳过左侧 | §4.1 #13 MUTE 图标 |

## 6. 底栏 (y=297~318)

| 元素 | 字体 | 颜色 | 位置 |
|---|---|---|---|
| 时间 "00:00:00" | PROFONT_14 | CYAN | x=6, baseline y=314 |
| PC 连接 "PC --" | PROFONT_12 | GRAY | x=120, baseline y=314 |
| 电台连接 "Radio --" | PROFONT_12 | GRAY | 右对齐 x=234, baseline y=314 |

## 7. 开机画面

- 全屏黑色背景
- Logo: 200×166 RGB565 位图, 水平居中, y=30
- 标题 "elfRadio Box": PROFONT_24, AMBER, 水平居中
- 版本 "V 0.1.0": PROFONT_14, CYAN, 水平居中
- 显示 1.5 秒后切换到主界面

## 8. 中文显示实现

### 8.1 字体来源

**GNU Unifont 16.0.02** — 开源 16×16 像素完美位图字体。

下载地址: `https://mirrors.ocf.berkeley.edu/gnu/unifont/unifont-16.0.02/unifont-16.0.02.hex.gz`

文件格式 (`.hex`): 每行 `CODEPOINT:DATA`，其中 16×16 汉字 DATA 为 64 位十六进制字符 = 32 字节 = 16 行 × 16 位。

### 8.2 提取方法

从 `.hex` 文件中 grep 对应 Unicode 码点，将 64 位十六进制拆为 16 个 `u16` 值：

```
例: 9AD8:02000100FFFE00000FE008200820...
→ const GLYPH_GAO: [u16; 16] = [0x0200, 0x0100, 0xFFFE, ...]; // 高
```

### 8.3 渲染方式

**必须使用原始 16×16 分辨率，禁止任何缩放。** 缩放会丢失关键笔画（经实机验证，12×12/14×14 均不可用）。

```rust
fn draw_cjk_16(display, x, y, glyph: &[u16; 16], color: Rgb565) {
    for row in 0..16 {
        let bits = glyph[row];
        for col in 0..16 {
            if bits & (0x8000 >> col) != 0 {
                Pixel(Point::new(x + col, y + row), color).draw(display);
            }
        }
    }
}
```

### 8.4 徽章渲染

中文以"徽章"形式显示：圆角矩形底色 + WHITE 文字，右对齐。

- 徽章尺寸: 宽 = n×16+2, 高 = 16px
- 圆角: 2px
- 文字偏移: 底色左边 +1px

### 8.5 字形清单 (14 个)

| 字 | Unicode | 常量名 | 用途 |
|---|---|---|---|
| 高 | U+9AD8 | GLYPH_GAO | 功率: 高功 |
| 功 | U+529F | GLYPH_GONG | 功率: 高功/低功 |
| 中 | U+4E2D | GLYPH_ZHONG | 功率: 中高/中低（预留） |
| 低 | U+4F4E | GLYPH_DI | 功率: 低功 |
| 忙 | U+5FD9 | GLYPH_MANG | 信道忙 |
| 锁 | U+9501 | GLYPH_SUO | 键盘锁 |
| 静 | U+9759 | GLYPH_JING | 静音 |
| 音 | U+97F3 | GLYPH_YIN | 静音 |
| 跳 | U+8DF3 | GLYPH_TIAO | 跳过 |
| 过 | U+8FC7 | GLYPH_GUO | 跳过 |
| 信 | U+4FE1 | GLYPH_XIN | 信道忙 |
| 道 | U+9053 | GLYPH_DAO | 信道忙 |
| 键 | U+952E | GLYPH_JIAN | 键盘锁 |
| 盘 | U+76D8 | GLYPH_PAN | 键盘锁 |

## 9. 字体规格 (ProFont)

| 字体 | 字宽 | 字高 | 基线偏移 | n 字总宽 |
|---|---|---|---|---|
| PROFONT_9 | 6px | 11px | 8 | n×6 |
| PROFONT_12 | 7px | 15px | 11 | n×8−1 |
| PROFONT_14 | 10px | 17px | 13 | n×10 |
| PROFONT_24 | 16px | 29px | 24 | n×16 |

## 10. BandState 数据结构

```rust
struct BandState {
    label: &str,           // "LEFT" / "RIGHT"
    is_main: bool,         // MAIN 徽章
    freq_main: &str,       // "438.500" 主频率
    freq_fine: &str,       // "000" 细分频率
    mode: &str,            // "FM" / "AM"
    power_glyphs: &[&[u16; 16]], // 功率中文字形
    s_level: u32,          // 0~9 信号强度
    vol_pct: u32,          // 0~100 音量百分比
    sql_pct: u32,          // 0~100 静噪百分比
    is_tx: bool,           // 发射状态
    channel: &str,         // "VFO" / "Ch:012"
    tone_type: &str,       // "ENC" / "DCS" / ""
    tone_freq: &str,       // "88.5" / "023" / ""
    shift: &str,           // "+Shft" / "-Shft" / ""
    is_busy: bool,         // 信道忙
    is_skip: bool,         // 跳过
    is_mute: bool,         // 静音
    is_lock: bool,         // 键盘锁
    is_mt: bool,           // Memory Tune
    is_pref: bool,         // 优先信道 ◄
}
```

每个字段对应 TH-9800 MITM 协议需要解析的状态，详见 TH-9800.md §14.1。

## 11. 帧缓冲方案（消除屏幕闪烁）

> 2026-02-23 实机验证通过

### 11.1 问题

`draw_main_ui` / `draw_band` 直接通过 SPI 画到屏幕，"先清后画"导致清空区域在内容画上去之前短暂可见（闪黑）。中文徽章（逐像素 SPI 传输）闪烁尤为严重。

### 11.2 方案

参考 u8g2 的 `clearBuffer → draw → sendBuffer` 模式，自定义 `FrameBuf`（`src/framebuf.rs`）：

- 所有绘制先在 RAM 中完成，再一次性 `fill_contiguous` 刷到屏幕
- 使用 `Vec<Rgb565>` 堆分配，避免 153KB 内联数组爆栈（主栈仅 16KB）
- 覆盖 `fill_contiguous` / `fill_solid` / `clear` 提升大面积填充性能

### 11.3 内存与性能

| 指标 | 值 |
|---|---|
| 帧缓冲大小 | 240 × 320 × 2 = 153,600 bytes (150KB) |
| 分配方式 | `Vec<Rgb565>` 堆分配，零栈压力 |
| ESP32-S3 可用 SRAM | ~350KB（512KB 减 ESP-IDF 运行时） |
| SPI 传输时间 | 153,600 × 8 / 40MHz ≈ 31ms |
| 刷新周期 | 100ms（31ms 传输 + ~5ms RAM 绘制，充裕） |

### 11.4 绘制流程

```
开机画面 → 直接画到屏幕（只画一次，无需帧缓冲）
         ↓ 1.5s
主界面   → draw_main_ui(&mut fb, ...) → flush_fb(&fb, &mut display)
         ↓
主循环   → 有新数据时: draw_main_ui(&mut fb, ...) → flush_fb()
```

`flush_fb` 实现：对屏幕调用 `fill_contiguous` 传入帧缓冲全部像素，SPI 驱动会连续传输 153KB 数据。

### 11.5 为什么不用 `embedded-graphics::Framebuffer`

- `Framebuffer<..., N>` 内部是 `[u8; N]` 内联数组，153KB 会爆栈
- `Box::new(Framebuffer::new())` 也会先在栈上创建再移动，同样爆栈
- 自定义 `FrameBuf` 用 `Vec<Rgb565>` 直接在堆上分配，完全规避栈溢出
