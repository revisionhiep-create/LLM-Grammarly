use eframe::egui;
use std::mem::size_of;
use std::ptr::null_mut;
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HDC,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

use crate::icon_render;

pub fn app_viewport_icon(active: bool) -> egui::IconData {
    let size = 64u32;
    egui::IconData {
        rgba: icon_render::render_keyboard_rgba(size, active),
        width: size,
        height: size,
    }
}

pub fn tray_hicon(active: bool) -> windows::core::Result<HICON> {
    let size = 32u32;
    let rgba = icon_render::render_keyboard_rgba(size, active);
    rgba_to_hicon(&rgba, size, size)
}

pub fn notification_hicon(active: bool) -> windows::core::Result<HICON> {
    let size = 64u32;
    let rgba = icon_render::render_keyboard_rgba(size, active);
    rgba_to_hicon(&rgba, size, size)
}

pub fn thumb_power_hicon(is_on: bool) -> windows::core::Result<HICON> {
    let size = 24u32;
    let rgba = icon_render::render_thumb_power_rgba(size, is_on);
    rgba_to_hicon(&rgba, size, size)
}

pub fn thumb_quit_hicon() -> windows::core::Result<HICON> {
    let size = 24u32;
    let rgba = icon_render::render_thumb_quit_rgba(size);
    rgba_to_hicon(&rgba, size, size)
}

struct ScreenDcGuard(HDC);

impl Drop for ScreenDcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = ReleaseDC(None, self.0);
            }
        }
    }
}

/// Converts a top-down RGBA8 pixel buffer into a Win32 HICON with per-pixel alpha.
pub fn rgba_to_hicon(rgba: &[u8], w: u32, h: u32) -> windows::core::Result<HICON> {
    if w == 0 || h == 0 {
        return Err(E_INVALIDARG.into());
    }
    let w_i32 = i32::try_from(w).map_err(|_| windows::core::Error::from(E_INVALIDARG))?;
    let h_i32 = i32::try_from(h).map_err(|_| windows::core::Error::from(E_INVALIDARG))?;
    let expected_bytes = (w as usize)
        .checked_mul(h as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
    if rgba.len() != expected_bytes {
        return Err(E_INVALIDARG.into());
    }
    let stride = (w as usize)
        .div_ceil(16)
        .checked_mul(2)
        .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
    let mask_len = stride
        .checked_mul(h as usize)
        .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;

    unsafe {
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w_i32;
        bmi.bmiHeader.biHeight = -h_i32; // Negative height => top-down DIB
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let dc_guard = ScreenDcGuard(GetDC(None));
        let mut bits_ptr: *mut core::ffi::c_void = null_mut();
        let hbm_color =
            CreateDIBSection(dc_guard.0, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)?;
        drop(dc_guard);

        if hbm_color.is_invalid() || bits_ptr.is_null() {
            if !hbm_color.is_invalid() {
                let _ = DeleteObject(hbm_color);
            }
            return Err(windows::core::Error::from_win32());
        }

        // Copy RGBA -> BGRA into DIB memory.
        let dst = std::slice::from_raw_parts_mut(bits_ptr as *mut u8, expected_bytes);
        for (src_px, dst_px) in rgba.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            dst_px[0] = src_px[2]; // B
            dst_px[1] = src_px[1]; // G
            dst_px[2] = src_px[0]; // R
            dst_px[3] = src_px[3]; // A
        }

        // Monochrome 1-bpp mask bitmap (all zeroes; Windows uses the 32-bit alpha channel).
        let mask_bits = vec![0u8; mask_len];
        let hbm_mask = CreateBitmap(w_i32, h_i32, 1, 1, Some(mask_bits.as_ptr() as _));
        if hbm_mask.is_invalid() {
            let _ = DeleteObject(hbm_color);
            return Err(windows::core::Error::from_win32());
        }

        let icon_info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };

        let hicon = CreateIconIndirect(&icon_info);
        let _ = DeleteObject(hbm_color);
        let _ = DeleteObject(hbm_mask);
        hicon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_to_hicon_rejects_invalid_dimensions_and_buffer_lengths() {
        assert!(rgba_to_hicon(&[], 0, 16).is_err());
        assert!(rgba_to_hicon(&[], 16, 0).is_err());
        assert!(rgba_to_hicon(&[0u8; 4], 2, 2).is_err());
        assert!(rgba_to_hicon(&[0u8; 64], u32::MAX, 2).is_err());
    }
}
