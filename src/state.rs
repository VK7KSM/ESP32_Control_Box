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
    // SET 菜单模式
    pub is_set: bool,              // SET 菜单模式激活
    pub menu_text: String<12>,     // 菜单文本（Len=09 非频率稳定帧，协议原始文本）
    pub menu_in_value: bool,       // true=已进入设置项（正在调整值），false=顶级菜单滚动
    pub menu_exit_count: u8,       // 连续频率帧计数，≥2 时退出菜单模式（防止 CTCSS 显示模式下频率/文本交替导致反复清空）
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

/// 电台整体状态
#[derive(Clone)]
pub struct RadioState {
    pub left: BandState,
    pub right: BandState,
    pub radio_alive: bool,
    pub pc_alive: bool,         // PC 连接状态（通过上位机心跳检测）
    pub body_count: u32,        // 下行帧计数
    pub head_count: u32,        // 上行帧计数
    // ===== PC 通信相关 =====
    pub pc_count: u32,          // PC 命令帧计数（诊断用）
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
}

impl RadioState {
    pub fn new() -> Self {
        Self {
            left:  BandState::new("LEFT"),
            right: BandState::new("RIGHT"),
            radio_alive: false,
            pc_alive: false,
            body_count: 0,
            head_count: 0,
            pc_count: 0,
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
        }
    }
}

pub type SharedState = std::sync::Arc<std::sync::Mutex<RadioState>>;

pub fn new_shared_state() -> SharedState {
    std::sync::Arc::new(std::sync::Mutex::new(RadioState::new()))
}
