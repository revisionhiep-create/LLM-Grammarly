use anyhow::Result;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
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
    RemoveWindowSubclass, SHCreateItemFromParsingName, SetWindowSubclass, Shell_NotifyIconW,
    TaskbarList, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP,
    NIIF_INFO, NIIF_LARGE_ICON, NIIF_NOSOUND, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, SIGDN_FILESYSPATH, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL,
    THBF_DISABLED, THBF_ENABLED, THBN_CLICKED, THB_BITMAP, THB_FLAGS, THB_TOOLTIP, THUMBBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, ChangeWindowMessageFilterEx, CreatePopupMenu, DestroyIcon, DestroyMenu,
    GetCursorPos, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed, PostMessageW,
    RegisterWindowMessageW, SetForegroundWindow, SetWindowPos, ShowWindow, TrackPopupMenu, HICON,
    HMENU, MF_ENABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSGFLT_ALLOW, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_COMMAND, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
    WM_NCDESTROY, WM_NULL, WM_PAINT, WM_RBUTTONUP, WM_USER,
};

use crate::icon;

const SUBCLASS_ID: usize = 0x4B4C_5442;
const BTN_TOGGLE_ID: u32 = 1001;
const BTN_QUIT_ID: u32 = 1002;
const TRAY_ICON_ID: u32 = 1;

const WM_APP_SYNC_SHELL: u32 = WM_APP + 20;
const WM_APP_SHOW_WINDOW: u32 = WM_APP + 21;
const WM_APP_HIDE_WINDOW: u32 = WM_APP + 22;
const WM_APP_CLOSE_WINDOW: u32 = WM_APP + 23;
const WM_APP_SET_PROGRESS: u32 = WM_APP + 24;
const WM_APP_TRAY_ICON: u32 = WM_APP + 25;
const WM_APP_NOTIFY: u32 = WM_APP + 26;

const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;

const MENU_ID_SHOW: usize = 1;
const MENU_ID_TOGGLE: usize = 2;
const MENU_ID_QUIT: usize = 3;

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

#[derive(Debug, Clone)]
struct TrayNotification {
    title: String,
    body: String,
    active_icon: bool,
}

fn fill_utf16<const N: usize>(buf: &mut [u16; N], text: &str) {
    buf.fill(0);
    if N <= 1 {
        return;
    }
    for (dst, ch) in buf.iter_mut().take(N - 1).zip(text.encode_utf16()) {
        *dst = ch;
    }
}

