//! Content body layouts - what goes inside containers.
//!
//! Body helpers take a rect (or center point) and draw a specific
//! content layout into it. They don't own state and don't draw
//! backgrounds, so screens can compose them with any container or
//! use them standalone on a bare rect.

use embedded_graphics::{
    geometry::Point,
    prelude::Primitive,
    primitives::{Line, PrimitiveStyle, Rectangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

use crate::ui::{fonts, theme};

use super::controls::{toggle, TOGGLE_H, TOGGLE_W};

// -- row ---------------------------------------------------------------------

/// Height of one settings-style row.
pub const ROW_H: i32 = 52;

/// Horizontal padding inside a row (left and right edges).
pub const ROW_PAD: i32 = 18;

/// Icon column width. Label starts after `ROW_PAD + ROW_ICON_COL_W`.
pub const ROW_ICON_COL_W: i32 = 40;

/// Right-side control on a `row`. Keeps the hot path allocation-free:
/// callers pick a variant and the renderer picks the draw code.
pub enum RowControl<'a> {
    /// Right-pointing chevron. Signals "tap to navigate".
    Chevron(Color),
    /// Toggle switch (on/off state).
    Toggle(bool),
    /// Short inline text (e.g. `STABLE`, `14/32K`).
    Inline(&'a str, Color),
}

/// Draw one settings-style row inside `rect`.
///
/// Layout:
/// - 16 px icon (caller-supplied closure), left column, vertically centered.
/// - Uppercase label in `FG`, starting `ROW_ICON_COL_W` px past the icon column.
/// - Right control per `control`, right-aligned to `rect.right - ROW_PAD`.
/// - 1 px steel hairline along the full width of the bottom.
pub fn row<D, F>(
    display: &mut D,
    rect: Rectangle,
    icon: F,
    icon_color: Color,
    label: &str,
    control: RowControl,
)
where
    D: BlendTarget,
    F: FnOnce(&mut D, i32, i32, Color),
{
    let x = rect.top_left.x;
    let y = rect.top_left.y;
    let w = rect.size.width as i32;
    let h = rect.size.height as i32;
    let cy = y + h / 2;

    let icon_cx = x + ROW_PAD + 8;
    icon(display, icon_cx, cy, icon_color);

    let label_font = fonts::body();
    let label_h = 14;
    fonts::draw_at(
        display, &label_font, label,
        x + ROW_PAD + ROW_ICON_COL_W, cy - label_h / 2,
        theme::FG,
    );

    match control {
        RowControl::Chevron(color) => {
            let right_x = x + w - ROW_PAD;
            let stroke = PrimitiveStyle::with_stroke(color, 2);
            Line::new(
                Point::new(right_x - 6, cy - 5),
                Point::new(right_x, cy),
            ).into_styled(stroke).draw(display).ok();
            Line::new(
                Point::new(right_x, cy),
                Point::new(right_x - 6, cy + 5),
            ).into_styled(stroke).draw(display).ok();
        }
        RowControl::Toggle(on) => {
            let top = Point::new(
                x + w - ROW_PAD - TOGGLE_W,
                cy - TOGGLE_H / 2,
            );
            toggle(display, top, on);
        }
        RowControl::Inline(text, color) => {
            // Match the label's body font (helvR14) so both sides of
            // the row read at the same weight.
            let font = fonts::body();
            fonts::draw_right(
                display, &font, text,
                x + w - ROW_PAD, cy - 7,
                color,
            );
        }
    }

    Line::new(
        Point::new(x, y + h - 1),
        Point::new(x + w - 1, y + h - 1),
    ).into_styled(PrimitiveStyle::with_stroke(theme::STEEL, 1))
    .draw(display).ok();
}
