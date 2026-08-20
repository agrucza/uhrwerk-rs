//! Clock sub-view: the TIME and DATE metric panels, the timezone
//! stepper, and the TIME SYNC trigger.
//!
//! Tapping either metric panel seeds and opens the matching wheel
//! picker in [`super::pickers`]. TIME SYNC sets the clock from
//! whichever source the board has - the Model picks one per tap
//! (WiFi when a network is stored, else GPS) and does not chain, so
//! the per-source triggers in the WIFI and GPS views stay the way to
//! force a specific radio.

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::ui::{fmt, fonts, layout, theme};
use crate::ui::layout::rect_hit;
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    chamfered_button, chamfered_panel, tag_label, ButtonVariant, NOTCH, TAG_LABEL_H,
};

use super::{draw_header, header_back_hit, leaf_top_y, SettingsScreen, SettingsView};

/// `UTC+01:00`-style offset readout - shared by the timezone stepper
/// here and the settings index's GPS row value.
pub(super) fn fmt_utc_offset(m: i16) -> String<12> {
    let a = m.unsigned_abs();
    let mut buf = String::new();
    let _ = write!(
        buf,
        "UTC{}{:02}:{:02}",
        if m < 0 { '-' } else { '+' },
        a / 60,
        a % 60,
    );
    buf
}

/// Height of one clock metric panel (TIME / DATE / TIMEZONE).
const CLOCK_PANEL_H: i32 = 84;
/// Vertical gap between adjacent clock panels.
const CLOCK_PANEL_GAP: i32 = 12;

/// All rects the CLOCK view needs. Render and event handlers both
/// call [`clock_slots`] and read the same fields, so geometry can
/// never drift between draw and hit-test.
struct ClockSlots {
    /// TIME panel - tap opens the time picker.
    time_panel: Rectangle,
    /// DATE panel - tap opens the date picker.
    date_panel: Rectangle,
    /// Outer chamfered panel for the timezone section.
    tz_panel: Rectangle,
    /// -15 min stepper inside `tz_panel`.
    tz_minus: Rectangle,
    /// +15 min stepper inside `tz_panel`.
    tz_plus: Rectangle,
    /// The TIME SYNC trigger.
    sync_btn: Rectangle,
    /// Caption line under the button: what the sync is doing now, or
    /// how the last one ended.
    status_line: Rectangle,
}

fn clock_slots(safe: &crate::data::SafeArea) -> ClockSlots {
    let mut s = layout::VStack::new(leaf_top_y(safe));
    let time_panel = s.slot(CLOCK_PANEL_H);
    s.gap(CLOCK_PANEL_GAP);
    let date_panel = s.slot(CLOCK_PANEL_H);
    s.gap(CLOCK_PANEL_GAP);
    let tz_panel = s.slot(CLOCK_PANEL_H);

    // The +/- steppers flank the timezone readout inside the panel,
    // vertically centered below its tag label.
    let inset: i32 = 14;
    let btn: i32 = 44;
    let by = tz_panel.top_left.y
        + TAG_LABEL_H
        + (tz_panel.size.height as i32 - TAG_LABEL_H - btn) / 2;
    let tz_minus = Rectangle::new(
        Point::new(tz_panel.top_left.x + inset, by),
        Size::new(btn as u32, btn as u32),
    );
    let tz_plus = Rectangle::new(
        Point::new(
            tz_panel.top_left.x + tz_panel.size.width as i32 - inset - btn,
            by,
        ),
        Size::new(btn as u32, btn as u32),
    );

    s.gap(14);
    let sync_btn = s.slot(44);
    s.gap(6);
    let status_line = s.slot(20);

    ClockSlots {
        time_panel, date_panel, tz_panel, tz_minus, tz_plus,
        sync_btn, status_line,
    }
}

