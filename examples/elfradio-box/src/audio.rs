// ===================================================================
// 音频模块（cpal + hound）
// RX 监听 + BUSY 触发录音 + PTT 发射 + DeepFilterNet 实时降噪
// 统一 48kHz 采样率（DeepFilterNet 降噪要求）
// ===================================================================

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU32, Ordering}};
use std::time::Duration;

const TARGET_SAMPLE_RATE: u32 = 48000;
const SILENCE_THRESHOLD: f32 = 0.01;  // 音频电平阈值
const PASSTHROUGH_BUF_MAX: usize = 96000; // 2s @48kHz

/// 查找名称包含关键词的音频设备
pub fn find_device_by_name(host: &cpal::Host, name_hint: &str, is_input: bool) -> Option<cpal::Device> {
    let devices = if is_input {
        host.input_devices().ok()?
    } else {
        host.output_devices().ok()?
    };

    for dev in devices {
        if let Ok(n) = dev.name() {
            if n.contains(name_hint) {
                return Some(dev);
            }
        }
    }
    None
}

/// 查找用户端输出设备（PC 扬声器/耳机）
/// 优先系统默认输出，但排除 CM108("USB Audio") 和虚拟线("CABLE")
pub fn find_user_output_device(host: &cpal::Host) -> Option<cpal::Device> {
    let excluded = ["USB Audio", "CABLE"];
    if let Some(dev) = host.default_output_device() {
        if let Ok(name) = dev.name() {
            if !excluded.iter().any(|e| name.contains(e)) {
                return Some(dev);
            }
        }
    }
    host.output_devices().ok()?.find(|d| {
        d.name().map(|n| !excluded.iter().any(|e| n.contains(e))).unwrap_or(false)
    })
}

/// 查找用户端输入设备（PC 麦克风）
/// 优先系统默认输入，但排除 CM108("USB Audio") 和虚拟线("CABLE")
pub fn find_user_input_device(host: &cpal::Host) -> Option<cpal::Device> {
    let excluded = ["USB Audio", "CABLE"];
    if let Some(dev) = host.default_input_device() {
        if let Ok(name) = dev.name() {
            if !excluded.iter().any(|e| name.contains(e)) {
                return Some(dev);
            }
        }
    }
    host.input_devices().ok()?.find(|d| {
        d.name().map(|n| !excluded.iter().any(|e| n.contains(e))).unwrap_or(false)
    })
}

/// 列出所有音频设备
pub fn list_audio_devices() -> Vec<(String, bool)> {
    let host = cpal::default_host();
    let mut result = Vec::new();

    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                result.push((format!("[输入] {}", name), true));
            }
        }
    }
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                result.push((format!("[输出] {}", name), false));
            }
        }
    }
    result
}

