// ===================================================================
// ElfRadio Control Box 固件
// 主入口：模块声明 + 硬件初始化 + 线程 spawn + UI 主循环
//
// 显示驱动：ESP-IDF esp_lcd C API + SPI DMA 异步传输
// 背光控制：LEDC PWM (GPIO 13)
// ===================================================================

mod framebuf;
mod state;
mod protocol;
mod uart;
mod ui;
mod pc_comm;
mod macro_engine;
mod wifi;
mod discovery;
mod pc_tcp;
mod rigctld;
mod ble;
mod button;
mod nvs_cfg;
mod softap;
mod dns_server;
mod web_config;
mod timesync;

// 必须显式 extern：esp_idf_svc::sys 通过 wildcard 引入了一个叫 `log` 的 struct，
// 与 `log` crate 冲突。extern crate log 把 log crate 强制放到 root namespace，
// 让 `::log::info!` 等宏正确解析。
extern crate log;

use esp_idf_svc::hal::prelude::*;
use esp_idf_svc::hal::rmt::{config::TransmitConfig, FixedLengthSignal, PinState, Pulse, TxRmtDriver};
use esp_idf_svc::hal::ledc::{LedcDriver, LedcTimerDriver, config::TimerConfig};

use esp_idf_svc::sys::*;

// ===== ESP-IDF LCD DMA 面板句柄（全局，供 flush 使用）=====
static mut LCD_PANEL: esp_lcd_panel_handle_t = std::ptr::null_mut();

// ===== LCD DMA 完成同步信号量 =====
// 2-tile framebuf 共用同一个 75KB buffer，必须等上一帧 DMA 读完才能改写下一帧。
// ISR 回调（on_color_trans_done）在 DMA 完成时 give，render_tiled 在 begin_tile
// 之前 take（首次由 init 后的预 give 通过）。这样 CPU 画下一 tile 与 DMA 传上一 tile
// 并行，但 buffer 不会被同时读写——避免之前"tile 0 中下部内容缺失"的 bug。
// dirty=false 跳过 DMA 时由 render_tiled 手动 give 维持信号量平衡。
static mut LCD_DONE_SEM: QueueHandle_t = std::ptr::null_mut();

/// LCD DMA 完成 ISR 回调（运行在 ISR 上下文，禁止阻塞）
unsafe extern "C" fn on_lcd_color_trans_done(
    _panel_io: esp_lcd_panel_io_handle_t,
    _edata: *mut esp_lcd_panel_io_event_data_t,
    _user_ctx: *mut std::ffi::c_void,
) -> bool {
    let mut high_task_woken: BaseType_t = 0;
    // queueQUEUE_TYPE_BINARY_SEMAPHORE 的 give = xQueueGiveFromISR
    xQueueGiveFromISR(LCD_DONE_SEM, &mut high_task_woken);
    // 返回 true → ESP-LCD 内部会调 portYIELD_FROM_ISR 让出 CPU 给被唤醒的高优先级任务
    high_task_woken != 0
}

/// 双 tile 循环 + dirty 跟踪 + DMA 同步的 helper
///
/// 信号量语义 = "buffer 当前空闲（可修改）"：
///   - init 后预 give 1 次 → 空闲
///   - 循环顶部 take → 占用
///   - 提交 DMA 后 → ISR 在 DMA 完成时 give → 空闲
///   - dirty=false 跳过 DMA → 立即手动 give 维持平衡 → 空闲
///
/// 关键：xQueueSemaphoreTake 在 begin_tile 之前——保证 buffer 不再被 DMA 读取再 memset
/// 这样 CPU 画下一 tile 与 DMA 传当前 tile 并行（max(5ms CPU, 15ms DMA) per tile）
/// 但 buffer 不会被同时读写，避免之前 tile 0 内容缺失的 bug
///
/// tile=0：刷新 (0, 0) - (240, 160) 上半屏
/// tile=1：刷新 (0, 160) - (240, 320) 下半屏
fn render_tiled<F: FnMut(&mut framebuf::FrameBuf)>(fb: &mut framebuf::FrameBuf, mut draw: F) {
    for tile in 0..framebuf::NUM_TILES {
        unsafe {
            // 等上一次 DMA 完成（首次由 init 后的预 give 立即通过）
            // 此处 take 之后 buffer 必空闲（DMA 不再读取）
            // portMAX_DELAY = TickType_t(u32) 的最大值 = 0xFFFFFFFF（无超时阻塞等待）
            xQueueSemaphoreTake(LCD_DONE_SEM, u32::MAX);
        }
        fb.begin_tile(tile);
        draw(fb);
        if fb.is_dirty(tile) {
            // 提交 DMA：swap + draw_bitmap 立即返回
            // ISR 在 DMA 完成时自动 give，下次循环 take 阻塞等待
            fb.swap_bytes();  // 小端序 → ST7789 大端序（~0.15ms / 75KB）
            let y0 = (tile * framebuf::TILE_H) as i32;
            let y1 = y0 + framebuf::TILE_H as i32;
            unsafe {
                esp_lcd_panel_draw_bitmap(
                    LCD_PANEL,
                    0, y0, 240, y1,
                    fb.pixels().as_ptr() as *const std::ffi::c_void,
                );
            }
        } else {
            // dirty=false 跳过 DMA → 没有 ISR give → 必须手动 give 维护信号量平衡
            // 否则下次循环 take 会永久阻塞（无人 give）
            unsafe {
                xQueueGenericSend(LCD_DONE_SEM, std::ptr::null::<std::ffi::c_void>(), 0, 0);
            }
        }
    }
}

