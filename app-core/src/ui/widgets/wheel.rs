//! Vertical scroll-wheel for picking a bounded integer.
//!
//! Visual: a stack of cells. The center cell is the selection
//! (accent colour, larger font); cells either side dim with
//! distance. Top and bottom hairlines bracket the selection band.
//!
//! Interaction: drag tracks finger travel 1:1, snapping to cells.
//! Release snaps any sub-cell drift back to the grid. Tap above
//! the band steps -1; tap below steps +1; tap on the band is a
//! no-op. Optional wrap mode rolls modularly across the range
//! (HH 23 -> 00); without it the value clamps at the ends.
//!
//! Composition: a single `Wheel` owns one column. Multi-column
//! pickers (HH:MM, HH:MM:SS, DD/MM/YYYY) use [`super::picker::Picker`]
//! which routes drags to whichever wheel started the gesture.

use core::fmt::Write;

use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{Line, PrimitiveStyle, Rectangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

use crate::ui::{fonts, theme};

/// Vertical pitch of one wheel cell. Drag travel is 1:1 with cell
/// advance, so a finger moving up by `WHEEL_CELL_H` advances the
/// value by 1.
pub const WHEEL_CELL_H: i32 = 40;

/// Number of neighbour cells rendered above and below the center
/// cell. Total visible count is `2 * VISIBLE_NEIGHBORS + 1`.
const VISIBLE_NEIGHBORS: i32 = 2;

/// Total height the widget needs to render comfortably (5 cells).
pub const WHEEL_TOTAL_H: i32 = (2 * VISIBLE_NEIGHBORS + 1) * WHEEL_CELL_H;

/// Bounded-integer scroll wheel. Owns its current value plus the
/// per-gesture drag state.
pub struct Wheel {
    min: i32,
    max: i32,
    wrap: bool,
    value: i32,
    /// Sub-cell drag offset in pixels, in `[-WHEEL_CELL_H/2 .. WHEEL_CELL_H/2)`.
    /// Reset to 0 on release.
    visual_offset: i32,
    /// Last finger y observed during the active drag, or `None` when
    /// no drag is in progress.
    last_drag_y: Option<i32>,
}

impl Wheel {
    /// Wheel ranging over `min..=max` with the given starting value.
    /// Value is clamped into the range.
    pub fn new(min: i32, max: i32, value: i32) -> Self {
        Self {
            min,
            max,
            wrap: false,
            value: value.clamp(min, max),
            visual_offset: 0,
            last_drag_y: None,
        }
    }

    /// Enable modular wrap-around (e.g. minutes 0..=59 wrap from
    /// 59 -> 0 when scrolled forward). Default is clamp at ends.
    pub fn with_wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Currently selected value, snapped to the integer grid.
    pub fn value(&self) -> i32 {
        self.value
    }

    /// Last finger y observed during the active drag, or `None`
    /// when no drag is in progress. Exposed for the picker layer
    /// so it can measure total displacement before [`release`] clears
    /// the field.
    pub fn last_drag_y(&self) -> Option<i32> {
        self.last_drag_y
    }

    /// Replace the current value, clamping/wrapping into range and
    /// dropping any in-flight drag state.
    pub fn set_value(&mut self, v: i32) {
        self.value = self.normalise(v);
        self.visual_offset = 0;
        self.last_drag_y = None;
    }

    /// Update the allowed range (e.g. day-of-month range when the
    /// month or year changes). Re-clamps the current value into the
    /// new range. Any drag in progress is cancelled.
    ///
    /// No-op when min and max are unchanged - callers like the date
    /// picker run this on every touch event to track month/year
    /// changes, and resetting drag state every time would prevent
    /// the day wheel itself from ever accumulating a drag.
    pub fn set_range(&mut self, min: i32, max: i32) {
        if self.min == min && self.max == max {
            return;
        }
        self.min = min;
        self.max = max;
        self.value = self.value.clamp(min, max);
        self.visual_offset = 0;
        self.last_drag_y = None;
    }

    /// Process a `TouchPressed` y-coordinate. The first call of a
    /// gesture just records the starting y. Subsequent calls
    /// translate finger motion into value steps + sub-cell offset.
    /// Returns `true` when the displayed state changed.
    pub fn drag(&mut self, y: i32) -> bool {
        let last = match self.last_drag_y {
            None => {
                self.last_drag_y = Some(y);
                return false;
            }
            Some(l) => l,
        };
        if y == last {
            return false;
        }
        self.last_drag_y = Some(y);

        // Finger up (delta < 0) -> value increases. Combine the
        // existing sub-cell offset with the new delta and split into
        // (whole cells, remainder centered on zero).
        let delta = y - last;
        let total = self.visual_offset - delta;
        let half = WHEEL_CELL_H / 2;
        let mut cells = total.div_euclid(WHEEL_CELL_H);
        let mut remainder = total.rem_euclid(WHEEL_CELL_H);
        if remainder >= half {
            cells += 1;
            remainder -= WHEEL_CELL_H;
        }

        let new_value = self.normalise(self.value + cells);
        // If clamping ate the cell movement, also pin the visual
        // offset to 0 so the wheel doesn't jiggle past the end stop.
        let pinned = !self.wrap
            && (new_value == self.min && total < 0
                || new_value == self.max && total > 0);
        let new_offset = if pinned { 0 } else { remainder };

        let changed = new_value != self.value || new_offset != self.visual_offset;
        self.value = new_value;
        self.visual_offset = new_offset;
        changed
    }

    /// Snap any sub-cell drag offset back to the grid and end the
    /// active drag. Returns `true` when the visual changed.
    pub fn release(&mut self) -> bool {
        let changed = self.visual_offset != 0 || self.last_drag_y.is_some();
        self.visual_offset = 0;
        self.last_drag_y = None;
        changed
    }

    /// Tap-step: tap above the center band steps -1, below steps +1,
    /// inside the band is a no-op. `rect` is the wheel's render rect.
    /// Returns `true` if the value changed.
    pub fn tap(&mut self, rect: Rectangle, y: i32) -> bool {
        let cy = rect.top_left.y + rect.size.height as i32 / 2;
        let band_top = cy - WHEEL_CELL_H / 2;
        let band_bot = cy + WHEEL_CELL_H / 2;
        let new_value = if y < band_top {
            self.normalise(self.value - 1)
        } else if y >= band_bot {
            self.normalise(self.value + 1)
        } else {
            return false;
        };
        if new_value != self.value {
            self.value = new_value;
            true
        } else {
            false
        }
    }

    /// Render the wheel into `rect`. `format` writes each cell's
    /// display string into the supplied buffer (e.g. zero-padded
    /// `{:02}`, or a day-name lookup). `accent` is the highlight
    /// colour for the center cell and selection-band hairlines.
    pub fn render<D, F>(
        &self,
        display: &mut D,
        rect: Rectangle,
        accent: Color,
        mut format: F,
    ) where
        D: BlendTarget,
        F: FnMut(i32, &mut heapless::String<16>),
    {
        let cy = rect.top_left.y + rect.size.height as i32 / 2;

        for i in -VISIBLE_NEIGHBORS..=VISIBLE_NEIGHBORS {
            // Out-of-range cells are blanked when wrap is off.
            let raw = self.value + i;
            let cell_value = if self.wrap {
                self.normalise(raw)
            } else if raw < self.min || raw > self.max {
                continue;
            } else {
                raw
            };

            // visual_offset is the cumulative finger displacement
            // since the last snap (positive = stack pulled up).
            // Cells render at their natural y minus that offset so
            // the strip tracks the finger 1:1 between snaps.
            let cell_cy = cy + i * WHEEL_CELL_H - self.visual_offset;
            let cell_rect = Rectangle::new(
                Point::new(rect.top_left.x, cell_cy - WHEEL_CELL_H / 2),
                Size::new(rect.size.width, WHEEL_CELL_H as u32),
            );

            let abs = i.abs();
            let color = if abs == 0 {
                accent
            } else if abs == 1 {
                theme::FG_MUTED
            } else {
                theme::FG_DIM
            };

            let mut buf: heapless::String<16> = heapless::String::new();
            format(cell_value, &mut buf);
            if abs == 0 {
                fonts::draw_centered_in_rect(display, &fonts::value(), &buf, cell_rect, color);
            } else {
                fonts::draw_centered_in_rect(display, &fonts::body(), &buf, cell_rect, color);
            }
        }

        // Selection-band hairlines. Inset slightly so they don't
        // butt up against the wheel's left/right edges.
        let band_top = cy - WHEEL_CELL_H / 2;
        let band_bot = cy + WHEEL_CELL_H / 2;
        let line_x0 = rect.top_left.x + 4;
        let line_x1 = rect.top_left.x + rect.size.width as i32 - 4;
        let style = PrimitiveStyle::with_stroke(accent, 1);
        Line::new(Point::new(line_x0, band_top), Point::new(line_x1, band_top))
            .into_styled(style)
            .draw(display)
            .ok();
        Line::new(Point::new(line_x0, band_bot), Point::new(line_x1, band_bot))
            .into_styled(style)
            .draw(display)
            .ok();
    }

    /// Apply wrap/clamp semantics to a candidate value.
    fn normalise(&self, v: i32) -> i32 {
        if self.wrap {
            let span = self.max - self.min + 1;
            self.min + (v - self.min).rem_euclid(span)
        } else {
            v.clamp(self.min, self.max)
        }
    }
}

/// Convenience helper for the most common formatter: zero-padded
/// two-digit decimal. Use as `wheel.render(.., .., .., fmt_2digit)`.
pub fn fmt_2digit(v: i32, buf: &mut heapless::String<16>) {
    let _ = write!(buf, "{:02}", v);
}

/// Convenience helper for unpadded decimal (years, large numbers).
#[allow(dead_code)]
pub fn fmt_decimal(v: i32, buf: &mut heapless::String<16>) {
    let _ = write!(buf, "{}", v);
}
