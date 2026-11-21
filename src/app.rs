use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::{
    default_model_dir, display_model_name, effective_model_dir, load_config, local_model_path,
    normalize_model_url, partial_download_path, preset_for_url, save_config, AppConfig,
    ComputeMode, HotkeyBinding, WindowState, MODEL_PRESETS,
};
use crate::controller::{controller_loop, notify_highlight_mode, AppPhase, CtrlCmd, SharedState};
use crate::hotkey::HotkeyController;
use crate::icon;
use crate::icon_render;
use crate::llm::LlmHandle;
use crate::taskbar::{
    get_window_placement_state, pick_folder_dialog, restore_window_position,
    set_window_dark_titlebar, ShellBridge, ShellCommand,
};
use crate::widgets::{
    apply_theme_visuals, card_frame, chip_button, draw_neutral_progress_bar, format_bytes,
    format_eta, format_speed, power_switch_row, render_hotkey_row, secondary_button,
    shorten_middle, theme_switch, toggle_switch_row, HotkeyRowAction, ThemeColors,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyTarget {
    Fix,
    TogglePower,
    ToggleHighlight,
}

struct FolderPickerGuard {
    flag: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl Drop for FolderPickerGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Relaxed);
        self.ctx.request_repaint();
    }
}

struct ModelFileCache {
    path: Option<PathBuf>,
    checked_at: Option<Instant>,
    cached_size: Option<u64>,
    part_size: Option<u64>,
    last_phase_tag: u8,
}

impl Default for ModelFileCache {
    fn default() -> Self {
        Self {
            path: None,
            checked_at: None,
            cached_size: None,
            part_size: None,
            last_phase_tag: u8::MAX,
        }
    }
}

fn phase_tag(phase: &AppPhase) -> u8 {
    match phase {
        AppPhase::Off => 0,
        AppPhase::Downloading { .. } => 1,
        AppPhase::Loading => 2,
        AppPhase::On { .. } => 3,
        AppPhase::Fixing { .. } => 4,
        AppPhase::Unloading => 5,
    }
}

impl ModelFileCache {
    fn invalidate(&mut self) {
        self.last_phase_tag = u8::MAX;
    }

    fn get(
        &mut self,
        preview_path: Option<&PathBuf>,
        phase: &AppPhase,
    ) -> (Option<u64>, Option<u64>) {
        let tag = phase_tag(phase);
        let path_changed = self.path.as_ref() != preview_path;
        let phase_changed = self.last_phase_tag != tag;
        let ttl_expired = self
            .checked_at
            .map_or(true, |t| t.elapsed() >= Duration::from_secs(1));

        if path_changed || phase_changed || ttl_expired {
            self.path = preview_path.cloned();
            self.last_phase_tag = tag;
            self.checked_at = Some(Instant::now());
            self.cached_size = preview_path
                .and_then(|p| fs::metadata(p).ok())
                .map(|m| m.len());
            self.part_size = preview_path
                .map(|p| partial_download_path(p))
                .and_then(|p| fs::metadata(p).ok())
                .map(|m| m.len())
                .filter(|&sz| sz > 0);
        }

        (self.cached_size, self.part_size)
    }
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
    shared_compute_mode: Arc<Mutex<ComputeMode>>,
    shared_hotkey: Arc<Mutex<HotkeyBinding>>,
    shared_highlighted_only: Arc<AtomicBool>,
    shared_show_notifications: Arc<AtomicBool>,
    shared_recording: Arc<AtomicBool>,
    state: Arc<Mutex<SharedState>>,
    ctrl_tx: Sender<CtrlCmd>,
    ctrl_join: Option<JoinHandle<()>>,
    shell: Option<ShellBridge>,
    hwnd_raw: isize,
    last_saved_window: Option<WindowState>,
    last_window_change: Option<Instant>,
    config_dirty_since: Option<Instant>,
    file_cache: ModelFileCache,
    icon_tex_off: egui::TextureHandle,
    icon_tex_on: egui::TextureHandle,
    last_viewport_active: bool,
    applied_dark_mode: bool,
    recording_hotkey: Option<HotkeyTarget>,
    hotkey_hint: Option<String>,
    browsing_folder: Arc<AtomicBool>,
}

