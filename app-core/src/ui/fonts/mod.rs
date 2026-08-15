//! Central font registry for the UI - baked Inter alpha atlases.
//!
//! Glyphs are pre-rasterized by `tools/fontbake` from the Inter TTFs
//! in `assets/fonts/` into 4-bit alpha atlases (the committed files
//! under `gen/`), and composited onto the framebuffer through
//! `BlendTarget::blend_pixel`, so text edges alpha-blend over
//! whatever is behind them.
//!
//! Screens use semantic roles (`caption`, `body`, `headline`,
//! `value`, `hero`, `mega`, `label`) instead of naming fonts - to
//! retypeface the UI, edit the role table in `tools/fontbake` and
//! re-run it; every screen follows.
//!
//! Helper functions wrap the common alignment patterns so callers
//! never assemble glyph runs themselves. Anchor semantics: `top_y`
//! is the top of the line box (baseline = `top_y + ascent`, where
//! ascent is the charset's tallest ink), horizontal centering is
//! advance-width based, and `draw_centered_in_rect` centers on the
//! exact ink bounding box.

use embedded_graphics::{geometry::Point, geometry::Size, primitives::Rectangle};

use crate::ui::theme::Color;
use crate::ui::types::BlendTarget;

mod gen;

// -- Baked-atlas data types ---------------------------------------------------
//
// Filled in by the generated tables under `gen/`; field layout is
// part of the fontbake output contract.

/// One baked font: vertical metrics plus its glyph table and packed
/// 4-bit alpha blob.
pub struct FontData {
    /// Baseline distance from the top of the line box.
    pub ascent: i16,
    /// Line-box extent below the baseline.
    pub descent: i16,
    /// Glyph metadata, sorted by `cp` for binary search.
    pub glyphs: &'static [Glyph],
    /// Packed alpha bitmaps: 4 bits per pixel, two pixels per byte
    /// (high nibble first), rows padded to whole bytes.
    pub alpha: &'static [u8],
}

/// Metadata for one glyph in the atlas.
pub struct Glyph {
    /// Unicode codepoint.
    pub cp: u32,
    /// Horizontal advance in pixels.
    pub adv: u8,
    /// Bitmap width / height in pixels (0 for blanks like space).
    pub w: u8,
    pub h: u8,
    /// Bitmap left bearing relative to the pen position.
    pub left: i8,
    /// Bitmap top relative to the baseline (negative = above).
    pub top: i8,
    /// Byte offset of the bitmap in `FontData::alpha`.
    pub off: u32,
}

/// Cheap copyable handle to a baked font. Factories return this by
/// value so call sites can keep passing `&fonts::caption()`.
#[derive(Copy, Clone)]
pub struct Font(&'static FontData);

impl Font {
    fn glyph(&self, ch: char) -> Option<&'static Glyph> {
        let cp = ch as u32;
        self.0
            .glyphs
            .binary_search_by(|g| g.cp.cmp(&cp))
            .ok()
            .map(|i| &self.0.glyphs[i])
    }

    /// Baseline distance from a top-anchored draw position.
    pub fn ascent(&self) -> i32 {
        self.0.ascent as i32
    }

    /// Full line-box height (ascent + descent).
    pub fn line_height(&self) -> i32 {
        (self.0.ascent + self.0.descent) as i32
    }
}

// -- Role factories -----------------------------------------------------------

/// Small caption / label text (Inter Regular, 10 px caps).
pub fn caption() -> Font { Font(&gen::caption::FONT) }
/// Default body text (Inter Regular, 14 px caps).
pub fn body() -> Font { Font(&gen::body::FONT) }
/// Section titles and dates (Inter Regular, 18 px caps).
pub fn headline() -> Font { Font(&gen::headline::FONT) }
/// Prominent values / headlines (Inter SemiBold, 24 px caps).
pub fn value() -> Font { Font(&gen::value::FONT) }
/// Uppercase chrome labels and pill text (Inter SemiBold, 10 px caps).
#[allow(dead_code)]
pub fn label() -> Font { Font(&gen::label::FONT) }
/// Hero numeric readouts, digits + punctuation only
/// (Inter Display SemiBold, 49 px digits).
pub fn hero() -> Font { Font(&gen::hero::FONT) }
/// Watch-face stacked HH/MM digits, digits + punctuation only
/// (Inter Display SemiBold, 78 px digits).
pub fn mega() -> Font { Font(&gen::mega::FONT) }

