#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod downloader;
mod hotkey;
mod icon;
mod icon_render;
mod llm;
mod taskbar;

use anyhow::{bail, Result};
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

struct InstanceMutex(HANDLE);

impl Drop for InstanceMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn acquire_single_instance() -> Option<InstanceMutex> {
    unsafe {
        let handle =
            CreateMutexW(None, false, w!("Local\\KeyboardLLM_SingleInstance_Mutex")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            if let Ok(hwnd) = FindWindowW(None, w!("Keyboard LLM")) {
                if !hwnd.0.is_null() {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                }
            }
            return None;
        }
        Some(InstanceMutex(handle))
    }
}

fn parse_self_test_model_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--model-dir" {
            if let Some(dir) = args.next() {
                let trimmed = dir.trim();
                if !trimmed.is_empty() {
                    return PathBuf::from(trimmed);
                }
            }
        }
    }
    config::effective_model_dir(&config::load_config())
}

fn run_self_test() -> Result<()> {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }

    let url = config::normalize_model_url(config::DEFAULT_MODEL_URL)?;
    let model_dir = parse_self_test_model_dir();
    let model_path = config::local_model_path(&model_dir, &url);

    println!("=== Keyboard-LLM Self-Test ===");
    println!("Model URL : {url}");
    println!("Storage   : {}", model_dir.display());
    println!("Model file: {}", model_path.display());

    if !model_path.exists() {
        println!("Downloading default uncensored Qwen3.5-4B Q4_K_M model...");
        let dl_start = Instant::now();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut last_pct = 0u64;
        downloader::download_model(&url, &model_path, cancel, |p| {
            if let Some(tot) = p.total {
                let pct = (p.downloaded * 100) / tot.max(1);
                if pct >= last_pct + 10 || pct == 100 {
                    last_pct = pct;
                    println!(
                        "  Download: {pct}% ({:.2} / {:.2} GB, {:.1} MB/s)",
                        p.downloaded as f64 / (1024.0 * 1024.0 * 1024.0),
                        tot as f64 / (1024.0 * 1024.0 * 1024.0),
                        p.speed_bps / (1024.0 * 1024.0)
                    );
                }
            }
        })?;
        println!("Download finished in {:.2}s", dl_start.elapsed().as_secs_f64());
    } else {
        println!("Model already cached on disk.");
    }

    let mut llm = llm::LlmHandle::spawn();
    let load_start = Instant::now();
    let info = llm.load(model_path)?;
    println!(
        "Loaded {} on {} in {:.2}s (includes warmup)",
        info.model_name,
        info.backend_desc,
        load_start.elapsed().as_secs_f64()
    );

    let cases = [
        (
            "Fast-typing typos",
            "i type waaay to fast sumtimes and my garmma gets messed up lol",
        ),
        (
            "Uncensored casual/profane",
            "this damn boss fight is kicking my ass wtf am i suppose to do",
        ),
        (
            "Multiline preservation",
            "hey team im running l8 today\nsory about that ill be there in 10 mins",
        ),
    ];

    for (name, input) in cases {
        let (output, ms) = llm.fix_text(input.to_string())?;
        println!("\n[{name}] ({ms} ms)");
        println!("  IN : {input:?}");
        println!("  OUT: {output:?}");
        if output.trim().is_empty() {
            bail!("Test case '{name}' returned empty output");
        }
        let lower = output.to_ascii_lowercase();
        if lower.contains("i cannot") || lower.contains("as an ai") || lower.contains("inappropriate")
        {
            bail!("Uncensored check failed for '{name}': got refusal {output:?}");
        }
        if name == "Multiline preservation" && !output.contains('\n') {
            bail!("Multiline preservation failed: newline lost in {output:?}");
        }
    }

    println!("\nUnloading model to verify VRAM release...");
    llm.unload();
    llm.shutdown();
    println!("Self-test PASSED.");
    Ok(())
}

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--self-test") {
        if let Err(err) = run_self_test() {
            eprintln!("Self-test FAILED: {err:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let Some(_instance_lock) = acquire_single_instance() else {
        return Ok(());
    };

    let startup_cfg = config::load_config();
    let initial_size = startup_cfg
        .window
        .map(|w| [w.width.max(435.0), w.height.max(455.0)])
        .unwrap_or([475.0, 485.0]);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Keyboard LLM")
            .with_inner_size(initial_size)
            .with_min_inner_size([435.0, 455.0])
            .with_icon(icon::app_viewport_icon(false)),
        ..Default::default()
    };

    eframe::run_native(
        "Keyboard LLM",
        options,
        Box::new(|cc| Ok(Box::new(app::KeyboardLlmApp::new(cc)))),
    )
}
