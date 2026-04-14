// ===================================================================
// UART 初始化 + TH-9800 MITM 中继线程
//
// 引脚分配（已通过 Fuzzer 实测验证）:
//   UART1 TX=GPIO17 → 主机RXD  (转发面板上行)
//   UART1 RX=GPIO18 ← 主机TXD  (接收主机下行)
//   UART2 TX=GPIO16 → 面板RXD  (转发主机下行)
//   UART2 RX=GPIO7  ← 面板TXD  (接收面板上行)
//
// 线程A (relay_down): GPIO18 → 解析下行 AA FD 帧 → GPIO16
// 线程B (relay_up):   GPIO7  → 帧级缓冲 → override(VOL/SQL/PTT) → GPIO17
// ===================================================================

use esp_idf_svc::hal::gpio;
use esp_idf_svc::hal::uart::{self, UartDriver, config::Config};
use esp_idf_svc::hal::units::Hertz;
use crate::protocol::{DownParser, UpParser};
use crate::state::SharedState;

// ===== 心跳帧常量 =====

// S-Meter 视觉心跳帧（→ uart_panel → 面板），1格场强闪烁
const SM_L_ON:  [u8; 6] = [0xAA,0xFD,0x02, 0x1D,0x01, 0x1E]; // 左侧 1 格
const SM_L_OFF: [u8; 6] = [0xAA,0xFD,0x02, 0x1D,0x00, 0x1F]; // 左侧 0 格
const SM_R_ON:  [u8; 6] = [0xAA,0xFD,0x02, 0x1D,0x81, 0x9E]; // 右侧 1 格
const SM_R_OFF: [u8; 6] = [0xAA,0xFD,0x02, 0x1D,0x80, 0x9F]; // 右侧 0 格

/// 动态构建上行旋钮帧：P[2]=旋钮步进值，VOL/SQL 填 0xFF（与真实面板旋钮帧一致）
pub fn build_knob_frame(step: u8) -> [u8; 16] {
    let mut f = [0u8; 16];
    f[0] = 0xAA; f[1] = 0xFD; f[2] = 0x0C;
    f[3] = 0x84;                        // P[0] 固定包头
    f[4] = 0xFF;                        // P[1] PTT 松开
    f[5] = step;                        // P[2] 旋钮步进
    f[6] = 0xFF;                        // P[3] 无按键
    f[7] = 0xFF;                        // P[4] 无键码
    f[8]  = 0x81;                       // P[5] VOL 标志（固定 0x81，与真实面板一致）
    f[9]  = 0xFF;                       // P[6] VOL 低字节（0xFF = 不更新）
    f[10] = 0xFF;                       // P[7] VOL 高字节
    f[11] = 0x82;                       // P[8] SQL 标志（固定 0x82，与真实面板一致）
    f[12] = 0xFF;                       // P[9] SQL 低字节（0xFF = 不更新）
    f[13] = 0xFF;                       // P[10] SQL 高字节
    f[14] = 0x00;                       // P[11] 固定尾
    // XOR 校验: 0x0C ^ Payload 各字节（与下行校验算法相同，非累加和！）
    let mut sum: u8 = 0x0C;
    for i in 3..15 { sum ^= f[i]; }
    f[15] = sum;
    f
}

/// 构建上行按键帧：P[3]=0x00(按下), P[4]=键码
pub fn build_key_frame(key: u8) -> [u8; 16] {
    let mut f = build_knob_frame(0xFF);  // 基于空闲帧模板
    f[6] = 0x00;   // P[3] = 按键触发
    f[7] = key;    // P[4] = 键码
    // 重算 XOR 校验
    let mut sum: u8 = 0x0C;
    for i in 3..15 { sum ^= f[i]; }
    f[15] = sum;
    f
}

/// 构建带实际 VOL/SQL ADC 值的上行帧
/// side: false=左侧(0x01/0x02), true=右侧(0x81/0x82)
pub fn build_vol_sql_frame(vol: Option<u16>, sql: Option<u16>, right_side: bool) -> [u8; 16] {
    let mut f = build_knob_frame(0xFF);
    if let Some(v) = vol {
        f[8]  = if right_side { 0x81 } else { 0x01 };
        f[9]  = (v & 0xFF) as u8;
        f[10] = (v >> 8) as u8;
    }
    if let Some(s) = sql {
        f[11] = if right_side { 0x82 } else { 0x02 };
        f[12] = (s & 0xFF) as u8;
        f[13] = (s >> 8) as u8;
    }
    let mut sum: u8 = 0x0C;
    for i in 3..15 { sum ^= f[i]; }
    f[15] = sum;
    f
}

