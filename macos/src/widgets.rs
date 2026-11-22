use eframe::egui;

#[derive(Clone, Copy)]
pub struct ThemeColors {
    pub bg_window: egui::Color32,
    pub bg_card: egui::Color32,
    pub bg_input: egui::Color32,
    pub bg_callout: egui::Color32,
    pub bg_callout_hover: egui::Color32,
    pub bg_callout_active: egui::Color32,
    pub bg_keycap: egui::Color32,
    pub bg_track: egui::Color32,
    pub border_subtle: egui::Color32,
    pub border_med: egui::Color32,
    pub text_primary: egui::Color32,
    pub text_muted: egui::Color32,
    pub accent_fill: egui::Color32,
    pub btn_sec_bg: egui::Color32,
    pub btn_sec_hover: egui::Color32,
    pub dot_ready: egui::Color32,
    pub dot_busy: egui::Color32,
    pub dot_off: egui::Color32,
    pub error_bg: egui::Color32,
    pub error_border: egui::Color32,
    pub error_text: egui::Color32,
    pub selection_bg: egui::Color32,
    pub power_border_off: egui::Color32,
}

impl ThemeColors {
    pub const fn light() -> Self {
        Self {
            bg_window: egui::Color32::from_rgb(244, 243, 241),         // #F4F3F1
            bg_card: egui::Color32::from_rgb(255, 255, 255),           // #FFFFFF
            bg_input: egui::Color32::from_rgb(250, 250, 249),          // #FAFAF9
            bg_callout: egui::Color32::from_rgb(248, 247, 245),        // #F8F7F5
            bg_callout_hover: egui::Color32::from_rgb(240, 238, 234),  // #F0EEEA
            bg_callout_active: egui::Color32::from_rgb(232, 229, 223), // #E8E5DF
            bg_keycap: egui::Color32::from_rgb(242, 240, 236),         // #F2F0EC
            bg_track: egui::Color32::from_rgb(231, 229, 226),          // #E7E5E2
            border_subtle: egui::Color32::from_rgb(229, 226, 220),     // #E5E2DC
            border_med: egui::Color32::from_rgb(214, 211, 205),        // #D6D3CD
            text_primary: egui::Color32::from_rgb(31, 29, 27),         // #1F1D1B
            text_muted: egui::Color32::from_rgb(115, 110, 104),        // #736E68
            accent_fill: egui::Color32::from_rgb(41, 38, 36),          // #292624
            btn_sec_bg: egui::Color32::from_rgb(236, 234, 230),        // #ECEAE6
            btn_sec_hover: egui::Color32::from_rgb(226, 223, 217),     // #E2DFD9
            dot_ready: egui::Color32::from_rgb(78, 128, 96),           // #4E8060
            dot_busy: egui::Color32::from_rgb(181, 137, 61),           // #B5893D
            dot_off: egui::Color32::from_rgb(160, 154, 146),           // #A09A92
            error_bg: egui::Color32::from_rgb(250, 241, 240),
            error_border: egui::Color32::from_rgb(224, 192, 188),
            error_text: egui::Color32::from_rgb(156, 60, 52),
            selection_bg: egui::Color32::from_rgb(214, 211, 205),
            power_border_off: egui::Color32::from_rgb(200, 196, 190), // #C8C4BE
        }
    }

