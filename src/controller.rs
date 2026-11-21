use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::config::{local_model_path, normalize_model_url, ComputeMode, HotkeyBinding};
use crate::downloader::{download_model, DownloadProgress};
use crate::hotkey::{HotkeyController, HotkeyEvent};
use crate::llm::{LlmHandle, LoadInfo};
use crate::taskbar::{ShellBridge, TaskbarProgress};

#[derive(Debug, Clone)]
pub enum AppPhase {
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
    pub fn is_on(&self) -> bool {
        matches!(self, AppPhase::On { .. } | AppPhase::Fixing { .. })
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            AppPhase::Downloading { .. }
                | AppPhase::Loading
                | AppPhase::Fixing { .. }
                | AppPhase::Unloading
        )
    }
}

pub struct SharedState {
    pub phase: AppPhase,
    pub error: Option<String>,
    pub notice: String,
    pub active_model_path: Option<PathBuf>,
    pub active_compute_mode: Option<ComputeMode>,
    pub force_quit: bool,
    pub download_cancel: Option<Arc<AtomicBool>>,
    pub picked_folder: Option<PathBuf>,
    pub hotkey_revert: Option<HotkeyBinding>,
    pub toggle_power_hotkey_revert: Option<Option<HotkeyBinding>>,
    pub toggle_highlight_hotkey_revert: Option<Option<HotkeyBinding>>,
    pub highlight_sync: Option<bool>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            phase: AppPhase::Off,
            error: None,
            notice: "Memory free".to_string(),
            active_model_path: None,
            active_compute_mode: None,
            force_quit: false,
            download_cancel: None,
            picked_folder: None,
            hotkey_revert: None,
            toggle_power_hotkey_revert: None,
            toggle_highlight_hotkey_revert: None,
            highlight_sync: None,
        }
    }
}

pub enum CtrlCmd {
    ToggleOnOff,
    TurnOn(String, PathBuf, ComputeMode),
    TurnOff,
    DownloadModel(String, PathBuf, ComputeMode),
    CancelDownload,
    UpdateHotkey(HotkeyBinding),
    UpdateTogglePowerHotkey(Option<HotkeyBinding>),
    UpdateToggleHighlightHotkey(Option<HotkeyBinding>),
    ShowWindow,
    Quit,
    DownloadProgress {
        progress: DownloadProgress,
    },
    DownloadFinished {
        dest: PathBuf,
        compute_mode: ComputeMode,
        res: anyhow::Result<()>,
        turn_on_after: bool,
        restore_on_cancel: bool,
        prev_backend_desc: Option<String>,
    },
    LoadFinished {
        dest: PathBuf,
        compute_mode: ComputeMode,
        res: anyhow::Result<LoadInfo>,
    },
    Hotkey(HotkeyEvent),
    ShutdownController,
}

pub fn notify_highlight_mode(shell: &ShellBridge, highlighted_only: bool, app_is_on: bool) {
    let (title, body) = match (highlighted_only, app_is_on) {
        (true, true) => ("Highlight Mode — On", "Only highlighted text will be fixed"),
        (true, false) => (
            "Highlight Mode — On",
            "Only highlighted text will be fixed · App is off",
        ),
        (false, true) => ("Highlight Mode — Off", "Select all & fix mode"),
        (false, false) => ("Highlight Mode — Off", "Select all & fix mode · App is off"),
    };
    shell.notify(title, body, highlighted_only);
}

