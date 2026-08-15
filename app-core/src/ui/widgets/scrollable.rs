//! Scrollable-region helpers.
//!
//! Anything that wants smooth-scroll behaviour (settings index,
//! alarm list, future event-log viewer, ...) goes through this
//! module rather than wiring up a `ScrollState` plus an ad-hoc
//! `BlendClipped` plus a manual `scrollbar_v` call. Two
//! entry points:
//!
//! * [`render_scrolled`] - draws the scrollable body inside a
//!   clipped sub-target and a scroll indicator next to it. Caller
//!   provides a closure that renders rows at their natural y
//!   positions shifted by `-offset`.
//! * [`handle_scroll_drag`] - turns `TouchPressed` / `TouchReleased`
//!   events into scroll-state updates. Returns `true` if the screen
//!   should redraw.
//!
//! The scrollbar runs the full height of the viewport on the right
//! edge so it visually matches the scrollable content area; callers
//! that put a viewport into a y-band that intrudes on the bezel arc
//! accept the same minor edge clipping for the scrollbar.

use embedded_graphics::primitives::Rectangle;
use crate::ui::types::{BlendClipped, BlendTarget};
use crate::ui::theme::Color;

use crate::events::SystemEvent;
use crate::ui::types::RenderCtx;
use crate::ui::{layout, primitives, theme};

/// Width of the scrollbar pill drawn on the right edge of a
/// scrollable viewport.
const SCROLLBAR_W: i32 = 4;

/// Distance from the viewport's right edge to the right edge of the
/// scrollbar. Lifts the bar inward so it doesn't kiss the bezel.
const SCROLLBAR_RIGHT_INSET: i32 = 12;

/// Horizontal gap reserved between the scrollable content's right
/// edge and the scrollbar's left edge, so content doesn't butt up
/// against the bar.
const SCROLLBAR_GAP: i32 = 8;

/// Total horizontal real estate the scrollbar gutter eats from the
/// right edge of a viewport (right inset + bar width + content gap).
/// Callers that draw edge-to-edge content (full-screen-wide rows
/// with right-aligned controls) should subtract this from their
/// content rect's width so internal positioning lands inside the
/// gap rather than under the bar.
pub const SCROLLBAR_GUTTER: i32 = SCROLLBAR_RIGHT_INSET + SCROLLBAR_W + SCROLLBAR_GAP;

/// Render a scrollable area: caller's `body` closure draws into a
/// clipped sub-target spanning `viewport`; this helper then draws
/// the scroll indicator in the standard right-edge band.
///
/// `body` receives the clipped target and the current scroll offset.
/// The closure must shift each row's y by `-offset` so off-screen
/// rows are drawn into the clipped area where the hardware clip
/// truncates them.
pub fn render_scrolled<D, F>(
    display: &mut D,
    scroll: i32,
    viewport: Rectangle,
    content_h: i32,
    accent: Color,
    ctx: &RenderCtx,
    body: F,
)
where
    D: BlendTarget,
    F: FnOnce(&mut BlendClipped<'_, D>, i32),
{
    {
        let mut clipped = BlendClipped::new(display, &viewport);
        body(&mut clipped, scroll);
    }
    let bar_x = viewport.top_left.x
        + viewport.size.width as i32
        - SCROLLBAR_RIGHT_INSET
        - SCROLLBAR_W;
    let bar_y = viewport.top_left.y;
    let bar_h = viewport.size.height as i32;
    // Skip the scrollbar entirely when its y-range falls outside the
    // current tile. For a typical app the scrollbar spans the whole
    // content viewport (~400 px), so it intersects most tiles - but the
    // very top tile (status bar / header) and the very bottom tile
    // (home indicator) usually clear it, saving the per-call overhead.
    if ctx.intersects_y(bar_y, bar_y + bar_h) {
        primitives::scrollbar_v(
            display,
            bar_x, bar_y, SCROLLBAR_W, bar_h,
            content_h, viewport.size.height as i32, scroll,
            accent, theme::FG_DIM,
        );
    }
}

/// Compute the maximum valid scroll offset for a viewport / content
/// pair. Zero when the content fits.
pub fn scroll_max(content_h: i32, viewport_h: i32) -> i32 {
    (content_h - viewport_h).max(0)
}

/// Translate a `TouchPressed` / `TouchReleased` event into a scroll
/// update. Returns `true` when the visible offset changed and the
/// screen should redraw. Other events are ignored.
pub fn handle_scroll_drag(
    scroll: &mut layout::ScrollState,
    event: &SystemEvent,
    viewport_h: i32,
    content_h: i32,
) -> bool {
    match event {
        SystemEvent::TouchPressed { y, .. } => {
            scroll.drag(*y as i32, scroll_max(content_h, viewport_h))
        }
        SystemEvent::TouchReleased => {
            scroll.release();
            false
        }
        _ => false,
    }
}
