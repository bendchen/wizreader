//! 应用设置（FR-09）持久化于 ~/.wizreader/settings.json

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub data_dir: Option<String>,
    #[serde(default = "default_font_size")]
    pub font_size: u32,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_read_width")]
    pub read_width: u32,
    #[serde(default)]
    pub allow_remote: bool,
}

fn default_font_size() -> u32 {
    16
}
fn default_theme() -> String {
    "system".into()
}
fn default_read_width() -> u32 {
    860
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data_dir: None,
            font_size: default_font_size(),
            theme: default_theme(),
            read_width: default_read_width(),
            allow_remote: false,
        }
    }
}

pub fn wiz_home() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".wizreader")
}

pub fn settings_path() -> PathBuf {
    wiz_home().join("settings.json")
}

pub fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(s: &Settings) -> Result<(), String> {
    let dir = wiz_home();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(settings_path(), json).map_err(|e| e.to_string())
}
