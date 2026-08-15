//! Container widgets - surface shapes that content lives inside.
//!
//! Every container is a chamfered Nightwatch surface: a 6-line
//! outline with 45-degree notches on the TL and BR corners.
//! Variants:
//!
//! * `chamfered_panel` - bare panel outline, the building block.
//! * `tile` - app-grid / quick-access tile (icon + caption inside
//!   a chamfered border).
//! * `info_tile` - leading glyph + value + suffix in a single short
//!   tile, paired with `layout::bottom_tile_row` for N-up bottom
//!   bands like the watch face's heart-rate / unread-count tiles.
//! * `tag_label` - flag-shaped label ribbon, optionally chamfered to
//!   nest into a parent panel's TL chamfer.
//!
//! Containers never draw their own content - body helpers and screen
//! code place content into the same rect after the container is drawn.

use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{Line, PrimitiveStyle, Rectangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

use crate::ui::{fonts, theme};

// -- Widget-local layout constants -------------------------------------------

/// Chamfer notch size for Nightwatch panels and tiles. Matches the
/// spec's default 10 px corner cut.
pub const NOTCH: i32 = 10;

/// Height of a tag-label ribbon.
pub const TAG_LABEL_H: i32 = 15;

/// Blend strength for the anti-aliasing pixels laid along 45-degree
/// chamfer edges (~43% coverage - the elbow of each stair step).
const DIAG_AA_ALPHA: u8 = 110;

// -- Chamfer anti-aliasing ---------------------------------------------------

/// Soften a 45-degree diagonal running from `(sx, sy)` in the
/// `(+1, -1)` direction for `len` steps (both Nightwatch chamfer
/// diagonals - TL and BR - have this orientation). Blends `color`
/// into the two "elbow" pixels of every stair step, turning the hard
/// staircase into an anti-aliased edge. Call after drawing the
/// diagonal itself.
fn soften_diag<D: BlendTarget>(display: &mut D, sx: i32, sy: i32, len: i32, color: Color) {
    for i in 0..len {
        display.blend_pixel(sx + i + 1, sy - i, color, DIAG_AA_ALPHA);
        display.blend_pixel(sx + i, sy - i - 1, color, DIAG_AA_ALPHA);
    }
}

// -- chamfered_panel ---------------------------------------------------------

/// Draw the 6-line outline of a chamfered hex panel: a rectangle with
/// `notch` px cut off the top-left and bottom-right corners.
///
/// Traces outline only - no fill. Screens that want a filled interior
/// fill a plain `Rectangle` first.
///
/// ```text
///       notch
///      ┌────────────────┐
///     ╱                 │
///    │                  │
///    │                  │
///    │                 ╱
///    └────────────────┘
///                notch
/// ```
pub fn chamfered_panel<D: BlendTarget>(
    display: &mut D,
    rect: Rectangle,
    notch: i32,
    color: Color,
    stroke_width: u32,
) {
    let x = rect.top_left.x;
    let y = rect.top_left.y;
    let w = rect.size.width as i32;
    let h = rect.size.height as i32;
    let r = x + w - 1;
    let b = y + h - 1;

    let style = PrimitiveStyle::with_stroke(color, stroke_width);

    Line::new(Point::new(x + notch, y), Point::new(r, y))
        .into_styled(style).draw(display).ok();
    Line::new(Point::new(r, y), Point::new(r, b - notch))
        .into_styled(style).draw(display).ok();
    Line::new(Point::new(r, b - notch), Point::new(r - notch, b))
        .into_styled(style).draw(display).ok();
    Line::new(Point::new(r - notch, b), Point::new(x, b))
        .into_styled(style).draw(display).ok();
    Line::new(Point::new(x, b), Point::new(x, y + notch))
        .into_styled(style).draw(display).ok();
    Line::new(Point::new(x, y + notch), Point::new(x + notch, y))
        .into_styled(style).draw(display).ok();

    // Anti-alias the two 45-degree diagonals; the four straight edges
    // are pixel-exact already.
    soften_diag(display, x, y + notch, notch, color);
    soften_diag(display, r - notch, b, notch, color);
}

// -- tag_label ---------------------------------------------------------------

/// Draw a tag-label flag - a filled accent-colored rectangle with a
/// chamfered bottom-right corner (the classic "flag" shape) and an
/// optional chamfered top-left corner so the tag fits flush against
/// a parent panel's matching TL chamfer.
///
/// `left_x` / `top_y` are the flag's top-left corner. `tl_notch = 0`
/// gives a square TL (tag hangs beside a panel's chamfer); `tl_notch
/// > 0` carves a matching chamfer so the tag can nest inside a
/// chamfered panel corner (pass the panel's own `NOTCH`).
///
/// Text is always drawn in black so it reads as printed on the
/// colored ribbon.
pub fn tag_label<D: BlendTarget>(
    display: &mut D,
    left_x: i32, top_y: i32,
    text: &str,
    color: Color,
    tl_notch: i32,
) {
    let font = fonts::caption();
    let text_w = fonts::measure_width(&font, text);
    let w = text_w + 12 + tl_notch;
    let h = TAG_LABEL_H;

    // Ribbon body: subtle top-lit vertical gradient instead of a flat
    // fill, dithered so the 15 px ramp shows no banding.
    display.fill_vgradient(
        left_x, top_y, w, h,
        color, theme::dimmed(color, 185),
    );

    let br_chamfer = 5i32;
    let r = left_x + w - 1;
    let b = top_y + h - 1;
    for i in 0..br_chamfer {
        Line::new(
            Point::new(r - i, b),
            Point::new(r, b - i),
        )
        .into_styled(PrimitiveStyle::with_stroke(theme::BG, 1))
        .draw(display).ok();
    }
    // Soften the carved edge: blend BG into the ribbon pixels just
    // inside the cut so the color/black boundary is anti-aliased.
    soften_diag(display, r - br_chamfer, b, br_chamfer, theme::BG);

    if tl_notch > 0 {
        for i in 0..tl_notch {
            Line::new(
                Point::new(left_x + i, top_y),
                Point::new(left_x, top_y + i),
            )
            .into_styled(PrimitiveStyle::with_stroke(theme::BG, 1))
            .draw(display).ok();
        }
        // Same softening for the TL nesting chamfer. This diagonal
        // runs from (left_x, top_y + tl_notch - 1) up-right, matching
        // soften_diag's `(+1, -1)` orientation.
        soften_diag(display, left_x, top_y + tl_notch - 1, tl_notch - 1, theme::BG);
    }

    let text_rect = Rectangle::new(
        Point::new(left_x + tl_notch / 2, top_y),
        Size::new((w - tl_notch / 2) as u32, h as u32),
    );
    fonts::draw_centered_in_rect(display, &font, text, text_rect, theme::BG);
}

// -- tile --------------------------------------------------------------------

/// Draw a chamfered tile suitable for app-grid / toggle-grid use: hex
/// outline in `border` color with an icon + caption inside.
///
/// `stroke_width` controls border thickness - pass 1 for a regular
/// tile, 2 to emphasise the tile as active ("launched from here")
/// without changing the color.
pub fn tile<D, F>(
    display: &mut D,
    rect: Rectangle,
    border: Color,
    stroke_width: u32,
    icon: F,
    icon_color: Color,
    caption: &str,
)
where
    D: BlendTarget,
    F: FnOnce(&mut D, i32, i32, Color),
{
    chamfered_panel(display, rect, NOTCH, border, stroke_width);

    let x = rect.top_left.x;
    let y = rect.top_left.y;
    let w = rect.size.width as i32;
    let h = rect.size.height as i32;

    let icon_cx = x + w / 2;
    let icon_cy = y + h * 42 / 100;
    icon(display, icon_cx, icon_cy, icon_color);

    let font = fonts::caption();
    fonts::draw_centered(
        display, &font, caption,
        icon_cx, y + h - 18,
        theme::FG,
    );
}

// -- info_tile ---------------------------------------------------------------

/// Chamfer notch for an `info_tile`. One step smaller than the panel
/// `NOTCH` so the chamfer reads at the tile's compact height.
pub const INFO_TILE_NOTCH: i32 = NOTCH - 2;

/// Glyph radius for an `info_tile`'s leading icon.
const INFO_TILE_ICON_R: i32 = 9;

/// Inner horizontal padding for an `info_tile`.
const INFO_TILE_PAD: i32 = 14;

/// Draw a chamfered info tile: leading glyph on the left, value text
/// after it, right-aligned caption suffix. Border + value share
/// `accent`; the suffix renders in `theme::FG_MUTED`.
///
/// Pair with [`crate::ui::layout::bottom_tile_row`] for the watch
/// face's bottom-tile band or any future N-up info row.
pub fn info_tile<D, F>(
    display: &mut D,
    rect: Rectangle,
    icon: F,
    value: &str,
    suffix: &str,
    accent: Color,
)
where
    D: BlendTarget,
    F: FnOnce(&mut D, i32, i32, i32, Color),
{
    chamfered_panel(display, rect, INFO_TILE_NOTCH, accent, 1);

    let x = rect.top_left.x;
    let y = rect.top_left.y;
    let w = rect.size.width as i32;
    let h = rect.size.height as i32;
    let cy = y + h / 2;

    let icon_cx = x + INFO_TILE_PAD + 6;
    icon(display, icon_cx, cy, INFO_TILE_ICON_R, accent);

    let val_font = fonts::body();
    let suf_font = fonts::caption();
    fonts::draw_at(
        display, &val_font, value,
        x + INFO_TILE_PAD + 22, cy - 8, accent,
    );
    fonts::draw_right(
        display, &suf_font, suffix,
        x + w - INFO_TILE_PAD, cy - 6, theme::FG_MUTED,
    );
}