/// 完整主 UI 渲染（双 tile 循环 + dirty 跟踪）
/// 单字段变化（如 DTrac 频率追踪）通常只动一个 tile，撕裂感几乎不可见
///
/// 注意：`wifi_tcp_clients` 参数语义为"仅 WiFi/TCP rigctld 客户端数量"（不含 BLE）。
/// 调用方必须传 `s.rigctld_clients.saturating_sub(s.ble_clients)`，否则 BLE 连接时
/// IP 地址也会被 ui.rs 误染橙色。
///
/// `status_msg` 非空时顶栏左侧替换为临时状态消息（power toggle 等），空时显示默认 "elfRadio"
/// 中间 WiFi/BT 双图标（仅 status_msg 为空时显示）：
///   - WiFi 图标：`softap_active` → 橙色，否则蓝色
///   - BT 图标：`ble_clients > 0` → 橙色，否则蓝色
/// `ble_advertising` 当前未参与图标渲染（保留参数避免再改签名）
/// 底栏 IP：`softap_active` → 显示 192.168.4.1（softap_clients>0 橙否则蓝），否则按 wifi_state 走 STA 路径
/// 底栏 Radio：`rigctld_clients_total > 0` → 灰色 "Radio --"（任何 rigctld 客户端连接期间，含 BLE+TCP）
fn render_main_ui_tiled(
    fb: &mut framebuf::FrameBuf,
    left: &state::BandState,
    right: &state::BandState,
    radio_alive: bool,
    pc_alive: bool,
    wifi_state: &state::WifiState,
    wifi_ip: &str,
    wifi_tcp_clients: u32,
    status_msg: &str,
    status_msg_color: state::StatusMsgColor,
    ble_advertising: bool,
    ble_clients: u32,
    softap_active: bool,
    softap_clients: u32,
    rigctld_clients_total: u32,
    time_str: &str,
    time_color: timesync::ClockColor,
) {
    render_tiled(fb, |fb| {
        ui::draw_main_ui(fb, left, right, radio_alive, pc_alive, wifi_state, wifi_ip, wifi_tcp_clients,
            status_msg, status_msg_color, ble_advertising, ble_clients,
            softap_active, softap_clients, rigctld_clients_total,
            time_str, time_color);
    });
}

