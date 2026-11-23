use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::backup::{
    check_file_not_locked, probe_dir_writable, resolve_backup_dir, ResolvedBackupDir,
};
use crate::config::{
    default_backup_dir, default_model_dir, display_model_name, effective_model_dir,
    ignore_word_collides, load_config, local_model_path, normalize_ignore_words,
    normalize_model_url, partial_download_path, preset_for_url, save_config, AppConfig,
    ComputeMode, HotkeyBinding, IgnoreWordEntry, WindowState, MODEL_PRESETS,
};
use crate::controller::{controller_loop, notify_highlight_mode, AppPhase, CtrlCmd, SharedState};
use crate::doc_review::{
    length_deviation_pct, save_document, save_document_as, spawn_review_worker, DiffOp, DocFormat,
    DocWorkerEvent, LoadedDocument, ReviewDecision, ReviewScope,
};
use crate::hotkey::HotkeyController;
use crate::icon;
use crate::icon_render;
use crate::llm::{LlmHandle, PromptMode};
use crate::taskbar::{
    get_window_placement_state, pick_document_dialog, pick_folder_dialog, restore_window_position,
    save_document_dialog, set_window_dark_titlebar, ShellBridge, ShellCommand, TaskbarProgress,
};
use crate::widgets::{
    apply_theme_visuals, card_frame, chip_button, draw_neutral_progress_bar, font_regular,
    font_semibold, format_bytes, format_eta, format_speed, heading_with_subtitle, install_fonts,
    power_switch_row, render_hotkey_row, secondary_button, shorten_middle, text_bold,
    text_regular, text_semibold, theme_switch, toggle_switch_row, HotkeyRowAction, ThemeColors,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ActiveTab {
    #[default]
    LiveFix,
    ReviewDocs,
    IgnoreWords,
}

#[derive(Default)]
struct IgnoreWordsUiState {
    word_input: String,
    match_case: bool,
    ignore_word_boundary: bool,
    search_filter: String,
    notice: Option<(String, bool)>,
    confirm_clear_all: bool,
}

struct DocReviewUiState {
    doc: Option<LoadedDocument>,
    prompt_mode: PromptMode,
    scope: ReviewScope,
    current_batch: usize,
    side_by_side: bool,
    running: bool,
    cancel: Option<Arc<AtomicBool>>,
    progress_batch: usize,
    progress_total_batches: usize,
    progress_done: usize,
    progress_total: usize,
    worker_rx: Option<Receiver<DocWorkerEvent>>,
    worker_join: Option<JoinHandle<()>>,
    pending_start_after_load: Option<ReviewScope>,
    pending_load_seen_busy: bool,
    error: Option<String>,
    notice: Option<String>,
    backup_open: bool,
    backup_dir_input: String,
    resolved_backup: ResolvedBackupDir,
    picking_doc: Arc<AtomicBool>,
    picking_backup_dir: Arc<AtomicBool>,
    saving_as: Arc<AtomicBool>,
    picked_doc_slot: Arc<Mutex<Option<PathBuf>>>,
    picked_backup_slot: Arc<Mutex<Option<PathBuf>>>,
    save_as_slot: Arc<Mutex<Option<PathBuf>>>,
}

impl DocReviewUiState {
    fn new(cfg: &AppConfig) -> Self {
        let resolved_backup = resolve_backup_dir(cfg.backup_dir.as_deref());
        let backup_dir_input = resolved_backup.path.to_string_lossy().into_owned();
        Self {
            doc: None,
            prompt_mode: PromptMode::GrammarOnly,
            scope: ReviewScope::OneBatch,
            current_batch: 0,
            side_by_side: false,
            running: false,
            cancel: None,
            progress_batch: 0,
            progress_total_batches: 0,
            progress_done: 0,
            progress_total: 0,
            worker_rx: None,
            worker_join: None,
            pending_start_after_load: None,
            pending_load_seen_busy: false,
            error: None,
            notice: None,
            backup_open: false,
            backup_dir_input,
            resolved_backup,
            picking_doc: Arc::new(AtomicBool::new(false)),
            picking_backup_dir: Arc::new(AtomicBool::new(false)),
            saving_as: Arc::new(AtomicBool::new(false)),
            picked_doc_slot: Arc::new(Mutex::new(None)),
            picked_backup_slot: Arc::new(Mutex::new(None)),
            save_as_slot: Arc::new(Mutex::new(None)),
        }
    }
}

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
    doc_review_busy: Arc<AtomicBool>,
    llm: Arc<LlmHandle>,
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
    active_tab: ActiveTab,
    doc_state: DocReviewUiState,
    ignore_state: IgnoreWordsUiState,
}