/// RX 监听器：从指定输入设备（USB Audio Mic）采集音频
pub struct RxMonitor {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<f32>>>,
    is_recording: Arc<AtomicBool>,
    level: Arc<Mutex<f32>>,
    sample_rate: u32,
    // 实时直通：CM108 Input → 滤波线程 → PC 扬声器/耳机
    passthrough_buf: Arc<Mutex<VecDeque<f32>>>,
    filtered_buf:    Arc<Mutex<VecDeque<f32>>>,   // 滤波后缓冲（输出回调读此缓冲）
    _passthrough_stream: Option<cpal::Stream>,
    passthrough_muted: Arc<AtomicBool>,
    // DeepFilterNet 降噪控制
    atten_lim_bits: Arc<AtomicU32>,   // f32::to_bits()，0.0=旁路
    filter_stop:    Arc<AtomicBool>,  // Drop 时置位，通知滤波线程退出
    _filter_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for RxMonitor {
    fn drop(&mut self) {
        self.filter_stop.store(true, Ordering::Relaxed);
    }
}

impl RxMonitor {
    /// 创建 RX 监听器（从 USB Audio 麦克风采集）
    pub fn new(device: &cpal::Device) -> Result<Self, String> {
        let config = device.default_input_config()
            .map_err(|e| format!("获取输入配置失败: {}", e))?;

        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let buffer = Arc::new(Mutex::new(Vec::<f32>::new()));
        let is_recording = Arc::new(AtomicBool::new(false));
        let level = Arc::new(Mutex::new(0.0f32));
        let passthrough_buf: Arc<Mutex<VecDeque<f32>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(PASSTHROUGH_BUF_MAX)));
        let filtered_buf: Arc<Mutex<VecDeque<f32>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(PASSTHROUGH_BUF_MAX)));
        let passthrough_muted = Arc::new(AtomicBool::new(false));
        let atten_lim_bits    = Arc::new(AtomicU32::new(0u32)); // 0.0f32.to_bits() == 0
        let filter_stop       = Arc::new(AtomicBool::new(false));

        let buf_clone  = buffer.clone();
        let rec_clone  = is_recording.clone();
        let lvl_clone  = level.clone();
        let pt_buf     = passthrough_buf.clone();

        let stream = device.build_input_stream(
            &config.into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut sum = 0.0f32;
                let mut count = 0usize;
                for chunk in data.chunks(channels) {
                    let sample = chunk[0];
                    sum += sample * sample;
                    count += 1;
                    // 写入直通缓冲（由滤波线程消费后写入 filtered_buf）
                    let mut pt = pt_buf.lock().unwrap();
                    pt.push_back(sample);
                    if pt.len() > PASSTHROUGH_BUF_MAX { pt.pop_front(); }
                }
                let rms = if count > 0 { (sum / count as f32).sqrt() } else { 0.0 };
                *lvl_clone.lock().unwrap() = rms;

                if rec_clone.load(Ordering::Relaxed) {
                    let mut buf = buf_clone.lock().unwrap();
                    for chunk in data.chunks(channels) {
                        buf.push(chunk[0]);
                    }
                }
            },
            |err| {
                eprintln!("[音频错误] RX: {}", err);
            },
            None,
        ).map_err(|e| format!("创建输入流失败: {}", e))?;

        stream.play().map_err(|e| format!("启动输入流失败: {}", e))?;

        // ── 启动 DeepFilterNet 滤波线程 ──────────────────────────────
        // passthrough_buf → [滤波线程] → filtered_buf
        // 旁路（atten_lim_db=0）时直接转发，无模型推理开销
        let ft_pt_buf     = passthrough_buf.clone();
        let ft_filt_buf   = filtered_buf.clone();
        let ft_atten      = atten_lim_bits.clone();
        let ft_stop       = filter_stop.clone();
        let _filter_thread = Some(std::thread::spawn(move || {
            // 初始化 RNNoise 降噪状态
            let mut model = init_df_model();

            loop {
                if ft_stop.load(Ordering::Relaxed) { break; }

                match model {
                    Some(ref mut state_box) => {
                        let state = state_box.as_mut();
                        const HOP: usize = nnnoiseless::DenoiseState::FRAME_SIZE; // 480
                        // 等待 passthrough_buf 积累一帧
                        let samples = {
                            let mut buf = ft_pt_buf.lock().unwrap();
                            if buf.len() < HOP {
                                drop(buf);
                                std::thread::sleep(Duration::from_millis(2));
                                continue;
                            }
                            buf.drain(..HOP).collect::<Vec<f32>>()
                        };

                        let db = f32::from_bits(ft_atten.load(Ordering::Relaxed));
                        let out_samples = if db > 0.1 {
                            // RNNoise 推理（输入输出均为 480 个 f32）
                            let mut input  = [0.0f32; nnnoiseless::DenoiseState::FRAME_SIZE];
                            let mut output = [0.0f32; nnnoiseless::DenoiseState::FRAME_SIZE];
                            for (i, s) in samples.iter().enumerate() { input[i] = *s * 32768.0; }
                            state.process_frame(&mut output, &input);
                            // 根据强度混合原始和降噪信号（db=100 全降噪，db=10 轻微）
                            let mix = (db / 100.0).clamp(0.0, 1.0);
                            samples.iter().enumerate()
                                .map(|(i, &orig)| {
                                    let denoised = output[i] / 32768.0;
                                    orig * (1.0 - mix) + denoised * mix
                                })
                                .collect::<Vec<f32>>()
                        } else {
                            // 旁路：直接转发
                            samples
                        };

                        let mut fbuf = ft_filt_buf.lock().unwrap();
                        if fbuf.len() + HOP > PASSTHROUGH_BUF_MAX {
                            fbuf.drain(..HOP); // 防积压，丢弃旧帧
                        }
                        for s in out_samples { fbuf.push_back(s); }
                    }
                    None => {
                        // 模型初始化失败：直接将 passthrough_buf 转发到 filtered_buf
                        let samples = {
                            let mut buf = ft_pt_buf.lock().unwrap();
                            if buf.is_empty() {
                                drop(buf);
                                std::thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            buf.drain(..).collect::<Vec<f32>>()
                        };
                        let mut fbuf = ft_filt_buf.lock().unwrap();
                        for s in samples {
                            if fbuf.len() >= PASSTHROUGH_BUF_MAX { fbuf.pop_front(); }
                            fbuf.push_back(s);
                        }
                    }
                }
            }
        }));

        // ── 直通输出流（读 filtered_buf）──────────────────────────────
        let pt_buf_out = filtered_buf.clone();
        let pt_muted   = passthrough_muted.clone();
        let _passthrough_stream = (|| -> Option<cpal::Stream> {
            let host = cpal::default_host();
            let out_dev = find_user_output_device(&host)?;
            let out_cfg = out_dev.default_output_config().ok()?;
            let out_rate     = out_cfg.sample_rate().0;
            let out_channels = out_cfg.channels() as usize;
            // 分数步进重采样（usb_in_rate → user_out_rate）
            let ratio = sample_rate as f64 / out_rate as f64;
            let mut frac = 0.0f64;
            let stream = out_dev.build_output_stream(
                &out_cfg.into(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let muted = pt_muted.load(Ordering::Relaxed);
                    let mut pt = pt_buf_out.lock().unwrap();
                    for frame in data.chunks_mut(out_channels.max(1)) {
                        let sample = if muted {
                            frac += ratio;
                            if frac >= 1.0 { frac -= 1.0; pt.pop_front(); }
                            0.0
                        } else {
                            frac += ratio;
                            if frac >= 1.0 {
                                frac -= 1.0;
                                pt.pop_front().unwrap_or(0.0)
                            } else {
                                pt.front().copied().unwrap_or(0.0)
                            }
                        };
                        for ch in frame.iter_mut() { *ch = sample; }
                    }
                },
                |e| eprintln!("[音频错误] RX Passthrough: {}", e),
                None,
            ).ok()?;
            stream.play().ok()?;
            Some(stream)
        })();

        Ok(Self {
            _stream: stream,
            buffer,
            is_recording,
            level,
            sample_rate,
            passthrough_buf,
            filtered_buf,
            _passthrough_stream,
            passthrough_muted,
            atten_lim_bits,
            filter_stop,
            _filter_thread,
        })
    }

    /// 获取当前音频电平 (0.0~1.0)
    pub fn level(&self) -> f32 {
        *self.level.lock().unwrap()
    }

    /// 开始录音
    pub fn start_recording(&self) {
        self.buffer.lock().unwrap().clear();
        self.is_recording.store(true, Ordering::Relaxed);
    }

    /// 停止录音并返回采样数据
    pub fn stop_recording(&self) -> Vec<f32> {
        self.is_recording.store(false, Ordering::Relaxed);
        let data = std::mem::take(&mut *self.buffer.lock().unwrap());
        data
    }

    /// 是否正在录音
    pub fn is_recording(&self) -> bool {
        self.is_recording.load(Ordering::Relaxed)
    }

    /// 设备采样率
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// PTT 激活时调用 — 静音直通输出，防止 PC 扬声器被 PC 麦克风拾取形成回路
    pub fn mute_passthrough(&self) {
        self.passthrough_muted.store(true, Ordering::Relaxed);
    }

    /// PTT 释放时调用 — 恢复直通输出
    pub fn unmute_passthrough(&self) {
        self.passthrough_muted.store(false, Ordering::Relaxed);
    }

    /// 设置降噪强度（0.0 = 关闭/旁路，10.0-100.0 = 开启）
    pub fn set_denoise_db(&self, db: f32) {
        self.atten_lim_bits.store(db.to_bits(), Ordering::Relaxed);
    }
}

