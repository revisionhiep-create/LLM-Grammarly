use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_MODEL_URL: &str = "https://huggingface.co/HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive/resolve/main/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf";
pub const CPU_STRONG_MODEL_URL: &str = "https://huggingface.co/HauhauCS/Qwen3.5-2B-Uncensored-HauhauCS-Aggressive/resolve/main/Qwen3.5-2B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf";
pub const CPU_WEAK_MODEL_URL: &str = "https://huggingface.co/LiquidAI/LFM2.5-1.2B-Instruct-GGUF/resolve/main/LFM2.5-1.2B-Instruct-Q4_K_M.gguf";
pub const DEFAULT_BACKUP_KEEP_COUNT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputeMode {
    #[default]
    Auto,
    Cpu,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPreset {
    pub label: &'static str,
    pub summary: &'static str,
    pub tooltip: &'static str,
    pub min_width: f32,
    pub url: &'static str,
}

pub const MODEL_PRESETS: [ModelPreset; 3] = [
    ModelPreset {
        label: "GPU",
        summary: "Qwen3.5-4B uncensored, ~2.5 GB",
        tooltip: "Qwen3.5-4B Uncensored Q4_K_M (~2.52 GB) — Default model, recommended for GPUs",
        min_width: 56.0,
        url: DEFAULT_MODEL_URL,
    },
    ModelPreset {
        label: "CPU Strong",
        summary: "Qwen3.5-2B uncensored, ~1.2 GB",
        tooltip: "Qwen3.5-2B Uncensored Q4_K_M (~1.21 GB) — Fast 2B uncensored model for modern CPUs / Intel iGPUs",
        min_width: 88.0,
        url: CPU_STRONG_MODEL_URL,
    },
    ModelPreset {
        label: "CPU Weak",
        summary: "LFM2.5-1.2B-Instruct, ~697 MB",
        tooltip: "LiquidAI LFM2.5-1.2B-Instruct Q4_K_M (~697 MB) — Lightweight hybrid model for older or low-RAM CPUs",
        min_width: 82.0,
        url: CPU_WEAK_MODEL_URL,
    },
];

pub fn preset_for_url(normalized_url: Option<&str>) -> Option<&'static ModelPreset> {
    let url = normalized_url?;
    MODEL_PRESETS.iter().find(|p| p.url == url)
}

pub fn preset_by_key(key: &str) -> Option<&'static ModelPreset> {
    match key.trim().to_ascii_lowercase().as_str() {
        "gpu" => Some(&MODEL_PRESETS[0]),
        "cpu-strong" | "cpu_strong" => Some(&MODEL_PRESETS[1]),
        "cpu-weak" | "cpu_weak" => Some(&MODEL_PRESETS[2]),
        _ => None,
    }
}

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
            shift: true,
            win: false,
            vk: 0x47, // Ctrl + Shift + G (avoids MacBook media key F8)
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
            parts.push("Option".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.win {
            parts.push("Cmd".to_string());
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
    pub compute_mode: ComputeMode,
    pub hotkey: HotkeyBinding,
    pub fix_highlighted_only: bool,
    pub toggle_power_hotkey: Option<HotkeyBinding>,
    pub toggle_highlight_hotkey: Option<HotkeyBinding>,
    pub minimize_to_tray_on_close: bool,
    pub show_notifications: bool,
    pub dark_mode: bool,
    pub window: Option<WindowState>,
    pub backup_dir: Option<String>,
    pub backup_keep_count: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model_url: DEFAULT_MODEL_URL.to_string(),
            model_dir: None,
            compute_mode: ComputeMode::Auto,
            hotkey: HotkeyBinding::default(),
            fix_highlighted_only: false,
            toggle_power_hotkey: None,
            toggle_highlight_hotkey: None,
            minimize_to_tray_on_close: false,
            show_notifications: true,
            dark_mode: true,
            window: None,
            backup_dir: None,
            backup_keep_count: DEFAULT_BACKUP_KEEP_COUNT,
        }
    }
}

pub fn config_file_path() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "LLM Grammarly") {
        dirs.config_dir().join("settings.json")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("LLM-Grammarly")
            .join("settings.json")
    } else {
        PathBuf::from("settings.json")
    }
}

