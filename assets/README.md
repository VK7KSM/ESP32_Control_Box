# assets — 固件静态资源

存放在编译时嵌入 ESP32 固件的图像资源。

---

## 文件说明

| 文件 | 用途 |
|---|---|
| `logo.png` | 开机画面 Logo，200×166 像素，RGB/RGBA PNG |

---

## 编译时处理流程

`build.rs` 在编译期自动将 `logo.png` 转换为 **RGB565 原始数据**并嵌入固件：

```
logo.png (PNG, ~66KB)
  → build.rs 解码为 RGBA 像素
  → 每像素转换为 RGB565 大端序
  → 输出 logo_rgb565.raw（包含宽高头）
  → 通过 include_bytes! 嵌入 .rodata 段（实际占用 ~66KB flash）
```

固件启动后，在 ST7789 屏幕上渲染 1.5 秒开机画面，随后切换到主界面。

---

## 替换 Logo

直接替换 `logo.png` 文件即可，`build.rs` 支持 RGB 和 RGBA 两种格式。  
建议分辨率不超过 240×320（屏幕全屏），文件大小影响 `.rodata` 占用。