/// Which source a TIME SYNC tap would actually use, mirroring the
/// Model's one-source-per-tap routing. `None` = nothing to sync
/// with, and the button renders Ghost.
fn sync_source(data: &SystemData) -> Option<crate::data::TimeSource> {
    if data.capabilities.wifi && data.config.wifi.is_set() {
        Some(crate::data::TimeSource::Wifi)
    } else if data.capabilities.gps {
        Some(crate::data::TimeSource::Gps)
    } else {
        None
    }
}

/// True while either radio is mid-session - the button goes Ghost and
/// the tap is dropped, matching every other disabled control here.
fn sync_busy(data: &SystemData) -> bool {
    matches!(data.wifi, crate::data::WifiState::Connecting)
        || matches!(data.gps_sync, crate::data::GpsSyncState::Syncing { .. })
}

/// Caption under the TIME SYNC button: the live session first, then
/// the last successful sync, then why there is nothing to sync with.
fn sync_status_line(data: &SystemData) -> String<28> {
    use crate::data::{GpsSyncState, WifiState};
    let mut line: String<28> = String::new();
    if matches!(data.wifi, WifiState::Connecting) {
        let _ = line.push_str("SYNCING VIA WIFI");
        return line;
    }
    if let GpsSyncState::Syncing { sats, .. } = data.gps_sync {
        let _ = write!(line, "SYNCING VIA GPS - {} SATS", sats);
        return line;
    }
    // Whichever source last set the RTC. Neither `wifi` nor
    // `gps_sync` is timestamped, so this reads the Model's stamp
    // instead of guessing which enum is fresher.
    if let Some(last) = data.last_time_sync {
        let _ = write!(
            line, "{} VIA {}",
            fmt::hm(last.hour, last.minute).as_str(),
            last.source.label(),
        );
        return line;
    }
    let _ = line.push_str(match sync_source(data) {
        Some(_) => "NEVER SYNCED",
        None if data.capabilities.wifi => "NO NETWORK STORED",
        None => "NO TIME SOURCE",
    });
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLOCK view grew a third panel plus the TIME SYNC button and
    /// its caption, and it does NOT scroll - so the last slot has to
    /// clear the home-indicator bar on the tightest board. T-Watch
    /// Ultra has the only non-zero insets (top 8 / bottom 4), which
    /// both push content down and lift the bar up.
    #[test]
    fn clock_slots_fit_above_the_home_bar() {
        for safe in [
            crate::data::SafeArea { top: 8, bottom: 4, left: 16, right: 16, corner_r: 112 },
            crate::data::SafeArea::default(),
        ] {
            let slots = clock_slots(&safe);
            let bottom = slots.status_line.top_left.y
                + slots.status_line.size.height as i32;
            let limit = crate::ui::widgets::viewport_to_home_bar(0, &safe);
            let limit_y = limit.top_left.y + limit.size.height as i32;
            assert!(
                bottom <= limit_y,
                "clock content ends at {bottom}, home-bar viewport ends at {limit_y}",
            );
        }
    }
}

fn draw_clock_panel<D: BlendTarget>(
    display: &mut D,
    rect: Rectangle,
    tag: &str,
    value: &str,
) {
    chamfered_panel(display, rect, NOTCH, theme::ACCENT, 1);
    tag_label(
        display,
        rect.top_left.x, rect.top_left.y,
        tag, theme::ACCENT, NOTCH,
    );
    let inner = Rectangle::new(
        Point::new(rect.top_left.x, rect.top_left.y + TAG_LABEL_H),
        Size::new(rect.size.width, rect.size.height - TAG_LABEL_H as u32),
    );
    fonts::draw_centered_in_rect(
        display, &fonts::value(),
        value, inner, theme::FG,
    );
}

