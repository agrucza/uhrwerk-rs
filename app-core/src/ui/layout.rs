//! Shared layout grammar for all screens.
//!
//! These constants and helpers define the positional defaults
//! screens use - bottom-tile row geometry, scrollbar placement, the
//! `VStack` / `ScrollState` cursors. Screens import from here so the
//! visual rhythm stays consistent.
//!
//! Split from `theme` on purpose:
//!
//! * [`theme`] is about the **visual language**: palette, fonts,
//!   physical screen constants, bezel geometry. Colors and sizes
//!   of things.
//! * [`layout`] is about **where things go**: rect helpers,
//!   content positioning, standard screen geometry. Composition
//!   of things.
//!
//! A palette tweak doesn't force re-reading this file, and a layout
//! tweak doesn't force re-reading the palette. Screens pulling in
//! `use crate::ui::layout` get the whole grammar at once.
//!
//! [`theme`]: super::theme

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};

use crate::ui::theme;

/// Half-open tap hit test shared by every screen's dispatch:
/// inside means `[top_left, top_left + size)`. Accepts the raw
/// `u16` touch coordinates or already-widened `i32`s.
pub fn rect_hit(rect: Rectangle, x: impl Into<i32>, y: impl Into<i32>) -> bool {
    let px: i32 = x.into();
    let py: i32 = y.into();
    let rx = rect.top_left.x;
    let ry = rect.top_left.y;
    px >= rx
        && px < rx + rect.size.width as i32
        && py >= ry
        && py < ry + rect.size.height as i32
}

// -- Bottom tile row ---------------------------------------------------------

/// Side padding for the bottom tile row.
pub const BOTTOM_TILE_PAD_X: i32 = 22;

/// Height of a tile in the bottom tile row.
pub const BOTTOM_TILE_H: i32 = 38;

/// Horizontal gap between adjacent bottom tiles.
pub const BOTTOM_TILE_GAP: i32 = 6;

/// Y of the top edge of the bottom tile row. Anchored against the
/// shared [`theme::BOTTOM_SAFE_MARGIN`] so tiles, CTA buttons, and
/// other bottom-parked controls share one baseline.
pub const BOTTOM_TILE_Y: i32 =
    theme::SCREEN_H as i32 - theme::BOTTOM_SAFE_MARGIN - BOTTOM_TILE_H;

/// Return `N` evenly-split slot rects for a screen's bottom tile row,
/// padded by [`BOTTOM_TILE_PAD_X`] on each side and separated by
/// [`BOTTOM_TILE_GAP`]. Pair each rect with
/// [`crate::ui::widgets::info_tile`] (or anything else of matching
/// height) to fill the row.
///
/// Panics in debug builds on `N == 0`; release builds return an empty
/// array.
pub fn bottom_tile_row<const N: usize>() -> [Rectangle; N] {
    debug_assert!(N > 0, "bottom_tile_row needs at least one cell");
    let total_w = theme::SCREEN_W as i32 - BOTTOM_TILE_PAD_X * 2;
    let cell_w = if N == 0 {
        0
    } else {
        (total_w - BOTTOM_TILE_GAP * (N as i32 - 1)) / N as i32
    };
    core::array::from_fn(|i| {
        Rectangle::new(
            Point::new(
                BOTTOM_TILE_PAD_X + i as i32 * (cell_w + BOTTOM_TILE_GAP),
                BOTTOM_TILE_Y,
            ),
            Size::new(cell_w as u32, BOTTOM_TILE_H as u32),
        )
    })
}

// -- VStack vertical-stack cursor --------------------------------------------

/// Side margin shared by the standard centered content band on every
/// sub-view (Battery / IMU / Storage / Vitals / Notifications / ...).
/// Picked so the band clears the 98 px bezel-corner arc at the rows'
/// y-band.
pub const VSTACK_SIDE_MARGIN: i32 = 28;

/// Vertical-stack layout cursor.
///
/// State: the next available y-coordinate. Each call advances the
/// cursor by the requested height, returns the rect at that slot,
/// and leaves `next_y` ready for the following slot.
///
/// Render and event handlers create a VStack with the same `top_y`
/// and call the same sequence of methods; both sides see identical
/// rects, so an event-side hit-test can never drift from the
/// render-side draw rect.
///
/// ```ignore
/// // Same call sequence in render and event handlers:
/// let mut s = VStack::new(top_y);
/// let panel = s.slot(100);
/// s.gap(18);
/// let (cancel, primary) = s.pair(36, 12);
/// ```
pub struct VStack {
    next_y: i32,
    x: i32,
    width: i32,
}

impl VStack {
    /// Cursor at `top_y` with the standard centered content band
    /// ([`VSTACK_SIDE_MARGIN`] inset on each side).
    pub fn new(top_y: i32) -> Self {
        let width = theme::SCREEN_W as i32 - VSTACK_SIDE_MARGIN * 2;
        Self {
            next_y: top_y,
            x: VSTACK_SIDE_MARGIN,
            width,
        }
    }

    /// Cursor with a caller-supplied side margin. Use this when a
    /// sub-view needs a wider or narrower band than the default.
    pub fn with_margin(top_y: i32, side_margin: i32) -> Self {
        let width = theme::SCREEN_W as i32 - side_margin * 2;
        Self { next_y: top_y, x: side_margin, width }
    }

