// ===================================================================
// BLE (Bluetooth Low Energy) — 第二步：GATT 服务 + rigctld 透传
//
// 设计：
//   - 设备名 "elfRadio"，BLE 4.x 广播兼容 DTrac 蓝牙发现
//   - GATT Service UUID: 0xFFF0（与 DTrac 官网协议规范一致，BLE 模块通用 Nordic UART 风格）
//   - Characteristic 0xFFF2 (Write/WriteNoResponse): 接收 Hamlib rigctld 文本命令
//   - Characteristic 0xFFF1 (Notify): 发送 rigctld 响应字节（按 MTU 切片）
//   - 命令处理复用 src/rigctld.rs::dispatch（与 TCP 4532 路径完全一致）
//   - 行缓冲：Write 收到的字节追加到 buffer，遇 \n 提交一行命令
//
// 客户端断开后自动重启 5 分钟广播（用户场景：临时退出 DTrac 可快速重连）
// ===================================================================

use crate::rigctld::{
    begin_sat_session, capture_rx_side, clear_sat_session, command_name, dispatch,
    handler_session_is_current, is_stateful_dtrac_command, DispatchOut,
};
use crate::state::{set_connection_status_if_idle, SharedState};
use esp_idf_svc::sys::esp_timer_get_time;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use esp32_nimble::{
    enums::*,
    utilities::BleUuid,
    BLEAdvertisementData, BLEDevice, NimbleProperties,
};

/// 断开后是否需要重启广播。on_disconnect 只设此信号，主循环异步处理，
/// 避免在 NimBLE host task callback 内 sleep+start 阻塞协议栈
static SHOULD_RESTART_ADV: AtomicBool = AtomicBool::new(false);

/// 外部模块（如 button.rs 长按 Fix 3）请求强制断开所有 BLE 客户端
/// 主循环每 500ms 检查此信号，遍历 server.connections() 调 disconnect(conn_handle)
/// 与 SHOULD_RESTART_ADV 同样的 swap 模式（不在调用方等待，异步生效）
static FORCE_DISCONNECT_BLE: AtomicBool = AtomicBool::new(false);

/// 外部模块（如 button.rs 短按）请求重启 BLE 广播
/// 主循环每 500ms 检查 SHOULD_RESTART_ADV，等 100ms 后调 advertising.start()
pub fn request_advertising_restart() {
    SHOULD_RESTART_ADV.store(true, Ordering::Relaxed);
}

/// 外部模块（如 button.rs::power_toggle_worker Fix 3）请求强制断开所有 BLE 客户端
/// 主循环异步处理：≤500ms 检查间隔 + ble_gap_terminate ~100-200ms = ≤700ms 实际断开延迟
/// on_disconnect handler 会被 NimBLE 异步触发：减 ble_clients/rigctld_clients + 设 sat pending
pub fn force_disconnect_all_ble() {
    FORCE_DISCONNECT_BLE.store(true, Ordering::Relaxed);
    ::log::info!("[BLE] 请求强制断开所有 BLE 客户端");
}

/// BLE 设备名默认值（用户在 SoftAP 网页可改，启动时从 state.cfg_ble_name 读取）
const BLE_DEVICE_NAME_DEFAULT: &str = "elfRadio";

/// GATT Service UUID（DTrac 兼容，BLE 模块通用 Nordic UART 风格）
const SERVICE_UUID: u16 = 0xFFF0;
/// Notify characteristic UUID（ESP32 → 客户端发响应）
const NOTIFY_CHAR_UUID: u16 = 0xFFF1;
/// Write characteristic UUID（客户端 → ESP32 发命令）
const WRITE_CHAR_UUID: u16 = 0xFFF2;

/// 广播超时（5 分钟）
const ADVERTISING_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// 单次 Notify 最大 payload（BLE MTU 247 - 3 ATT header = 244）
const NOTIFY_CHUNK_SIZE: usize = 200;

/// 行缓冲最大长度（rigctld 命令通常 < 50 字节，给 512 余量）
const LINE_BUFFER_MAX: usize = 512;