/// 初始化 RNNoise 降噪状态（始终成功，返回 Some(Box<DenoiseState>)）
fn init_df_model() -> Option<Box<nnnoiseless::DenoiseState<'static>>> {
    Some(nnnoiseless::DenoiseState::new())
}

/// 保存 f32 采样为 48kHz WAV 文件
pub fn save_wav_48k(samples: &[f32], src_rate: u32, path: &str) -> Result<(), String> {
    // 确保父目录存在（如 recordings/ 目录）
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败: {}", e))?;
        }
    }

    // 如果需要重采样到 48kHz
    let resampled: Vec<f32>;
    let final_samples = if src_rate != TARGET_SAMPLE_RATE {
        resampled = resample(samples, src_rate, TARGET_SAMPLE_RATE);
        &resampled
    } else {
        samples
    };

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| format!("创建 WAV 失败: {}", e))?;

    for &s in final_samples {
        let i16_val = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        writer.write_sample(i16_val)
            .map_err(|e| format!("写入采样失败: {}", e))?;
    }

    writer.finalize()
        .map_err(|e| format!("完成 WAV 失败: {}", e))?;
    Ok(())
}

/// PTT TX 音频路由：PC 麦克风（非 USB Audio）→ CM108 Output → 电台 PIN6
/// 创建时立即开始路由，Drop 时自动停止
pub struct TxMicCapture {
    _input_stream: cpal::Stream,
    _output_stream: cpal::Stream,
}

