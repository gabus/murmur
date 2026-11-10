//! Tray icon pixmaps: the Murmur "Wave M" mark (docs/icon-options/2-wave-m), drawn
//! from its curve so no image assets are needed.
//!
//! The panel runs outside the Flatpak and can't read files inside it, so state icons
//! are sent as ARGB32 pixmaps. Idle uses the exported symbolic icon by name (the panel
//! recolours it to match its theme); the idle pixmap is only a fallback.

/// Icon sizes offered to the panel (it picks the best fit, incl. HiDPI).
pub const SIZES: [u32; 3] = [22, 32, 48];
/// Number of distinct voice-level steps shown while listening.
pub const LEVELS: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Look {
    /// Neutral grey mark (fallback when the panel can't find the named icon).
    Idle,
    /// Red mark; the wave gets taller with the voice `level` (0..LEVELS).
    Listening { level: u8 },
    /// Grey mark at reduced opacity; `phase` 0..4 animates a pulse.
    Loading { phase: u8 },
    /// Grey mark with an amber warning badge.
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

const RED: Rgba = Rgba(0.85, 0.27, 0.33, 1.0);
const GREY: Rgba = Rgba(0.6, 0.62, 0.64, 1.0);
const AMBER: Rgba = Rgba(0.96, 0.64, 0.0, 1.0);
const DARK: Rgba = Rgba(0.1, 0.1, 0.1, 1.0);

/// The Wave M in a 24-unit design grid: the same curve as the SVG (16-unit grid × 1.5).
const WAVE_M: [[(f32, f32); 4]; 4] = [
    [(2.4, 18.3), (4.5, 18.3), (5.1, 5.7), (7.5, 5.7)],
    [(7.5, 5.7), (9.75, 5.7), (10.5, 15.3), (12.0, 15.3)],
    [(12.0, 15.3), (13.5, 15.3), (14.25, 5.7), (16.5, 5.7)],
    [(16.5, 5.7), (18.9, 5.7), (19.5, 18.3), (21.6, 18.3)],
];
const STROKE_HALF: f32 = 1.3;
const CENTER_Y: f32 = 12.0;

/// The curve as a polyline, with its height scaled by `k` around the centre line.
fn wave_points(k: f32) -> Vec<(f32, f32)> {
    const STEPS: usize = 16;
    let mut pts = Vec::with_capacity(WAVE_M.len() * STEPS + 1);
    for (i, [p0, p1, p2, p3]) in WAVE_M.iter().enumerate() {
        for s in 0..=STEPS {
            if i > 0 && s == 0 {
                continue;
            }
            let t = s as f32 / STEPS as f32;
            let u = 1.0 - t;
            let x = u * u * u * p0.0
                + 3.0 * u * u * t * p1.0
                + 3.0 * u * t * t * p2.0
                + t * t * t * p3.0;
            let y = u * u * u * p0.1
                + 3.0 * u * u * t * p1.1
                + 3.0 * u * t * t * p2.1
                + t * t * t * p3.1;
            pts.push((x, CENTER_Y + (y - CENTER_Y) * k));
        }
    }
    pts
}

/// Distance from `p` to the polyline.
fn dist_polyline(px: f32, py: f32, pts: &[(f32, f32)]) -> f32 {
    pts.windows(2)
        .map(|w| {
            let ((ax, ay), (bx, by)) = (w[0], w[1]);
            let (dx, dy) = (bx - ax, by - ay);
            let len2 = dx * dx + dy * dy;
            let t = if len2 > 0.0 {
                (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
        })
        .fold(f32::INFINITY, f32::min)
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
    let u = size as f32 / 24.0; // pixels per design unit
    let (color, k) = match look {
        Look::Idle | Look::Error => (GREY, 1.0),
        Look::Listening { level } => {
            // Quiet: a flat wave; loud: the full M.
            let l = f32::from(level.min(LEVELS - 1)) / f32::from(LEVELS - 1);
            (RED, 0.45 + 0.55 * l)
        }
        Look::Loading { phase } => {
            let a = 0.45 + 0.15 * f32::from(phase % 4);
            (Rgba(GREY.0, GREY.1, GREY.2, a), 1.0)
        }
    };
    let pts = wave_points(k);
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (px, py) = ((x as f32 + 0.5) / u, (y as f32 + 0.5) / u);
            let mut c = Rgba(0.0, 0.0, 0.0, 0.0);
            let d = dist_polyline(px, py, &pts) - STROKE_HALF;
            blend(&mut c, color, cover(d * u));
            if look == Look::Error {
                // amber badge with a dark "!" in the lower-right corner
                let d_badge = ((px - 18.5).powi(2) + (py - 17.5).powi(2)).sqrt() - 5.0;
                blend(&mut c, AMBER, cover(d_badge * u));
                let d_bar = rect(px, py, 17.8, 14.3, 19.2, 18.3);
                let d_dot = rect(px, py, 17.8, 19.3, 19.2, 20.6);
                blend(&mut c, DARK, cover(d_bar.min(d_dot) * u));
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
    fn louder_voice_draws_a_taller_wave() {
        let lo = red_sum(&render(Look::Listening { level: 0 }, 48));
        let hi = red_sum(&render(Look::Listening { level: LEVELS - 1 }, 48));
        assert!(hi > lo, "{hi} <= {lo}");
    }

    #[test]
    fn the_wave_is_centred_and_inside_the_icon() {
        let p = render(Look::Idle, 48);
        let row_has_ink = |y: usize| (0..48).any(|x| p.data[(y * 48 + x) * 4] > 0);
        assert!(!row_has_ink(0) && !row_has_ink(47), "touches the edge");
        assert!(row_has_ink(24), "nothing across the middle");
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
