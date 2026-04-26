// ===================================================================
// 共享状态：电台数据结构（BandState / RadioState / SharedState）
// ===================================================================

use heapless::String;

/// 功率等级
#[derive(Clone, Copy, PartialEq)]
pub enum PowerLevel {
    High,   // 无 L/M 标志
    Mid,    // M 亮
    Low,    // L 亮
}

/// 单个波段的状态
#[derive(Clone)]
pub struct BandState {
    pub label: &'static str,
    pub is_main: bool,
    pub freq: String<12>,        // 频率字符串，如 "438.500"
    pub mode: String<4>,         // "FM" / "AM"
    pub power: PowerLevel,
    pub power_confirmed: bool,        // 是否已收到 CmdID=0x15 确认功率（开机时机身不主动发送）
    pub s_level: u32,            // 0~9 信号强度 / S 表
    pub vol: u16,                // 16-bit ADC 原始值 (0~1023)
    pub sql: u16,                // 16-bit ADC 原始值 (0~1023)
    pub is_tx: bool,
    pub channel: String<8>,      // "VFO" / "Ch:012" 等
    // 亚音状态（来自 CmdID=18/19/20）
    pub tone_enc: bool,          // CmdID=0x19: CTCSS 编码器
    pub tone_dec: bool,          // CmdID=0x18: CTCSS 解码器
    pub tone_dcs: bool,          // CmdID=0x20: DCS
    pub tone_type: String<4>,    // "ENC"/"T/R"/"DCS"/"" (由上面三个推导)
    pub tone_freq: String<8>,    // "88.5"/"023"/"" (暂不解析，留空)
    // 中继偏移（来自 CmdID=16/17）
    pub shift_plus: bool,        // CmdID=0x16: + 正偏移
    pub shift_minus: bool,       // CmdID=0x17: - 负偏移
    pub shift: String<8>,        // "+Shft"/"-Shft"/"" (由上面两个推导)
    // 其他状态
    pub is_busy: bool,
    pub is_skip: bool,
    pub is_mute: bool,
    pub is_lock: bool,
    pub is_mt: bool,
    pub is_pref: bool,
    // SET 菜单模式（仅协议状态，不参与频率区镜像显示）
    pub is_set: bool,
    pub menu_text: String<12>,
    pub menu_in_value: bool,
    pub menu_exit_count: u8,
    // 机头频率区当前非数字文本（Len=09 原始文本）；为空时显示 freq
    pub display_text: String<12>,
}

impl BandState {
    pub fn new(label: &'static str) -> Self {
        let mut s = Self {
            label,
            is_main: false,
            freq: String::new(),
            mode: String::new(),
            power: PowerLevel::High,
            power_confirmed: false,
            s_level: 0,
            vol: 0,
            sql: 0,
            is_tx: false,
            channel: String::new(),
            tone_enc: false,
            tone_dec: false,
            tone_dcs: false,
            tone_type: String::new(),
            tone_freq: String::new(),
            shift_plus: false,
            shift_minus: false,
            shift: String::new(),
            is_busy: false,
            is_skip: false,
            is_mute: false,
            is_lock: false,
            is_mt: false,
            is_pref: false,
            is_set: false,
            menu_text: String::new(),
            menu_in_value: false,
            menu_exit_count: 0,
            display_text: String::new(),
        };
        let _ = s.freq.push_str("---.---");
        let _ = s.mode.push_str("FM");
        let _ = s.channel.push_str("VFO");
        s
    }

    /// 音量百分比 (0~100)，基于实测 ADC 范围 20~960
    pub fn vol_pct(&self) -> u32 {
        if self.vol <= 20 { return 0; }
        let v = (self.vol as u32).min(960);
        (v - 20) * 100 / 940
    }

    /// 静噪百分比 (0~100)，基于实测 ADC 范围 20~1000
    pub fn sql_pct(&self) -> u32 {
        if self.sql <= 20 { return 0; }
        let v = (self.sql as u32).min(1000);
        (v - 20) * 100 / 980
    }

    /// 根据 tone_enc/dec/dcs 重新生成 tone_type 字符串
    pub fn refresh_tone_type(&mut self) {
        self.tone_type.clear();
        if self.tone_dcs {
            let _ = self.tone_type.push_str("DCS");
        } else if self.tone_enc && self.tone_dec {
            let _ = self.tone_type.push_str("T/R");
        } else if self.tone_enc {
            let _ = self.tone_type.push_str("ENC");
        } else if self.tone_dec {
            let _ = self.tone_type.push_str("DEC");
        }
    }

    /// 根据 shift_plus/minus 重新生成 shift 字符串
    pub fn refresh_shift(&mut self) {
        self.shift.clear();
        if self.shift_plus {
            let _ = self.shift.push_str("+Shft");
        } else if self.shift_minus {
            let _ = self.shift.push_str("-Shft");
        }
    }
}

/// WiFi 连接状态（用于屏幕左下角显示）
#[derive(Clone, Debug, PartialEq)]
pub enum WifiState {
    Disabled,        // sdkconfig 关闭或用户禁用
    NoCredentials,   // NVS 中无 SSID/PSK，等待 USB 配网
    Connecting,      // 正在连接 / 重试
    Connected,       // 已连上，IP 存在 wifi_ip 字段
    Failed,          // 多次重试失败
}

