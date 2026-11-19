# LLM-Grammarly

A fast, local, private Windows grammar and spelling corrector written in Rust (`eframe`/`egui` + embedded `llama.cpp` with Vulkan GPU acceleration and automatic CPU fallback).

Press a system-wide hotkey in any Windows text field to fix typos, spelling, grammar, and punctuation locally on your machine — no cloud APIs, no Python runtime, and no external Ollama server required.

---

## Download & Launch on Windows (File Explorer)

No installation, Python, or external dependencies required — just download and run:

- **Latest Release Page**: [GitHub Releases](https://github.com/revisionhiep-create/LLM-Grammarly/releases/latest)
- **Direct Download**: [`LLM-Grammarly.exe`](https://github.com/revisionhiep-create/LLM-Grammarly/releases/latest/download/LLM-Grammarly.exe) (Windows 10/11 64-bit; Vulkan GPU recommended, automatic CPU fallback built in)

### Launching via File Explorer

1. **Download `LLM-Grammarly.exe`** from the link above.
2. **Open File Explorer** (press **`Win + E`** or click the folder icon on your taskbar) and go to your **Downloads** folder.
3. **Move `LLM-Grammarly.exe` to its own folder (recommended)** — for example, create `C:\Users\<YourName>\LLM-Grammarly` and drag `LLM-Grammarly.exe` into it.
   > **Why?** By default, the app downloads its ~2.52 GB `.gguf` AI model into the same folder as `LLM-Grammarly.exe`, so keeping it in a dedicated folder keeps your Downloads folder tidy.
4. **Double-click `LLM-Grammarly.exe`** to open the app.
   - **If Windows Defender SmartScreen appears** (*"Windows protected your PC"* because the open-source binary is unsigned): click **More info** → **Run anyway**.
   - Alternatively, right-click `LLM-Grammarly.exe` → **Properties** → check **Unblock** → click **OK**, then double-click to launch.
5. **Download the AI model on first launch**:
   - Click **Download & Turn On** in the app window (or click **Browse...** first if you want to store models in a different folder).
   - Once the download finishes and loads, the top toggle switch slides right and turns **green** (**Ready**).
6. **Start fixing text anywhere**:
   - Click into any text box (browser, Discord/Slack, Notepad, email, IDE, etc.) and press **`Ctrl + Q`** (default hotkey) to fix spelling, grammar, and punctuation in place.
   - *(Optional)* Right-click `LLM-Grammarly.exe` in File Explorer and choose **Pin to Start**, **Show more options → Pin to taskbar**, or **Create shortcut** for quick access later.

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

## Everyday Usage & Customization

1. **Turn On / Turn Off**:
   - Click anywhere inside the top **Turn On / Turn Off** toggle box (or use the system tray icon menu / Windows taskbar hover buttons) to load or unload the model from GPU VRAM and enable or disable the global hotkey.
2. **Fix Text Anywhere**:
   - Place your cursor in any text input and press your hotkey (default **`Ctrl + Q`**) to select and fix the text in place.
3. **Pause, Resume, or Switch Models**:
   - You can click **Pause / Cancel Download** mid-download (keeping the `.gguf.part` file to resume later via **Resume Download** / **Resume Download & Turn On**) or click **Discard partial file**.
4. **Customize Hotkey, Model, or Storage Folder**:
   - **Hotkey**: Click **Change Hotkey**, press your desired shortcut (or `F1`–`F12`), or click **Reset** to restore `Ctrl + Q`.
   - **Model URL**: Paste any Hugging Face `.gguf` URL (`/blob/` or `/resolve/`), then click **Download Model** (or **Reset Default** to return to the default model).
   - **Model Folder**: Click **Browse...** to choose another storage folder or **Reset** to use the `.exe` folder again.
   - **Tray Behavior**: Enable **Minimize to tray on close** or click **Hide to Tray** to keep the app running quietly in the background.
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

> **Note**: Large binaries (`*.gguf`, `*.exe`) are intentionally excluded from git history via `.gitignore`. Prebuilt `.exe` binaries are published on [GitHub Releases](https://github.com/revisionhiep-create/LLM-Grammarly/releases/latest).

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
