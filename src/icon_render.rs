#![allow(dead_code)]

/// Pure-Rust procedural icon renderer shared between build.rs (PE .ico) and runtime UI/tray/taskbar.
/// Produces tightly anti-aliased RGBA8 buffers in neutral stone/charcoal tones.

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn mix_u8(a: u8, b: u8, t: f32) -> u8 {
    let t = clamp01(t);
    ((a as f32) * (1.0 - t) + (b as f32) * t).round().clamp(0.0, 255.0) as u8
}

/// Premultiplied-alpha-aware "over" blend onto an RGBA pixel.
fn blend_over(dst: &mut [u8], r: u8, g: u8, b: u8, alpha: f32) {
    let a = clamp01(alpha);
    if a <= 0.001 {
        return;
    }
    let da = (dst[3] as f32) / 255.0;
    let out_a = a + da * (1.0 - a);
    if out_a <= 0.0001 {
        dst[0] = 0;
        dst[1] = 0;
        dst[2] = 0;
        dst[3] = 0;
        return;
    }
    let sr = r as f32;
    let sg = g as f32;
    let sb = b as f32;
    let dr = dst[0] as f32;
    let dg = dst[1] as f32;
    let db = dst[2] as f32;

    dst[0] = ((sr * a + dr * da * (1.0 - a)) / out_a).round().clamp(0.0, 255.0) as u8;
    dst[1] = ((sg * a + dg * da * (1.0 - a)) / out_a).round().clamp(0.0, 255.0) as u8;
    dst[2] = ((sb * a + db * da * (1.0 - a)) / out_a).round().clamp(0.0, 255.0) as u8;
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

/// Signed distance to an axis-aligned rounded rectangle defined by [min_x, max_x] x [min_y, max_y].
fn sd_rounded_rect(px: f32, py: f32, min_x: f32, min_y: f32, max_x: f32, max_y: f32, radius: f32) -> f32 {
    let cx = (min_x + max_x) * 0.5;
    let cy = (min_y + max_y) * 0.5;
    let hx = ((max_x - min_x) * 0.5 - radius).max(0.0);
    let hy = ((max_y - min_y) * 0.5 - radius).max(0.0);
    let qx = (px - cx).abs() - hx;
    let qy = (py - cy).abs() - hy;
    let out_dist = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let in_dist = qx.max(qy).min(0.0);
    out_dist + in_dist - radius
}

/// Signed distance to a line segment from (ax, ay) to (bx, by).
fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let pax = px - ax;
    let pay = py - ay;
    let bax = bx - ax;
    let bay = by - ay;
    let denom = (bax * bax + bay * bay).max(1e-6);
    let h = clamp01((pax * bax + pay * bay) / denom);
    let dx = pax - bax * h;
    let dy = pay - bay * h;
    (dx * dx + dy * dy).sqrt()
}

/// Convert signed distance (in normalized 0..1 units) to anti-aliased coverage [0, 1].
fn cov(dist: f32, aa: f32) -> f32 {
    clamp01(0.5 - dist / aa)
}