impl TxMicCapture {
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();

        // PC 用户麦克风（排除 CM108 和 CABLE）
        let mic_dev = find_user_input_device(&host)
            .ok_or_else(|| "未找到用户麦克风（已排除 USB Audio/CABLE）".to_string())?;

        // CM108 输出 → 电台 PIN6（麦克风输入）
        let usb_out = find_device_by_name(&host, "USB Audio", false)
            .ok_or_else(|| "未找到 CM108 USB Audio 输出设备".to_string())?;

        let mic_cfg = mic_dev.default_input_config()
            .map_err(|e| format!("麦克风配置失败: {e}"))?;
        let usb_cfg = usb_out.default_output_config()
            .map_err(|e| format!("CM108 输出配置失败: {e}"))?;

        let mic_channels = mic_cfg.channels() as usize;
        let mic_rate     = mic_cfg.sample_rate().0;
        let usb_rate     = usb_cfg.sample_rate().0;
        let usb_channels = usb_cfg.channels() as usize;

        // 共享环形缓冲（PC 麦克风 → CM108 输出）
        let buf: Arc<Mutex<VecDeque<f32>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(PASSTHROUGH_BUF_MAX)));
        let buf_in  = buf.clone();
        let buf_out = buf.clone();

        // PC 麦克风 → 缓冲
        let input_stream = mic_dev.build_input_stream(
            &mic_cfg.into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut b = buf_in.lock().unwrap();
                for chunk in data.chunks(mic_channels) {
                    b.push_back(chunk[0]); // mono，取第一声道
                }
                while b.len() > PASSTHROUGH_BUF_MAX { b.pop_front(); }
            },
            |e| eprintln!("[TxMic 输入错误] {e}"),
            None,
        ).map_err(|e| format!("麦克风流创建失败: {e}"))?;

        // 缓冲 → CM108 输出（分数步进重采样：mic_rate → usb_rate）
        let ratio = mic_rate as f64 / usb_rate as f64;
        let mut frac = 0.0f64;
        let output_stream = usb_out.build_output_stream(
            &usb_cfg.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut b = buf_out.lock().unwrap();
                for frame in data.chunks_mut(usb_channels.max(1)) {
                    frac += ratio;
                    let s = if frac >= 1.0 {
                        frac -= 1.0;
                        b.pop_front().unwrap_or(0.0)
                    } else {
                        b.front().copied().unwrap_or(0.0)
                    };
                    for ch in frame.iter_mut() { *ch = s; }
                }
            },
            |e| eprintln!("[TxMic 输出错误] {e}"),
            None,
        ).map_err(|e| format!("CM108 输出流创建失败: {e}"))?;

        input_stream.play().map_err(|e| format!("麦克风流启动失败: {e}"))?;
        output_stream.play().map_err(|e| format!("CM108 输出流启动失败: {e}"))?;

        Ok(Self { _input_stream: input_stream, _output_stream: output_stream })
    }
}

/// 播放 WAV 文件到 USB Audio 输出设备（用于 PTT 发射）
/// 内部委托给 play_audio_file_to_cm108，现已支持任意格式 + 30s截断
pub fn play_wav_to_usb(path: &str) -> Result<Duration, String> {
    play_audio_file_to_cm108(path, 30, Arc::new(AtomicBool::new(false)))
}

