// ===================================================================
// 帧缓冲：所有绘制先在 RAM 中完成，再一次性刷到屏幕，消除闪烁
// 240×320×2 = 150KB，堆分配避免爆栈
// ===================================================================

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;

const W: usize = 240;
const H: usize = 320;

pub struct FrameBuf {
    pixels: Vec<Rgb565>,
}

impl FrameBuf {
    pub fn new() -> Self {
        Self { pixels: vec![Rgb565::BLACK; W * H] }
    }

    /// 获取像素数据用于 flush
    pub fn pixels(&self) -> &[Rgb565] {
        &self.pixels
    }

    /// RGB565 字节序交换：小端序（embedded-graphics）→ 大端序（ST7789 SPI）
    /// DMA 提交前调用，76800 个 u16 swap @240MHz ≈ 0.3ms
    pub fn swap_bytes(&mut self) {
        let ptr = self.pixels.as_mut_ptr() as *mut u16;
        let len = self.pixels.len();
        unsafe {
            let slice = std::slice::from_raw_parts_mut(ptr, len);
            for v in slice.iter_mut() {
                *v = v.swap_bytes();
            }
        }
    }
}

impl DrawTarget for FrameBuf {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where I: IntoIterator<Item = Pixel<Self::Color>> {
        for Pixel(pos, color) in pixels {
            if pos.x >= 0 && pos.x < W as i32 && pos.y >= 0 && pos.y < H as i32 {
                self.pixels[pos.y as usize * W + pos.x as usize] = color;
            }
        }
        Ok(())
    }

    // 覆盖 fill_contiguous 提升大面积填充性能
    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where I: IntoIterator<Item = Self::Color> {
        let area = area.intersection(&self.bounding_box());
        for (pos, color) in area.points().zip(colors) {
            self.pixels[pos.y as usize * W + pos.x as usize] = color;
        }
        Ok(())
    }

    // 覆盖 fill_solid 提升 clear() 和矩形填充性能
    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&self.bounding_box());
        for pos in area.points() {
            self.pixels[pos.y as usize * W + pos.x as usize] = color;
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.pixels.fill(color);
        Ok(())
    }
}

impl OriginDimensions for FrameBuf {
    fn size(&self) -> Size { Size::new(W as u32, H as u32) }
}
