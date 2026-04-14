fn main() {
    embuild::espidf::sysenv::output();

    // ===== 编译时转换 logo.png → RGB565 原始数据 =====
    let logo_path = std::path::Path::new("assets/logo.png");
    if logo_path.exists() {
        println!("cargo:rerun-if-changed=assets/logo.png");

        let file = std::fs::File::open(logo_path).expect("无法打开 logo.png");
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

            // 透明像素 → 黑色背景 (0x0000)
            let rgb565: u16 = if a < 64 {
                0x0000
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
        std::fs::write(out.join("logo.raw"), &raw).expect("无法写入 logo.raw");
        std::fs::write(out.join("logo_w.txt"), width.to_string()).unwrap();
        std::fs::write(out.join("logo_h.txt"), height.to_string()).unwrap();

        println!("cargo:warning=Logo converted: {}x{} -> {} bytes RGB565", width, height, raw.len());
    }
}
