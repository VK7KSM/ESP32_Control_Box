// ===================================================================
// TH-9800 GPIO 直通中继 v3 — 纯寄存器操作
// ===================================================================

use esp_idf_svc::hal::prelude::*;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("=== TH-9800 GPIO 直通中继 v3 ===");

    let _peripherals = Peripherals::take().unwrap();

    // ESP32-S3 GPIO 寄存器地址
    const GPIO_ENABLE_W1TS: u32 = 0x6000_4020; // GPIO_ENABLE_W1TS_REG: 使能输出
    const GPIO_ENABLE_W1TC: u32 = 0x6000_4024; // GPIO_ENABLE_W1TC_REG: 禁用输出(设为输入)
    const GPIO_OUT_W1TS: u32 = 0x6000_4008;    // GPIO_OUT_W1TS_REG: 置高
    const GPIO_OUT_W1TC: u32 = 0x6000_400C;    // GPIO_OUT_W1TC_REG: 置低
    const GPIO_IN: u32 = 0x6000_403C;          // GPIO_IN_REG: 读输入
    const GPIO_FUNC_OUT_SEL_BASE: u32 = 0x6000_4554; // GPIO_FUNCn_OUT_SEL_CFG_REG

    let bit_17 = 1u32 << 17;
    let bit_18 = 1u32 << 18;
    let bit_19 = 1u32 << 19;
    let bit_20 = 1u32 << 20;

    unsafe {
        // 配置 GPIO 17, 20 为输出（通过 GPIO 矩阵，选择 simple GPIO output = 0x80）
        // GPIO_FUNCn_OUT_SEL_CFG_REG 地址 = base + n*4
        let func17 = (GPIO_FUNC_OUT_SEL_BASE + 17 * 4) as *mut u32;
        let func20 = (GPIO_FUNC_OUT_SEL_BASE + 20 * 4) as *mut u32;
        core::ptr::write_volatile(func17, 0x80); // SIG_GPIO_OUT_IDX = 128
        core::ptr::write_volatile(func20, 0x80);

        // 使能 GPIO 17, 20 为输出
        core::ptr::write_volatile(GPIO_ENABLE_W1TS as *mut u32, bit_17 | bit_20);

        // 配置 GPIO 18, 19 为输入（禁用输出使能）
        core::ptr::write_volatile(GPIO_ENABLE_W1TC as *mut u32, bit_18 | bit_19);

        // 配置 IO MUX: 选择 GPIO 功能 (Function 1 = GPIO)
        // IO_MUX_GPIOx_REG: 每个 GPIO 有一个 IO MUX 寄存器
        // 地址: 0x6000_9000 + pin_offset (不同引脚偏移不同)
        // 简单方式: 使用 esp-idf 的 gpio_set_direction
        esp_idf_svc::sys::gpio_reset_pin(17);
        esp_idf_svc::sys::gpio_reset_pin(18);
        esp_idf_svc::sys::gpio_reset_pin(19);
        esp_idf_svc::sys::gpio_reset_pin(20);

        esp_idf_svc::sys::gpio_set_direction(17, esp_idf_svc::sys::gpio_mode_t_GPIO_MODE_OUTPUT);
        esp_idf_svc::sys::gpio_set_direction(18, esp_idf_svc::sys::gpio_mode_t_GPIO_MODE_INPUT);
        esp_idf_svc::sys::gpio_set_direction(19, esp_idf_svc::sys::gpio_mode_t_GPIO_MODE_INPUT);
        esp_idf_svc::sys::gpio_set_direction(20, esp_idf_svc::sys::gpio_mode_t_GPIO_MODE_OUTPUT);

        // 初始状态: 输出高电平 (UART 空闲)
        core::ptr::write_volatile(GPIO_OUT_W1TS as *mut u32, bit_17 | bit_20);
    }

    log::info!("GPIO 17(输出) 18(输入) 19(输入) 20(输出) 配置完成");
    log::info!("进入直通循环...");

    // 验证: 读一次输入值
    unsafe {
        let val = core::ptr::read_volatile(GPIO_IN as *const u32);
        log::info!("GPIO_IN = 0x{:08X}, bit18={}, bit19={}", val, (val >> 18) & 1, (val >> 19) & 1);
    }

    // 从看门狗中移除主线程，避免紧密循环触发 WDT
    unsafe {
        let handle = esp_idf_svc::sys::xTaskGetCurrentTaskHandle();
        esp_idf_svc::sys::esp_task_wdt_delete(handle);
    }
    log::info!("已禁用主线程看门狗");

    loop {
        unsafe {
            let input = core::ptr::read_volatile(GPIO_IN as *const u32);

            // 下行: GPIO18 → GPIO20
            if input & bit_18 != 0 {
                core::ptr::write_volatile(GPIO_OUT_W1TS as *mut u32, bit_20);
            } else {
                core::ptr::write_volatile(GPIO_OUT_W1TC as *mut u32, bit_20);
            }

            // 上行: GPIO19 → GPIO17
            if input & bit_19 != 0 {
                core::ptr::write_volatile(GPIO_OUT_W1TS as *mut u32, bit_17);
            } else {
                core::ptr::write_volatile(GPIO_OUT_W1TC as *mut u32, bit_17);
            }
        }
    }
}