/// Render the neutral minimalist keyboard application icon as RGBA8 (size x size).
pub fn render_keyboard_rgba(size: u32, active: bool) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let aa = 1.25 / (size as f32);

    // Keycap layout: (min_x, min_y, max_x, max_y)
    let mut keys: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(14);

    // Row 1: 6 keys
    let gap = 0.026;
    let left = 0.13;
    let right = 0.87;
    let r1_w = (right - left - 5.0 * gap) / 6.0;
    for i in 0..6 {
        let x0 = left + (i as f32) * (r1_w + gap);
        keys.push((x0, 0.255, x0 + r1_w, 0.370));
    }

    // Row 2: 5 keys (wide end caps + 3 middle keys)
    let end_w = 0.145;
    let mid_w = (right - left - 2.0 * end_w - 4.0 * gap) / 3.0;
    let mut x = left;
    keys.push((x, 0.422, x + end_w, 0.537));
    x += end_w + gap;
    for _ in 0..3 {
        keys.push((x, 0.422, x + mid_w, 0.537));
        x += mid_w + gap;
    }
    keys.push((x, 0.422, right, 0.537));

    // Row 3: left modifier, spacebar, right modifier
    let mod_w = 0.145;
    keys.push((left, 0.590, left + mod_w, 0.708));
    keys.push((left + mod_w + gap, 0.590, right - mod_w - gap, 0.708));
    keys.push((right - mod_w, 0.590, right, 0.708));

    let key_radius = 0.026;
    let border_w = (1.1 / size as f32).max(0.018);

    for py_i in 0..size {
        let py = (py_i as f32 + 0.5) / (size as f32);
        for px_i in 0..size {
            let px = (px_i as f32 + 0.5) / (size as f32);
            let idx = ((py_i * size + px_i) * 4) as usize;
            let pixel = &mut buf[idx..idx + 4];

            // Subtle drop shadow underneath keyboard body
            let shadow_d = sd_rounded_rect(px, py - 0.02, 0.055, 0.165, 0.945, 0.835, 0.11);
            let shadow_a = cov(shadow_d, aa * 1.8) * 0.22;
            blend_over(pixel, 15, 14, 13, shadow_a);

            // Outer keyboard chassis
            let body_d = sd_rounded_rect(px, py, 0.055, 0.165, 0.945, 0.815, 0.105);
            let body_a = cov(body_d, aa);
            if body_a > 0.0 {
                let t_y = clamp01((py - 0.165) / 0.65);
                let mut r = mix_u8(54, 36, t_y);
                let mut g = mix_u8(50, 33, t_y);
                let mut b = mix_u8(47, 31, t_y);

                // Subtle rim highlight along chassis edge
                let rim = cov(body_d.abs() - border_w * 0.5, aa);
                r = mix_u8(r, 92, rim * 0.65);
                g = mix_u8(g, 87, rim * 0.65);
                b = mix_u8(b, 82, rim * 0.65);

                blend_over(pixel, r, g, b, body_a);
            }

            // Keycaps + key base bevel
            for &(x0, y0, x1, y1) in &keys {
                let base_d = sd_rounded_rect(px, py, x0, y0 + 0.012, x1, y1 + 0.016, key_radius);
                let base_a = cov(base_d, aa);
                if base_a > 0.0 {
                    blend_over(pixel, 156, 150, 144, base_a);
                }

                let cap_d = sd_rounded_rect(px, py, x0, y0, x1, y1, key_radius);
                let cap_a = cov(cap_d, aa);
                if cap_a > 0.0 {
                    let kt = clamp01((py - y0) / (y1 - y0));
                    let kr = mix_u8(243, 227, kt);
                    let kg = mix_u8(241, 224, kt);
                    let kb = mix_u8(238, 220, kt);
                    blend_over(pixel, kr, kg, kb, cap_a);
                }
            }

            // Status LED dot in top-right bezel
            let led_cx = 0.858;
            let led_cy = 0.210;
            let led_r = if active { 0.030 } else { 0.024 };
            let dist = ((px - led_cx).powi(2) + (py - led_cy).powi(2)).sqrt();
            if active {
                let halo_a = clamp01(1.0 - dist / (led_r * 2.4)) * 0.35;
                blend_over(pixel, 94, 158, 114, halo_a);
            }
            let led_a = cov(dist - led_r, aa);
            if led_a > 0.0 {
                if active {
                    blend_over(pixel, 98, 170, 120, led_a);
                } else {
                    blend_over(pixel, 125, 119, 113, led_a);
                }
            }
        }
    }

    buf
}

