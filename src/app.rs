use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::{
    default_model_dir, display_model_name, effective_model_dir, load_config, local_model_path,
    normalize_model_url, save_config, AppConfig, HotkeyBinding, WindowState, DEFAULT_MODEL_URL,
};
use crate::downloader::{download_model, DownloadProgress};
use crate::hotkey::{HotkeyController, HotkeyEvent};
use crate::icon;
use crate::icon_render;
use crate::llm::{LlmHandle, LoadInfo};
use crate::taskbar::{
    get_window_placement_state, pick_folder_dialog, restore_window_position, ShellBridge,
    ShellCommand, TaskbarProgress,
};

const BG_WINDOW: egui::Color32 = egui::Color32::from_rgb(244, 243, 241); // #F4F3F1
const BG_CARD: egui::Color32 = egui::Color32::from_rgb(255, 255, 255); // #FFFFFF
const BG_INPUT: egui::Color32 = egui::Color32::from_rgb(250, 250, 249); // #FAFAF9
const BG_CALLOUT: egui::Color32 = egui::Color32::from_rgb(248, 247, 245); // #F8F7F5
const BG_KEYCAP: egui::Color32 = egui::Color32::from_rgb(242, 240, 236); // #F2F0EC
const BG_TRACK: egui::Color32 = egui::Color32::from_rgb(231, 229, 226); // #E7E5E2
const BORDER_SUBTLE: egui::Color32 = egui::Color32::from_rgb(229, 226, 220); // #E5E2DC
const BORDER_MED: egui::Color32 = egui::Color32::from_rgb(214, 211, 205); // #D6D3CD
const TEXT_PRIMARY: egui::Color32 = egui::Color32::from_rgb(31, 29, 27); // #1F1D1B
const TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(115, 110, 104); // #736E68
const BTN_PRIMARY_BG: egui::Color32 = egui::Color32::from_rgb(41, 38, 36); // #292624
const BTN_SEC_BG: egui::Color32 = egui::Color32::from_rgb(236, 234, 230); // #ECEAE6
const BTN_SEC_HOVER: egui::Color32 = egui::Color32::from_rgb(226, 223, 217); // #E2DFD9
const DOT_READY: egui::Color32 = egui::Color32::from_rgb(78, 128, 96); // #4E8060
const DOT_BUSY: egui::Color32 = egui::Color32::from_rgb(181, 137, 61); // #B5893D
const DOT_OFF: egui::Color32 = egui::Color32::from_rgb(160, 154, 146); // #A09A92
const ERROR_BG: egui::Color32 = egui::Color32::from_rgb(250, 241, 240);
const ERROR_BORDER: egui::Color32 = egui::Color32::from_rgb(224, 192, 188);
const ERROR_TEXT: egui::Color32 = egui::Color32::from_rgb(156, 60, 52);

#[derive(Debug, Clone)]
enum AppPhase {
    Off,
    Downloading {
        downloaded: u64,
        total: Option<u64>,
        speed_bps: f64,
        dest_display: String,
    },
    Loading,
    On {
        backend_desc: String,
    },
    Fixing {
        backend_desc: String,
    },
    Unloading,
}

impl AppPhase {
    fn is_on(&self) -> bool {
        matches!(self, AppPhase::On { .. } | AppPhase::Fixing { .. })
    }

    fn is_busy(&self) -> bool {
        matches!(
            self,
            AppPhase::Downloading { .. }
                | AppPhase::Loading
                | AppPhase::Fixing { .. }
                | AppPhase::Unloading
        )
    }
}

struct SharedState {
    phase: AppPhase,
    error: Option<String>,
    notice: String,
    active_model_path: Option<PathBuf>,
    force_quit: bool,
    download_cancel: Option<Arc<AtomicBool>>,
    picked_folder: Option<PathBuf>,
    hotkey_revert: Option<HotkeyBinding>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            phase: AppPhase::Off,
            error: None,
            notice: "VRAM free".to_string(),
            active_model_path: None,
            force_quit: false,
            download_cancel: None,
            picked_folder: None,
            hotkey_revert: None,
        }
    }
}

enum CtrlCmd {
    ToggleOnOff,
    TurnOn(String, PathBuf),
    TurnOff,
    DownloadModel(String, PathBuf),
    CancelDownload,
    UpdateHotkey(HotkeyBinding),
    ShowWindow,
    Quit,
    DownloadProgress {
        progress: DownloadProgress,
    },
    DownloadFinished {
        dest: PathBuf,
        res: anyhow::Result<()>,
        turn_on_after: bool,
        restore_on_cancel: bool,
    },
    LoadFinished {
        dest: PathBuf,
        res: anyhow::Result<LoadInfo>,
    },
    Hotkey(HotkeyEvent),
    ShutdownController,
}

fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if (bytes as f64) >= GB * 0.95 {
        format!("{:.2} GB", (bytes as f64) / GB)
    } else {
        format!("{:.1} MB", (bytes as f64) / MB)
    }
}

fn format_speed(bps: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bps <= 1.0 {
        "— MB/s".to_string()
    } else {
        format!("{:.1} MB/s", bps / MB)
    }
}

fn format_eta(downloaded: u64, total: Option<u64>, speed_bps: f64) -> Option<String> {
    let total = total?;
    if total <= downloaded || speed_bps <= 1024.0 {
        return None;
    }
    let secs = ((total - downloaded) as f64 / speed_bps).ceil() as u64;
    if secs < 60 {
        Some(format!("~{secs}s left"))
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        Some(format!("~{mins}m {rem:02}s left"))
    }
}

fn shorten_middle(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars || max_chars < 12 {
        return text.to_string();
    }
    let keep = (max_chars - 3) / 2;
    let start: String = text.chars().take(keep).collect();
    let end: String = text.chars().skip(count - keep).collect();
    format!("{start}...{end}")
}

fn egui_key_to_vk(key: egui::Key) -> Option<u32> {
    use egui::Key;
    let vk = match key {
        Key::A => 0x41,
        Key::B => 0x42,
        Key::C => 0x43,
        Key::D => 0x44,
        Key::E => 0x45,
        Key::F => 0x46,
        Key::G => 0x47,
        Key::H => 0x48,
        Key::I => 0x49,
        Key::J => 0x4A,
        Key::K => 0x4B,
        Key::L => 0x4C,
        Key::M => 0x4D,
        Key::N => 0x4E,
        Key::O => 0x4F,
        Key::P => 0x50,
        Key::Q => 0x51,
        Key::R => 0x52,
        Key::S => 0x53,
        Key::T => 0x54,
        Key::U => 0x55,
        Key::V => 0x56,
        Key::W => 0x57,
        Key::X => 0x58,
        Key::Y => 0x59,
        Key::Z => 0x5A,
        Key::Num0 => 0x30,
        Key::Num1 => 0x31,
        Key::Num2 => 0x32,
        Key::Num3 => 0x33,
        Key::Num4 => 0x34,
        Key::Num5 => 0x35,
        Key::Num6 => 0x36,
        Key::Num7 => 0x37,
        Key::Num8 => 0x38,
        Key::Num9 => 0x39,
        Key::F1 => 0x70,
        Key::F2 => 0x71,
        Key::F3 => 0x72,
        Key::F4 => 0x73,
        Key::F5 => 0x74,
        Key::F6 => 0x75,
        Key::F7 => 0x76,
        Key::F8 => 0x77,
        Key::F9 => 0x78,
        Key::F10 => 0x79,
        Key::F11 => 0x7A,
        Key::F12 => 0x7B,
        Key::Space => 0x20,
        Key::Tab => 0x09,
        Key::Insert => 0x2D,
        Key::Delete => 0x2E,
        Key::Home => 0x24,
        Key::End => 0x23,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::ArrowLeft => 0x25,
        Key::ArrowUp => 0x26,
        Key::ArrowRight => 0x27,
        Key::ArrowDown => 0x28,
        Key::Minus => 0xBD,
        Key::Equals => 0xBB,
        Key::OpenBracket => 0xDB,
        Key::CloseBracket => 0xDD,
        Key::Semicolon => 0xBA,
        Key::Quote => 0xDE,
        Key::Comma => 0xBC,
        Key::Period => 0xBE,
        Key::Slash => 0xBF,
        Key::Backtick => 0xC0,
        Key::Backslash => 0xDC,
        _ => return None,
    };
    Some(vk)
}

