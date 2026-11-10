//! Tray icon pixmaps, drawn from signed-distance shapes (no image assets needed).
//!
//! The panel runs outside the Flatpak and can't read icon files inside it, so
//! state icons are sent as ARGB32 pixmaps. Idle uses the desktop's own symbolic
//! icon instead, so it matches the panel theme.

/// Icon sizes offered to the panel (it picks the best fit, incl. HiDPI).
pub const SIZES: [u32; 3] = [22, 32, 48];
/// Number of distinct voice-level steps shown while listening.
pub const LEVELS: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Look {
    /// Red mic; `level` 0..LEVELS fills the capsule from the bottom.
    Listening { level: u8 },
    /// Grey mic at reduced opacity; `phase` 0..4 animates a pulse.
    Loading { phase: u8 },
    /// Grey mic with an amber warning badge.
    Error,
}

/// ARGB32 pixels in network byte order (what StatusNotifierItem expects).
#[derive(Debug, Clone, PartialEq)]
pub struct Pixmap {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Rgba(f32, f32, f32, f32);

const RED: Rgba = Rgba(0.87, 0.18, 0.18, 1.0);
const RED_DIM: Rgba = Rgba(0.87, 0.18, 0.18, 0.45);
const GREY: Rgba = Rgba(0.55, 0.57, 0.6, 1.0);
const AMBER: Rgba = Rgba(0.96, 0.64, 0.0, 1.0);
const DARK: Rgba = Rgba(0.1, 0.1, 0.1, 1.0);

/// Signed distance from `p` to a vertical capsule (centre x, from y0 to y1, radius r).
fn capsule(px: f32, py: f32, x: f32, y0: f32, y1: f32, r: f32) -> f32 {
    let cy = py.clamp(y0, y1);
    ((px - x).powi(2) + (py - cy).powi(2)).sqrt() - r
}

/// Distance to the lower half of a ring (the mic holder "U").
fn holder(px: f32, py: f32, cx: f32, cy: f32, r: f32, w: f32) -> f32 {
    if py < cy {
        // straight sides above the centre
        let dx = (px - cx).abs() - r;
        let dy = (cy - py) - r * 0.15;
        let side = dx.abs() - w;
        return side.max(dy);
    }
    let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
    (d - r).abs() - w
}

fn rect(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let dx = (x0 - px).max(px - x1);
    let dy = (y0 - py).max(py - y1);
    dx.max(dy)
}

/// Anti-aliased coverage for a signed distance (in pixels).
fn cover(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

fn blend(dst: &mut Rgba, src: Rgba, a: f32) {
    let a = src.3 * a;
    let out_a = a + dst.3 * (1.0 - a);
    if out_a <= 0.0 {
        *dst = Rgba(0.0, 0.0, 0.0, 0.0);
        return;
    }
    let mix = |s: f32, d: f32| (s * a + d * dst.3 * (1.0 - a)) / out_a;
    *dst = Rgba(
        mix(src.0, dst.0),
        mix(src.1, dst.1),
        mix(src.2, dst.2),
        out_a,
    );
}

/// Renders one icon at `size`×`size`.
pub fn render(look: Look, size: u32) -> Pixmap {
    let s = size as f32;
    let u = s / 24.0; // design grid: 24×24
    let (body, fill_color, fill_frac) = match look {
        Look::Listening { level } => (
            RED_DIM,
            RED,
            f32::from(level.min(LEVELS - 1) + 1) / f32::from(LEVELS),
        ),
        Look::Loading { phase } => {
            let a = 0.45 + 0.15 * f32::from(phase % 4);
            (Rgba(GREY.0, GREY.1, GREY.2, a), GREY, 0.0)
        }
        Look::Error => (GREY, GREY, 0.0),
    };
    let stroke = match look {
        Look::Listening { .. } => RED,
        Look::Loading { phase } => Rgba(GREY.0, GREY.1, GREY.2, 0.45 + 0.15 * f32::from(phase % 4)),
        Look::Error => GREY,
    };
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            // sample at pixel centre, in design units
            let (px, py) = ((x as f32 + 0.5) / u, (y as f32 + 0.5) / u);
            let px_per_unit = 1.0 / u;
            let mut c = Rgba(0.0, 0.0, 0.0, 0.0);
            // capsule body (x 12, y 4.5..10.5, r 3.5)
            let d_body = capsule(px, py, 12.0, 5.5, 10.5, 3.5);
            blend(&mut c, body, cover(d_body / px_per_unit));
            // level fill: lower part of the capsule
            if fill_frac > 0.0 {
                let top = 2.0 + (14.0 - 2.0) * (1.0 - fill_frac);
                let d_fill = d_body.max(top - py);
                blend(&mut c, fill_color, cover(d_fill / px_per_unit));
            }
            // holder, stem and base
            let d_holder = holder(px, py, 12.0, 10.5, 6.0, 0.9);
            let d_stem = rect(px, py, 11.1, 16.5, 12.9, 20.0);
            let d_base = rect(px, py, 8.0, 19.6, 16.0, 21.2);
            let d = d_holder.min(d_stem).min(d_base);
            blend(&mut c, stroke, cover(d / px_per_unit));
            if look == Look::Error {
                // amber badge with a dark "!" in the lower-right corner
                let d_badge = ((px - 18.5).powi(2) + (py - 17.5).powi(2)).sqrt() - 5.0;
                blend(&mut c, AMBER, cover(d_badge / px_per_unit));
                let d_bar = rect(px, py, 17.8, 14.3, 19.2, 18.3);
                let d_dot = rect(px, py, 17.8, 19.3, 19.2, 20.6);
                blend(&mut c, DARK, cover(d_bar.min(d_dot) / px_per_unit));
            }
            let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            data.extend_from_slice(&[to8(c.3), to8(c.0), to8(c.1), to8(c.2)]);
        }
    }
    Pixmap {
        width: size as i32,
        height: size as i32,
        data,
    }
}