fn wide_null(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
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

struct ThumbState {
    hwnd: HWND,
    msg_taskbar_created: u32,
    msg_shell_taskbar_created: u32,
    com_initialized: bool,
    tbl: Option<ITaskbarList3>,
    himl: HIMAGELIST,
    buttons_added: bool,
    tray_added: bool,
    tray_hicon_off: HICON,
    tray_hicon_on: HICON,
    balloon_hicon_off: HICON,
    balloon_hicon_on: HICON,
    is_on: bool,
    is_busy: bool,
    hotkey_label: Arc<Mutex<String>>,
    progress: Arc<Mutex<TaskbarProgress>>,
    pending_notification: Arc<Mutex<VecDeque<TrayNotification>>>,
    on_cmd: Arc<dyn Fn(ShellCommand) + Send + Sync>,
}

impl Drop for ThumbState {
    fn drop(&mut self) {
        unsafe {
            if self.tray_added {
                remove_tray_icon(self.hwnd);
                self.tray_added = false;
            }
            for icon in [
                &mut self.tray_hicon_off,
                &mut self.tray_hicon_on,
                &mut self.balloon_hicon_off,
                &mut self.balloon_hicon_on,
            ] {
                if !icon.is_invalid() {
                    let _ = DestroyIcon(*icon);
                    *icon = HICON::default();
                }
            }
            if !self.himl.is_invalid() {
                let _ = ImageList_Destroy(self.himl);
                self.himl = HIMAGELIST::default();
            }
            self.tbl = None;
            if self.com_initialized {
                CoUninitialize();
                self.com_initialized = false;
            }
        }
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
    fill_utf16(&mut toggle.szTip, tip_text);

    let mut quit = THUMBBUTTON::default();
    quit.dwMask = THB_BITMAP | THB_TOOLTIP | THB_FLAGS;
    quit.iId = BTN_QUIT_ID;
    quit.iBitmap = 2;
    quit.dwFlags = THBF_ENABLED;
    fill_utf16(&mut quit.szTip, "Quit LLM Grammarly");

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

unsafe fn ensure_tray_icon(hwnd: HWND, state: &mut ThumbState, force_readd: bool) {
    if state.tray_hicon_off.is_invalid() {
        if let Ok(h) = icon::tray_hicon(false) {
            state.tray_hicon_off = h;
        }
    }
    if state.tray_hicon_on.is_invalid() {
        if let Ok(h) = icon::tray_hicon(true) {
            state.tray_hicon_on = h;
        }
    }

    let label = state.hotkey_label.lock().unwrap().clone();
    let progress = *state.progress.lock().unwrap();
    let tip = format_tray_tip(state.is_on, state.is_busy, &label, progress);

    let hicon = if state.is_on {
        state.tray_hicon_on
    } else {
        state.tray_hicon_off
    };

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_APP_TRAY_ICON,
        ..Default::default()
    };
    if !hicon.is_invalid() {
        nid.uFlags |= NIF_ICON;
        nid.hIcon = hicon;
    }
    fill_utf16(&mut nid.szTip, &tip);

    if !state.tray_added || force_readd {
        state.tray_added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    } else if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
        state.tray_added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    }
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn show_tray_notification(
    hwnd: HWND,
    state: &mut ThumbState,
    notif: &TrayNotification,
) {
    if !state.tray_added {
        ensure_tray_icon(hwnd, state, false);
    }

    // Clear any active balloon on this icon so back-to-back toggles display immediately.
    let clear_nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_INFO,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_MODIFY, &clear_nid);

    let balloon_hicon = if notif.active_icon {
        if state.balloon_hicon_on.is_invalid() {
            if let Ok(h) = icon::notification_hicon(true) {
                state.balloon_hicon_on = h;
            }
        }
        state.balloon_hicon_on
    } else {
        if state.balloon_hicon_off.is_invalid() {
            if let Ok(h) = icon::notification_hicon(false) {
                state.balloon_hicon_off = h;
            }
        }
        state.balloon_hicon_off
    };

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_INFO,
        ..Default::default()
    };
    fill_utf16(&mut nid.szInfoTitle, &notif.title);
    fill_utf16(&mut nid.szInfo, &notif.body);

    if !balloon_hicon.is_invalid() {
        nid.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON | NIIF_NOSOUND;
        nid.hBalloonIcon = balloon_hicon;
    } else {
        nid.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
    }

    if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
        ensure_tray_icon(hwnd, state, true);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

struct PopupMenuGuard(HMENU);

impl Drop for PopupMenuGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DestroyMenu(self.0);
            }
        }
    }
}

unsafe fn show_tray_context_menu(
    hwnd: HWND,
    is_on: bool,
    is_busy: bool,
    on_cmd: &Arc<dyn Fn(ShellCommand) + Send + Sync>,
) {
    let Ok(hmenu) = CreatePopupMenu() else {
        return;
    };
    let _menu_guard = PopupMenuGuard(hmenu);

    let show_w = wide_null("Show Window");
    let _ = AppendMenuW(
        hmenu,
        MF_STRING | MF_ENABLED,
        MENU_ID_SHOW,
        PCWSTR(show_w.as_ptr()),
    );

    let (toggle_label, toggle_flags) = if is_busy {
        ("Working...", MF_STRING | MF_GRAYED)
    } else if is_on {
        ("Turn Off", MF_STRING | MF_ENABLED)
    } else {
        ("Turn On", MF_STRING | MF_ENABLED)
    };
    let toggle_w = wide_null(toggle_label);
    let _ = AppendMenuW(
        hmenu,
        toggle_flags,
        MENU_ID_TOGGLE,
        PCWSTR(toggle_w.as_ptr()),
    );

    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

    let quit_w = wide_null("Quit");
    let _ = AppendMenuW(
        hmenu,
        MF_STRING | MF_ENABLED,
        MENU_ID_QUIT,
        PCWSTR(quit_w.as_ptr()),
    );

    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return;
    }
    let _ = SetForegroundWindow(hwnd);
    let selected = TrackPopupMenu(
        hmenu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

    match selected.0 as usize {
        MENU_ID_SHOW => on_cmd(ShellCommand::ShowWindow),
        MENU_ID_TOGGLE => on_cmd(ShellCommand::ToggleOnOff),
        MENU_ID_QUIT => on_cmd(ShellCommand::Quit),
        _ => {}
    }
}