fn legacy_config_file_path() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "keyboard-llm") {
        dirs.config_dir().join("settings.json")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("keyboard-llm")
            .join("settings.json")
    } else {
        PathBuf::from("settings.json")
    }
}

/// Default model storage folder in macOS Application Support.
pub fn default_model_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "LLM Grammarly") {
        dirs.data_local_dir().join("Models")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("LLM-Grammarly")
            .join("Models")
    } else {
        PathBuf::from("Models")
    }
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

pub fn default_backup_dir() -> PathBuf {
    fallback_backup_dir()
}

pub fn fallback_backup_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "LLM Grammarly") {
        dirs.data_local_dir().join("Backups")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("LLM-Grammarly")
            .join("Backups")
    } else {
        PathBuf::from("Backups")
    }
}

pub fn load_config() -> AppConfig {
    let path = config_file_path();
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            let legacy = legacy_config_file_path();
            let Ok(s) = fs::read_to_string(&legacy) else {
                return AppConfig::default();
            };
            s
        }
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
    cfg.backup_keep_count = cfg.backup_keep_count.clamp(1, 500);
    cfg
}

pub fn save_config_to_path(path: &Path, cfg: &AppConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(cfg)?;
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp_path = PathBuf::from(tmp_os);

    {
        let mut tmp_file = File::create(&tmp_path)
            .with_context(|| format!("Failed to create temp settings {}", tmp_path.display()))?;
        tmp_file
            .write_all(json.as_bytes())
            .and_then(|()| tmp_file.sync_all())
            .with_context(|| format!("Failed to write temp settings {}", tmp_path.display()))?;
    }

    if fs::rename(&tmp_path, path).is_err() {
        let _ = fs::remove_file(&tmp_path);
        fs::write(path, json.as_bytes())
            .with_context(|| format!("Failed to write settings to {}", path.display()))?;
    }
    Ok(())
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let path = config_file_path();
    save_config_to_path(&path, cfg)
}