impl KeyboardLlmApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let config = load_config();
        let dark_mode = config.dark_mode;
        install_fonts(&cc.egui_ctx);
        apply_theme_visuals(&cc.egui_ctx, dark_mode);

        let initial_window = config.window;
        let url_input = config.model_url.clone();
        let effective_dir = effective_model_dir(&config);
        let dir_input = effective_dir.to_string_lossy().into_owned();
        let doc_state = DocReviewUiState::new(&config);

        let shared_url = Arc::new(Mutex::new(url_input.clone()));
        let shared_dir = Arc::new(Mutex::new(effective_dir));
        let shared_compute_mode = Arc::new(Mutex::new(config.compute_mode));
        let shared_hotkey = Arc::new(Mutex::new(config.hotkey));
        let shared_highlighted_only = Arc::new(AtomicBool::new(config.fix_highlighted_only));
        let shared_show_notifications = Arc::new(AtomicBool::new(config.show_notifications));
        let shared_recording = Arc::new(AtomicBool::new(false));
        let doc_review_busy = Arc::new(AtomicBool::new(false));
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
        llm.set_ignore_words(config.ignore_words.clone());

        let tx_hk = ctrl_tx.clone();
        let ctx_hk = cc.egui_ctx.clone();
        let hotkey = HotkeyController::spawn(
            Arc::clone(&llm),
            Arc::clone(&shared_highlighted_only),
            Arc::clone(&shared_recording),
            Arc::clone(&doc_review_busy),
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
        let doc_busy_ctrl = Arc::clone(&doc_review_busy);
        let llm_ctrl = Arc::clone(&llm);
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
                    doc_busy_ctrl,
                    llm_ctrl,
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
            doc_review_busy,
            llm,
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
            active_tab: ActiveTab::LiveFix,
            doc_state,
            ignore_state: IgnoreWordsUiState::default(),
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

    fn load_review_doc(&mut self, path: PathBuf) {
        if self.doc_state.running {
            self.doc_state.error =
                Some("Cancel the active document review before opening another file.".to_string());
            return;
        }
        self.doc_state.pending_start_after_load = None;
        self.doc_state.pending_load_seen_busy = false;
        self.doc_state.error = None;
        self.doc_state.notice = None;
        match LoadedDocument::load(path) {
            Ok(doc) => {
                if let Err(lock_err) = check_file_not_locked(&doc.path) {
                    self.doc_state.error = Some(lock_err.to_string());
                } else {
                    self.doc_state.notice = Some(format!(
                        "Loaded {} ({} batch{}, {} paragraph{})",
                        doc.path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("document"),
                        doc.batches.len(),
                        if doc.batches.len() == 1 { "" } else { "es" },
                        doc.items.len(),
                        if doc.items.len() == 1 { "" } else { "s" },
                    ));
                }
                self.doc_state.doc = Some(doc);
                self.doc_state.current_batch = 0;
            }
            Err(err) => {
                self.doc_state.error = Some(err.to_string());
            }
        }
    }

    fn apply_backup_dir(&mut self, dir: PathBuf) {
        if probe_dir_writable(&dir) {
            if dir == default_backup_dir() {
                self.config.backup_dir = None;
            } else {
                self.config.backup_dir = Some(dir.to_string_lossy().into_owned());
            }
            self.doc_state.resolved_backup = resolve_backup_dir(self.config.backup_dir.as_deref());
            self.doc_state.backup_dir_input =
                self.doc_state.resolved_backup.path.to_string_lossy().into_owned();
            self.doc_state.error = None;
            self.save_current_config();
        } else {
            self.config.backup_dir = None;
            self.doc_state.resolved_backup = resolve_backup_dir(None);
            self.doc_state.backup_dir_input =
                self.doc_state.resolved_backup.path.to_string_lossy().into_owned();
            self.doc_state.error = Some(format!(
                "Backup folder {} is not writable; using {}",
                dir.display(),
                self.doc_state.resolved_backup.path.display()
            ));
            self.save_current_config();
        }
    }

    fn start_doc_review(&mut self, scope: ReviewScope, ctx: &egui::Context) {
        let Some(doc) = self.doc_state.doc.as_ref() else {
            return;
        };
        if doc.batches.is_empty() {
            self.doc_state.notice = Some("No editable paragraphs found in this file.".to_string());
            return;
        }
        if let Err(lock_err) = check_file_not_locked(&doc.path) {
            self.doc_state.error = Some(lock_err.to_string());
            return;
        }

        let mut work_items: Vec<(usize, usize, String)> = Vec::new();
        let total_batches = match scope {
            ReviewScope::OneBatch => {
                let b_idx = self.doc_state.current_batch.min(doc.batches.len() - 1);
                for &item_idx in &doc.batches[b_idx].item_indices {
                    work_items.push((0, item_idx, doc.items[item_idx].original.clone()));
                }
                1
            }
            ReviewScope::WholeFile => {
                for (b_idx, batch) in doc.batches.iter().enumerate() {
                    for &item_idx in &batch.item_indices {
                        work_items.push((b_idx, item_idx, doc.items[item_idx].original.clone()));
                    }
                }
                doc.batches.len()
            }
        };

        let total_items = work_items.len();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let preserve_markdown = doc.format == DocFormat::Markdown;
        let auto_accept_unflagged = scope == ReviewScope::WholeFile;

        match spawn_review_worker(
            Arc::clone(&self.llm),
            work_items,
            total_batches,
            self.doc_state.prompt_mode,
            preserve_markdown,
            auto_accept_unflagged,
            Arc::clone(&cancel),
            Arc::clone(&self.doc_review_busy),
            tx,
            ctx.clone(),
        ) {
            Ok(handle) => {
                self.doc_state.running = true;
                self.doc_state.cancel = Some(cancel);
                self.doc_state.worker_rx = Some(rx);
                self.doc_state.worker_join = Some(handle);
                self.doc_state.progress_batch = 0;
                self.doc_state.progress_total_batches = total_batches;
                self.doc_state.progress_done = 0;
                self.doc_state.progress_total = total_items;
                self.doc_state.error = None;
                self.doc_state.notice = None;
                if let Some(sh) = self.shell.as_ref() {
                    sh.set_progress(TaskbarProgress::Indeterminate);
                }
            }
            Err(err) => {
                self.doc_state.error = Some(err.to_string());
            }
        }
    }

    fn request_start_doc_review(
        &mut self,
        scope: ReviewScope,
        phase: &AppPhase,
        preview_cached: bool,
        ctx: &egui::Context,
    ) {
        if self.doc_state.running {
            return;
        }
        let Some(doc) = self.doc_state.doc.as_ref() else {
            self.doc_state.error = Some("Load a .txt, .md, or .docx file first.".to_string());
            return;
        };
        if doc.batches.is_empty() {
            self.doc_state.notice = Some("No editable paragraphs found in this file.".to_string());
            return;
        }
        if let Err(lock_err) = check_file_not_locked(&doc.path) {
            self.doc_state.error = Some(lock_err.to_string());
            return;
        }

        match phase {
            AppPhase::On { .. } => self.start_doc_review(scope, ctx),
            AppPhase::Off => {
                if preview_cached {
                    self.state.lock().unwrap().error = None;
                    self.doc_state.error = None;
                    self.doc_state.notice =
                        Some("Loading model to start document review...".to_string());
                    self.doc_state.pending_start_after_load = Some(scope);
                    self.doc_state.pending_load_seen_busy = false;
                    let url = self.url_input.clone();
                    if let Ok(norm) = normalize_model_url(&url) {
                        self.url_input = norm.clone();
                        self.config.model_url = norm.clone();
                        *self.shared_url.lock().unwrap() = norm;
                        self.save_current_config();
                    }
                    let dir = self.current_storage_dir();
                    let _ = self.ctrl_tx.send(CtrlCmd::TurnOn(
                        self.url_input.clone(),
                        dir,
                        self.config.compute_mode,
                    ));
                } else {
                    self.doc_state.error = Some(
                        "No local model downloaded yet. Download a model in Live Fix first."
                            .to_string(),
                    );
                }
            }
            _ => {
                self.doc_state.error =
                    Some("Model is currently busy — wait a moment and try again.".to_string());
            }
        }
    }

    fn perform_save(&mut self) {
        let Some(doc) = self.doc_state.doc.as_ref() else {
            return;
        };
        if !doc
            .items
            .iter()
            .any(|it| it.decision == ReviewDecision::Accepted && it.has_change())
        {
            self.doc_state.notice =
                Some("No accepted suggestions yet — click Accept on items to save.".to_string());
            return;
        }

        self.doc_state.resolved_backup = resolve_backup_dir(self.config.backup_dir.as_deref());
        self.doc_state.backup_dir_input =
            self.doc_state.resolved_backup.path.to_string_lossy().into_owned();
        let target_path = doc.path.clone();

        match save_document(
            doc,
            &self.doc_state.resolved_backup.path,
            self.config.backup_keep_count,
        ) {
            Ok(backup_path) => {
                let backup_name = backup_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("backup");
                if let Ok(reloaded) = LoadedDocument::load(target_path) {
                    let max_b = reloaded.batches.len().saturating_sub(1);
                    self.doc_state.current_batch = self.doc_state.current_batch.min(max_b);
                    self.doc_state.doc = Some(reloaded);
                }
                self.doc_state.error = None;
                self.doc_state.notice = Some(format!("Saved cleanly · Backup: {backup_name}"));
            }
            Err(err) => {
                self.doc_state.error = Some(err.to_string());
            }
        }
    }

    fn perform_save_as(&mut self, dest: PathBuf) {
        let Some(doc) = self.doc_state.doc.as_ref() else {
            return;
        };
        match save_document_as(doc, &dest) {
            Ok(()) => {
                if let Ok(reloaded) = LoadedDocument::load(dest.clone()) {
                    let max_b = reloaded.batches.len().saturating_sub(1);
                    self.doc_state.current_batch = self.doc_state.current_batch.min(max_b);
                    self.doc_state.doc = Some(reloaded);
                }
                self.doc_state.error = None;
                self.doc_state.notice = Some(format!("Saved copy to {}", dest.display()));
            }
            Err(err) => {
                self.doc_state.error = Some(err.to_string());
            }
        }
    }

    fn poll_doc_review_events(
        &mut self,
        ctx: &egui::Context,
        phase: &AppPhase,
        phase_error: Option<&String>,
    ) {
        let picked_doc = self.doc_state.picked_doc_slot.lock().unwrap().take();
        if let Some(path) = picked_doc {
            self.load_review_doc(path);
        }

        let picked_backup = self.doc_state.picked_backup_slot.lock().unwrap().take();
        if let Some(dir) = picked_backup {
            self.apply_backup_dir(dir);
        }

        let save_as_dest = self.doc_state.save_as_slot.lock().unwrap().take();
        if let Some(dest) = save_as_dest {
            self.perform_save_as(dest);
        }

        if self.active_tab == ActiveTab::ReviewDocs {
            let dropped = ctx.input(|i| i.raw.dropped_files.clone());
            if let Some(path) = dropped.into_iter().find_map(|f| f.path) {
                self.load_review_doc(path);
            }
        }

        let mut events = Vec::new();
        if let Some(rx) = self.doc_state.worker_rx.as_ref() {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }

        for ev in events {
            match ev {
                DocWorkerEvent::Progress {
                    batch_index,
                    total_batches,
                    completed_items,
                    total_items,
                } => {
                    self.doc_state.progress_batch = batch_index;
                    self.doc_state.progress_total_batches = total_batches;
                    self.doc_state.progress_done = completed_items;
                    self.doc_state.progress_total = total_items;
                    if let Some(sh) = self.shell.as_ref() {
                        if total_items > 0 {
                            sh.set_progress(TaskbarProgress::Determinate {
                                current: completed_items as u64,
                                total: total_items as u64,
                            });
                        }
                    }
                }
                DocWorkerEvent::ItemDone {
                    item_index,
                    res,
                    auto_accept_unflagged,
                } => {
                    if let Some(doc) = self.doc_state.doc.as_mut() {
                        if let Some(item) = doc.items.get_mut(item_index) {
                            match res {
                                Ok(fixed) => {
                                    let changed = fixed != item.original;
                                    item.diff_ops = if changed {
                                        crate::doc_review::compute_word_diff(&item.original, &fixed)
                                    } else {
                                        Vec::new()
                                    };
                                    let delta = if changed {
                                        length_deviation_pct(&item.original, &fixed)
                                    } else {
                                        None
                                    };
                                    item.length_delta_pct = delta;
                                    item.error = None;
                                    item.decision = if changed
                                        && auto_accept_unflagged
                                        && delta.is_none()
                                    {
                                        ReviewDecision::Accepted
                                    } else {
                                        ReviewDecision::Pending
                                    };
                                    item.suggested = Some(fixed);
                                }
                                Err(err_msg) => {
                                    item.error = Some(err_msg);
                                }
                            }
                        }
                    }
                }
                DocWorkerEvent::Finished { cancelled } => {
                    self.doc_state.running = false;
                    self.doc_state.cancel = None;
                    self.doc_state.worker_rx = None;
                    if let Some(join) = self.doc_state.worker_join.take() {
                        let _ = join.join();
                    }
                    if let Some(sh) = self.shell.as_ref() {
                        sh.set_progress(TaskbarProgress::None);
                    }
                    let flagged_pending = self
                        .doc_state
                        .doc
                        .as_ref()
                        .map(|d| d.stats().3)
                        .unwrap_or(0);
                    self.doc_state.notice = Some(if cancelled {
                        "Cancelled — completed items remain staged below; file unchanged."
                            .to_string()
                    } else if flagged_pending > 0 {
                        format!(
                            "Review complete — {flagged_pending} chunk(s) changed length by >25% and need review."
                        )
                    } else {
                        "Review complete — inspect changes below and click Save to apply."
                            .to_string()
                    });
                }
            }
        }

        if let Some(pending_scope) = self.doc_state.pending_start_after_load {
            match phase {
                AppPhase::Loading | AppPhase::Downloading { .. } => {
                    self.doc_state.pending_load_seen_busy = true;
                }
                AppPhase::On { .. } => {
                    self.doc_state.pending_start_after_load = None;
                    self.doc_state.pending_load_seen_busy = false;
                    self.start_doc_review(pending_scope, ctx);
                }
                AppPhase::Off if phase_error.is_some() || self.doc_state.pending_load_seen_busy => {
                    self.doc_state.pending_start_after_load = None;
                    self.doc_state.pending_load_seen_busy = false;
                    if phase_error.is_some() {
                        self.doc_state.error = phase_error.cloned();
                    } else {
                        self.doc_state.notice = Some("Model load cancelled.".to_string());
                    }
                }
                _ => {}
            }
        }
    }

    fn render_review_docs_tab(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        theme: &ThemeColors,
        phase: &AppPhase,
        preview_cached: bool,
    ) {
        let model_busy = matches!(
            phase,
            AppPhase::Downloading { .. }
                | AppPhase::Loading
                | AppPhase::Unloading
                | AppPhase::Fixing { .. }
        );
        let running = self.doc_state.running;
        let is_hovering_drop = ctx.input(|i| !i.raw.hovered_files.is_empty());

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

                // 1. Document Picker & Mode Card
                card_frame(theme).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                    ui.horizontal(|ui| {
                        ui.label(heading_with_subtitle(
                            "Document Review",
                            "(.txt, .md, .docx)",
                            15.0,
                            12.0,
                            theme,
                        ));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let picking = self.doc_state.picking_doc.load(Ordering::Relaxed);
                            if ui
                                .add_enabled(
                                    !running && !picking,
                                    egui::Button::new(text_semibold(
                                        "Browse File...",
                                        12.5,
                                        theme.text_primary,
                                    ))
                                    .fill(theme.btn_sec_bg)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .min_size(egui::vec2(100.0, 28.0)),
                                )
                                .clicked()
                            {
                                self.doc_state.picking_doc.store(true, Ordering::Relaxed);
                                let guard = FolderPickerGuard {
                                    flag: Arc::clone(&self.doc_state.picking_doc),
                                    ctx: ctx.clone(),
                                };
                                let slot = Arc::clone(&self.doc_state.picked_doc_slot);
                                let init_dir = self
                                    .doc_state
                                    .doc
                                    .as_ref()
                                    .and_then(|d| d.path.parent().map(PathBuf::from));
                                let _ = thread::Builder::new()
                                    .name("doc-picker".to_string())
                                    .spawn(move || {
                                        let _guard = guard;
                                        if let Some(chosen) =
                                            pick_document_dialog(init_dir.as_deref())
                                        {
                                            *slot.lock().unwrap() = Some(chosen);
                                        }
                                    });
                            }
                        });
                    });

                    // Drop zone / loaded file summary
                    let drop_border = if is_hovering_drop {
                        theme.accent_fill
                    } else {
                        theme.border_subtle
                    };
                    egui::Frame::new()
                        .fill(theme.bg_callout)
                        .stroke(egui::Stroke::new(1.0_f32, drop_border))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if let Some(doc) = self.doc_state.doc.as_ref() {
                                let fname = doc
                                    .path
                                    .file_name()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("document");
                                ui.horizontal(|ui| {
                                    ui.label(text_semibold(
                                        shorten_middle(fname, 36),
                                        13.5,
                                        theme.text_primary,
                                    ))
                                    .on_hover_text(doc.path.to_string_lossy().as_ref());
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(text_regular(
                                                format!(
                                                    "{} · {} batch{} · {} chunk{}",
                                                    doc.format.label(),
                                                    doc.batches.len(),
                                                    if doc.batches.len() == 1 { "" } else { "es" },
                                                    doc.items.len(),
                                                    if doc.items.len() == 1 { "" } else { "s" },
                                                ),
                                                11.5,
                                                theme.text_muted,
                                            ));
                                        },
                                    );
                                });
                            } else {
                                let drop_rt = if is_hovering_drop {
                                    text_semibold(
                                        "Drop .txt, .md, or .docx file to load",
                                        12.5,
                                        theme.text_primary,
                                    )
                                } else {
                                    text_regular(
                                        "Drag & drop a .txt, .md, or .docx file here — or click Browse File...",
                                        12.0,
                                        theme.text_muted,
                                    )
                                };
                                ui.label(drop_rt);
                            }
                        });

                    // Prompt Mode selector
                    ui.horizontal(|ui| {
                        ui.label(text_semibold("Prompt Mode", 13.0, theme.text_primary));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if chip_button(
                                ui,
                                theme,
                                "Rewrite for Clarity",
                                self.doc_state.prompt_mode == PromptMode::RewriteClarity,
                                !running,
                                egui::vec2(130.0, 26.0),
                                12.0,
                            )
                            .clicked()
                            {
                                self.doc_state.prompt_mode = PromptMode::RewriteClarity;
                            }
                            if chip_button(
                                ui,
                                theme,
                                "Spelling / Grammar Only",
                                self.doc_state.prompt_mode == PromptMode::GrammarOnly,
                                !running,
                                egui::vec2(156.0, 26.0),
                                12.0,
                            )
                            .clicked()
                            {
                                self.doc_state.prompt_mode = PromptMode::GrammarOnly;
                            }
                        });
                    });

                    // Review Scope selector
                    ui.horizontal(|ui| {
                        ui.label(text_semibold("Review Scope", 13.0, theme.text_primary));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if chip_button(
                                ui,
                                theme,
                                "Fix Whole File",
                                self.doc_state.scope == ReviewScope::WholeFile,
                                !running,
                                egui::vec2(108.0, 26.0),
                                12.0,
                            )
                            .clicked()
                            {
                                self.doc_state.scope = ReviewScope::WholeFile;
                            }
                            if chip_button(
                                ui,
                                theme,
                                "One Batch at a Time",
                                self.doc_state.scope == ReviewScope::OneBatch,
                                !running,
                                egui::vec2(144.0, 26.0),
                                12.0,
                            )
                            .clicked()
                            {
                                self.doc_state.scope = ReviewScope::OneBatch;
                            }
                        });
                    });

                    if running {
                        let tot = self.doc_state.progress_total;
                        let done = self.doc_state.progress_done;
                        let frac = if tot > 0 {
                            (done as f32 / tot as f32).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        let pct = (frac * 100.0).round();
                        let b_cur = (self.doc_state.progress_batch + 1)
                            .min(self.doc_state.progress_total_batches.max(1));
                        let b_tot = self.doc_state.progress_total_batches.max(1);

                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(text_semibold(
                                format!("Fixing batch {b_cur} of {b_tot} ({pct:.0}%)"),
                                13.5,
                                theme.text_primary,
                            ));
                        });
                        draw_neutral_progress_bar(ui, theme, frac, tot == 0);
                        if secondary_button(ui, theme, "Cancel", 32.0).clicked() {
                            if let Some(c) = self.doc_state.cancel.as_ref() {
                                c.store(true, Ordering::Relaxed);
                            }
                        }
                    } else if self.doc_state.pending_start_after_load.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(text_regular(
                                "Loading model before starting review...",
                                12.0,
                                theme.text_muted,
                            ));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(text_semibold(
                                                "Cancel",
                                                11.5,
                                                theme.text_primary,
                                            ))
                                            .fill(theme.btn_sec_bg)
                                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                            .corner_radius(egui::CornerRadius::same(6))
                                            .min_size(egui::vec2(66.0, 25.0)),
                                        )
                                        .clicked()
                                    {
                                        self.doc_state.pending_start_after_load = None;
                                        self.doc_state.pending_load_seen_busy = false;
                                        self.doc_state.notice =
                                            Some("Document review cancelled.".to_string());
                                    }
                                },
                            );
                        });
                    } else {
                        let can_start = self.doc_state.doc.is_some() && !model_busy;
                        let run_label = match self.doc_state.scope {
                            ReviewScope::OneBatch => "Review Current Batch",
                            ReviewScope::WholeFile => "Fix Whole File",
                        };
                        let (fill, text_col, stroke) = if can_start {
                            (
                                theme.accent_fill,
                                theme.bg_card,
                                egui::Stroke::NONE,
                            )
                        } else {
                            (
                                theme.btn_sec_bg,
                                theme.text_muted,
                                egui::Stroke::new(1.0_f32, theme.border_med),
                            )
                        };
                        let clicked = ui
                            .with_layout(egui::Layout::top_down_justified(egui::Align::Center), |ui| {
                                ui.add_enabled(
                                    can_start,
                                    egui::Button::new(text_bold(run_label, 14.0, text_col))
                                        .fill(fill)
                                        .stroke(stroke)
                                        .corner_radius(egui::CornerRadius::same(8))
                                        .min_size(egui::vec2(ui.available_width(), 38.0)),
                                )
                                .clicked()
                            })
                            .inner;
                        if clicked {
                            self.request_start_doc_review(
                                self.doc_state.scope,
                                phase,
                                preview_cached,
                                ctx,
                            );
                        }
                    }

                    if let Some(msg) = self.doc_state.notice.as_ref() {
                        ui.label(text_regular(msg, 12.0, theme.text_muted));
                    }

                    if let Some(err_msg) = self.doc_state.error.clone() {
                        egui::Frame::new()
                            .fill(theme.error_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.error_border))
                            .corner_radius(egui::CornerRadius::same(6))
                            .inner_margin(egui::Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(text_semibold(&err_msg, 12.0, theme.error_text));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.small_button("×").clicked() {
                                                self.doc_state.error = None;
                                            }
                                        },
                                    );
                                });
                            });
                    }
                });

                // 2. Review / Diff Panel
                if let Some(doc) = self.doc_state.doc.as_ref() {
                    let batch_count = doc.batches.len();
                    let cur_batch = if batch_count == 0 {
                        0
                    } else {
                        self.doc_state.current_batch.min(batch_count - 1)
                    };
                    let cur_batch_words = doc.batches.get(cur_batch).map_or(0, |b| b.word_count);
                    self.doc_state.current_batch = cur_batch;
                    let (_reviewed, accepted, pending_rev, flagged_pending) = doc.stats();

                    card_frame(theme).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                        // Navigation + Accept All in Batch
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    cur_batch > 0,
                                    egui::Button::new(text_semibold(
                                        "Prev",
                                        12.0,
                                        theme.text_primary,
                                    ))
                                    .fill(theme.btn_sec_bg)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                    .corner_radius(egui::CornerRadius::same(5))
                                    .min_size(egui::vec2(54.0, 26.0)),
                                )
                                .clicked()
                            {
                                self.doc_state.current_batch = cur_batch.saturating_sub(1);
                            }

                            if batch_count == 0 {
                                ui.label(text_bold("Batch 0 of 0", 13.5, theme.text_primary));
                            } else {
                                ui.label(heading_with_subtitle(
                                    &format!("Batch {} of {}", cur_batch + 1, batch_count),
                                    &format!("({cur_batch_words} words)"),
                                    13.5,
                                    11.5,
                                    theme,
                                ));
                            }

                            if ui
                                .add_enabled(
                                    cur_batch + 1 < batch_count,
                                    egui::Button::new(text_semibold(
                                        "Next",
                                        12.0,
                                        theme.text_primary,
                                    ))
                                    .fill(theme.btn_sec_bg)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                    .corner_radius(egui::CornerRadius::same(5))
                                    .min_size(egui::vec2(54.0, 26.0)),
                                )
                                .clicked()
                            {
                                self.doc_state.current_batch = (cur_batch + 1).min(batch_count - 1);
                            }

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .add_enabled(
                                        !running && batch_count > 0,
                                        egui::Button::new(text_semibold(
                                            "Accept All in Batch",
                                            12.0,
                                            theme.text_primary,
                                        ))
                                        .fill(theme.btn_sec_bg)
                                        .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                        .corner_radius(egui::CornerRadius::same(5))
                                        .min_size(egui::vec2(130.0, 26.0)),
                                    )
                                    .clicked()
                                {
                                    if let Some(d) = self.doc_state.doc.as_mut() {
                                        d.accept_all_in_batch(cur_batch);
                                    }
                                }
                            });
                        });

                        // Summary & optional side-by-side toggle on wide windows
                        let allow_sbs = ui.available_width() >= 700.0;
                        ui.horizontal(|ui| {
                            ui.label(text_regular(
                                format!(
                                    "{accepted} accepted · {pending_rev} pending · {flagged_pending} flagged (>±25%)"
                                ),
                                11.5,
                                theme.text_muted,
                            ));
                            if allow_sbs {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if chip_button(
                                            ui,
                                            theme,
                                            "Side-by-side",
                                            self.doc_state.side_by_side,
                                            true,
                                            egui::vec2(90.0, 24.0),
                                            11.5,
                                        )
                                        .clicked()
                                        {
                                            self.doc_state.side_by_side = true;
                                        }
                                        if chip_button(
                                            ui,
                                            theme,
                                            "Inline",
                                            !self.doc_state.side_by_side,
                                            true,
                                            egui::vec2(60.0, 24.0),
                                            11.5,
                                        )
                                        .clicked()
                                        {
                                            self.doc_state.side_by_side = false;
                                        }
                                    },
                                );
                            }
                        });

                        // Collect item actions in this frame and apply after borrowing immutably
                        let mut item_actions: Vec<(usize, ReviewDecision)> = Vec::new();
                        if let Some(batch) = self
                            .doc_state
                            .doc
                            .as_ref()
                            .and_then(|d| d.batches.get(cur_batch))
                        {
                            let items_ref = &self.doc_state.doc.as_ref().unwrap().items;
                            for &item_idx in &batch.item_indices {
                                let item = &items_ref[item_idx];
                                egui::Frame::new()
                                    .fill(theme.bg_callout)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .inner_margin(egui::Margin::same(10))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.spacing_mut().item_spacing.y = 6.0;

                                        ui.horizontal(|ui| {
                                            ui.label(text_bold(
                                                format!("#{}", item.id + 1),
                                                12.5,
                                                theme.text_primary,
                                            ));
                                            let (status_lbl, status_col) =
                                                if item.suggested.is_some() && !item.has_change() {
                                                    ("No changes", theme.text_muted)
                                                } else {
                                                    match item.decision {
                                                        ReviewDecision::Accepted => {
                                                            ("Accepted", theme.dot_ready)
                                                        }
                                                        ReviewDecision::Rejected => {
                                                            ("Rejected", theme.text_muted)
                                                        }
                                                        ReviewDecision::Pending => {
                                                            if item.suggested.is_some() {
                                                                ("Pending review", theme.dot_busy)
                                                            } else {
                                                                ("Not reviewed", theme.dot_off)
                                                            }
                                                        }
                                                    }
                                                };
                                            ui.label(text_semibold(status_lbl, 11.5, status_col));

                                            if let Some(pct) = item.length_delta_pct {
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        egui::Frame::new()
                                                            .fill(theme.bg_card)
                                                            .stroke(egui::Stroke::new(
                                                                1.0_f32,
                                                                theme.dot_busy,
                                                            ))
                                                            .corner_radius(egui::CornerRadius::same(
                                                                4,
                                                            ))
                                                            .inner_margin(egui::Margin::symmetric(
                                                                6, 2,
                                                            ))
                                                            .show(ui, |ui| {
                                                                ui.label(text_semibold(
                                                                    format!(
                                                                        "Length {pct:+.0}% — review"
                                                                    ),
                                                                    11.0,
                                                                    theme.dot_busy,
                                                                ));
                                                            });
                                                    },
                                                );
                                            }
                                        });

                                        if let Some(err) = item.error.as_deref() {
                                            ui.label(text_semibold(
                                                format!("Error: {err}"),
                                                12.0,
                                                theme.error_text,
                                            ));
                                        } else if item.suggested.is_some() {
                                            if !item.has_change() {
                                                ui.label(text_regular(
                                                    "No changes suggested.",
                                                    11.5,
                                                    theme.text_muted,
                                                ));
                                                ui.label(text_regular(
                                                    &item.original,
                                                    13.0,
                                                    theme.text_primary,
                                                ));
                                            } else {
                                                let ops = &item.diff_ops;
                                                if allow_sbs && self.doc_state.side_by_side {
                                                    let (orig_job, sug_job) =
                                                        build_side_diff_jobs(&ops, theme);
                                                    ui.columns(2, |cols| {
                                                        cols[0].label(text_semibold(
                                                            "Original",
                                                            11.5,
                                                            theme.text_muted,
                                                        ));
                                                        cols[0].label(orig_job);
                                                        cols[1].label(text_semibold(
                                                            "Suggested",
                                                            11.5,
                                                            theme.text_muted,
                                                        ));
                                                        cols[1].label(sug_job);
                                                    });
                                                } else {
                                                    let job = build_inline_diff_job(&ops, theme);
                                                    ui.label(job);
                                                }

                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 6.0;
                                                    if chip_button(
                                                        ui,
                                                        theme,
                                                        "Accept",
                                                        item.decision == ReviewDecision::Accepted,
                                                        !running,
                                                        egui::vec2(68.0, 24.0),
                                                        11.5,
                                                    )
                                                    .clicked()
                                                    {
                                                        item_actions.push((
                                                            item_idx,
                                                            ReviewDecision::Accepted,
                                                        ));
                                                    }
                                                    if chip_button(
                                                        ui,
                                                        theme,
                                                        "Reject",
                                                        item.decision == ReviewDecision::Rejected,
                                                        !running,
                                                        egui::vec2(64.0, 24.0),
                                                        11.5,
                                                    )
                                                    .clicked()
                                                    {
                                                        item_actions.push((
                                                            item_idx,
                                                            ReviewDecision::Rejected,
                                                        ));
                                                    }
                                                });
                                            }
                                        } else {
                                            ui.label(text_regular(
                                                &item.original,
                                                13.0,
                                                theme.text_muted,
                                            ));
                                        }
                                    });
                            }
                        }

                        if let Some(d) = self.doc_state.doc.as_mut() {
                            for (idx, dec) in item_actions {
                                d.items[idx].decision = dec;
                            }
                        }

                        // Save & Save As row
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if ui
                                .add_enabled(
                                    !running && accepted > 0,
                                    egui::Button::new(text_bold(
                                        "Save (with Backup)",
                                        12.5,
                                        theme.text_primary,
                                    ))
                                    .fill(theme.btn_sec_bg)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .min_size(egui::vec2(148.0, 30.0)),
                                )
                                .clicked()
                            {
                                self.perform_save();
                            }

                            let saving_as = self.doc_state.saving_as.load(Ordering::Relaxed);
                            if ui
                                .add_enabled(
                                    !running && !saving_as,
                                    egui::Button::new(text_semibold(
                                        "Save As...",
                                        12.5,
                                        theme.text_primary,
                                    ))
                                    .fill(theme.btn_sec_bg)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .min_size(egui::vec2(90.0, 30.0)),
                                )
                                .clicked()
                            {
                                if let Some(d) = self.doc_state.doc.as_ref() {
                                    self.doc_state.saving_as.store(true, Ordering::Relaxed);
                                    let guard = FolderPickerGuard {
                                        flag: Arc::clone(&self.doc_state.saving_as),
                                        ctx: ctx.clone(),
                                    };
                                    let slot = Arc::clone(&self.doc_state.save_as_slot);
                                    let init_dir = d.path.parent().map(PathBuf::from);
                                    let stem = d
                                        .path
                                        .file_stem()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("document");
                                    let ext = d.format.default_extension().to_string();
                                    let default_name = format!("{stem}_fixed.{ext}");
                                    let _ = thread::Builder::new()
                                        .name("doc-save-as".to_string())
                                        .spawn(move || {
                                            let _guard = guard;
                                            if let Some(dest) = save_document_dialog(
                                                init_dir.as_deref(),
                                                &default_name,
                                                &ext,
                                            ) {
                                                *slot.lock().unwrap() = Some(dest);
                                            }
                                        });
                                }
                            }
                        });
                    });
                }

                // 3. Backup Settings Card (collapsible, matching Model Storage Folder row)
                card_frame(theme).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                    let folder_tag = if self.doc_state.resolved_backup.is_custom {
                        "Custom folder"
                    } else if self.doc_state.resolved_backup.used_fallback {
                        "LocalAppData fallback"
                    } else {
                        "App folder (.exe)"
                    };

                    ui.horizontal(|ui| {
                        let arrow = if self.doc_state.backup_open {
                            "▾ Backup Settings"
                        } else {
                            "▸ Backup Settings"
                        };
                        if ui
                            .add(
                                egui::Button::new(text_bold(arrow, 15.0, theme.text_primary))
                                    .frame(false),
                            )
                            .clicked()
                        {
                            self.doc_state.backup_open = !self.doc_state.backup_open;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(text_regular(
                                format!(
                                    "Keep {} · {folder_tag}",
                                    self.config.backup_keep_count
                                ),
                                11.5,
                                theme.text_muted,
                            ));
                        });
                    });

                    if self.doc_state.backup_open {
                        let is_custom_backup = self.config.backup_dir.is_some();
                        ui.horizontal(|ui| {
                            let btn_space = if is_custom_backup { 162.0 } else { 100.0 };
                            let edit_w = (ui.available_width() - btn_space).max(120.0);
                            let dir_resp = ui.add_enabled(
                                !running,
                                egui::TextEdit::singleline(&mut self.doc_state.backup_dir_input)
                                    .desired_width(edit_w)
                                    .hint_text("Folder to save document backups")
                                    .margin(egui::Margin::symmetric(8, 5)),
                            );
                            if dir_resp.lost_focus() {
                                let trimmed = self.doc_state.backup_dir_input.trim().to_string();
                                if trimmed.is_empty() {
                                    self.config.backup_dir = None;
                                    self.doc_state.resolved_backup = resolve_backup_dir(None);
                                    self.doc_state.backup_dir_input = self
                                        .doc_state
                                        .resolved_backup
                                        .path
                                        .to_string_lossy()
                                        .into_owned();
                                    self.save_current_config();
                                } else {
                                    let cand = PathBuf::from(&trimmed);
                                    if cand != self.doc_state.resolved_backup.path {
                                        self.apply_backup_dir(cand);
                                    }
                                }
                            }

                            let browsing =
                                self.doc_state.picking_backup_dir.load(Ordering::Relaxed);
                            if ui
                                .add_enabled(
                                    !running && !browsing,
                                    egui::Button::new(text_semibold(
                                        "Browse...",
                                        12.0,
                                        theme.text_primary,
                                    ))
                                    .fill(theme.btn_sec_bg)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .min_size(egui::vec2(78.0, 27.0)),
                                )
                                .clicked()
                            {
                                self.doc_state
                                    .picking_backup_dir
                                    .store(true, Ordering::Relaxed);
                                let guard = FolderPickerGuard {
                                    flag: Arc::clone(&self.doc_state.picking_backup_dir),
                                    ctx: ctx.clone(),
                                };
                                let slot = Arc::clone(&self.doc_state.picked_backup_slot);
                                let init_dir = self.doc_state.resolved_backup.path.clone();
                                let _ = thread::Builder::new()
                                    .name("backup-folder-picker".to_string())
                                    .spawn(move || {
                                        let _guard = guard;
                                        if let Some(chosen) = pick_folder_dialog(Some(&init_dir)) {
                                            *slot.lock().unwrap() = Some(chosen);
                                        }
                                    });
                            }

                            if is_custom_backup
                                && ui
                                    .add_enabled(
                                        !running,
                                        egui::Button::new(text_regular(
                                            "Reset",
                                            11.5,
                                            theme.text_muted,
                                        ))
                                        .fill(egui::Color32::TRANSPARENT)
                                        .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                                        .corner_radius(egui::CornerRadius::same(6))
                                        .min_size(egui::vec2(54.0, 27.0)),
                                    )
                                    .clicked()
                            {
                                self.config.backup_dir = None;
                                self.doc_state.resolved_backup = resolve_backup_dir(None);
                                self.doc_state.backup_dir_input = self
                                    .doc_state
                                    .resolved_backup
                                    .path
                                    .to_string_lossy()
                                    .into_owned();
                                self.save_current_config();
                            }
                        });

                        ui.horizontal(|ui| {
                            ui.label(text_semibold(
                                "Keep last backups",
                                13.0,
                                theme.text_primary,
                            ));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                if ui
                                    .add_enabled(
                                        !running && self.config.backup_keep_count < 500,
                                        egui::Button::new(text_bold(
                                            "+",
                                            13.0,
                                            theme.text_primary,
                                        ))
                                        .fill(theme.btn_sec_bg)
                                        .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                        .corner_radius(egui::CornerRadius::same(5))
                                        .min_size(egui::vec2(28.0, 24.0)),
                                    )
                                    .clicked()
                                {
                                    self.config.backup_keep_count =
                                        (self.config.backup_keep_count + 1).min(500);
                                    self.save_current_config();
                                }

                                ui.label(text_bold(
                                    format!("{}", self.config.backup_keep_count),
                                    13.0,
                                    theme.text_primary,
                                ));

                                if ui
                                    .add_enabled(
                                        !running && self.config.backup_keep_count > 1,
                                        egui::Button::new(text_bold(
                                            "-",
                                            13.0,
                                            theme.text_primary,
                                        ))
                                        .fill(theme.btn_sec_bg)
                                        .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                        .corner_radius(egui::CornerRadius::same(5))
                                        .min_size(egui::vec2(28.0, 24.0)),
                                    )
                                    .clicked()
                                {
                                    self.config.backup_keep_count =
                                        self.config.backup_keep_count.saturating_sub(1).max(1);
                                    self.save_current_config();
                                }
                            });
                        });
                    }
                });
            });
    }

    fn sync_ignore_words(&mut self) {
        self.config.ignore_words =
            normalize_ignore_words(std::mem::take(&mut self.config.ignore_words));
        self.llm.set_ignore_words(self.config.ignore_words.clone());
        self.save_current_config();
    }

    fn try_add_ignore_words(&mut self) {
        let candidates: Vec<String> = self
            .ignore_state
            .word_input
            .split('\n')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        if candidates.is_empty() {
            self.ignore_state.notice = Some(("Enter a word or phrase first.".to_string(), true));
            return;
        }

        let mut added = 0usize;
        let mut skipped = 0usize;
        for word in candidates {
            let entry = IgnoreWordEntry {
                word,
                match_case: self.ignore_state.match_case,
                ignore_word_boundary: self.ignore_state.ignore_word_boundary,
            };
            if self
                .config
                .ignore_words
                .iter()
                .any(|existing| ignore_word_collides(existing, &entry))
            {
                skipped += 1;
            } else {
                self.config.ignore_words.push(entry);
                added += 1;
            }
        }

        self.ignore_state.confirm_clear_all = false;
        if added > 0 {
            self.ignore_state.word_input.clear();
            self.sync_ignore_words();
            let msg = if added == 1 && skipped == 0 {
                "Added 1 word to Ignore Words.".to_string()
            } else if skipped == 0 {
                format!("Added {added} words to Ignore Words.")
            } else {
                format!("Added {added}, skipped {skipped} duplicate(s).")
            };
            self.ignore_state.notice = Some((msg, false));
        } else {
            self.ignore_state.notice =
                Some(("Already in Ignore Words with matching options.".to_string(), true));
        }
    }

    fn render_ignore_words_tab(&mut self, ui: &mut egui::Ui, theme: &ThemeColors) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

                // 1. Add Word Card
                card_frame(theme).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                    let total_words = self.config.ignore_words.len();
                    ui.horizontal(|ui| {
                        ui.label(heading_with_subtitle(
                            "Ignore Words",
                            "(Live Fix & Review Docs)",
                            15.0,
                            12.0,
                            theme,
                        ));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let count_label = if total_words == 1 {
                                "1 word".to_string()
                            } else {
                                format!("{total_words} words")
                            };
                            ui.label(text_regular(count_label, 11.5, theme.text_muted));
                        });
                    });

                    ui.label(text_regular(
                        "Words or phrases added here stay untouched and will not be corrected or replaced.",
                        12.0,
                        theme.text_muted,
                    ));

                    let mut submitted_enter = false;
                    let mut add_clicked = false;
                    ui.horizontal(|ui| {
                        let edit_w = (ui.available_width() - 82.0).max(120.0);
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.ignore_state.word_input)
                                .desired_width(edit_w)
                                .hint_text("Enter a word or phrase (e.g. ShadowFox, corpa)")
                                .margin(egui::Margin::symmetric(8, 6)),
                        );
                        if resp.changed() {
                            self.ignore_state.notice = None;
                            self.ignore_state.confirm_clear_all = false;
                        }
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            submitted_enter = true;
                        }

                        let can_add = !self.ignore_state.word_input.trim().is_empty();
                        let (fill, text_col, stroke) = if can_add {
                            (theme.accent_fill, theme.bg_card, egui::Stroke::NONE)
                        } else {
                            (
                                theme.btn_sec_bg,
                                theme.text_muted,
                                egui::Stroke::new(1.0_f32, theme.border_med),
                            )
                        };
                        if ui
                            .add_enabled(
                                can_add,
                                egui::Button::new(text_bold("Add", 12.5, text_col))
                                    .fill(fill)
                                    .stroke(stroke)
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .min_size(egui::vec2(74.0, 28.0)),
                            )
                            .clicked()
                        {
                            add_clicked = true;
                        }
                    });

                    if submitted_enter || add_clicked {
                        self.try_add_ignore_words();
                    }

                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut self.ignore_state.match_case,
                            text_semibold("Match Case", 12.5, theme.text_primary),
                        )
                        .on_hover_text(
                            "Only ignore when capitalization matches exactly (e.g. keep \"US\" without ignoring \"us\")",
                        );

                        ui.add_space(12.0);

                        ui.checkbox(
                            &mut self.ignore_state.ignore_word_boundary,
                            text_semibold("Ignore Word Boundary", 12.5, theme.text_primary),
                        )
                        .on_hover_text(
                            "Also protect longer words containing this text (e.g. \"corpa\" also protects \"megacorpa\" and \"corpas\")",
                        );
                    });

                    if let Some((msg, is_warn)) = self.ignore_state.notice.as_ref() {
                        let col = if *is_warn {
                            theme.dot_busy
                        } else {
                            theme.text_muted
                        };
                        ui.label(text_regular(msg, 11.5, col));
                    }
                });

                // 2. Saved Words Card
                card_frame(theme).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                    let mut do_clear_all = false;
                    ui.horizontal(|ui| {
                        ui.label(text_bold("Saved Words", 15.0, theme.text_primary));
                        if !self.config.ignore_words.is_empty() {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    if self.ignore_state.confirm_clear_all {
                                        if ui
                                            .add(
                                                egui::Button::new(text_semibold(
                                                    "Confirm Clear?",
                                                    11.5,
                                                    theme.error_text,
                                                ))
                                                .fill(theme.error_bg)
                                                .stroke(egui::Stroke::new(
                                                    1.0_f32,
                                                    theme.error_border,
                                                ))
                                                .corner_radius(egui::CornerRadius::same(5))
                                                .min_size(egui::vec2(100.0, 24.0)),
                                            )
                                            .clicked()
                                        {
                                            do_clear_all = true;
                                        }
                                        if ui
                                            .add(
                                                egui::Button::new(text_regular(
                                                    "Cancel",
                                                    11.5,
                                                    theme.text_muted,
                                                ))
                                                .fill(egui::Color32::TRANSPARENT)
                                                .stroke(egui::Stroke::new(
                                                    1.0_f32,
                                                    theme.border_subtle,
                                                ))
                                                .corner_radius(egui::CornerRadius::same(5))
                                                .min_size(egui::vec2(54.0, 24.0)),
                                            )
                                            .clicked()
                                        {
                                            self.ignore_state.confirm_clear_all = false;
                                        }
                                    } else if ui
                                        .add(
                                            egui::Button::new(text_regular(
                                                "Clear All",
                                                11.5,
                                                theme.text_muted,
                                            ))
                                            .fill(egui::Color32::TRANSPARENT)
                                            .stroke(egui::Stroke::new(
                                                1.0_f32,
                                                theme.border_subtle,
                                            ))
                                            .corner_radius(egui::CornerRadius::same(5))
                                            .min_size(egui::vec2(66.0, 24.0)),
                                        )
                                        .clicked()
                                    {
                                        self.ignore_state.confirm_clear_all = true;
                                    }
                                },
                            );
                        }
                    });

                    if do_clear_all {
                        self.config.ignore_words.clear();
                        self.ignore_state.search_filter.clear();
                        self.ignore_state.confirm_clear_all = false;
                        self.ignore_state.notice =
                            Some(("Cleared all ignored words.".to_string(), false));
                        self.sync_ignore_words();
                    }

                    if self.config.ignore_words.len() >= 8 {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ignore_state.search_filter)
                                .desired_width(f32::INFINITY)
                                .hint_text("Filter saved words...")
                                .margin(egui::Margin::symmetric(8, 5)),
                        );
                    } else if !self.ignore_state.search_filter.is_empty() {
                        self.ignore_state.search_filter.clear();
                    }

                    if self.config.ignore_words.is_empty() {
                        egui::Frame::new()
                            .fill(theme.bg_callout)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                            .corner_radius(egui::CornerRadius::same(8))
                            .inner_margin(egui::Margin::same(10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(text_regular(
                                    "No ignored words yet. Add usernames, slang, or custom terms above to keep them unchanged.",
                                    12.0,
                                    theme.text_muted,
                                ));
                            });
                    } else {
                        enum RowAction {
                            ToggleCase(usize),
                            ToggleBoundary(usize),
                            Remove(usize),
                        }
                        let filter_lower = self.ignore_state.search_filter.trim().to_lowercase();
                        let mut row_action: Option<RowAction> = None;
                        let mut shown = 0usize;

                        for (idx, entry) in self.config.ignore_words.iter().enumerate() {
                            if !filter_lower.is_empty()
                                && !entry.word.to_lowercase().contains(&filter_lower)
                            {
                                continue;
                            }
                            shown += 1;

                            egui::Frame::new()
                                .fill(theme.bg_callout)
                                .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                                .corner_radius(egui::CornerRadius::same(6))
                                .inner_margin(egui::Margin::symmetric(10, 6))
                                .show(ui, |ui| {
                                    let row_w = ui.available_width();
                                    ui.set_width(row_w);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(row_w, 24.0),
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.spacing_mut().item_spacing.x = 6.0;
                                            if ui
                                                .add(
                                                    egui::Button::new(text_semibold(
                                                        "×",
                                                        12.5,
                                                        theme.text_muted,
                                                    ))
                                                    .fill(egui::Color32::TRANSPARENT)
                                                    .stroke(egui::Stroke::new(
                                                        1.0_f32,
                                                        theme.border_subtle,
                                                    ))
                                                    .corner_radius(egui::CornerRadius::same(5))
                                                    .min_size(egui::vec2(26.0, 24.0)),
                                                )
                                                .on_hover_text("Remove")
                                                .clicked()
                                            {
                                                row_action = Some(RowAction::Remove(idx));
                                            }

                                            if chip_button(
                                                ui,
                                                theme,
                                                "Ignore Boundary",
                                                entry.ignore_word_boundary,
                                                true,
                                                egui::vec2(104.0, 24.0),
                                                11.0,
                                            )
                                            .on_hover_text(
                                                "Ignore Word Boundary: also match inside longer words",
                                            )
                                            .clicked()
                                            {
                                                row_action = Some(RowAction::ToggleBoundary(idx));
                                            }

                                            if chip_button(
                                                ui,
                                                theme,
                                                "Match Case",
                                                entry.match_case,
                                                true,
                                                egui::vec2(78.0, 24.0),
                                                11.0,
                                            )
                                            .on_hover_text(
                                                "Match Case: require exact capitalization",
                                            )
                                            .clicked()
                                            {
                                                row_action = Some(RowAction::ToggleCase(idx));
                                            }

                                            let rem_w = ui.available_width().max(40.0);
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(rem_w, 24.0),
                                                egui::Layout::left_to_right(egui::Align::Center),
                                                |ui| {
                                                    ui.add(
                                                        egui::Label::new(text_semibold(
                                                            &entry.word,
                                                            13.0,
                                                            theme.text_primary,
                                                        ))
                                                        .truncate(),
                                                    )
                                                    .on_hover_text(&entry.word);
                                                },
                                            );
                                        },
                                    );
                                });
                        }

                        if shown == 0 {
                            ui.label(text_regular(
                                "No saved words match your filter.",
                                12.0,
                                theme.text_muted,
                            ));
                        }

                        if let Some(action) = row_action {
                            self.ignore_state.confirm_clear_all = false;
                            match action {
                                RowAction::ToggleCase(idx) => {
                                    let mut candidate = self.config.ignore_words[idx].clone();
                                    candidate.match_case = !candidate.match_case;
                                    let collides = self
                                        .config
                                        .ignore_words
                                        .iter()
                                        .enumerate()
                                        .any(|(j, other)| {
                                            j != idx && ignore_word_collides(other, &candidate)
                                        });
                                    if collides {
                                        self.ignore_state.notice = Some((
                                            "Another entry already exists with those options."
                                                .to_string(),
                                            true,
                                        ));
                                    } else {
                                        self.config.ignore_words[idx] = candidate;
                                        self.ignore_state.notice = None;
                                        self.sync_ignore_words();
                                    }
                                }
                                RowAction::ToggleBoundary(idx) => {
                                    self.config.ignore_words[idx].ignore_word_boundary =
                                        !self.config.ignore_words[idx].ignore_word_boundary;
                                    self.ignore_state.notice = None;
                                    self.sync_ignore_words();
                                }
                                RowAction::Remove(idx) => {
                                    self.config.ignore_words.remove(idx);
                                    self.ignore_state.notice = None;
                                    self.sync_ignore_words();
                                }
                            }
                        }
                    }
                });
            });
    }
}

