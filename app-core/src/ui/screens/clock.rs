//! Watch face - the Nightwatch OS ambient time display.
//!
//! Layout (top to bottom on a 410x502 canvas):
//! 1. Telemetry strip: cyan `SYS-ID <code>` on the left, chrome
//!    `DOW DD MON` on the right. Small mono-ish font.
//! 2. Swipe-hint bar: a thin cyan 2 px line near the top, a visual
//!    cue for the swipe-down-from-top edge gesture (routed by the
//!    Model into Quick Access).
//! 3. Stacked numerals: HH in signal red, MM directly below in bone.
//!    Both rendered in the geometric `Mega` (logisoso78) face, digits
//!    only. Meta row beneath: `:SS` in cyan + a chrome readout -
//!    today's step count on step-capable boards, else the last GPS
//!    fix on GPS boards, else the spec's static `LAT .. LON ..`
//!    filler. Boards with steps AND GPS get the fix coordinates on
//!    their own line below the meta row (last outdoor sync, not
//!    live - the receiver is rail-gated between sessions).
//! 4. Two chamfered info tiles at the bottom (via `info_tile` +
//!    `layout::bottom_tile_row::<2>()`):
//!    - left: yellow border, bell glyph, next enabled alarm time
//!      (`HH:MM`) or `OFF` if none, suffix `ALARM`.
//!    - right: orange border, hourglass glyph, timer remaining
//!      (`MM:SS` < 1 h, `HH:MM` ≥ 1 h) or `OFF`, suffix `TIMER`.
//!
//! Interactions:
//! - Tap on the alarm tile → switch to Alarm screen.
//! - Tap on the timer tile → switch to Timer screen.
//! - Anywhere else: no-op. Quick Access reaches via swipe-down-from-
//!   top, App Drawer via swipe-up-from-bottom (both at the Model
//!   level, not handled here).
//!
//! Copy follows the spec's systemic voice: ALL CAPS chrome, leading
//! zeros on numerals, no em-dashes.

use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{Point, Size},
    pixelcolor::Rgb565,
    prelude::Primitive,
    primitives::{PrimitiveStyle, Rectangle},
    Drawable,
};
use heapless::String;
use core::fmt::Write;
use u8g2_fonts::FontRenderer;

use crate::events::SystemEvent;
use crate::ui::{fonts, glyphs, layout, theme};
use crate::ui::types::{Action, DirtyRegion, RenderCtx, Screen, ScreenId, SystemData};
use crate::ui::widgets::info_tile;

// -- Geometry ---------------------------------------------------------------

const PAD_TOP: i32 = 28;
const PAD_X: i32 = 40;

/// Y of the swipe-hint bar (2px tall, 36px wide, centered on X).
const HINT_Y: i32 = 8;
const HINT_W: i32 = 36;
const HINT_H: i32 = 2;

/// Y of the telemetry strip's glyph tops.
const TELE_Y: i32 = PAD_TOP;

/// Hero HH/MM block - vertically centered between the telemetry strip
/// and the bottom tile row with a slight upward bias so the meta row
/// under MM sits in the visual midline.
const HERO_HH_TOP: i32 = 120;
/// Spacing between HH baseline and MM baseline. Slight overlap
/// compared to a natural line-height so the two numerals read as one
/// stacked block.
const HERO_STACK_GAP: i32 = 72;
const HERO_MM_TOP: i32 = HERO_HH_TOP + HERO_STACK_GAP;
/// Y of the meta row (:SS + steps/coords readout) under the MM
/// glyphs.
const META_Y: i32 = HERO_MM_TOP + 84;
/// Y of the last-fix coordinates line, rendered only on boards
/// where steps occupy the meta row's right-hand slot.
const GPS_Y: i32 = META_Y + 28;

// -- Dirty-region rectangles -------------------------------------------------
//
// Each visible block of the watch face maps to one rectangle. When the
// underlying data (seconds, minutes, hours, date, timer remaining)
// changes, the corresponding rect is the only thing the renderer has to
// touch. Rects are padded a few pixels around the visible glyph extent
// so anti-aliased edges aren't clipped if a font happens to overhang
// its nominal box.

