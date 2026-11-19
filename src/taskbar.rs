use anyhow::{anyhow, Result};
use eframe::egui;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Controls::{
    ImageList_Create, ImageList_Destroy, ImageList_ReplaceIcon, HIMAGELIST, ILC_COLOR32, ILC_MASK,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, FileOpenDialog, IFileOpenDialog, IShellItem, ITaskbarList3,
    RemoveWindowSubclass, SHCreateItemFromParsingName, SetWindowSubclass, TaskbarList,
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH, TBPF_INDETERMINATE, TBPF_NOPROGRESS,
    TBPF_NORMAL, THBF_DISABLED, THBF_ENABLED, THBN_CLICKED, THB_BITMAP, THB_FLAGS, THB_TOOLTIP,
    THUMBBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, DestroyIcon, GetWindowRect, IsIconic, IsWindowVisible,
    IsZoomed, PostMessageW, RegisterWindowMessageW, SetForegroundWindow, SetWindowPos,
    ShowWindow, MSGFLT_ALLOW, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_RESTORE,
    SW_SHOW, WM_APP, WM_CLOSE, WM_COMMAND, WM_NCDESTROY,
};

use crate::icon;

const SUBCLASS_ID: usize = 0x4B4C_5442;
const BTN_TOGGLE_ID: u32 = 1001;
const BTN_QUIT_ID: u32 = 1002;

const WM_APP_SYNC_SHELL: u32 = WM_APP + 20;
const WM_APP_SHOW_WINDOW: u32 = WM_APP + 21;
const WM_APP_HIDE_WINDOW: u32 = WM_APP + 22;
const WM_APP_CLOSE_WINDOW: u32 = WM_APP + 23;
const WM_APP_SET_PROGRESS: u32 = WM_APP + 24;

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

struct TrayUi {
    tray: TrayIcon,
    toggle_item: MenuItem,
    last_active: Option<bool>,
}

thread_local! {
    static TRAY_UI: RefCell<Option<TrayUi>> = const { RefCell::new(None) };
}

fn sync_tray_on_ui_thread(
    is_on: bool,
    is_busy: bool,
    hotkey_label: &str,
    progress: TaskbarProgress,
) {
    TRAY_UI.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(ui) = borrow.as_mut() else {
            return;
        };

        if is_busy {
            ui.toggle_item.set_text("Working...");
            ui.toggle_item.set_enabled(false);
        } else if is_on {
            ui.toggle_item.set_text("Turn Off");
            ui.toggle_item.set_enabled(true);
        } else {
            ui.toggle_item.set_text("Turn On");
            ui.toggle_item.set_enabled(true);
        }

        let tip = match progress {
            TaskbarProgress::Determinate { current, total } if total > 0 => {
                let pct = ((current.saturating_mul(100)) / total).min(100);
                format!("Keyboard LLM — Downloading ({pct}%)")
            }
            TaskbarProgress::Indeterminate if is_busy => "Keyboard LLM — Loading...".to_string(),
            _ if is_busy => "Keyboard LLM — Working...".to_string(),
            _ if is_on => format!("Keyboard LLM — On ({hotkey_label})"),
            _ => "Keyboard LLM — Off".to_string(),
        };
        let _ = ui.tray.set_tooltip(Some(tip));

        if ui.last_active != Some(is_on) {
            if let Ok(img) = icon::tray_icon_image(is_on) {
                let _ = ui.tray.set_icon(Some(img));
            }
            ui.last_active = Some(is_on);
        }
    });
}

fn drop_tray_on_ui_thread() {
    TRAY_UI.with(|slot| {
        slot.borrow_mut().take();
    });
}

struct ThumbState {
    msg_taskbar_created: u32,
    tbl: Option<ITaskbarList3>,
    himl: HIMAGELIST,
    buttons_added: bool,
    is_on: bool,
    is_busy: bool,
    hotkey_label: Arc<Mutex<String>>,
    progress: Arc<Mutex<TaskbarProgress>>,
    on_cmd: Arc<dyn Fn(ShellCommand) + Send + Sync>,
    shutdown_flag: Arc<AtomicBool>,
}

