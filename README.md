# LLM-Grammarly

A fast, local, private Windows grammar and spelling corrector written in Rust (`eframe`/`egui` + embedded `llama.cpp` with Vulkan GPU acceleration and automatic CPU fallback).

Press a system-wide hotkey in any Windows text field to fix typos, spelling, grammar, and punctuation locally on your machine — no cloud APIs, no Python runtime, and no external Ollama server required.

---

## Features

- **Single Self-Contained Windows Executable**: Compiles into one portable `.exe` (`LLM-Grammarly.exe`) with `llama.cpp` compiled directly in, linking only standard Windows OS / MSVC runtime libraries and `vulkan-1.dll`.
- **Default Uncensored Local Model**: Ships configured for `HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive` (`Qwen3.5-4B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf`, ~2.52 GB) running in fast non-thinking mode (`<think>\n\n</think>\n\n`), preserving original meaning, tone, names, slang, profanity, and line breaks. Also supports custom GGUF models via embedded chat templates or ChatML fallback.
- **Customizable System-Wide Hotkey**:
  - Defaults to **`Ctrl + Q`**; rebind live via **Change Hotkey** / **Reset** to any modifier combo (`Ctrl` / `Alt` / `Shift` / `Win` + key) or standalone function key (`F1`–`F12`).
  - Waits for physical key/modifier release before simulating `Ctrl+A` → `Ctrl+C`, runs local LLM correction, pastes the result with `Ctrl+V` only if changed, polls `GetClipboardSequenceNumber` adaptively, and restores your previous clipboard text afterward.
- **Full-Box Clickable On / Off Switch with VRAM Unloading**:
  - Clean neutral UI featuring an animated white (Off) / green (On) toggle pill where clicking anywhere inside the switch row toggles the assistant.
  - Turning **Off** unregisters the global hotkey and unloads the model + KV cache to free GPU VRAM immediately. Turning **On** loads the model, warms up Vulkan GPU shaders, and activates the global hotkey.
- **Interactive Hugging Face GGUF Downloader**:
  - First-run and model-switching download panel showing progress percentage, transferred bytes, download speed (MB/s), and ETA.
  - Supports **Pause / Cancel Download** and **Resume** via `.gguf.part` partial files and HTTP `Range` requests, validates the `GGUF` magic header (`b"GGUF"`), automatically converts Hugging Face `/blob/` links to `/resolve/` URLs, and mirrors progress onto the Windows Taskbar button.
- **Configurable Model URL & Storage Folder**:
  - Edit the Hugging Face GGUF download link directly (**Reset Default** available) and choose any **Model Storage Folder** via a native Windows folder picker (**Browse...** / **Reset**). Defaults to the folder containing the running `.exe`.
- **Native Windows Desktop Integration**:
  - Procedurally rendered neutral keyboard icon embedded into PE executable resources (`assets/icon.ico`), window chrome, system tray, and taskbar.
  - **Windows Taskbar Hover Toolbar** (`ITaskbarList3`) with **Turn On / Turn Off** and **Quit** thumbnail buttons.
  - **System Tray Icon** with **Show Window**, **Turn On / Turn Off**, and **Quit**, plus optional **Minimize to tray on close** and **Hide to Tray**.
  - **Single-Instance Guard** that brings the existing window to focus if launched a second time.
- **Multi-Monitor & Per-Monitor DPI Aware**:
  - Persists window size, virtual-screen coordinates, monitor placement, and maximized state in `%APPDATA%\keyboard-llm\config\settings.json`, clamps safely onto visible monitors if a display is disconnected, and scales cleanly across displays with different resolutions/DPI settings.

---

## Usage

1. **First Launch**:
   - Place `LLM-Grammarly.exe` in your preferred folder and launch it.
   - If no `.gguf` file is found yet in the active model storage folder (the `.exe` folder by default), click **Download & Turn On** (or **Download Model**) to stream the default uncensored Qwen3.5-4B `Q4_K_M` model from Hugging Face.
   - You can pause/cancel mid-download (keeping the `.gguf.part` file to resume later) or discard the partial file at any time.
2. **Turn On / Turn Off**:
   - Click anywhere in the top **Turn On / Turn Off** toggle box (or use the system tray menu / taskbar thumbnail button) to load or unload the model from VRAM and enable/disable the hotkey.
3. **Fix Text Anywhere**:
   - Place your cursor in any text input (browser, chat app, notepad, IDE, etc.) and press your hotkey (default **`Ctrl + Q`**) to correct spelling, grammar, and punctuation in place.