/// 解码任意格式音频文件 → (单声道 f32 样本, 源采样率)
/// max_secs：样本级截断（不依赖超时），支持 WAV/MP3/OGG/FLAC/AAC 等
fn decode_audio_file(path: &str, max_secs: u64) -> Result<(Vec<f32>, u32), String> {
    use std::io::BufReader;
    use rodio::Decoder;
    use rodio::Source;  // 提供 sample_rate(), channels() 方法

    let file = std::fs::File::open(path)
        .map_err(|e| format!("打开文件失败: {}", e))?;
    let source = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("解码失败（不支持的格式）: {}", e))?;

    let src_rate = source.sample_rate();
    let channels = source.channels() as usize;

    // 样本级截断：最多取 max_secs 秒的帧
    let max_frames = src_rate as usize * max_secs as usize;

    // 解码为 f32（i16 → f32 归一化），截断到 max_secs
    let all: Vec<f32> = source
        .take(max_frames * channels)
        .map(|s| s as f32 / 32768.0)
        .collect();

    // 多声道混合为单声道（平均所有声道）
    let mono: Vec<f32> = if channels > 1 {
        all.chunks(channels)
           .map(|c| c.iter().sum::<f32>() / channels as f32)
           .collect()
    } else {
        all
    };

    Ok((mono, src_rate))
}

/// 播放任意格式音频文件 → CM108 USB Audio Output → 电台 PIN6（麦克风输入）
/// max_secs: 最长发射秒数（含样本级截断，硬限30s以匹配ESP32看门狗）
/// stop_flag: 外部停止信号，设为 true 时立即停止
/// 返回实际播放时长
pub fn play_audio_file_to_cm108(
    path: &str,
    max_secs: u64,
    stop_flag: Arc<AtomicBool>,
) -> Result<Duration, String> {
    let (mono_samples, src_rate) = decode_audio_file(path, max_secs)?;

    // 找 CM108 USB Audio 输出设备
    let host = cpal::default_host();
    let device = find_device_by_name(&host, "USB Audio", false)
        .ok_or_else(|| "未找到 CM108 USB Audio 输出设备（请检查声卡连接）".to_string())?;

    let out_cfg = device.default_output_config()
        .map_err(|e| format!("获取输出配置失败: {}", e))?;
    let out_rate     = out_cfg.sample_rate().0;
    let out_channels = out_cfg.channels() as usize;

    // 重采样到 CM108 原生采样率
    let play_samples = resample(&mono_samples, src_rate, out_rate);
    let play_duration = Duration::from_secs_f64(
        play_samples.len() as f64 / out_rate as f64);

    // 共享播放状态
    let buf  = Arc::new(Mutex::new(play_samples));
    let pos  = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let (buf2, pos2, done2) = (buf.clone(), pos.clone(), done.clone());

    // 构建输出流（回调从缓冲区读取样本）
    let stream = device.build_output_stream(
        &out_cfg.into(),
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let samples = buf2.lock().unwrap();
            let cur    = pos2.load(Ordering::Relaxed);
            let frames = data.len() / out_channels.max(1);
            for (i, frame) in data.chunks_mut(out_channels.max(1)).enumerate() {
                let s = samples.get(cur + i).copied().unwrap_or(0.0);
                for ch in frame.iter_mut() { *ch = s; }
            }
            let new_pos = (cur + frames).min(samples.len());
            pos2.store(new_pos, Ordering::Relaxed);
            if new_pos >= samples.len() { done2.store(true, Ordering::Relaxed); }
        },
        |e| eprintln!("[文件发射输出错误] {e}"),
        None,
    ).map_err(|e| format!("创建输出流失败: {}", e))?;

    stream.play().map_err(|e| format!("启动输出流失败: {}", e))?;

    // 阻塞等待：3重保险（done + stop_flag + deadline）
    let deadline = std::time::Instant::now() + play_duration + Duration::from_secs(1);
    while !done.load(Ordering::Relaxed) {
        if stop_flag.load(Ordering::Relaxed) { break; }
        if std::time::Instant::now() > deadline { break; }
        std::thread::sleep(Duration::from_millis(50));
    }

    let actual_dur = Duration::from_secs_f64(
        pos.load(Ordering::Relaxed) as f64 / out_rate as f64);
    Ok(actual_dur)
}

/// 线性插值重采样
fn resample(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate || input.is_empty() {
        return input.to_vec();
    }

    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = ((input.len() as f64) / ratio).ceil() as usize;
    let mut output = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos as usize;
        let frac = (src_pos - idx as f64) as f32;

        let s0 = input[idx.min(input.len() - 1)];
        let s1 = input[(idx + 1).min(input.len() - 1)];
        output.push(s0 + (s1 - s0) * frac);
    }

    output
}