impl Drop for ThumbState {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        drop_tray_on_ui_thread();
        unsafe {
            if !self.himl.is_invalid() {
                let _ = ImageList_Destroy(self.himl);
                self.himl = HIMAGELIST::default();
            }
        }
    }
}

fn fill_tip(buf: &mut [u16; 260], text: &str) {
    buf.fill(0);
    for (dst, ch) in buf.iter_mut().take(259).zip(text.encode_utf16()) {
        *dst = ch;
    }
}

fn build_thumb_buttons(is_on: bool, is_busy: bool) -> [THUMBBUTTON; 2] {
    let mut toggle = THUMBBUTTON::default();
    toggle.dwMask = THB_BITMAP | THB_TOOLTIP | THB_FLAGS;
    toggle.iId = BTN_TOGGLE_ID;
    toggle.iBitmap = if is_on { 1 } else { 0 };
    toggle.dwFlags = if is_busy { THBF_DISABLED } else { THBF_ENABLED };
    let tip_text = if is_busy {
        "Working..."
    } else if is_on {
        "Turn Off"
    } else {
        "Turn On"
    };
    fill_tip(&mut toggle.szTip, tip_text);

    let mut quit = THUMBBUTTON::default();
    quit.dwMask = THB_BITMAP | THB_TOOLTIP | THB_FLAGS;
    quit.iId = BTN_QUIT_ID;
    quit.iBitmap = 2;
    quit.dwFlags = THBF_ENABLED;
    fill_tip(&mut quit.szTip, "Quit Keyboard LLM");

    [toggle, quit]
}

unsafe fn apply_taskbar_progress(hwnd: HWND, tbl: &ITaskbarList3, progress: TaskbarProgress) {
    match progress {
        TaskbarProgress::None => {
            let _ = tbl.SetProgressState(hwnd, TBPF_NOPROGRESS);
        }
        TaskbarProgress::Indeterminate => {
            let _ = tbl.SetProgressState(hwnd, TBPF_INDETERMINATE);
        }
        TaskbarProgress::Determinate { current, total } => {
            let _ = tbl.SetProgressState(hwnd, TBPF_NORMAL);
            let _ = tbl.SetProgressValue(hwnd, current, total.max(1));
        }
    }
}