pub struct KeyboardLlmApp {
    config: AppConfig,
    url_input: String,
    dir_input: String,
    shared_url: Arc<Mutex<String>>,
    shared_dir: Arc<Mutex<PathBuf>>,
    shared_hotkey: Arc<Mutex<HotkeyBinding>>,
    state: Arc<Mutex<SharedState>>,
    ctrl_tx: Sender<CtrlCmd>,
    ctrl_join: Option<JoinHandle<()>>,
    shell: Option<ShellBridge>,
    hwnd_raw: isize,
    last_saved_window: Option<WindowState>,
    last_window_change: Option<Instant>,
    icon_tex_off: egui::TextureHandle,
    icon_tex_on: egui::TextureHandle,
    last_viewport_active: bool,
    recording_hotkey: bool,
    hotkey_hint: Option<String>,
    browsing_folder: Arc<AtomicBool>,
}

impl KeyboardLlmApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_neutral_visuals(&cc.egui_ctx);

        let config = load_config();
        let initial_window = config.window;
        let url_input = config.model_url.clone();
        let effective_dir = effective_model_dir(&config);
        let dir_input = effective_dir.to_string_lossy().into_owned();

        let shared_url = Arc::new(Mutex::new(url_input.clone()));
        let shared_dir = Arc::new(Mutex::new(effective_dir));
        let shared_hotkey = Arc::new(Mutex::new(config.hotkey));
        let state = Arc::new(Mutex::new(SharedState::default()));

        let tex_off = cc.egui_ctx.load_texture(
            "kbd_emblem_off",
            egui::ColorImage::from_rgba_unmultiplied(
                [128, 128],
                &icon_render::render_keyboard_rgba(128, false),
            ),
            egui::TextureOptions::LINEAR,
        );
        let tex_on = cc.egui_ctx.load_texture(
            "kbd_emblem_on",
            egui::ColorImage::from_rgba_unmultiplied(
                [128, 128],
                &icon_render::render_keyboard_rgba(128, true),
            ),
            egui::TextureOptions::LINEAR,
        );

        let (ctrl_tx, ctrl_rx) = mpsc::channel();

        let hwnd_raw = cc
            .window_handle()
            .ok()
            .and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
                _ => None,
            })
            .unwrap_or(0);

        if let Some(ws) = initial_window {
            if hwnd_raw != 0 {
                restore_window_position(hwnd_raw, ws.x, ws.y);
                if ws.maximized {
                    cc.egui_ctx
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                }
            }
        }

        let shell = if hwnd_raw != 0 {
            let tx_shell = ctrl_tx.clone();
            let ctx_shell = cc.egui_ctx.clone();
            ShellBridge::install(
                hwnd_raw,
                config.hotkey.display_label(),
                cc.egui_ctx.clone(),
                Arc::new(move |cmd| {
                    let msg = match cmd {
                        ShellCommand::ToggleOnOff => CtrlCmd::ToggleOnOff,
                        ShellCommand::ShowWindow => CtrlCmd::ShowWindow,
                        ShellCommand::Quit => CtrlCmd::Quit,
                    };
                    let _ = tx_shell.send(msg);
                    ctx_shell.request_repaint();
                }),
            )
            .ok()
        } else {
            None
        };

        let llm = Arc::new(LlmHandle::spawn());

        let tx_hk = ctrl_tx.clone();
        let ctx_hk = cc.egui_ctx.clone();
        let hotkey = HotkeyController::spawn(
            Arc::clone(&llm),
            Arc::new(move |evt| {
                let _ = tx_hk.send(CtrlCmd::Hotkey(evt));
                ctx_hk.request_repaint();
            }),
        )
        .expect("Failed to start global hotkey listener");

        let state_ctrl = Arc::clone(&state);
        let url_ctrl = Arc::clone(&shared_url);
        let dir_ctrl = Arc::clone(&shared_dir);
        let hk_ctrl = Arc::clone(&shared_hotkey);
        let tx_ctrl = ctrl_tx.clone();
        let shell_ctrl = shell.clone();
        let ctx_ctrl = cc.egui_ctx.clone();

        let ctrl_join = thread::Builder::new()
            .name("app-controller".to_string())
            .spawn(move || {
                controller_loop(
                    ctrl_rx,
                    tx_ctrl,
                    state_ctrl,
                    url_ctrl,
                    dir_ctrl,
                    hk_ctrl,
                    llm,
                    hotkey,
                    shell_ctrl,
                    ctx_ctrl,
                );
            })
            .expect("Failed to spawn controller thread");

        Self {
            config,
            url_input,
            dir_input,
            shared_url,
            shared_dir,
            shared_hotkey,
            state,
            ctrl_tx,
            ctrl_join: Some(ctrl_join),
            shell,
            hwnd_raw,
            last_saved_window: initial_window,
            last_window_change: None,
            icon_tex_off: tex_off,
            icon_tex_on: tex_on,
            last_viewport_active: false,
            recording_hotkey: false,
            hotkey_hint: None,
            browsing_folder: Arc::new(AtomicBool::new(false)),
        }
    }

    fn track_window_geometry(&mut self, ctx: &egui::Context) {
        let Some((x, y, is_min, is_max)) = get_window_placement_state(self.hwnd_raw) else {
            return;
        };
        if is_min || x < -30000 || y < -30000 {
            return;
        }
        let size = ctx
            .input(|i| i.viewport().inner_rect.map(|r| r.size()))
            .unwrap_or_else(|| ctx.screen_rect().size());
        if size.x < 400.0 || size.y < 400.0 {
            return;
        }

        let next = if is_max {
            let prev = self.config.window.unwrap_or(WindowState {
                x,
                y,
                width: size.x.max(435.0),
                height: size.y.max(455.0),
                maximized: true,
            });
            WindowState {
                maximized: true,
                ..prev
            }
        } else {
            WindowState {
                x,
                y,
                width: size.x.max(435.0),
                height: size.y.max(455.0),
                maximized: false,
            }
        };

        if self.config.window != Some(next) {
            self.config.window = Some(next);
            self.last_window_change = Some(Instant::now());
        } else if let Some(changed_at) = self.last_window_change {
            if self.config.window != self.last_saved_window
                && changed_at.elapsed() >= Duration::from_millis(600)
            {
                self.last_saved_window = self.config.window;
                self.last_window_change = None;
                self.save_current_config();
            }
        }
    }

    fn save_current_config(&self) {
        let _ = save_config(&self.config);
    }

    fn apply_storage_dir(&mut self, dir: PathBuf) {
        let default_dir = default_model_dir();
        if dir == default_dir {
            self.config.model_dir = None;
        } else {
            self.config.model_dir = Some(dir.to_string_lossy().into_owned());
        }
        self.dir_input = dir.to_string_lossy().into_owned();
        *self.shared_dir.lock().unwrap() = dir;
        self.save_current_config();
    }

    fn current_storage_dir(&self) -> PathBuf {
        let trimmed = self.dir_input.trim();
        if trimmed.is_empty() {
            default_model_dir()
        } else {
            PathBuf::from(trimmed)
        }
    }

    fn handle_hotkey_recording(&mut self, ctx: &egui::Context) {
        if !self.recording_hotkey {
            return;
        }
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } = ev
            {
                if key == egui::Key::Escape {
                    self.recording_hotkey = false;
                    self.hotkey_hint = None;
                    return;
                }
                if let Some(vk) = egui_key_to_vk(key) {
                    let candidate = HotkeyBinding {
                        ctrl: modifiers.ctrl,
                        alt: modifiers.alt,
                        shift: modifiers.shift,
                        win: false,
                        vk,
                    };
                    if candidate.is_valid() {
                        self.recording_hotkey = false;
                        self.hotkey_hint = None;
                        self.config.hotkey = candidate;
                        *self.shared_hotkey.lock().unwrap() = candidate;
                        self.save_current_config();
                        let _ = self.ctrl_tx.send(CtrlCmd::UpdateHotkey(candidate));
                        return;
                    } else {
                        self.hotkey_hint =
                            Some("Add Ctrl, Alt, or Shift (or use F1–F12)".to_string());
                    }
                }
            }
        }
    }
}

