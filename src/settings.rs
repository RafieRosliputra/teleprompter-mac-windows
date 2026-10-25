//! Settings saved as JSON in the per-user app data folder
//! (macOS: ~/Library/Application Support/JohorStudio Teleprompter,
//!  Windows: %APPDATA%\JohorStudio Teleprompter).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub font_size: f32,
    pub line_spacing: f32,
    pub margin_left: f32,
    pub margin_right: f32,
    pub align: TextAlign,
    /// Reading line position, fraction of screen height from the top.
    pub read_line: f32,
    pub flip_h: bool,
    pub flip_v: bool,
    pub show_arrows: bool,
    pub arrow_size: f32,
    pub monitor: usize,
    pub fullscreen: bool,
    pub main_font_size: f32,
    pub server_url: String,
    pub admin_token: String,
    /// Speed level that Space starts scrolling at.
    pub default_speed: i32,
    /// Main-window script uses the exact same layout as screen 2
    /// (same line breaks, margins, alignment), scaled to fit.
    pub match_main: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 80.0,
            line_spacing: 1.25,
            margin_left: 0.08,
            margin_right: 0.08,
            align: TextAlign::Left,
            read_line: 1.0 / 3.0,
            flip_h: true,
            flip_v: false,
            show_arrows: true,
            arrow_size: 40.0,
            monitor: 1,
            fullscreen: true,
            main_font_size: 22.0,
            server_url: "https://tele.johorstudio.com".into(),
            admin_token: String::new(),
            default_speed: 6,
            match_main: false,
        }
    }
}

pub fn dir() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("JohorStudio Teleprompter")
}

impl Settings {
    pub fn load() -> Self {
        let mut s: Settings = std::fs::read_to_string(dir().join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if s.admin_token.trim().is_empty() {
            if let Ok(t) = std::fs::read_to_string(dir().join("admin_token.txt")) {
                s.admin_token = t.trim().to_string();
            }
        }
        s
    }

    pub fn save(&self) {
        let d = dir();
        let _ = std::fs::create_dir_all(&d);
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(d.join("settings.json"), t);
        }
    }
}

pub fn load_script() -> Option<String> {
    std::fs::read_to_string(dir().join("last_script.txt")).ok()
}

pub fn save_script(s: &str) {
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    let _ = std::fs::write(d.join("last_script.txt"), s);
}
