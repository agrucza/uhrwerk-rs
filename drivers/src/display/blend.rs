//! Alpha blending and ordered dithering on an RGB565 framebuffer.
//!
//! `BlendTarget` extends `DrawTarget` with the read-modify-write
//! operations plain `DrawTarget` cannot express (it is write-only):
//! per-pixel alpha blending and dithered gradient fills. The UI's
//! render foundations (ring gauges, anti-aliased edges, panel
//! gradients, alpha-blended glyphs) all build on these three
//! operations.
//!
//! The free functions are the shared pixel math: implementors call
//! them so the blend/dither behavior stays identical everywhere, and
//! host tests can pin the math down without a framebuffer.
//!
//! All coordinates are panel-absolute; implementors clip, exactly as
//! they do for `DrawTarget`.

use embedded_graphics_core::{
    draw_target::DrawTarget,
    geometry::{Dimensions, Point},
    pixelcolor::Rgb565,
    primitives::Rectangle,
    Pixel,
};

/// `DrawTarget` extension for targets whose pixels can be read back,
/// enabling alpha blending and dithered fills.
pub trait BlendTarget: DrawTarget<Color = Rgb565> {
    /// Blend `color` over the existing pixel at panel-absolute
    /// `(x, y)` with `alpha` (0 = keep destination, 255 = opaque).
    fn blend_pixel(&mut self, x: i32, y: i32, color: Rgb565, alpha: u8);

    /// Fill a rectangle with a vertical gradient from `top` to
    /// `bottom`, ordered-dithered (Bayer 4x4) so RGB565 banding
    /// disappears. Opaque. The ramp spans `[y, y + h)` in panel
    /// coordinates, so a fill split across tiles stays seamless.
    fn fill_vgradient(&mut self, x: i32, y: i32, w: i32, h: i32, top: Rgb565, bottom: Rgb565);

    /// Blend a constant-alpha rectangle of `color` over the existing
    /// pixels.
    fn fill_blend(&mut self, x: i32, y: i32, w: i32, h: i32, color: Rgb565, alpha: u8);
}

// -- Clipping adapter ---------------------------------------------------------

/// Rectangular clipping adapter that preserves the [`BlendTarget`]
/// operations - the blend-capable counterpart of `embedded-graphics`'
/// `Clipped` (which erases them). Coordinates stay panel-absolute;
/// writes outside `clip` are dropped.
pub struct BlendClipped<'a, D: BlendTarget> {
    parent: &'a mut D,
    clip: Rectangle,
}

impl<'a, D: BlendTarget> BlendClipped<'a, D> {
    /// Clip all drawing to `clip` (intersected with the parent's own
    /// bounding box).
    pub fn new(parent: &'a mut D, clip: &Rectangle) -> Self {
        let clip = clip.intersection(&parent.bounding_box());
        Self { parent, clip }
    }

    /// Intersect a `(x, y, w, h)` rect with the clip window. Returns
    /// panel-absolute `(x, y, w, h)`, or `None` when nothing remains.
    fn clip_xywh(&self, x: i32, y: i32, w: i32, h: i32) -> Option<(i32, i32, i32, i32)> {
        let cx0 = x.max(self.clip.top_left.x);
        let cy0 = y.max(self.clip.top_left.y);
        let cx1 = (x + w).min(self.clip.top_left.x + self.clip.size.width as i32);
        let cy1 = (y + h).min(self.clip.top_left.y + self.clip.size.height as i32);
        if cx0 >= cx1 || cy0 >= cy1 { None } else { Some((cx0, cy0, cx1 - cx0, cy1 - cy0)) }
    }
}

impl<D: BlendTarget> Dimensions for BlendClipped<'_, D> {
    fn bounding_box(&self) -> Rectangle {
        self.clip
    }
}

impl<D: BlendTarget> DrawTarget for BlendClipped<'_, D> {
    type Color = Rgb565;
    type Error = D::Error;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Rgb565>>,
    {
        let clip = self.clip;
        self.parent
            .draw_iter(pixels.into_iter().filter(|Pixel(pt, _)| clip.contains(*pt)))
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Rgb565) -> Result<(), Self::Error> {
        let area = area.intersection(&self.clip);
        if area.is_zero_sized() { return Ok(()); }
        self.parent.fill_solid(&area, color)
    }
}