impl Drop for KeyboardLlmApp {
    fn drop(&mut self) {
        if self.config.window != self.last_saved_window {
            self.save_current_config();
            self.last_saved_window = self.config.window;
        }
        let _ = self.ctrl_tx.send(CtrlCmd::ShutdownController);
        if let Some(join) = self.ctrl_join.take() {
            let _ = join.join();
        }
        if let Some(shell) = self.shell.as_ref() {
            shell.shutdown();
        }
    }
}

impl eframe::App for KeyboardLlmApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.track_window_geometry(ctx);

        if ctx.input(|i| i.viewport().close_requested()) {
            let force_quit = self.state.lock().unwrap().force_quit;
            if !force_quit && self.config.minimize_to_tray_on_close {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if let Some(shell) = self.shell.as_ref() {
                    shell.hide_window();
                }
            } else {
                if self.config.window != self.last_saved_window {
                    self.save_current_config();
                    self.last_saved_window = self.config.window;
                }
                let _ = self.ctrl_tx.send(CtrlCmd::ShutdownController);
            }
        }

        self.handle_hotkey_recording(ctx);

        let (phase, error, notice, active_model_path, picked_folder, hotkey_revert) = {
            let mut s = self.state.lock().unwrap();
            (
                s.phase.clone(),
                s.error.clone(),
                s.notice.clone(),
                s.active_model_path.clone(),
                s.picked_folder.take(),
                s.hotkey_revert.take(),
            )
        };

        if let Some(folder) = picked_folder {
            self.apply_storage_dir(folder);
        }
        if let Some(prev_hk) = hotkey_revert {
            self.config.hotkey = prev_hk;
            *self.shared_hotkey.lock().unwrap() = prev_hk;
            self.save_current_config();
        }

        let is_on = phase.is_on();
        if is_on != self.last_viewport_active {
            self.last_viewport_active = is_on;
            ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::new(
                icon::app_viewport_icon(is_on),
            ))));
        }

        let storage_dir = self.current_storage_dir();
        let normalized_preview = normalize_model_url(&self.url_input).ok();
        let preview_path = normalized_preview
            .as_deref()
            .map(|u| local_model_path(&storage_dir, u));
        let cached_size = preview_path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len());
        let part_path = preview_path
            .as_ref()
            .map(|p| p.with_extension("gguf.part"));
        let part_size = part_path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len())
            .filter(|&sz| sz > 0);
        let is_different_from_active =
            is_on && preview_path.is_some() && preview_path != active_model_path;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG_WINDOW)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

                // 1. Header bar
                ui.horizontal(|ui| {
                    let tex = if is_on {
                        &self.icon_tex_on
                    } else {
                        &self.icon_tex_off
                    };
                    ui.image((tex.id(), egui::vec2(26.0, 26.0)));
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Keyboard LLM")
                            .size(17.5)
                            .strong()
                            .color(TEXT_PRIMARY),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (dot_color, status_text): (egui::Color32, String) = match &phase {
                            AppPhase::Off if cached_size.is_none() => {
                                (DOT_BUSY, "Setup".to_string())
                            }
                            AppPhase::Off => (DOT_OFF, "Off".to_string()),
                            AppPhase::Downloading {
                                downloaded, total, ..
                            } => {
                                if let Some(tot) = total.filter(|&t| t > 0) {
                                    let pct = ((*downloaded as f64 / tot as f64) * 100.0)
                                        .clamp(0.0, 100.0);
                                    (DOT_BUSY, format!("Downloading {pct:.0}%"))
                                } else {
                                    (DOT_BUSY, "Downloading...".to_string())
                                }
                            }
                            AppPhase::Loading => (DOT_BUSY, "Loading...".to_string()),
                            AppPhase::On { .. } => (DOT_READY, "Ready".to_string()),
                            AppPhase::Fixing { .. } => (DOT_BUSY, "Fixing...".to_string()),
                            AppPhase::Unloading => (DOT_BUSY, "Unloading...".to_string()),
                        };
                        egui::Frame::new()
                            .fill(BG_CARD)
                            .stroke(egui::Stroke::new(1.0_f32, BORDER_SUBTLE))
                            .corner_radius(egui::CornerRadius::same(12))
                            .inner_margin(egui::Margin::symmetric(10, 4))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(8.0, 8.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().circle_filled(rect.center(), 4.0, dot_color);
                                    ui.label(
                                        egui::RichText::new(status_text)
                                            .size(12.0)
                                            .color(TEXT_PRIMARY),
                                    );
                                });
                            });
                    });
                });

                // 2. Main Power & Interactive Download Card
                card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

                    match &phase {
                        AppPhase::Off => {
                            if cached_size.is_none() {
                                egui::Frame::new()
                                    .fill(BG_CALLOUT)
                                    .stroke(egui::Stroke::new(1.0_f32, BORDER_SUBTLE))
                                    .corner_radius(egui::CornerRadius::same(8))
                                    .inner_margin(egui::Margin::same(10))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.spacing_mut().item_spacing.y = 4.0;
                                        let title = if let Some(ps) = part_size {
                                            format!("Download paused — {} saved", format_bytes(ps))
                                        } else {
                                            "No model downloaded yet".to_string()
                                        };
                                        ui.label(
                                            egui::RichText::new(title)
                                                .size(12.5)
                                                .strong()
                                                .color(TEXT_PRIMARY),
                                        );
                                        ui.label(
                                            egui::RichText::new(
                                                "Click below to download the default uncensored Qwen3.5-4B model (~2.5 GB) from Hugging Face to your storage folder.",
                                            )
                                            .size(11.5)
                                            .color(TEXT_MUTED),
                                        );
                                    });
                            }

                            let switch_label = if cached_size.is_some() {
                                "Turn On"
                            } else if part_size.is_some() {
                                "Resume Download & Turn On"
                            } else {
                                "Download & Turn On"
                            };
                            if power_switch_row(ui, switch_label, false, true, false).clicked() {
                                let url = self.url_input.clone();
                                if let Ok(norm) = normalize_model_url(&url) {
                                    self.url_input = norm.clone();
                                    self.config.model_url = norm.clone();
                                    *self.shared_url.lock().unwrap() = norm;
                                    self.save_current_config();
                                }
                                let dir = self.current_storage_dir();
                                let _ = self
                                    .ctrl_tx
                                    .send(CtrlCmd::TurnOn(self.url_input.clone(), dir));
                            }

                            if cached_size.is_none() {
                                if let Some(part) = part_path.as_ref().filter(|_| part_size.is_some()) {
                                    ui.horizontal(|ui| {
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("Discard partial file")
                                                                .size(11.0)
                                                                .color(TEXT_MUTED),
                                                        )
                                                        .frame(false),
                                                    )
                                                    .clicked()
                                                {
                                                    let _ = fs::remove_file(part);
                                                }
                                            },
                                        );
                                    });
                                }
                            }
                        }
                        AppPhase::Downloading {
                            downloaded,
                            total,
                            speed_bps,
                            dest_display,
                        } => {
                            egui::Frame::new()
                                .fill(BG_CALLOUT)
                                .stroke(egui::Stroke::new(1.0_f32, BORDER_SUBTLE))
                                .corner_radius(egui::CornerRadius::same(8))
                                .inner_margin(egui::Margin::same(10))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.spacing_mut().item_spacing.y = 7.0;

                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        let heading = if *downloaded == 0 {
                                            "Connecting to Hugging Face..."
                                        } else {
                                            "Downloading model from Hugging Face..."
                                        };
                                        ui.label(
                                            egui::RichText::new(heading)
                                                .size(12.5)
                                                .strong()
                                                .color(TEXT_PRIMARY),
                                        );
                                        if let Some(tot) = total.filter(|&t| t > 0) {
                                            let pct = ((*downloaded as f64 / tot as f64) * 100.0)
                                                .clamp(0.0, 100.0);
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.label(
                                                        egui::RichText::new(format!("{pct:.0}%"))
                                                            .size(12.5)
                                                            .strong()
                                                            .color(TEXT_PRIMARY),
                                                    );
                                                },
                                            );
                                        }
                                    });

                                    let frac = total
                                        .filter(|&t| t > 0)
                                        .map(|t| (*downloaded as f32 / t as f32).clamp(0.0, 1.0))
                                        .unwrap_or(0.0);
                                    draw_neutral_progress_bar(ui, frac, total.is_none());

                                    let mut stats = if let Some(tot) = total {
                                        format!(
                                            "{} / {}  ·  {}",
                                            format_bytes(*downloaded),
                                            format_bytes(*tot),
                                            format_speed(*speed_bps)
                                        )
                                    } else {
                                        format!(
                                            "{}  ·  {}",
                                            format_bytes(*downloaded),
                                            format_speed(*speed_bps)
                                        )
                                    };
                                    if let Some(eta) = format_eta(*downloaded, *total, *speed_bps) {
                                        stats.push_str(&format!("  ·  {eta}"));
                                    }
                                    ui.label(
                                        egui::RichText::new(stats).size(11.5).color(TEXT_PRIMARY),
                                    );

                                    let dest_short = shorten_middle(dest_display, 58);
                                    ui.label(
                                        egui::RichText::new(format!("Saving to: {dest_short}"))
                                            .size(11.0)
                                            .color(TEXT_MUTED),
                                    )
                                    .on_hover_text(dest_display);
                                });

                            if secondary_button(ui, "Pause / Cancel Download", 38.0).clicked() {
                                let _ = self.ctrl_tx.send(CtrlCmd::CancelDownload);
                            }
                        }
                        AppPhase::Loading => {
                            let _ = power_switch_row(
                                ui,
                                "Loading model into GPU...",
                                true,
                                false,
                                true,
                            );
                        }
                        AppPhase::On { .. } | AppPhase::Fixing { .. } => {
                            if power_switch_row(ui, "Turn Off", true, true, false).clicked() {
                                let _ = self.ctrl_tx.send(CtrlCmd::TurnOff);
                            }
                        }
                        AppPhase::Unloading => {
                            let _ = power_switch_row(ui, "Unloading VRAM...", false, false, true);
                        }
                    }

                    // Hotkey + customize row
                    ui.horizontal(|ui| {
                        egui::Frame::new()
                            .fill(BG_KEYCAP)
                            .stroke(egui::Stroke::new(1.0_f32, BORDER_MED))
                            .corner_radius(egui::CornerRadius::same(5))
                            .inner_margin(egui::Margin::symmetric(8, 2))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(self.config.hotkey.display_label())
                                        .size(11.5)
                                        .strong()
                                        .color(TEXT_PRIMARY),
                                );
                            });
                        ui.label(
                            egui::RichText::new("Select all & fix")
                                .size(12.0)
                                .color(TEXT_MUTED),
                        );

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let is_busy_hotkey = matches!(
                                phase,
                                AppPhase::Loading | AppPhase::Fixing { .. } | AppPhase::Unloading
                            );
                            if self.config.hotkey != HotkeyBinding::default()
                                && !self.recording_hotkey
                            {
                                if ui
                                    .add_enabled(
                                        !is_busy_hotkey,
                                        egui::Button::new(
                                            egui::RichText::new("Reset")
                                                .size(11.0)
                                                .color(TEXT_MUTED),
                                        )
                                        .fill(egui::Color32::TRANSPARENT)
                                        .stroke(egui::Stroke::new(1.0_f32, BORDER_SUBTLE))
                                        .corner_radius(egui::CornerRadius::same(5))
                                        .min_size(egui::vec2(48.0, 22.0)),
                                    )
                                    .clicked()
                                {
                                    let def = HotkeyBinding::default();
                                    self.config.hotkey = def;
                                    *self.shared_hotkey.lock().unwrap() = def;
                                    self.save_current_config();
                                    let _ = self.ctrl_tx.send(CtrlCmd::UpdateHotkey(def));
                                }
                            }

                            let change_label = if self.recording_hotkey {
                                "Press keys... (Esc)"
                            } else {
                                "Change Hotkey"
                            };
                            if ui
                                .add_enabled(
                                    !is_busy_hotkey,
                                    egui::Button::new(
                                        egui::RichText::new(change_label)
                                            .size(11.0)
                                            .color(TEXT_PRIMARY),
                                    )
                                    .fill(if self.recording_hotkey {
                                        BG_KEYCAP
                                    } else {
                                        BTN_SEC_BG
                                    })
                                    .stroke(egui::Stroke::new(1.0_f32, BORDER_MED))
                                    .corner_radius(egui::CornerRadius::same(5))
                                    .min_size(egui::vec2(96.0, 22.0)),
                                )
                                .clicked()
                            {
                                self.recording_hotkey = !self.recording_hotkey;
                                self.hotkey_hint = if self.recording_hotkey {
                                    Some("Use Ctrl, Alt, or Shift + key, or F1–F12".to_string())
                                } else {
                                    None
                                };
                            }
                        });
                    });

                    if let Some(hint) = &self.hotkey_hint {
                        ui.label(egui::RichText::new(hint).size(11.0).color(DOT_BUSY));
                    }

                    // Status notice line
                    if matches!(phase, AppPhase::Fixing { .. }) {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(egui::RichText::new(&notice).size(11.5).color(TEXT_MUTED));
                        });
                    } else if !matches!(phase, AppPhase::Downloading { .. } | AppPhase::Loading) {
                        ui.label(egui::RichText::new(&notice).size(11.5).color(TEXT_MUTED));
                    }

                    if let Some(err_msg) = error {
                        egui::Frame::new()
                            .fill(ERROR_BG)
                            .stroke(egui::Stroke::new(1.0_f32, ERROR_BORDER))
                            .corner_radius(egui::CornerRadius::same(6))
                            .inner_margin(egui::Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(&err_msg)
                                            .size(11.5)
                                            .color(ERROR_TEXT),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.small_button("×").clicked() {
                                                self.state.lock().unwrap().error = None;
                                            }
                                        },
                                    );
                                });
                            });
                    }
                });

                // 3. Model Source & Storage Folder Card
                card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 7.0);

                    let is_downloading = matches!(phase, AppPhase::Downloading { .. });
                    let is_busy_loading = matches!(
                        phase,
                        AppPhase::Loading | AppPhase::Unloading | AppPhase::Fixing { .. }
                    );

                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Model Download Link (Hugging Face GGUF)")
                                .size(12.5)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let cache_label = match cached_size {
                                Some(sz) => format!("Cached · {}", format_bytes(sz)),
                                None if part_size.is_some() => {
                                    format!("Partial · {}", format_bytes(part_size.unwrap()))
                                }
                                None => "Not downloaded".to_string(),
                            };
                            ui.label(
                                egui::RichText::new(cache_label)
                                    .size(11.5)
                                    .color(TEXT_MUTED),
                            );
                        });
                    });

                    let resp = ui.add_enabled(
                        !is_downloading,
                        egui::TextEdit::singleline(&mut self.url_input)
                            .desired_width(f32::INFINITY)
                            .hint_text("https://huggingface.co/.../model.gguf")
                            .margin(egui::Margin::symmetric(8, 6)),
                    );
                    if resp.changed() {
                        *self.shared_url.lock().unwrap() = self.url_input.clone();
                        if let Ok(norm) = normalize_model_url(&self.url_input) {
                            self.config.model_url = norm;
                            self.save_current_config();
                        }
                    }

                    if let Some(norm) = normalized_preview.as_deref() {
                        ui.label(
                            egui::RichText::new(format!("File: {}", display_model_name(norm)))
                                .size(11.0)
                                .color(TEXT_MUTED),
                        );
                    }

                    ui.horizontal(|ui| {
                        let dl_label = if is_different_from_active {
                            if cached_size.is_some() {
                                "Switch Model"
                            } else {
                                "Download & Switch"
                            }
                        } else if cached_size.is_some() {
                            "Model Ready"
                        } else if part_size.is_some() {
                            "Resume Download"
                        } else {
                            "Download Model"
                        };

                        let can_click_dl = !is_downloading
                            && !is_busy_loading
                            && (cached_size.is_none() || is_different_from_active);

                        if ui
                            .add_enabled(
                                can_click_dl,
                                egui::Button::new(
                                    egui::RichText::new(dl_label).size(12.0).color(TEXT_PRIMARY),
                                )
                                .fill(BTN_SEC_BG)
                                .stroke(egui::Stroke::new(1.0_f32, BORDER_MED))
                                .corner_radius(egui::CornerRadius::same(6))
                                .min_size(egui::vec2(130.0, 27.0)),
                            )
                            .clicked()
                        {
                            let url = self.url_input.clone();
                            if let Ok(norm) = normalize_model_url(&url) {
                                self.url_input = norm.clone();
                                self.config.model_url = norm.clone();
                                *self.shared_url.lock().unwrap() = norm;
                                self.save_current_config();
                            }
                            let dir = self.current_storage_dir();
                            let _ = self
                                .ctrl_tx
                                .send(CtrlCmd::DownloadModel(self.url_input.clone(), dir));
                        }

                        if ui
                            .add_enabled(
                                !is_downloading && self.url_input.trim() != DEFAULT_MODEL_URL,
                                egui::Button::new(
                                    egui::RichText::new("Reset Default")
                                        .size(12.0)
                                        .color(TEXT_MUTED),
                                )
                                .fill(egui::Color32::TRANSPARENT)
                                .stroke(egui::Stroke::new(1.0_f32, BORDER_SUBTLE))
                                .corner_radius(egui::CornerRadius::same(6))
                                .min_size(egui::vec2(96.0, 27.0)),
                            )
                            .clicked()
                        {
                            self.url_input = DEFAULT_MODEL_URL.to_string();
                            self.config.model_url = self.url_input.clone();
                            *self.shared_url.lock().unwrap() = self.url_input.clone();
                            self.save_current_config();
                        }
                    });

                    ui.add_space(2.0);
                    ui.separator();
                    ui.add_space(1.0);

                    // Model Storage Folder selector
                    let is_custom_dir = self.config.model_dir.is_some();
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Model Storage Folder")
                                .size(12.5)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let tag = if is_custom_dir {
                                "Custom folder"
                            } else {
                                "App folder (.exe)"
                            };
                            ui.label(egui::RichText::new(tag).size(11.0).color(TEXT_MUTED));
                        });
                    });

                    ui.horizontal(|ui| {
                        let btn_space = if is_custom_dir { 156.0 } else { 96.0 };
                        let edit_w = (ui.available_width() - btn_space).max(120.0);
                        let dir_resp = ui.add_enabled(
                            !is_downloading,
                            egui::TextEdit::singleline(&mut self.dir_input)
                                .desired_width(edit_w)
                                .hint_text("Folder to save .gguf models")
                                .margin(egui::Margin::symmetric(8, 5)),
                        );
                        if dir_resp.changed() {
                            let dir = self.current_storage_dir();
                            self.config.model_dir = if dir == default_model_dir() {
                                None
                            } else {
                                Some(self.dir_input.trim().to_string())
                            };
                            *self.shared_dir.lock().unwrap() = dir;
                            self.save_current_config();
                        }

                        let browsing = self.browsing_folder.load(Ordering::Relaxed);
                        if ui
                            .add_enabled(
                                !is_downloading && !browsing,
                                egui::Button::new(
                                    egui::RichText::new("Browse...")
                                        .size(11.5)
                                        .color(TEXT_PRIMARY),
                                )
                                .fill(BTN_SEC_BG)
                                .stroke(egui::Stroke::new(1.0_f32, BORDER_MED))
                                .corner_radius(egui::CornerRadius::same(6))
                                .min_size(egui::vec2(74.0, 26.0)),
                            )
                            .clicked()
                        {
                            self.browsing_folder.store(true, Ordering::Relaxed);
                            let flag = Arc::clone(&self.browsing_folder);
                            let state_c = Arc::clone(&self.state);
                            let ctx_c = ctx.clone();
                            let init_dir = self.current_storage_dir();
                            let _ = thread::Builder::new()
                                .name("folder-picker".to_string())
                                .spawn(move || {
                                    if let Some(chosen) = pick_folder_dialog(Some(&init_dir)) {
                                        state_c.lock().unwrap().picked_folder = Some(chosen);
                                    }
                                    flag.store(false, Ordering::Relaxed);
                                    ctx_c.request_repaint();
                                });
                        }

                        if is_custom_dir
                            && ui
                                .add_enabled(
                                    !is_downloading,
                                    egui::Button::new(
                                        egui::RichText::new("Reset")
                                            .size(11.5)
                                            .color(TEXT_MUTED),
                                    )
                                    .fill(egui::Color32::TRANSPARENT)
                                    .stroke(egui::Stroke::new(1.0_f32, BORDER_SUBTLE))
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .min_size(egui::vec2(52.0, 26.0)),
                                )
                                .clicked()
                        {
                            self.apply_storage_dir(default_model_dir());
                        }
                    });
                });

                // 4. Footer
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .checkbox(
                                &mut self.config.minimize_to_tray_on_close,
                                egui::RichText::new("Minimize to tray on close")
                                    .size(11.5)
                                    .color(TEXT_MUTED),
                            )
                            .changed()
                        {
                            self.save_current_config();
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Hide to Tray")
                                            .size(11.5)
                                            .color(TEXT_MUTED),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                if let Some(shell) = self.shell.as_ref() {
                                    shell.hide_window();
                                }
                            }
                        });
                    });
                });
            });
    }
}