/// 扫描到的 AP 简要信息
#[derive(Clone, Debug)]
pub struct WifiAp {
    pub ssid: heapless::String<32>,
    pub rssi: i8,
    pub auth: u8,    // 0=Open, 1=WEP, 2=WPA, 3=WPA2, 4=WPAWPA2, 5=WPA2Enterprise, 6=WPA3, 7=WPA2WPA3
}

/// 电台整体状态
#[derive(Clone)]
pub struct RadioState {
    pub left: BandState,
    pub right: BandState,
    pub radio_alive: bool,
    pub pc_alive: bool,         // PC 连接状态（通过上位机心跳检测）
    // ===== WiFi 状态（仅 UI 显示用，rigctld/CRC16-TCP 服务器自行判断 is_connected） =====
    pub wifi_state: WifiState,
    pub wifi_ip:    heapless::String<16>,  // "192.168.1.42" 形式
    // ===== WiFi 扫描（PC 配网用）=====
    pub scan_request: bool,                       // PC 请求扫描，wifi 线程消费
    pub scanning:     bool,                       // wifi 线程正在扫描
    pub scan_results: heapless::Vec<WifiAp, 16>,  // 最近一次扫描结果
    pub scan_seq:     u32,                        // 每次扫描完成自增，pc_comm 检测变化推送给 PC
    pub body_count: u32,        // 下行帧计数
    pub head_count: u32,        // 上行帧计数
    // ===== PC 通信相关 =====
    pub pc_count: u32,          // PC 命令帧计数（诊断用）
    pub pc_last_hb_us: u64,     // 最近一次任意通道收到 CMD_HEARTBEAT 的时间（USB / TCP 共享）
    pub macro_running: bool,    // 宏正在执行
    // ===== 上行帧 override（由 PC 设置，relay_up_thread 执行替换）=====
    pub vol_override: Option<u16>,  // Some(adc) = 替换音量 ADC，None = 使用物理旋钮
    pub sql_override: Option<u16>,  // Some(adc) = 替换静噪 ADC，None = 使用物理旋钮
    pub ptt_override: bool,         // true = 强制 PTT 按下
    pub ptt_start_us: u64,          // PTT override 开始时间（esp_timer_get_time 微秒）
    pub key_override: Option<u8>,   // Some(keycode) = 一次性按键注入（发送后自动清除）
    pub key_release: bool,          // true = 一次性发送松开帧（发送后自动清除）
    pub knob_inject: Option<u8>,    // Some(step) = 一次性旋钮注入（发送后自动清除）
    pub vol_changed: bool,          // PC 刚设置新音量，需注入一帧（面板空闲时 apply_overrides 不触发）
    pub sql_changed: bool,          // PC 刚设置新静噪，同上
    pub main_probed: bool,          // 已执行过 MAIN 探测（启动后一次性触发，防重入）
    // ===== rigctld set_freq 异步步进目标 =====
    pub rigctld_target_hz: Option<u64>,
    // ===== rigctld 连接状态 =====
    pub rigctld_clients: u32,       // 当前活跃 rigctld 客户端数（>0 → IP 显示橙色）
    pub rigctld_ctcss_tone: u32,    // 最后设置的 CTCSS 频率（0.1 Hz 单位，0=OFF）
    pub rigctld_initial_freq_done: bool, // DTrac 首个频率已写入电台
    pub rigctld_step_ready: bool,        // TH-9800 STEP 已设为 2.5kHz
    pub rigctld_setup_running: bool,     // rigctld 初始设置线程正在运行
    pub rigctld_last_step_us: u64,       // 最近一次 DTrac 追踪旋钮注入时间
}

impl RadioState {
    pub fn new() -> Self {
        Self {
            left:  BandState::new("LEFT"),
            right: BandState::new("RIGHT"),
            radio_alive: false,
            pc_alive: false,
            wifi_state: WifiState::NoCredentials,
            wifi_ip:    heapless::String::new(),
            scan_request: false,
            scanning:     false,
            scan_results: heapless::Vec::new(),
            scan_seq:     0,
            body_count: 0,
            head_count: 0,
            pc_count: 0,
            pc_last_hb_us: 0,
            macro_running: false,
            vol_override: None,
            sql_override: None,
            ptt_override: false,
            ptt_start_us: 0,
            key_override: None,
            key_release: false,
            knob_inject: None,
            vol_changed: false,
            sql_changed: false,
            main_probed: false,
            rigctld_target_hz: None,
            rigctld_clients: 0,
            rigctld_ctcss_tone: 0,
            rigctld_initial_freq_done: false,
            rigctld_step_ready: false,
            rigctld_setup_running: false,
            rigctld_last_step_us: 0,
        }
    }
}

pub type SharedState = std::sync::Arc<std::sync::Mutex<RadioState>>;

pub fn new_shared_state() -> SharedState {
    std::sync::Arc::new(std::sync::Mutex::new(RadioState::new()))
}
