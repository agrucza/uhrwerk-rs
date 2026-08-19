//! Non-text drawing primitives shared across screens.
//!
//! Text rendering lives in `crate::ui::fonts` (baked Inter alpha
//! atlases). This module is purely shape primitives.
//!
//! - `rounded_panel` - rounded rectangle with optional fill and 1 px
//!   border. Building block for scrollbar tracks/thumbs and the
//!   numpad's pressed-key fill.
//! - `scrollbar_v` - vertical pill-shaped page indicator used by
//!   the page-scrollbar chrome widget.
//! - `battery_color` - status-color picker for a battery percent.

use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{PrimitiveStyleBuilder, Rectangle, RoundedRectangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

// -- Rounded panel -----------------------------------------------------------

/// Draw a rounded rectangle with optional fill and border. Returns the
/// axis-aligned bounding rectangle so callers can lay content inside it.
pub fn rounded_panel<D: BlendTarget>(
    display: &mut D,
    x: i32, y: i32, w: i32, h: i32,
    radius: u32,
    fill: Option<Color>,
    border: Option<Color>,
) -> Rectangle {
    let rect = Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32));
    let rr = RoundedRectangle::with_equal_corners(rect, Size::new(radius, radius));

    let mut sb = PrimitiveStyleBuilder::new();
    if let Some(c) = fill { sb = sb.fill_color(c); }
    if let Some(c) = border { sb = sb.stroke_color(c).stroke_width(1); }
    rr.into_styled(sb.build()).draw(display).ok();

    rect
}

// -- Vertical scrollbar ------------------------------------------------------

/// Smooth-scroll vertical scrollbar drawn at `(x, y, w, h)`.
///
/// Track is a dim pill; thumb is a brighter pill whose height is
/// proportional to `viewport_h / content_h` and whose y position is
/// proportional to `offset / scroll_max`. The thumb is clamped to a
/// minimum height of `w` so the pill shape stays readable even when
/// the content is very long relative to the viewport.
///
/// When the content fits inside the viewport (`content_h <= viewport_h`)
/// the call is a no-op - no scrollbar is needed.
pub fn scrollbar_v<D: BlendTarget>(
    display: &mut D,
    x: i32, y: i32, w: i32, h: i32,
    content_h: i32,
    viewport_h: i32,
    offset: i32,
    active_color: Color,
    dim_color: Color,
) {
    if content_h <= viewport_h || h <= 0 || w <= 0 { return; }
    let radius = (w as u32) / 2;

    // Track
    rounded_panel(display, x, y, w, h, radius, Some(dim_color), None);

    // Thumb height = proportional to visible fraction, clamped to
    // at least one pill-width so the shape stays legible.
    let thumb_h = ((h as i64 * viewport_h as i64) / content_h as i64) as i32;
    let thumb_h = thumb_h.max(w);
    let max_off = (h - thumb_h).max(0);
    let scroll_max = (content_h - viewport_h).max(1);
    let clamped = offset.clamp(0, scroll_max);
    let thumb_y = y + (clamped as i64 * max_off as i64 / scroll_max as i64) as i32;
    rounded_panel(display, x, thumb_y, w, thumb_h, radius, Some(active_color), None);
}

// -- Battery indicators ------------------------------------------------------

/// Pick the right status color for a given battery percentage:
/// bone (neutral) when healthy, yellow as a heads-up, signal red when
/// critical.
/// Slider lower bound for brightness. The hardware can render below
/// this but anything dimmer is unreadable on AMOLED at room light,
/// so both brightness sliders (Quick Access, Settings Display) clip
/// the bottom 5 %.
pub const BRIGHT_MIN_PCT: u8 = 5;

/// Brightness as a percent for the sliders: the hardware 0..=255
/// register mapped back into the `BRIGHT_MIN_PCT..=100` range.
pub fn brightness_pct(data: &crate::ui::types::SystemData) -> u8 {
    let hw = data.config.display.brightness_active as u16;
    ((hw * 100 / 255) as u8).clamp(BRIGHT_MIN_PCT, 100)
}

pub fn battery_color(percent: u8) -> Color {
    use super::theme;
    if percent > 50 { theme::FG }
    else if percent >= 20 { theme::WARN }
    else { theme::DANGER }
}

