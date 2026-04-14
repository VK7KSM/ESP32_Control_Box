//! ST7789 校准固件 - 在屏幕上绘制参考网格，用于对比模拟器输出
//!
//! 校准图案：四边白色边框 + 50px 参考线 + 四角坐标标注

use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle};
use embedded_graphics::text::Text;
use profont::PROFONT_9_POINT;

use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::spi::{SpiDeviceDriver, SpiDriverConfig, config::Config as SpiConfig};
use esp_idf_svc::hal::units::FromValueType;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("校准固件启动");

    let peripherals = Peripherals::take().expect("无法获取外设");

    // SPI + ST7789 初始化（与主项目完全一致）
    let spi = peripherals.spi2;
    let sclk = peripherals.pins.gpio12;
    let mosi = peripherals.pins.gpio11;
    let cs = peripherals.pins.gpio14;
    let dc = PinDriver::output(peripherals.pins.gpio9).expect("DC 引脚初始化失败");
    let rst = PinDriver::output(peripherals.pins.gpio10).expect("RST 引脚初始化失败");

    let spi_driver = SpiDeviceDriver::new_single(
        spi,
        sclk,
        mosi,
        Option::<esp_idf_svc::hal::gpio::AnyIOPin>::None,
        Some(cs),
        &SpiDriverConfig::default(),
        &SpiConfig::default().baudrate(40.MHz().into()),
    ).expect("SPI 初始化失败");

    let di = display_interface_spi::SPIInterface::new(spi_driver, dc);

    let mut display = mipidsi::Builder::new(mipidsi::models::ST7789, di)
        .reset_pin(rst)
        .display_size(240, 320)
        .invert_colors(mipidsi::options::ColorInversion::Inverted)
        .init(&mut FreeRtos)
        .expect("ST7789 初始化失败");

    log::info!("屏幕初始化完成，开始绘制校准图案");

    draw_calibration(&mut display);

    log::info!("校准图案绘制完成");
    loop { FreeRtos::delay_ms(1000); }
}

fn draw_calibration<D>(display: &mut D)
where
    D: DrawTarget<Color = Rgb565>,
    D::Error: core::fmt::Debug,
{
    // 黑色背景
    display.clear(Rgb565::BLACK).unwrap();

    let white_stroke = PrimitiveStyle::with_stroke(Rgb565::WHITE, 1);
    let gray_stroke = PrimitiveStyle::with_stroke(Rgb565::new(8, 20, 8), 1);
    let text_style = MonoTextStyleBuilder::new()
        .font(&PROFONT_9_POINT)
        .text_color(Rgb565::WHITE)
        .build();

    // ===== 四边 1px 白色边框 =====
    // 上边 y=0
    Line::new(Point::new(0, 0), Point::new(239, 0))
        .into_styled(white_stroke).draw(display).unwrap();
    // 下边 y=319
    Line::new(Point::new(0, 319), Point::new(239, 319))
        .into_styled(white_stroke).draw(display).unwrap();
    // 左边 x=0
    Line::new(Point::new(0, 0), Point::new(0, 319))
        .into_styled(white_stroke).draw(display).unwrap();
    // 右边 x=239
    Line::new(Point::new(239, 0), Point::new(239, 319))
        .into_styled(white_stroke).draw(display).unwrap();

    // ===== 水平参考线 (每50px) =====
    for y_val in [50, 100, 150, 200, 250, 300] {
        Line::new(Point::new(0, y_val), Point::new(239, y_val))
            .into_styled(gray_stroke).draw(display).unwrap();
        // y 坐标标注（左侧）
        let mut buf = [0u8; 3];
        let label = fmt_u16(y_val as u16, &mut buf);
        Text::new(label, Point::new(4, y_val - 2), text_style)
            .draw(display).unwrap();
    }

    // ===== 垂直参考线 (每50px) =====
    for x_val in [50, 100, 150, 200] {
        Line::new(Point::new(x_val, 0), Point::new(x_val, 319))
            .into_styled(gray_stroke).draw(display).unwrap();
        // x 坐标标注（顶部）
        let mut buf = [0u8; 3];
        let label = fmt_u16(x_val as u16, &mut buf);
        Text::new(label, Point::new(x_val + 2, 12), text_style)
            .draw(display).unwrap();
    }

    // ===== 四角坐标标注 =====
    // 左上
    Text::new("0,0", Point::new(4, 12), text_style).draw(display).unwrap();
    // 右上
    Text::new("239,0", Point::new(200, 12), text_style).draw(display).unwrap();
    // 左下
    Text::new("0,319", Point::new(4, 314), text_style).draw(display).unwrap();
    // 右下
    Text::new("239,319", Point::new(186, 314), text_style).draw(display).unwrap();

    // ===== 中心十字 =====
    let cx = 120;
    let cy = 160;
    Line::new(Point::new(cx - 10, cy), Point::new(cx + 10, cy))
        .into_styled(white_stroke).draw(display).unwrap();
    Line::new(Point::new(cx, cy - 10), Point::new(cx, cy + 10))
        .into_styled(white_stroke).draw(display).unwrap();
    Text::new("120,160", Point::new(cx + 4, cy - 4), text_style)
        .draw(display).unwrap();
}

fn fmt_u16(val: u16, buf: &mut [u8; 3]) -> &str {
    if val >= 100 {
        buf[0] = b'0' + (val / 100) as u8;
        buf[1] = b'0' + ((val / 10) % 10) as u8;
        buf[2] = b'0' + (val % 10) as u8;
        core::str::from_utf8(&buf[..3]).unwrap()
    } else if val >= 10 {
        buf[0] = b'0' + (val / 10) as u8;
        buf[1] = b'0' + (val % 10) as u8;
        core::str::from_utf8(&buf[..2]).unwrap()
    } else {
        buf[0] = b'0' + val as u8;
        core::str::from_utf8(&buf[..1]).unwrap()
    }
}
