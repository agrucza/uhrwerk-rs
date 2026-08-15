//! Ring gauge - an anti-aliased annular progress ring.
//!
//! The arc starts at 12 o'clock and sweeps clockwise by
//! `value / max` of a full turn, drawn over an optional full-circle
//! track. Both radial edges are anti-aliased (the ring's silhouette
//! is what sells the look on the AMOLED); the sweep's angular ends
//! are finished with rounded caps, which also gives a "zero dot" at
//! `value = 0`.
//!
//! All math is integer-only (no FPU on the C6): distances in Q8 via
//! a 64-bit integer square root that only runs for pixels inside the
//! two 2 px edge bands, angles via a Q14 quarter-wave sine table and
//! cross-product half-plane tests.

use crate::ui::theme::Color;
use crate::ui::types::{BlendTarget, RenderCtx};

/// Quarter-wave sine table, `sin(0..=90 deg) * 16384`.
const SIN_Q14: [i32; 91] = [
    0, 286, 572, 857, 1143, 1428, 1713, 1997, 2280, 2563,
    2845, 3126, 3406, 3686, 3964, 4240, 4516, 4790, 5063, 5334,
    5604, 5872, 6138, 6402, 6664, 6924, 7182, 7438, 7692, 7943,
    8192, 8438, 8682, 8923, 9162, 9397, 9630, 9860, 10087, 10311,
    10531, 10749, 10963, 11174, 11381, 11585, 11786, 11982, 12176, 12365,
    12551, 12733, 12911, 13085, 13255, 13421, 13583, 13741, 13894, 14044,
    14189, 14330, 14466, 14598, 14726, 14849, 14968, 15082, 15191, 15296,
    15396, 15491, 15582, 15668, 15749, 15826, 15897, 15964, 16026, 16083,
    16135, 16182, 16225, 16262, 16294, 16322, 16344, 16362, 16374, 16382,
    16384,
];

/// `(sin, cos)` of `deg` degrees in Q14, quadrant-folded from the
/// quarter-wave table.
fn sin_cos_q14(deg: i32) -> (i32, i32) {
    let d = deg.rem_euclid(360) as usize;
    match d {
        0..=90 => (SIN_Q14[d], SIN_Q14[90 - d]),
        91..=180 => (SIN_Q14[180 - d], -SIN_Q14[d - 90]),
        181..=270 => (-SIN_Q14[d - 180], -SIN_Q14[270 - d]),
        _ => (-SIN_Q14[360 - d], SIN_Q14[d - 270]),
    }
}

/// Integer square root of a `u64`, bit-pair method.
fn isqrt64(v: u64) -> u32 {
    let mut x = v;
    let mut res: u64 = 0;
    let mut bit: u64 = 1 << 62;
    while bit > x {
        bit >>= 2;
    }
    while bit != 0 {
        if x >= res + bit {
            x -= res + bit;
            res = (res >> 1) + bit;
        } else {
            res >>= 1;
        }
        bit >>= 2;
    }
    res as u32
}

/// Radial coverage of the annulus `[r_in, r_out]` at squared distance
/// `d2` from the center, as 0..=255. `None` means "fully inside -
/// skip the square root". The AA band is ~1 px on each side of each
/// edge.
fn annulus_alpha(d2: i64, r_in: i32, r_out: i32) -> Option<u8> {
    // Fully inside both edges: opaque without computing the root.
    let inner_solid = if r_in > 0 { d2 >= ((r_in + 1) as i64).pow(2) } else { true };
    if inner_solid && d2 <= ((r_out - 1) as i64).pow(2) {
        return Some(255);
    }
    let d_q8 = isqrt64((d2 as u64) << 16) as i32;
    let cov_out = ((r_out << 8) + 128 - d_q8).clamp(0, 256);
    let cov_in = if r_in > 0 {
        (d_q8 - (r_in << 8) + 128).clamp(0, 256)
    } else {
        256
    };
    let a = (cov_out * cov_in) >> 8;
    if a <= 0 { None } else { Some(a.min(255) as u8) }
}

/// Draw an anti-aliased filled disc at `(ccx, ccy)`, clipped to the
/// tile's row band. Used for the arc's rounded end caps.
fn aa_disc<D: BlendTarget>(
    display: &mut D,
    ctx: &RenderCtx,
    ccx: i32,
    ccy: i32,
    r: i32,
    color: Color,
) {
    let (t0, t1) = ctx.y_range();
    let y0 = (ccy - r - 1).max(t0);
    let y1 = (ccy + r + 2).min(t1);
    for y in y0..y1 {
        let dy = (y - ccy) as i64;
        for x in (ccx - r - 1)..(ccx + r + 2) {
            let dx = (x - ccx) as i64;
            if let Some(a) = annulus_alpha(dx * dx + dy * dy, 0, r) {
                display.blend_pixel(x, y, color, a);
            }
        }
    }
}

