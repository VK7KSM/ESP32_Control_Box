// ===================================================================
// TTS 模块 — Microsoft Edge TTS（msedge-tts 0.2.x，同步接口）
// 合成文字为 MP3，保存到 recordings/，无需 tokio 异步运行时
// ===================================================================

use std::path::PathBuf;
use msedge_tts::tts::SpeechConfig;
use msedge_tts::tts::client::connect;

pub const DEFAULT_VOICE: &str = "zh-TW-HsiaoChenNeural";
const AUDIO_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";

/// 获取 recordings 目录路径（与可执行文件同级，自动创建）
pub fn recordings_dir() -> PathBuf {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("recordings")
}

/// 合成文字为 MP3，保存到 recordings/YYYYMMDD_HHMMSS_tts.mp3，返回文件路径
pub fn synthesize(text: &str, voice: &str) -> Result<PathBuf, String> {
    // 1. 确保目录存在
    let dir = recordings_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建 recordings 目录失败: {}", e))?;

    // 2. 生成文件路径（时间戳命名）
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let path = dir.join(format!("{}_tts.mp3", ts));

    // 3. 连接 Edge TTS（同步 TCP，需要网络）
    let mut client = connect()
        .map_err(|e| format!("连接 Edge TTS 服务失败: {}", e))?;

    // 4. 构造语音配置
    let config = SpeechConfig {
        voice_name:   voice.to_string(),
        audio_format: AUDIO_FORMAT.to_string(),
        pitch:        0,
        rate:         0,
        volume:       100,
    };

    // 5. 合成
    let audio = client.synthesize(text, &config)
        .map_err(|e| format!("TTS 合成失败: {}", e))?;

    // 6. 写入 MP3 文件
    std::fs::write(&path, &audio.audio_bytes)
        .map_err(|e| format!("写入音频文件失败: {}", e))?;

    Ok(path)
}