    pub const fn dark() -> Self {
        Self {
            bg_window: egui::Color32::from_rgb(23, 22, 21),            // #171615
            bg_card: egui::Color32::from_rgb(33, 32, 30),              // #21201E
            bg_input: egui::Color32::from_rgb(27, 26, 24),             // #1B1A18
            bg_callout: egui::Color32::from_rgb(40, 38, 36),           // #282624
            bg_callout_hover: egui::Color32::from_rgb(49, 47, 44),     // #312F2C
            bg_callout_active: egui::Color32::from_rgb(57, 54, 50),    // #393632
            bg_keycap: egui::Color32::from_rgb(46, 44, 41),            // #2E2C29
            bg_track: egui::Color32::from_rgb(53, 50, 47),             // #35322F
            border_subtle: egui::Color32::from_rgb(51, 48, 44),        // #33302C
            border_med: egui::Color32::from_rgb(71, 67, 62),           // #47433E
            text_primary: egui::Color32::from_rgb(242, 240, 236),      // #F2F0EC
            text_muted: egui::Color32::from_rgb(161, 155, 147),        // #A19B93
            accent_fill: egui::Color32::from_rgb(230, 226, 220),       // #E6E2DC
            btn_sec_bg: egui::Color32::from_rgb(46, 44, 41),           // #2E2C29
            btn_sec_hover: egui::Color32::from_rgb(58, 55, 51),        // #3A3733
            dot_ready: egui::Color32::from_rgb(92, 166, 116),          // #5CA674
            dot_busy: egui::Color32::from_rgb(214, 165, 75),           // #D6A54B
            dot_off: egui::Color32::from_rgb(126, 120, 113),           // #7E7871
            error_bg: egui::Color32::from_rgb(51, 31, 30),             // #331F1E
            error_border: egui::Color32::from_rgb(110, 59, 54),        // #6E3B36
            error_text: egui::Color32::from_rgb(240, 138, 130),        // #F08A82
            selection_bg: egui::Color32::from_rgb(74, 70, 64),         // #4A4640
            power_border_off: egui::Color32::from_rgb(163, 158, 150),  // #A39E96
        }
    }