// -- Core glyph-run rendering -------------------------------------------------

/// Blit one glyph's 4-bit alpha bitmap at pen position `(pen_x,
/// baseline_y)`.
fn blit<D: BlendTarget>(
    display: &mut D,
    data: &FontData,
    g: &Glyph,
    pen_x: i32,
    baseline_y: i32,
    color: Color,
) {
    let w = g.w as usize;
    let row_bytes = (w + 1) / 2;
    let x0 = pen_x + g.left as i32;
    let y0 = baseline_y + g.top as i32;
    for row in 0..g.h as usize {
        let ro = g.off as usize + row * row_bytes;
        for col in 0..w {
            let byte = data.alpha[ro + col / 2];
            let n = if col % 2 == 0 { byte >> 4 } else { byte & 0x0F };
            if n != 0 {
                // Expand 4-bit coverage to 8-bit (0xF -> 0xFF).
                display.blend_pixel(x0 + col as i32, y0 + row as i32, color, (n << 4) | n);
            }
        }
    }
}

/// Draw a glyph run with the pen starting at `x`, top of the line box
/// at `top_y`. Unknown characters are skipped.
fn draw_run<D: BlendTarget>(
    display: &mut D,
    font: &Font,
    text: &str,
    x: i32,
    top_y: i32,
    color: Color,
) {
    let baseline = top_y + font.ascent();
    let mut pen = x;
    for ch in text.chars() {
        let Some(g) = font.glyph(ch) else { continue };
        if g.w > 0 {
            blit(display, font.0, g, pen, baseline, color);
        }
        pen += g.adv as i32;
    }
}

// -- Measurement --------------------------------------------------------------

/// Measure the horizontal advance width of `text`. Unknown
/// characters contribute nothing, matching `draw_run`.
pub fn measure_width(font: &Font, text: &str) -> i32 {
    text.chars()
        .filter_map(|ch| font.glyph(ch))
        .map(|g| g.adv as i32)
        .sum()
}

/// Measure the exact ink bounding box of `text` relative to a
/// [`draw_at`] anchor of `(0, 0)`. `None` when the run leaves no ink
/// (empty string, blanks only).
pub fn measure_bbox(font: &Font, text: &str) -> Option<Rectangle> {
    let ascent = font.ascent();
    let mut pen = 0i32;
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    let mut any = false;
    for ch in text.chars() {
        let Some(g) = font.glyph(ch) else { continue };
        if g.w > 0 && g.h > 0 {
            let gx0 = pen + g.left as i32;
            let gy0 = ascent + g.top as i32;
            x0 = x0.min(gx0);
            y0 = y0.min(gy0);
            x1 = x1.max(gx0 + g.w as i32);
            y1 = y1.max(gy0 + g.h as i32);
            any = true;
        }
        pen += g.adv as i32;
    }
    any.then(|| {
        Rectangle::new(
            Point::new(x0, y0),
            Size::new((x1 - x0) as u32, (y1 - y0) as u32),
        )
    })
}

// -- Rendering helpers --------------------------------------------------------

/// Draw text horizontally centered around `cx`, with the top of the
/// line box at `top_y`.
pub fn draw_centered<D: BlendTarget>(
    display: &mut D,
    font: &Font,
    text: &str,
    cx: i32, top_y: i32,
    color: Color,
) {
    let w = measure_width(font, text);
    draw_run(display, font, text, cx - w / 2, top_y, color);
}

/// Draw text fully centered (both axes) around (cx, cy) using the
/// line box for vertical centering. Prefer `draw_centered_in_rect`
/// for visually precise centering in a known container - line-box
/// centering leaves text without descenders sitting visually too
/// high.
#[allow(dead_code)]
pub fn draw_centered_xy<D: BlendTarget>(
    display: &mut D,
    font: &Font,
    text: &str,
    cx: i32, cy: i32,
    color: Color,
) {
    draw_centered(display, font, text, cx, cy - font.line_height() / 2, color);
}