/// Telemetry strip at the very top: SYS-ID + DOW DD MON.
const TELEMETRY_RECT: Rectangle = Rectangle::new(
    Point::new(0, TELE_Y - 8),
    Size::new(theme::SCREEN_W as u32, 40),
);
/// Hero HH glyphs (signal-red, top of the stacked hero block).
const HERO_HH_RECT: Rectangle = Rectangle::new(
    Point::new(0, HERO_HH_TOP - 10),
    Size::new(theme::SCREEN_W as u32, 88),
);
/// Hero MM glyphs (bone, directly below HH).
const HERO_MM_RECT: Rectangle = Rectangle::new(
    Point::new(0, HERO_MM_TOP - 10),
    Size::new(theme::SCREEN_W as u32, 88),
);
/// Meta row: `:SS` and the steps/coords readout.
const META_RECT: Rectangle = Rectangle::new(
    Point::new(0, META_Y - 8),
    Size::new(theme::SCREEN_W as u32, 36),
);
/// Last-fix coordinates line below the meta row.
const GPS_RECT: Rectangle = Rectangle::new(
    Point::new(0, GPS_Y - 8),
    Size::new(theme::SCREEN_W as u32, 36),
);
/// Bottom tile row (alarm + timer info tiles).
const BOTTOM_RECT: Rectangle = Rectangle::new(
    Point::new(0, layout::BOTTOM_TILE_Y - 4),
    Size::new(theme::SCREEN_W as u32, (layout::BOTTOM_TILE_H + 8) as u32),
);

// -- Screen -----------------------------------------------------------------

/// Snapshot of the inputs that drive the watch face's visible glyphs.
/// Held by [`ClockScreen`] from one render to the next so
/// [`Screen::dirty_rects`] can return only the regions whose underlying
/// fields actually changed.
#[derive(Debug, Clone, Copy)]
struct RenderedSnapshot {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    /// Timer remaining in whole seconds at last render. The displayed
    /// timer value ticks at 1 Hz; tracking integer seconds means
    /// dirty_rects only fires the bottom rect when the visible string
    /// would change.
    timer_secs: u64,
    /// Daily step count at last render (always 0 on boards without
    /// the steps capability, so the compare stays inert there).
    steps: u32,
    /// Last GPS fix at last render (`None` forever on boards
    /// without GPS, so the compare stays inert there).
    fix: Option<crate::data::GpsFix>,
}

pub struct ClockScreen {
    /// `None` until the first render. Avoids a default snapshot that
    /// could spuriously match real data and return `Empty` for the
    /// first dirty_rects call.
    last: Option<RenderedSnapshot>,
    /// Sticky: when set, the next `dirty_rects` returns `FullScreen`
    /// regardless of how `last` compares to current data. Used for
    /// events that change something not represented in
    /// `RenderedSnapshot` (AlarmFired changes the alarm-tile value
    /// without touching seconds; TimerExpired clears the timer tile
    /// the moment the timer hits zero).
    force_full_next: bool,
}

impl ClockScreen {
    pub fn new() -> Self {
        Self { last: None, force_full_next: false }
    }
}

impl Screen for ClockScreen {
    fn render<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        _ctx: &RenderCtx,
    ) {
        // Clock face widgets are fixed-position; the driver clips
        // per-pixel for the current tile, so we don't gain anything
        // from acting on `ctx` here.
        draw_telemetry_strip(display, data);
        draw_swipe_hint(display);
        draw_hero_numerals(display, data);
        draw_meta_row(display, data);
        draw_gps_row(display, data);
        draw_bottom_tiles(display, data);
    }

    fn dirty_rects(&self, data: &SystemData) -> DirtyRegion {
        if self.force_full_next {
            return DirtyRegion::FullScreen;
        }
        let Some(prev) = self.last else {
            // First frame after construction - we have no snapshot to
            // diff against, so paint the whole face once.
            return DirtyRegion::FullScreen;
        };

        let mut region = DirtyRegion::empty();
        if prev.second != data.time.second {
            region.add(META_RECT);
        }
        if prev.minute != data.time.minute {
            region.add(HERO_MM_RECT);
        }
        if prev.hour != data.time.hour {
            region.add(HERO_HH_RECT);
        }
        if prev.day != data.time.day || prev.month != data.time.month {
            region.add(TELEMETRY_RECT);
        }
        if prev.timer_secs != data.timer.remaining().as_secs() {
            region.add(BOTTOM_RECT);
        }
        if prev.steps != data.steps_today {
            region.add(META_RECT);
        }
        if prev.fix != data.gps_fix {
            region.add(GPS_RECT);
        }
        region
    }

    fn clear_dirty(&mut self, data: &SystemData) {
        self.last = Some(RenderedSnapshot {
            second: data.time.second,
            minute: data.time.minute,
            hour: data.time.hour,
            day: data.time.day,
            month: data.time.month,
            timer_secs: data.timer.remaining().as_secs(),
            steps: data.steps_today,
            fix: data.gps_fix,
        });
        self.force_full_next = false;
    }

    fn on_event(&mut self, event: &SystemEvent, _data: &mut SystemData) -> Action {
        match event {
            SystemEvent::PowerButtonLong => Action::Shutdown,
            // A seconds tick forces a redraw so `:SS` and the MM
            // digit keep in sync with the RTC.
            SystemEvent::TimeUpdated { .. } => Action::Redraw,
            // Discrete state transitions that happen between seconds
            // and would otherwise leave the bottom tiles showing
            // stale data until the next TimeUpdated. The
            // `RenderedSnapshot` doesn't capture the alarm-tile string
            // or the moment-of-expiry timer flip, so flag a one-shot
            // full repaint to be safe.
            SystemEvent::AlarmFired { .. } | SystemEvent::TimerExpired { .. } => {
                self.force_full_next = true;
                Action::Redraw
            }
            // Bottom tiles route to their target apps; everywhere else
            // is a no-op. Quick Access opens via swipe-down-from-top
            // and App Drawer via swipe-up-from-bottom (both routed at
            // the Model level, not here).
            SystemEvent::Tap { x, y } => {
                let [left, right] = layout::bottom_tile_row::<2>();
                if rect_hit(left, *x, *y) {
                    Action::SwitchScreen(ScreenId::Alarm)
                } else if rect_hit(right, *x, *y) {
                    Action::SwitchScreen(ScreenId::Timer)
                } else {
                    Action::None
                }
            }
            _ => Action::None,
        }
    }
}

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