fn draw_neutral_progress_bar(ui: &mut egui::Ui, frac: f32, indeterminate: bool) {
    let width = ui.available_width();
    let height = 10.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let radius = egui::CornerRadius::same(5);
    ui.painter().rect_filled(rect, radius, BG_TRACK);

    if indeterminate {
        ui.ctx().request_repaint();
        let t = ui.input(|i| i.time) as f32;
        let seg_w = (width * 0.28).max(36.0);
        let travel = (width - seg_w).max(1.0);
        let pos = ((t * 0.9).sin() * 0.5 + 0.5) * travel;
        let fill_rect = egui::Rect::from_min_size(
            egui::pos2(rect.min.x + pos, rect.min.y),
            egui::vec2(seg_w, height),
        );
        ui.painter().rect_filled(fill_rect, radius, BTN_PRIMARY_BG);
    } else if frac > 0.001 {
        let fill_w = (width * frac.clamp(0.0, 1.0)).max(height);
        let fill_rect =
            egui::Rect::from_min_size(rect.min, egui::vec2(fill_w.min(width), height));
        ui.painter().rect_filled(fill_rect, radius, BTN_PRIMARY_BG);
    }
}

fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BG_CARD)
        .stroke(egui::Stroke::new(1.0_f32, BORDER_SUBTLE))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 {
        ((x as f32) + ((y as f32) - (x as f32)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    egui::Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

fn draw_pill_switch(
    ui: &egui::Ui,
    rect: egui::Rect,
    id: egui::Id,
    is_on: bool,
    enabled: bool,
    hovered: bool,
) {
    let t = ui.ctx().animate_bool_with_time(id, is_on, 0.16);

    let track_off = egui::Color32::WHITE;
    let track_on = egui::Color32::from_rgb(77, 150, 98); // #4D9662
    let border_off = egui::Color32::from_rgb(200, 196, 190); // #C8C4BE
    let border_on = egui::Color32::from_rgb(67, 133, 86); // #438556

    let mut track_fill = lerp_color(track_off, track_on, t);
    let mut track_border = lerp_color(border_off, border_on, t);
    if !enabled {
        track_fill = lerp_color(track_fill, BG_TRACK, 0.45);
        track_border = lerp_color(track_border, BORDER_MED, 0.45);
    } else if hovered {
        track_border = lerp_color(track_border, TEXT_MUTED, 0.25);
    }

    let radius = egui::CornerRadius::same(15);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, track_fill);
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.2_f32, track_border),
        egui::StrokeKind::Inside,
    );

    let cx = egui::lerp((rect.left() + 15.0)..=(rect.right() - 15.0), t);
    let cy = rect.center().y;
    painter.circle_filled(
        egui::pos2(cx, cy + 1.5),
        11.8,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 18),
    );
    painter.circle_filled(
        egui::pos2(cx, cy + 0.8),
        11.5,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 28),
    );
    let knob_fill = if enabled {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(245, 244, 242)
    };
    painter.circle_filled(egui::pos2(cx, cy), 11.3, knob_fill);
    painter.circle_stroke(
        egui::pos2(cx, cy),
        11.3,
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(220, 217, 212)),
    );
}

