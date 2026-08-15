//! Interactive control widgets - toggles, sliders, chamfered buttons.
//!
//! Controls are the smallest interactive primitives. They know how to
//! draw themselves in each visual state (off/on, pressed, disabled)
//! but don't track state themselves - callers pass the current state
//! each render.

use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{Line, PrimitiveStyle, Rectangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

use crate::ui::{fonts, theme};
use crate::ui::widgets::containers::chamfered_panel;

// -- toggle ------------------------------------------------------------------

/// Toggle outer width, per the Nightwatch spec (32 x 16 px).
pub const TOGGLE_W: i32 = 32;
/// Toggle outer height.
pub const TOGGLE_H: i32 = 16;

/// Draw a toggle switch at the given top-left.
///
/// - Off: elevated-surface trough, `BORDER` border, `FG_DIM` pill
///   flush-left.
/// - On: `ACCENT` trough and border, `BG` pill flush-right.
///
/// Troughs are top-lit vertical gradients (dithered) so the control
/// reads as a lit tube rather than a flat chip.
pub fn toggle<D: BlendTarget>(
    display: &mut D,
    top_left: Point,
    on: bool,
) {
    let (trough_top, trough_bottom, border, pill) = if on {
        (theme::ACCENT, theme::dimmed(theme::ACCENT, 165), theme::ACCENT, theme::BG)
    } else {
        (theme::SURFACE_3, theme::SURFACE, theme::BORDER, theme::FG_DIM)
    };

    display.fill_vgradient(
        top_left.x, top_left.y, TOGGLE_W, TOGGLE_H,
        trough_top, trough_bottom,
    );
    Rectangle::new(top_left, Size::new(TOGGLE_W as u32, TOGGLE_H as u32))
        .into_styled(PrimitiveStyle::with_stroke(border, 1))
        .draw(display).ok();

    let pill_size = 12i32;
    let pill_x = if on {
        top_left.x + TOGGLE_W - pill_size - 1
    } else {
        top_left.x + 1
    };
    let pill_y = top_left.y + (TOGGLE_H - pill_size) / 2;
    Rectangle::new(
        Point::new(pill_x, pill_y),
        Size::new(pill_size as u32, pill_size as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(pill))
    .draw(display).ok();
}

// -- slider ------------------------------------------------------------------

/// Suggested height for a slider trough. Callers pick their own
/// rect; this is the value Quick Access and the Display sub-view
/// both use, kept here so any new slider lands at the same height
/// without re-deriving the magic number.
pub const SLIDER_BAR_H: i32 = 12;

/// Vertical slack above and below the trough accepted as a slider
/// hit. Lets the user drag slightly outside the bar without losing
/// the gesture.
pub const SLIDER_VSLOP: i32 = 12;

/// Vertical offset of the slider's value label above the trough.
/// The label draws at `rect.top_left.y - SLIDER_LABEL_OFFSET`.
const SLIDER_LABEL_OFFSET: i32 = 6;

/// Draw a horizontal slider into `rect`.
///
/// The trough fills the full `rect`; a signal-coloured fill grows
/// from the left to represent `value` against `min..=max`. So when
/// `value == max`, the fill spans the whole bar; when `value <= min`
/// the bar reads as empty. `min` and `max` belong to the caller's
/// problem domain (brightness percent, volume %, anything else
/// linearly mappable) - the widget is value-agnostic.
///
/// `value_label`, when provided, draws right-aligned just above the
/// bar so the caller doesn't reinvent the "format value + position
/// it relative to the bar" boilerplate at every call site. Pass
/// `None` for a bare bar (e.g. when the surrounding panel already
/// shows the value some other way).
pub fn slider<D: BlendTarget>(
    display: &mut D,
    rect: Rectangle,
    value: u8,
    min: u8,
    max: u8,
    value_label: Option<&str>,
) {
    if let Some(label) = value_label {
        fonts::draw_right(
            display, &fonts::caption(),
            label,
            rect.top_left.x + rect.size.width as i32,
            rect.top_left.y - SLIDER_LABEL_OFFSET,
            theme::FG_MUTED,
        );
    }

    // Trough + value fill as top-lit vertical gradients (dithered) -
    // the accent fill reads as a glowing tube against the dark trough.
    display.fill_vgradient(
        rect.top_left.x, rect.top_left.y,
        rect.size.width as i32, rect.size.height as i32,
        theme::SURFACE_3, theme::SURFACE,
    );
    Rectangle::new(rect.top_left, rect.size)
        .into_styled(PrimitiveStyle::with_stroke(theme::BORDER, 1))
        .draw(display).ok();
    let range = (max as i32 - min as i32).max(1);
    let fill_w =
        ((value as i32 - min as i32).max(0) * (rect.size.width as i32 - 2)) / range;
    if fill_w > 0 {
        display.fill_vgradient(
            rect.top_left.x + 1, rect.top_left.y + 1,
            fill_w, rect.size.height as i32 - 2,
            theme::ACCENT, theme::dimmed(theme::ACCENT, 165),
        );
    }
}

/// Hit-test a slider drag. `(x, y)` is the touch point; `rect` is
/// the same rect passed to [`slider`]. Returns the matching value
/// clamped to `min..=max`, or `None` if the touch falls outside the
/// trough's vertical range plus [`SLIDER_VSLOP`] slack on each side.
///
/// The caller decides what `min` / `max` mean - brightness percent,
/// volume, etc. Returning a [`u8`] keeps the widget portable across
/// those problem domains; a u8 spans the practical range of UI
/// sliders without dragging in conversions.
pub fn slider_value_from_x(
    rect: Rectangle,
    x: i32,
    y: i32,
    min: u8,
    max: u8,
) -> Option<u8> {
    let top = rect.top_left.y;
    let bot = top + rect.size.height as i32;
    if y < top - SLIDER_VSLOP || y >= bot + SLIDER_VSLOP {
        return None;
    }
    let left = rect.top_left.x;
    let right = left + rect.size.width as i32;
    let clamped = x.clamp(left, right - 1);
    let range = (max as i32 - min as i32).max(1);
    let frac = (clamped - left) * range / (rect.size.width as i32 - 1);
    Some((min as i32 + frac).clamp(min as i32, max as i32) as u8)
}

// -- chamfered_button --------------------------------------------------------

/// Notch size for chamfered buttons. Smaller than the panel notch
/// (10) so buttons read as a different category of surface.
pub const BUTTON_NOTCH: i32 = 8;

/// Variant of a [`chamfered_button`].
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum ButtonVariant {
    /// Filled accent background, black text. The "primary action"
    /// affordance (PURGE, RESTORE, CONFIRM).
    Primary,
    /// Steel border, transparent body, FG label. The "cancel /
    /// non-destructive" affordance.
    Ghost,
}

/// Draw a chamfered hex button into `rect`.
///
/// `Primary`: interior filled with `accent`, TL+BR corners carved
///   black to expose the chamfer; label drawn in black so it reads
///   as printed on the colored body.
/// `Ghost`: outline-only in steel, label in `theme::FG`.
pub fn chamfered_button<D: BlendTarget>(
    display: &mut D,
    rect: Rectangle,
    label: &str,
    variant: ButtonVariant,
    accent: Color,
) {
    let notch = BUTTON_NOTCH;
    match variant {
        ButtonVariant::Primary => {
            // Fill the whole rect with a top-lit accent gradient, then
            // carve TL and BR chamfer corners back to BG so the hex
            // shape reads.
            display.fill_vgradient(
                rect.top_left.x, rect.top_left.y,
                rect.size.width as i32, rect.size.height as i32,
                accent, theme::dimmed(accent, 175),
            );

            let x = rect.top_left.x;
            let y = rect.top_left.y;
            let r = x + rect.size.width as i32 - 1;
            let b = y + rect.size.height as i32 - 1;
            for i in 0..notch {
                // TL chamfer
                Line::new(
                    Point::new(x + i, y),
                    Point::new(x, y + i),
                )
                .into_styled(PrimitiveStyle::with_stroke(theme::BG, 1))
                .draw(display).ok();
                // BR chamfer
                Line::new(
                    Point::new(r - i, b),
                    Point::new(r, b - i),
                )
                .into_styled(PrimitiveStyle::with_stroke(theme::BG, 1))
                .draw(display).ok();
            }

            // Outline (so the chamfer reads as a sharp edge, not a
            // jagged carve).
            chamfered_panel(display, rect, notch, accent, 1);

            fonts::draw_centered_in_rect(
                display, &fonts::caption(), label, rect, theme::BG,
            );
        }
        ButtonVariant::Ghost => {
            // No fill - just the chamfered outline in steel and the
            // label in FG.
            chamfered_panel(display, rect, notch, theme::BORDER, 1);
            let _ = accent; // unused for Ghost
            fonts::draw_centered_in_rect(
                display, &fonts::caption(), label, rect, theme::FG,
            );
        }
    }
}
