// ===================================================================
// 配置文件读写（与可执行文件同目录的 elfradio-box.cfg）
// 格式：key=value 每行一条，不依赖外部 crate
// ===================================================================

use std::path::PathBuf;

pub struct AppConfig {
    pub tts_voice:  String,
    pub denoise_db: f32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            tts_voice:  crate::tts::DEFAULT_VOICE.to_string(),
            denoise_db: 0.0,
        }
    }
}

fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("elfradio-box.cfg")
}

pub fn load_config() -> AppConfig {
    let mut cfg = AppConfig::default();
    let path = config_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return cfg,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() { continue; }
        let mut parts = line.splitn(2, '=');
        let key   = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        match key {
            "tts_voice"  => cfg.tts_voice  = value.to_string(),
            "denoise_db" => cfg.denoise_db = value.parse().unwrap_or(0.0),
            _ => {}
        }
    }
    cfg
}

pub fn save_config(cfg: &AppConfig) -> Result<(), String> {
    let content = format!(
        "# elfRadio BOX 配置文件\ntts_voice={}\ndenoise_db={}\n",
        cfg.tts_voice, cfg.denoise_db
    );
    std::fs::write(config_path(), content)
        .map_err(|e| format!("保存配置失败: {}", e))
}