    /// Cursor scoped to the interior of `parent`, inset horizontally
    /// by `inset_x` and starting at `top_y`. Use this when an inner
    /// stack of items lives inside a chamfered panel - the inner
    /// VStack's `slot` / `pair` / `row` methods then return rects
    /// within the panel automatically, so callers don't recompute
    /// `panel.x + inset` math at every call site.
    pub fn inside(parent: Rectangle, inset_x: i32, top_y: i32) -> Self {
        Self {
            next_y: top_y,
            x: parent.top_left.x + inset_x,
            width: parent.size.width as i32 - inset_x * 2,
        }
    }

    /// Current cursor y. Lets a caller chain a second [`VStack`] at
    /// the bottom of the first (e.g. switch from a margined band to
    /// a full-width band) without recomputing total heights by hand.
    pub fn cursor_y(&self) -> i32 {
        self.next_y
    }

    /// Advance by `height` and return a full-width rect at that slot.
    pub fn slot(&mut self, height: i32) -> Rectangle {
        let r = Rectangle::new(
            Point::new(self.x, self.next_y),
            Size::new(self.width as u32, height as u32),
        );
        self.next_y += height;
        r
    }

    /// Advance by `height` and return two side-by-side half-width
    /// rects with `gap_x` between them. Left rect first, right
    /// second.
    pub fn pair(&mut self, height: i32, gap_x: i32) -> (Rectangle, Rectangle) {
        let half = (self.width - gap_x) / 2;
        let left = Rectangle::new(
            Point::new(self.x, self.next_y),
            Size::new(half as u32, height as u32),
        );
        let right = Rectangle::new(
            Point::new(self.x + half + gap_x, self.next_y),
            Size::new(half as u32, height as u32),
        );
        self.next_y += height;
        (left, right)
    }

    /// Advance by `height` and return `N` evenly-split cells laid
    /// out horizontally, with `gap_x` between adjacent cells. The
    /// generalised form of [`Self::pair`] for buttons-in-a-row /
    /// segmented-control-style layouts. `N` is a const generic so
    /// the array shape is part of the call site - destructure or
    /// index directly without a length check.
    ///
    /// Panics in debug builds on `N == 0`; release builds return
    /// an empty array.
    pub fn row<const N: usize>(
        &mut self, height: i32, gap_x: i32,
    ) -> [Rectangle; N] {
        debug_assert!(N > 0, "VStack::row needs at least one cell");
        let cell_w = if N == 0 {
            0
        } else {
            (self.width - gap_x * (N as i32 - 1)) / N as i32
        };
        let y = self.next_y;
        let cells = core::array::from_fn(|i| {
            Rectangle::new(
                Point::new(self.x + i as i32 * (cell_w + gap_x), y),
                Size::new(cell_w as u32, height as u32),
            )
        });
        self.next_y += height;
        cells
    }

    /// Advance vertically without producing a rect.
    pub fn gap(&mut self, height: i32) {
        self.next_y += height;
    }
}

// -- ScrollState -------------------------------------------------------------

/// Vertical-scroll state for a screen (or sub-region of a screen)
/// whose content can be longer than its viewport.
///
/// Owned per scrollable region - a screen with two independent
/// scroll areas holds two `ScrollState`s. Drag tracking is internal:
/// the screen's event handler forwards `TouchPressed { y }` deltas
/// to [`ScrollState::drag`] and `TouchReleased` to
/// [`ScrollState::release`].
///
/// The viewport rect is owned by the screen (its position depends
/// on chrome geometry above and below); only the 1-D vertical
/// offset and drag math live here. The current `max` (typically
/// `content_h - viewport_h`) is passed into [`Self::drag`] each
/// call, so the state struct never has to be mutated from `render`,
/// where the screen is `&self`.
///
/// ## Usage sketch
///
/// ```ignore
/// // In on_event:
/// SystemEvent::TouchPressed { y, .. } => {
///     let max = (content_h - viewport_h).max(0);
///     if scroll.drag(*y as i32, max) { return Action::Redraw; }
/// }
/// SystemEvent::TouchReleased => scroll.release(),
///
/// // In render:
/// // ... draw fixed chrome ...
/// let mut clipped = BlendClipped::new(display, &viewport_rect);
/// // ... draw scrollable content with y - scroll.offset() ...
/// ```
#[derive(Debug, Default)]
pub struct ScrollState {
    offset: i32,
    last_drag_y: Option<i32>,
}

impl ScrollState {
    /// Fresh state - offset 0, no active drag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current scroll offset in pixels. 0 = top of content aligned
    /// with viewport top. Positive = content scrolled up (rows
    /// below come into view).
    pub fn offset(&self) -> i32 {
        self.offset
    }

    /// Process a `TouchPressed` during a drag, clamped to
    /// `0..=max`. The first call of a gesture just records the
    /// starting y; subsequent calls translate finger motion into
    /// scroll-offset deltas (drag up = scroll forward into content).
    /// Returns `true` if the offset changed (the screen should
    /// redraw).
    pub fn drag(&mut self, y: i32, max: i32) -> bool {
        let max = max.max(0);
        match self.last_drag_y {
            None => {
                self.last_drag_y = Some(y);
                // Clamp once on touch-down in case `max` shrank
                // since the last gesture.
                let new_offset = self.offset.clamp(0, max);
                if new_offset != self.offset {
                    self.offset = new_offset;
                    return true;
                }
                false
            }
            Some(last) => {
                let delta = y - last;
                self.last_drag_y = Some(y);
                let new_offset = (self.offset - delta).clamp(0, max);
                if new_offset != self.offset {
                    self.offset = new_offset;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Reset drag tracking on `TouchReleased`. The offset stays
    /// where the finger left it (no inertia / momentum).
    pub fn release(&mut self) {
        self.last_drag_y = None;
    }
}
