use anyhow::{anyhow, bail, Result};
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, SendInput, UnregisterHotKey, HOT_KEY_MODIFIERS, INPUT,
    INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MOD_ALT,
    MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VIRTUAL_KEY, VK_CONTROL, VK_LCONTROL,
    VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_APP,
    WM_HOTKEY,
};

use crate::config::HotkeyBinding;
use crate::llm::LlmHandle;

const HOTKEY_FIX_ID: i32 = 1;
const HOTKEY_TOGGLE_POWER_ID: i32 = 2;
const HOTKEY_TOGGLE_HIGHLIGHT_ID: i32 = 3;

const WM_APP_CMD: u32 = WM_APP + 1;

#[derive(Debug, Clone)]
pub enum HotkeyEvent {
    FixingStarted,
    FixSucceeded { elapsed_ms: u128, changed: bool },
    FixCopiedToClipboard { elapsed_ms: u128 },
    FixNotice(String),
    FixError(String),
    TogglePowerRequested,
    ToggleHighlightRequested,
}

enum HotkeyCmd {
    Enable {
        binding: HotkeyBinding,
        reply: Sender<Result<()>>,
    },
    Disable {
        reply: Sender<()>,
    },
    SetTogglePower {
        binding: Option<HotkeyBinding>,
        reply: Sender<Result<()>>,
    },
    SetToggleHighlight {
        binding: Option<HotkeyBinding>,
        reply: Sender<Result<()>>,
    },
    Quit,
}

pub struct HotkeyController {
    thread_id: u32,
    cmd_tx: Sender<HotkeyCmd>,
    join: Option<JoinHandle<()>>,
}

impl HotkeyController {
    pub fn spawn(
        llm: Arc<LlmHandle>,
        fix_highlighted_only: Arc<AtomicBool>,
        recording_hotkey: Arc<AtomicBool>,
        notify: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
    ) -> Result<Self> {
        let (tid_tx, tid_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();

        let join = thread::Builder::new()
            .name("hotkey-loop".to_string())
            .spawn(move || {
                hotkey_thread(
                    tid_tx,
                    cmd_rx,
                    fix_highlighted_only,
                    recording_hotkey,
                    llm,
                    notify,
                )
            })
            .map_err(|e| anyhow!("Failed to spawn hotkey thread: {e}"))?;

        let thread_id = tid_rx
            .recv()
            .map_err(|_| anyhow!("Hotkey thread exited during initialization"))?;

        Ok(Self {
            thread_id,
            cmd_tx,
            join: Some(join),
        })
    }

    fn post_wake(&self) -> Result<()> {
        unsafe {
            PostThreadMessageW(self.thread_id, WM_APP_CMD, WPARAM(0), LPARAM(0))
                .map_err(|e| anyhow!("Failed to signal hotkey thread: {e}"))
        }
    }

    pub fn enable(&self, binding: HotkeyBinding) -> Result<()> {
        if !binding.is_valid() {
            bail!("Invalid hotkey binding: {}", binding.display_label());
        }
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(HotkeyCmd::Enable { binding, reply: tx })
            .map_err(|_| anyhow!("Hotkey thread stopped"))?;
        self.post_wake()?;
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("Timed out enabling {} hotkey", binding.display_label()))?
    }