/// Draw text centered inside `rect` using the **visible glyph
/// bounding box** as the alignment reference. Produces visually
/// correct centering for any font/text combination, independent of
/// ascender/descender metrics.
pub fn draw_centered_in_rect<D: BlendTarget>(
    display: &mut D,
    font: &Font,
    text: &str,
    rect: Rectangle,
    color: Color,
) {
    let Some(bbox) = measure_bbox(font, text) else { return };
    let text_w = bbox.size.width as i32;
    let text_h = bbox.size.height as i32;

    let rect_cx = rect.top_left.x + rect.size.width as i32 / 2;
    let rect_cy = rect.top_left.y + rect.size.height as i32 / 2;

    // Anchor so the ink bbox center lands exactly on the rect center:
    // drawn at (x, y), the ink appears at (x, y) + bbox.top_left.
    let draw_x = rect_cx - text_w / 2 - bbox.top_left.x;
    let draw_y = rect_cy - text_h / 2 - bbox.top_left.y;
    draw_run(display, font, text, draw_x, draw_y, color);
}

/// Draw text top-left aligned at `(x, y)`.
pub fn draw_at<D: BlendTarget>(
    display: &mut D,
    font: &Font,
    text: &str,
    x: i32, y: i32,
    color: Color,
) {
    draw_run(display, font, text, x, y, color);
}

/// Draw text right-aligned: glyphs end at `right_x`, top at `y`.
pub fn draw_right<D: BlendTarget>(
    display: &mut D,
    font: &Font,
    text: &str,
    right_x: i32, y: i32,
    color: Color,
) {
    let w = measure_width(font, text);
    draw_run(display, font, text, right_x - w, y, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_fonts() -> [(&'static str, Font); 5] {
        [
            ("caption", caption()),
            ("body", body()),
            ("headline", headline()),
            ("label", label()),
            ("value", value()),
        ]
    }

    #[test]
    fn text_fonts_cover_printable_ascii_and_german() {
        for (name, f) in text_fonts() {
            for cp in 0x20u32..=0x7E {
                let ch = char::from_u32(cp).unwrap();
                assert!(f.glyph(ch).is_some(), "{name}: missing '{ch}'");
            }
            for ch in "ÄÖÜäöüß°".chars() {
                assert!(f.glyph(ch).is_some(), "{name}: missing '{ch}'");
            }
        }
    }

    #[test]
    fn digit_fonts_cover_their_charset() {
        for (name, f) in [("hero", hero()), ("mega", mega())] {
            for ch in "0123456789:.-+% ".chars() {
                assert!(f.glyph(ch).is_some(), "{name}: missing '{ch}'");
            }
        }
    }

    #[test]
    fn glyph_tables_sorted_and_bitmaps_in_bounds() {
        let all = [caption(), body(), headline(), label(), value(), hero(), mega()];
        for f in all {
            let d = f.0;
            for pair in d.glyphs.windows(2) {
                assert!(pair[0].cp < pair[1].cp, "glyph table not strictly sorted");
            }
            for g in d.glyphs {
                let row_bytes = (g.w as usize + 1) / 2;
                let end = g.off as usize + row_bytes * g.h as usize;
                assert!(end <= d.alpha.len(), "cp {} bitmap out of bounds", g.cp);
            }
        }
    }

    #[test]
    fn measurement_sanity() {
        let f = body();
        let w1 = measure_width(&f, "H");
        let w2 = measure_width(&f, "HH");
        assert!(w1 > 0);
        assert_eq!(w2, 2 * w1);

        // 'H' ink height is the bake target for the body role.
        let bbox = measure_bbox(&f, "H").unwrap();
        assert_eq!(bbox.size.height, 14);
        // Ink sits above the baseline, inside the line box.
        assert!(bbox.top_left.y >= 0);
        assert!(bbox.top_left.y + (bbox.size.height as i32) <= f.ascent());

        assert!(measure_bbox(&f, " ").is_none());
        assert!(measure_bbox(&f, "").is_none());
    }

    #[test]
    fn mega_digits_are_target_height() {
        let bbox = measure_bbox(&mega(), "0").unwrap();
        assert_eq!(bbox.size.height, 78);
        let bbox = measure_bbox(&hero(), "0").unwrap();
        assert_eq!(bbox.size.height, 49);
    }

    #[test]
    fn digit_ink_starts_at_the_draw_anchor() {
        // The digit roles' ascent equals the digit ink height (their
        // charset has nothing taller), so a Top-anchored draw puts
        // digit ink exactly at top_y - where the clock's hero-stack
        // layout constants expect it. Guards against em-metric
        // ascents sneaking back in and shifting the stack.
        for f in [mega(), hero()] {
            let bbox = measure_bbox(&f, "0123456789").unwrap();
            assert_eq!(bbox.top_left.y, 0);
        }
    }
}