fn build_inline_diff_job(ops: &[DiffOp], theme: &ThemeColors) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let font_reg = font_regular(13.0);
    let font_ins = font_semibold(13.0);
    for op in ops {
        match op {
            DiffOp::Equal(s) => {
                job.append(
                    s,
                    0.0,
                    egui::TextFormat {
                        font_id: font_reg.clone(),
                        color: theme.text_primary,
                        ..Default::default()
                    },
                );
            }
            DiffOp::Delete(s) => {
                job.append(
                    s,
                    0.0,
                    egui::TextFormat {
                        font_id: font_reg.clone(),
                        color: theme.error_text,
                        background: theme.error_bg,
                        strikethrough: egui::Stroke::new(1.0_f32, theme.error_text),
                        ..Default::default()
                    },
                );
            }
            DiffOp::Insert(s) => {
                job.append(
                    s,
                    0.0,
                    egui::TextFormat {
                        font_id: font_ins.clone(),
                        color: theme.dot_ready,
                        background: theme.bg_keycap,
                        ..Default::default()
                    },
                );
            }
        }
    }
    job
}

fn build_side_diff_jobs(
    ops: &[DiffOp],
    theme: &ThemeColors,
) -> (egui::text::LayoutJob, egui::text::LayoutJob) {
    let mut orig = egui::text::LayoutJob::default();
    let mut sug = egui::text::LayoutJob::default();
    let font_reg = font_regular(13.0);
    let font_ins = font_semibold(13.0);
    for op in ops {
        match op {
            DiffOp::Equal(s) => {
                let fmt = egui::TextFormat {
                    font_id: font_reg.clone(),
                    color: theme.text_primary,
                    ..Default::default()
                };
                orig.append(s, 0.0, fmt.clone());
                sug.append(s, 0.0, fmt);
            }
            DiffOp::Delete(s) => {
                orig.append(
                    s,
                    0.0,
                    egui::TextFormat {
                        font_id: font_reg.clone(),
                        color: theme.error_text,
                        background: theme.error_bg,
                        strikethrough: egui::Stroke::new(1.0_f32, theme.error_text),
                        ..Default::default()
                    },
                );
            }
            DiffOp::Insert(s) => {
                sug.append(
                    s,
                    0.0,
                    egui::TextFormat {
                        font_id: font_ins.clone(),
                        color: theme.dot_ready,
                        background: theme.bg_keycap,
                        ..Default::default()
                    },
                );
            }
        }
    }
    (orig, sug)
}

