// ===================================================================
// 固件更新模块 — GitHub Releases 下载 + espflash 库烧录
// ===================================================================

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use espflash::{
    connection::{Connection, Port, ResetAfterOperation, ResetBeforeOperation},
    flasher::Flasher,
    target::ProgressCallbacks,
};
use serialport::UsbPortInfo;

const REPO:             &str = "VK7KSM/ESP32_Control_Box";
const FIRMWARE_ASSET:   &str = "elfradio-hwnode.bin";
const APP_FLASH_OFFSET: u32  = 0x10000;  // ESP-IDF 默认 app 分区偏移

/// GitHub Releases 最新版本信息
pub struct ReleaseInfo {
    pub tag:          String,  // e.g. "v0.2.0"
    pub firmware_url: String,  // browser_download_url
}

/// 从 GitHub API 获取最新 Release 信息
pub fn check_latest() -> Result<ReleaseInfo, String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
    let resp = ureq::get(&url)
        .header("User-Agent", "elfradio-box")
        .call()
        .map_err(|e| format!("网络请求失败: {}", e))?;

    let body: serde_json::Value = resp
        .into_body()
        .read_json()
        .map_err(|e| format!("JSON 解析失败: {}", e))?;

    let tag = body["tag_name"]
        .as_str()
        .ok_or("响应中缺少 tag_name 字段")?
        .to_string();

    let assets = body["assets"]
        .as_array()
        .ok_or("响应中缺少 assets 字段")?;

    let firmware_url = assets
        .iter()
        .find(|a| a["name"].as_str() == Some(FIRMWARE_ASSET))
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or_else(|| format!("Release {} 中未找到 {} 资源", tag, FIRMWARE_ASSET))?
        .to_string();

    Ok(ReleaseInfo { tag, firmware_url })
}

/// 下载固件到 {exe_dir}/firmware/elfradio-hwnode.bin
/// progress_cb(已下载字节, 总字节) — 用于显示进度
pub fn download_firmware(
    url:         &str,
    progress_cb: impl Fn(u64, u64),
) -> Result<PathBuf, String> {
    let resp = ureq::get(url)
        .header("User-Agent", "elfradio-box")
        .call()
        .map_err(|e| format!("下载请求失败: {}", e))?;

    let total: u64 = resp
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let save_dir = firmware_dir();
    std::fs::create_dir_all(&save_dir)
        .map_err(|e| format!("创建目录失败: {}", e))?;
    let save_path = save_dir.join(FIRMWARE_ASSET);

    let mut reader = resp.into_body().into_reader();
    let mut file = std::fs::File::create(&save_path)
        .map_err(|e| format!("创建文件失败: {}", e))?;

    let mut buf = [0u8; 65536];  // 64KB 块
    let mut downloaded: u64 = 0;
    loop {
        let n = reader.read(&mut buf)
            .map_err(|e| format!("读取失败: {}", e))?;
        if n == 0 { break; }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入失败: {}", e))?;
        downloaded += n as u64;
        progress_cb(downloaded, total);
    }

    Ok(save_path)
}

/// 扫描串口，找到非 Espressif VID 的 USB 串口 = UART 桥接芯片（CH343/CH340 等）烧录口
/// 判断规则：VID≠0x303A（与 auto_detect_port 互补：通信口=Espressif VID，烧录口=非Espressif VID）
/// 独立于 TinyUSB，无论固件运行何种 USB 模式，UART 桥始终可见
pub fn find_flash_port() -> Option<String> {
    serialport::available_ports().ok()?.into_iter().find_map(|p| {
        if let serialport::SerialPortType::UsbPort(usb) = &p.port_type {
            if usb.vid != 0x303A {
                return Some(p.port_name);
            }
        }
        None
    })
}

/// 根据端口名获取 UsbPortInfo（供 espflash Connection 使用）
fn find_port_info(port_name: &str) -> Option<UsbPortInfo> {
    serialport::available_ports().ok()?.into_iter().find_map(|p| {
        if p.port_name == port_name {
            if let serialport::SerialPortType::UsbPort(usb) = p.port_type {
                return Some(usb);
            }
        }
        None
    })
}

/// 烧录进度回调适配器（将 espflash ProgressCallbacks 转换为用户进度闭包）
struct FlashProgress<F: FnMut(usize, usize)> {
    cb:    F,
    total: usize,
}

impl<F: FnMut(usize, usize)> ProgressCallbacks for FlashProgress<F> {
    fn init(&mut self, _addr: u32, total: usize) { self.total = total; }
    fn update(&mut self, current: usize)         { (self.cb)(current, self.total); }
    fn verifying(&mut self)                       {}
    fn finish(&mut self, _skipped: bool)          {}
}

/// 使用内置 espflash 库烧录固件 .bin 到 ESP32
/// bin_path:    本地 elfradio-hwnode.bin 路径
/// port_name:   JTAG 串口名（如 "COM9"），由 find_jtag_port() 自动检测
/// progress_cb: 进度回调 (current_bytes, total_bytes)
pub fn flash_firmware(
    bin_path:    &Path,
    port_name:   &str,
    progress_cb: impl FnMut(usize, usize),
) -> Result<(), String> {
    // 1. 获取端口 UsbPortInfo（espflash Connection 需要）
    let port_info = find_port_info(port_name)
        .ok_or_else(|| format!("未找到串口 {} 的 USB 信息", port_name))?;

    // 2. 用 open_native() 打开串口（返回平台原生类型，Windows = COMPort）
    //    espflash 要求 115200 baud 初始连接
    let serial: Port = serialport::new(port_name, 115_200)
        .timeout(std::time::Duration::from_secs(3))
        .open_native()
        .map_err(|e| format!("打开串口 {} 失败: {}", port_name, e))?;

    // 3. 构建 espflash Connection
    //    - PID=0x1001 时 espflash 内部自动使用 UsbJtagSerialReset 复位策略
    //    - DefaultReset = 烧录后重启到正常运行模式
    let connection = Connection::new(
        serial,
        port_info,
        ResetAfterOperation::HardReset,
        ResetBeforeOperation::DefaultReset,
        115_200,
    );

    // 4. 连接 ESP32 bootloader（发复位序列，握手）
    //    use_stub=true 与标准 `espflash flash` 行为一致（加载 stub 提升速度）
    let mut flasher = Flasher::connect(
        connection,
        true,   // use_stub
        false,  // verify（校验会使总时间翻倍，此处不启用）
        false,  // skip
        None,   // chip（自动检测）
        None,   // baud（连接后 espflash 自动提速）
    )
    .map_err(|e| format!("连接 ESP32 失败: {}", e))?;

    // 5. 读取固件 bin
    let bin_data = std::fs::read(bin_path)
        .map_err(|e| format!("读取固件文件失败: {}", e))?;

    // 6. 烧录 app 分区镜像到偏移 0x10000（ESP-IDF 默认 app 分区起始地址）
    let mut progress = FlashProgress { cb: progress_cb, total: bin_data.len() };
    flasher
        .write_bin_to_flash(APP_FLASH_OFFSET, &bin_data, &mut progress)
        .map_err(|e| format!("烧录失败: {}", e))?;

    Ok(())
}

/// 固件存放目录（与可执行文件同级的 firmware/ 子目录）
fn firmware_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("firmware")
}
