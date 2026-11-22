use anyhow::{bail, Result};
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::icon_render;

extern "C" {
    fn macos_begin_shutdown();
    fn macos_shell_install(
        ns_view_ptr: *mut c_void,
        icon_off_rgba: *const u8,
        icon_on_rgba: *const u8,
        icon_size: u32,
        initial_tip_utf8: *const c_char,
        cb: extern "C" fn(u32, *mut c_void),
        user_data: *mut c_void,
    ) -> bool;
    fn macos_shell_sync_state(
        is_on: bool,
        is_busy: bool,
        tip_utf8: *const c_char,
        toggle_label_utf8: *const c_char,
    );
    fn macos_shell_set_dock_badge(badge_utf8_or_null: *const c_char);
    fn macos_shell_show_window();
    fn macos_shell_hide_window();
    fn macos_shell_close_window();
    fn macos_shell_shutdown();
    fn macos_set_dark_titlebar(ns_view_ptr: *mut c_void, dark: bool);
    fn macos_get_window_placement(
        ns_view_ptr: *mut c_void,
        out_x: *mut i32,
        out_y: *mut i32,
        out_min: *mut bool,
        out_max: *mut bool,
    ) -> bool;
    fn macos_restore_window_position(ns_view_ptr: *mut c_void, x: i32, y: i32);
    fn macos_pick_folder(initial_dir_or_null: *const c_char) -> *mut c_char;
    fn macos_pick_document(initial_dir_or_null: *const c_char) -> *mut c_char;
    fn macos_save_document(
        initial_dir_or_null: *const c_char,
        default_name: *const c_char,
        default_ext: *const c_char,
    ) -> *mut c_char;
    fn macos_free_string(ptr: *mut c_char);
}

#[derive(Debug, Clone, Copy)]
pub enum ShellCommand {
    ToggleOnOff,
    ShowWindow,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarProgress {
    None,
    Indeterminate,
    Determinate { current: u64, total: u64 },
}

fn lock_unpoisoned<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn format_tray_tip(
    is_on: bool,
    is_busy: bool,
    hotkey_label: &str,
    progress: TaskbarProgress,
) -> String {
    match progress {
        TaskbarProgress::Determinate { current, total } if total > 0 => {
            let pct = ((current.saturating_mul(100)) / total).min(100);
            format!("LLM Grammarly — Downloading ({pct}%)")
        }
        TaskbarProgress::Indeterminate if is_busy => "LLM Grammarly — Loading...".to_string(),
        _ if is_busy => "LLM Grammarly — Working...".to_string(),
        _ if is_on => format!("LLM Grammarly — On ({hotkey_label})"),
        _ => "LLM Grammarly — Off".to_string(),
    }
}

extern "C" fn on_shell_cmd(cmd_id: u32, user_data: *mut c_void) {
    if user_data.is_null() {
        return;
    }
    let cb = unsafe { &*(user_data as *const Arc<dyn Fn(ShellCommand) + Send + Sync>) };
    let cmd = match cmd_id {
        0 => ShellCommand::ToggleOnOff,
        1 => ShellCommand::ShowWindow,
        2 => ShellCommand::Quit,
        _ => return,
    };
    cb(cmd);
}

pub fn begin_shutdown() {
    unsafe {
        macos_begin_shutdown();
    }
}

#[derive(Clone)]
pub struct ShellBridge {
    hotkey_label: Arc<Mutex<String>>,
    progress: Arc<Mutex<TaskbarProgress>>,
    is_on: Arc<AtomicBool>,
    is_busy: Arc<AtomicBool>,
}

impl ShellBridge {
    pub fn install(
        ns_view_raw: isize,
        initial_hotkey_label: String,
        on_cmd: Arc<dyn Fn(ShellCommand) + Send + Sync>,
    ) -> Result<Self> {
        let off_rgba = icon_render::render_keyboard_rgba(36, false);
        let on_rgba = icon_render::render_keyboard_rgba(36, true);
        let tip = format_tray_tip(false, false, &initial_hotkey_label, TaskbarProgress::None);
        let tip_c = CString::new(tip.replace('\0', "")).unwrap_or_default();
        let cb_ptr = Box::into_raw(Box::new(on_cmd)) as *mut c_void;

        let ok = unsafe {
            macos_shell_install(
                ns_view_raw as *mut c_void,
                off_rgba.as_ptr(),
                on_rgba.as_ptr(),
                36,
                tip_c.as_ptr(),
                on_shell_cmd,
                cb_ptr,
            )
        };
        if !ok {
            unsafe {
                drop(Box::from_raw(
                    cb_ptr as *mut Arc<dyn Fn(ShellCommand) + Send + Sync>,
                ));
            }
            bail!("Failed to install macOS menu bar icon");
        }

        Ok(Self {
            hotkey_label: Arc::new(Mutex::new(initial_hotkey_label)),
            progress: Arc::new(Mutex::new(TaskbarProgress::None)),
            is_on: Arc::new(AtomicBool::new(false)),
            is_busy: Arc::new(AtomicBool::new(false)),
        })
    }