/// Draw a ring gauge centered at `(cx, cy)`.
///
/// * `r_outer` / `thickness` - outer radius and radial width of the
///   ring band.
/// * `value` / `max` - arc sweep as a fraction of a full turn,
///   clamped to `max`; `max == 0` draws the track only.
/// * `color` - arc + cap color.
/// * `track` - optional full-circle underlay for the remainder.
///
/// The row loop is clipped to `ctx`'s tile band, so per-tile calls
/// only pay for the rows they can affect.
pub fn ring_gauge<D: BlendTarget>(
    display: &mut D,
    ctx: &RenderCtx,
    cx: i32,
    cy: i32,
    r_outer: i32,
    thickness: i32,
    value: u32,
    max: u32,
    color: Color,
    track: Option<Color>,
) {
    if r_outer <= 1 || thickness < 1 {
        return;
    }
    if !ctx.intersects_y(cy - r_outer - 1, cy + r_outer + 2) {
        return;
    }
    let r_in = (r_outer - thickness).max(0);
    let deg = if max == 0 { 0 } else { (value.min(max) as u64 * 360 / max as u64) as i32 };
    let full = deg >= 360;
    let (sin_e, cos_e) = sin_cos_q14(deg);
    // Arc end direction in screen coords (y down, 0 deg = up).
    let (ex, ey) = (sin_e, -cos_e);

    let (t0, t1) = ctx.y_range();
    let y0 = (cy - r_outer - 1).max(t0);
    let y1 = (cy + r_outer + 2).min(t1);
    let lim2 = ((r_outer + 1) as i64).pow(2);
    for y in y0..y1 {
        let dy = y - cy;
        let dy2 = (dy as i64) * (dy as i64);
        for x in (cx - r_outer - 1)..(cx + r_outer + 2) {
            let dx = x - cx;
            let d2 = (dx as i64) * (dx as i64) + dy2;
            if d2 > lim2 {
                continue;
            }
            let Some(alpha) = annulus_alpha(d2, r_in, r_outer) else { continue };
            let in_arc = full || deg > 0 && {
                // Clockwise-from-12 sweep membership via half-plane
                // tests: `dx >= 0` = right of the start direction,
                // cross(p, end) >= 0 = not yet past the end direction.
                // (`deg == 0` is an empty arc - the half-plane pair
                // would otherwise pass on the whole x == cx line.)
                let before_end = dx * ey - dy * ex >= 0;
                if deg <= 180 { dx >= 0 && before_end } else { dx >= 0 || before_end }
            };
            let c = if in_arc {
                color
            } else {
                match track {
                    Some(t) => t,
                    None => continue,
                }
            };
            display.blend_pixel(x, y, c, alpha);
        }
    }

    // Rounded end caps on the arc (skipped on a seamless full ring).
    // At zero sweep both caps coincide at 12 o'clock; drawing one
    // gives the "zero dot".
    if !full && thickness >= 3 {
        let cap_r = thickness / 2;
        let r_mid = r_outer - thickness / 2;
        aa_disc(display, ctx, cx, cy - r_mid, cap_r, color);
        if deg > 0 {
            let ecx = cx + ((r_mid * sin_e + 8192) >> 14);
            let ecy = cy - ((r_mid * cos_e + 8192) >> 14);
            aa_disc(display, ctx, ecx, ecy, cap_r, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::{
        draw_target::DrawTarget,
        geometry::{OriginDimensions, Size},
        pixelcolor::Rgb565,
        Pixel,
    };
    use drivers::display::blend::{blend565, pack565};

    const W: i32 = 64;
    const H: i32 = 64;

    /// Minimal blend-capable canvas for geometry tests.
    struct TestCanvas {
        px: [u16; (W * H) as usize],
    }

    impl TestCanvas {
        fn new() -> Self {
            Self { px: [0; (W * H) as usize] }
        }
        fn at(&self, x: i32, y: i32) -> u16 {
            self.px[(y * W + x) as usize]
        }
    }

    impl OriginDimensions for TestCanvas {
        fn size(&self) -> Size {
            Size::new(W as u32, H as u32)
        }
    }

    impl DrawTarget for TestCanvas {
        type Color = Rgb565;
        type Error = core::convert::Infallible;
        fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
        where
            I: IntoIterator<Item = Pixel<Rgb565>>,
        {
            for Pixel(pt, c) in pixels {
                if pt.x >= 0 && pt.x < W && pt.y >= 0 && pt.y < H {
                    self.px[(pt.y * W + pt.x) as usize] = pack565(c);
                }
            }
            Ok(())
        }
    }

    impl BlendTarget for TestCanvas {
        fn blend_pixel(&mut self, x: i32, y: i32, color: Rgb565, alpha: u8) {
            if x >= 0 && x < W && y >= 0 && y < H {
                let idx = (y * W + x) as usize;
                self.px[idx] = blend565(self.px[idx], pack565(color), alpha);
            }
        }
        fn fill_vgradient(&mut self, _: i32, _: i32, _: i32, _: i32, _: Rgb565, _: Rgb565) {
            unimplemented!("not exercised by gauge tests");
        }
        fn fill_blend(&mut self, _: i32, _: i32, _: i32, _: i32, _: Rgb565, _: u8) {
            unimplemented!("not exercised by gauge tests");
        }
    }

    const ARC: Rgb565 = Rgb565::new(31, 0, 0);
    const TRACK: Rgb565 = Rgb565::new(0, 0, 31);
    const CX: i32 = 32;
    const CY: i32 = 32;
    const R: i32 = 20;
    const TH: i32 = 6;

    fn full_ctx() -> RenderCtx {
        RenderCtx::full_panel(H as u16)
    }

    #[test]
    fn full_ring_covers_annulus_only() {
        let mut c = TestCanvas::new();
        ring_gauge(&mut c, &full_ctx(), CX, CY, R, TH, 10, 10, ARC, None);
        let mid = R - TH / 2;
        // On the ring band: arc color, fully opaque.
        assert_eq!(c.at(CX + mid, CY), pack565(ARC));
        assert_eq!(c.at(CX - mid, CY), pack565(ARC));
        assert_eq!(c.at(CX, CY + mid), pack565(ARC));
        // Center and far outside: untouched.
        assert_eq!(c.at(CX, CY), 0);
        assert_eq!(c.at(CX + R + 4, CY), 0);
    }

    #[test]
    fn half_sweep_splits_arc_and_track() {
        let mut c = TestCanvas::new();
        ring_gauge(&mut c, &full_ctx(), CX, CY, R, TH, 5, 10, ARC, Some(TRACK));
        let mid = R - TH / 2;
        // 50% = 180 deg: right side is arc, left side is track.
        assert_eq!(c.at(CX + mid, CY), pack565(ARC));
        assert_eq!(c.at(CX - mid, CY), pack565(TRACK));
    }

    #[test]
    fn quarter_sweep_membership() {
        let mut c = TestCanvas::new();
        ring_gauge(&mut c, &full_ctx(), CX, CY, R, TH, 25, 100, ARC, Some(TRACK));
        let mid = R - TH / 2;
        // 25% = 90 deg. 45 deg (up-right diagonal) is in the arc;
        // 135 deg (down-right) is past it. d = mid / sqrt(2).
        let d = (mid * 1000) / 1414;
        assert_eq!(c.at(CX + d, CY - d), pack565(ARC));
        assert_eq!(c.at(CX + d, CY + d), pack565(TRACK));
    }

    #[test]
    fn tile_split_matches_full_render() {
        // Rendering per-tile must be pixel-identical to one full-panel
        // render - the gauge's tile clipping is an optimization only.
        let mut whole = TestCanvas::new();
        ring_gauge(&mut whole, &full_ctx(), CX, CY, R, TH, 3, 10, ARC, Some(TRACK));

        let mut tiled = TestCanvas::new();
        for tile_y in (0..H as u16).step_by(16) {
            let ctx = RenderCtx { tile_y, tile_h: 16 };
            ring_gauge(&mut tiled, &ctx, CX, CY, R, TH, 3, 10, ARC, Some(TRACK));
        }
        assert_eq!(whole.px[..], tiled.px[..]);
    }

    #[test]
    fn zero_value_draws_dot_and_track() {
        let mut c = TestCanvas::new();
        ring_gauge(&mut c, &full_ctx(), CX, CY, R, TH, 0, 10, ARC, Some(TRACK));
        let mid = R - TH / 2;
        // Zero dot at 12 o'clock, track elsewhere.
        assert_eq!(c.at(CX, CY - mid), pack565(ARC));
        assert_eq!(c.at(CX, CY + mid), pack565(TRACK));
    }

    #[test]
    fn sin_cos_quadrants() {
        assert_eq!(sin_cos_q14(0), (0, 16384));
        assert_eq!(sin_cos_q14(90), (16384, 0));
        assert_eq!(sin_cos_q14(180), (0, -16384));
        assert_eq!(sin_cos_q14(270), (-16384, 0));
        let (s, c) = sin_cos_q14(45);
        assert_eq!(s, c);
    }
}