/// Maps an RMS level (0..1) to 0..LEVELS-1 on a dB scale (−55 dB … −15 dB).
pub fn level_step(rms: f32) -> u8 {
    let db = 20.0 * rms.max(1e-6).log10();
    let t = ((db + 55.0) / 40.0).clamp(0.0, 0.999);
    (t * f32::from(LEVELS)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_sum(p: &Pixmap) -> u64 {
        p.data.chunks(4).map(|px| u64::from(px[0])).sum()
    }

    fn red_sum(p: &Pixmap) -> u64 {
        p.data
            .chunks(4)
            .filter(|px| px[1] > 150 && px[2] < 100)
            .map(|px| u64::from(px[0]))
            .sum()
    }

    #[test]
    fn sizes_and_format() {
        for s in SIZES {
            let p = render(Look::Error, s);
            assert_eq!(p.width as u32, s);
            assert_eq!(p.data.len() as u32, s * s * 4);
            // corners are transparent, something is drawn
            assert_eq!(p.data[0], 0);
            assert!(alpha_sum(&p) > 0);
        }
    }

    #[test]
    fn higher_level_shows_more_solid_red() {
        let lo = red_sum(&render(Look::Listening { level: 0 }, 48));
        let hi = red_sum(&render(Look::Listening { level: LEVELS - 1 }, 48));
        assert!(hi > lo, "{hi} <= {lo}");
    }

    #[test]
    fn error_has_a_badge() {
        let e = render(Look::Error, 48);
        let l = render(Look::Loading { phase: 3 }, 48);
        let amber = |p: &Pixmap| {
            p.data
                .chunks(4)
                .filter(|px| px[1] > 200 && px[2] > 120 && px[3] < 60)
                .count()
        };
        assert!(amber(&e) > 20 && amber(&l) == 0);
    }

    #[test]
    fn level_steps() {
        assert_eq!(level_step(0.0), 0);
        assert_eq!(level_step(0.001), 0); // -60 dB
        assert_eq!(level_step(1.0), LEVELS - 1);
        assert!(level_step(0.02) > level_step(0.003));
    }
}
