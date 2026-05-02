// ===================================================================
// SoftAP 配网网页（暗色模式单页 HTML+CSS+JS embedded）
//
// 嵌入 assets/web_config.html 到二进制 .rodata 段，softap.rs::GET / 路由直接返回。
// 不依赖任何外部 CDN（用户连 SoftAP 没互联网），所有 CSS/JS 内联。
// ===================================================================

pub const HTML: &str = include_str!("../assets/web_config.html");

/// Logo PNG 原始字节（用于 GET /logo.png 和 GET /favicon.ico）
/// 200×166 RGBA PNG，~17.4 KB（PNG 压缩后），浏览器自动缩放到 CSS 指定尺寸
pub const LOGO_PNG: &[u8] = include_bytes!("../assets/logo.png");
