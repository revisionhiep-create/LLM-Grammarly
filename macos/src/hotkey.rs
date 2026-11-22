use anyhow::{anyhow, bail, Result};
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::HotkeyBinding;
use crate::llm::LlmHandle;

const HOTKEY_FIX_ID: u32 = 1;
const HOTKEY_TOGGLE_POWER_ID: u32 = 2;
const HOTKEY_TOGGLE_HIGHLIGHT_ID: u32 = 3;

const KVK_ANSI_A: u16 = 0x00;
const KVK_ANSI_C: u16 = 0x08;
const KVK_ANSI_V: u16 = 0x09;
const KVK_RIGHT_ARROW: u16 = 0x7C;

static ACCESSIBILITY_PROMPTED: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn macos_hotkey_init(
        cb: extern "C" fn(u32, *mut c_void),
        user_data: *mut c_void,
    ) -> bool;
    fn macos_hotkey_register(id: u32, keycode: u32, carbon_mods: u32) -> i32;
    fn macos_hotkey_unregister(id: u32);
    fn macos_hotkey_shutdown();
    fn macos_is_accessibility_trusted(prompt: bool) -> bool;
    fn macos_open_accessibility_settings();
    fn macos_is_key_down(keycode: u16) -> bool;
    fn macos_send_cmd_key(keycode: u16);
    fn macos_tap_key(keycode: u16);
    fn macos_frontmost_pid() -> i32;
    fn macos_clipboard_change_count() -> i64;
    fn macos_clipboard_read_text() -> *mut c_char;
    fn macos_clipboard_write_text(utf8: *const c_char) -> bool;
    fn macos_free_string(ptr: *mut c_char);
}

pub fn is_accessibility_trusted() -> bool {
    unsafe { macos_is_accessibility_trusted(false) }
}

pub fn request_accessibility_permission() {
    unsafe {
        let _ = macos_is_accessibility_trusted(true);
        macos_open_accessibility_settings();
    }
}

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

enum LoopMsg {
    Cmd(HotkeyCmd),
    Triggered(u32),
}

extern "C" fn on_carbon_hotkey(hotkey_id: u32, user_data: *mut c_void) {
    if !user_data.is_null() {
        let tx = unsafe { &*(user_data as *const Sender<LoopMsg>) };
        let _ = tx.send(LoopMsg::Triggered(hotkey_id));
    }
}

pub struct HotkeyController {
    cmd_tx: Sender<LoopMsg>,
    join: Option<JoinHandle<()>>,
}

impl HotkeyController {
    pub fn spawn(
        llm: Arc<LlmHandle>,
        fix_highlighted_only: Arc<AtomicBool>,
        recording_hotkey: Arc<AtomicBool>,
        doc_review_busy: Arc<AtomicBool>,
        notify: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<LoopMsg>();
        let cb_tx_ptr = Box::into_raw(Box::new(cmd_tx.clone())) as *mut c_void;

        // Called on the AppKit main thread inside KeyboardLlmApp::new.
        let ok = unsafe { macos_hotkey_init(on_carbon_hotkey, cb_tx_ptr) };
        if !ok {
            unsafe {
                drop(Box::from_raw(cb_tx_ptr as *mut Sender<LoopMsg>));
            }
            bail!("Failed to initialize macOS Carbon hotkey handler");
        }

        let join = thread::Builder::new()
            .name("hotkey-loop".to_string())
            .spawn(move || {
                hotkey_thread(
                    cmd_rx,
                    fix_highlighted_only,
                    recording_hotkey,
                    doc_review_busy,
                    llm,
                    notify,
                )
            })
            .map_err(|e| anyhow!("Failed to spawn hotkey thread: {e}"))?;

        Ok(Self {
            cmd_tx,
            join: Some(join),
        })
    }

