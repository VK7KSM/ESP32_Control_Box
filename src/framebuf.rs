// ===================================================================
// 帧缓冲：2-tile + dirty 跟踪
// 240×320 虚拟全屏分 2 块（上半 240×160 / 下半 240×160），每块 75KB
// 内部 SRAM 占用 75KB（DMA-capable），相比之前全屏 150KB 释放 75KB
//
// 释放出的 75KB 用于满足 ESP32-S3 BLE controller (~30KB) + WiFi 内部
// DMA buffer (esf_buf + STATIC_RX 共约 30-40KB) 的并发分配需求，
// 让 BLE+WiFi 共存可行。
//
// 工作流程（main.rs 调用 render_main_ui_tiled / render_splash_tiled）：
//   for tile in 0..NUM_TILES:
//       fb.begin_tile(tile)        // 切换 viewport_y，清空当前 tile 缓冲
//       ui::draw_main_ui(&mut fb, ...)  // ui.rs 不动，按虚拟 240×320 坐标画
//                                       // 超出当前 tile y 范围的像素被 DrawTarget 静默丢弃
//       if fb.is_dirty(tile):       // FNV-1a hash 对比上次 flush，无变化跳过
//           flush_tile_dma(fb, tile)
//
// dirty 跟踪：每 tile 一个 u64 hash（FNV-1a 64bit on 9600 个 u64 chunk，~0.2ms/tile）。
// 当 BandState 单字段变化只触发单 tile DMA，撕裂感几乎不可见。
// 两个 tile 都变化时全屏更新约 30ms（4ms DMA + 5ms CPU draw 各 2 次 + 1ms hash），
// 5fps 显示节流下完全够用。
// ===================================================================

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;

const W: usize = 240;
const H: usize = 320;

/// tile 高度（像素行数）
pub const TILE_H: usize = 160;
/// tile 数量（240×320 / 240×160 = 2 块）
pub const NUM_TILES: usize = 2;

const TILE_PIXELS: usize = W * TILE_H;
const TILE_BYTES: usize = TILE_PIXELS * 2;

pub struct FrameBuf {
    /// 单 tile 像素缓冲（240×160 = 38400 像素 = 75KB），不变
    ptr: *mut Rgb565,
    /// 当前 tile 在虚拟 240×320 中的 y 起点（0 或 160），begin_tile 切换
    viewport_y: i32,
    /// 每 tile 上次 flush 的 hash，用于 dirty 跳过
    /// 初始全 0，第一次 is_dirty() 调用必为 true（确保首次 DMA）
    tile_hash: [u64; NUM_TILES],
}

unsafe impl Send for FrameBuf {}
unsafe impl Sync for FrameBuf {}

impl FrameBuf {
    pub fn new() -> Self {
        use esp_idf_svc::sys::*;
        let cap_flags = MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA | MALLOC_CAP_8BIT;
        let raw = unsafe { heap_caps_malloc(TILE_BYTES, cap_flags) as *mut Rgb565 };
        assert!(!raw.is_null(), "[FrameBuf] heap_caps_malloc INTERNAL+DMA 失败（75KB tile）");
        // 初始化为黑色
        unsafe {
            for i in 0..TILE_PIXELS {
                *raw.add(i) = Rgb565::BLACK;
            }
        }
        ::log::info!("[FrameBuf] 已在内部 SRAM 分配 {} 字节（2-tile 模式，每块 240×160）", TILE_BYTES);
        Self {
            ptr: raw,
            viewport_y: 0,
            tile_hash: [0; NUM_TILES],
        }
    }

    /// 切换到指定 tile 并清空当前 tile 缓冲。tile=0 上半屏，tile=1 下半屏。
    /// 调用后 viewport_y 改变，DrawTarget 的写入会按虚拟 240×320 坐标过滤
    pub fn begin_tile(&mut self, tile: usize) {
        debug_assert!(tile < NUM_TILES, "tile index 越界");
        self.viewport_y = (tile * TILE_H) as i32;
        // 清空当前 tile 缓冲为 0x0000（黑色）
        // 注意：ui::draw_main_ui / draw_splash 自身首句也会 clear()，这里是防御性预清
        unsafe {
            core::ptr::write_bytes(self.ptr as *mut u8, 0, TILE_BYTES);
        }
    }

