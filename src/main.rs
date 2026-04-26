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

use esp_idf_svc::hal::prelude::*;
use esp_idf_svc::hal::rmt::{config::TransmitConfig, FixedLengthSignal, PinState, Pulse, TxRmtDriver};
use esp_idf_svc::hal::ledc::{LedcDriver, LedcTimerDriver, config::TimerConfig};

use esp_idf_svc::sys::*;

// ===== ESP-IDF LCD DMA 面板句柄（全局，供 flush 使用）=====
static mut LCD_PANEL: esp_lcd_panel_handle_t = std::ptr::null_mut();

/// 将帧缓冲通过 DMA 异步刷到屏幕
/// swap_bytes 修正 RGB565 字节序（小端→大端），然后提交 DMA
/// queue_depth=2 + 双缓冲：draw_bitmap 立即返回，CPU 不阻塞
/// 注意：提交后不恢复字节序——双缓冲下此 buffer 在 DMA 完成前不会被 CPU 写入
fn flush_fb_dma(fb: &mut framebuf::FrameBuf) {
    fb.swap_bytes();  // 小端序 → ST7789 大端序（0.3ms）
    unsafe {
        esp_lcd_panel_draw_bitmap(
            LCD_PANEL,
            0, 0, 240, 320,
            fb.pixels().as_ptr() as *const std::ffi::c_void,
        );
    }
    // 不恢复字节序！此 buffer 交给 DMA，下次循环画到另一个 buffer
    // 当此 buffer 再次轮到绘制时，draw_ui 会先 clear 再重绘，覆盖所有像素
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("ElfRadio HwNode 启动中...");

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
    log::info!("PWM 背光已初始化 (GPIO 13)");

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
        log::info!("SPI2 总线 + DMA 初始化完成");

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
        log::info!("LCD IO SPI 初始化完成 (40MHz, DMA queue=2)");

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
        log::info!("ST7789 DMA 面板初始化完成");
    }

    // ===== 单帧缓冲（内部 SRAM，DMA-capable）=====
    // PSRAM 与 WiFi 共享 GDMA 通道导致 LCD DMA 严重抖动；改用内部 SRAM
    let mut fb = framebuf::FrameBuf::new();
    log::info!("单帧缓冲已分配 ({}KB 内部 SRAM)", 240 * 320 * 2 / 1024);

    // ===== 开机画面 =====
    ui::draw_splash(&mut fb);
    flush_fb_dma(&mut fb);
    // 点亮背光（60% 亮度）
    let max_duty = backlight.get_max_duty();
    backlight.set_duty(max_duty * 60 / 100).unwrap();
    log::info!("开机画面已显示，背光 60%");
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

    // ===== 启动 UART 中继线程 =====
    let state_a = shared.clone();
    let _thread_a = std::thread::Builder::new()
        .name("relay_down".into())
        .stack_size(8192)
        .spawn(move || {
            uart::relay_down_thread(uart1_ref, uart2_ref, state_a);
        })
        .expect("下行中继线程启动失败");

    let state_b = shared.clone();
    let _thread_b = std::thread::Builder::new()
        .name("relay_up".into())
        .stack_size(16384)  // 增大：PTT/VOL/SQL 注入 + Mutex + UART 写入需要足够栈
        .spawn(move || {
            uart::relay_up_thread(uart2_ref, uart1_ref, state_b);
        })
        .expect("上行中继线程启动失败");

    log::info!("UART 中继线程已启动");

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
    log::info!("GPIO 8 (开关机光耦) 初始化完成");

    // ===== PC 通信线程（共口二进制协议）=====
    pc_comm::init_pc_comm();
    let state_c = shared.clone();
    let _thread_c = std::thread::Builder::new()
        .name("pc_comm".into())
        .stack_size(8192)
        .spawn(move || {
            pc_comm::pc_comm_thread(uart1_ref, state_c, 8);
        })
        .expect("PC 通信线程启动失败");
    log::info!("PC 通信线程已启动");

    // ===== WiFi STA 后台线程 =====
    wifi::start_wifi_thread(peripherals.modem, shared.clone());
    log::info!("WiFi 线程已启动");

    // ===== LAN 设备发现（UDP 4534）=====
    discovery::start_discovery_thread(shared.clone());

    // ===== PC 通信 LAN 通道（TCP 4533，CRC16 协议与 USB 字节级一致）=====
    pc_tcp::start_pc_tcp_thread(shared.clone(), 8);

    // ===== Hamlib rigctld 文本协议服务器（TCP 4532）+ 频率步进线程 =====
    rigctld::start_rigctld_thread(shared.clone());
    rigctld::start_freq_stepper_thread(shared.clone());

    // ===== 初始绘制 =====
    {
        let s = shared.lock().unwrap();
        ui::draw_main_ui(&mut fb, &s.left, &s.right, s.radio_alive, s.pc_alive,
            &s.wifi_state, s.wifi_ip.as_str(), s.rigctld_clients);
    }
    flush_fb_dma(&mut fb);
    log::info!("UI 就绪，进入主循环");

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

    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let now_us = unsafe { esp_timer_get_time() } as u64;
        let can_redraw = now_us.saturating_sub(last_redraw_us) >= MIN_REDRAW_INTERVAL_US;

        if let Ok(mut s) = shared.try_lock() {
            let radio_changed = s.body_count != last_body_count || s.head_count != last_head_count;
            let pc_changed = s.pc_alive != last_pc_alive;
            let wifi_changed = s.wifi_state != last_wifi_state || s.wifi_ip != last_wifi_ip;
            let rc_changed = s.rigctld_clients != last_rigctld_clients;

            if (radio_changed || pc_changed || wifi_changed || rc_changed) && can_redraw {
                last_body_count = s.body_count;
                last_head_count = s.head_count;
                last_pc_alive = s.pc_alive;
                last_wifi_state = s.wifi_state.clone();
                last_wifi_ip.clear();
                let _ = last_wifi_ip.push_str(s.wifi_ip.as_str());
                last_rigctld_clients = s.rigctld_clients;
                no_data_ticks = 0;
                let (left, right, alive, pc, ws, ip, rc) = (
                    s.left.clone(), s.right.clone(),
                    s.radio_alive, s.pc_alive,
                    s.wifi_state.clone(), s.wifi_ip.clone(), s.rigctld_clients
                );
                drop(s);
                ui::draw_main_ui(&mut fb, &left, &right, alive, pc, &ws, ip.as_str(), rc);
                flush_fb_dma(&mut fb);
                last_redraw_us = now_us;
            } else if !radio_changed && !pc_changed && !wifi_changed && !rc_changed {
                no_data_ticks += 1;
                if no_data_ticks == 300 {  // 300 × 50ms = 15s
                    s.radio_alive = false;
                    let (left, right, pc, ws, ip, rc) = (
                        s.left.clone(), s.right.clone(), s.pc_alive,
                        s.wifi_state.clone(), s.wifi_ip.clone(), s.rigctld_clients
                    );
                    drop(s);
                    ui::draw_main_ui(&mut fb, &left, &right, false, pc, &ws, ip.as_str(), rc);
                    flush_fb_dma(&mut fb);
                }
            }
        }
    }
}