unsafe fn ensure_thumbbar(hwnd: HWND, state: &mut ThumbState, force_readd: bool) {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    if state.himl.is_invalid() {
        let himl = ImageList_Create(24, 24, ILC_COLOR32 | ILC_MASK, 3, 0);
        if !himl.is_invalid() {
            if let Ok(h_off) = icon::thumb_power_hicon(false) {
                let _ = ImageList_ReplaceIcon(himl, -1, h_off);
                let _ = DestroyIcon(h_off);
            }
            if let Ok(h_on) = icon::thumb_power_hicon(true) {
                let _ = ImageList_ReplaceIcon(himl, -1, h_on);
                let _ = DestroyIcon(h_on);
            }
            if let Ok(h_quit) = icon::thumb_quit_hicon() {
                let _ = ImageList_ReplaceIcon(himl, -1, h_quit);
                let _ = DestroyIcon(h_quit);
            }
            state.himl = himl;
        }
    }

    if state.tbl.is_none() || force_readd {
        if let Ok(tbl) = CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
        {
            if tbl.HrInit().is_ok() {
                state.tbl = Some(tbl);
                state.buttons_added = false;
            }
        }
    }

    let Some(tbl) = state.tbl.as_ref() else {
        return;
    };

    let buttons = build_thumb_buttons(state.is_on, state.is_busy);
    if !state.buttons_added {
        if !state.himl.is_invalid() {
            let _ = tbl.ThumbBarSetImageList(hwnd, state.himl);
        }
        if tbl.ThumbBarAddButtons(hwnd, &buttons).is_ok() {
            state.buttons_added = true;
        }
    } else if tbl.ThumbBarUpdateButtons(hwnd, &buttons).is_err() {
        if !state.himl.is_invalid() {
            let _ = tbl.ThumbBarSetImageList(hwnd, state.himl);
        }
        if tbl.ThumbBarAddButtons(hwnd, &buttons).is_ok() {
            state.buttons_added = true;
        }
    }

    let progress = *state.progress.lock().unwrap();
    apply_taskbar_progress(hwnd, tbl, progress);
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    u_msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
    u_id_subclass: usize,
    dw_ref_data: usize,
) -> LRESULT {
    if dw_ref_data == 0 {
        return DefSubclassProc(hwnd, u_msg, w_param, l_param);
    }
    let state = &mut *(dw_ref_data as *mut ThumbState);

    if u_msg == state.msg_taskbar_created {
        ensure_thumbbar(hwnd, state, true);
    } else if u_msg == WM_APP_SYNC_SHELL {
        state.is_on = w_param.0 != 0;
        state.is_busy = l_param.0 != 0;
        ensure_thumbbar(hwnd, state, false);
        let label = state.hotkey_label.lock().unwrap().clone();
        let progress = *state.progress.lock().unwrap();
        sync_tray_on_ui_thread(state.is_on, state.is_busy, &label, progress);
        return LRESULT(0);
    } else if u_msg == WM_APP_SET_PROGRESS {
        let progress = *state.progress.lock().unwrap();
        if let Some(tbl) = state.tbl.as_ref() {
            apply_taskbar_progress(hwnd, tbl, progress);
        }
        let label = state.hotkey_label.lock().unwrap().clone();
        sync_tray_on_ui_thread(state.is_on, state.is_busy, &label, progress);
        return LRESULT(0);
    } else if u_msg == WM_APP_SHOW_WINDOW {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
        return LRESULT(0);
    } else if u_msg == WM_APP_HIDE_WINDOW {
        let _ = ShowWindow(hwnd, SW_HIDE);
        return LRESULT(0);
    } else if u_msg == WM_APP_CLOSE_WINDOW {
        let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        return LRESULT(0);
    } else if u_msg == WM_COMMAND {
        let wp = w_param.0 as u32;
        let hi = (wp >> 16) & 0xFFFF;
        let lo = wp & 0xFFFF;
        if hi == THBN_CLICKED {
            if lo == BTN_TOGGLE_ID {
                (state.on_cmd)(ShellCommand::ToggleOnOff);
                return LRESULT(0);
            } else if lo == BTN_QUIT_ID {
                (state.on_cmd)(ShellCommand::Quit);
                return LRESULT(0);
            }
        }
    } else if u_msg == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), u_id_subclass);
        let boxed = Box::from_raw(dw_ref_data as *mut ThumbState);
        drop(boxed);
        return DefSubclassProc(hwnd, u_msg, w_param, l_param);
    }

    DefSubclassProc(hwnd, u_msg, w_param, l_param)
}

#[derive(Clone)]
pub struct ShellBridge {
    hwnd_raw: isize,
    hotkey_label: Arc<Mutex<String>>,
    progress: Arc<Mutex<TaskbarProgress>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl ShellBridge {
    pub fn install(
        hwnd_raw: isize,
        initial_hotkey_label: String,
        egui_ctx: egui::Context,
        on_cmd: Arc<dyn Fn(ShellCommand) + Send + Sync>,
    ) -> Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let hotkey_label = Arc::new(Mutex::new(initial_hotkey_label));
        let progress = Arc::new(Mutex::new(TaskbarProgress::None));
        let hwnd = HWND(hwnd_raw as *mut _);

        let show_item = MenuItem::new("Show Window", true, None);
        let toggle_item = MenuItem::new("Turn On", true, None);
        let sep = PredefinedMenuItem::separator();
        let quit_item = MenuItem::new("Quit", true, None);

        let show_id: MenuId = show_item.id().clone();
        let toggle_id: MenuId = toggle_item.id().clone();
        let quit_id: MenuId = quit_item.id().clone();

        let tray_menu = Menu::new();
        tray_menu
            .append(&show_item)
            .map_err(|e| anyhow!("Tray menu error: {e}"))?;
        tray_menu
            .append(&toggle_item)
            .map_err(|e| anyhow!("Tray menu error: {e}"))?;
        tray_menu
            .append(&sep)
            .map_err(|e| anyhow!("Tray menu error: {e}"))?;
        tray_menu
            .append(&quit_item)
            .map_err(|e| anyhow!("Tray menu error: {e}"))?;

        let tray_icon_img = icon::tray_icon_image(false)?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_tooltip("Keyboard LLM — Off")
            .with_icon(tray_icon_img)
            .build()
            .map_err(|e| anyhow!("Failed to create tray icon: {e}"))?;

        TRAY_UI.with(|slot| {
            *slot.borrow_mut() = Some(TrayUi {
                tray,
                toggle_item,
                last_active: Some(false),
            });
        });

        unsafe {
            let msg_taskbar_created =
                RegisterWindowMessageW(windows::core::w!("TaskbarButtonCreated"));
            let _ = ChangeWindowMessageFilterEx(hwnd, msg_taskbar_created, MSGFLT_ALLOW, None);
            let _ = ChangeWindowMessageFilterEx(hwnd, WM_COMMAND, MSGFLT_ALLOW, None);

            let state = Box::new(ThumbState {
                msg_taskbar_created,
                tbl: None,
                himl: HIMAGELIST::default(),
                buttons_added: false,
                is_on: false,
                is_busy: false,
                hotkey_label: Arc::clone(&hotkey_label),
                progress: Arc::clone(&progress),
                on_cmd: Arc::clone(&on_cmd),
                shutdown_flag: Arc::clone(&shutdown_flag),
            });
            let ptr = Box::into_raw(state);
            let ok = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, ptr as usize);
            if !ok.as_bool() {
                drop(Box::from_raw(ptr));
            } else {
                let _ = PostMessageW(hwnd, WM_APP_SYNC_SHELL, WPARAM(0), LPARAM(0));
            }
        }