    pub fn set_toggle_power(&self, binding: Option<HotkeyBinding>) -> Result<()> {
        if let Some(b) = binding {
            if !b.is_valid() {
                bail!("Invalid On / Off hotkey binding: {}", b.display_label());
            }
        }
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(HotkeyCmd::SetTogglePower { binding, reply: tx })
            .map_err(|_| anyhow!("Hotkey thread stopped"))?;
        self.post_wake()?;
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("Timed out updating On / Off hotkey"))?
    }

    pub fn set_toggle_highlight(&self, binding: Option<HotkeyBinding>) -> Result<()> {
        if let Some(b) = binding {
            if !b.is_valid() {
                bail!(
                    "Invalid Highlight mode hotkey binding: {}",
                    b.display_label()
                );
            }
        }
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(HotkeyCmd::SetToggleHighlight { binding, reply: tx })
            .map_err(|_| anyhow!("Hotkey thread stopped"))?;
        self.post_wake()?;
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("Timed out updating Highlight mode hotkey"))?
    }

    pub fn disable(&self) {
        let (tx, rx) = mpsc::channel();
        if self.cmd_tx.send(HotkeyCmd::Disable { reply: tx }).is_ok() && self.post_wake().is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }

    pub fn shutdown(&mut self) {
        if self.cmd_tx.send(HotkeyCmd::Quit).is_ok() {
            let _ = self.post_wake();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for HotkeyController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct InProgressGuard(Arc<AtomicBool>);

impl Drop for InProgressGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn build_modifiers(binding: HotkeyBinding) -> HOT_KEY_MODIFIERS {
    let mut mods = MOD_NOREPEAT;
    if binding.ctrl {
        mods |= MOD_CONTROL;
    }
    if binding.alt {
        mods |= MOD_ALT;
    }
    if binding.shift {
        mods |= MOD_SHIFT;
    }
    if binding.win {
        mods |= MOD_WIN;
    }
    mods
}

unsafe fn update_aux_hotkey(id: i32, name: &str, binding: Option<HotkeyBinding>) -> Result<()> {
    let _ = UnregisterHotKey(None, id);
    if let Some(b) = binding {
        RegisterHotKey(None, id, build_modifiers(b), b.vk).map_err(|_| {
            anyhow!(
                "{} ({name}) is already registered by another application",
                b.display_label()
            )
        })?;
    }
    Ok(())
}

unsafe fn drain_hotkey_commands(
    cmd_rx: &mpsc::Receiver<HotkeyCmd>,
    active_binding: &mut HotkeyBinding,
) -> bool {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            HotkeyCmd::Enable { binding, reply } => {
                let _ = UnregisterHotKey(None, HOTKEY_FIX_ID);
                let res = RegisterHotKey(None, HOTKEY_FIX_ID, build_modifiers(binding), binding.vk)
                    .map(|()| {
                        *active_binding = binding;
                    })
                    .map_err(|_| {
                        anyhow!(
                            "{} is already registered by another application",
                            binding.display_label()
                        )
                    });
                let _ = reply.send(res);
            }
            HotkeyCmd::Disable { reply } => {
                let _ = UnregisterHotKey(None, HOTKEY_FIX_ID);
                let _ = reply.send(());
            }
            HotkeyCmd::SetTogglePower { binding, reply } => {
                let res = update_aux_hotkey(HOTKEY_TOGGLE_POWER_ID, "On / Off", binding);
                let _ = reply.send(res);
            }
            HotkeyCmd::SetToggleHighlight { binding, reply } => {
                let res = update_aux_hotkey(HOTKEY_TOGGLE_HIGHLIGHT_ID, "Highlight mode", binding);
                let _ = reply.send(res);
            }
            HotkeyCmd::Quit => {
                let _ = UnregisterHotKey(None, HOTKEY_FIX_ID);
                let _ = UnregisterHotKey(None, HOTKEY_TOGGLE_POWER_ID);
                let _ = UnregisterHotKey(None, HOTKEY_TOGGLE_HIGHLIGHT_ID);
                return true;
            }
        }
    }
    false
}