/// Returns the `.part` staging path alongside `dest` without replacing the `.gguf` extension.
pub fn partial_download_path(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
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
        let d1 = Path::new("Models");
        assert_eq!(local_model_path(d1, u1), d1.join("model.gguf"));
        assert_eq!(display_model_name(u1), "model.gguf");

        let mut cfg = AppConfig::default();
        assert_eq!(effective_model_dir(&cfg), default_model_dir());
        cfg.model_dir = Some(" /tmp/LLMModels ".to_string());
        assert_eq!(effective_model_dir(&cfg), PathBuf::from("/tmp/LLMModels"));
    }

    #[test]
    fn hotkey_binding_validation_and_display() {
        let def = HotkeyBinding::default();
        assert!(def.is_valid());
        assert_eq!(def.display_label(), "Ctrl + Shift + G");

        let combo = HotkeyBinding {
            ctrl: true,
            alt: true,
            shift: false,
            win: true,
            vk: 0x47,
        };
        assert!(combo.is_valid());
        assert_eq!(combo.display_label(), "Ctrl + Option + Cmd + G");

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
            vk: 0x70, // F1
        };
        assert!(bare_fn.is_valid());
        assert_eq!(bare_fn.display_label(), "F1");
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

    #[test]
    fn notifications_defaults_true_and_roundtrips() {
        let def = AppConfig::default();
        assert!(def.show_notifications);

        let legacy_json =
            r#"{"model_url":"https://huggingface.co/a/b/resolve/main/m.gguf"}"#;
        let decoded_legacy: AppConfig = serde_json::from_str(legacy_json).unwrap();
        assert!(decoded_legacy.show_notifications);

        for enabled in [false, true] {
            let mut cfg = AppConfig::default();
            cfg.show_notifications = enabled;
            let encoded = serde_json::to_string(&cfg).unwrap();
            let decoded: AppConfig = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.show_notifications, enabled);
        }
    }

    #[test]
    fn model_presets_are_valid_and_distinct() {
        let mut filenames = std::collections::HashSet::new();
        for preset in MODEL_PRESETS {
            let norm = normalize_model_url(preset.url).unwrap();
            assert_eq!(norm, preset.url);
            assert!(filenames.insert(display_model_name(&norm)));
            assert_eq!(preset_for_url(Some(&norm)), Some(&preset));

            let blob_url = preset.url.replace("/resolve/main/", "/blob/main/");
            let norm_blob = normalize_model_url(&blob_url).unwrap();
            assert_eq!(preset_for_url(Some(&norm_blob)), Some(&preset));
        }
        assert_eq!(filenames.len(), MODEL_PRESETS.len());
        assert_eq!(
            preset_for_url(Some("https://huggingface.co/some/other/resolve/main/model.gguf")),
            None
        );
        assert_eq!(preset_for_url(None), None);
        assert_eq!(preset_by_key("gpu"), Some(&MODEL_PRESETS[0]));
        assert_eq!(preset_by_key("CPU-STRONG"), Some(&MODEL_PRESETS[1]));
        assert_eq!(preset_by_key("cpu_weak"), Some(&MODEL_PRESETS[2]));
        assert_eq!(preset_by_key("unknown"), None);
    }

    #[test]
    fn compute_mode_defaults_auto_and_roundtrips() {
        let def = AppConfig::default();
        assert_eq!(def.compute_mode, ComputeMode::Auto);

        let legacy_json =
            r#"{"model_url":"https://huggingface.co/a/b/resolve/main/m.gguf"}"#;
        let decoded_legacy: AppConfig = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(decoded_legacy.compute_mode, ComputeMode::Auto);

        for mode in [ComputeMode::Auto, ComputeMode::Cpu] {
            let mut cfg = AppConfig::default();
            cfg.compute_mode = mode;
            let encoded = serde_json::to_string(&cfg).unwrap();
            let decoded: AppConfig = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.compute_mode, mode);
        }
    }

    #[test]
    fn partial_download_path_appends_part_without_replacing_extension() {
        let dest = Path::new("C:\\Models\\model.v1.gguf");
        assert_eq!(
            partial_download_path(dest),
            PathBuf::from("C:\\Models\\model.v1.gguf.part")
        );
        let no_ext = Path::new("C:\\Models\\weights");
        assert_eq!(
            partial_download_path(no_ext),
            PathBuf::from("C:\\Models\\weights.part")
        );
    }

    #[test]
    fn atomic_save_config_writes_cleanly_without_leaving_tmp() {
        let dir = std::env::temp_dir().join(format!("kbllm_cfg_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");

        let mut cfg = AppConfig::default();
        cfg.compute_mode = ComputeMode::Cpu;
        save_config_to_path(&path, &cfg).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let decoded: AppConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(decoded.compute_mode, ComputeMode::Cpu);
        assert!(!dir.join("settings.json.tmp").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_path_uses_llm_grammarly_folder() {
        let path = config_file_path();
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some("settings.json")
        );
        let suffix = Path::new("LLM-Grammarly").join("settings.json");
        assert!(
            path.ends_with(&suffix),
            "Expected config path to end with {:?}, got: {}",
            suffix,
            path.display()
        );
    }

    #[test]
    fn backup_config_defaults_and_roundtrips() {
        let def = AppConfig::default();
        assert!(def.backup_dir.is_none());
        assert_eq!(def.backup_keep_count, DEFAULT_BACKUP_KEEP_COUNT);

        let legacy_json =
            r#"{"model_url":"https://huggingface.co/a/b/resolve/main/m.gguf"}"#;
        let decoded_legacy: AppConfig = serde_json::from_str(legacy_json).unwrap();
        assert!(decoded_legacy.backup_dir.is_none());
        assert_eq!(decoded_legacy.backup_keep_count, DEFAULT_BACKUP_KEEP_COUNT);

        let mut cfg = AppConfig::default();
        cfg.backup_dir = Some("D:\\Backups".to_string());
        cfg.backup_keep_count = 7;
        let encoded = serde_json::to_string(&cfg).unwrap();
        let decoded: AppConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.backup_dir.as_deref(), Some("D:\\Backups"));
        assert_eq!(decoded.backup_keep_count, 7);
    }
}