unsafe fn ensure_thumbbar(hwnd: HWND, state: &mut ThumbState, force_readd: bool) {
    if !state.com_initialized {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        state.com_initialized = hr.is_ok();
    }

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

    if u_msg == WM_APP_TRAY_ICON {
        let evt = (l_param.0 as u32) & 0xFFFF;
        match evt {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_BALLOONUSERCLICK => {
                let on_cmd = {
                    let state = &*(dw_ref_data as *const ThumbState);
                    Arc::clone(&state.on_cmd)
                };
                on_cmd(ShellCommand::ShowWindow);
                return LRESULT(0);
            }
            WM_RBUTTONUP => {
                let (is_on, is_busy, on_cmd) = {
                    let state = &*(dw_ref_data as *const ThumbState);
                    (state.is_on, state.is_busy, Arc::clone(&state.on_cmd))
                };
                show_tray_context_menu(hwnd, is_on, is_busy, &on_cmd);
                return LRESULT(0);
            }
            _ => return LRESULT(0),
        }
    }

    if u_msg == WM_COMMAND {
        let wp = w_param.0 as u32;
        let hi = (wp >> 16) & 0xFFFF;
        let lo = wp & 0xFFFF;
        if hi == THBN_CLICKED {
            let on_cmd = {
                let state = &*(dw_ref_data as *const ThumbState);
                Arc::clone(&state.on_cmd)
            };
            if lo == BTN_TOGGLE_ID {
                on_cmd(ShellCommand::ToggleOnOff);
                return LRESULT(0);
            } else if lo == BTN_QUIT_ID {
                on_cmd(ShellCommand::Quit);
                return LRESULT(0);
            }
        }
        return DefSubclassProc(hwnd, u_msg, w_param, l_param);
    }

    if u_msg == WM_APP_CLOSE_WINDOW {
        let _ = DefSubclassProc(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        let _ = DefSubclassProc(hwnd, WM_PAINT, WPARAM(0), LPARAM(0));
        return LRESULT(0);
    }

    if u_msg == WM_CLOSE {
        let res = DefSubclassProc(hwnd, u_msg, w_param, l_param);
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            let _ = DefSubclassProc(hwnd, WM_PAINT, WPARAM(0), LPARAM(0));
        }
        return res;
    }

    if u_msg == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), u_id_subclass);
        let boxed = Box::from_raw(dw_ref_data as *mut ThumbState);
        drop(boxed);
        return DefSubclassProc(hwnd, u_msg, w_param, l_param);
    }

    let state = &mut *(dw_ref_data as *mut ThumbState);

    if u_msg == state.msg_taskbar_created {
        ensure_thumbbar(hwnd, state, true);
    } else if u_msg == state.msg_shell_taskbar_created {
        ensure_tray_icon(hwnd, state, true);
    } else if u_msg == WM_APP_SYNC_SHELL {
        state.is_on = w_param.0 != 0;
        state.is_busy = l_param.0 != 0;
        ensure_thumbbar(hwnd, state, false);
        ensure_tray_icon(hwnd, state, false);
        return LRESULT(0);
    } else if u_msg == WM_APP_SET_PROGRESS {
        let progress = *state.progress.lock().unwrap();
        if let Some(tbl) = state.tbl.as_ref() {
            apply_taskbar_progress(hwnd, tbl, progress);
        }
        ensure_tray_icon(hwnd, state, false);
        return LRESULT(0);
    } else if u_msg == WM_APP_NOTIFY {
        let notif = state.pending_notification.lock().unwrap().pop_front();
        if let Some(n) = notif {
            show_tray_notification(hwnd, state, &n);
        }
        return LRESULT(0);
    } else if u_msg == WM_APP_SHOW_WINDOW {
        let _ = ShowWindow(hwnd, SW_SHOW);
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        return LRESULT(0);
    } else if u_msg == WM_APP_HIDE_WINDOW {
        let _ = ShowWindow(hwnd, SW_HIDE);
        return LRESULT(0);
    }

    DefSubclassProc(hwnd, u_msg, w_param, l_param)
}

