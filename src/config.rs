use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_MODEL_URL: &str = "https://huggingface.co/HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive/resolve/main/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyBinding {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u32,
}

impl Default for HotkeyBinding {
    fn default() -> Self {
        Self {
            ctrl: true,
            alt: false,
            shift: false,
            win: false,
            vk: 0x51, // 'Q'
        }
    }
}

impl HotkeyBinding {
    pub fn is_valid(&self) -> bool {
        if vk_to_name(self.vk).is_none() {
            return false;
        }
        let is_function_key = (0x70..=0x7B).contains(&self.vk);
        is_function_key || self.ctrl || self.alt || self.shift || self.win
    }

    pub fn display_label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.win {
            parts.push("Win".to_string());
        }
        let key_name = vk_to_name(self.vk).unwrap_or_else(|| format!("0x{:02X}", self.vk));
        parts.push(key_name);
        parts.join(" + ")
    }
}

pub fn vk_to_name(vk: u32) -> Option<String> {
    match vk {
        0x41..=0x5A => Some(((vk as u8) as char).to_string()),
        0x30..=0x39 => Some(((vk as u8) as char).to_string()),
        0x70..=0x7B => Some(format!("F{}", vk - 0x70 + 1)),
        0x20 => Some("Space".to_string()),
        0x09 => Some("Tab".to_string()),
        0x2D => Some("Insert".to_string()),
        0x2E => Some("Delete".to_string()),
        0x24 => Some("Home".to_string()),
        0x23 => Some("End".to_string()),
        0x21 => Some("PageUp".to_string()),
        0x22 => Some("PageDown".to_string()),
        0x25 => Some("Left".to_string()),
        0x26 => Some("Up".to_string()),
        0x27 => Some("Right".to_string()),
        0x28 => Some("Down".to_string()),
        0xBD => Some("-".to_string()),
        0xBB => Some("=".to_string()),
        0xDB => Some("[".to_string()),
        0xDD => Some("]".to_string()),
        0xBA => Some(";".to_string()),
        0xDE => Some("'".to_string()),
        0xBC => Some(",".to_string()),
        0xBE => Some(".".to_string()),
        0xBF => Some("/".to_string()),
        0xC0 => Some("`".to_string()),
        0xDC => Some("\\".to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub model_url: String,
    pub model_dir: Option<String>,
    pub hotkey: HotkeyBinding,
    pub fix_highlighted_only: bool,
    pub toggle_power_hotkey: Option<HotkeyBinding>,
    pub toggle_highlight_hotkey: Option<HotkeyBinding>,
    pub minimize_to_tray_on_close: bool,
    pub dark_mode: bool,
    pub window: Option<WindowState>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model_url: DEFAULT_MODEL_URL.to_string(),
            model_dir: None,
            hotkey: HotkeyBinding::default(),
            fix_highlighted_only: false,
            toggle_power_hotkey: None,
            toggle_highlight_hotkey: None,
            minimize_to_tray_on_close: false,
            dark_mode: true,
            window: None,
        }
    }
}

pub fn config_file_path() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "keyboard-llm") {
        dirs.config_dir().join("settings.json")
    } else if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata).join("keyboard-llm").join("settings.json")
    } else {
        PathBuf::from("settings.json")
    }
}

/// Default model storage folder: the folder containing the running `.exe`.
pub fn default_model_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn effective_model_dir(cfg: &AppConfig) -> PathBuf {
    if let Some(dir) = cfg.model_dir.as_deref() {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    default_model_dir()
}

pub fn load_config() -> AppConfig {
    let path = config_file_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        return AppConfig::default();
    };
    let mut cfg: AppConfig = serde_json::from_str(&raw).unwrap_or_default();
    if cfg.model_url.trim().is_empty() {
        cfg.model_url = DEFAULT_MODEL_URL.to_string();
    }
    if !cfg.hotkey.is_valid() {
        cfg.hotkey = HotkeyBinding::default();
    }
    if let Some(hk) = cfg.toggle_power_hotkey {
        if !hk.is_valid() || hk == cfg.hotkey {
            cfg.toggle_power_hotkey = None;
        }
    }
    if let Some(hk) = cfg.toggle_highlight_hotkey {
        if !hk.is_valid() || hk == cfg.hotkey || Some(hk) == cfg.toggle_power_hotkey {
            cfg.toggle_highlight_hotkey = None;
        }
    }
    cfg
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(cfg)?;
    fs::write(&path, json)
        .with_context(|| format!("Failed to write settings to {}", path.display()))?;
    Ok(())
}

/// Validates and normalizes a model download URL.
/// Converts Hugging Face `/blob/` links automatically into `/resolve/` direct download links.
pub fn normalize_model_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Model download link cannot be empty");
    }
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        bail!("Model download link must start with https:// or http://");
    }

    let mut normalized = trimmed.to_string();
    if let Some(after_scheme) = normalized
        .strip_prefix("https://")
        .or_else(|| normalized.strip_prefix("http://"))
    {
        let host = after_scheme.split('/').next().unwrap_or("");
        if host.eq_ignore_ascii_case("huggingface.co")
            || host.to_ascii_lowercase().ends_with(".huggingface.co")
        {
            if let Some(idx) = normalized.find("/blob/") {
                normalized.replace_range(idx..idx + "/blob/".len(), "/resolve/");
            }
        }
    }

    Ok(normalized)
}