        let menu_cmd = Arc::clone(&on_cmd);
        let menu_stop = Arc::clone(&shutdown_flag);
        let menu_ctx = egui_ctx.clone();
        let _ = thread::Builder::new()
            .name("tray-menu-events".to_string())
            .spawn(move || {
                let rx = MenuEvent::receiver();
                while !menu_stop.load(Ordering::Relaxed) {
                    if let Ok(evt) = rx.recv_timeout(std::time::Duration::from_millis(120)) {
                        if evt.id == show_id {
                            menu_cmd(ShellCommand::ShowWindow);
                            menu_ctx.request_repaint();
                        } else if evt.id == toggle_id {
                            menu_cmd(ShellCommand::ToggleOnOff);
                            menu_ctx.request_repaint();
                        } else if evt.id == quit_id {
                            menu_cmd(ShellCommand::Quit);
                            menu_ctx.request_repaint();
                            break;
                        }
                    }
                }
            });

        let click_cmd = Arc::clone(&on_cmd);
        let click_stop = Arc::clone(&shutdown_flag);
        let click_ctx = egui_ctx;
        let _ = thread::Builder::new()
            .name("tray-click-events".to_string())
            .spawn(move || {
                let rx = TrayIconEvent::receiver();
                while !click_stop.load(Ordering::Relaxed) {
                    if let Ok(evt) = rx.recv_timeout(std::time::Duration::from_millis(120)) {
                        match evt {
                            TrayIconEvent::Click {
                                button: MouseButton::Left,
                                button_state: MouseButtonState::Up,
                                ..
                            }
                            | TrayIconEvent::DoubleClick {
                                button: MouseButton::Left,
                                ..
                            } => {
                                click_cmd(ShellCommand::ShowWindow);
                                click_ctx.request_repaint();
                            }
                            _ => {}
                        }
                    }
                }
            });

        Ok(Self {
            hwnd_raw,
            hotkey_label,
            progress,
            shutdown_flag,
        })
    }

    pub fn set_hotkey_label(&self, label: String) {
        *self.hotkey_label.lock().unwrap() = label;
    }

    pub fn set_progress(&self, next: TaskbarProgress) {
        {
            let mut slot = self.progress.lock().unwrap();
            if *slot == next {
                return;
            }
            *slot = next;
        }
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_SET_PROGRESS, WPARAM(0), LPARAM(0));
        }
    }

    pub fn sync_state(&self, is_on: bool, is_busy: bool) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(
                hwnd,
                WM_APP_SYNC_SHELL,
                WPARAM(is_on as usize),
                LPARAM(is_busy as isize),
            );
        }
    }

    pub fn show_window(&self) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_SHOW_WINDOW, WPARAM(0), LPARAM(0));
        }
    }

    pub fn hide_window(&self) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_HIDE_WINDOW, WPARAM(0), LPARAM(0));
        }
    }

    pub fn request_close_window(&self) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_CLOSE_WINDOW, WPARAM(0), LPARAM(0));
        }
    }

    pub fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        drop_tray_on_ui_thread();
    }
}