fn power_switch_row(
    ui: &mut egui::Ui,
    label: &str,
    is_on: bool,
    enabled: bool,
    show_spinner: bool,
) -> egui::Response {
    let width = ui.available_width();
    let height = 46.0;
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), sense);
    let response = if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    };

    let (box_fill, box_border) = if enabled && response.is_pointer_button_down_on() {
        (egui::Color32::from_rgb(232, 229, 223), BORDER_MED)
    } else if enabled && response.hovered() {
        (egui::Color32::from_rgb(240, 238, 234), BORDER_MED)
    } else {
        (BG_CALLOUT, BORDER_SUBTLE)
    };

    let box_radius = egui::CornerRadius::same(8);
    ui.painter().rect_filled(rect, box_radius, box_fill);
    ui.painter().rect_stroke(
        rect,
        box_radius,
        egui::Stroke::new(1.0_f32, box_border),
        egui::StrokeKind::Inside,
    );

    let text_left = rect.left() + 14.0 + if show_spinner { 24.0 } else { 0.0 };
    let switch_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 12.0 - 27.0, rect.center().y),
        egui::vec2(54.0, 30.0),
    );

    if show_spinner {
        let spinner_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 22.0, rect.center().y),
            egui::vec2(16.0, 16.0),
        );
        ui.put(spinner_rect, egui::Spinner::new().size(15.0));
    }

    let text_color = if enabled { TEXT_PRIMARY } else { TEXT_MUTED };
    let avail_text_w = (switch_rect.left() - 10.0 - text_left).max(40.0);
    let galley = egui::WidgetText::from(
        egui::RichText::new(label)
            .size(14.0)
            .strong()
            .color(text_color),
    )
    .into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        avail_text_w,
        egui::TextStyle::Button,
    );
    let text_pos = egui::pos2(text_left, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(text_pos, galley, text_color);

    draw_pill_switch(ui, switch_rect, response.id, is_on, enabled, response.hovered());

    response
}

