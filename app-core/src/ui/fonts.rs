//! Central font registry for the UI.
//!
//! All u8g2-fonts type names live here so any font choice changes
//! happen in exactly one file. Screens use semantic roles
//! (`caption`, `body`, `headline`, `value`, `hero`) instead of raw
//! font types - to retypeface the entire UI, edit the right-hand
//! sides of the type aliases below and every screen follows.
//!
//! Helper functions wrap the most common rendering patterns so
//! callers never have to assemble the verbose `render_aligned`
//! arguments themselves.

use embedded_graphics::{
    geometry::Point,
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;
use u8g2_fonts::{fonts as raw, FontRenderer};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};

// -- Font role aliases -------------------------------------------------------
//
// `_te` charset includes Latin-1 (German umlauts, French accents, etc.).

/// Small grey caption / label text (~10 px tall, regular weight).
pub type Caption = raw::u8g2_font_helvR10_te;
/// Default body text (~14 px tall, regular weight).
pub type Body = raw::u8g2_font_helvR14_te;
/// Medium-prominent text for section titles, dates, captions that
/// need a little more presence (~18 px tall, regular weight).
pub type Headline = raw::u8g2_font_helvR18_te;
/// Bold value / headline text (~24 px tall, bold weight).
pub type Value = raw::u8g2_font_helvB24_te;
/// Hero numeric display text - clean bold sans-serif at 49 px, used
/// for stopwatch/timer readouts. Digits-only `_tn` charset for
/// minimal flash footprint.
pub type Hero = raw::u8g2_font_fub49_tn;
/// Squared-off geometric digits at 78 px, used for the Nightwatch
/// watch face's stacked HH/MM. Digits-only.
pub type Mega = raw::u8g2_font_logisoso78_tn;
/// Bold small caps-ish label font (~10 px). Used for Nightwatch
/// uppercase chrome labels and pill text.
pub type Label = raw::u8g2_font_helvB10_te;

// -- Renderer factories ------------------------------------------------------
//
// Each call constructs a fresh `FontRenderer`. They're cheap to
// build (no heap, no allocation) so callers don't need to cache them.

pub fn caption()  -> FontRenderer { FontRenderer::new::<Caption>() }
pub fn body()     -> FontRenderer { FontRenderer::new::<Body>() }
pub fn headline() -> FontRenderer { FontRenderer::new::<Headline>() }
pub fn value()    -> FontRenderer { FontRenderer::new::<Value>() }
pub fn hero()     -> FontRenderer { FontRenderer::new::<Hero>() }

// -- Rendering helpers -------------------------------------------------------

/// Draw text horizontally centered around `cx`, with the top of the
/// glyphs at `top_y`.
pub fn draw_centered<D: BlendTarget>(
    display: &mut D,
    font: &FontRenderer,
    text: &str,
    cx: i32, top_y: i32,
    color: Color,
) {
    let _ = font.render_aligned(
        text,
        Point::new(cx, top_y),
        VerticalPosition::Top,
        HorizontalAlignment::Center,
        FontColor::Transparent(color),
        display,
    );
}

/// Draw text fully centered (both axes) around (cx, cy) using u8g2's
/// built-in line-box centering. Prefer `draw_centered_in_rect` for
/// visually precise centering in a known container - u8g2's
/// `VerticalPosition::Center` is based on font line metrics
/// (ascent + descent), which leaves text without descenders sitting
/// visually too high.
#[allow(dead_code)]
pub fn draw_centered_xy<D: BlendTarget>(
    display: &mut D,
    font: &FontRenderer,
    text: &str,
    cx: i32, cy: i32,
    color: Color,
) {
    let _ = font.render_aligned(
        text,
        Point::new(cx, cy),
        VerticalPosition::Center,
        HorizontalAlignment::Center,
        FontColor::Transparent(color),
        display,
    );
}

/// Draw text centered inside `rect` using the **visible glyph bounding
/// box** as the alignment reference. Produces visually correct
/// centering for any font/text combination, independent of the font's
/// ascender/descender metrics.
///
/// Implementation: measure the text at (0,0) top-aligned, then offset
/// the render position so the returned bbox lands at the rect's
/// center.
pub fn draw_centered_in_rect<D: BlendTarget>(
    display: &mut D,
    font: &FontRenderer,
    text: &str,
    rect: Rectangle,
    color: Color,
) {
    // 1. Measure the rendered glyph bounding box at origin, top-aligned.
    let dims = match font.get_rendered_dimensions(
        text, Point::zero(), VerticalPosition::Top,
    ) {
        Ok(d) => d,
        Err(_) => return,
    };
    let bbox = match dims.bounding_box {
        Some(b) => b,
        None => return,
    };
    let text_w = bbox.size.width as i32;
    let text_h = bbox.size.height as i32;

    // 2. Center of the target rectangle.
    let rect_cx = rect.top_left.x + rect.size.width as i32 / 2;
    let rect_cy = rect.top_left.y + rect.size.height as i32 / 2;

    // 3. Compute the render anchor so that the glyph bbox center lands
    // exactly on the rect center. When rendered top-aligned at point P,
    // the visible bbox appears at P + bbox.top_left with size
    // (text_w, text_h). Its center is P + bbox.top_left +
    // (text_w/2, text_h/2). We want that to equal (rect_cx, rect_cy).
    let draw_x = rect_cx - text_w / 2 - bbox.top_left.x;
    let draw_y = rect_cy - text_h / 2 - bbox.top_left.y;

    let _ = font.render_aligned(
        text,
        Point::new(draw_x, draw_y),
        VerticalPosition::Top,
        HorizontalAlignment::Left,
        FontColor::Transparent(color),
        display,
    );
}

/// Draw text top-left aligned at `(x, y)`.
#[allow(dead_code)]
pub fn draw_at<D: BlendTarget>(
    display: &mut D,
    font: &FontRenderer,
    text: &str,
    x: i32, y: i32,
    color: Color,
) {
    let _ = font.render_aligned(
        text,
        Point::new(x, y),
        VerticalPosition::Top,
        HorizontalAlignment::Left,
        FontColor::Transparent(color),
        display,
    );
}

/// Draw text right-aligned: glyphs end at `right_x`, top at `y`.
pub fn draw_right<D: BlendTarget>(
    display: &mut D,
    font: &FontRenderer,
    text: &str,
    right_x: i32, y: i32,
    color: Color,
) {
    let _ = font.render_aligned(
        text,
        Point::new(right_x, y),
        VerticalPosition::Top,
        HorizontalAlignment::Right,
        FontColor::Transparent(color),
        display,
    );
}

/// Measure the horizontal advance width of `text` rendered with
/// `font`. Returns 0 if the measurement fails (which only happens
/// for malformed/missing glyphs).
pub fn measure_width(font: &FontRenderer, text: &str) -> i32 {
    font.get_rendered_dimensions(text, Point::zero(), VerticalPosition::Top)
        .map(|d| d.advance.x)
        .unwrap_or(0)
}
