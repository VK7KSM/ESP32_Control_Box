# calibrate — 屏幕坐标校准固件

用于验证 ST7789 屏幕坐标系与桌面模拟器（`sim/`）的一致性。在屏幕上绘制参考网格和坐标标注，确保 UI 开发时的像素对齐。

---

## 功能

在 240×320 屏幕上绘制：

- 四边 1px 白色边框
- 每 50px 一条参考线（横/纵）
- 四角坐标标注（`0,0`、`239,0`、`0,319`、`239,319`）
- 屏幕中心十字线

通过对比实机画面与 `sim/sim_output.png`，确认坐标系完全吻合（已验证：无偏移，可视区域完整 240×320）。

---

## 编译与烧录

> ⚠️ **路径长度限制**：此项目源码在 `C:\eh\calibrate\`，但由于 Windows 路径长度限制，**实际编译目录在 `C:\ec\`**。

```bash
# 将源码复制到短路径编译目录
cp C:\eh\calibrate\src\main.rs C:\ec\src\main.rs

# 编译
cd /c/ec && cargo build

# 烧录
espflash flash /c/ec/target/xtensa-esp32s3-espidf/debug/elfradio-calibrate -p COM3
```

---

## 校准结论

实机对比验证通过（2026-02-23）：

- 屏幕可视区域为完整 240×320，无裁切
- mipidsi offset (0, 0) 正确
- `sim/` 模拟器输出与实机坐标系完全一致，可直接信任模拟器预览效果