// -- Draw helpers -----------------------------------------------------------

fn draw_swipe_hint<D: DrawTarget<Color = Rgb565>>(display: &mut D) {
    // 2px bar, 36px wide, centered horizontally. Cyan at 55% opacity
    // in the spec - we render at full saturation because embedded-
    // graphics has no blending and a dimmer cyan would just look
    // washed out on pure black.
    let cx = theme::SCREEN_W as i32 / 2;
    let x = cx - HINT_W / 2;
    Rectangle::new(
        Point::new(x, HINT_Y),
        Size::new(HINT_W as u32, HINT_H as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(theme::CYAN))
    .draw(display).ok();
}

fn draw_telemetry_strip<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, data: &SystemData,
) {
    let font = fonts::caption();

    // Left: SYS-ID code. Static filler per the spec - no real
    // telemetry to report yet.
    fonts::draw_at(
        display, &font,
        "SYS-ID 232.29CB.98B",
        PAD_X, TELE_Y,
        theme::CYAN,
    );

    // Right: "TUE 24 APR".
    let weekday = crate::ui::screens::alarm::day_of_week(
        data.time.year as i32,
        data.time.month as i32,
        data.time.day as i32,
    );
    let dow = short_weekday(weekday);
    let mon = short_month(data.time.month);

    let mut buf: String<16> = String::new();
    let _ = write!(buf, "{} {:02} {}", dow, data.time.day, mon);

    fonts::draw_right(
        display, &font,
        buf.as_str(),
        theme::SCREEN_W as i32 - PAD_X, TELE_Y,
        theme::FG_MUTED,
    );
}

fn draw_hero_numerals<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, data: &SystemData,
) {
    let mega: FontRenderer = FontRenderer::new::<fonts::Mega>();
    let cx = theme::SCREEN_W as i32 / 2;

    let mut hh: String<4> = String::new();
    let _ = write!(hh, "{:02}", data.time.hour);
    fonts::draw_centered(display, &mega, hh.as_str(), cx, HERO_HH_TOP, theme::SIGNAL);

    let mut mm: String<4> = String::new();
    let _ = write!(mm, "{:02}", data.time.minute);
    fonts::draw_centered(display, &mega, mm.as_str(), cx, HERO_MM_TOP, theme::BONE);
}

fn draw_meta_row<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, data: &SystemData,
) {
    let font = fonts::caption();

    // Seconds block: ":SS" in cyan. The colon is part of the same
    // glyph run so spacing stays tight against the number.
    let mut ss: String<4> = String::new();
    let _ = write!(ss, ":{:02}", data.time.second);

    // Measure the seconds glyph width so we can place it and the
    // right-hand string side-by-side, separated by a fixed gap.
    // Steps on step-capable boards (the last fix then gets its own
    // line - see draw_gps_row), else the last GPS fix, else the
    // spec's static LAT/LON filler.
    let ss_w = fonts::measure_width(&font, ss.as_str());
    let mut right_buf: String<32> = String::new();
    let coords: &str = if data.capabilities.steps {
        let _ = write!(right_buf, "{} STEPS", data.steps_today);
        right_buf.as_str()
    } else if data.capabilities.gps {
        write_coords(&mut right_buf, data.gps_fix);
        right_buf.as_str()
    } else {
        "LAT 0.8314  LON 2.6"
    };
    let coords_w = fonts::measure_width(&font, coords);
    let gap = 14i32;
    let group_w = ss_w + gap + coords_w;

    let cx = theme::SCREEN_W as i32 / 2;
    let left_x = cx - group_w / 2;

    fonts::draw_at(display, &font, ss.as_str(),
        left_x, META_Y, theme::CYAN);
    fonts::draw_at(display, &font, coords,
        left_x + ss_w + gap, META_Y, theme::FG_MUTED);
}