impl KeyboardLlmApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let config = load_config();
        let dark_mode = config.dark_mode;
        apply_theme_visuals(&cc.egui_ctx, dark_mode);

        let initial_window = config.window;
        let url_input = config.model_url.clone();
        let effective_dir = effective_model_dir(&config);
        let dir_input = effective_dir.to_string_lossy().into_owned();

        let shared_url = Arc::new(Mutex::new(url_input.clone()));
        let shared_dir = Arc::new(Mutex::new(effective_dir));
        let shared_compute_mode = Arc::new(Mutex::new(config.compute_mode));
        let shared_hotkey = Arc::new(Mutex::new(config.hotkey));
        let shared_highlighted_only = Arc::new(AtomicBool::new(config.fix_highlighted_only));
        let shared_show_notifications = Arc::new(AtomicBool::new(config.show_notifications));
        let shared_recording = Arc::new(AtomicBool::new(false));
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

        if hwnd_raw != 0 {
            set_window_dark_titlebar(hwnd_raw, dark_mode);
        }

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
            Arc::clone(&shared_highlighted_only),
            Arc::clone(&shared_recording),
            Arc::new(move |evt| {
                let _ = tx_hk.send(CtrlCmd::Hotkey(evt));
                ctx_hk.request_repaint();
            }),
        )
        .expect("Failed to start global hotkey listener");

        let state_ctrl = Arc::clone(&state);
        let url_ctrl = Arc::clone(&shared_url);
        let dir_ctrl = Arc::clone(&shared_dir);
        let compute_ctrl = Arc::clone(&shared_compute_mode);
        let hk_ctrl = Arc::clone(&shared_hotkey);
        let hl_ctrl = Arc::clone(&shared_highlighted_only);
        let notif_ctrl = Arc::clone(&shared_show_notifications);
        let init_power_hk = config.toggle_power_hotkey;
        let init_hl_hk = config.toggle_highlight_hotkey;
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
                    compute_ctrl,
                    hk_ctrl,
                    init_power_hk,
                    init_hl_hk,
                    hl_ctrl,
                    notif_ctrl,
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
            shared_compute_mode,
            shared_hotkey,
            shared_highlighted_only,
            shared_show_notifications,
            shared_recording,
            state,
            ctrl_tx,
            ctrl_join: Some(ctrl_join),
            shell,
            hwnd_raw,
            last_saved_window: initial_window,
            last_window_change: None,
            config_dirty_since: None,
            file_cache: ModelFileCache::default(),
            icon_tex_off: tex_off,
            icon_tex_on: tex_on,
            last_viewport_active: false,
            applied_dark_mode: dark_mode,
            recording_hotkey: None,
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
                width: size.x.max(470.0),
                height: size.y.max(595.0),
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
                width: size.x.max(470.0),
                height: size.y.max(595.0),
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

    fn save_current_config(&mut self) {
        self.config_dirty_since = None;
        let _ = save_config(&self.config);
    }

    fn mark_config_dirty(&mut self, ctx: &egui::Context) {
        self.config_dirty_since = Some(Instant::now());
        ctx.request_repaint_after(Duration::from_millis(420));
    }

    fn flush_dirty_config(&mut self, force: bool) {
        if let Some(dirty_at) = self.config_dirty_since {
            if force || dirty_at.elapsed() >= Duration::from_millis(400) {
                self.save_current_config();
            }
        }
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
        self.file_cache.invalidate();
        self.save_current_config();
    }

    fn apply_model_url(&mut self, url: &str) {
        self.url_input = url.to_string();
        self.config.model_url = self.url_input.clone();
        *self.shared_url.lock().unwrap() = self.url_input.clone();
        self.file_cache.invalidate();
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

    fn toggle_recording(&mut self, target: HotkeyTarget) {
        if self.recording_hotkey == Some(target) {
            self.recording_hotkey = None;
            self.hotkey_hint = None;
        } else {
            self.recording_hotkey = Some(target);
            self.hotkey_hint = Some("Use Ctrl, Alt, or Shift + key, or F1–F12".to_string());
        }
        self.shared_recording
            .store(self.recording_hotkey.is_some(), Ordering::Relaxed);
    }

    fn find_hotkey_conflict(
        &self,
        target: HotkeyTarget,
        candidate: HotkeyBinding,
    ) -> Option<&'static str> {
        let existing: [(HotkeyTarget, &'static str, Option<HotkeyBinding>); 3] = [
            (HotkeyTarget::Fix, "Fix", Some(self.config.hotkey)),
            (
                HotkeyTarget::TogglePower,
                "On / Off",
                self.config.toggle_power_hotkey,
            ),
            (
                HotkeyTarget::ToggleHighlight,
                "Highlight mode",
                self.config.toggle_highlight_hotkey,
            ),
        ];
        existing
            .into_iter()
            .find(|(t, _, b)| *t != target && *b == Some(candidate))
            .map(|(_, name, _)| name)
    }

    fn handle_hotkey_recording(&mut self, ctx: &egui::Context) {
        let Some(target) = self.recording_hotkey else {
            return;
        };
        if ctx.wants_keyboard_input() {
            self.recording_hotkey = None;
            self.shared_recording.store(false, Ordering::Relaxed);
            self.hotkey_hint = None;
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
                    self.recording_hotkey = None;
                    self.shared_recording.store(false, Ordering::Relaxed);
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
                    if !candidate.is_valid() {
                        self.hotkey_hint =
                            Some("Add Ctrl, Alt, or Shift (or use F1–F12)".to_string());
                        continue;
                    }
                    if let Some(conflict_name) = self.find_hotkey_conflict(target, candidate) {
                        self.hotkey_hint =
                            Some(format!("Already used by {conflict_name} hotkey"));
                        return;
                    }
                    self.recording_hotkey = None;
                    self.shared_recording.store(false, Ordering::Relaxed);
                    self.hotkey_hint = None;
                    match target {
                        HotkeyTarget::Fix => {
                            self.config.hotkey = candidate;
                            *self.shared_hotkey.lock().unwrap() = candidate;
                            self.save_current_config();
                            let _ = self.ctrl_tx.send(CtrlCmd::UpdateHotkey(candidate));
                        }
                        HotkeyTarget::TogglePower => {
                            self.config.toggle_power_hotkey = Some(candidate);
                            self.save_current_config();
                            let _ = self
                                .ctrl_tx
                                .send(CtrlCmd::UpdateTogglePowerHotkey(Some(candidate)));
                        }
                        HotkeyTarget::ToggleHighlight => {
                            self.config.toggle_highlight_hotkey = Some(candidate);
                            self.save_current_config();
                            let _ = self
                                .ctrl_tx
                                .send(CtrlCmd::UpdateToggleHighlightHotkey(Some(candidate)));
                        }
                    }
                    return;
                }
            }
        }
    }

    fn persist_pending_on_exit(&mut self) {
        let latest_hl = self.shared_highlighted_only.load(Ordering::Relaxed);
        let hl_changed = self.config.fix_highlighted_only != latest_hl;
        if hl_changed {
            self.config.fix_highlighted_only = latest_hl;
        }
        if hl_changed
            || self.config_dirty_since.is_some()
            || self.config.window != self.last_saved_window
        {
            self.save_current_config();
            self.last_saved_window = self.config.window;
        }
    }
}