#[derive(Clone)]
pub struct ShellBridge {
    hwnd_raw: isize,
    hotkey_label: Arc<Mutex<String>>,
    progress: Arc<Mutex<TaskbarProgress>>,
    pending_notification: Arc<Mutex<VecDeque<TrayNotification>>>,
}

impl ShellBridge {
    pub fn install(
        hwnd_raw: isize,
        initial_hotkey_label: String,
        on_cmd: Arc<dyn Fn(ShellCommand) + Send + Sync>,
    ) -> Result<Self> {
        let hotkey_label = Arc::new(Mutex::new(initial_hotkey_label));
        let progress = Arc::new(Mutex::new(TaskbarProgress::None));
        let pending_notification = Arc::new(Mutex::new(VecDeque::new()));
        let hwnd = HWND(hwnd_raw as *mut _);

        unsafe {
            let msg_taskbar_created =
                RegisterWindowMessageW(windows::core::w!("TaskbarButtonCreated"));
            let msg_shell_taskbar_created =
                RegisterWindowMessageW(windows::core::w!("TaskbarCreated"));
            let _ = ChangeWindowMessageFilterEx(hwnd, msg_taskbar_created, MSGFLT_ALLOW, None);
            let _ =
                ChangeWindowMessageFilterEx(hwnd, msg_shell_taskbar_created, MSGFLT_ALLOW, None);
            let _ = ChangeWindowMessageFilterEx(hwnd, WM_COMMAND, MSGFLT_ALLOW, None);

            let state = Box::new(ThumbState {
                hwnd,
                msg_taskbar_created,
                msg_shell_taskbar_created,
                com_initialized: false,
                tbl: None,
                himl: HIMAGELIST::default(),
                buttons_added: false,
                tray_added: false,
                tray_hicon_off: HICON::default(),
                tray_hicon_on: HICON::default(),
                balloon_hicon_off: HICON::default(),
                balloon_hicon_on: HICON::default(),
                is_on: false,
                is_busy: false,
                hotkey_label: Arc::clone(&hotkey_label),
                progress: Arc::clone(&progress),
                pending_notification: Arc::clone(&pending_notification),
                on_cmd,
            });
            let ptr = Box::into_raw(state);
            let ok = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, ptr as usize);
            if !ok.as_bool() {
                drop(Box::from_raw(ptr));
            } else {
                let _ = PostMessageW(hwnd, WM_APP_SYNC_SHELL, WPARAM(0), LPARAM(0));
            }
        }

        Ok(Self {
            hwnd_raw,
            hotkey_label,
            progress,
            pending_notification,
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

    pub fn notify(&self, title: impl Into<String>, body: impl Into<String>, active_icon: bool) {
        if self.hwnd_raw == 0 {
            return;
        }
        {
            let mut queue = self.pending_notification.lock().unwrap();
            if queue.len() >= 4 {
                queue.pop_front();
            }
            queue.push_back(TrayNotification {
                title: title.into(),
                body: body.into(),
                active_icon,
            });
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_NOTIFY, WPARAM(0), LPARAM(0));
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
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            remove_tray_icon(hwnd);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_utf16_handles_normal_and_bounded_truncation() {
        let mut small = [0xFFFFu16; 6];
        fill_utf16(&mut small, "On");
        assert_eq!(small, [b'O' as u16, b'n' as u16, 0, 0, 0, 0]);

        let mut trunc = [0xFFFFu16; 5];
        fill_utf16(&mut trunc, "Hello world — café");
        assert_eq!(trunc[4], 0);
        assert_eq!(
            String::from_utf16(&trunc[..4]).unwrap(),
            "Hell"
        );
    }

    #[test]
    fn format_tray_tip_reflects_state() {
        assert_eq!(
            format_tray_tip(false, false, "Ctrl + Q", TaskbarProgress::None),
            "LLM Grammarly — Off"
        );
        assert_eq!(
            format_tray_tip(true, false, "F8", TaskbarProgress::None),
            "LLM Grammarly — On (F8)"
        );
        assert_eq!(
            format_tray_tip(false, true, "F8", TaskbarProgress::Indeterminate),
            "LLM Grammarly — Loading..."
        );
        assert_eq!(
            format_tray_tip(
                false,
                true,
                "F8",
                TaskbarProgress::Determinate {
                    current: 50,
                    total: 200
                }
            ),
            "LLM Grammarly — Downloading (25%)"
        );
    }
}