fn raw_url_filename(normalized_url: &str) -> String {
    let without_fragment = normalized_url.split('#').next().unwrap_or(normalized_url);
    let without_query = without_fragment.split('?').next().unwrap_or(without_fragment);
    let segment = without_query
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("model.gguf");

    let sanitized: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "model.gguf".to_string()
    } else if sanitized.to_ascii_lowercase().ends_with(".gguf") {
        sanitized
    } else {
        format!("{sanitized}.gguf")
    }
}

/// Returns the local GGUF file path inside the active model storage folder.
pub fn local_model_path(storage_dir: &Path, normalized_url: &str) -> PathBuf {
    storage_dir.join(raw_url_filename(normalized_url))
}

/// Returns the human-friendly model filename.
pub fn display_model_name(normalized_url: &str) -> String {
    raw_url_filename(normalized_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hf_blob_links_and_rejects_invalid() {
        let blob = "  https://huggingface.co/HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive/blob/main/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf?download=true  ";
        let norm = normalize_model_url(blob).unwrap();
        assert_eq!(
            norm,
            "https://huggingface.co/HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive/resolve/main/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf?download=true"
        );
        assert!(normalize_model_url("").is_err());
        assert!(normalize_model_url("ftp://example.com/a.gguf").is_err());
    }

    #[test]
    fn builds_local_model_path_inside_requested_storage_dir() {
        let u1 = "https://huggingface.co/a/repo/resolve/main/model.gguf";
        let d1 = Path::new("C:\\Models");
        assert_eq!(local_model_path(d1, u1), PathBuf::from("C:\\Models\\model.gguf"));
        assert_eq!(display_model_name(u1), "model.gguf");

        let mut cfg = AppConfig::default();
        assert_eq!(effective_model_dir(&cfg), default_model_dir());
        cfg.model_dir = Some(" D:\\LLMModels ".to_string());
        assert_eq!(effective_model_dir(&cfg), PathBuf::from("D:\\LLMModels"));
    }

    #[test]
    fn hotkey_binding_validation_and_display() {
        let def = HotkeyBinding::default();
        assert!(def.is_valid());
        assert_eq!(def.display_label(), "Ctrl + Q");

        let combo = HotkeyBinding {
            ctrl: true,
            alt: false,
            shift: true,
            win: false,
            vk: 0x47,
        };
        assert!(combo.is_valid());
        assert_eq!(combo.display_label(), "Ctrl + Shift + G");

        let bare_letter = HotkeyBinding {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            vk: 0x41,
        };
        assert!(!bare_letter.is_valid());

        let bare_fn = HotkeyBinding {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            vk: 0x77, // F8
        };
        assert!(bare_fn.is_valid());
        assert_eq!(bare_fn.display_label(), "F8");
    }

    #[test]
    fn window_state_roundtrip_and_backwards_compat() {
        let legacy_json = r#"{"model_url":"https://huggingface.co/a/b/resolve/main/m.gguf","minimize_to_tray_on_close":true}"#;
        let legacy_cfg: AppConfig = serde_json::from_str(legacy_json).unwrap();
        assert!(legacy_cfg.window.is_none());
        assert!(legacy_cfg.minimize_to_tray_on_close);

        let mut cfg = AppConfig::default();
        cfg.window = Some(WindowState {
            x: 1920,
            y: 120,
            width: 500.0,
            height: 510.0,
            maximized: false,
        });
        let encoded = serde_json::to_string(&cfg).unwrap();
        let decoded: AppConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.window, cfg.window);
    }

    #[test]
    fn dark_mode_defaults_true_and_roundtrips() {
        let def = AppConfig::default();
        assert!(def.dark_mode);

        let missing_field_json =
            r#"{"model_url":"https://huggingface.co/a/b/resolve/main/m.gguf"}"#;
        let decoded_missing: AppConfig = serde_json::from_str(missing_field_json).unwrap();
        assert!(decoded_missing.dark_mode);

        for mode in [false, true] {
            let mut cfg = AppConfig::default();
            cfg.dark_mode = mode;
            let encoded = serde_json::to_string(&cfg).unwrap();
            let decoded: AppConfig = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.dark_mode, mode);
        }
    }

    #[test]
    fn highlight_mode_and_aux_hotkeys_defaults_and_roundtrip() {
        let def = AppConfig::default();
        assert!(!def.fix_highlighted_only);
        assert!(def.toggle_power_hotkey.is_none());
        assert!(def.toggle_highlight_hotkey.is_none());

        let legacy_json =
            r#"{"model_url":"https://huggingface.co/a/b/resolve/main/m.gguf"}"#;
        let decoded_legacy: AppConfig = serde_json::from_str(legacy_json).unwrap();
        assert!(!decoded_legacy.fix_highlighted_only);
        assert!(decoded_legacy.toggle_power_hotkey.is_none());
        assert!(decoded_legacy.toggle_highlight_hotkey.is_none());

        let mut cfg = AppConfig::default();
        cfg.fix_highlighted_only = true;
        cfg.toggle_power_hotkey = Some(HotkeyBinding {
            ctrl: true,
            alt: true,
            shift: false,
            win: false,
            vk: 0x50, // Ctrl + Alt + P
        });
        cfg.toggle_highlight_hotkey = Some(HotkeyBinding {
            ctrl: true,
            alt: false,
            shift: true,
            win: false,
            vk: 0x48, // Ctrl + Shift + H
        });

        let encoded = serde_json::to_string(&cfg).unwrap();
        let decoded: AppConfig = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.fix_highlighted_only);
        assert_eq!(decoded.toggle_power_hotkey, cfg.toggle_power_hotkey);
        assert_eq!(decoded.toggle_highlight_hotkey, cfg.toggle_highlight_hotkey);
    }
}
