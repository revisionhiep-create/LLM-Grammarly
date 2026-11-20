use std::env;
use std::fs::{self, File};
use std::path::PathBuf;

#[path = "src/icon_render.rs"]
mod icon_render;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=src/icon_render.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let ico_path = out_dir.join("icon.ico");

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    for &size in &[16u32, 24, 32, 48, 64, 256] {
        let rgba = icon_render::render_keyboard_rgba(size, false);
        let image = ico::IconImage::from_rgba_data(size, size, rgba);
        let entry = ico::IconDirEntry::encode(&image)?;
        icon_dir.add_entry(entry);
    }

    let file = File::create(&ico_path)?;
    icon_dir.write(file)?;

    // Also keep a copy in assets/icon.ico for reference/shortcuts when its contents change.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let assets_dir = manifest_dir.join("assets");
    let _ = fs::create_dir_all(&assets_dir);
    let assets_ico = assets_dir.join("icon.ico");
    let new_bytes = fs::read(&ico_path)?;
    let needs_write = fs::read(&assets_ico)
        .map(|existing| existing != new_bytes)
        .unwrap_or(true);
    if needs_write {
        let _ = fs::write(&assets_ico, &new_bytes);
    }

    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico_path.to_str().ok_or("Invalid icon path")?);
        res.compile()?;
    }

    Ok(())
}