    pub fn for_mode(dark_mode: bool) -> Self {
        if dark_mode {
            Self::dark()
        } else {
            Self::light()
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if (bytes as f64) >= GB * 0.95 {
        format!("{:.2} GB", (bytes as f64) / GB)
    } else {
        format!("{:.1} MB", (bytes as f64) / MB)
    }
}

pub fn format_speed(bps: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bps <= 1.0 {
        "— MB/s".to_string()
    } else {
        format!("{:.1} MB/s", bps / MB)
    }
}

pub fn format_eta(downloaded: u64, total: Option<u64>, speed_bps: f64) -> Option<String> {
    let total = total?;
    if total <= downloaded || speed_bps <= 1024.0 {
        return None;
    }
    let secs = ((total - downloaded) as f64 / speed_bps).ceil() as u64;
    if secs < 60 {
        Some(format!("~{secs}s left"))
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        Some(format!("~{mins}m {rem:02}s left"))
    }
}

pub fn shorten_middle(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars || max_chars < 12 {
        return text.to_string();
    }
    let keep = (max_chars - 3) / 2;
    let start: String = text.chars().take(keep).collect();
    let end: String = text.chars().skip(count - keep).collect();
    format!("{start}...{end}")
}

pub fn draw_neutral_progress_bar(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    frac: f32,
    indeterminate: bool,
) {
    let width = ui.available_width();
    let height = 10.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let radius = egui::CornerRadius::same(5);
    ui.painter().rect_filled(rect, radius, theme.bg_track);

    if indeterminate {
        ui.ctx().request_repaint();
        let t = ui.input(|i| i.time) as f32;
        let seg_w = (width * 0.28).max(36.0);
        let travel = (width - seg_w).max(1.0);
        let pos = ((t * 0.9).sin() * 0.5 + 0.5) * travel;
        let fill_rect = egui::Rect::from_min_size(
            egui::pos2(rect.min.x + pos, rect.min.y),
            egui::vec2(seg_w, height),
        );
        ui.painter()
            .rect_filled(fill_rect, radius, theme.accent_fill);
    } else if frac > 0.001 {
        let fill_w = (width * frac.clamp(0.0, 1.0)).max(height);
        let fill_rect =
            egui::Rect::from_min_size(rect.min, egui::vec2(fill_w.min(width), height));
        ui.painter()
            .rect_filled(fill_rect, radius, theme.accent_fill);
    }
}

pub fn card_frame(theme: &ThemeColors) -> egui::Frame {
    egui::Frame::new()
        .fill(theme.bg_card)
        .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 {
        ((x as f32) + ((y as f32) - (x as f32)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    egui::Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

fn draw_sun_icon(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: egui::Color32) {
    painter.circle_filled(center, radius * 0.43, color);
    let inner_r = radius * 0.67;
    let outer_r = radius;
    let stroke = egui::Stroke::new(1.3_f32, color);
    for i in 0..8 {
        let angle = (i as f32) * std::f32::consts::FRAC_PI_4;
        let (s, c) = angle.sin_cos();
        let p0 = egui::pos2(center.x + c * inner_r, center.y + s * inner_r);
        let p1 = egui::pos2(center.x + c * outer_r, center.y + s * outer_r);
        painter.line_segment([p0, p1], stroke);
    }
}

fn draw_moon_icon(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    color: egui::Color32,
    cutout_bg: egui::Color32,
) {
    painter.circle_filled(center, radius * 0.88, color);
    let cutout_center = egui::pos2(center.x + radius * 0.34, center.y - radius * 0.22);
    painter.circle_filled(cutout_center, radius * 0.78, cutout_bg);
}

pub fn theme_switch(ui: &mut egui::Ui, is_dark: bool, theme: &ThemeColors) -> egui::Response {
    let desired_size = egui::vec2(52.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let tooltip = if is_dark {
        "Switch to Light Mode"
    } else {
        "Switch to Dark Mode"
    };
    let response = response.on_hover_text(tooltip);

    let t = ui.ctx().animate_bool_with_time(response.id, is_dark, 0.16);

    let track_light = egui::Color32::WHITE;
    let track_dark = egui::Color32::from_rgb(44, 42, 39); // #2C2A27
    let border_light = egui::Color32::from_rgb(200, 196, 190); // #C8C4BE
    let border_dark = egui::Color32::from_rgb(78, 74, 69); // #4E4A45

    let track_fill = lerp_color(track_light, track_dark, t);
    let mut track_border = lerp_color(border_light, border_dark, t);
    if response.hovered() {
        track_border = lerp_color(track_border, theme.text_muted, 0.35);
    }

    let radius = egui::CornerRadius::same(13);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, track_fill);
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.2_f32, track_border),
        egui::StrokeKind::Inside,
    );

    let cy = rect.center().y;
    let left_slot = egui::pos2(rect.left() + 13.0, cy);
    let right_slot = egui::pos2(rect.right() - 13.0, cy);

    if t > 0.05 {
        let sun_muted = lerp_color(track_fill, egui::Color32::from_rgb(140, 134, 126), t);
        draw_sun_icon(painter, left_slot, 5.4, sun_muted);
    }
    if t < 0.95 {
        let moon_muted = lerp_color(track_fill, egui::Color32::from_rgb(158, 152, 144), 1.0 - t);
        draw_moon_icon(painter, right_slot, 5.2, moon_muted, track_fill);
    }

    let knob_x = egui::lerp(left_slot.x..=right_slot.x, t);
    let knob_center = egui::pos2(knob_x, cy);
    painter.circle_filled(
        egui::pos2(knob_center.x, knob_center.y + 1.2),
        10.2,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 20),
    );
    painter.circle_filled(
        egui::pos2(knob_center.x, knob_center.y + 0.6),
        9.9,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 30),
    );
    let knob_fill = egui::Color32::WHITE;
    painter.circle_filled(knob_center, 9.8, knob_fill);
    painter.circle_stroke(
        knob_center,
        9.8,
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(214, 210, 203)),
    );

    let dark_icon = egui::Color32::from_rgb(34, 32, 30); // #22201E
    if t < 0.5 {
        let weight = (1.0 - t * 2.0).clamp(0.0, 1.0);
        let icon_col = lerp_color(knob_fill, dark_icon, weight);
        draw_sun_icon(painter, knob_center, 5.3, icon_col);
    } else {
        let weight = (t * 2.0 - 1.0).clamp(0.0, 1.0);
        let icon_col = lerp_color(knob_fill, dark_icon, weight);
        draw_moon_icon(painter, knob_center, 5.2, icon_col, knob_fill);
    }

    response
}

fn draw_pill_switch(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    rect: egui::Rect,
    id: egui::Id,
    is_on: bool,
    enabled: bool,
    hovered: bool,
) {
    let t = ui.ctx().animate_bool_with_time(id, is_on, 0.16);

    let track_off = egui::Color32::WHITE;
    let track_on = egui::Color32::from_rgb(77, 150, 98); // #4D9662
    let border_off = theme.power_border_off;
    let border_on = egui::Color32::from_rgb(67, 133, 86); // #438556

    let mut track_fill = lerp_color(track_off, track_on, t);
    let mut track_border = lerp_color(border_off, border_on, t);
    if !enabled {
        track_fill = lerp_color(track_fill, theme.bg_track, 0.45);
        track_border = lerp_color(track_border, theme.border_med, 0.45);
    } else if hovered {
        track_border = lerp_color(track_border, theme.text_muted, 0.25);
    }

    let radius = egui::CornerRadius::same(15);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, track_fill);
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.2_f32, track_border),
        egui::StrokeKind::Inside,
    );

    let cx = egui::lerp((rect.left() + 15.0)..=(rect.right() - 15.0), t);
    let cy = rect.center().y;
    painter.circle_filled(
        egui::pos2(cx, cy + 1.5),
        11.8,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 18),
    );
    painter.circle_filled(
        egui::pos2(cx, cy + 0.8),
        11.5,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 28),
    );
    let knob_fill = if enabled {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(245, 244, 242)
    };
    painter.circle_filled(egui::pos2(cx, cy), 11.3, knob_fill);
    painter.circle_stroke(
        egui::pos2(cx, cy),
        11.3,
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(220, 217, 212)),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyRowAction {
    None,
    ToggleRecord,
    Secondary,
}