impl<D: BlendTarget> BlendTarget for BlendClipped<'_, D> {
    fn blend_pixel(&mut self, x: i32, y: i32, color: Rgb565, alpha: u8) {
        if self.clip.contains(Point::new(x, y)) {
            self.parent.blend_pixel(x, y, color, alpha);
        }
    }

    fn fill_vgradient(&mut self, x: i32, y: i32, w: i32, h: i32, top: Rgb565, bottom: Rgb565) {
        // The ramp must stay anchored to the caller's rect even when
        // the clip cuts it, so this can't forward the clipped rect to
        // the parent (that would compress the gradient). Row-by-row:
        // each surviving row keeps its position in the original ramp.
        let Some((cx, cy, cw, ch)) = self.clip_xywh(x, y, w, h) else { return };
        let (tr, tg, tb) = expand565(pack565(top));
        let (br, bg, bb) = expand565(pack565(bottom));
        let den = (h - 1).max(1) as u32;
        for py in cy..cy + ch {
            let num = (py - y) as u32;
            let r8 = lerp8(tr, br, num, den);
            let g8 = lerp8(tg, bg, num, den);
            let b8 = lerp8(tb, bb, num, den);
            let brow = &BAYER4[(py & 3) as usize];
            for px in cx..cx + cw {
                let raw = dither565(r8, g8, b8, brow[(px & 3) as usize]);
                let (r5, g6, b5) = ((raw >> 11) & 0x1F, (raw >> 5) & 0x3F, raw & 0x1F);
                self.parent.blend_pixel(px, py, Rgb565::new(r5 as u8, g6 as u8, b5 as u8), 255);
            }
        }
    }

    fn fill_blend(&mut self, x: i32, y: i32, w: i32, h: i32, color: Rgb565, alpha: u8) {
        let Some((cx, cy, cw, ch)) = self.clip_xywh(x, y, w, h) else { return };
        self.parent.fill_blend(cx, cy, cw, ch, color, alpha);
    }
}

// -- Shared pixel math --------------------------------------------------------

/// Bayer 4x4 ordered-dither thresholds (0..=15). Indexed
/// `[y & 3][x & 3]`.
pub const BAYER4: [[u8; 4]; 4] = [
    [0, 8, 2, 10],
    [12, 4, 14, 6],
    [3, 11, 1, 9],
    [15, 7, 13, 5],
];

/// Expand a packed RGB565 pixel to 8-bit channels with bit
/// replication, so full-scale 565 maps to full-scale 888.
pub const fn expand565(c: u16) -> (u8, u8, u8) {
    let r5 = ((c >> 11) & 0x1F) as u8;
    let g6 = ((c >> 5) & 0x3F) as u8;
    let b5 = (c & 0x1F) as u8;
    ((r5 << 3) | (r5 >> 2), (g6 << 2) | (g6 >> 4), (b5 << 3) | (b5 >> 2))
}

/// Pack an `embedded-graphics` color to the raw RGB565 wire format.
pub fn pack565(c: Rgb565) -> u16 {
    use embedded_graphics_core::pixelcolor::RgbColor;
    ((c.r() as u16) << 11) | ((c.g() as u16) << 5) | (c.b() as u16)
}

/// Quantize one 8-bit channel to `maxq + 1` levels (`maxq` = 31 or
/// 63) with an ordered-dither threshold `t` (0..=15). The bias spans
/// exactly one quantization step, so outputs are always the floor or
/// ceil of the ideal value and a 4x4 block averages to it.
const fn dither_channel(v8: u32, maxq: u32, t: u32) -> u16 {
    let scaled = v8 * maxq;
    let biased = scaled + (t * 255 + 8) / 16;
    let q = biased / 255;
    if q > maxq { maxq as u16 } else { q as u16 }
}