4. **Customize Hotkey, Model, or Storage Folder**:
   - **Hotkey**: Click **Change Hotkey**, press your desired shortcut (or `F1`–`F12`), or click **Reset** to restore `Ctrl + Q`.
   - **Model URL**: Paste any Hugging Face `.gguf` URL (`/blob/` or `/resolve/`), then click **Download Model** / **Reset Default**.
   - **Model Folder**: Click **Browse...** to choose another storage folder or **Reset** to use the `.exe` folder again.
   - **Settings File**: Saved automatically at `%APPDATA%\keyboard-llm\config\settings.json`.

---

## CLI Self-Test

You can verify model caching/downloading, Vulkan GPU offloading, shader warmup, inference latency, uncensored output, multiline preservation, and clean VRAM unloading from a terminal:

```powershell
.\LLM-Grammarly.exe --self-test
```

To test against a custom model directory:

```powershell
.\LLM-Grammarly.exe --self-test --model-dir "D:\Models"
```

---

## Project Structure

```text
LLM-Grammarly/
├── .gitignore            # Excludes target/, *.gguf, *.gguf.part, *.exe, *.pdb, logs
├── Cargo.toml            # Crate manifest, dependencies, winresource metadata, release profile
├── Cargo.lock            # Locked Rust dependency graph
├── build.rs              # Generates assets/icon.ico & embeds Windows PE icon resource
├── LICENSE               # MIT License
├── README.md
├── assets/
│   └── icon.ico          # Multi-resolution neutral keyboard icon (16..256 px)
└── src/
    ├── main.rs           # Single-instance mutex, --self-test runner, eframe entrypoint
    ├── app.rs            # egui desktop UI, toggle switch, downloader UI, settings & state loop
    ├── config.rs         # settings.json persistence, HF URL normalization, hotkey/window config
    ├── downloader.rs     # Streaming GGUF downloader with HTTP Range resume & magic validation
    ├── hotkey.rs         # Win32 RegisterHotKey loop, SendInput copy/paste, clipboard restore
    ├── llm.rs            # Embedded llama-cpp-2 worker (Vulkan GPU/CPU fallback, warmup, VRAM unload)
    ├── taskbar.rs        # Win32 ITaskbarList3 progress/thumbbar, tray menu, folder picker, monitor clamping
    ├── icon.rs           # Converts rendered RGBA icon buffers to egui/tray/Win32 HICON formats
    └── icon_render.rs    # Pure-Rust anti-aliased procedural icon renderer shared with build.rs
```

> **Note**: Large binaries (`*.gguf`, `*.exe`) are intentionally excluded via `.gitignore`. Prebuilt `.exe` binaries can be distributed through GitHub Releases.

---

## Building from Source (Windows)

### Prerequisites

- **OS**: Windows 10 or Windows 11 (x64)
- **Rust Toolchain**: `stable-x86_64-pc-windows-msvc` via [rustup](https://rustup.rs/)
- **Visual Studio 2022 Build Tools**: `Desktop development with C++` workload (provides MSVC `cl.exe`, Windows SDK, CMake, and Ninja)
- **LLVM**: Install LLVM for Windows so `bindgen` can locate `libclang.dll` (typically `C:\Program Files\LLVM\bin`)
- **Vulkan SDK**: Install the LunarG [Vulkan SDK](https://vulkan.lunarg.com/) for Windows (`glslc.exe`, headers, and `vulkan-1.lib`)

### Build Commands

When compiling `llama-cpp-2` with the `vulkan` feature on Windows, use **Ninja** from a **VS 2022 x64 Developer environment** and a short `CARGO_TARGET_DIR` path. (Using Ninja avoids CMake 4.x / MSBuild parallel `CustomBuild` ordering issues when building `ggml-vulkan`'s `vulkan-shaders-gen` tool.)

From **cmd.exe** (adjust `VULKAN_SDK` / `LIBCLANG_PATH` paths to match your machine):

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat" -arch=amd64 -host_arch=amd64
set "CMAKE_GENERATOR=Ninja"
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
set "VULKAN_SDK=%LOCALAPPDATA%\VulkanSDK\1.4.363.0"
set "PATH=%VULKAN_SDK%\Bin;%PATH%"
set "CARGO_TARGET_DIR=C:\kbllm_target"

cargo test
cargo build --release
```

Cargo builds the release binary at `C:\kbllm_target\release\keyboard-llm.exe` (matching the package name in `Cargo.toml`), which you can copy and rename to `LLM-Grammarly.exe`.

---

## License

This project is licensed under the [MIT License](LICENSE).