fn secondary_button(ui: &mut egui::Ui, text: &str, height: f32) -> egui::Response {
    let w = ui.available_width();
    let btn = egui::Button::new(
        egui::RichText::new(text)
            .size(14.0)
            .strong()
            .color(TEXT_PRIMARY),
    )
    .fill(BTN_SEC_BG)
    .stroke(egui::Stroke::new(1.0_f32, BORDER_MED))
    .corner_radius(egui::CornerRadius::same(8))
    .min_size(egui::vec2(w, height));
    ui.scope(|ui| {
        ui.visuals_mut().widgets.hovered.bg_fill = BTN_SEC_HOVER;
        ui.visuals_mut().widgets.active.bg_fill = BTN_SEC_BG;
        ui.add(btn)
    })
    .inner
}

fn apply_neutral_visuals(ctx: &egui::Context) {
    let mut v = egui::Visuals::light();
    v.window_fill = BG_WINDOW;
    v.panel_fill = BG_WINDOW;
    v.extreme_bg_color = BG_INPUT;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, TEXT_PRIMARY);
    v.widgets.inactive.bg_fill = BG_INPUT;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, BORDER_MED);
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, BTN_PRIMARY_BG);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.2_f32, BTN_PRIMARY_BG);
    v.selection.bg_fill = egui::Color32::from_rgb(214, 211, 205);
    v.selection.stroke = egui::Stroke::new(1.0_f32, BTN_PRIMARY_BG);
    ctx.set_visuals(v);
}