impl Drop for KeyboardLlmApp {
    fn drop(&mut self) {
        if let Some(cancel) = self.doc_state.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(join) = self.doc_state.worker_join.take() {
            let _ = join.join();
        }
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
                if let Some(cancel) = self.doc_state.cancel.as_ref() {
                    cancel.store(true, Ordering::Relaxed);
                }
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

        self.poll_doc_review_events(ctx, &phase, error.as_ref());
        let doc_busy = self.doc_review_busy.load(Ordering::Relaxed);

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
                    ui.add_space(5.0);
                    ui.label(text_bold("LLM Grammarly", 20.0, theme.text_primary));

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
                                    ui.label(text_semibold(status_text, 12.5, theme.text_primary));
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

                // 1b. Tab switcher
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if chip_button(
                        ui,
                        &theme,
                        "Live Fix",
                        self.active_tab == ActiveTab::LiveFix,
                        true,
                        egui::vec2(92.0, 28.0),
                        13.0,
                    )
                    .clicked()
                    {
                        self.active_tab = ActiveTab::LiveFix;
                        self.ignore_state.confirm_clear_all = false;
                    }
                    if chip_button(
                        ui,
                        &theme,
                        "Review Docs",
                        self.active_tab == ActiveTab::ReviewDocs,
                        true,
                        egui::vec2(112.0, 28.0),
                        13.0,
                    )
                    .clicked()
                    {
                        self.active_tab = ActiveTab::ReviewDocs;
                        self.ignore_state.confirm_clear_all = false;
                    }
                    if chip_button(
                        ui,
                        &theme,
                        "Ignore Words",
                        self.active_tab == ActiveTab::IgnoreWords,
                        true,
                        egui::vec2(116.0, 28.0),
                        13.0,
                    )
                    .clicked()
                    {
                        self.active_tab = ActiveTab::IgnoreWords;
                    }
                });

                match self.active_tab {
                    ActiveTab::LiveFix => {
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
                                        ui.label(text_bold(title, 14.5, theme.text_primary));
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
                                        ui.label(text_regular(desc, 12.0, theme.text_muted));
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
                                                        egui::Button::new(text_regular(
                                                            "Discard partial file",
                                                            11.5,
                                                            theme.text_muted,
                                                        ))
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
                                        ui.label(text_bold(heading, 14.0, theme.text_primary));
                                        if let Some(tot) = total.filter(|&t| t > 0) {
                                            let pct = ((*downloaded as f64 / tot as f64) * 100.0)
                                                .clamp(0.0, 100.0);
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.label(text_bold(
                                                        format!("{pct:.0}%"),
                                                        14.0,
                                                        theme.text_primary,
                                                    ));
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
                                    ui.label(text_semibold(stats, 12.0, theme.text_primary));

                                    let dest_short = shorten_middle(dest_display, 58);
                                    ui.label(text_regular(
                                        format!("Saving to: {dest_short}"),
                                        11.5,
                                        theme.text_muted,
                                    ))
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
                            let label = if doc_busy {
                                "Turn Off (Review Docs active)"
                            } else {
                                "Turn Off"
                            };
                            if power_switch_row(ui, &theme, label, true, !doc_busy, false).clicked()
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
                        ui.label(text_semibold(hint, 11.5, theme.dot_busy));
                    }

                    // Status notice line
                    if matches!(phase, AppPhase::Fixing { .. }) {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(text_regular(&notice, 11.5, theme.text_muted));
                        });
                    } else if !matches!(
                        phase,
                        AppPhase::Downloading { .. } | AppPhase::Loading | AppPhase::Unloading
                    ) {
                        ui.label(text_regular(&notice, 11.5, theme.text_muted));
                    }

                    if let Some(err_msg) = error {
                        egui::Frame::new()
                            .fill(theme.error_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.error_border))
                            .corner_radius(egui::CornerRadius::same(6))
                            .inner_margin(egui::Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(text_semibold(&err_msg, 12.0, theme.error_text));
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
                        ui.label(heading_with_subtitle(
                            "Model Download Link",
                            "(Hugging Face GGUF)",
                            15.0,
                            12.0,
                            &theme,
                        ));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let cache_label = match cached_size {
                                Some(sz) => format!("Cached · {}", format_bytes(sz)),
                                None if part_size.is_some() => {
                                    format!("Partial · {}", format_bytes(part_size.unwrap()))
                                }
                                None => "Not downloaded".to_string(),
                            };
                            ui.label(text_regular(cache_label, 11.5, theme.text_muted));
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
                        ui.label(text_regular(
                            format!("File: {}", display_model_name(norm)),
                            11.5,
                            theme.text_muted,
                        ));
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
                            && !doc_busy
                            && (cached_size.is_none()
                                || is_different_from_active
                                || is_compute_changed);

                        if ui
                            .add_enabled(
                                can_click_dl,
                                egui::Button::new(text_semibold(
                                    dl_label,
                                    12.5,
                                    theme.text_primary,
                                ))
                                .fill(theme.btn_sec_bg)
                                .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                .corner_radius(egui::CornerRadius::same(6))
                                .min_size(egui::vec2(130.0, 28.0)),
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
                                egui::vec2(preset.min_width, 28.0),
                                12.5,
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
                        ui.label(text_semibold("Compute", 13.0, theme.text_primary));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let can_change_compute = !is_downloading && !is_busy_loading;

                            if chip_button(
                                ui,
                                &theme,
                                "CPU Only",
                                self.config.compute_mode == ComputeMode::Cpu,
                                can_change_compute,
                                egui::vec2(78.0, 26.0),
                                12.0,
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
                                egui::vec2(60.0, 26.0),
                                12.0,
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
                        ui.label(text_bold("Model Storage Folder", 15.0, theme.text_primary));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let tag = if is_custom_dir {
                                "Custom folder"
                            } else {
                                "App folder (.exe)"
                            };
                            ui.label(text_regular(tag, 11.5, theme.text_muted));
                        });
                    });

                    ui.horizontal(|ui| {
                        let btn_space = if is_custom_dir { 162.0 } else { 100.0 };
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
                                egui::Button::new(text_semibold(
                                    "Browse...",
                                    12.0,
                                    theme.text_primary,
                                ))
                                .fill(theme.btn_sec_bg)
                                .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                .corner_radius(egui::CornerRadius::same(6))
                                .min_size(egui::vec2(78.0, 27.0)),
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
                                    egui::Button::new(text_regular(
                                        "Reset",
                                        11.5,
                                        theme.text_muted,
                                    ))
                                    .fill(egui::Color32::TRANSPARENT)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .min_size(egui::vec2(54.0, 27.0)),
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
                                text_regular("Minimize to tray on close", 12.0, theme.text_muted),
                            )
                            .changed()
                        {
                            self.save_current_config();
                        }

                        ui.add_space(10.0);

                        if ui
                            .checkbox(
                                &mut self.config.show_notifications,
                                text_regular("Notifications", 12.0, theme.text_muted),
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
                                    egui::Button::new(text_semibold(
                                        "Hide to Tray",
                                        12.0,
                                        theme.text_muted,
                                    ))
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
                    }
                    ActiveTab::ReviewDocs => {
                        self.render_review_docs_tab(
                            ui,
                            ctx,
                            &theme,
                            &phase,
                            cached_size.is_some(),
                        );
                    }
                    ActiveTab::IgnoreWords => {
                        self.render_ignore_words_tab(ui, &theme);
                    }
                }
            });
    }
}