    fn push_shell_sync(&self) {
        let is_on = self.is_on.load(Ordering::Relaxed);
        let is_busy = self.is_busy.load(Ordering::Relaxed);
        let label = lock_unpoisoned(&self.hotkey_label).clone();
        let progress = *lock_unpoisoned(&self.progress);
        let tip = format_tray_tip(is_on, is_busy, &label, progress);
        let toggle_label = if is_busy {
            "Working..."
        } else if is_on {
            "Turn Off"
        } else {
            "Turn On"
        };

        let tip_c = CString::new(tip.replace('\0', "")).unwrap_or_default();
        let toggle_c = CString::new(toggle_label).unwrap_or_default();
        unsafe {
            macos_shell_sync_state(is_on, is_busy, tip_c.as_ptr(), toggle_c.as_ptr());
        }
    }

    pub fn set_hotkey_label(&self, label: String) {
        *lock_unpoisoned(&self.hotkey_label) = label;
    }

    pub fn set_progress(&self, next: TaskbarProgress) {
        {
            let mut slot = lock_unpoisoned(&self.progress);
            if *slot == next {
                return;
            }
            *slot = next;
        }
        match next {
            TaskbarProgress::Determinate { current, total } if total > 0 => {
                let pct = ((current.saturating_mul(100)) / total).min(100);
                if let Ok(badge) = CString::new(format!("{pct}%")) {
                    unsafe {
                        macos_shell_set_dock_badge(badge.as_ptr());
                    }
                }
            }
            _ => unsafe {
                macos_shell_set_dock_badge(std::ptr::null());
            },
        }
        self.push_shell_sync();
    }

    pub fn sync_state(&self, is_on: bool, is_busy: bool) {
        self.is_on.store(is_on, Ordering::Relaxed);
        self.is_busy.store(is_busy, Ordering::Relaxed);
        self.push_shell_sync();
    }

    pub fn notify(&self, title: impl Into<String>, body: impl Into<String>, _active_icon: bool) {
        let title = title.into();
        let body = body.into();
        let _ = std::thread::Builder::new()
            .name("macos-notify".to_string())
            .spawn(move || {
                let _ = std::process::Command::new("osascript")
                    .args([
                        "-e",
                        "on run argv",
                        "-e",
                        "display notification (item 2 of argv) with title (item 1 of argv)",
                        "-e",
                        "end run",
                        "--",
                        &title,
                        &body,
                    ])
                    .status();
            });
    }

    pub fn show_window(&self) {
        unsafe {
            macos_shell_show_window();
        }
    }

    pub fn hide_window(&self) {
        unsafe {
            macos_shell_hide_window();
        }
    }

    pub fn request_close_window(&self) {
        unsafe {
            macos_shell_close_window();
        }
    }

