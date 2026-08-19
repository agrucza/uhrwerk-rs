//! Timer screen - count-down timer rebuilt on the Nightwatch theme.
//!
//! Two internal views:
//!
//! **Main view**:
//! - Standard app chrome: status bar (orange-tinted), Nightwatch
//!   header `TIMER` + `TMR.0001` system-code, signal-red home
//!   indicator at the bottom.
//! - Countdown dial: a large orange ring gauge that drains from
//!   full as the countdown runs (remaining vs the started-with
//!   total; an idle armed timer reads as full). Hero `HH:MM:SS`
//!   inside with a `REMAINING` caption above it. Tappable when not
//!   running to open the picker. Expiry surfaces in the global
//!   Notifications overlay - no in-screen flash.
//! - Action row: START / PAUSE / RESUME (Primary orange) and
//!   RESET (Ghost steel when zero, Primary signal-red when there's
//!   a duration set).
//!
//! **Picker view** - duration entry:
//! - Standard app chrome with chevron-back == Cancel.
//! - Three-column HH:MM:SS wheel picker (orange accent). HH is
//!   range-limited 0..=4 (the hardware countdown max is 4h15m);
//!   MM and SS wrap modularly.
//! - `CANCEL | SET` action row.
//!
//! Tapping the dial in idle/paused opens the picker, seeded
//! with the current duration. Tapping Set validates: if the
//! chosen total exceeds [`MAX_TIMER_SECS`] the picker stays
//! open, the wheels reset to the capped value, and the wheel
//! accent flashes orange↔red so the user can confirm the cap
//! before committing.


use embassy_time::{Duration, Instant};
use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

use crate::events::SystemEvent;
use crate::ui::{fonts, layout, theme};
use crate::ui::layout::rect_hit;
use crate::ui::types::{Action, DirtyRegion, RenderCtx, Screen, SystemData, TimerState};
use crate::data::TimeData;
use crate::ui::widgets::{
    action_row_rects, app_chrome_back_hit, chamfered_button,
    draw_app_chrome, fmt_2digit, render_action_row, ring_gauge, ButtonVariant,
    Picker, Wheel,
    app_content_top, STATUS_BAR_H, WHEEL_TOTAL_H,
};

// -- Constants ---------------------------------------------------------------

/// Per-screen accent. Orange = "data stream / dynamic" per the spec;
/// also differentiates Timer (counting down) from Stopwatch (green,
/// counting up).
const ACCENT: Color = theme::MEDIA;

/// Static system-code shown in the header's right-telemetry slot.
const TELEMETRY: &str = "TMR.0001";

/// Gap between the dial's bounding box and the action row.
const DIAL_BUTTON_GAP: i32 = 8;

/// Countdown dial - the main view's readout. Outer radius fills the
/// content band (~322 px tall on the watch, 11 px air top and
/// bottom). The hero readout sits inside the 280 px inner diameter;
/// its adaptive format (see [`dial_readout`]) is at most ~240 px
/// wide, leaving >= 17 px to the ring at the numeral rows.
const DIAL_CX: i32 = theme::SCREEN_W as i32 / 2;
const DIAL_R: i32 = 150;
const DIAL_TH: i32 = 10;

/// Y of the dial center - the middle of the band between the header
/// bottom and the action row.
fn dial_cy(safe: &crate::data::SafeArea) -> i32 {
    let ct = app_content_top(safe);
    (ct + layout::BOTTOM_TILE_Y - DIAL_BUTTON_GAP) / 2
}

/// Top y of the wheel picker. Centered between the header bottom
/// and the action row.
fn picker_top(safe: &crate::data::SafeArea) -> i32 {
    let ct = app_content_top(safe);
    ct + (layout::BOTTOM_TILE_Y - ct - WHEEL_TOTAL_H) / 2
}

/// Width of one wheel column.
const PICKER_COL_W: i32 = 72;

/// Horizontal gap between columns - wide enough to hold the colon
/// glyph at value-font size with breathing room on both sides.
const PICKER_GAP: i32 = 28;

/// Total horizontal extent of the three-column picker.
const PICKER_TOTAL_W: i32 = PICKER_COL_W * 3 + PICKER_GAP * 2;

/// Maximum timer duration in seconds (255 * 60s = 4h15m), capped
/// by the PCF85063 hardware countdown register.
const MAX_TIMER_SECS: u64 = 15300;

/// Maximum hour value selectable on the picker. The hardware cap
/// is 4h15m, so values past 4h would always be clamped on Set;
/// limit the wheel itself so the user can't spin past 4 h.
const MAX_TIMER_H: i32 = 4;