impl SettingsScreen {
    pub(super) fn render_clock<D: BlendTarget>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "CLOCK", theme::ACCENT, ctx);
        let slots = clock_slots(&data.safe_area);

        let time_buf = fmt::hms_parts(
            data.time.hour as u64, data.time.minute as u64, data.time.second as u64,
        );
        draw_clock_panel(display, slots.time_panel, "TIME", time_buf.as_str());

        let mut date_buf: String<12> = String::new();
        let _ = write!(date_buf, "{:02}.{:02}.{:04}",
            data.time.day, data.time.month, data.time.year);
        draw_clock_panel(display, slots.date_panel, "DATE", date_buf.as_str());

        // Timezone stepper: +/- 15 min covers every real UTC offset
        // (Newfoundland, Nepal); the value sits between the steppers.
        chamfered_panel(display, slots.tz_panel, NOTCH, theme::BORDER, 1);
        tag_label(
            display,
            slots.tz_panel.top_left.x, slots.tz_panel.top_left.y,
            "TIMEZONE", theme::BORDER, NOTCH,
        );
        chamfered_button(
            display, slots.tz_minus, "-", ButtonVariant::Ghost, theme::BORDER,
        );
        chamfered_button(
            display, slots.tz_plus, "+", ButtonVariant::Ghost, theme::BORDER,
        );
        let tz = fmt_utc_offset(data.config.time.tz_offset_minutes);
        let between_x = slots.tz_minus.top_left.x + slots.tz_minus.size.width as i32;
        let tz_rect = Rectangle::new(
            Point::new(between_x, slots.tz_minus.top_left.y),
            Size::new(
                (slots.tz_plus.top_left.x - between_x) as u32,
                slots.tz_minus.size.height,
            ),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(), tz.as_str(), tz_rect, theme::FG,
        );

        // TIME SYNC: Ghost (and tap-dead) with no usable source or a
        // session already running.
        let busy = sync_busy(data);
        let label = if busy { "SYNCING" } else { "TIME SYNC" };
        if sync_source(data).is_some() && !busy {
            chamfered_button(
                display, slots.sync_btn, label,
                ButtonVariant::Primary, theme::ACCENT,
            );
        } else {
            chamfered_button(
                display, slots.sync_btn, label,
                ButtonVariant::Ghost, theme::BORDER,
            );
        }
        fonts::draw_centered_in_rect(
            display, &fonts::caption(),
            sync_status_line(data).as_str(), slots.status_line, theme::FG_MUTED,
        );
    }

    pub(super) fn clock_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            // Keep the display fresh.
            SystemEvent::TimeUpdated { .. } => Action::Redraw,
            // Live session progress drives the status line + button
            // state, from whichever radio is running.
            SystemEvent::WifiStatusUpdated { .. }
            | SystemEvent::GpsSyncUpdated { .. } => Action::Redraw,

            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Index;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Index;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let slots = clock_slots(&data.safe_area);
                let p = Point::new(*x as i32, *y as i32);

                if rect_hit(slots.tz_minus, *x, *y) {
                    return Action::AdjustTimezone { delta_min: -15 };
                }
                if rect_hit(slots.tz_plus, *x, *y) {
                    return Action::AdjustTimezone { delta_min: 15 };
                }
                if rect_hit(slots.sync_btn, *x, *y) {
                    // Visually disabled - the tap dies here too, not
                    // just in the Model.
                    if sync_source(data).is_none() || sync_busy(data) {
                        return Action::None;
                    }
                    return Action::TimeSync;
                }
                if slots.time_panel.contains(p) {
                    // Open time picker, seed from current time.
                    let t = &data.time;
                    self.time_picker.wheels[0].set_value(t.hour as i32);
                    self.time_picker.wheels[1].set_value(t.minute as i32);
                    self.time_picker.wheels[2].set_value(t.second as i32);
                    self.view = SettingsView::TimeEntry;
                    Action::Redraw
                } else if slots.date_panel.contains(p) {
                    // Open date picker, seed from current date. Set
                    // month + year first, then re-clamp day range
                    // before assigning day so it's always valid.
                    let t = &data.time;
                    self.date_picker.wheels[1].set_value(t.month as i32);
                    self.date_picker.wheels[2].set_value(t.year as i32);
                    self.refresh_date_day_range();
                    self.date_picker.wheels[0].set_value(t.day as i32);
                    self.view = SettingsView::DateEntry;
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            _ => Action::None,
        }
    }
}