#[allow(clippy::too_many_arguments)]
pub fn render_hotkey_row(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    badge_label: Option<String>,
    description: &str,
    is_recording: bool,
    is_busy: bool,
    secondary_label: Option<&str>,
    empty_action_label: &str,
) -> HotkeyRowAction {
    let mut action = HotkeyRowAction::None;
    ui.horizontal(|ui| {
        let has_binding = badge_label.is_some();
        let (fill, text_color, strong, text) = match badge_label {
            Some(lbl) => (theme.bg_keycap, theme.text_primary, true, lbl),
            None => (
                theme.bg_callout,
                theme.text_muted,
                false,
                "No hotkey set".to_string(),
            ),
        };
        egui::Frame::new()
            .fill(fill)
            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
            .corner_radius(egui::CornerRadius::same(5))
            .inner_margin(egui::Margin::symmetric(8, 2))
            .show(ui, |ui| {
                let mut rt = egui::RichText::new(text).size(11.5).color(text_color);
                if strong {
                    rt = rt.strong();
                }
                ui.label(rt);
            });
        ui.label(
            egui::RichText::new(description)
                .size(12.0)
                .color(theme.text_muted),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(sec_lbl) = secondary_label.filter(|_| !is_recording) {
                if ui
                    .add_enabled(
                        !is_busy,
                        egui::Button::new(
                            egui::RichText::new(sec_lbl)
                                .size(11.0)
                                .color(theme.text_muted),
                        )
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                        .corner_radius(egui::CornerRadius::same(5))
                        .min_size(egui::vec2(48.0, 22.0)),
                    )
                    .clicked()
                {
                    action = HotkeyRowAction::Secondary;
                }
            }

            let action_label = if is_recording {
                "Press keys... (Esc)"
            } else if has_binding {
                "Change Hotkey"
            } else {
                empty_action_label
            };
            if ui
                .add_enabled(
                    !is_busy,
                    egui::Button::new(
                        egui::RichText::new(action_label)
                            .size(11.0)
                            .color(theme.text_primary),
                    )
                    .fill(if is_recording {
                        theme.bg_keycap
                    } else {
                        theme.btn_sec_bg
                    })
                    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                    .corner_radius(egui::CornerRadius::same(5))
                    .min_size(egui::vec2(96.0, 22.0)),
                )
                .clicked()
            {
                action = HotkeyRowAction::ToggleRecord;
            }
        });
    });
    action
}