/// Ticks per flash phase (250 ms at 20 Hz = 5 ticks).
const FLASH_PHASE_TICKS: u8 = 5;

/// Total flash ticks for the clamp-warning animation (4 phases = 1 s).
const FLASH_TOTAL_TICKS: u8 = FLASH_PHASE_TICKS * 4;

// -- Dirty rects -------------------------------------------------------------
//
// Main-view regions. The picker view animates wheels and flashes the
// accent, so it stays on the safe full-redraw path (see `dirty_rects`).

/// Top status bar - `HH:MM` + battery%. Repaints on the minute roll
/// or a battery-percent change so the clock stays live while the
/// timer counts down.
fn status_rect(safe: &crate::data::SafeArea) -> Rectangle {
    Rectangle::new(
        Point::new(0, 0),
        Size::new(theme::SCREEN_W as u32, (safe.top + STATUS_BAR_H + 4) as u32),
    )
}
/// Dial bounding box - ring, hero numerals and caption included.
/// Repaints when the displayed second changes (the arc endpoint and
/// the numerals move together). Also the tap target for opening the
/// picker.
fn dial_rect(safe: &crate::data::SafeArea) -> Rectangle {
    Rectangle::new(
        Point::new(DIAL_CX - DIAL_R - 4, dial_cy(safe) - DIAL_R - 4),
        Size::new((DIAL_R * 2 + 8) as u32, (DIAL_R * 2 + 8) as u32),
    )
}
/// Bottom action row - START/PAUSE/RESUME label and the RESET button
/// variant. Repaints only on a run-state edge or the zero<->non-zero
/// edge, not every second.
const ACTION_ROW_RECT: Rectangle = Rectangle::new(
    Point::new(0, layout::BOTTOM_TILE_Y - 4),
    Size::new(theme::SCREEN_W as u32, (layout::BOTTOM_TILE_H + 8) as u32),
);

// -- Internal types ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerView {
    Main,
    Picker,
}

// -- Screen ------------------------------------------------------------------

/// Snapshot of the inputs that drive the main view's visible glyphs,
/// held from one render to the next so [`Screen::dirty_rects`] can
/// return only the regions whose underlying fields actually changed.
/// Only meaningful while `view == Main`; the picker view stays on the
/// full-redraw path.
#[derive(Debug, Clone, Copy)]
struct RenderedSnapshot {
    /// Whole remaining seconds at last render - drives the dial.
    remaining_secs: u64,
    /// Timer was in the running state - drives the left button label.
    running: bool,
    /// Remaining was zero - drives the RESET button variant.
    zero: bool,
    /// Wall-clock minute - drives the status-bar `HH:MM`.
    minute: u8,
    /// Battery percent - drives the status-bar battery glyph.
    battery_pct: Option<u8>,
}

pub struct TimerScreen {
    view: TimerView,
    /// Last displayed remaining second, gates 1 Hz redraw.
    last_rendered_sec: u64,
    /// `None` until the first render - avoids a default snapshot that
    /// could spuriously match real data and skip the first frame.
    last: Option<RenderedSnapshot>,
    /// Sticky: forces the next `dirty_rects` to `FullScreen`. Set on
    /// picker -> main transitions, where the header text and panel
    /// change but the `RenderedSnapshot` fields may not.
    force_full_next: bool,
    /// HH:MM:SS wheel picker for duration entry. Seeded from the
    /// current duration on entry; values read back into a Duration
    /// on Set.
    picker: Picker<3>,
    /// Remaining flash ticks for the clamp-warning animation.
    /// Alternates the picker readout label between accent and danger.
    flash_ticks: u8,
}

impl TimerScreen {
    pub fn new() -> Self {
        Self {
            view: TimerView::Main,
            last_rendered_sec: 0,
            last: None,
            force_full_next: false,
            picker: Picker::new([
                Wheel::new(0, MAX_TIMER_H, 0),
                Wheel::new(0, 59, 0).with_wrap(true),
                Wheel::new(0, 59, 0).with_wrap(true),
            ]),
            flash_ticks: 0,
        }
    }

    /// Seed the picker from a `Duration` (used on entry to the
    /// picker view and after a clamp).
    fn seed_picker_from(&mut self, d: Duration) {
        let total = d.as_secs();
        let h = (total / 3600).min(MAX_TIMER_H as u64) as i32;
        let m = ((total / 60) % 60) as i32;
        let s = (total % 60) as i32;
        self.picker.wheels[0].set_value(h);
        self.picker.wheels[1].set_value(m);
        self.picker.wheels[2].set_value(s);
    }