/// Last-fix coordinates on their own line below the meta row - only
/// on boards where steps occupy the meta row's right-hand slot AND
/// a GPS exists. Chrome like the meta readout it descends from.
fn draw_gps_row<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, data: &SystemData,
) {
    if !(data.capabilities.steps && data.capabilities.gps) {
        return;
    }
    let font = fonts::caption();
    let mut buf: String<32> = String::new();
    write_coords(&mut buf, data.gps_fix);
    let cx = theme::SCREEN_W as i32 / 2;
    fonts::draw_centered(display, &font, buf.as_str(), cx, GPS_Y, theme::FG_MUTED);
}

/// `LAT 51.2334  LON 6.7832` from the last fix, dashes before the
/// first one. Four decimals is ~11 m - a glance readout, not
/// navigation.
fn write_coords(buf: &mut String<32>, fix: Option<crate::data::GpsFix>) {
    let Some(f) = fix else {
        let _ = buf.push_str("LAT --.----  LON --.----");
        return;
    };
    write_coord(buf, "LAT ", f.lat_e7);
    let _ = buf.push_str("  ");
    write_coord(buf, "LON ", f.lon_e7);
}

/// One 1e-7-degree value as `[-]D.DDDD`. The sign is emitted
/// separately because integer division toward zero would silently
/// drop it for values between -1 and 0 degrees.
fn write_coord(buf: &mut String<32>, label: &str, v_e7: i32) {
    let _ = buf.push_str(label);
    let a = v_e7.unsigned_abs();
    let _ = write!(
        buf,
        "{}{}.{:04}",
        if v_e7 < 0 { "-" } else { "" },
        a / 10_000_000,
        (a % 10_000_000) / 1_000,
    );
}

fn draw_bottom_tiles<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, data: &SystemData,
) {
    let [left, right] = layout::bottom_tile_row::<2>();

    // -- Alarm tile: next enabled alarm time, or OFF -----------------------
    let weekday = crate::ui::screens::alarm::day_of_week(
        data.time.year as i32,
        data.time.month as i32,
        data.time.day as i32,
    );
    let mut alarm_buf: String<8> = String::new();
    let alarm_value: &str = match data.config.alarms.next_alarm(
        data.time.hour, data.time.minute, weekday,
    ) {
        Some(idx) => {
            let entry = &data.config.alarms.entries[idx];
            let _ = write!(alarm_buf, "{:02}:{:02}", entry.hour, entry.minute);
            alarm_buf.as_str()
        }
        None => "OFF",
    };
    info_tile(display, left, glyphs::bell, alarm_value, "ALARM", theme::YELLOW);

    // -- Timer tile: remaining time, or OFF when idle/zero -----------------
    let mut timer_buf: String<8> = String::new();
    let secs = data.timer.remaining().as_secs();
    let timer_value: &str = if secs == 0 {
        "OFF"
    } else if secs < 3600 {
        let _ = write!(timer_buf, "{:02}:{:02}", secs / 60, secs % 60);
        timer_buf.as_str()
    } else {
        let _ = write!(timer_buf, "{:02}:{:02}", secs / 3600, (secs / 60) % 60);
        timer_buf.as_str()
    };
    info_tile(display, right, glyphs::hourglass, timer_value, "TIMER", theme::ORANGE);
}

// -- Small date helpers -----------------------------------------------------

fn short_weekday(w: u8) -> &'static str {
    match w % 7 {
        0 => "SUN",
        1 => "MON",
        2 => "TUE",
        3 => "WED",
        4 => "THU",
        5 => "FRI",
        _ => "SAT",
    }
}

fn short_month(m: u8) -> &'static str {
    match m {
        1 => "JAN",
        2 => "FEB",
        3 => "MAR",
        4 => "APR",
        5 => "MAY",
        6 => "JUN",
        7 => "JUL",
        8 => "AUG",
        9 => "SEP",
        10 => "OCT",
        11 => "NOV",
        _ => "DEC",
    }
}