    pub fn shutdown(&self) {
        unsafe {
            macos_shell_shutdown();
        }
    }
}

fn dir_to_cstring(dir: Option<&Path>) -> Option<CString> {
    let dir = dir?;
    if !dir.exists() {
        return None;
    }
    let s = dir.to_str()?;
    CString::new(s).ok()
}

unsafe fn take_macos_path(ptr: *mut c_char) -> Option<PathBuf> {
    if ptr.is_null() {
        return None;
    }
    let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    macos_free_string(ptr);
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Opens a native macOS folder selection panel. Safe to invoke from a background picker thread.
pub fn pick_folder_dialog(initial_dir: Option<&Path>) -> Option<PathBuf> {
    let init_c = dir_to_cstring(initial_dir);
    let ptr = unsafe {
        macos_pick_folder(init_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()))
    };
    unsafe { take_macos_path(ptr) }
}

/// Opens a native macOS file picker for `.txt`, `.md`, and `.docx` documents.
pub fn pick_document_dialog(initial_dir: Option<&Path>) -> Option<PathBuf> {
    let init_c = dir_to_cstring(initial_dir);
    let ptr = unsafe {
        macos_pick_document(init_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()))
    };
    unsafe { take_macos_path(ptr) }
}

/// Opens a native macOS Save As panel for documents.
pub fn save_document_dialog(
    initial_dir: Option<&Path>,
    default_name: &str,
    default_ext: &str,
) -> Option<PathBuf> {
    let init_c = dir_to_cstring(initial_dir);
    let name_c = CString::new(default_name.replace('\0', "")).unwrap_or_default();
    let ext_c = CString::new(default_ext.replace('\0', "")).unwrap_or_default();
    let ptr = unsafe {
        macos_save_document(
            init_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            name_c.as_ptr(),
            ext_c.as_ptr(),
        )
    };
    unsafe { take_macos_path(ptr) }
}

/// Returns `(left, top, is_minimized, is_maximized)` in Cocoa screen points if the window is visible.
pub fn get_window_placement_state(ns_view_raw: isize) -> Option<(i32, i32, bool, bool)> {
    if ns_view_raw == 0 {
        return None;
    }
    let mut x = 0i32;
    let mut y = 0i32;
    let mut is_min = false;
    let mut is_max = false;
    let ok = unsafe {
        macos_get_window_placement(
            ns_view_raw as *mut c_void,
            &mut x,
            &mut y,
            &mut is_min,
            &mut is_max,
        )
    };
    if ok {
        Some((x, y, is_min, is_max))
    } else {
        None
    }
}

/// Restores a window's position in Cocoa screen points, clamping back onto the nearest visible screen if needed.
pub fn restore_window_position(ns_view_raw: isize, x: i32, y: i32) {
    if ns_view_raw == 0 {
        return;
    }
    unsafe {
        macos_restore_window_position(ns_view_raw as *mut c_void, x, y);
    }
}

/// Syncs the native macOS window appearance between light (Aqua) and dark (DarkAqua) mode.
pub fn set_window_dark_titlebar(ns_view_raw: isize, dark: bool) {
    if ns_view_raw == 0 {
        return;
    }
    unsafe {
        macos_set_dark_titlebar(ns_view_raw as *mut c_void, dark);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tray_tip_reflects_state() {
        assert_eq!(
            format_tray_tip(false, false, "Ctrl + Q", TaskbarProgress::None),
            "LLM Grammarly — Off"
        );
        assert_eq!(
            format_tray_tip(true, false, "Ctrl + Shift + G", TaskbarProgress::None),
            "LLM Grammarly — On (Ctrl + Shift + G)"
        );
        assert_eq!(
            format_tray_tip(false, true, "Ctrl + Shift + G", TaskbarProgress::Indeterminate),
            "LLM Grammarly — Loading..."
        );
        assert_eq!(
            format_tray_tip(
                false,
                true,
                "Ctrl + Shift + G",
                TaskbarProgress::Determinate {
                    current: 50,
                    total: 200
                }
            ),
            "LLM Grammarly — Downloading (25%)"
        );
    }
}