#[allow(clippy::too_many_arguments)]
pub fn controller_loop(
    rx: Receiver<CtrlCmd>,
    tx: Sender<CtrlCmd>,
    state: Arc<Mutex<SharedState>>,
    shared_url: Arc<Mutex<String>>,
    shared_dir: Arc<Mutex<PathBuf>>,
    shared_compute_mode: Arc<Mutex<ComputeMode>>,
    shared_hotkey: Arc<Mutex<HotkeyBinding>>,
    initial_power_hotkey: Option<HotkeyBinding>,
    initial_highlight_hotkey: Option<HotkeyBinding>,
    shared_highlighted_only: Arc<AtomicBool>,
    shared_show_notifications: Arc<AtomicBool>,
    doc_review_busy: Arc<AtomicBool>,
    mut llm: Arc<LlmHandle>,
    mut hotkey: HotkeyController,
    shell: Option<ShellBridge>,
    ctx: egui::Context,
) {
    let mut registered_hotkey = *shared_hotkey.lock().unwrap();
    let mut registered_power_hotkey = initial_power_hotkey;
    if let Some(hk) = initial_power_hotkey {
        if let Err(e) = hotkey.set_toggle_power(Some(hk)) {
            registered_power_hotkey = None;
            let mut s = state.lock().unwrap();
            s.toggle_power_hotkey_revert = Some(None);
            s.error = Some(e.to_string());
        }
    }
    let mut registered_highlight_hotkey = initial_highlight_hotkey;
    if let Some(hk) = initial_highlight_hotkey {
        if let Err(e) = hotkey.set_toggle_highlight(Some(hk)) {
            registered_highlight_hotkey = None;
            let mut s = state.lock().unwrap();
            s.toggle_highlight_hotkey_revert = Some(None);
            s.error = Some(e.to_string());
        }
    }

    let sync_shell = |is_on: bool, is_busy: bool, progress: Option<TaskbarProgress>| {
        if let Some(sh) = shell.as_ref() {
            if let Some(p) = progress {
                sh.set_progress(p);
            }
            sh.sync_state(is_on, is_busy);
        }
        ctx.request_repaint();
    };

    let spawn_loader = |dest: PathBuf,
                        compute_mode: ComputeMode,
                        state: &Arc<Mutex<SharedState>>,
                        tx: &Sender<CtrlCmd>,
                        ctx: &egui::Context| {
        {
            let mut s = state.lock().unwrap();
            s.phase = AppPhase::Loading;
            s.error = None;
            s.notice = "Loading model...".to_string();
        }
        sync_shell(false, true, Some(TaskbarProgress::Indeterminate));

        let llm_c = Arc::clone(&llm);
        let tx_c = tx.clone();
        let ctx_c = ctx.clone();
        let dest_c = dest.clone();
        if thread::Builder::new()
            .name("model-loader".to_string())
            .spawn(move || {
                let res = llm_c.load(dest_c.clone(), compute_mode);
                let _ = tx_c.send(CtrlCmd::LoadFinished {
                    dest: dest_c,
                    compute_mode,
                    res,
                });
                ctx_c.request_repaint();
            })
            .is_err()
        {
            let _ = tx.send(CtrlCmd::LoadFinished {
                dest,
                compute_mode,
                res: Err(anyhow::anyhow!("Failed to spawn model loader thread")),
            });
            ctx.request_repaint();
        }
    };

    let start_normalized_download = |norm: String,
                                     storage_dir: PathBuf,
                                     compute_mode: ComputeMode,
                                     turn_on_after: bool,
                                     restore_on_cancel: bool,
                                     prev_backend_desc: Option<String>,
                                     state: &Arc<Mutex<SharedState>>,
                                     tx: &Sender<CtrlCmd>,
                                     ctx: &egui::Context| {
        let dest = local_model_path(&storage_dir, &norm);
        if dest.exists() {
            if turn_on_after {
                spawn_loader(dest, compute_mode, state, tx, ctx);
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
        let dest_err = dest.clone();
        let prev_backend_err = prev_backend_desc.clone();
        if thread::Builder::new()
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
                    compute_mode,
                    res,
                    turn_on_after,
                    restore_on_cancel,
                    prev_backend_desc,
                });
                ctx_c.request_repaint();
            })
            .is_err()
        {
            let _ = tx.send(CtrlCmd::DownloadFinished {
                dest: dest_err,
                compute_mode,
                res: Err(anyhow::anyhow!("Failed to spawn model downloader thread")),
                turn_on_after,
                restore_on_cancel,
                prev_backend_desc: prev_backend_err,
            });
            ctx.request_repaint();
        }
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            CtrlCmd::ToggleOnOff => {
                if doc_review_busy.load(Ordering::Relaxed) {
                    state.lock().unwrap().notice =
                        "Wait for or cancel the active document review first".to_string();
                    ctx.request_repaint();
                    continue;
                }
                let (is_on, is_busy) = {
                    let s = state.lock().unwrap();
                    (s.phase.is_on(), s.phase.is_busy())
                };
                if is_busy {
                    continue;
                }
                if is_on {
                    let _ = tx.send(CtrlCmd::TurnOff);
                } else {
                    let url = shared_url.lock().unwrap().clone();
                    let dir = shared_dir.lock().unwrap().clone();
                    let compute_mode = *shared_compute_mode.lock().unwrap();
                    let _ = tx.send(CtrlCmd::TurnOn(url, dir, compute_mode));
                }
            }
            CtrlCmd::TurnOn(raw_url, storage_dir, compute_mode) => {
                let can_start = {
                    let s = state.lock().unwrap();
                    matches!(s.phase, AppPhase::Off)
                };
                if !can_start {
                    continue;
                }
                let norm = match normalize_model_url(&raw_url) {
                    Ok(u) => u,
                    Err(e) => {
                        state.lock().unwrap().error = Some(e.to_string());
                        sync_shell(false, false, Some(TaskbarProgress::None));
                        continue;
                    }
                };
                start_normalized_download(
                    norm,
                    storage_dir,
                    compute_mode,
                    true,
                    false,
                    None,
                    &state,
                    &tx,
                    &ctx,
                );
            }
            CtrlCmd::TurnOff => {
                if doc_review_busy.load(Ordering::Relaxed) {
                    state.lock().unwrap().notice =
                        "Wait for or cancel the active document review first".to_string();
                    ctx.request_repaint();
                    continue;
                }
                let was_on = {
                    let mut s = state.lock().unwrap();
                    if !s.phase.is_on() {
                        false
                    } else {
                        s.phase = AppPhase::Unloading;
                        s.notice = "Unloading model...".to_string();
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
                    s.active_compute_mode = None;
                    s.notice = "Memory free".to_string();
                }
                sync_shell(false, false, Some(TaskbarProgress::None));
                if shared_show_notifications.load(Ordering::Relaxed) {
                    if let Some(sh) = shell.as_ref() {
                        sh.notify("LLM Grammarly — Off", "Turned off · Memory free", false);
                    }
                }
            }
            CtrlCmd::DownloadModel(raw_url, storage_dir, compute_mode) => {
                if doc_review_busy.load(Ordering::Relaxed) {
                    state.lock().unwrap().notice =
                        "Wait for or cancel the active document review first".to_string();
                    ctx.request_repaint();
                    continue;
                }
                let (is_on, is_busy, prev_backend_desc) = {
                    let s = state.lock().unwrap();
                    let bd = match &s.phase {
                        AppPhase::On { backend_desc } | AppPhase::Fixing { backend_desc } => {
                            Some(backend_desc.clone())
                        }
                        _ => None,
                    };
                    (s.phase.is_on(), s.phase.is_busy(), bd)
                };
                if is_busy {
                    continue;
                }
                let norm = match normalize_model_url(&raw_url) {
                    Ok(u) => u,
                    Err(e) => {
                        state.lock().unwrap().error = Some(e.to_string());
                        ctx.request_repaint();
                        continue;
                    }
                };
                if is_on {
                    hotkey.disable();
                }
                start_normalized_download(
                    norm,
                    storage_dir,
                    compute_mode,
                    is_on,
                    is_on,
                    prev_backend_desc,
                    &state,
                    &tx,
                    &ctx,
                );
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
                                    _ => "Model".to_string(),
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
            CtrlCmd::UpdateTogglePowerHotkey(next_binding) => {
                match hotkey.set_toggle_power(next_binding) {
                    Ok(()) => {
                        registered_power_hotkey = next_binding;
                        state.lock().unwrap().error = None;
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        let _ = hotkey.set_toggle_power(registered_power_hotkey);
                        let mut s = state.lock().unwrap();
                        s.toggle_power_hotkey_revert = Some(registered_power_hotkey);
                        s.error = Some(e.to_string());
                        ctx.request_repaint();
                    }
                }
            }
            CtrlCmd::UpdateToggleHighlightHotkey(next_binding) => {
                match hotkey.set_toggle_highlight(next_binding) {
                    Ok(()) => {
                        registered_highlight_hotkey = next_binding;
                        state.lock().unwrap().error = None;
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        let _ = hotkey.set_toggle_highlight(registered_highlight_hotkey);
                        let mut s = state.lock().unwrap();
                        s.toggle_highlight_hotkey_revert = Some(registered_highlight_hotkey);
                        s.error = Some(e.to_string());
                        ctx.request_repaint();
                    }
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
                compute_mode,
                res,
                turn_on_after,
                restore_on_cancel,
                prev_backend_desc,
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
                            spawn_loader(dest, compute_mode, &state, &tx, &ctx);
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
                            if active_path.is_some() {
                                match hotkey.enable(active_hk) {
                                    Ok(()) => {
                                        registered_hotkey = active_hk;
                                        {
                                            let mut s = state.lock().unwrap();
                                            s.phase = AppPhase::On {
                                                backend_desc: prev_backend_desc
                                                    .unwrap_or_else(|| "CPU".to_string()),
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
                                    Err(_) => {
                                        llm.unload();
                                    }
                                }
                            }
                        }
                        {
                            let mut s = state.lock().unwrap();
                            s.phase = AppPhase::Off;
                            s.active_model_path = None;
                            s.active_compute_mode = None;
                            if was_cancelled {
                                s.notice = "Download paused · Memory free".to_string();
                            } else {
                                s.error = Some(msg);
                                s.notice = "Memory free".to_string();
                            }
                        }
                        sync_shell(false, false, Some(TaskbarProgress::None));
                    }
                }
            }
            CtrlCmd::LoadFinished {
                dest,
                compute_mode,
                res,
            } => {
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
                                s.active_compute_mode = Some(compute_mode);
                                s.error = None;
                                s.notice = format!(
                                    "{} · Ready ({})",
                                    info.backend_desc,
                                    active_hk.display_label()
                                );
                            }
                            sync_shell(true, false, Some(TaskbarProgress::None));
                            if shared_show_notifications.load(Ordering::Relaxed) {
                                if let Some(sh) = shell.as_ref() {
                                    let hl = shared_highlighted_only.load(Ordering::Relaxed);
                                    let mode = if hl {
                                        "Highlighted text only"
                                    } else {
                                        "Select all & fix"
                                    };
                                    sh.notify(
                                        "LLM Grammarly — On",
                                        format!("Ready ({}) · {mode}", active_hk.display_label()),
                                        true,
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            llm.unload();
                            {
                                let mut s = state.lock().unwrap();
                                s.phase = AppPhase::Off;
                                s.active_model_path = None;
                                s.active_compute_mode = None;
                                s.error = Some(e.to_string());
                                s.notice = "Memory free".to_string();
                            }
                            sync_shell(false, false, Some(TaskbarProgress::None));
                        }
                    },
                    Err(e) => {
                        {
                            let mut s = state.lock().unwrap();
                            s.phase = AppPhase::Off;
                            s.active_model_path = None;
                            s.active_compute_mode = None;
                            s.error = Some(e.to_string());
                            s.notice = "Memory free".to_string();
                        }
                        sync_shell(false, false, Some(TaskbarProgress::None));
                    }
                }
            }
            CtrlCmd::Hotkey(evt) => match evt {
                HotkeyEvent::FixingStarted => {
                    let updated = {
                        let mut s = state.lock().unwrap();
                        if let AppPhase::On { backend_desc } = &s.phase {
                            s.phase = AppPhase::Fixing {
                                backend_desc: backend_desc.clone(),
                            };
                            s.notice = "Fixing text...".to_string();
                            true
                        } else {
                            false
                        }
                    };
                    if updated {
                        sync_shell(true, true, Some(TaskbarProgress::Indeterminate));
                    }
                }
                HotkeyEvent::FixSucceeded {
                    elapsed_ms,
                    changed,
                } => {
                    let updated = {
                        let mut s = state.lock().unwrap();
                        let backend_desc = match &s.phase {
                            AppPhase::Fixing { backend_desc } | AppPhase::On { backend_desc } => {
                                Some(backend_desc.clone())
                            }
                            _ => None,
                        };
                        if let Some(backend_desc) = backend_desc {
                            s.phase = AppPhase::On {
                                backend_desc: backend_desc.clone(),
                            };
                            let secs = (elapsed_ms as f64) / 1000.0;
                            s.notice = if changed {
                                format!("{backend_desc} · Fixed in {secs:.2}s")
                            } else {
                                format!("{backend_desc} · No changes needed ({secs:.2}s)")
                            };
                            true
                        } else {
                            false
                        }
                    };
                    if updated {
                        sync_shell(true, false, Some(TaskbarProgress::None));
                    }
                }
                HotkeyEvent::FixCopiedToClipboard { elapsed_ms } => {
                    let updated = {
                        let mut s = state.lock().unwrap();
                        let backend_desc = match &s.phase {
                            AppPhase::Fixing { backend_desc } | AppPhase::On { backend_desc } => {
                                Some(backend_desc.clone())
                            }
                            _ => None,
                        };
                        if let Some(backend_desc) = backend_desc {
                            s.phase = AppPhase::On {
                                backend_desc: backend_desc.clone(),
                            };
                            let secs = (elapsed_ms as f64) / 1000.0;
                            s.notice = format!(
                                "{backend_desc} · Copied fix to clipboard ({secs:.2}s — window changed)"
                            );
                            true
                        } else {
                            false
                        }
                    };
                    if updated {
                        sync_shell(true, false, Some(TaskbarProgress::None));
                    }
                    if shared_show_notifications.load(Ordering::Relaxed) {
                        if let Some(sh) = shell.as_ref() {
                            sh.notify(
                                "LLM Grammarly — Fix Ready",
                                "Window changed — corrected text copied to clipboard",
                                true,
                            );
                        }
                    }
                }
                HotkeyEvent::FixNotice(msg) => {
                    let updated = {
                        let mut s = state.lock().unwrap();
                        if s.phase.is_on() {
                            if let AppPhase::Fixing { backend_desc } = &s.phase {
                                s.phase = AppPhase::On {
                                    backend_desc: backend_desc.clone(),
                                };
                            }
                            s.notice = msg;
                            true
                        } else {
                            false
                        }
                    };
                    if updated {
                        sync_shell(true, false, Some(TaskbarProgress::None));
                    }
                }
                HotkeyEvent::FixError(err) => {
                    let updated = {
                        let mut s = state.lock().unwrap();
                        if s.phase.is_on() {
                            if let AppPhase::Fixing { backend_desc } = &s.phase {
                                s.phase = AppPhase::On {
                                    backend_desc: backend_desc.clone(),
                                };
                            }
                            s.error = Some(err);
                            true
                        } else {
                            false
                        }
                    };
                    if updated {
                        sync_shell(true, false, Some(TaskbarProgress::None));
                    }
                }
                HotkeyEvent::TogglePowerRequested => {
                    let _ = tx.send(CtrlCmd::ToggleOnOff);
                }
                HotkeyEvent::ToggleHighlightRequested => {
                    let next = !shared_highlighted_only.load(Ordering::Relaxed);
                    shared_highlighted_only.store(next, Ordering::Relaxed);
                    let is_on = {
                        let mut s = state.lock().unwrap();
                        s.highlight_sync = Some(next);
                        let mode_msg = if next {
                            "Highlight-only mode on"
                        } else {
                            "Highlight-only mode off"
                        };
                        s.notice = match &s.phase {
                            AppPhase::On { backend_desc } | AppPhase::Fixing { backend_desc } => {
                                format!("{backend_desc} · {mode_msg}")
                            }
                            AppPhase::Off => format!("{mode_msg} · Memory free"),
                            _ => mode_msg.to_string(),
                        };
                        s.phase.is_on()
                    };
                    if shared_show_notifications.load(Ordering::Relaxed) {
                        if let Some(sh) = shell.as_ref() {
                            notify_highlight_mode(sh, next, is_on);
                        }
                    }
                    ctx.request_repaint();
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