    pub fn enable(&self, binding: HotkeyBinding) -> Result<()> {
        if !binding.is_valid() {
            bail!("Invalid hotkey binding: {}", binding.display_label());
        }
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(LoopMsg::Cmd(HotkeyCmd::Enable { binding, reply: tx }))
            .map_err(|_| anyhow!("Hotkey thread stopped"))?;
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
            .send(LoopMsg::Cmd(HotkeyCmd::SetTogglePower {
                binding,
                reply: tx,
            }))
            .map_err(|_| anyhow!("Hotkey thread stopped"))?;
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
            .send(LoopMsg::Cmd(HotkeyCmd::SetToggleHighlight {
                binding,
                reply: tx,
            }))
            .map_err(|_| anyhow!("Hotkey thread stopped"))?;
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("Timed out updating Highlight mode hotkey"))?
    }

    pub fn disable(&self) {
        let (tx, rx) = mpsc::channel();
        if self
            .cmd_tx
            .send(LoopMsg::Cmd(HotkeyCmd::Disable { reply: tx }))
            .is_ok()
        {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(LoopMsg::Cmd(HotkeyCmd::Quit));
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

fn binding_to_carbon_mods(binding: HotkeyBinding) -> u32 {
    let mut mods = 0u32;
    if binding.ctrl {
        mods |= 1 << 12; // controlKey
    }
    if binding.alt {
        mods |= 1 << 11; // optionKey
    }
    if binding.shift {
        mods |= 1 << 9; // shiftKey
    }
    if binding.win {
        mods |= 1 << 8; // cmdKey
    }
    mods
}

pub fn vk_to_macos_keycode(vk: u32) -> Option<u16> {
    let code = match vk {
        0x41 => 0x00, // A
        0x42 => 0x0B, // B
        0x43 => 0x08, // C
        0x44 => 0x02, // D
        0x45 => 0x0E, // E
        0x46 => 0x03, // F
        0x47 => 0x05, // G
        0x48 => 0x04, // H
        0x49 => 0x22, // I
        0x4A => 0x26, // J
        0x4B => 0x28, // K
        0x4C => 0x25, // L
        0x4D => 0x2E, // M
        0x4E => 0x2D, // N
        0x4F => 0x1F, // O
        0x50 => 0x23, // P
        0x51 => 0x0C, // Q
        0x52 => 0x0F, // R
        0x53 => 0x01, // S
        0x54 => 0x11, // T
        0x55 => 0x20, // U
        0x56 => 0x09, // V
        0x57 => 0x0D, // W
        0x58 => 0x07, // X
        0x59 => 0x10, // Y
        0x5A => 0x06, // Z
        0x30 => 0x1D, // 0
        0x31 => 0x12, // 1
        0x32 => 0x13, // 2
        0x33 => 0x14, // 3
        0x34 => 0x15, // 4
        0x35 => 0x17, // 5
        0x36 => 0x16, // 6
        0x37 => 0x1A, // 7
        0x38 => 0x1C, // 8
        0x39 => 0x19, // 9
        0x70 => 0x7A, // F1
        0x71 => 0x78, // F2
        0x72 => 0x63, // F3
        0x73 => 0x76, // F4
        0x74 => 0x60, // F5
        0x75 => 0x61, // F6
        0x76 => 0x62, // F7
        0x77 => 0x64, // F8
        0x78 => 0x65, // F9
        0x79 => 0x6D, // F10
        0x7A => 0x67, // F11
        0x7B => 0x6F, // F12
        0x20 => 0x31, // Space
        0x09 => 0x30, // Tab
        0x2D => 0x72, // Insert / Help
        0x2E => 0x75, // Forward Delete
        0x24 => 0x73, // Home
        0x23 => 0x77, // End
        0x21 => 0x74, // PageUp
        0x22 => 0x79, // PageDown
        0x25 => 0x7B, // Left
        0x26 => 0x7E, // Up
        0x27 => 0x7C, // Right
        0x28 => 0x7D, // Down
        0xBD => 0x1B, // -
        0xBB => 0x18, // =
        0xDB => 0x21, // [
        0xDD => 0x1E, // ]
        0xBA => 0x29, // ;
        0xDE => 0x27, // '
        0xBC => 0x2B, // ,
        0xBE => 0x2F, // .
        0xBF => 0x2C, // /
        0xC0 => 0x32, // `
        0xDC => 0x2A, // \
        _ => return None,
    };
    Some(code)
}

fn register_mac_hotkey(id: u32, name: Option<&str>, binding: HotkeyBinding) -> Result<()> {
    let keycode = vk_to_macos_keycode(binding.vk)
        .ok_or_else(|| anyhow!("Unsupported key for macOS: {}", binding.display_label()))?;
    let mods = binding_to_carbon_mods(binding);
    let status = unsafe { macos_hotkey_register(id, keycode as u32, mods) };
    if status != 0 {
        if let Some(n) = name {
            bail!(
                "{} ({n}) could not be registered (already used by macOS or another app, OSStatus {status})",
                binding.display_label()
            );
        } else {
            bail!(
                "{} could not be registered (already used by macOS or another app, OSStatus {status})",
                binding.display_label()
            );
        }
    }
    Ok(())
}

fn hotkey_thread(
    rx: mpsc::Receiver<LoopMsg>,
    fix_highlighted_only: Arc<AtomicBool>,
    recording_hotkey: Arc<AtomicBool>,
    doc_review_busy: Arc<AtomicBool>,
    llm: Arc<LlmHandle>,
    notify: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
) {
    let mut active_binding = HotkeyBinding::default();
    let in_progress = Arc::new(AtomicBool::new(false));
    let mut last_fix_trigger: Option<Instant> = None;
    let mut last_power_trigger: Option<Instant> = None;
    let mut last_highlight_trigger: Option<Instant> = None;

    while let Ok(msg) = rx.recv() {
        match msg {
            LoopMsg::Cmd(cmd) => match cmd {
                HotkeyCmd::Enable { binding, reply } => {
                    let res = register_mac_hotkey(HOTKEY_FIX_ID, None, binding).map(|()| {
                        active_binding = binding;
                    });
                    let _ = reply.send(res);
                }
                HotkeyCmd::Disable { reply } => {
                    unsafe {
                        macos_hotkey_unregister(HOTKEY_FIX_ID);
                    }
                    let _ = reply.send(());
                }
                HotkeyCmd::SetTogglePower { binding, reply } => {
                    let res = match binding {
                        Some(b) => register_mac_hotkey(HOTKEY_TOGGLE_POWER_ID, Some("On / Off"), b),
                        None => {
                            unsafe {
                                macos_hotkey_unregister(HOTKEY_TOGGLE_POWER_ID);
                            }
                            Ok(())
                        }
                    };
                    let _ = reply.send(res);
                }
                HotkeyCmd::SetToggleHighlight { binding, reply } => {
                    let res = match binding {
                        Some(b) => register_mac_hotkey(
                            HOTKEY_TOGGLE_HIGHLIGHT_ID,
                            Some("Highlight mode"),
                            b,
                        ),
                        None => {
                            unsafe {
                                macos_hotkey_unregister(HOTKEY_TOGGLE_HIGHLIGHT_ID);
                            }
                            Ok(())
                        }
                    };
                    let _ = reply.send(res);
                }
                HotkeyCmd::Quit => {
                    unsafe {
                        macos_hotkey_shutdown();
                    }
                    break;
                }
            },
            LoopMsg::Triggered(id) => {
                if recording_hotkey.load(Ordering::Relaxed) {
                    continue;
                }
                let now = Instant::now();
                match id {
                    HOTKEY_FIX_ID => {
                        if doc_review_busy.load(Ordering::Relaxed) {
                            notify(HotkeyEvent::FixNotice(
                                "Paused while Review Docs is running".to_string(),
                            ));
                            continue;
                        }
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
        }
    }
}

fn wait_for_hotkey_release(binding: HotkeyBinding) {
    let keycode = vk_to_macos_keycode(binding.vk);
    let modifier_codes: [u16; 8] = [
        0x37, // Command
        0x36, // Right Command
        0x38, // Shift
        0x3C, // Right Shift
        0x3A, // Option
        0x3D, // Right Option
        0x3B, // Control
        0x3E, // Right Control
    ];
    for _ in 0..88 {
        let mod_down = modifier_codes
            .iter()
            .any(|&code| unsafe { macos_is_key_down(code) });
        let key_down = keycode.is_some_and(|code| unsafe { macos_is_key_down(code) });
        if !mod_down && !key_down {
            return;
        }
        thread::sleep(Duration::from_millis(8));
    }
}

fn read_clipboard_text() -> Option<String> {
    unsafe {
        let ptr = macos_clipboard_read_text();
        if ptr.is_null() {
            return None;
        }
        let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        macos_free_string(ptr);
        Some(text)
    }
}

fn write_clipboard_text(text: &str) -> Result<()> {
    let sanitized;
    let payload = if text.as_bytes().contains(&0) {
        sanitized = text.replace('\0', "");
        sanitized.as_str()
    } else {
        text
    };
    let cstr = CString::new(payload).map_err(|e| anyhow!("Clipboard encoding error: {e}"))?;
    let ok = unsafe { macos_clipboard_write_text(cstr.as_ptr()) };
    if !ok {
        bail!("Failed to write text to macOS clipboard");
    }
    Ok(())
}

fn poll_clipboard_change(seq_before: i64, max_wait_ms: u64) -> bool {
    let steps = (max_wait_ms / 10).max(1);
    for _ in 0..steps {
        thread::sleep(Duration::from_millis(10));
        let now = unsafe { macos_clipboard_change_count() };
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

    if unsafe { !macos_is_accessibility_trusted(false) } {
        if !ACCESSIBILITY_PROMPTED.swap(true, Ordering::Relaxed) {
            unsafe {
                let _ = macos_is_accessibility_trusted(true);
            }
        }
        notify(HotkeyEvent::FixError(
            "Accessibility permission required: System Settings → Privacy & Security → Accessibility → enable LLM Grammarly"
                .to_string(),
        ));
        return;
    }

    let pid_before = unsafe { macos_frontmost_pid() };
    let prev_clipboard = read_clipboard_text();
    let seq_before = unsafe { macos_clipboard_change_count() };

    let copied = if fix_highlighted_only {
        unsafe { macos_send_cmd_key(KVK_ANSI_C) };
        let mut ok = poll_clipboard_change(seq_before, 200);
        if !ok {
            thread::sleep(Duration::from_millis(80));
            unsafe { macos_send_cmd_key(KVK_ANSI_C) };
            ok = poll_clipboard_change(seq_before, 240);
        }
        ok
    } else {
        unsafe { macos_send_cmd_key(KVK_ANSI_A) };
        thread::sleep(Duration::from_millis(60));
        unsafe { macos_send_cmd_key(KVK_ANSI_C) };
        let mut ok = poll_clipboard_change(seq_before, 240);
        if !ok {
            unsafe { macos_send_cmd_key(KVK_ANSI_A) };
            thread::sleep(Duration::from_millis(120));
            unsafe { macos_send_cmd_key(KVK_ANSI_C) };
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
            let pid_after = unsafe { macos_frontmost_pid() };
            let same_app = pid_before != 0 && pid_before == pid_after;

            if fixed == captured || fixed.trim().is_empty() {
                if !fix_highlighted_only && same_app {
                    unsafe { macos_tap_key(KVK_RIGHT_ARROW) };
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

            if !same_app {
                notify(HotkeyEvent::FixCopiedToClipboard { elapsed_ms });
                return;
            }

            let seq_after_set = unsafe { macos_clipboard_change_count() };
            thread::sleep(Duration::from_millis(20));
            unsafe { macos_send_cmd_key(KVK_ANSI_V) };

            notify(HotkeyEvent::FixSucceeded {
                elapsed_ms,
                changed: true,
            });

            if let Some(prev) = prev_clipboard {
                thread::sleep(Duration::from_millis(250));
                let seq_now = unsafe { macos_clipboard_change_count() };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::vk_to_name;
    use std::collections::HashSet;

    #[test]
    fn all_supported_vks_map_to_distinct_macos_keycodes() {
        let mut seen = HashSet::new();
        let mut count = 0usize;
        for vk in 0x00u32..=0xFFu32 {
            if let Some(name) = vk_to_name(vk) {
                let mac = vk_to_macos_keycode(vk)
                    .unwrap_or_else(|| panic!("Missing macOS keycode for {name} (0x{vk:02X})"));
                assert!(
                    seen.insert(mac),
                    "Duplicate macOS keycode 0x{mac:02X} for {name} (0x{vk:02X})"
                );
                count += 1;
            }
        }
        assert!(count >= 60);
    }
}