/// 重算上行帧 XOR 校验（修改帧内容后必须调用）
fn recalc_uplink_xor(frame: &mut [u8; 16]) {
    let mut sum: u8 = 0x0C;
    for i in 3..15 { sum ^= frame[i]; }
    frame[15] = sum;
}

/// 对完整上行帧应用 override（仅 PTT），然后重算校验
/// Key/Knob 注入已移至 timeout 分支（面板空闲时发送独立帧），不篡改面板真实帧
fn apply_overrides(frame: &mut [u8; 16], state: &SharedState) {
    let s = state.lock().unwrap();
    if s.ptt_override {
        frame[4] = 0x00;  // P[1] = PTT 按下
        drop(s);
        recalc_uplink_xor(frame);
    }
}

/// 初始化 UART1（主机侧）和 UART2（面板侧）
pub fn init_uarts<'a>(
    uart1: uart::UART1,
    tx1:   gpio::Gpio17,  // 主机侧 TX: GPIO17
    rx1:   gpio::Gpio18,  // 主机侧 RX: GPIO18
    uart2: uart::UART2,
    tx2:   gpio::Gpio16,  // 面板侧 TX: GPIO16 → 机头RXD (A4位置)
    rx2:   gpio::Gpio7,   // 面板侧 RX: GPIO7  ← 机头TXD (A3位置)
) -> (UartDriver<'a>, UartDriver<'a>) {
    let cfg = Config::new().baudrate(Hertz(19200));

    let uart1_drv = UartDriver::new(
        uart1, tx1, rx1,
        Option::<gpio::AnyIOPin>::None,
        Option::<gpio::AnyIOPin>::None,
        &cfg,
    ).expect("UART1 (主机侧) 初始化失败");

    let uart2_drv = UartDriver::new(
        uart2, tx2, rx2,
        Option::<gpio::AnyIOPin>::None,
        Option::<gpio::AnyIOPin>::None,
        &cfg,
    ).expect("UART2 (面板侧) 初始化失败");

    log::info!("UART1 + UART2 初始化完成 (19200 8N1)");
    (uart1_drv, uart2_drv)
}