    /// Read the current picker values back as a `Duration`.
    fn picker_duration(&self) -> Duration {
        let h = self.picker.wheels[0].value() as u64;
        let m = self.picker.wheels[1].value() as u64;
        let s = self.picker.wheels[2].value() as u64;
        Duration::from_secs(h * 3600 + m * 60 + s)
    }

    /// Compute remaining seconds from current RTC wall time + the
    /// stored target seconds-since-midnight. Handles midnight wrap.
    fn remaining_from_rtc(target_secs: u32, time: &TimeData) -> u32 {
        let now_secs = time.hour as u32 * 3600
            + time.minute as u32 * 60
            + time.second as u32;
        if target_secs >= now_secs {
            target_secs - now_secs
        } else {
            (24 * 3600 - now_secs) + target_secs
        }
    }

    /// Set up the running state: compute target_secs from current
    /// RTC time and arm the embassy deadline. `total` is the dial's
    /// 100% reference - the set duration on START, the carried
    /// original on RESUME.
    fn start_countdown(secs: u32, total: u32, data: &mut SystemData) {
        let now_secs = data.time.hour as u32 * 3600
            + data.time.minute as u32 * 60
            + data.time.second as u32;
        let target_secs = (now_secs + secs) % (24 * 3600);
        data.timer = TimerState::Running {
            deadline: Instant::now() + Duration::from_secs(secs as u64),
            target_secs,
            total_secs: total,
        };
    }

    /// Color used for the picker wheels (selection cell + hairlines)
    /// and colon separators. Flashes accent↔danger during the
    /// post-clamp warning so the user sees the cap was applied
    /// before they tap Set again.
    fn picker_accent(&self) -> Color {
        if self.flash_ticks == 0 {
            return ACCENT;
        }
        let phase = (FLASH_TOTAL_TICKS - self.flash_ticks) / FLASH_PHASE_TICKS;
        if phase % 2 == 0 { theme::DANGER } else { ACCENT }
    }
}

impl Screen for TimerScreen {
    fn render<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        match self.view {
            TimerView::Main => self.render_main(display, data, ctx),
            TimerView::Picker => self.render_picker(display, data, ctx),
        }
    }

    fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        if matches!(event, SystemEvent::PowerButtonLong) {
            return Action::Shutdown;
        }

        // Resync embassy deadline from RTC time on every wall-clock
        // tick. Without this the embassy Instant drifts across light
        // sleep / RTC adjustment.
        if let SystemEvent::TimeUpdated { data: time } = event {
            if let TimerState::Running { target_secs, total_secs, .. } = data.timer {
                let remaining = Self::remaining_from_rtc(target_secs, time);
                if remaining == 0 {
                    data.timer = TimerState::Idle {
                        duration: Duration::from_ticks(0),
                    };
                } else {
                    data.timer = TimerState::Running {
                        deadline: Instant::now() + Duration::from_secs(remaining as u64),
                        target_secs,
                        total_secs,
                    };
                }
                return Action::Redraw;
            }
        }

        match self.view {
            TimerView::Main => self.main_event(event, data),
            TimerView::Picker => self.picker_event(event, data),
        }
    }

    fn dirty_rects(&self, data: &SystemData) -> DirtyRegion {
        // The picker view animates wheels on drag and flashes the
        // accent on a clamp; per-widget invalidation there is deferred,
        // so it stays on the safe full-redraw path.
        if self.force_full_next || self.view == TimerView::Picker {
            return DirtyRegion::FullScreen;
        }
        let Some(prev) = self.last else {
            // First frame after construction - no snapshot to diff
            // against, so paint the whole screen once.
            return DirtyRegion::FullScreen;
        };

        let mut region = DirtyRegion::empty();
        if prev.remaining_secs != data.timer.remaining().as_secs() {
            region.add(dial_rect(&data.safe_area));
        }
        let zero = data.timer.remaining().as_secs() == 0;
        if prev.running != data.timer.is_running() || prev.zero != zero {
            region.add(ACTION_ROW_RECT);
        }
        if prev.minute != data.time.minute
            || prev.battery_pct != data.power.battery_percent
        {
            region.add(status_rect(&data.safe_area));
        }
        region
    }

    fn clear_dirty(&mut self, data: &SystemData) {
        self.last = Some(RenderedSnapshot {
            remaining_secs: data.timer.remaining().as_secs(),
            running: data.timer.is_running(),
            zero: data.timer.remaining().as_secs() == 0,
            minute: data.time.minute,
            battery_pct: data.power.battery_percent,
        });
        self.force_full_next = false;
    }
}

// -- Main view ---------------------------------------------------------------