/// Render a 24x24 (or arbitrary size) power button icon for the Windows taskbar thumbnail toolbar.
pub fn render_thumb_power_rgba(size: u32, is_on: bool) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let aa = 1.25 / (size as f32);

    let (bg_r, bg_g, bg_b, rim_r, rim_g, rim_b, fg_r, fg_g, fg_b) = if is_on {
        (78u8, 128u8, 96u8, 108u8, 158u8, 126u8, 255u8, 255u8, 255u8)
    } else {
        (56u8, 53u8, 50u8, 96u8, 90u8, 84u8, 240u8, 238u8, 235u8)
    };

    for py_i in 0..size {
        let py = (py_i as f32 + 0.5) / (size as f32);
        for px_i in 0..size {
            let px = (px_i as f32 + 0.5) / (size as f32);
            let idx = ((py_i * size + px_i) * 4) as usize;
            let pixel = &mut buf[idx..idx + 4];

            // Circular badge
            let badge_d = ((px - 0.5).powi(2) + (py - 0.5).powi(2)).sqrt() - 0.44;
            let badge_a = cov(badge_d, aa);
            if badge_a > 0.0 {
                let rim = cov(badge_d.abs() - 0.02, aa);
                let r = mix_u8(bg_r, rim_r, rim * 0.7);
                let g = mix_u8(bg_g, rim_g, rim * 0.7);
                let b = mix_u8(bg_b, rim_b, rim * 0.7);
                blend_over(pixel, r, g, b, badge_a);
            }

            // Power ring (arc with top gap)
            let ring_cx = 0.5;
            let ring_cy = 0.52;
            let dx = px - ring_cx;
            let dy = py - ring_cy;
            let angle_from_top = dx.abs().atan2(-dy);
            let ring_dist = if angle_from_top < 0.52 {
                // Smoothly cap near the gap edges (~30 degrees from top)
                let gx = 0.22 * 0.52_f32.sin();
                let gy = ring_cy - 0.22 * 0.52_f32.cos();
                let cap_l = ((px - (ring_cx - gx)).powi(2) + (py - gy).powi(2)).sqrt() - 0.048;
                let cap_r = ((px - (ring_cx + gx)).powi(2) + (py - gy).powi(2)).sqrt() - 0.048;
                cap_l.min(cap_r)
            } else {
                ((dx * dx + dy * dy).sqrt() - 0.22).abs() - 0.048
            };
            let bar_dist = sd_segment(px, py, 0.5, 0.23, 0.5, 0.48) - 0.048;
            let glyph_d = ring_dist.min(bar_dist);
            let glyph_a = cov(glyph_d, aa);
            blend_over(pixel, fg_r, fg_g, fg_b, glyph_a);
        }
    }

    buf
}

/// Render a quit ('x') icon for the Windows taskbar thumbnail toolbar.
pub fn render_thumb_quit_rgba(size: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let aa = 1.25 / (size as f32);

    for py_i in 0..size {
        let py = (py_i as f32 + 0.5) / (size as f32);
        for px_i in 0..size {
            let px = (px_i as f32 + 0.5) / (size as f32);
            let idx = ((py_i * size + px_i) * 4) as usize;
            let pixel = &mut buf[idx..idx + 4];

            let badge_d = ((px - 0.5).powi(2) + (py - 0.5).powi(2)).sqrt() - 0.44;
            let badge_a = cov(badge_d, aa);
            if badge_a > 0.0 {
                let rim = cov(badge_d.abs() - 0.02, aa);
                let r = mix_u8(56, 96, rim * 0.7);
                let g = mix_u8(53, 90, rim * 0.7);
                let b = mix_u8(50, 84, rim * 0.7);
                blend_over(pixel, r, g, b, badge_a);
            }

            let d1 = sd_segment(px, py, 0.34, 0.34, 0.66, 0.66) - 0.048;
            let d2 = sd_segment(px, py, 0.66, 0.34, 0.34, 0.66) - 0.048;
            let glyph_a = cov(d1.min(d2), aa);
            blend_over(pixel, 232, 229, 225, glyph_a);
        }
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_expected_sizes_and_distinct_states() {
        let off = render_keyboard_rgba(32, false);
        let on = render_keyboard_rgba(32, true);
        assert_eq!(off.len(), 32 * 32 * 4);
        assert_eq!(on.len(), 32 * 32 * 4);
        assert_ne!(off, on);

        let p_off = render_thumb_power_rgba(24, false);
        let p_on = render_thumb_power_rgba(24, true);
        let q = render_thumb_quit_rgba(24);
        assert_eq!(p_off.len(), 24 * 24 * 4);
        assert_eq!(p_on.len(), 24 * 24 * 4);
        assert_eq!(q.len(), 24 * 24 * 4);
        assert_ne!(p_off, p_on);
        assert_ne!(p_off, q);
    }
}
