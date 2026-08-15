//! Stopwatch screen - count-up timer rebuilt on the Nightwatch theme.
//!
//! Layout:
//! - Standard app chrome: status bar (green-tinted), Nightwatch
//!   header with `STOPWATCH` title + `STW.0001` system-code
//!   telemetry, signal-red home indicator at the bottom.
//! - Readout panel: full-content-width chamfered_panel with green
//!   border, `ELAPSED` tag-label pinned to the TL chamfer, and
//!   `HH:MM:SS` in the hero font role centered inside. Same
//!   vocabulary as the Vitals heart-rate panel - tag-labeled
//!   chamfered surface, big numeric value.
//! - Action row at the foot: two chamfered_buttons via
//!   `bottom_tile_row::<2>()`. Left = START / PAUSE (Primary green;
//!   label and accent flip with run state). Right = RESET (Ghost
//!   steel when there's nothing to clear, Primary signal-red when
//!   the elapsed counter is non-zero).
//!
//! Accent: green - matches RUN/CALL "active / running / safe-live"
//! semantics from the design spec. Differentiates this screen from
//! Timer (orange) at a glance.
//!
//! Live cadence: redraws at most once per second, gated by
//! [`StopwatchScreen::last_rendered_sec`] - the IMU's 20 Hz
//! `MotionUpdated` ticks are sampled but only translated into a
//! redraw when the displayed seconds change. Pause/resume/reset
//! redraws happen immediately on tap.
//!
//! Known limitation (carried over from the legacy screen): state
//! lives on `StopwatchScreen`, so navigating away drops the count.
//! Hoisting it into `SystemData` for cross-screen persistence is a
//! future change when the use case shows up.

use core::fmt::Write;

use embassy_time::{Duration, Instant};
use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

use crate::events::SystemEvent;
use crate::ui::{fonts, layout, theme};
use crate::ui::types::{Action, DirtyRegion, RenderCtx, Screen, StopwatchState, SystemData};
use crate::ui::widgets::{
    app_chrome_back_hit, chamfered_button, chamfered_panel, draw_app_chrome,
    tag_label, ButtonVariant, APP_CONTENT_TOP, NOTCH, STATUS_BAR_H, TAG_LABEL_H,
};

// -- Constants ---------------------------------------------------------------

/// Per-screen accent. Stopwatch reads as "running / live / active",
/// which lines up with the spec's RUN/CALL green semantics.
const ACCENT: Color = theme::OK;

/// Static system-code shown in the header's right-telemetry slot.
const TELEMETRY: &str = "STW.0001";

/// Side margin for the readout panel - matches
/// `layout::VSTACK_SIDE_MARGIN` so this screen's content lines up
/// with settings sub-views.
const SIDE_MARGIN: i32 = layout::VSTACK_SIDE_MARGIN;

/// Readout panel height. Tall enough that the numerals plus the
/// TL `ELAPSED` tag-label don't crowd each other.
const READOUT_H: i32 = 130;

/// Gap reserved between the readout's bottom edge and the action
/// row above it, so the two visual blocks read as separated.
const READOUT_BUTTON_GAP: i32 = 8;

/// Y of the readout panel's top edge - vertically centred between
/// the header bottom and the action row top so the panel sits in
/// the optical middle of the bezel-safe band.
const READOUT_TOP: i32 = APP_CONTENT_TOP
    + (layout::BOTTOM_TILE_Y - READOUT_BUTTON_GAP - APP_CONTENT_TOP - READOUT_H) / 2;

// -- Dirty rects -------------------------------------------------------------
//
// Three regions hold everything that changes on this screen; the rest
// of the chrome (header band, panel border, tag-label, home indicator)
// is static once drawn.