/// 启动 BLE 后台线程
pub fn start_ble_thread(state: SharedState) {
    // 绑到 CPU 1，与 ESP-IDF NimBLE host task 同核；释放 CPU 0 给 UART/LCD/IDLE0
    let _ = esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
        pin_to_core: Some(esp_idf_svc::hal::cpu::Core::Core1),
        ..Default::default()
    }.set();
    std::thread::Builder::new()
        .name("ble".into())
        .stack_size(8192)
        .spawn(move || ble_main(state))
        .expect("BLE 线程启动失败");
}

fn ble_main(state: SharedState) {
    // 立即初始化 NimBLE 协议栈，先抢内部 SRAM。WiFi 任务在 wifi.rs 主循环开头延迟 3 秒，
    // 让 BLE controller (~30KB 内部 SRAM) 先分配，避免 WiFi 占走 RESERVE_INTERNAL 池后
    // BLE controller init malloc fail（"BLE_INIT: Malloc failed / controller init failed"）。
    ::log::info!("[BLE] 启动 NimBLE 协议栈...");

    let device = BLEDevice::take();

    // 配置广播功率（最大 +9dBm，提升手机扫描距离）
    if let Err(e) = device.set_power(PowerType::Default, PowerLevel::P9) {
        ::log::warn!("[BLE] 设置广播功率失败: {:?}", e);
    }

    // 不调用 set_auth / set_io_cap：vendor BLESecurity 是 lazy struct，
    // 不调 setter 就不写 ble_hs_cfg.sm_*，NimBLE 走 ESP-IDF 默认（无 SM 协商）。
    // Characteristic 无 ENC flag，连接时不会触发配对；ESP32 用 public address
    // (efuse MAC)，手机蓝牙列表保存的 elfRadio 用旧地址即可重连。
    // 这是修复"重启 ESP32 才能再连"的根因——SM 状态机断开后被卡住。

    let server = device.get_server();

    // ===== GATT 服务定义 =====
    let service = server.create_service(BleUuid::Uuid16(SERVICE_UUID));

    // Notify characteristic (0xFFF1) — ESP32 → 客户端发响应
    let notify_char = service.lock().create_characteristic(
        BleUuid::Uuid16(NOTIFY_CHAR_UUID),
        NimbleProperties::READ | NimbleProperties::NOTIFY,
    );
    notify_char.lock().set_value(b"");

    // 行缓冲：Write 收到字节追加到这里，遇 \n 提交命令
    let line_buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::with_capacity(256)));

    // BLE handler 的 gate state（与 TCP handle_client 第 600-612 行一致）：
    //   - rx_is_left_at_accept: BLE 客户端连接瞬间采样 MAIN 物理侧（作为 RX）
    //   - handler_session_id: 第一个 stateful 命令时调 begin_sat_session 创建
    let rx_is_left_at_accept: Arc<Mutex<bool>> = Arc::new(Mutex::new(true));
    let handler_session_id: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let connection_counted: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

    // Write characteristic (0xFFF2) — 客户端 → ESP32 发命令
    let write_char = service.lock().create_characteristic(
        BleUuid::Uuid16(WRITE_CHAR_UUID),
        NimbleProperties::WRITE | NimbleProperties::WRITE_NO_RSP,
    );
    {
        let buffer = line_buffer.clone();
        let notify = notify_char.clone();
        let state_w = state.clone();
        let rx_left = rx_is_left_at_accept.clone();
        let session = handler_session_id.clone();
        write_char.lock().on_write(move |args| {
            let recv_data = args.recv_data();
            let mut buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
            if buf.len() + recv_data.len() > LINE_BUFFER_MAX {
                // 溢出：清缓冲 + 丢弃当前 recv_data（避免立即再次溢出 / 死循环）
                // 等下一行完整数据到达再恢复正常处理
                ::log::warn!(
                    "[BLE] 行缓冲溢出（buf={} + recv={} > {}），清空并丢弃当前包",
                    buf.len(), recv_data.len(), LINE_BUFFER_MAX
                );
                buf.clear();
                return;
            }
            buf.extend_from_slice(recv_data);

            // 拆出所有完整行，逐行 gate + dispatch
            loop {
                let nl_pos = buf.iter().position(|&b| b == b'\n');
                let Some(pos) = nl_pos else { break; };

                let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                let line_str = String::from_utf8_lossy(&line_bytes);
                let line_trim = line_str.trim_end_matches(|c: char| c == '\n' || c == '\r');

                if line_trim.is_empty() {
                    continue;
                }

                // ===== Gate 逻辑（与 TCP handle_client 行 600-612 完全一致）=====
                let cmd_name = command_name(line_trim);
                let is_stateful = is_stateful_dtrac_command(line_trim);
                let mut session_guard = session.lock().unwrap_or_else(|e| e.into_inner());
                let rx_left_val = *rx_left.lock().unwrap_or_else(|e| e.into_inner());

                if let Some(id) = *session_guard {
                    // 已有 session：检查是否仍是当前 session
                    if !handler_session_is_current(&state_w, id) {
                        ::log::info!(
                            "[RigctldGate-BLE] cmd={:?} handler_session #{} 已过期，忽略",
                            line_trim, id
                        );
                        // BLE 不像 TCP 能 close socket，重置 session_id 让下个 stateful 命令重新 begin
                        *session_guard = None;
                        continue;
                    }
                } else if is_stateful {
                    // 第一个 stateful 命令 → 启动新 sat session
                    let session_id = begin_sat_session(&state_w, rx_left_val);
                    *session_guard = Some(session_id);
                    ::log::info!(
                        "[RigctldGate-BLE] cmd={:?} name={:?} 启动 SatSession #{}",
                        line_trim, cmd_name, session_id
                    );
                } else {
                    ::log::info!(
                        "[RigctldGate-BLE] cmd={:?} name={:?} 非 stateful，handler_session=None",
                        line_trim, cmd_name
                    );
                }
                drop(session_guard);

                // ===== 命令分发（与 TCP 路径一致）=====
                let reply = match dispatch(line_trim, &state_w) {
                    DispatchOut::Reply(s) => s,
                    DispatchOut::Quit => {
                        ::log::info!("[BLE] 客户端发送 quit 命令（BLE 端忽略，由客户端自行断开）");
                        continue;
                    }
                };

                if reply.is_empty() {
                    continue;
                }
                // 通过 Notify 发回响应（按 MTU 切片）
                // 分片间检查 ble_clients > 0：客户端断开时 break，避免 NimBLE 内部 spam
                // notify error 日志（节省 CPU 和日志带宽）
                let reply_bytes = reply.as_bytes();
                let mut offset = 0;
                while offset < reply_bytes.len() {
                    // 每分片前检查客户端是否还在
                    let still_connected = {
                        let s = state_w.lock().unwrap_or_else(|e| e.into_inner());
                        s.ble_clients > 0
                    };
                    if !still_connected {
                        ::log::info!("[BLE] 响应分片中客户端已断开，丢弃剩余 {} 字节",
                            reply_bytes.len() - offset);
                        break;
                    }
                    let end = (offset + NOTIFY_CHUNK_SIZE).min(reply_bytes.len());
                    notify.lock().set_value(&reply_bytes[offset..end]).notify();
                    offset = end;
                    if offset < reply_bytes.len() {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        });
    }

    // 客户端连接事件：更新 ble_clients + rigctld_clients + 采样 rx_is_left_at_accept（与 TCP 接受连接一致）
    // 关键：BLE 客户端走 rigctld 协议，必须计入 rigctld_clients，否则 freq_stepper 不会启动 setup
    let state_conn = state.clone();
    let rx_left_conn = rx_is_left_at_accept.clone();
    let session_conn = handler_session_id.clone();
    let counted_conn = connection_counted.clone();
    server.on_connect(move |server, conn_desc| {
        let conn_handle = conn_desc.conn_handle();
        let now_us = unsafe { esp_timer_get_time() } as u64;
        *counted_conn.lock().unwrap_or_else(|e| e.into_inner()) = false;
        let mut s = state_conn.lock().unwrap_or_else(|e| e.into_inner());
        if s.rigctld_clients > 0 {
            let owner_name = if s.ble_clients > 0 { "BLE" } else { "TCP" };
            drop(s);
            ::log::warn!(
                "[BLE] 拒绝客户端 {:?}：rigctld 已被 {} 占用",
                conn_desc.address(),
                owner_name
            );
            if let Err(e) = server.disconnect(conn_handle) {
                ::log::warn!("[BLE] 冲突断开 conn_handle={} 失败: {:?}", conn_handle, e);
            }
            return;
        }
        set_connection_status_if_idle(&mut s, "Rigctld BLE", now_us);
        let session_id = s.rigctld_session_id;
        let rx_left = capture_rx_side(&s, session_id);
        s.ble_clients = s.ble_clients.saturating_add(1);
        s.rigctld_clients = s.rigctld_clients.saturating_add(1); // BLE 也是 rigctld 协议客户端
        s.head_count = s.head_count.wrapping_add(1);
        drop(s);
        *counted_conn.lock().unwrap_or_else(|e| e.into_inner()) = true;
        *rx_left_conn.lock().unwrap_or_else(|e| e.into_inner()) = rx_left;
        *session_conn.lock().unwrap_or_else(|e| e.into_inner()) = None;
        ::log::info!(
            "[BLE] 客户端连接: addr={:?} RX采样={}（左=true）",
            conn_desc.address(),
            rx_left
        );
    });

    // 客户端断开事件：减少计数 + 清行缓冲 + 清 session_id + 设置异步重启广播信号
    // 注意：本回调在 NimBLE host task context 中执行，禁止 sleep / 持锁调 advertising.start()
    let state_disc = state.clone();
    let buffer_disc = line_buffer.clone();
    let session_disc = handler_session_id.clone();
    let counted_disc = connection_counted.clone();
    server.on_disconnect(move |conn_desc, reason| {
        ::log::info!("[BLE] 客户端断开: addr={:?} reason={:?}", conn_desc.address(), reason);

        // 先取本 handler 的 session_id 副本，再持有 state 锁（避免锁顺序问题）
        let was_session = *session_disc.lock().unwrap_or_else(|e| e.into_inner());
        let was_counted = {
            let mut counted = counted_disc.lock().unwrap_or_else(|e| e.into_inner());
            let v = *counted;
            *counted = false;
            v
        };

        if !was_counted {
            buffer_disc.lock().unwrap_or_else(|e| e.into_inner()).clear();
            *session_disc.lock().unwrap_or_else(|e| e.into_inner()) = None;
            SHOULD_RESTART_ADV.store(true, Ordering::Relaxed);
            return;
        }

        let mut s = state_disc.lock().unwrap_or_else(|e| e.into_inner());
        s.ble_clients = s.ble_clients.saturating_sub(1);
        s.rigctld_clients = s.rigctld_clients.saturating_sub(1); // 配套递减
        s.head_count = s.head_count.wrapping_add(1);

        // === 完整清理（与 TCP rigctld.rs:522-536 对齐）===
        // clear_sat_session 是纯字段操作（无 sleep / 网络 / UART），可在 NimBLE host task callback 中安全执行。
        // 副作用：设 sql_close_pending + tone_off_pending → freq_stepper 在 rigctld_clients=0 时
        // 跑 maintenance 注入帧 → TH-9800 回应下行帧 → radio_alive 持续 true → 屏幕不会被 15s 超时关掉。
        s.ptt_override = false;
        s.rigctld_target_hz = None;
        s.rigctld_setup_running = false;
        // BLE 单连接（sdkconfig MAX_CONNECTIONS=1）：handler_session 存在或全断开时都做完整 sat 清理
        if was_session.is_some() || s.rigctld_clients == 0 {
            clear_sat_session(&mut s);
        }
        drop(s);

        buffer_disc.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *session_disc.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // 设信号让主循环异步重启广播，不阻塞 NimBLE host task
        SHOULD_RESTART_ADV.store(true, Ordering::Relaxed);
    });

    // 广播配置：设备名 + Service UUID 0xFFF0
    // 设备名从 state.cfg_ble_name 读取（C4：网页可配置；空或未设置时用默认 BLE_DEVICE_NAME_DEFAULT）
    let device_name: heapless::String<16> = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.cfg_ble_name.is_empty() {
            let mut d: heapless::String<16> = heapless::String::new();
            let _ = d.push_str(BLE_DEVICE_NAME_DEFAULT);
            d
        } else {
            s.cfg_ble_name.clone()
        }
    };
    let advertising = device.get_advertising();
    {
        let mut adv = advertising.lock();
        if let Err(e) = adv.set_data(
            BLEAdvertisementData::new()
                .name(device_name.as_str())
                .add_service_uuid(BleUuid::Uuid16(SERVICE_UUID)),
        ) {
            ::log::warn!("[BLE] 设置广播数据失败: {:?}", e);
        }
    }

    ::log::info!(
        "[BLE] GATT 服务就绪: Service=0x{:04X} Write=0x{:04X} Notify=0x{:04X}",
        SERVICE_UUID, WRITE_CHAR_UUID, NOTIFY_CHAR_UUID
    );
    ::log::info!("[BLE] 设备名 '{}'，开始广播 (5 分钟超时)", device_name.as_str());

    // 主循环：广播 5 分钟 → 关闭 → 等待外部触发重启
    loop {
        // 启动广播
        if let Err(e) = advertising.lock().start() {
            ::log::warn!("[BLE] 启动广播失败: {:?}", e);
            std::thread::sleep(Duration::from_secs(10));
            continue;
        }
        // 进入新一轮广播，先吃掉可能残留的重启信号
        SHOULD_RESTART_ADV.store(false, Ordering::Relaxed);
        {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.ble_advertising = true;
            s.head_count = s.head_count.wrapping_add(1);
        }

        let mut started = std::time::Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(500));

            // Fix 3：异步处理外部"强制断开 BLE"信号
            // 触发场景：button.rs 长按关机时调 force_disconnect_all_ble，需要真断 socket
            // 让 rigctld_clients 减小，以便屏幕能正常息屏
            if FORCE_DISCONNECT_BLE.swap(false, Ordering::Relaxed) {
                let server = device.get_server();
                let conn_handles: Vec<u16> = server.connections().map(|c| c.conn_handle()).collect();
                if conn_handles.is_empty() {
                    ::log::info!("[BLE] 强制断开请求：当前无活跃连接");
                } else {
                    for h in conn_handles {
                        match server.disconnect(h) {
                            Ok(_)  => ::log::info!("[BLE] disconnect conn_handle={} OK", h),
                            Err(e) => ::log::warn!("[BLE] disconnect conn_handle={} 失败: {:?}", h, e),
                        }
                    }
                }
                // 不在此设置 SHOULD_RESTART_ADV：on_disconnect handler 会自己设
            }

            // 异步处理 on_disconnect 设的"重启广播"信号
            if SHOULD_RESTART_ADV.swap(false, Ordering::Relaxed) {
                // 等 100ms 让 NimBLE 释放 LL/连接 slot
                std::thread::sleep(Duration::from_millis(100));
                match advertising.lock().start() {
                    Ok(_) => ::log::info!("[BLE] 断开后已重启广播"),
                    Err(e) => ::log::warn!("[BLE] 断开后重启广播失败（可能仍在广播）: {:?}", e),
                }
                // 重置超时计时器：让用户在断开后仍有完整 5 分钟可重连
                started = std::time::Instant::now();
            }

            let connected = {
                let s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.ble_clients > 0
            };

            // 客户端在线时：等到断开再重新计时（避免被超时关闭）
            if connected {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }

            if started.elapsed() >= ADVERTISING_TIMEOUT {
                break;
            }
        }

        // 5 分钟超时，关广播
        if let Err(e) = advertising.lock().stop() {
            ::log::warn!("[BLE] 停止广播失败: {:?}", e);
        }
        {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.ble_advertising = false;
            s.head_count = s.head_count.wrapping_add(1);
        }
        ::log::info!("[BLE] 广播超时关闭，等待外部触发重启（如短按物理按钮）");

        // 真的等外部触发：每 500ms 检查 SHOULD_RESTART_ADV 信号
        // 短按物理按钮（button.rs::request_advertising_restart）/ on_disconnect 都会 set 此信号
        // 不再自动 60s 重启（之前的临时实现违反"等待外部触发"语义，导致广播在用户不知情下重启）
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if SHOULD_RESTART_ADV.swap(false, Ordering::Relaxed) {
                ::log::info!("[BLE] 收到外部重启信号，恢复广播 5 分钟窗口");
                break;
            }
        }
    }
}