impl Drop for KeyboardLlmApp {
    fn drop(&mut self) {
        self.persist_pending_on_exit();
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
        self.flush_dirty_config(false);

        if ctx.input(|i| i.viewport().close_requested()) {
            let force_quit = self.state.lock().unwrap().force_quit;
            if !force_quit && self.config.minimize_to_tray_on_close {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if let Some(shell) = self.shell.as_ref() {
                    shell.hide_window();
                }
            } else {
                self.persist_pending_on_exit();
                let _ = self.ctrl_tx.send(CtrlCmd::ShutdownController);
            }
        }

        self.handle_hotkey_recording(ctx);

        let (
            phase,
            error,
            notice,
            active_model_path,
            active_compute_mode,
            picked_folder,
            hotkey_revert,
            toggle_power_hotkey_revert,
            toggle_highlight_hotkey_revert,
            highlight_sync,
        ) = {
            let mut s = self.state.lock().unwrap();
            (
                s.phase.clone(),
                s.error.clone(),
                s.notice.clone(),
                s.active_model_path.clone(),
                s.active_compute_mode,
                s.picked_folder.take(),
                s.hotkey_revert.take(),
                s.toggle_power_hotkey_revert.take(),
                s.toggle_highlight_hotkey_revert.take(),
                s.highlight_sync.take(),
            )
        };

        if let Some(folder) = picked_folder {
            self.apply_storage_dir(folder);
        }
        let mut config_dirty = false;
        if let Some(prev_hk) = hotkey_revert {
            self.config.hotkey = prev_hk;
            *self.shared_hotkey.lock().unwrap() = prev_hk;
            config_dirty = true;
        }
        if let Some(prev_power_hk) = toggle_power_hotkey_revert {
            self.config.toggle_power_hotkey = prev_power_hk;
            config_dirty = true;
        }
        if let Some(prev_hl_hk) = toggle_highlight_hotkey_revert {
            self.config.toggle_highlight_hotkey = prev_hl_hk;
            config_dirty = true;
        }
        if let Some(hl) = highlight_sync {
            if self.config.fix_highlighted_only != hl {
                self.config.fix_highlighted_only = hl;
                config_dirty = true;
            }
        }
        if config_dirty {
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
        let (cached_size, part_size) = self.file_cache.get(preview_path.as_ref(), &phase);
        let part_path = preview_path.as_ref().map(|p| partial_download_path(p));
        let is_different_from_active =
            is_on && preview_path.is_some() && preview_path != active_model_path;
        let is_compute_changed = is_on
            && !is_different_from_active
            && Some(self.config.compute_mode) != active_compute_mode;

        if self.config.dark_mode != self.applied_dark_mode {
            apply_theme_visuals(ctx, self.config.dark_mode);
            set_window_dark_titlebar(self.hwnd_raw, self.config.dark_mode);
            self.applied_dark_mode = self.config.dark_mode;
        }
        let theme = ThemeColors::for_mode(self.config.dark_mode);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme.bg_window)
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
                        egui::RichText::new("LLM Grammarly")
                            .size(17.5)
                            .strong()
                            .color(theme.text_primary),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (dot_color, status_text): (egui::Color32, String) = match &phase {
                            AppPhase::Off if cached_size.is_none() => {
                                (theme.dot_busy, "Setup".to_string())
                            }
                            AppPhase::Off => (theme.dot_off, "Off".to_string()),
                            AppPhase::Downloading {
                                downloaded, total, ..
                            } => {
                                if let Some(tot) = total.filter(|&t| t > 0) {
                                    let pct = ((*downloaded as f64 / tot as f64) * 100.0)
                                        .clamp(0.0, 100.0);
                                    (theme.dot_busy, format!("Downloading {pct:.0}%"))
                                } else {
                                    (theme.dot_busy, "Downloading...".to_string())
                                }
                            }
                            AppPhase::Loading => (theme.dot_busy, "Loading...".to_string()),
                            AppPhase::On { .. } => (theme.dot_ready, "Ready".to_string()),
                            AppPhase::Fixing { .. } => (theme.dot_busy, "Fixing...".to_string()),
                            AppPhase::Unloading => (theme.dot_busy, "Unloading...".to_string()),
                        };
                        egui::Frame::new()
                            .fill(theme.bg_card)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
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
                                            .color(theme.text_primary),
                                    );
                                });
                            });

                        ui.add_space(6.0);
                        if theme_switch(ui, self.config.dark_mode, &theme).clicked() {
                            self.config.dark_mode = !self.config.dark_mode;
                            apply_theme_visuals(ctx, self.config.dark_mode);
                            set_window_dark_titlebar(self.hwnd_raw, self.config.dark_mode);
                            self.applied_dark_mode = self.config.dark_mode;
                            self.save_current_config();
                        }
                    });
                });

                // 2. Main Power & Interactive Download Card
                card_frame(&theme).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

                    match &phase {
                        AppPhase::Off => {
                            if cached_size.is_none() {
                                egui::Frame::new()
                                    .fill(theme.bg_callout)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
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
                                                .color(theme.text_primary),
                                        );
                                        let desc = if let Some(preset) =
                                            preset_for_url(normalized_preview.as_deref())
                                        {
                                            format!(
                                                "Click below to download the {} model ({}) from Hugging Face to your storage folder.",
                                                preset.label, preset.summary
                                            )
                                        } else {
                                            "Click below to download the selected GGUF model from Hugging Face to your storage folder."
                                                .to_string()
                                        };
                                        ui.label(
                                            egui::RichText::new(desc)
                                                .size(11.5)
                                                .color(theme.text_muted),
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
                            if power_switch_row(ui, &theme, switch_label, false, true, false)
                                .clicked()
                            {
                                let url = self.url_input.clone();
                                if let Ok(norm) = normalize_model_url(&url) {
                                    self.url_input = norm.clone();
                                    self.config.model_url = norm.clone();
                                    *self.shared_url.lock().unwrap() = norm;
                                    self.save_current_config();
                                } else {
                                    self.flush_dirty_config(true);
                                }
                                let dir = self.current_storage_dir();
                                let _ = self.ctrl_tx.send(CtrlCmd::TurnOn(
                                    self.url_input.clone(),
                                    dir,
                                    self.config.compute_mode,
                                ));
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
                                                                .color(theme.text_muted),
                                                        )
                                                        .frame(false),
                                                    )
                                                    .clicked()
                                                {
                                                    crate::downloader::discard_partial_download(
                                                        part,
                                                    );
                                                    self.file_cache.invalidate();
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
                                .fill(theme.bg_callout)
                                .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
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
                                                .color(theme.text_primary),
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
                                                            .color(theme.text_primary),
                                                    );
                                                },
                                            );
                                        }
                                    });

                                    let frac = total
                                        .filter(|&t| t > 0)
                                        .map(|t| (*downloaded as f32 / t as f32).clamp(0.0, 1.0))
                                        .unwrap_or(0.0);
                                    draw_neutral_progress_bar(ui, &theme, frac, total.is_none());

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
                                        egui::RichText::new(stats)
                                            .size(11.5)
                                            .color(theme.text_primary),
                                    );

                                    let dest_short = shorten_middle(dest_display, 58);
                                    ui.label(
                                        egui::RichText::new(format!("Saving to: {dest_short}"))
                                            .size(11.0)
                                            .color(theme.text_muted),
                                    )
                                    .on_hover_text(dest_display);
                                });

                            if secondary_button(ui, &theme, "Pause / Cancel Download", 38.0)
                                .clicked()
                            {
                                let _ = self.ctrl_tx.send(CtrlCmd::CancelDownload);
                            }
                        }
                        AppPhase::Loading => {
                            let _ = power_switch_row(
                                ui,
                                &theme,
                                "Loading model...",
                                true,
                                false,
                                true,
                            );
                        }
                        AppPhase::On { .. } => {
                            if power_switch_row(ui, &theme, "Turn Off", true, true, false).clicked()
                            {
                                let _ = self.ctrl_tx.send(CtrlCmd::TurnOff);
                            }
                        }
                        AppPhase::Fixing { .. } => {
                            let _ =
                                power_switch_row(ui, &theme, "Fixing text...", true, false, true);
                        }
                        AppPhase::Unloading => {
                            let _ = power_switch_row(
                                ui,
                                &theme,
                                "Unloading model...",
                                false,
                                false,
                                true,
                            );
                        }
                    }

                    // Highlighted-only mode switch row
                    let is_fixing = matches!(phase, AppPhase::Fixing { .. });
                    let hl_resp = toggle_switch_row(
                        ui,
                        &theme,
                        egui::Id::new("fix_highlighted_only_switch"),
                        "Fix highlighted text only",
                        self.config.fix_highlighted_only,
                        !is_fixing,
                    )
                    .on_hover_text(
                        "When on, skips Ctrl+A and only corrects text you have highlighted",
                    );
                    if hl_resp.clicked() {
                        self.config.fix_highlighted_only = !self.config.fix_highlighted_only;
                        self.shared_highlighted_only
                            .store(self.config.fix_highlighted_only, Ordering::Relaxed);
                        self.save_current_config();
                        if self.config.show_notifications {
                            if let Some(sh) = self.shell.as_ref() {
                                notify_highlight_mode(sh, self.config.fix_highlighted_only, is_on);
                            }
                        }
                    }

                    // Hotkey rows (Fix, On / Off toggle, Highlight mode toggle)
                    let is_busy_hotkey = matches!(
                        phase,
                        AppPhase::Loading | AppPhase::Fixing { .. } | AppPhase::Unloading
                    );
                    let fix_desc = if self.config.fix_highlighted_only {
                        "Fix highlighted text"
                    } else {
                        "Select all & fix"
                    };
                    match render_hotkey_row(
                        ui,
                        &theme,
                        Some(self.config.hotkey.display_label()),
                        fix_desc,
                        self.recording_hotkey == Some(HotkeyTarget::Fix),
                        is_busy_hotkey,
                        (self.config.hotkey != HotkeyBinding::default()).then_some("Reset"),
                        "Change Hotkey",
                    ) {
                        HotkeyRowAction::ToggleRecord => self.toggle_recording(HotkeyTarget::Fix),
                        HotkeyRowAction::Secondary => {
                            let def = HotkeyBinding::default();
                            if let Some(conflict_name) =
                                self.find_hotkey_conflict(HotkeyTarget::Fix, def)
                            {
                                self.state.lock().unwrap().error = Some(format!(
                                    "Cannot reset: {} is currently used by {conflict_name} hotkey",
                                    def.display_label()
                                ));
                            } else {
                                self.config.hotkey = def;
                                *self.shared_hotkey.lock().unwrap() = def;
                                self.save_current_config();
                                let _ = self.ctrl_tx.send(CtrlCmd::UpdateHotkey(def));
                            }
                        }
                        HotkeyRowAction::None => {}
                    }

                    let is_busy_power = matches!(
                        phase,
                        AppPhase::Loading
                            | AppPhase::Fixing { .. }
                            | AppPhase::Unloading
                            | AppPhase::Downloading { .. }
                    );
                    match render_hotkey_row(
                        ui,
                        &theme,
                        self.config.toggle_power_hotkey.map(|b| b.display_label()),
                        "On / Off",
                        self.recording_hotkey == Some(HotkeyTarget::TogglePower),
                        is_busy_power,
                        self.config.toggle_power_hotkey.map(|_| "Clear"),
                        "Set Hotkey",
                    ) {
                        HotkeyRowAction::ToggleRecord => {
                            self.toggle_recording(HotkeyTarget::TogglePower)
                        }
                        HotkeyRowAction::Secondary => {
                            self.config.toggle_power_hotkey = None;
                            self.save_current_config();
                            let _ = self.ctrl_tx.send(CtrlCmd::UpdateTogglePowerHotkey(None));
                        }
                        HotkeyRowAction::None => {}
                    }

                    match render_hotkey_row(
                        ui,
                        &theme,
                        self.config.toggle_highlight_hotkey.map(|b| b.display_label()),
                        "Highlight mode",
                        self.recording_hotkey == Some(HotkeyTarget::ToggleHighlight),
                        false,
                        self.config.toggle_highlight_hotkey.map(|_| "Clear"),
                        "Set Hotkey",
                    ) {
                        HotkeyRowAction::ToggleRecord => {
                            self.toggle_recording(HotkeyTarget::ToggleHighlight)
                        }
                        HotkeyRowAction::Secondary => {
                            self.config.toggle_highlight_hotkey = None;
                            self.save_current_config();
                            let _ = self
                                .ctrl_tx
                                .send(CtrlCmd::UpdateToggleHighlightHotkey(None));
                        }
                        HotkeyRowAction::None => {}
                    }

                    if let Some(hint) = &self.hotkey_hint {
                        ui.label(egui::RichText::new(hint).size(11.0).color(theme.dot_busy));
                    }

                    // Status notice line
                    if matches!(phase, AppPhase::Fixing { .. }) {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new(&notice)
                                    .size(11.5)
                                    .color(theme.text_muted),
                            );
                        });
                    } else if !matches!(
                        phase,
                        AppPhase::Downloading { .. } | AppPhase::Loading | AppPhase::Unloading
                    ) {
                        ui.label(
                            egui::RichText::new(&notice)
                                .size(11.5)
                                .color(theme.text_muted),
                        );
                    }

                    if let Some(err_msg) = error {
                        egui::Frame::new()
                            .fill(theme.error_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.error_border))
                            .corner_radius(egui::CornerRadius::same(6))
                            .inner_margin(egui::Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(&err_msg)
                                            .size(11.5)
                                            .color(theme.error_text),
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
                card_frame(&theme).show(ui, |ui| {
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
                                .color(theme.text_primary),
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
                                    .color(theme.text_muted),
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
                            self.mark_config_dirty(ctx);
                        }
                    }
                    if resp.lost_focus() {
                        self.flush_dirty_config(true);
                    }

                    if let Some(norm) = normalized_preview.as_deref() {
                        ui.label(
                            egui::RichText::new(format!("File: {}", display_model_name(norm)))
                                .size(11.0)
                                .color(theme.text_muted),
                        );
                    }

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;

                        let dl_label = if is_different_from_active {
                            if cached_size.is_some() {
                                "Switch Model"
                            } else {
                                "Download & Switch"
                            }
                        } else if is_compute_changed {
                            "Reload Model"
                        } else if cached_size.is_some() {
                            "Model Ready"
                        } else if part_size.is_some() {
                            "Resume Download"
                        } else {
                            "Download Model"
                        };

                        let can_click_dl = !is_downloading
                            && !is_busy_loading
                            && (cached_size.is_none()
                                || is_different_from_active
                                || is_compute_changed);

                        if ui
                            .add_enabled(
                                can_click_dl,
                                egui::Button::new(
                                    egui::RichText::new(dl_label)
                                        .size(12.0)
                                        .color(theme.text_primary),
                                )
                                .fill(theme.btn_sec_bg)
                                .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                .corner_radius(egui::CornerRadius::same(6))
                                .min_size(egui::vec2(126.0, 27.0)),
                            )
                            .clicked()
                        {
                            let url = self.url_input.clone();
                            if let Ok(norm) = normalize_model_url(&url) {
                                self.url_input = norm.clone();
                                self.config.model_url = norm.clone();
                                *self.shared_url.lock().unwrap() = norm;
                                self.save_current_config();
                            } else {
                                self.flush_dirty_config(true);
                            }
                            let dir = self.current_storage_dir();
                            let _ = self.ctrl_tx.send(CtrlCmd::DownloadModel(
                                self.url_input.clone(),
                                dir,
                                self.config.compute_mode,
                            ));
                        }

                        for preset in MODEL_PRESETS {
                            let is_selected = normalized_preview.as_deref() == Some(preset.url);
                            if chip_button(
                                ui,
                                &theme,
                                preset.label,
                                is_selected,
                                !is_downloading,
                                egui::vec2(preset.min_width, 27.0),
                                12.0,
                            )
                            .on_hover_text(preset.tooltip)
                            .clicked()
                                && self.url_input.trim() != preset.url
                            {
                                self.apply_model_url(preset.url);
                                ctx.request_repaint();
                            }
                        }
                    });

                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Compute")
                                .size(12.0)
                                .color(theme.text_muted),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let can_change_compute = !is_downloading && !is_busy_loading;

                            if chip_button(
                                ui,
                                &theme,
                                "CPU Only",
                                self.config.compute_mode == ComputeMode::Cpu,
                                can_change_compute,
                                egui::vec2(74.0, 24.0),
                                11.5,
                            )
                            .on_hover_text(
                                "Run inference strictly on CPU (avoids Vulkan/iGPU driver overhead)",
                            )
                            .clicked()
                                && self.config.compute_mode != ComputeMode::Cpu
                            {
                                self.config.compute_mode = ComputeMode::Cpu;
                                *self.shared_compute_mode.lock().unwrap() = ComputeMode::Cpu;
                                self.save_current_config();
                            }

                            if chip_button(
                                ui,
                                &theme,
                                "Auto",
                                self.config.compute_mode == ComputeMode::Auto,
                                can_change_compute,
                                egui::vec2(56.0, 24.0),
                                11.5,
                            )
                            .on_hover_text("Use Vulkan GPU if available with automatic CPU fallback")
                            .clicked()
                                && self.config.compute_mode != ComputeMode::Auto
                            {
                                self.config.compute_mode = ComputeMode::Auto;
                                *self.shared_compute_mode.lock().unwrap() = ComputeMode::Auto;
                                self.save_current_config();
                            }
                        });
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
                                .color(theme.text_primary),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let tag = if is_custom_dir {
                                "Custom folder"
                            } else {
                                "App folder (.exe)"
                            };
                            ui.label(egui::RichText::new(tag).size(11.0).color(theme.text_muted));
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
                            self.mark_config_dirty(ctx);
                        }
                        if dir_resp.lost_focus() {
                            self.flush_dirty_config(true);
                        }

                        let browsing = self.browsing_folder.load(Ordering::Relaxed);
                        if ui
                            .add_enabled(
                                !is_downloading && !browsing,
                                egui::Button::new(
                                    egui::RichText::new("Browse...")
                                        .size(11.5)
                                        .color(theme.text_primary),
                                )
                                .fill(theme.btn_sec_bg)
                                .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                .corner_radius(egui::CornerRadius::same(6))
                                .min_size(egui::vec2(74.0, 26.0)),
                            )
                            .clicked()
                        {
                            self.browsing_folder.store(true, Ordering::Relaxed);
                            let guard = FolderPickerGuard {
                                flag: Arc::clone(&self.browsing_folder),
                                ctx: ctx.clone(),
                            };
                            let state_c = Arc::clone(&self.state);
                            let init_dir = self.current_storage_dir();
                            let _ = thread::Builder::new()
                                .name("folder-picker".to_string())
                                .spawn(move || {
                                    let _guard = guard;
                                    if let Some(chosen) = pick_folder_dialog(Some(&init_dir)) {
                                        state_c.lock().unwrap().picked_folder = Some(chosen);
                                    }
                                });
                        }

                        if is_custom_dir
                            && ui
                                .add_enabled(
                                    !is_downloading,
                                    egui::Button::new(
                                        egui::RichText::new("Reset")
                                            .size(11.5)
                                            .color(theme.text_muted),
                                    )
                                    .fill(egui::Color32::TRANSPARENT)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
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
                                    .color(theme.text_muted),
                            )
                            .changed()
                        {
                            self.save_current_config();
                        }

                        ui.add_space(10.0);

                        if ui
                            .checkbox(
                                &mut self.config.show_notifications,
                                egui::RichText::new("Notifications")
                                    .size(11.5)
                                    .color(theme.text_muted),
                            )
                            .on_hover_text(
                                "Show silent Windows notifications when On/Off or Highlight mode changes",
                            )
                            .changed()
                        {
                            self.shared_show_notifications
                                .store(self.config.show_notifications, Ordering::Relaxed);
                            self.save_current_config();
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Hide to Tray")
                                            .size(11.5)
                                            .color(theme.text_muted),
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