/// Top status bar - `HH:MM` + battery%. Repaints on the minute roll or
/// a battery-percent change so the clock stays live while the
/// stopwatch counts.
const STATUS_RECT: Rectangle = Rectangle::new(
    Point::new(0, 0),
    Size::new(theme::SCREEN_W as u32, (STATUS_BAR_H + 4) as u32),
);
/// Readout panel - the `HH:MM:SS` hero numerals. Repaints when the
/// displayed second changes.
const READOUT_RECT: Rectangle = Rectangle::new(
    Point::new(SIDE_MARGIN, READOUT_TOP),
    Size::new(
        (theme::SCREEN_W as i32 - SIDE_MARGIN * 2) as u32,
        READOUT_H as u32,
    ),
);
/// Bottom action row - START/PAUSE label and the RESET button variant.
/// Repaints only on a run-state edge or the zero<->non-zero edge, not
/// every second.
const ACTION_ROW_RECT: Rectangle = Rectangle::new(
    Point::new(0, layout::BOTTOM_TILE_Y - 4),
    Size::new(theme::SCREEN_W as u32, (layout::BOTTOM_TILE_H + 8) as u32),
);

// -- Screen ------------------------------------------------------------------

/// Snapshot of the inputs that drive this screen's visible glyphs,
/// held from one render to the next so [`Screen::dirty_rects`] can
/// return only the regions whose underlying fields actually changed.
#[derive(Debug, Clone, Copy)]
struct RenderedSnapshot {
    /// Whole elapsed seconds at last render - drives the readout.
    elapsed_secs: u64,
    /// Stopwatch was in the running state - drives the left button
    /// label (START vs PAUSE).
    running: bool,
    /// Elapsed was zero - drives the RESET button variant
    /// (Ghost/steel vs Primary/signal).
    zero: bool,
    /// Wall-clock minute - drives the status-bar `HH:MM`.
    minute: u8,
    /// Battery percent - drives the status-bar battery glyph.
    battery_pct: Option<u8>,
}

pub struct StopwatchScreen {
    /// Last displayed elapsed second, to gate the once-per-second
    /// redraw against the IMU's 20 Hz MotionUpdated cadence.
    last_rendered_sec: u64,
    /// `None` until the first render - avoids a default snapshot that
    /// could spuriously match real data and skip the first frame.
    last: Option<RenderedSnapshot>,
}

impl StopwatchScreen {
    pub fn new() -> Self {
        Self { last_rendered_sec: 0, last: None }
    }
}

