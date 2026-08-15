//! Multi-column wheel picker.
//!
//! `Picker<N>` owns `N` [`Wheel`] columns plus the bookkeeping for
//! routing a touch gesture to whichever column it started on. The
//! screen owns layout (which rect each column occupies) and
//! rendering (per-column formatter, separator glyphs, header chrome
//! and confirm/cancel buttons).
//!
//! Typical use:
//!
//! ```ignore
//! // In the screen's state:
//! let picker = Picker::new([
//!     Wheel::new(0, 23, hour).with_wrap(true),
//!     Wheel::new(0, 59, minute).with_wrap(true),
//! ]);
//!
//! // In on_event:
//! if self.picker.handle_event(event, &cell_rects) {
//!     return Action::Redraw;
//! }
//!
//! // In render:
//! self.picker.wheels[0].render(display, cell_rects[0], ACCENT, fmt_2digit);
//! self.picker.wheels[1].render(display, cell_rects[1], ACCENT, fmt_2digit);
//! ```

use embedded_graphics::{
    geometry::Point, primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

use crate::events::SystemEvent;
use crate::ui::layout;
use crate::ui::widgets::controls::{chamfered_button, ButtonVariant};
use crate::ui::widgets::wheel::Wheel;

/// Pixels of finger displacement at which a gesture stops being a
/// "tap with a tiny jiggle" and becomes a drag for picker
/// purposes. Below the touch driver's `SWIPE_THRESHOLD`, the FT3168
/// emits a trailing `Tap { x, y }` after `TouchReleased` for
/// *every* gesture - including drags within a single wheel column.
/// Without this guard, a 25 px drag would step the wheel via
/// drag *and* step it again (in the opposite direction) when the
/// trailing Tap fires.
const TAP_TOLERANCE_PX: i32 = 8;

/// Multi-wheel value picker. Stores one [`Wheel`] per column plus
/// per-gesture state used to:
/// - keep a drag glued to its starting column even if the finger
///   drifts horizontally (`active_wheel`);
/// - suppress the trailing `Tap` event the touch driver emits for
///   any gesture below `SWIPE_THRESHOLD`, when the gesture was
///   actually a drag (`gesture_start_y` + `suppress_next_tap`).
pub struct Picker<const N: usize> {
    pub wheels: [Wheel; N],
    active_wheel: Option<usize>,
    /// y of the first `TouchPressed` of the current gesture, used
    /// to measure how far the finger has moved.
    gesture_start_y: Option<i32>,
    /// Set on `TouchReleased` if the gesture was a drag. The next
    /// `Tap` consumes and clears this flag without stepping.
    suppress_next_tap: bool,
}

impl<const N: usize> Picker<N> {
    /// Build a picker around `N` pre-configured wheels.
    pub fn new(wheels: [Wheel; N]) -> Self {
        Self {
            wheels,
            active_wheel: None,
            gesture_start_y: None,
            suppress_next_tap: false,
        }
    }

    /// Route a `TouchPressed` / `TouchReleased` / `Tap` event to the
    /// appropriate wheel. `rects` is the caller's layout - one rect
    /// per wheel, in the same order as the `wheels` array. Returns
    /// `true` when a wheel's state changed and the screen should
    /// redraw. Other events are ignored.
    ///
    /// On `TouchPressed`: if no drag is active, the wheel under the
    /// finger becomes active; otherwise the active wheel keeps the
    /// gesture even if the finger has drifted into another column.
    /// On `TouchReleased`: the active wheel snaps to its grid.
    pub fn handle_event(&mut self, event: &SystemEvent, rects: &[Rectangle; N]) -> bool {
        match event {
            SystemEvent::TouchPressed { x, y } => {
                let px = *x as i32;
                let py = *y as i32;
                let idx = match self.active_wheel {
                    Some(i) => i,
                    None => match wheel_at(rects, px, py) {
                        Some(i) => {
                            self.active_wheel = Some(i);
                            self.gesture_start_y = Some(py);
                            i
                        }
                        None => return false,
                    },
                };
                self.wheels[idx].drag(py)
            }
            SystemEvent::TouchReleased => {
                // Decide drag-vs-tap before clearing gesture state.
                if let (Some(i), Some(start_y)) = (self.active_wheel, self.gesture_start_y) {
                    // The finger's last position lives on the wheel
                    // as `last_drag_y` until `release()` clears it.
                    let last_y = self.wheels[i].last_drag_y().unwrap_or(start_y);
                    if (last_y - start_y).abs() > TAP_TOLERANCE_PX {
                        self.suppress_next_tap = true;
                    }
                }
                self.gesture_start_y = None;
                match self.active_wheel.take() {
                    Some(i) => self.wheels[i].release(),
                    None => false,
                }
            }
            SystemEvent::Tap { x, y } => {
                if self.suppress_next_tap {
                    self.suppress_next_tap = false;
                    return false;
                }
                let px = *x as i32;
                let py = *y as i32;
                if let Some(i) = wheel_at(rects, px, py) {
                    return self.wheels[i].tap(rects[i], py);
                }
                false
            }
            _ => false,
        }
    }
}

/// Find the index of the wheel whose rect contains `(x, y)`.
fn wheel_at<const N: usize>(rects: &[Rectangle; N], x: i32, y: i32) -> Option<usize> {
    rects
        .iter()
        .position(|r| r.contains(Point::new(x, y)))
}

// -- Action row --------------------------------------------------------------

/// `(cancel_rect, set_rect)` for the standard picker action row.
/// Same geometry as [`layout::bottom_tile_row`] so picker views
/// share the bottom baseline with stopwatch / timer / settings
/// CTAs. Returned as a tuple (rather than an array) so call sites
/// can name the slots without an index.
pub fn action_row_rects() -> (Rectangle, Rectangle) {
    let [cancel, set] = layout::bottom_tile_row::<2>();
    (cancel, set)
}

/// Draw the standard `CANCEL | SET` action row at the bottom of a
/// picker view. Cancel is always `Ghost`; Set takes the screen's
/// `accent`. Returns the rects so the event handler can hit-test
/// against the same slots without recomputing them.
pub fn render_action_row<D: BlendTarget>(
    display: &mut D,
    accent: Color,
) -> (Rectangle, Rectangle) {
    let (cancel, set) = action_row_rects();
    chamfered_button(display, cancel, "CANCEL", ButtonVariant::Ghost, accent);
    chamfered_button(display, set, "SET", ButtonVariant::Primary, accent);
    (cancel, set)
}