struct ComInitGuard(bool);

impl Drop for ComInitGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// Opens a native Windows folder selection dialog. Must be called from an STA thread (e.g. a dedicated helper thread).
pub fn pick_folder_dialog(initial_dir: Option<&Path>) -> Option<PathBuf> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _com_guard = ComInitGuard(hr.is_ok());

        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let _ = dialog.SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM);

        if let Some(dir) = initial_dir {
            if dir.exists() {
                let wide: Vec<u16> = dir
                    .as_os_str()
                    .to_string_lossy()
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                if let Ok(item) =
                    SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None)
                {
                    let _ = dialog.SetFolder(&item);
                }
            }
        }

        dialog.Show(HWND::default()).ok()?;
        let result_item = dialog.GetResult().ok()?;
        let pwstr = result_item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path_str = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as _));
        path_str.map(PathBuf::from)
    }
}

/// Returns `(left, top, is_minimized, is_maximized)` in virtual-screen physical coordinates if the window is visible.
pub fn get_window_placement_state(hwnd_raw: isize) -> Option<(i32, i32, bool, bool)> {
    if hwnd_raw == 0 {
        return None;
    }
    let hwnd = HWND(hwnd_raw as *mut _);
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        let is_min = IsIconic(hwnd).as_bool();
        let is_max = IsZoomed(hwnd).as_bool();
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        Some((rect.left, rect.top, is_min, is_max))
    }
}

/// Restores a window's position in virtual-screen physical coordinates, clamping back onto the nearest visible monitor if needed.
pub fn restore_window_position(hwnd_raw: isize, x: i32, y: i32) {
    if hwnd_raw == 0 {
        return;
    }
    let hwnd = HWND(hwnd_raw as *mut _);
    unsafe {
        let mut cur = RECT::default();
        let (w, h) = if GetWindowRect(hwnd, &mut cur).is_ok() {
            ((cur.right - cur.left).max(200), (cur.bottom - cur.top).max(100))
        } else {
            (475, 485)
        };

        let probe = RECT {
            left: x,
            top: y,
            right: x + 200,
            bottom: y + 40,
        };
        let hmon_null = MonitorFromRect(&probe, MONITOR_DEFAULTTONULL);
        let hmon = if hmon_null.is_invalid() {
            MonitorFromRect(&probe, MONITOR_DEFAULTTONEAREST)
        } else {
            hmon_null
        };

        let mut target_x = x;
        let mut target_y = y;
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            let work = mi.rcWork;
            if hmon_null.is_invalid()
                || x < work.left
                || y < work.top
                || x + 80 > work.right
                || y + 40 > work.bottom
            {
                target_x = x.clamp(work.left, (work.right - w).max(work.left));
                target_y = y.clamp(work.top, (work.bottom - h).max(work.top));
            }
        }

        let _ = SetWindowPos(
            hwnd,
            None,
            target_x,
            target_y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: HWND,
        dwattribute: u32,
        pvattribute: *const core::ffi::c_void,
        cbattribute: u32,
    ) -> windows::core::HRESULT;
}

/// Syncs the native Windows title bar between light and dark mode.
pub fn set_window_dark_titlebar(hwnd_raw: isize, dark: bool) {
    if hwnd_raw == 0 {
        return;
    }
    let hwnd = HWND(hwnd_raw as *mut _);
    let enabled: i32 = i32::from(dark);
    unsafe {
        let hr = DwmSetWindowAttribute(
            hwnd,
            20, // DWMWA_USE_IMMERSIVE_DARK_MODE (Windows 11 / 10 20H1+)
            (&enabled as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
        if hr.is_err() {
            let _ = DwmSetWindowAttribute(
                hwnd,
                19, // Pre-20H1 Windows 10 fallback
                (&enabled as *const i32).cast(),
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
}