impl TimerScreen {
    fn render_main<D: BlendTarget>(&self, display: &mut D, data: &SystemData, ctx: &RenderCtx) {
        draw_app_chrome(display, data, "TIMER", TELEMETRY, ACCENT, ctx);

        // -- Countdown dial ------------------------------------------------
        // Ring drains from full as the countdown runs: remaining vs
        // the started-with total. Idle with a set duration reads as a
        // full ring (armed); zero leaves just the track.
        let remaining = data.timer.remaining().as_secs();
        let total = data.timer.total_secs();
        let cy = dial_cy(&data.safe_area);
        ring_gauge(
            display, ctx,
            DIAL_CX, cy, DIAL_R, DIAL_TH,
            remaining.min(total) as u32, total.max(1) as u32,
            ACCENT,
            Some(theme::SURFACE_3),
        );

        fonts::draw_centered(
            display, &fonts::caption(), "REMAINING",
            DIAL_CX, cy - 46, theme::FG_MUTED,
        );
        let buf = dial_readout(remaining);
        fonts::draw_centered(
            display, &fonts::hero(), buf.as_str(),
            DIAL_CX, cy - 24, ACCENT,
        );

        // -- Action row ----------------------------------------------------
        let [left, right] = layout::bottom_tile_row::<2>();

        let run_label = match data.timer {
            TimerState::Running { .. } => "PAUSE",
            TimerState::Paused { remaining, .. } if remaining.as_ticks() > 0 => "RESUME",
            _ => "START",
        };
        // START is meaningful only when there's a duration to run.
        // For Idle@zero we still draw Primary so the affordance is
        // visible; the tap is rejected by `main_event`.
        chamfered_button(display, left, run_label, ButtonVariant::Primary, ACCENT);

        if data.timer.remaining().as_secs() == 0 {
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

    fn main_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            // 20 Hz tick: redraw only when the displayed second changes.
            SystemEvent::MotionUpdated { .. }
                if data.timer.is_running() =>
            {
                let sec = data.timer.remaining().as_secs();
                if sec != self.last_rendered_sec {
                    self.last_rendered_sec = sec;
                    Action::Redraw
                } else {
                    Action::None
                }
            }

            // Header back chevron: pop the nav stack.
            SystemEvent::Tap { x, y } if app_chrome_back_hit(*x, *y, &data.safe_area) => {
                Action::Back
            }

            // Tap the dial (when not running) → picker.
            SystemEvent::Tap { x, y }
                if !data.timer.is_running() && rect_hit(dial_rect(&data.safe_area), *x, *y) =>
            {
                self.seed_picker_from(data.timer.remaining());
                self.view = TimerView::Picker;
                Action::Redraw
            }

            SystemEvent::Tap { x, y } => {
                let [left, right] = layout::bottom_tile_row::<2>();
                if rect_hit(left, *x, *y) {
                    match data.timer {
                        TimerState::Idle { duration } if duration.as_ticks() > 0 => {
                            let secs = duration.as_secs() as u32;
                            Self::start_countdown(secs, secs, data);
                            Action::StartTimer { seconds: secs }
                        }
                        TimerState::Running { deadline, total_secs, .. } => {
                            let now = Instant::now();
                            let remaining = if now >= deadline {
                                Duration::from_ticks(0)
                            } else {
                                deadline.duration_since(now)
                            };
                            data.timer = TimerState::Paused { remaining, total_secs };
                            Action::CancelTimer
                        }
                        TimerState::Paused { remaining, total_secs }
                            if remaining.as_ticks() > 0 =>
                        {
                            let secs = remaining.as_secs() as u32;
                            Self::start_countdown(secs, total_secs, data);
                            Action::StartTimer { seconds: secs }
                        }
                        // Idle@zero or Paused@zero - nothing to start.
                        _ => Action::None,
                    }
                } else if rect_hit(right, *x, *y) {
                    if data.timer.remaining().as_secs() == 0 {
                        // Ghost RESET when nothing to clear; drop the tap.
                        Action::None
                    } else {
                        let was_running = data.timer.is_running();
                        data.timer = TimerState::Idle {
                            duration: Duration::from_ticks(0),
                        };
                        self.last_rendered_sec = 0;
                        if was_running {
                            Action::CancelTimer
                        } else {
                            Action::Redraw
                        }
                    }
                } else {
                    Action::None
                }
            }

            _ => Action::None,
        }
    }
}

// -- Picker view -------------------------------------------------------------