fn hotkey_thread(
    tid_tx: Sender<u32>,
    cmd_rx: mpsc::Receiver<HotkeyCmd>,
    fix_highlighted_only: Arc<AtomicBool>,
    recording_hotkey: Arc<AtomicBool>,
    llm: Arc<LlmHandle>,
    notify: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
) {
    unsafe {
        // Force creation of the thread message queue before sharing thread ID.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_NOREMOVE);
        let tid = GetCurrentThreadId();
        let _ = tid_tx.send(tid);

        let mut active_binding = HotkeyBinding::default();
        if drain_hotkey_commands(&cmd_rx, &mut active_binding) {
            return;
        }

        let in_progress = Arc::new(AtomicBool::new(false));
        let mut last_fix_trigger: Option<Instant> = None;
        let mut last_power_trigger: Option<Instant> = None;
        let mut last_highlight_trigger: Option<Instant> = None;

        loop {
            let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
            if ret.0 <= 0 {
                break;
            }
            match msg.message {
                WM_APP_CMD => {
                    if drain_hotkey_commands(&cmd_rx, &mut active_binding) {
                        break;
                    }
                }
                WM_HOTKEY => {
                    if recording_hotkey.load(Ordering::Relaxed) {
                        continue;
                    }
                    let id = msg.wParam.0 as i32;
                    let now = Instant::now();
                    match id {
                        HOTKEY_FIX_ID => {
                            if let Some(prev) = last_fix_trigger {
                                if now.duration_since(prev) < Duration::from_millis(400) {
                                    continue;
                                }
                            }
                            if in_progress
                                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                                .is_err()
                            {
                                continue;
                            }
                            last_fix_trigger = Some(now);

                            let guard = InProgressGuard(Arc::clone(&in_progress));
                            let binding = active_binding;
                            let highlighted_only = fix_highlighted_only.load(Ordering::Relaxed);
                            let llm_clone = Arc::clone(&llm);
                            let notify_clone = Arc::clone(&notify);
                            if thread::Builder::new()
                                .name("hotkey-fix".to_string())
                                .spawn(move || {
                                    let _guard = guard;
                                    run_fix_pipeline(
                                        binding,
                                        highlighted_only,
                                        llm_clone,
                                        notify_clone,
                                    );
                                })
                                .is_err()
                            {
                                notify(HotkeyEvent::FixError(
                                    "Failed to start text-fix worker thread".to_string(),
                                ));
                            }
                        }
                        HOTKEY_TOGGLE_POWER_ID => {
                            if let Some(prev) = last_power_trigger {
                                if now.duration_since(prev) < Duration::from_millis(350) {
                                    continue;
                                }
                            }
                            last_power_trigger = Some(now);
                            notify(HotkeyEvent::TogglePowerRequested);
                        }
                        HOTKEY_TOGGLE_HIGHLIGHT_ID => {
                            if let Some(prev) = last_highlight_trigger {
                                if now.duration_since(prev) < Duration::from_millis(250) {
                                    continue;
                                }
                            }
                            last_highlight_trigger = Some(now);
                            notify(HotkeyEvent::ToggleHighlightRequested);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

fn is_key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

fn wait_for_hotkey_release(binding: HotkeyBinding) {
    for _ in 0..88 {
        let ctrl_down = is_key_down(VK_CONTROL.0 as i32)
            || is_key_down(VK_LCONTROL.0 as i32)
            || is_key_down(VK_RCONTROL.0 as i32);
        let alt_down = is_key_down(VK_MENU.0 as i32)
            || is_key_down(VK_LMENU.0 as i32)
            || is_key_down(VK_RMENU.0 as i32);
        let shift_down = is_key_down(VK_SHIFT.0 as i32)
            || is_key_down(VK_LSHIFT.0 as i32)
            || is_key_down(VK_RSHIFT.0 as i32);
        let win_down = is_key_down(VK_LWIN.0 as i32) || is_key_down(VK_RWIN.0 as i32);
        let key_down = is_key_down(binding.vk as i32);

        if !ctrl_down && !alt_down && !shift_down && !win_down && !key_down {
            return;
        }
        thread::sleep(Duration::from_millis(8));
    }
}

fn key_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if key_up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_ctrl_combo(key: u16) {
    let inputs = [
        key_input(VK_CONTROL, false),
        key_input(VIRTUAL_KEY(key), false),
        key_input(VIRTUAL_KEY(key), true),
        key_input(VK_CONTROL, true),
    ];
    unsafe {
        let _ = SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

fn tap_key(vk: VIRTUAL_KEY) {
    let inputs = [key_input(vk, false), key_input(vk, true)];
    unsafe {
        let _ = SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn open_clipboard_retry() -> Result<ClipboardGuard> {
    for _ in 0..12 {
        unsafe {
            if OpenClipboard(None).is_ok() {
                return Ok(ClipboardGuard);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    bail!("Clipboard is locked by another application")
}

fn read_clipboard_text() -> Option<String> {
    let _guard = open_clipboard_retry().ok()?;
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() {
            return None;
        }
        let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        if handle.is_invalid() {
            return None;
        }
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        let text = String::from_utf16_lossy(slice);
        let _ = GlobalUnlock(hglobal);
        Some(text)
    }
}

fn write_clipboard_text(text: &str) -> Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let byte_len = utf16.len() * size_of::<u16>();

    unsafe {
        let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_len)
            .map_err(|e| anyhow!("Clipboard GlobalAlloc failed: {e}"))?;
        let ptr = GlobalLock(hmem) as *mut u16;
        if ptr.is_null() {
            let _ = GlobalFree(hmem);
            bail!("Clipboard GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
        let _ = GlobalUnlock(hmem);

        let _guard = match open_clipboard_retry() {
            Ok(g) => g,
            Err(e) => {
                let _ = GlobalFree(hmem);
                return Err(e);
            }
        };

        if let Err(e) = EmptyClipboard() {
            let _ = GlobalFree(hmem);
            bail!("EmptyClipboard failed: {e}");
        }

        if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0)) {
            let _ = GlobalFree(hmem);
            bail!("SetClipboardData failed: {e}");
        }
    }

    Ok(())
}

fn poll_clipboard_change(seq_before: u32, max_wait_ms: u64) -> bool {
    let steps = (max_wait_ms / 10).max(1);
    for _ in 0..steps {
        thread::sleep(Duration::from_millis(10));
        let now = unsafe { GetClipboardSequenceNumber() };
        if now != seq_before {
            return true;
        }
    }
    false
}

fn run_fix_pipeline(
    binding: HotkeyBinding,
    fix_highlighted_only: bool,
    llm: Arc<LlmHandle>,
    notify: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
) {
    wait_for_hotkey_release(binding);

    let fg_before = unsafe { GetForegroundWindow() };
    let prev_clipboard = read_clipboard_text();
    let seq_before = unsafe { GetClipboardSequenceNumber() };

    let copied = if fix_highlighted_only {
        send_ctrl_combo(b'C' as u16);
        let mut ok = poll_clipboard_change(seq_before, 180);
        if !ok {
            thread::sleep(Duration::from_millis(80));
            send_ctrl_combo(b'C' as u16);
            ok = poll_clipboard_change(seq_before, 220);
        }
        ok
    } else {
        send_ctrl_combo(b'A' as u16);
        thread::sleep(Duration::from_millis(50));
        send_ctrl_combo(b'C' as u16);
        let mut ok = poll_clipboard_change(seq_before, 220);
        if !ok {
            send_ctrl_combo(b'A' as u16);
            thread::sleep(Duration::from_millis(120));
            send_ctrl_combo(b'C' as u16);
            ok = poll_clipboard_change(seq_before, 280);
        }
        ok
    };

    if !copied {
        let msg = if fix_highlighted_only {
            "No highlighted text selected"
        } else {
            "No text found to fix"
        };
        notify(HotkeyEvent::FixNotice(msg.to_string()));
        return;
    }

    let captured = read_clipboard_text().unwrap_or_default();
    if captured.trim().is_empty() {
        if let Some(prev) = prev_clipboard {
            let _ = write_clipboard_text(&prev);
        }
        let msg = if fix_highlighted_only {
            "No highlighted text selected"
        } else {
            "No text found to fix"
        };
        notify(HotkeyEvent::FixNotice(msg.to_string()));
        return;
    }

    notify(HotkeyEvent::FixingStarted);

    match llm.fix_text(captured.clone()) {
        Ok((fixed, elapsed_ms)) => {
            let fg_after = unsafe { GetForegroundWindow() };
            let same_window = fg_before.0.is_null() || fg_before == fg_after;

            if fixed == captured || fixed.trim().is_empty() {
                if !fix_highlighted_only && same_window {
                    tap_key(VK_RIGHT);
                }
                if let Some(prev) = prev_clipboard {
                    let _ = write_clipboard_text(&prev);
                }
                notify(HotkeyEvent::FixSucceeded {
                    elapsed_ms,
                    changed: false,
                });
                return;
            }

            if let Err(err) = write_clipboard_text(&fixed) {
                notify(HotkeyEvent::FixError(err.to_string()));
                return;
            }

            if !same_window {
                notify(HotkeyEvent::FixCopiedToClipboard { elapsed_ms });
                return;
            }

            let seq_after_set = unsafe { GetClipboardSequenceNumber() };
            thread::sleep(Duration::from_millis(15));
            send_ctrl_combo(b'V' as u16);

            notify(HotkeyEvent::FixSucceeded {
                elapsed_ms,
                changed: true,
            });

            if let Some(prev) = prev_clipboard {
                thread::sleep(Duration::from_millis(200));
                let seq_now = unsafe { GetClipboardSequenceNumber() };
                if seq_now == seq_after_set {
                    let _ = write_clipboard_text(&prev);
                }
            }
        }
        Err(err) => {
            if let Some(prev) = prev_clipboard {
                let _ = write_clipboard_text(&prev);
            }
            notify(HotkeyEvent::FixError(err.to_string()));
        }
    }
}