#[allow(clippy::too_many_arguments)]
fn controller_loop(
    rx: Receiver<CtrlCmd>,
    tx: Sender<CtrlCmd>,
    state: Arc<Mutex<SharedState>>,
    shared_url: Arc<Mutex<String>>,
    shared_dir: Arc<Mutex<PathBuf>>,
    shared_hotkey: Arc<Mutex<HotkeyBinding>>,
    mut llm: Arc<LlmHandle>,
    mut hotkey: HotkeyController,
    shell: Option<ShellBridge>,
    ctx: egui::Context,
) {
    let mut registered_hotkey = *shared_hotkey.lock().unwrap();

    let sync_shell = |is_on: bool, is_busy: bool, progress: Option<TaskbarProgress>| {
        if let Some(sh) = shell.as_ref() {
            if let Some(p) = progress {
                sh.set_progress(p);
            }
            sh.sync_state(is_on, is_busy);
        }
        ctx.request_repaint();
    };

    let start_download = |raw_url: String,
                          storage_dir: PathBuf,
                          turn_on_after: bool,
                          restore_on_cancel: bool,
                          state: &Arc<Mutex<SharedState>>,
                          tx: &Sender<CtrlCmd>,
                          ctx: &egui::Context| {
        let norm = match normalize_model_url(&raw_url) {
            Ok(u) => u,
            Err(e) => {
                let mut s = state.lock().unwrap();
                s.error = Some(e.to_string());
                drop(s);
                sync_shell(false, false, Some(TaskbarProgress::None));
                return;
            }
        };

        let mut cfg = load_config();
        cfg.model_url = norm.clone();
        cfg.model_dir = if storage_dir == default_model_dir() {
            None
        } else {
            Some(storage_dir.to_string_lossy().into_owned())
        };
        let _ = save_config(&cfg);

        let dest = local_model_path(&storage_dir, &norm);
        if dest.exists() {
            if turn_on_after {
                let mut s = state.lock().unwrap();
                s.phase = AppPhase::Loading;
                s.error = None;
                s.notice = "Loading model into GPU...".to_string();
                drop(s);
                sync_shell(false, true, Some(TaskbarProgress::Indeterminate));

                let llm_c = Arc::clone(&llm);
                let tx_c = tx.clone();
                let ctx_c = ctx.clone();
                let dest_c = dest.clone();
                let _ = thread::Builder::new()
                    .name("model-loader".to_string())
                    .spawn(move || {
                        let res = llm_c.load(dest_c.clone());
                        let _ = tx_c.send(CtrlCmd::LoadFinished { dest: dest_c, res });
                        ctx_c.request_repaint();
                    });
            } else {
                let mut s = state.lock().unwrap();
                s.notice = "Model already downloaded".to_string();
                s.error = None;
                ctx.request_repaint();
            }
            return;
        }

        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut s = state.lock().unwrap();
            s.phase = AppPhase::Downloading {
                downloaded: 0,
                total: None,
                speed_bps: 0.0,
                dest_display: dest.to_string_lossy().into_owned(),
            };
            s.error = None;
            s.notice = "Downloading model from Hugging Face...".to_string();
            s.download_cancel = Some(Arc::clone(&cancel));
        }
        sync_shell(false, true, Some(TaskbarProgress::Indeterminate));

        let tx_c = tx.clone();
        let ctx_c = ctx.clone();
        let _ = thread::Builder::new()
            .name("model-downloader".to_string())
            .spawn(move || {
                let tx_prog = tx_c.clone();
                let ctx_prog = ctx_c.clone();
                let res = download_model(&norm, &dest, cancel, move |progress| {
                    let _ = tx_prog.send(CtrlCmd::DownloadProgress { progress });
                    ctx_prog.request_repaint();
                });
                let _ = tx_c.send(CtrlCmd::DownloadFinished {
                    dest,
                    res,
                    turn_on_after,
                    restore_on_cancel,
                });
                ctx_c.request_repaint();
            });
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            CtrlCmd::ToggleOnOff => {
                let is_on_or_busy = {
                    let s = state.lock().unwrap();
                    (s.phase.is_on(), s.phase.is_busy())
                };
                if is_on_or_busy.1 {
                    continue;
                }
                if is_on_or_busy.0 {
                    let _ = tx.send(CtrlCmd::TurnOff);
                } else {
                    let url = shared_url.lock().unwrap().clone();
                    let dir = shared_dir.lock().unwrap().clone();
                    let _ = tx.send(CtrlCmd::TurnOn(url, dir));
                }
            }
            CtrlCmd::TurnOn(raw_url, storage_dir) => {
                let can_start = {
                    let s = state.lock().unwrap();
                    matches!(s.phase, AppPhase::Off)
                };
                if !can_start {
                    continue;
                }
                start_download(raw_url, storage_dir, true, false, &state, &tx, &ctx);
            }
            CtrlCmd::TurnOff => {
                let was_on = {
                    let mut s = state.lock().unwrap();
                    if !s.phase.is_on() {
                        false
                    } else {
                        s.phase = AppPhase::Unloading;
                        s.notice = "Unloading VRAM...".to_string();
                        true
                    }
                };
                if !was_on {
                    continue;
                }
                sync_shell(false, true, Some(TaskbarProgress::Indeterminate));

                hotkey.disable();
                llm.unload();

                {
                    let mut s = state.lock().unwrap();
                    s.phase = AppPhase::Off;
                    s.active_model_path = None;
                    s.notice = "VRAM free".to_string();
                }
                sync_shell(false, false, Some(TaskbarProgress::None));
            }
            CtrlCmd::DownloadModel(raw_url, storage_dir) => {
                let (is_on, is_busy) = {
                    let s = state.lock().unwrap();
                    (s.phase.is_on(), s.phase.is_busy())
                };
                if is_busy {
                    continue;
                }
                if is_on {
                    hotkey.disable();
                }
                start_download(raw_url, storage_dir, is_on, is_on, &state, &tx, &ctx);
            }
            CtrlCmd::CancelDownload => {
                let cancel = state.lock().unwrap().download_cancel.clone();
                if let Some(c) = cancel {
                    c.store(true, Ordering::Relaxed);
                }
            }
            CtrlCmd::UpdateHotkey(new_binding) => {
                let is_on = state.lock().unwrap().phase.is_on();
                if is_on {
                    match hotkey.enable(new_binding) {
                        Ok(()) => {
                            registered_hotkey = new_binding;
                            if let Some(sh) = shell.as_ref() {
                                sh.set_hotkey_label(new_binding.display_label());
                            }
                            {
                                let mut s = state.lock().unwrap();
                                let backend_desc = match &s.phase {
                                    AppPhase::On { backend_desc }
                                    | AppPhase::Fixing { backend_desc } => backend_desc.clone(),
                                    _ => "GPU".to_string(),
                                };
                                s.error = None;
                                s.notice = format!(
                                    "{backend_desc} · Ready ({})",
                                    new_binding.display_label()
                                );
                            }
                            sync_shell(true, false, None);
                        }
                        Err(e) => {
                            let _ = hotkey.enable(registered_hotkey);
                            {
                                let mut s = state.lock().unwrap();
                                s.hotkey_revert = Some(registered_hotkey);
                                s.error = Some(e.to_string());
                            }
                            if let Some(sh) = shell.as_ref() {
                                sh.set_hotkey_label(registered_hotkey.display_label());
                            }
                            sync_shell(true, false, None);
                        }
                    }
                } else {
                    registered_hotkey = new_binding;
                    if let Some(sh) = shell.as_ref() {
                        sh.set_hotkey_label(new_binding.display_label());
                    }
                    sync_shell(false, false, None);
                }
            }
            CtrlCmd::DownloadProgress { progress } => {
                {
                    let mut s = state.lock().unwrap();
                    if let AppPhase::Downloading {
                        downloaded,
                        total,
                        speed_bps,
                        ..
                    } = &mut s.phase
                    {
                        *downloaded = progress.downloaded;
                        *total = progress.total;
                        *speed_bps = progress.speed_bps;
                    }
                }
                if let Some(sh) = shell.as_ref() {
                    let tb = match progress.total {
                        Some(tot) if tot > 0 => TaskbarProgress::Determinate {
                            current: progress.downloaded,
                            total: tot,
                        },
                        _ => TaskbarProgress::Indeterminate,
                    };
                    sh.set_progress(tb);
                }
            }
            CtrlCmd::DownloadFinished {
                dest,
                res,
                turn_on_after,
                restore_on_cancel,
            } => {
                {
                    let mut s = state.lock().unwrap();
                    s.download_cancel = None;
                }
                if state.lock().unwrap().force_quit {
                    continue;
                }
                match res {
                    Ok(()) => {
                        if turn_on_after {
                            {
                                let mut s = state.lock().unwrap();
                                s.phase = AppPhase::Loading;
                                s.error = None;
                                s.notice = "Loading model into GPU...".to_string();
                            }
                            sync_shell(false, true, Some(TaskbarProgress::Indeterminate));

                            let llm_c = Arc::clone(&llm);
                            let tx_c = tx.clone();
                            let ctx_c = ctx.clone();
                            let _ = thread::Builder::new()
                                .name("model-loader".to_string())
                                .spawn(move || {
                                    let res = llm_c.load(dest.clone());
                                    let _ = tx_c.send(CtrlCmd::LoadFinished { dest, res });
                                    ctx_c.request_repaint();
                                });
                        } else {
                            {
                                let mut s = state.lock().unwrap();
                                s.phase = AppPhase::Off;
                                s.error = None;
                                s.notice = "Model downloaded and ready".to_string();
                            }
                            sync_shell(false, false, Some(TaskbarProgress::None));
                        }
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        let was_cancelled = msg.contains("cancelled");
                        let active_hk = *shared_hotkey.lock().unwrap();
                        if restore_on_cancel {
                            let active_path = state.lock().unwrap().active_model_path.clone();
                            if active_path.is_some() && hotkey.enable(active_hk).is_ok() {
                                registered_hotkey = active_hk;
                                {
                                    let mut s = state.lock().unwrap();
                                    s.phase = AppPhase::On {
                                        backend_desc: "GPU".to_string(),
                                    };
                                    if was_cancelled {
                                        s.notice = "Download cancelled".to_string();
                                    } else {
                                        s.error = Some(msg);
                                    }
                                }
                                sync_shell(true, false, Some(TaskbarProgress::None));
                                continue;
                            }
                        }
                        {
                            let mut s = state.lock().unwrap();
                            s.phase = AppPhase::Off;
                            if was_cancelled {
                                s.notice = "Download paused · VRAM free".to_string();
                            } else {
                                s.error = Some(msg);
                                s.notice = "VRAM free".to_string();
                            }
                        }
                        sync_shell(false, false, Some(TaskbarProgress::None));
                    }
                }
            }
            CtrlCmd::LoadFinished { dest, res } => {
                if state.lock().unwrap().force_quit {
                    llm.unload();
                    continue;
                }
                let active_hk = *shared_hotkey.lock().unwrap();
                match res {
                    Ok(info) => match hotkey.enable(active_hk) {
                        Ok(()) => {
                            registered_hotkey = active_hk;
                            if let Some(sh) = shell.as_ref() {
                                sh.set_hotkey_label(active_hk.display_label());
                            }
                            {
                                let mut s = state.lock().unwrap();
                                s.phase = AppPhase::On {
                                    backend_desc: info.backend_desc.clone(),
                                };
                                s.active_model_path = Some(dest);
                                s.error = None;
                                s.notice = format!(
                                    "{} · Ready ({})",
                                    info.backend_desc,
                                    active_hk.display_label()
                                );
                            }
                            sync_shell(true, false, Some(TaskbarProgress::None));
                        }
                        Err(e) => {
                            llm.unload();
                            {
                                let mut s = state.lock().unwrap();
                                s.phase = AppPhase::Off;
                                s.active_model_path = None;
                                s.error = Some(e.to_string());
                                s.notice = "VRAM free".to_string();
                            }
                            sync_shell(false, false, Some(TaskbarProgress::None));
                        }
                    },
                    Err(e) => {
                        {
                            let mut s = state.lock().unwrap();
                            s.phase = AppPhase::Off;
                            s.active_model_path = None;
                            s.error = Some(e.to_string());
                            s.notice = "VRAM free".to_string();
                        }
                        sync_shell(false, false, Some(TaskbarProgress::None));
                    }
                }
            }
            CtrlCmd::Hotkey(evt) => match evt {
                HotkeyEvent::FixingStarted => {
                    {
                        let mut s = state.lock().unwrap();
                        if let AppPhase::On { backend_desc } = &s.phase {
                            s.phase = AppPhase::Fixing {
                                backend_desc: backend_desc.clone(),
                            };
                        }
                        s.notice = "Fixing text...".to_string();
                    }
                    sync_shell(true, true, Some(TaskbarProgress::Indeterminate));
                }
                HotkeyEvent::FixSucceeded {
                    elapsed_ms,
                    changed,
                } => {
                    {
                        let mut s = state.lock().unwrap();
                        let backend_desc = match &s.phase {
                            AppPhase::Fixing { backend_desc } | AppPhase::On { backend_desc } => {
                                backend_desc.clone()
                            }
                            _ => "GPU".to_string(),
                        };
                        if s.phase.is_on() {
                            s.phase = AppPhase::On {
                                backend_desc: backend_desc.clone(),
                            };
                        }
                        let secs = (elapsed_ms as f64) / 1000.0;
                        s.notice = if changed {
                            format!("{backend_desc} · Fixed in {secs:.2}s")
                        } else {
                            format!("{backend_desc} · No changes needed ({secs:.2}s)")
                        };
                    }
                    sync_shell(true, false, Some(TaskbarProgress::None));
                }
                HotkeyEvent::FixNotice(msg) => {
                    {
                        let mut s = state.lock().unwrap();
                        if let AppPhase::Fixing { backend_desc } = &s.phase {
                            s.phase = AppPhase::On {
                                backend_desc: backend_desc.clone(),
                            };
                        }
                        s.notice = msg;
                    }
                    sync_shell(true, false, Some(TaskbarProgress::None));
                }
                HotkeyEvent::FixError(err) => {
                    {
                        let mut s = state.lock().unwrap();
                        if let AppPhase::Fixing { backend_desc } = &s.phase {
                            s.phase = AppPhase::On {
                                backend_desc: backend_desc.clone(),
                            };
                        }
                        s.error = Some(err);
                    }
                    sync_shell(true, false, Some(TaskbarProgress::None));
                }
            },
            CtrlCmd::ShowWindow => {
                if let Some(sh) = shell.as_ref() {
                    sh.show_window();
                }
            }
            CtrlCmd::Quit => {
                {
                    let mut s = state.lock().unwrap();
                    s.force_quit = true;
                    if let Some(c) = s.download_cancel.as_ref() {
                        c.store(true, Ordering::Relaxed);
                    }
                }
                hotkey.disable();
                llm.unload();
                if let Some(sh) = shell.as_ref() {
                    sh.set_progress(TaskbarProgress::None);
                    sh.request_close_window();
                }
                break;
            }
            CtrlCmd::ShutdownController => {
                {
                    let mut s = state.lock().unwrap();
                    s.force_quit = true;
                    if let Some(c) = s.download_cancel.as_ref() {
                        c.store(true, Ordering::Relaxed);
                    }
                }
                hotkey.disable();
                llm.unload();
                break;
            }
        }
    }

    hotkey.shutdown();
    if let Some(llm_mut) = Arc::get_mut(&mut llm) {
        llm_mut.shutdown();
    }
}