/// Quantize an 8-bit-per-channel color to packed RGB565 with ordered
/// dithering. `t` is the Bayer threshold for this pixel position.
pub const fn dither565(r8: u8, g8: u8, b8: u8, t: u8) -> u16 {
    (dither_channel(r8 as u32, 31, t as u32) << 11)
        | (dither_channel(g8 as u32, 63, t as u32) << 5)
        | dither_channel(b8 as u32, 31, t as u32)
}

/// Blend `src` over `dst` (both packed RGB565) with `alpha`
/// (0 = dst, 255 = src). Component-wise in native 565 widths.
pub const fn blend565(dst: u16, src: u16, alpha: u8) -> u16 {
    let a = alpha as u32;
    let na = 255 - a;
    let dr = (dst >> 11) as u32 & 0x1F;
    let dg = (dst >> 5) as u32 & 0x3F;
    let db = dst as u32 & 0x1F;
    let sr = (src >> 11) as u32 & 0x1F;
    let sg = (src >> 5) as u32 & 0x3F;
    let sb = src as u32 & 0x1F;
    let r = (dr * na + sr * a + 127) / 255;
    let g = (dg * na + sg * a + 127) / 255;
    let b = (db * na + sb * a + 127) / 255;
    ((r as u16) << 11) | ((g as u16) << 5) | (b as u16)
}

/// Scale a packed RGB565 color by `alpha` (0 = black, 255 =
/// unchanged) - blending against a known-black background without
/// reading it.
pub const fn scale565(c: u16, alpha: u8) -> u16 {
    blend565(0, c, alpha)
}

/// Linear interpolation between two 8-bit values at `num / den`
/// (`num <= den`, `den > 0`).
pub const fn lerp8(a: u8, b: u8, num: u32, den: u32) -> u8 {
    ((a as u32 * (den - num) + b as u32 * num + den / 2) / den) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_full_scale() {
        assert_eq!(expand565(0xFFFF), (255, 255, 255));
        assert_eq!(expand565(0x0000), (0, 0, 0));
    }

    #[test]
    fn dither_outputs_floor_or_ceil_and_averages() {
        // For every channel value, the 16 dithered outputs must all be
        // the floor or ceil of the ideal quantization, and their mean
        // must land within half a step of the ideal value.
        for maxq in [31u32, 63] {
            for v8 in 0..=255u32 {
                let ideal_num = v8 * maxq; // ideal = ideal_num / 255
                let floor = ideal_num / 255;
                let ceil = floor + if ideal_num % 255 == 0 { 0 } else { 1 };
                let mut sum = 0u32;
                for t in 0..16u32 {
                    let q = dither_channel(v8, maxq, t) as u32;
                    assert!(q == floor || q == ceil, "v8={v8} maxq={maxq} t={t} q={q}");
                    sum += q;
                }
                // mean*255*16 vs ideal*16*255: allow half a step (255*8).
                let mean_num = sum * 255; // = mean * 16 * 255
                let ideal16 = ideal_num * 16;
                let diff = mean_num.abs_diff(ideal16);
                assert!(diff <= 255 * 8, "v8={v8} maxq={maxq} mean off by {diff}");
            }
        }
    }

    #[test]
    fn blend_endpoints() {
        let dst = 0b01010_101010_01010;
        let src = 0b11111_000000_11111;
        assert_eq!(blend565(dst, src, 0), dst);
        assert_eq!(blend565(dst, src, 255), src);
        assert_eq!(scale565(src, 0), 0);
        assert_eq!(scale565(src, 255), src);
    }

    #[test]
    fn blend_midpoint_rounds() {
        // 50% between 0 and max must round to the upper middle for
        // odd ranges: (0*128 + 31*127)/255 with +127 rounding = 15..16.
        let mid = blend565(0, 0xFFFF, 128);
        let r = (mid >> 11) & 0x1F;
        let g = (mid >> 5) & 0x3F;
        let b = mid & 0x1F;
        assert!((15..=16).contains(&r));
        assert!((31..=32).contains(&g));
        assert!((15..=16).contains(&b));
    }

    #[test]
    fn lerp_endpoints_and_middle() {
        assert_eq!(lerp8(10, 250, 0, 100), 10);
        assert_eq!(lerp8(10, 250, 100, 100), 250);
        assert_eq!(lerp8(0, 200, 50, 100), 100);
    }
}
