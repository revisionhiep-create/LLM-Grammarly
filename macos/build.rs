use std::env;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

#[path = "src/icon_render.rs"]
mod icon_render;

fn write_png(path: &Path, size: u32) -> Result<(), Box<dyn std::error::Error>> {
    let rgba = icon_render::render_keyboard_rgba(size, false);
    let file = File::create(path)?;
    let w = BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgba)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=src/icon_render.rs");
    println!("cargo:rerun-if-changed=src/macos_bridge.m");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=MACOS_ICONSET_DIR");

    cc::Build::new()
        .file("src/macos_bridge.m")
        .flag("-fobjc-arc")
        .compile("macos_bridge");

    println!("cargo:rustc-link-lib=framework=Cocoa");
    println!("cargo:rustc-link-lib=framework=Carbon");
    println!("cargo:rustc-link-lib=framework=ApplicationServices");
    println!("cargo:rustc-link-lib=framework=UniformTypeIdentifiers");
    // Apple Clang emits calls to ___isPlatformVersionAtLeast for @available(...) checks
    // (used by ggml-metal-device.m on arm64 for macOS 15 residency sets when deploying
    // to macOS 12+). rustc invokes cc with -nodefaultlibs, which omits libclang_rt.osx.a
    // unless -fapple-link-rtlib is passed explicitly.
    println!("cargo:rustc-link-arg=-fapple-link-rtlib");

    if let Ok(iconset_dir_str) = env::var("MACOS_ICONSET_DIR") {
        let iconset_dir = PathBuf::from(iconset_dir_str);
        fs::create_dir_all(&iconset_dir)?;
        let specs: [(&str, u32); 10] = [
            ("icon_16x16.png", 16),
            ("icon_16x16@2x.png", 32),
            ("icon_32x32.png", 32),
            ("icon_32x32@2x.png", 64),
            ("icon_128x128.png", 128),
            ("icon_128x128@2x.png", 256),
            ("icon_256x256.png", 256),
            ("icon_256x256@2x.png", 512),
            ("icon_512x512.png", 512),
            ("icon_512x512@2x.png", 1024),
        ];
        for (name, px) in specs {
            write_png(&iconset_dir.join(name), px)?;
        }
    }

    Ok(())
}