/// 开机画面（双 tile 循环，强制全部 dirty 确保完整刷新）
fn render_splash_tiled(fb: &mut framebuf::FrameBuf) {
    fb.invalidate_all();  // 开机首帧：所有 tile 都视为脏
    render_tiled(fb, |fb| ui::draw_splash(fb));
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    ::log::info!("ElfRadio HwNode 启动中...");

    let peripherals = Peripherals::take().unwrap();

    // ===== 关闭板载 WS2812B LED (GPIO 48，默认开机白色非常刺眼) =====
    {
        use std::time::Duration;
        let tx_config = TransmitConfig::new().clock_divider(1);
        if let Ok(mut tx) = TxRmtDriver::new(
            peripherals.rmt.channel0, peripherals.pins.gpio48, &tx_config
        ) {
            let ticks_hz: esp_idf_svc::hal::units::Hertz = tx.counter_clock().unwrap();
            let t0h = Pulse::new_with_duration(ticks_hz, PinState::High, &Duration::from_nanos(350)).unwrap();
            let t0l = Pulse::new_with_duration(ticks_hz, PinState::Low,  &Duration::from_nanos(800)).unwrap();
            let mut signal = FixedLengthSignal::<24>::new();
            for i in 0..24 { let _ = signal.set(i, &(t0h, t0l)); }
            let _ = tx.start_blocking(&signal);
        }
    }

    // ===== PWM 背光控制 (GPIO 13, 60% 亮度) =====
    let timer = LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::default().frequency(1.kHz().into()),
    ).expect("LEDC timer 初始化失败");
    let mut backlight = LedcDriver::new(
        peripherals.ledc.channel0, &timer, peripherals.pins.gpio13,
    ).expect("LEDC 背光通道初始化失败");
    // 先关闭背光，等屏幕初始化完成后再点亮
    backlight.set_duty(0).unwrap();
    ::log::info!("PWM 背光已初始化 (GPIO 13)");

    // ===== ESP-IDF esp_lcd SPI DMA 屏幕初始化 =====
    //
    // 使用 esp_idf_sys FFI 直接调用 ESP-IDF C API:
    //   1. spi_bus_initialize() — SPI 总线 + DMA
    //   2. esp_lcd_new_panel_io_spi() — SPI LCD IO + DMA 队列
    //   3. esp_lcd_new_panel_st7789() — ST7789 面板驱动
    //   4. esp_lcd_panel_draw_bitmap() — DMA 异步全屏刷新
    unsafe {
        // Step 1: SPI 总线（DMA 自动分配）
        // 注意：esp-idf-hal 的 SPI 已经占用了 peripherals.spi2，
        // 但我们不用 hal 的 SPI，直接用 C API 初始化 SPI2_HOST
        let mut bus_cfg: spi_bus_config_t = std::mem::zeroed();
        bus_cfg.__bindgen_anon_1.mosi_io_num = 11;  // GPIO11 = SDA
        bus_cfg.sclk_io_num = 12;                    // GPIO12 = SCL
        bus_cfg.__bindgen_anon_2.miso_io_num = -1;
        bus_cfg.__bindgen_anon_3.quadwp_io_num = -1;
        bus_cfg.__bindgen_anon_4.quadhd_io_num = -1;
        bus_cfg.max_transfer_sz = 240 * 320 * 2;     // 全屏一次传输
        let ret = spi_bus_initialize(spi_host_device_t_SPI2_HOST, &bus_cfg, spi_common_dma_t_SPI_DMA_CH_AUTO);
        assert!(ret == ESP_OK, "SPI 总线初始化失败: {}", ret);
        ::log::info!("SPI2 总线 + DMA 初始化完成");

        // Step 2: SPI LCD IO（DMA 队列深度 2，配合双缓冲）
        let mut io_cfg: esp_lcd_panel_io_spi_config_t = std::mem::zeroed();
        io_cfg.dc_gpio_num = 9;
        io_cfg.cs_gpio_num = 14;
        io_cfg.pclk_hz = 40_000_000;
        io_cfg.trans_queue_depth = 2;  // 双缓冲：2 帧队列，draw_bitmap 立即返回
        io_cfg.lcd_cmd_bits = 8;
        io_cfg.lcd_param_bits = 8;
        io_cfg.flags.set_dc_low_on_data(0);  // DC=0 命令，DC=1 数据（标准 ST7789）
        io_cfg.flags.set_lsb_first(0);

        let mut io_handle: esp_lcd_panel_io_handle_t = std::ptr::null_mut();
        let ret = esp_lcd_new_panel_io_spi(
            spi_host_device_t_SPI2_HOST as esp_lcd_spi_bus_handle_t,
            &io_cfg,
            &mut io_handle,
        );
        assert!(ret == ESP_OK, "LCD IO SPI 初始化失败: {}", ret);
        ::log::info!("LCD IO SPI 初始化完成 (40MHz, DMA queue=2)");

        // ===== 创建 LCD DMA 完成信号量 + 注册 on_color_trans_done ISR 回调 =====
        // 二值信号量：xQueueGenericCreate(length=1, item_size=0, type=3=BINARY_SEMAPHORE)
        LCD_DONE_SEM = xQueueGenericCreate(1, 0, 3);
        assert!(!LCD_DONE_SEM.is_null(), "LCD DMA 信号量创建失败");
        // 预 give 一次：让首次 render_tiled 循环顶部的 take 立即通过（buffer 此时空闲）
        // xQueueGenericSend(queue, item, ticks, pos): pos=0=queueSEND_TO_BACK
        // 二值信号量 item_size=0，pvItemToQueue 可为 null（FreeRTOS 实现忽略）
        xQueueGenericSend(LCD_DONE_SEM, std::ptr::null::<std::ffi::c_void>(), 0, 0);
        let cbs = esp_lcd_panel_io_callbacks_t {
            on_color_trans_done: Some(on_lcd_color_trans_done),
        };
        let ret = esp_lcd_panel_io_register_event_callbacks(io_handle, &cbs, std::ptr::null_mut());
        assert!(ret == ESP_OK, "LCD on_color_trans_done 回调注册失败: {}", ret);
        ::log::info!("LCD DMA 完成信号量 + ISR 回调已注册");

        // Step 3: ST7789 面板
        let mut panel_cfg: esp_lcd_panel_dev_config_t = std::mem::zeroed();
        panel_cfg.reset_gpio_num = 10;
        panel_cfg.bits_per_pixel = 16;  // RGB565
        // color_space 默认 0 = LCD_RGB_ELEMENT_ORDER_RGB

        let mut panel: esp_lcd_panel_handle_t = std::ptr::null_mut();
        let ret = esp_lcd_new_panel_st7789(io_handle, &panel_cfg, &mut panel);
        assert!(ret == ESP_OK, "ST7789 面板初始化失败: {}", ret);

        let ret = esp_lcd_panel_reset(panel);
        assert!(ret == ESP_OK, "面板复位失败: {}", ret);

        let ret = esp_lcd_panel_init(panel);
        assert!(ret == ESP_OK, "面板 init 失败: {}", ret);

        // IPS 屏需要反转颜色
        esp_lcd_panel_invert_color(panel, true);

        // 打开显示输出（init 只发 SLPOUT，不发 DISPON）
        esp_lcd_panel_disp_on_off(panel, true);

        // 保存到全局句柄
        LCD_PANEL = panel;
        ::log::info!("ST7789 DMA 面板初始化完成");
    }

    // ===== 单帧缓冲（内部 SRAM，DMA-capable）=====
    // PSRAM 与 WiFi 共享 GDMA 通道导致 LCD DMA 严重抖动；改用内部 SRAM
    let mut fb = framebuf::FrameBuf::new();
    ::log::info!("帧缓冲已分配（2-tile 模式，{}KB × {} = {}KB 内部 SRAM）",
        240 * framebuf::TILE_H * 2 / 1024, framebuf::NUM_TILES,
        240 * framebuf::TILE_H * 2 * framebuf::NUM_TILES / 1024);

    // ===== 开机画面（2 tile 循环刷新）=====
    render_splash_tiled(&mut fb);
    // 点亮背光（60% 亮度）
    let max_duty = backlight.get_max_duty();
    // 防烧屏：normal=用户配置（默认 60%）/ dim 0（PWM duty=0 完全断 LED，效果如断屏电源）
    // bl_normal 启动用 60% 默认，加载 NVS cfg 后会重新赋值（mut）
    let mut bl_normal: u32 = max_duty * 60 / 100;
    let bl_dim: u32 = 0;
    backlight.set_duty(bl_normal).unwrap();
    ::log::info!("开机画面已显示，背光 60%（待 NVS cfg 加载后调整）");
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // ===== 共享状态 =====
    let shared = state::new_shared_state();

    // ===== UART 初始化 =====
    let (uart1, uart2) = uart::init_uarts(
        peripherals.uart1,
        peripherals.pins.gpio17,
        peripherals.pins.gpio18,
        peripherals.uart2,
        peripherals.pins.gpio16,
        peripherals.pins.gpio7,
    );

    let uart1: &'static mut _ = Box::leak(Box::new(uart1));
    let uart2: &'static mut _ = Box::leak(Box::new(uart2));
    let uart1_ref: &'static _ = &*uart1;
    let uart2_ref: &'static _ = &*uart2;

    // ===== 启动 UART 中继线程（绑 CPU 0：硬件中继实时性强，与 main loop / LCD 同核）=====
    // ThreadSpawnConfiguration 是 sticky 全局状态，必须每次 spawn 前显式设；否则会
    // 继承上一次 spawn 的 pin_to_core，例如 BLE 后续 spawn 设 Core1 后再不显式设
    // Core0 就会跑到 CPU 1 上
    let state_a = shared.clone();
    let _ = esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
        pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core0),
        ..Default::default()
    }.set();
    let _thread_a = std::thread::Builder::new()
        .name("relay_down".into())
        .stack_size(8192)
        .spawn(move || {
            uart::relay_down_thread(uart1_ref, uart2_ref, state_a);
        })
        .expect("下行中继线程启动失败");

    let state_b = shared.clone();
    let _ = esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
        pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core0),
        ..Default::default()
    }.set();
    let _thread_b = std::thread::Builder::new()
        .name("relay_up".into())
        .stack_size(16384)  // 增大：PTT/VOL/SQL 注入 + Mutex + UART 写入需要足够栈
        .spawn(move || {
            uart::relay_up_thread(uart2_ref, uart1_ref, state_b);
        })
        .expect("上行中继线程启动失败");

    ::log::info!("UART 中继线程已启动 (CPU 0)");

    // ===== GPIO 8: TH-9800 开关机光耦控制 =====
    // GPIO 8 → 330Ω → PC817C (Opto2) → RJ-12 Pin 5 (电源开关)
    unsafe {
        let io_cfg = esp_idf_svc::sys::gpio_config_t {
            pin_bit_mask: 1u64 << 8,
            mode: esp_idf_svc::sys::gpio_mode_t_GPIO_MODE_OUTPUT,
            pull_up_en: esp_idf_svc::sys::gpio_pullup_t_GPIO_PULLUP_DISABLE,
            pull_down_en: esp_idf_svc::sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
            intr_type: esp_idf_svc::sys::gpio_int_type_t_GPIO_INTR_DISABLE,
        };
        esp_idf_svc::sys::gpio_config(&io_cfg);
        esp_idf_svc::sys::gpio_set_level(8, 0);  // 默认低电平（不触发）
    }
    ::log::info!("GPIO 8 (开关机光耦) 初始化完成");

    // ===== GPIO 2 物理按钮（长按 3 秒触发电台开关机 / 短按重启 BLE / 双击进退 SoftAP）=====
    button::start_button_thread(shared.clone());
    ::log::info!("[Button] 按钮线程已启动 (CPU 0)");

    // ===== PC 通信线程（共口二进制协议，绑 CPU 0：UART 实时通信，与 main loop 同核）=====
    // SoftAP 模式也保留：用户仍能通过 USB CDC 调试 / 用上位机 PC 配置
    pc_comm::init_pc_comm();
    let state_c = shared.clone();
    let _ = esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
        pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core0),
        ..Default::default()
    }.set();
    let _thread_c = std::thread::Builder::new()
        .name("pc_comm".into())
        .stack_size(8192)
        .spawn(move || {
            pc_comm::pc_comm_thread(uart1_ref, state_c, 8);
        })
        .expect("PC 通信线程启动失败");
    ::log::info!("PC 通信线程已启动 (CPU 0)");

    // ===== NVS 初始化 + SoftAP 模式判定 + 加载 cfg 到 state（C 功能）=====
    // 必须在 wifi.rs 之前完成，以决定后续启动哪些线程
    nvs_cfg::nvs_init_safe();
    let (is_softap, no_credentials) = nvs_cfg::should_enter_softap();
    ::log::info!("[启动模式] SoftAP={} no_credentials={}", is_softap, no_credentials);

    // 加载 NVS cfg → state（覆盖 state.rs::new() 默认值）
    {
        let mut s = shared.lock().unwrap();
        if let Some(name) = nvs_cfg::read_string(nvs_cfg::NS_CFG, nvs_cfg::KEY_BLE_NAME, 17) {
            if !name.is_empty() {
                s.cfg_ble_name.clear();
                let _ = s.cfg_ble_name.push_str(name.as_str());
            }
        }
        if let Some(b) = nvs_cfg::read_u8(nvs_cfg::NS_CFG, nvs_cfg::KEY_BRIGHTNESS) {
            s.cfg_brightness = b;
        }
        if let Some(t) = nvs_cfg::read_u16(nvs_cfg::NS_CFG, nvs_cfg::KEY_DIM_TIMEOUT) {
            s.cfg_dim_timeout_secs = t;
        }
        if let Some(n) = nvs_cfg::read_u8(nvs_cfg::NS_CFG, nvs_cfg::KEY_NTP_ENABLED) {
            s.cfg_ntp_enabled = n != 0;
        }
        if let Some(t) = nvs_cfg::read_u64(nvs_cfg::NS_CFG, nvs_cfg::KEY_MANUAL_TIME) {
            s.cfg_manual_time_us = t;
        }
        if let Some(tz) = nvs_cfg::read_string(nvs_cfg::NS_CFG, nvs_cfg::KEY_TZ_POSIX, 49) {
            if !tz.is_empty() {
                s.cfg_tz_posix.clear();
                let _ = s.cfg_tz_posix.push_str(tz.as_str());
            }
        }
        if let Some(srv) = nvs_cfg::read_string(nvs_cfg::NS_CFG, nvs_cfg::KEY_NTP_SERVER, 49) {
            if !srv.is_empty() {
                s.cfg_ntp_server.clear();
                let _ = s.cfg_ntp_server.push_str(srv.as_str());
            }
        }
        ::log::info!("[NVS cfg] ble_name=\"{}\" brightness={} dim_timeout={}s ntp={} tz=\"{}\" ntp_server=\"{}\"",
            s.cfg_ble_name.as_str(), s.cfg_brightness, s.cfg_dim_timeout_secs, s.cfg_ntp_enabled,
            s.cfg_tz_posix.as_str(), s.cfg_ntp_server.as_str());
    }

    // ===== 应用 TZ + 手动时间（必须在第一次 localtime_r/clock 渲染前完成）=====
    {
        let (tz, manual_us, ntp_on) = {
            let s = shared.lock().unwrap();
            (s.cfg_tz_posix.as_str().to_string(), s.cfg_manual_time_us, s.cfg_ntp_enabled)
        };
        timesync::apply_tz(&tz);
        if !ntp_on && manual_us != 0 {
            timesync::apply_manual_time(&shared, manual_us);
        }
    }

    // 应用 cfg_brightness 到 PWM 背光（覆盖默认 60%）
    {
        let cfg_brightness = shared.lock().unwrap().cfg_brightness.max(10).min(100);
        bl_normal = max_duty * (cfg_brightness as u32) / 100;
        backlight.set_duty(bl_normal).unwrap();
        ::log::info!("[屏幕] 应用 cfg 亮度 {}%", cfg_brightness);
    }

    // ===== WiFi 后台线程（STA 或 SoftAP，由 NVS 决定）=====
    let softap_param = wifi::SoftApMode { enabled: is_softap, no_credentials };
    wifi::start_wifi_thread(peripherals.modem, shared.clone(), softap_param);
    ::log::info!("WiFi 线程已启动");

    if !is_softap {
        // ===== STA 模式：启动 LAN / rigctld / BLE 等所有网络服务 =====
        // ===== LAN 设备发现（UDP 4534）=====
        discovery::start_discovery_thread(shared.clone());

        // ===== PC 通信 LAN 通道（TCP 4533，CRC16 协议与 USB 字节级一致）=====
        pc_tcp::start_pc_tcp_thread(shared.clone(), 8);

        // ===== Hamlib rigctld 文本协议服务器（TCP 4532）+ 频率步进线程 =====
        rigctld::start_rigctld_thread(shared.clone());
        rigctld::start_freq_stepper_thread(shared.clone());

        // ===== BLE 广播（手机 DTrac 直连用）— 阶段 2: GATT + rigctld 透传 =====
        ble::start_ble_thread(shared.clone());

        // ===== NTP 时间同步（仅 STA 模式；线程内部等 WiFi Connected）=====
        timesync::start_ntp_thread(shared.clone());
    } else {
        // ===== SoftAP 模式：跳过 BLE / rigctld / pc_tcp / discovery 启动 =====
        // 释放 ~48KB 内部 SRAM 给 HTTP server + DNS hijack（详见 plan 内存图）
        // softap.rs HTTP server + dns_server.rs UDP hijack 由 wifi.rs::ap_main 启动
        ::log::info!("[启动模式] SoftAP：跳过 BLE/rigctld/pc_tcp/discovery，仅启动 wifi+softap+dns_server+button+ui+pc_comm");
    }

    // ===== 初始绘制（2 tile 循环刷新，强制全部 dirty 确保完整画面）=====
    fb.invalidate_all();
    {
        let s = shared.lock().unwrap();
        // tcp_only：仅 WiFi/TCP rigctld 客户端数（排除 BLE）。BLE 客户端虽计入
        // rigctld_clients 用于触发 freq_stepper setup，但 IP 颜色应仅反映 LAN 活动
        let tcp_only = s.rigctld_clients.saturating_sub(s.ble_clients);
        let rc_total = s.rigctld_clients;
        let (left, right, alive, pc, ws, ip, smsg, scolor, ble_adv, ble_n, sap_a, sap_n) = (
            s.left.clone(), s.right.clone(),
            s.radio_alive, s.pc_alive,
            s.wifi_state.clone(), s.wifi_ip.clone(),
            s.status_msg.clone(), s.status_msg_color,
            s.ble_advertising, s.ble_clients,
            s.softap_active, s.softap_clients,
        );
        drop(s);
        let (init_time_str, init_time_color) = timesync::current_clock(&shared);
        render_main_ui_tiled(&mut fb, &left, &right, alive, pc, &ws, ip.as_str(), tcp_only,
            smsg.as_str(), scolor, ble_adv, ble_n, sap_a, sap_n, rc_total,
            init_time_str.as_str(), init_time_color);
    }

    // ===== 主循环: 双缓冲 + DMA 异步 =====
    // - sleep 20ms（100Hz tick 下 = 2 tick 真实让出，IDLE0 不饿 → 无 watchdog）
    // - 重绘最小间隔 50ms（20fps 上限，避免 SPI DMA queue 饱和）
    let mut last_body_count: u32 = 0;
    let mut last_head_count: u32 = 0;
    let mut last_pc_alive: bool = false;
    let mut last_wifi_state: state::WifiState = state::WifiState::NoCredentials;
    let mut last_wifi_ip: heapless::String<16> = heapless::String::new();
    let mut last_rigctld_clients: u32 = 0;
    let mut no_data_ticks:   u32 = 0;
    let mut last_redraw_us: u64 = 0;
    const MIN_REDRAW_INTERVAL_US: u64 = 200_000;  // 200ms = 5fps cap（PSRAM 帧缓冲 + WiFi 共享 DMA 时容易拥塞，状态显示 5fps 已足够）

    // ===== 防烧屏（150s 无用户活动后背光从 60% 调暗到 8%）=====
    // 心跳的 CW/CCW 注入会触发 body_count++ 和非 MAIN 侧 freq 变化，但**不**算用户活动。
    // 真实用户活动定义：head_count 变化 OR pc/wifi/rc 变化 OR MAIN 侧频率变化 OR S 上升沿到 ≥3
    //   - head_count: 上行帧（按键/旋钮）+ vol/sql 注入 + BLE/rigctld 状态事件
    //   - MAIN freq 变化: 用户调台（DTrac/手动）影响主操作侧（心跳只动非 MAIN 侧不会触发）
    //   - S 上升沿 ≥3: 强信号到达瞬间（≥3 是经验值，1-2 视为弱噪声不打扰）
    // S 持续 ≥3 期间暂停 dim 计时（信号期间保持亮屏，避免边沿触发后又被调暗）
    // DIM_AFTER_TICKS 由 NVS cfg_dim_timeout_secs 决定：默认 150s（C4 前硬编码 3000 ticks）
    // 0 = 禁用（永不熄屏）；其他 = 秒 × 20（50ms tick → 1 秒 = 20 ticks）
    let dim_after_ticks: u32 = {
        let s = shared.lock().unwrap();
        if s.cfg_dim_timeout_secs == 0 {
            u32::MAX  // 禁用熄屏
        } else {
            (s.cfg_dim_timeout_secs as u32).saturating_mul(20)
        }
    };
    ::log::info!("[屏幕] cfg 熄屏超时 = {} ticks ({} 秒)",
        dim_after_ticks, if dim_after_ticks == u32::MAX { 0 } else { dim_after_ticks / 20 });
    const SIGNAL_WAKE_THRESHOLD: u32 = 3;    // S ≥ 3 视为真实信号
    let mut no_user_activity_ticks: u32 = 0;
    let mut backlight_dimmed: bool = false;
    // 跟踪上次值用于检测 MAIN freq 变化、S 上升沿、BUSY 上升沿
    let mut last_left_freq: heapless::String<12> = heapless::String::new();
    let mut last_right_freq: heapless::String<12> = heapless::String::new();
    let mut last_max_s_level: u32 = 0;
    let mut last_left_busy: bool = false;
    let mut last_right_busy: bool = false;

    // ===== 启动宽限期 + 自动开机（Bug 1 修复）=====
    // 通电 ESP32 时若电台未开机，radio_alive=false 默认；不能立即关屏装死。
    // 给电台开机后"主动发首帧"留 8 秒窗口（电台启动 2-5s + 首帧到达 ~1s + 安全余量）。
    // 8s 内屏幕保持亮等待 radio_alive 自然变 true（每收到下行帧 protocol.rs:145 置 true）。
    // 8s 后仍 false → 自动 spawn power_toggle_worker(true)：发 GPIO 8 脉冲启动电台 → 电台首帧 → alive=true。
    // 仅触发 1 次（auto_boot_attempted），避免循环故障；硬件故障时 30s "Radio On FAIL" 红字提示。
    let boot_us = unsafe { esp_timer_get_time() } as u64;
    const BOOT_GRACE_US: u64 = 8_000_000; // 启动 8 秒宽限期
    let mut auto_boot_attempted: bool = false;

    // ===== 时钟每秒 tick（用于驱动右上角 HH:MM:SS 每秒刷新）=====
    // last_displayed_sec 跟踪上次渲染时的 unix 秒数；秒数变化且已同步 → 触发 clock_tick → redraw
    // 未同步时 synced=false → 不触发，屏幕静态显示 "--:--:--" 省 CPU
    let mut last_displayed_sec: i64 = -1;

    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let now_us = unsafe { esp_timer_get_time() } as u64;
        let can_redraw = now_us.saturating_sub(last_redraw_us) >= MIN_REDRAW_INTERVAL_US;

        // 时钟 tick 检测：每 50ms 取 unix 秒（gettimeofday 不需要锁）
        // 仅 last_ntp_sync_us != 0 时认为 synced；secs 变化才触发
        let cur_unix_sec = timesync::current_unix_sec();
        let synced_for_clock = shared.try_lock()
            .map(|s| s.last_ntp_sync_us != 0)
            .unwrap_or(false);
        let clock_tick = synced_for_clock && cur_unix_sec != last_displayed_sec;

        if let Ok(mut s) = shared.try_lock() {
            // 顶栏 status_msg 自动过期清理（如 "Power Toggle FAILED" 5 秒后清空）
            // 必须在 head_count 检测之前；清空时也 head_count++ 触发本轮重绘
            if !s.status_msg.is_empty()
                && s.status_msg_clear_at_us > 0
                && now_us >= s.status_msg_clear_at_us
            {
                s.status_msg.clear();
                s.status_msg_clear_at_us = 0;
                s.head_count = s.head_count.wrapping_add(1);
                ::log::info!("[屏幕] 状态消息到期自动清除");
            }

            let body_changed = s.body_count != last_body_count;
            let head_changed = s.head_count != last_head_count;
            let radio_changed = body_changed || head_changed;
            let pc_changed = s.pc_alive != last_pc_alive;
            let wifi_changed = s.wifi_state != last_wifi_state || s.wifi_ip != last_wifi_ip;
            let rc_changed = s.rigctld_clients != last_rigctld_clients;
            // clock_tick 加入 redraw 触发：每秒重绘时间区
            let any_change = radio_changed || pc_changed || wifi_changed || rc_changed || clock_tick;

            // ===== 防烧屏：识别真实用户活动（排除心跳响应）=====
            // 心跳: body_count++ + 非 MAIN 侧 freq 变化（不算活动）
            // 真活动: head_count 变化 / pc/wifi/rc 变化 / MAIN 侧 freq 变化 / S 上升沿到 ≥3 / BUSY 上升沿
            let curr_max_s = s.left.s_level.max(s.right.s_level);
            let signal_arrived = last_max_s_level < SIGNAL_WAKE_THRESHOLD
                && curr_max_s >= SIGNAL_WAKE_THRESHOLD;
            let signal_present = curr_max_s >= SIGNAL_WAKE_THRESHOLD;
            // BUSY=ON 意味着 squelch open + 喇叭出音，用户大概率想看屏幕
            // 上升沿（OFF→ON）唤醒；持续 ON 期间暂停 dim 计时
            let busy_present = s.left.is_busy || s.right.is_busy;
            let busy_arrived = (s.left.is_busy && !last_left_busy)
                || (s.right.is_busy && !last_right_busy);
            let main_freq_changed =
                (s.left.is_main && s.left.freq != last_left_freq) ||
                (s.right.is_main && s.right.freq != last_right_freq);
            let user_activity = head_changed || pc_changed || wifi_changed || rc_changed
                || main_freq_changed || signal_arrived || busy_arrived;

            // 电台开关机进行中标志（button.rs 长按触发后置 true，worker 完成后 release）
            // 用作 wake 例外：长按时屏幕亮起反馈（即使电台还没启动）
            let power_toggling = button::POWER_TOGGLE_IN_PROGRESS
                .load(std::sync::atomic::Ordering::Relaxed);

            // 启动宽限期判定：8 秒内不关屏 + 8 秒后 alive=false 自动开机
            let since_boot_us = now_us.saturating_sub(boot_us);
            let in_boot_grace = since_boot_us < BOOT_GRACE_US;

            // 启动 8s 后仍 alive=false → 先发主动探针，电台若已开机会响应（跳过脉冲）
            // 否则才发开机脉冲。避免"电台已开机但静默"被误判为关机后被脉冲关掉
            // 不在 power_toggling 期间触发（避免与手动长按或本次自动重复）
            if !s.radio_alive && !auto_boot_attempted && !in_boot_grace && !power_toggling {
                auto_boot_attempted = true;
                drop(s);
                if button::try_acquire_power_lock() {
                    let st = shared.clone();
                    std::thread::spawn(move || {
                        ::log::info!("[自动开机] 启动 {}s 后未收到下行帧，先发主动探针确认电台是否真的关机",
                            BOOT_GRACE_US / 1_000_000);
                        let mut radio_responded = false;
                        for probe_attempt in 0..3u8 {
                            if button::probe_radio_alive(&st) {
                                radio_responded = true;
                                ::log::info!("[自动开机] 探针 {}/3 有响应，电台已开机，跳过自动开机脉冲",
                                    probe_attempt + 1);
                                break;
                            }
                            ::log::info!("[自动开机] 探针 {}/3 无响应", probe_attempt + 1);
                        }
                        if !radio_responded {
                            ::log::info!("[自动开机] 3 次探针均无响应，电台真的关机，触发自动开机脉冲");
                            button::power_toggle_worker(st, true);  // is_auto=true，FAIL 显示 30s
                        }
                        // 探针响应：响应帧已让 protocol.rs 设 radio_alive=true，无需额外动作
                        button::release_power_lock();
                    });
                } else {
                    ::log::warn!("[自动开机] 已有 PowerToggle 进行中，跳过自动开机");
                }
                continue;  // 让下次循环处理新状态（power_toggling 已 true）
            }

            // 电台离线 → 强制关背光（覆盖 user_activity 唤醒）
            // 例外 1：power_toggling=true 时允许唤醒（让用户长按/自动开机时屏幕亮起反馈）
            // 例外 2：in_boot_grace（启动 8s 内）保持亮屏，等电台开机后下行帧自然到达
            if !s.radio_alive && !backlight_dimmed && !power_toggling && !in_boot_grace {
                let _ = backlight.set_duty(bl_dim);
                backlight_dimmed = true;
                ::log::info!("[屏幕] 电台离线，关闭背光");
            }

            // 用户活动 → 立即恢复背光（独立于 can_redraw 节流）
            // 唤醒条件：alive 或 power_toggling 或 in_boot_grace；否则即使有活动也保持关屏
            if user_activity && backlight_dimmed && (s.radio_alive || power_toggling || in_boot_grace) {
                let _ = backlight.set_duty(bl_normal);
                backlight_dimmed = false;
                ::log::info!("[屏幕] 检测到活动，背光恢复 60%");
            }

            // 更新跟踪：last_left_freq / last_right_freq / last_max_s_level / last_*_busy
            // 必须每次主循环都更新（不依赖 redraw 路径），保证 main_freq_changed / signal_arrived / busy_arrived 准确
            last_max_s_level = curr_max_s;
            last_left_busy = s.left.is_busy;
            last_right_busy = s.right.is_busy;
            if s.left.freq != last_left_freq {
                last_left_freq.clear();
                let _ = last_left_freq.push_str(s.left.freq.as_str());
            }
            if s.right.freq != last_right_freq {
                last_right_freq.clear();
                let _ = last_right_freq.push_str(s.right.freq.as_str());
            }

            if any_change && can_redraw {
                last_body_count = s.body_count;
                last_head_count = s.head_count;
                last_pc_alive = s.pc_alive;
                last_wifi_state = s.wifi_state.clone();
                last_wifi_ip.clear();
                let _ = last_wifi_ip.push_str(s.wifi_ip.as_str());
                last_rigctld_clients = s.rigctld_clients;
                last_displayed_sec = cur_unix_sec;  // 标记本次 redraw 已显示该秒
                no_data_ticks = 0;
                // tcp_only：仅 WiFi/TCP 客户端，排除 BLE（BLE 用 GATT 不用 IP）
                let tcp_only = s.rigctld_clients.saturating_sub(s.ble_clients);
                let rc_total = s.rigctld_clients;
                let (left, right, alive, pc, ws, ip, smsg, scolor, ble_adv, ble_n, sap_a, sap_n) = (
                    s.left.clone(), s.right.clone(),
                    s.radio_alive, s.pc_alive,
                    s.wifi_state.clone(), s.wifi_ip.clone(),
                    s.status_msg.clone(), s.status_msg_color,
                    s.ble_advertising, s.ble_clients,
                    s.softap_active, s.softap_clients,
                );
                drop(s);
                let (tstr, tcol) = timesync::current_clock(&shared);
                render_main_ui_tiled(&mut fb, &left, &right, alive, pc, &ws, ip.as_str(), tcp_only,
                    smsg.as_str(), scolor, ble_adv, ble_n, sap_a, sap_n, rc_total,
                    tstr.as_str(), tcol);
                last_redraw_us = now_us;
            } else if !any_change {
                no_data_ticks += 1;
                // 仅在没有 rigctld 客户端时才超时置 alive=false
                // rigctld 客户端（含 BLE）连接时认为电台一直 alive：
                //   - DTrac 多阶段 setup 期间存在 10-30s 静默窗口（阶段间重试节流），
                //     若此时 alive=false 会导致 inject_menu_set 的 Guard2 拒绝执行，
                //     表现为"BLE 初始化卡死，需按机头键解除"
                //   - 真正下行响应到达时 protocol::apply_to_state 会自动重置 alive=true
                //   - 真正掉线场景：DTrac 自己会 timeout 报错，无需 ESP32 端探测
                if no_data_ticks == 300 && s.rigctld_clients == 0 {  // 300 × 50ms = 15s
                    s.radio_alive = false;
                    let tcp_only = s.rigctld_clients.saturating_sub(s.ble_clients);
                    let rc_total = s.rigctld_clients;
                    let (left, right, pc, ws, ip, smsg, scolor, ble_adv, ble_n, sap_a, sap_n) = (
                        s.left.clone(), s.right.clone(), s.pc_alive,
                        s.wifi_state.clone(), s.wifi_ip.clone(),
                        s.status_msg.clone(), s.status_msg_color,
                        s.ble_advertising, s.ble_clients,
                        s.softap_active, s.softap_clients,
                    );
                    drop(s);
                    let (tstr, tcol) = timesync::current_clock(&shared);
                    render_main_ui_tiled(&mut fb, &left, &right, false, pc, &ws, ip.as_str(), tcp_only,
                        smsg.as_str(), scolor, ble_adv, ble_n, sap_a, sap_n, rc_total,
                        tstr.as_str(), tcol);
                }
            }

            // ===== 防烧屏调暗判定（独立于 any_change 路径）=====
            // dim_pause = 信号持续 ≥3 OR BUSY 持续 ON（喇叭出音用户想看屏）
            // 用户活动 reset 计时；dim_pause 期间暂停计时（不调暗也不重置）；其它累加
            let dim_pause = signal_present || busy_present;
            if user_activity {
                no_user_activity_ticks = 0;
            } else if !dim_pause {
                no_user_activity_ticks = no_user_activity_ticks.saturating_add(1);
                // 用 == 触发：精确 150s 时关 LED 一次，之后 backlight_dimmed 防重入
                if no_user_activity_ticks == dim_after_ticks && !backlight_dimmed {
                    let _ = backlight.set_duty(bl_dim);
                    backlight_dimmed = true;
                    ::log::info!("[屏幕] 150 秒无用户活动且无信号无 BUSY，关闭背光");
                }
            }
            // dim_pause && !user_activity 时既不重置也不累加，等信号/BUSY 消失后从原值继续
        }
    }
}