#[allow(clippy::too_many_arguments)]
fn switch_row_widget(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    pill_id: egui::Id,
    height: f32,
    font_size: f32,
    label: &str,
    is_on: bool,
    enabled: bool,
    show_spinner: bool,
) -> egui::Response {
    let width = ui.available_width();
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), sense);
    let response = if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    };

    let (box_fill, box_border) = if enabled && response.is_pointer_button_down_on() {
        (theme.bg_callout_active, theme.border_med)
    } else if enabled && response.hovered() {
        (theme.bg_callout_hover, theme.border_med)
    } else {
        (theme.bg_callout, theme.border_subtle)
    };

    let box_radius = egui::CornerRadius::same(8);
    ui.painter().rect_filled(rect, box_radius, box_fill);
    ui.painter().rect_stroke(
        rect,
        box_radius,
        egui::Stroke::new(1.0_f32, box_border),
        egui::StrokeKind::Inside,
    );

    let text_left = rect.left() + 14.0 + if show_spinner { 24.0 } else { 0.0 };
    let switch_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 12.0 - 27.0, rect.center().y),
        egui::vec2(54.0, 30.0),
    );

    if show_spinner {
        let spinner_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 22.0, rect.center().y),
            egui::vec2(16.0, 16.0),
        );
        ui.put(spinner_rect, egui::Spinner::new().size(15.0));
    }

    let text_color = if enabled {
        theme.text_primary
    } else {
        theme.text_muted
    };
    let avail_text_w = (switch_rect.left() - 10.0 - text_left).max(40.0);
    let galley = egui::WidgetText::from(
        egui::RichText::new(label)
            .size(font_size)
            .strong()
            .color(text_color),
    )
    .into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        avail_text_w,
        egui::TextStyle::Button,
    );
    let text_pos = egui::pos2(text_left, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(text_pos, galley, text_color);

    draw_pill_switch(
        ui,
        theme,
        switch_rect,
        pill_id,
        is_on,
        enabled,
        response.hovered(),
    );

    response
}

pub fn power_switch_row(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    label: &str,
    is_on: bool,
    enabled: bool,
    show_spinner: bool,
) -> egui::Response {
    switch_row_widget(
        ui,
        theme,
        egui::Id::new("power_toggle_switch"),
        46.0,
        14.0,
        label,
        is_on,
        enabled,
        show_spinner,
    )
}

pub fn toggle_switch_row(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    pill_id: egui::Id,
    label: &str,
    is_on: bool,
    enabled: bool,
) -> egui::Response {
    switch_row_widget(ui, theme, pill_id, 42.0, 13.0, label, is_on, enabled, false)
}

pub fn chip_button(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    label: &str,
    selected: bool,
    enabled: bool,
    min_size: egui::Vec2,
    font_size: f32,
) -> egui::Response {
    let text = if selected {
        egui::RichText::new(label)
            .size(font_size)
            .strong()
            .color(theme.text_primary)
    } else {
        egui::RichText::new(label)
            .size(font_size)
            .color(theme.text_muted)
    };
    let (fill, stroke_color) = if selected {
        (theme.btn_sec_bg, theme.text_muted)
    } else {
        (egui::Color32::TRANSPARENT, theme.border_subtle)
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(text)
            .fill(fill)
            .stroke(egui::Stroke::new(1.0_f32, stroke_color))
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(min_size),
    )
}

pub fn secondary_button(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    text: &str,
    height: f32,
) -> egui::Response {
    let w = ui.available_width();
    let btn = egui::Button::new(
        egui::RichText::new(text)
            .size(14.0)
            .strong()
            .color(theme.text_primary),
    )
    .fill(theme.btn_sec_bg)
    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
    .corner_radius(egui::CornerRadius::same(8))
    .min_size(egui::vec2(w, height));
    ui.scope(|ui| {
        ui.visuals_mut().widgets.hovered.bg_fill = theme.btn_sec_hover;
        ui.visuals_mut().widgets.active.bg_fill = theme.btn_sec_bg;
        ui.add(btn)
    })
    .inner
}

pub fn apply_theme_visuals(ctx: &egui::Context, dark_mode: bool) {
    let theme = ThemeColors::for_mode(dark_mode);
    let mut v = if dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.window_fill = theme.bg_window;
    v.panel_fill = theme.bg_window;
    v.extreme_bg_color = theme.bg_input;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, theme.text_primary);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, theme.border_subtle);
    v.widgets.inactive.bg_fill = theme.bg_input;
    v.widgets.inactive.weak_bg_fill = theme.bg_input;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, theme.text_primary);
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, theme.border_med);
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, theme.text_primary);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, theme.accent_fill);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, theme.text_primary);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.2_f32, theme.accent_fill);
    v.selection.bg_fill = theme.selection_bg;
    v.selection.stroke = egui::Stroke::new(1.0_f32, theme.accent_fill);
    ctx.set_visuals(v);
}