impl TimerScreen {
    fn render_picker<D: BlendTarget>(&self, display: &mut D, data: &SystemData, ctx: &RenderCtx) {
        draw_app_chrome(display, data, "SET TIMER", TELEMETRY, ACCENT, ctx);

        // Wheels are the readout - their selection cells already
        // show the current HH/MM/SS. The accent flashes red
        // during the post-clamp warning so the user sees the cap
        // was applied before re-pressing Set.
        let accent = self.picker_accent();
        let cells = picker_cell_rects(&data.safe_area);
        self.picker.wheels[0].render(display, cells[0], accent, fmt_2digit);
        self.picker.wheels[1].render(display, cells[1], accent, fmt_2digit);
        self.picker.wheels[2].render(display, cells[2], accent, fmt_2digit);

        // Colons between adjacent columns, on the picker's
        // selection-band centerline.
        let band_cy = cells[0].top_left.y + cells[0].size.height as i32 / 2;
        for i in 0..2 {
            let cx = (cells[i].top_left.x + cells[i].size.width as i32
                + cells[i + 1].top_left.x) / 2;
            let colon_rect = Rectangle::new(
                Point::new(cx - 8, band_cy - 16),
                Size::new(16, 32),
            );
            fonts::draw_centered_in_rect(
                display, &fonts::value(), ":", colon_rect, accent,
            );
        }

        // CANCEL | SET action row.
        render_action_row(display, ACCENT);
    }

    fn picker_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            // Flash animation: count ticks, redraw on phase change.
            SystemEvent::MotionUpdated { .. } if self.flash_ticks > 0 => {
                let old_phase = self.flash_ticks / FLASH_PHASE_TICKS;
                self.flash_ticks -= 1;
                let new_phase = self.flash_ticks / FLASH_PHASE_TICKS;
                if new_phase != old_phase { Action::Redraw } else { Action::None }
            }

            // Header chevron == CANCEL: discard and return to Main.
            SystemEvent::Tap { x, y } if app_chrome_back_hit(*x, *y, &data.safe_area) => {
                self.flash_ticks = 0;
                self.force_full_next = true;
                self.view = TimerView::Main;
                Action::Redraw
            }

            SystemEvent::Tap { x, y } => {
                let (cancel, set) = action_row_rects();
                if rect_hit(cancel, *x, *y) {
                    self.flash_ticks = 0;
                    self.force_full_next = true;
                    self.view = TimerView::Main;
                    return Action::Redraw;
                }
                if rect_hit(set, *x, *y) {
                    let dur = self.picker_duration();
                    if dur.as_secs() > MAX_TIMER_SECS {
                        // Cap and flash; user must press Set again
                        // with the capped value to commit.
                        let capped = Duration::from_secs(MAX_TIMER_SECS);
                        data.timer = TimerState::Idle { duration: capped };
                        self.seed_picker_from(capped);
                        self.flash_ticks = FLASH_TOTAL_TICKS;
                        return Action::Redraw;
                    }
                    data.timer = TimerState::Idle { duration: dur };
                    self.force_full_next = true;
                    self.view = TimerView::Main;
                    return Action::Redraw;
                }

                // Picker tap-step (above/below center band).
                let cells = picker_cell_rects(&data.safe_area);
                if self.picker.handle_event(event, &cells) {
                    return Action::Redraw;
                }
                Action::None
            }

            // Drag scroll on the wheels.
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let cells = picker_cell_rects(&data.safe_area);
                if self.picker.handle_event(event, &cells) {
                    return Action::Redraw;
                }
                Action::None
            }

            _ => Action::None,
        }
    }
}

/// Per-column rects for the HH:MM:SS wheel picker, centred horizontally.
fn picker_cell_rects(safe: &crate::data::SafeArea) -> [Rectangle; 3] {
    let start_x = (theme::SCREEN_W as i32 - PICKER_TOTAL_W) / 2;
    core::array::from_fn(|i| {
        Rectangle::new(
            Point::new(
                start_x + i as i32 * (PICKER_COL_W + PICKER_GAP),
                picker_top(safe),
            ),
            Size::new(PICKER_COL_W as u32, WHEEL_TOTAL_H as u32),
        )
    })
}

// -- Helpers -----------------------------------------------------------------

/// Dial readout: `MM:SS` below one hour, `H:MM:SS` from one hour up
/// (the picker caps hours at 4, so one digit always suffices). The
/// full `HH:MM:SS` at hero size is wider than the dial's interior -
/// hero digits are up to 42 px, 8 glyphs = 282 px vs 280 px inner
/// diameter.
fn dial_readout(secs: u64) -> heapless::String<12> {
    use core::fmt::Write;
    let mut buf: heapless::String<12> = heapless::String::new();
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h == 0 {
        let _ = write!(buf, "{:02}:{:02}", m, s);
    } else {
        let _ = write!(buf, "{}:{:02}:{:02}", h, m, s);
    }
    buf
}




