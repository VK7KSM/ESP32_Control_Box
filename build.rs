/// 把 PNG 文件转换为 RGB565 little-endian 原始字节数组
///
/// alpha 通道处理（二值化阈值化）：
///   - a < 128 → 视为透明，填 `background_rgb565`（与目标显示位的底层背景同色，视觉透明）
///   - a >= 128 → 不透明，直接用原 RGB 值（不与背景混合，保持饱和锐利感，避免半透明渐变模糊）
///
/// 16×16 小尺寸图标二值化是业界像素艺术原则：去除矢量图渲染的抗锯齿过渡像素，提升锐利度。
///
/// `background_rgb565` 由调用方按目标显示位的底层背景色指定：
///   - logo（开机画面，BG=BLACK）：传 0x0000
///   - 顶栏图标（PANEL = Rgb565::new(1,3,2) = 0x0862）：传 0x0862
///
/// 输出三个文件到 OUT_DIR：
///   - {out_name}.raw    — RGB565 LE 字节数组
///   - {out_name}_w.txt  — 宽度（十进制字符串）
///   - {out_name}_h.txt  — 高度（十进制字符串）
fn convert_png_to_raw(in_path: &str, out_name: &str, background_rgb565: u16) {
    let path = std::path::Path::new(in_path);
    if !path.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", in_path);

    let file = std::fs::File::open(path).expect("无法打开 PNG");
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().expect("无法读取 PNG");
    let info = reader.info().clone();
    let width = info.width;
    let height = info.height;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).expect("无法解码 PNG");
    let data = &buf[..frame.buffer_size()];

    let bpp: usize = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        _ => panic!("不支持的 PNG 颜色类型: {:?}", info.color_type),
    };

    let mut raw = Vec::with_capacity((width * height * 2) as usize);
    for i in 0..(width * height) as usize {
        let off = i * bpp;
        let (r, g, b, a) = match info.color_type {
            png::ColorType::Rgba => (data[off], data[off + 1], data[off + 2], data[off + 3]),
            png::ColorType::Rgb => (data[off], data[off + 1], data[off + 2], 255),
            png::ColorType::GrayscaleAlpha => (data[off], data[off], data[off], data[off + 1]),
            _ => (data[off], data[off], data[off], 255),
        };

        // alpha 二值化：a<128 → 背景色（视觉透明）；a>=128 → 原 RGB（实色锐利）
        let rgb565: u16 = if a < 128 {
            background_rgb565
        } else {
            let r5 = (r >> 3) as u16;
            let g6 = (g >> 2) as u16;
            let b5 = (b >> 3) as u16;
            (r5 << 11) | (g6 << 5) | b5
        };
        raw.extend_from_slice(&rgb565.to_le_bytes());
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out = std::path::Path::new(&out_dir);
    std::fs::write(out.join(format!("{}.raw", out_name)), &raw)
        .expect("无法写入 raw 文件");
    std::fs::write(out.join(format!("{}_w.txt", out_name)), width.to_string()).unwrap();
    std::fs::write(out.join(format!("{}_h.txt", out_name)), height.to_string()).unwrap();

    println!("cargo:warning=Converted: {} ({}x{} -> {} bytes RGB565, bg=0x{:04X})",
        in_path, width, height, raw.len(), background_rgb565);
}

fn main() {
    embuild::espidf::sysenv::output();

    // 开机画面 logo（splash 背景 BG=BLACK，背景色 0x0000）
    convert_png_to_raw("assets/logo.png", "logo", 0x0000);

    // 顶栏 WiFi / 蓝牙 状态图标（16×16）
    // 顶栏背景 PANEL = Rgb565::new(1,3,2) = (1<<11)|(3<<5)|2 = 0x0862（暗青绿色）
    // 透明像素填 PANEL 色 → 与顶栏背景无缝融合，消除"黑色方框"
    const PANEL_RGB565: u16 = 0x0862;
    convert_png_to_raw("assets/wifi-blue.png",        "wifi_blue",        PANEL_RGB565);
    convert_png_to_raw("assets/wifi-orange.png",      "wifi_orange",      PANEL_RGB565);
    convert_png_to_raw("assets/bluetooth-blue.png",   "bluetooth_blue",   PANEL_RGB565);
    convert_png_to_raw("assets/bluetooth-orange.png", "bluetooth_orange", PANEL_RGB565);
}