    /// 计算当前 tile 缓冲的 FNV-1a 64bit hash（按 u64 chunk 处理，~0.2ms）
    /// 注意：必须在 swap_bytes() 之前调用，hash 对比的是 native LE 内容
    fn compute_hash(&self) -> u64 {
        // TILE_PIXELS = 38400, 每 4 个 u16 = 1 个 u64，共 9600 个 u64 word
        let chunks = unsafe {
            std::slice::from_raw_parts(self.ptr as *const u64, TILE_PIXELS / 4)
        };
        let mut h: u64 = 0xcbf29ce484222325;
        for &c in chunks {
            h ^= c;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// 检查当前 tile 是否变化，并更新 hash 记录
    /// 返回 true 表示需要 DMA 刷新；false 表示与上次 flush 内容相同可跳过
    pub fn is_dirty(&mut self, tile: usize) -> bool {
        debug_assert!(tile < NUM_TILES);
        let hash = self.compute_hash();
        if self.tile_hash[tile] != hash {
            self.tile_hash[tile] = hash;
            true
        } else {
            false
        }
    }

    /// 强制标记所有 tile 为脏（开机 / 模式切换 / 外部触发完整重绘时调用）
    pub fn invalidate_all(&mut self) {
        // 用 0xFFFF... 不太可能与真实 hash 冲突
        self.tile_hash = [0xFFFF_FFFF_FFFF_FFFF; NUM_TILES];
    }

    /// 像素数据，给 DMA 提交使用（覆盖单 tile 的 38400 像素 = 75KB）
    pub fn pixels(&self) -> &[Rgb565] {
        unsafe { std::slice::from_raw_parts(self.ptr, TILE_PIXELS) }
    }

    fn pixels_mut(&mut self) -> &mut [Rgb565] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, TILE_PIXELS) }
    }

    /// RGB565 字节序交换：小端序（embedded-graphics）→ 大端序（ST7789 SPI）
    /// 必须在 is_dirty() 之后、DMA 提交之前调用
    pub fn swap_bytes(&mut self) {
        let ptr = self.ptr as *mut u16;
        unsafe {
            let slice = std::slice::from_raw_parts_mut(ptr, TILE_PIXELS);
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
        let vy = self.viewport_y;
        let vh = TILE_H as i32;
        let buf = self.pixels_mut();
        for Pixel(pos, color) in pixels {
            // 过滤 x 越界 + 不在当前 tile y 范围的像素
            if pos.x >= 0 && pos.x < W as i32 && pos.y >= vy && pos.y < vy + vh {
                let local_y = (pos.y - vy) as usize;
                buf[local_y * W + pos.x as usize] = color;
            }
        }
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where I: IntoIterator<Item = Self::Color> {
        let area = area.intersection(&self.bounding_box());
        let vy = self.viewport_y;
        let vh = TILE_H as i32;
        let buf = self.pixels_mut();
        for (pos, color) in area.points().zip(colors) {
            if pos.y >= vy && pos.y < vy + vh {
                let local_y = (pos.y - vy) as usize;
                buf[local_y * W + pos.x as usize] = color;
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&self.bounding_box());
        let vy = self.viewport_y;
        let vh = TILE_H as i32;
        let buf = self.pixels_mut();
        for pos in area.points() {
            if pos.y >= vy && pos.y < vy + vh {
                let local_y = (pos.y - vy) as usize;
                buf[local_y * W + pos.x as usize] = color;
            }
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        // 仅清当前 tile 缓冲（ui::draw_main_ui 首句调用 clear() 时只影响当前 tile）
        let buf = self.pixels_mut();
        buf.fill(color);
        Ok(())
    }
}

impl OriginDimensions for FrameBuf {
    /// 返回完整虚拟尺寸 240×320，让 ui.rs 仍按全屏坐标绘制
    /// DrawTarget 的 viewport 过滤负责把超出当前 tile 的像素静默丢弃
    fn size(&self) -> Size { Size::new(W as u32, H as u32) }
}