impl Screen for StopwatchScreen {
    fn render<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        _ctx: &RenderCtx,
    ) {
        draw_app_chrome(display, data, "STOPWATCH", TELEMETRY, ACCENT);

        // -- Readout panel -------------------------------------------------
        let panel = READOUT_RECT;
        chamfered_panel(display, panel, NOTCH, ACCENT, 1);
        tag_label(
            display,
            panel.top_left.x,
            panel.top_left.y,
            "ELAPSED",
            ACCENT,
            NOTCH,
        );

        let elapsed = data.stopwatch.elapsed();
        let total_secs = elapsed.as_secs();
        let hours   = (total_secs / 3600).min(99);
        let minutes = (total_secs / 60) % 60;
        let seconds = total_secs % 60;
        let mut buf: heapless::String<12> = heapless::String::new();
        let _ = write!(buf, "{:02}:{:02}:{:02}", hours, minutes, seconds);

        // Centre vertically inside the panel below the tag-label
        // band so the numerals don't sit on top of "ELAPSED".
        let inner_rect = Rectangle::new(
            Point::new(
                panel.top_left.x,
                panel.top_left.y + TAG_LABEL_H,
            ),
            Size::new(
                panel.size.width,
                panel.size.height - TAG_LABEL_H as u32,
            ),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::hero(),
            buf.as_str(), inner_rect, ACCENT,
        );

        // -- Action row ----------------------------------------------------
        let [left, right] = layout::bottom_tile_row::<2>();

        let run_label = match data.stopwatch {
            StopwatchState::Running { .. } => "PAUSE",
            StopwatchState::Paused { .. } => "RESUME",
            StopwatchState::Idle => "START",
        };
        chamfered_button(
            display, left, run_label,
            ButtonVariant::Primary, ACCENT,
        );

        if total_secs == 0 {
            // Nothing to reset - show as inert ghost.
            chamfered_button(
                display, right, "RESET",
                ButtonVariant::Ghost, theme::BORDER,
            );
        } else {
            chamfered_button(
                display, right, "RESET",
                ButtonVariant::Primary, theme::ACCENT,
            );
        }
    }

    fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::PowerButtonLong => Action::Shutdown,

            // 20 Hz tick: redraw only when the displayed second
            // changes, so we don't waste frames on sub-second IMU
            // events while the stopwatch is running.
            SystemEvent::MotionUpdated { .. } if data.stopwatch.is_running() => {
                let sec = data.stopwatch.elapsed().as_secs();
                if sec != self.last_rendered_sec {
                    self.last_rendered_sec = sec;
                    Action::Redraw
                } else {
                    Action::None
                }
            }

            // Header back chevron: pop the nav stack.
            SystemEvent::Tap { x, y } if app_chrome_back_hit(*x, *y) => {
                Action::Back
            }

            SystemEvent::Tap { x, y } => {
                let [left, right] = layout::bottom_tile_row::<2>();
                if rect_hit(left, *x, *y) {
                    data.stopwatch = match data.stopwatch {
                        StopwatchState::Idle => StopwatchState::Running {
                            start: Instant::now(),
                            accumulated: Duration::from_ticks(0),
                        },
                        StopwatchState::Running { start, accumulated } => StopwatchState::Paused {
                            accumulated: accumulated + Instant::now().duration_since(start),
                        },
                        StopwatchState::Paused { accumulated } => StopwatchState::Running {
                            start: Instant::now(),
                            accumulated,
                        },
                    };
                    Action::Redraw
                } else if rect_hit(right, *x, *y) {
                    if data.stopwatch.elapsed().as_secs() == 0 {
                        // RESET renders as Ghost when there's
                        // nothing to clear; drop the tap so the
                        // visual disabled state matches behaviour.
                        Action::None
                    } else {
                        data.stopwatch = StopwatchState::Idle;
                        self.last_rendered_sec = 0;
                        Action::Redraw
                    }
                } else {
                    Action::None
                }
            }

            _ => Action::None,
        }
    }

    fn dirty_rects(&self, data: &SystemData) -> DirtyRegion {
        let Some(prev) = self.last else {
            // First frame after construction - no snapshot to diff
            // against, so paint the whole screen once.
            return DirtyRegion::FullScreen;
        };

        let mut region = DirtyRegion::empty();
        if prev.elapsed_secs != data.stopwatch.elapsed().as_secs() {
            region.add(READOUT_RECT);
        }
        let zero = data.stopwatch.elapsed().as_secs() == 0;
        if prev.running != data.stopwatch.is_running() || prev.zero != zero {
            region.add(ACTION_ROW_RECT);
        }
        if prev.minute != data.time.minute
            || prev.battery_pct != data.power.battery_percent
        {
            region.add(STATUS_RECT);
        }
        region
    }

    fn clear_dirty(&mut self, data: &SystemData) {
        self.last = Some(RenderedSnapshot {
            elapsed_secs: data.stopwatch.elapsed().as_secs(),
            running: data.stopwatch.is_running(),
            zero: data.stopwatch.elapsed().as_secs() == 0,
            minute: data.time.minute,
            battery_pct: data.power.battery_percent,
        });
    }
}

// -- Helpers -----------------------------------------------------------------

fn rect_hit(rect: Rectangle, x: u16, y: u16) -> bool {
    let px = x as i32;
    let py = y as i32;
    let rx = rect.top_left.x;
    let ry = rect.top_left.y;
    px >= rx
        && px < rx + rect.size.width as i32
        && py >= ry
        && py < ry + rect.size.height as i32
}
