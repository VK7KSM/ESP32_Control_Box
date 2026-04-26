// ===================================================================
// 帧缓冲：所有绘制先在 RAM 中完成，再一次性刷到屏幕，消除闪烁
// 240×320×2 = 150KB
//
// 强制分配到内部 SRAM（MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA）：
//   - PSRAM 与 WiFi DMA 争用会让 LCD DMA 耗时 300-600ms（理论 30ms），
//     SPI queue 频繁饱和，屏幕大面积区域无法刷新。
//   - 内部 SRAM DMA 直通无争用，30ms 稳定完成。
//   - 单帧缓冲即可（5fps 的状态显示无需双缓冲）。
// ===================================================================

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;

const W: usize = 240;
const H: usize = 320;
const PIXELS: usize = W * H;
const BYTES: usize = PIXELS * 2;

pub struct FrameBuf {
    ptr: *mut Rgb565,
    len: usize,
}

unsafe impl Send for FrameBuf {}
unsafe impl Sync for FrameBuf {}

impl FrameBuf {
    pub fn new() -> Self {
        use esp_idf_svc::sys::*;
        let cap_flags = MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA | MALLOC_CAP_8BIT;
        let raw = unsafe { heap_caps_malloc(BYTES, cap_flags) as *mut Rgb565 };
        assert!(!raw.is_null(), "[FrameBuf] heap_caps_malloc INTERNAL+DMA 失败（150KB）");
        // 初始化为黑色
        unsafe {
            for i in 0..PIXELS {
                *raw.add(i) = Rgb565::BLACK;
            }
        }
        log::info!("[FrameBuf] 已在内部 SRAM 分配 {} 字节", BYTES);
        Self { ptr: raw, len: PIXELS }
    }

    /// 获取像素数据用于 flush
    pub fn pixels(&self) -> &[Rgb565] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn pixels_mut(&mut self) -> &mut [Rgb565] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// RGB565 字节序交换：小端序（embedded-graphics）→ 大端序（ST7789 SPI）
    pub fn swap_bytes(&mut self) {
        let ptr = self.ptr as *mut u16;
        unsafe {
            let slice = std::slice::from_raw_parts_mut(ptr, self.len);
            for v in slice.iter_mut() {
                *v = v.swap_bytes();
            }
        }
    }
}

impl Drop for FrameBuf {
    fn drop(&mut self) {
        unsafe {
            esp_idf_svc::sys::heap_caps_free(self.ptr as *mut core::ffi::c_void);
        }
    }
}

impl DrawTarget for FrameBuf {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where I: IntoIterator<Item = Pixel<Self::Color>> {
        let buf = self.pixels_mut();
        for Pixel(pos, color) in pixels {
            if pos.x >= 0 && pos.x < W as i32 && pos.y >= 0 && pos.y < H as i32 {
                buf[pos.y as usize * W + pos.x as usize] = color;
            }
        }
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where I: IntoIterator<Item = Self::Color> {
        let area = area.intersection(&self.bounding_box());
        let buf = self.pixels_mut();
        for (pos, color) in area.points().zip(colors) {
            buf[pos.y as usize * W + pos.x as usize] = color;
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&self.bounding_box());
        let buf = self.pixels_mut();
        for pos in area.points() {
            buf[pos.y as usize * W + pos.x as usize] = color;
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        let buf = self.pixels_mut();
        buf.fill(color);
        Ok(())
    }
}

impl OriginDimensions for FrameBuf {
    fn size(&self) -> Size { Size::new(W as u32, H as u32) }
}
