use eframe::egui;

use crate::icon_render;

pub fn app_viewport_icon(active: bool) -> egui::IconData {
    let size = 64u32;
    egui::IconData {
        rgba: icon_render::render_keyboard_rgba(size, active),
        width: size,
        height: size,
    }
}