/// 线程A: 下行透传 — 主机TXD → 解析 AA FD 帧 → 面板RXD
///
/// uart_host:  UART1 (读主机下行数据)
/// uart_panel: UART2 (写给面板)
pub fn relay_down_thread(
    uart_host:  &UartDriver<'_>,
    uart_panel: &UartDriver<'_>,
    state: SharedState,
) {
    let mut parser = DownParser::new();
    let mut buf = [0u8; 1];
    let mut count: u32 = 0;

    log::info!("[下行] 中继线程启动");

    loop {
        match uart_host.read(&mut buf, 1000) {  // 1000 tick 超时，约 1s
            Ok(1) => {
                // 先透传，再解析（零延迟）
                let _ = uart_panel.write(&buf);

                if parser.feed(buf[0]) {
                    count = count.wrapping_add(1);
                    if count == 1 || count % 200 == 0 {
                        log::info!("[下行] 第 {} 帧", count);
                    }
                    {
                        let mut s = state.lock().unwrap();
                        parser.apply_to_state(&mut s);
                        s.body_count = count;
                    } // 锁释放后再 sleep，确保 IDLE0 能定期运行（防止 Watchdog）
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            _ => {
                // Ok(0) 或 Err(ESP_ERR_TIMEOUT)：均为超时，1 秒无数据
                log::warn!("[下行] 等待数据... 共收到 {} 帧", count);
            }
        }
    }
}

/// 线程B: 上行中继 — 面板TXD → 帧级缓冲 → override(VOL/SQL/PTT) → 主机RXD
///
/// 与旧版逐字节透传的区别：
/// - 帧同步中的字节先缓冲，帧完成后修改并一次性转发（+8ms 延迟）
/// - 同步失败时补发缓冲字节（不丢数据）
/// - 支持 VOL/SQL ADC 值替换和 PTT 强制按下
///
/// uart_panel: UART2 (读面板上行数据)
/// uart_host:  UART1 (写给主机)
pub fn relay_up_thread(
    uart_panel: &UartDriver<'_>,
    uart_host:  &UartDriver<'_>,
    state: SharedState,
) {
    let mut parser = UpParser::new();
    let mut buf = [0u8; 1];
    let mut count: u32 = 0;
    let mut heartbeat_ticks: u32 = 0;
    let mut last_ptt = false;  // 用于检测 PTT 松开瞬间，发送释放帧

    // 帧同步期间的字节缓冲（最多 15 字节：帧头 3 + payload 12，第 16 字节触发完成）
    let mut pending = [0u8; 16];
    let mut pending_len: usize = 0;

    log::info!("[上行] 中继线程启动（帧级缓冲模式）");

    loop {
        match uart_panel.read(&mut buf, 10) {  // 10 tick ≈ 100ms（100Hz FreeRTOS），降低 override 响应延迟
            Ok(1) => {
                heartbeat_ticks = 0;  // 有面板数据 → 重置心跳计数
                let byte = buf[0];
                let prev_state = parser.state;
                let complete = parser.feed(byte);

                if complete {
                    // ===== 帧完成：修改 + 转发 =====
                    count = count.wrapping_add(1);
                    let mut frame = parser.get_frame();
                    apply_overrides(&mut frame, &state);
                    let _ = uart_host.write(&frame);
                    pending_len = 0;

                    // 诊断日志 + 状态更新
                    parser.log_diag(count);
                    let mut s = state.lock().unwrap();
                    parser.apply_to_state(&mut s);
                } else if parser.state > 0 {
                    // ===== 帧同步中：缓冲，不转发 =====
                    if pending_len < 16 {
                        pending[pending_len] = byte;
                        pending_len += 1;
                    }
                } else {
                    // ===== 未在帧中 =====
                    if pending_len > 0 {
                        // 同步刚失败 → 补发之前缓冲的字节（不丢数据）
                        let _ = uart_host.write(&pending[..pending_len]);
                        pending_len = 0;
                    }
                    // 转发当前字节
                    let _ = uart_host.write(&[byte]);
                }
            }
            _ => {
                // ===== 超时：优先级：key → key_release → knob → PTT → VOL/SQL → 心跳 =====
                let mut s = state.lock().unwrap();

                if let Some(key) = s.key_override.take() {
                    // PC 按键注入（一次性）
                    drop(s);
                    let frame = build_key_frame(key);
                    let _ = uart_host.write(&frame);
                    heartbeat_ticks = 0;
                } else if s.key_release {
                    // PC 按键松开（一次性）
                    s.key_release = false;
                    drop(s);
                    let frame = build_knob_frame(0xFF);
                    let _ = uart_host.write(&frame);
                    heartbeat_ticks = 0;
                } else if let Some(step) = s.knob_inject.take() {
                    // PC 旋钮注入（一次性）
                    drop(s);
                    let frame = build_knob_frame(step);
                    let _ = uart_host.write(&frame);
                    heartbeat_ticks = 0;
                } else if s.ptt_override || (last_ptt && !s.ptt_override) {
                    // PTT 持续注入：每 ~100ms 发一帧保持 PTT；PTT 松开瞬间发释放帧
                    let ptt_on = s.ptt_override;
                    drop(s);
                    let mut frame = build_knob_frame(0xFF);
                    frame[4] = if ptt_on { 0x00 } else { 0xFF };  // P[1]: PTT 按下/松开
                    let mut sum: u8 = 0x0C;
                    for i in 3..15 { sum ^= frame[i]; }
                    frame[15] = sum;
                    let _ = uart_host.write(&frame);
                    last_ptt = ptt_on;
                    heartbeat_ticks = 0;  // PTT 期间抑制心跳，防止 CW/CCW 干扰发射
                } else if s.vol_changed {
                    // VOL 注入：根据 MAIN 侧选边
                    let vol = s.vol_override;
                    let is_right = s.right.is_main;
                    s.vol_changed = false;
                    drop(s);
                    if let Some(v) = vol {
                        let frame = build_vol_sql_frame(Some(v), None, is_right);
                        let _ = uart_host.write(&frame);
                        let mut s2 = state.lock().unwrap();
                        let band = if is_right { &mut s2.right } else { &mut s2.left };
                        band.vol = v;
                        s2.head_count = s2.head_count.wrapping_add(1); // 触发 UI 刷新
                    }
                    heartbeat_ticks = 0;
                } else if s.sql_changed {
                    let sql = s.sql_override;
                    let is_right = s.right.is_main;
                    s.sql_changed = false;
                    drop(s);
                    if let Some(s_val) = sql {
                        let frame = build_vol_sql_frame(None, Some(s_val), is_right);
                        let _ = uart_host.write(&frame);
                        let mut s2 = state.lock().unwrap();
                        let band = if is_right { &mut s2.right } else { &mut s2.left };
                        band.sql = s_val;
                        s2.head_count = s2.head_count.wrapping_add(1); // 触发 UI 刷新
                    }
                    heartbeat_ticks = 0;
                } else {
                    drop(s);
                    // 正常心跳逻辑（约 10 秒间隔：100 次 × 100ms = 10s）
                    heartbeat_ticks += 1;

                if heartbeat_ticks >= 100 {
                    heartbeat_ticks = 0;

                    // ===== MAIN 位置探测（一次性，启动后首次 radio_alive 时触发）=====
                    // 发送一次 P1 键，让 TH-9800 发送 CmdID=0x14 上报真实 MAIN 位置
                    // 上位机进入监听模式时也会发送一次 P1，两次合计净效果为零（MAIN还原）
                    let should_probe = {
                        let s = state.lock().unwrap();
                        s.radio_alive && !s.main_probed
                            && !s.left.is_main && !s.right.is_main
                            && !s.left.is_tx && !s.right.is_tx
                    };

                    if should_probe {
                        state.lock().unwrap().main_probed = true; // 立即标记防重入
                        log::info!("[MAIN探测] MAIN 位置未知，发送 P1 键触发 CmdID=0x14");

                        // P1 按下
                        let _ = uart_host.write(&build_key_frame(0x10));
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        // P1 松开（空闲帧）
                        let _ = uart_host.write(&build_knob_frame(0xFF));
                        std::thread::sleep(std::time::Duration::from_millis(350));
                        // TH-9800 已发送 CmdID=0x14，SharedState.is_main 已更新
                        log::info!("[MAIN探测] 完成，等待 STATE_REPORT 同步到上位机");
                        // 跳过本次心跳，下次 100 ticks 后恢复
                        continue;
                    }

                    // 读取状态：MAIN 侧 + 安全检查
                    let (safe, use_right) = {
                        let s = state.lock().unwrap();
                        let use_right = if s.right.is_main {
                            false  // RIGHT=MAIN → 步进 LEFT
                        } else if s.left.is_main {
                            true   // LEFT=MAIN → 步进 RIGHT
                        } else {
                            false  // MAIN 未知 → 默认步进 LEFT
                        };
                        let side = if use_right { &s.right } else { &s.left };
                        let safe = side.s_level == 0 && !side.is_tx && !side.is_busy;
                        (safe, use_right)
                    };

                    if safe {
                        let (cw_step, ccw_step) = if use_right {
                            (0x82u8, 0x81u8)
                        } else {
                            (0x02u8, 0x01u8)
                        };

                        let cw_frame   = build_knob_frame(cw_step);
                        let ccw_frame  = build_knob_frame(ccw_step);
                        let idle_frame = build_knob_frame(0xFF);

                        let (sm_on, sm_off) = if use_right {
                            (&SM_R_ON, &SM_R_OFF)
                        } else {
                            (&SM_L_ON, &SM_L_OFF)
                        };

                        let _ = uart_host.write(&idle_frame);
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let _ = uart_host.write(&cw_frame);
                        let _ = uart_panel.write(sm_on);
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        let _ = uart_host.write(&ccw_frame);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        let _ = uart_panel.write(sm_off);
                        let _ = uart_host.write(&idle_frame);

                        log::info!("[上行] → 心跳 side={} sum={:02X}",
                            if use_right {"右"} else {"左"}, cw_frame[15]);
                    } else {
                        log::info!("[上行] → 心跳跳过 (不安全)");
                    }
                }
                } // else（正常心跳）
            }
        }
    }
}
