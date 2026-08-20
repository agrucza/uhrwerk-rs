//! GPS sub-view: session status, the SYNC trigger, the timezone
//! stepper and the tracking enable + cadence controls. Only reachable
//! on boards with the gps capability (the index row is gated).

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::config::GpsTrackingCadence;
use crate::data::GpsSyncState;
use crate::ui::{fmt, fonts, layout, theme};
use crate::ui::layout::rect_hit;
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    chamfered_button, chamfered_panel, tag_label, toggle, ButtonVariant, NOTCH,
    TAG_LABEL_H, TOGGLE_H, TOGGLE_W,
};

use super::{draw_header, header_back_hit, leaf_top_y, SettingsScreen, SettingsView};

/// All rects the GPS sub-view needs. Render and event handlers both
/// call [`gps_slots`] and read the same fields, so geometry can
/// never drift between draw and hit-test.
struct GpsSlots {
    /// Outer chamfered panel for the session status section.
    status_panel: Rectangle,
    /// The SYNC NOW trigger (disabled while a session runs). Forces a
    /// GPS session specifically - the CLOCK view's TIME SYNC button
    /// would pick WiFi whenever a network is stored.
    sync_btn: Rectangle,
    /// Outer chamfered panel for the tracking section.
    tracking_panel: Rectangle,
    /// Hit area of the enable toggle in the panel's tag row (the
    /// drawn toggle is centered inside it).
    tracking_toggle: Rectangle,
    /// One rect per [`TRACKING_CADENCES`] entry, in order.
    tracking_buttons: [Rectangle; 4],
}

/// Cadence options for the TRACKING panel, in button order. A
/// remembered preference, selectable while tracking is off.
const TRACKING_CADENCES: [(&str, GpsTrackingCadence); 4] = [
    ("CONT", GpsTrackingCadence::Continuous),
    ("15S", GpsTrackingCadence::Every15s),
    ("30S", GpsTrackingCadence::Every30s),
    ("60S", GpsTrackingCadence::Every60s),
];

fn gps_slots(safe: &crate::data::SafeArea) -> GpsSlots {
    let mut s = layout::VStack::new(leaf_top_y(safe));
    let status_panel = s.slot(96);
    s.gap(18);
    let sync_btn = s.slot(44);
    let inset: i32 = 14;

    // Tracking panel: enable toggle in the tag row, cadence buttons
    // hugging the bottom (auto-lock layout idiom).
    s.gap(18);
    let tracking_panel = s.slot(76);
    let tracking_toggle = Rectangle::new(
        Point::new(
            tracking_panel.top_left.x + tracking_panel.size.width as i32
                - inset - TOGGLE_W - 12,
            tracking_panel.top_left.y,
        ),
        Size::new((TOGGLE_W + inset + 12) as u32, (TAG_LABEL_H + 8) as u32),
    );
    let btn_h = 30i32;
    let row_y = tracking_panel.top_left.y
        + tracking_panel.size.height as i32 - btn_h - 12;
    let mut tracking_inner = layout::VStack::inside(tracking_panel, 10, row_y);
    let tracking_buttons = tracking_inner.row::<4>(btn_h, 8);

    GpsSlots {
        status_panel, sync_btn,
        tracking_panel, tracking_toggle, tracking_buttons,
    }
}

impl SettingsScreen {
    pub(super) fn render_gps<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "GPS", theme::ACCENT, ctx);
        let slots = gps_slots(&data.safe_area);

        // Session status: state line + the field lesson as a hint
        // (low-e window glazing blocks GNSS outright - sessions need
        // real sky).
        chamfered_panel(display, slots.status_panel, NOTCH, theme::BORDER, 1);
        tag_label(
            display,
            slots.status_panel.top_left.x, slots.status_panel.top_left.y,
            "STATUS", theme::BORDER, NOTCH,
        );
        // What the receiver is doing NOW. A running session reports
        // itself; between tracking sessions the receiver is powered
        // down waiting on the scheduler, and saying so beats
        // parroting the previous session's outcome. The historical
        // states only show while nothing is scheduled.
        let mut line: String<24> = String::new();
        match data.gps_sync {
            GpsSyncState::Syncing { sats, fix_ok } => {
                let _ = write!(line, "SYNCING - {} SATS", sats);
                if fix_ok { let _ = line.push_str(" FIX"); }
            }
            _ if data.config.gps.tracking_enabled => {
                match data.gps_next_session_secs {
                    Some(s) => { let _ = write!(line, "NEXT IN {}s", s); }
                    // Kick in flight (the task reports within
                    // milliseconds) or first tick pending.
                    None => { let _ = line.push_str("STARTING"); }
                }
            }
            GpsSyncState::Idle => { let _ = line.push_str("READY"); }
            GpsSyncState::Synced { hour, minute } => {
                let _ = write!(line, "SYNCED {}", fmt::hm(hour, minute).as_str());
            }
            GpsSyncState::NoSignal => { let _ = line.push_str("NO SIGNAL"); }
        }
        let value_rect = Rectangle::new(
            Point::new(
                slots.status_panel.top_left.x,
                slots.status_panel.top_left.y + 28,
            ),
            Size::new(slots.status_panel.size.width, 32),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(), line.as_str(), value_rect, theme::FG,
        );
        let hint_rect = Rectangle::new(
            Point::new(
                slots.status_panel.top_left.x,
                slots.status_panel.top_left.y + 64,
            ),
            Size::new(slots.status_panel.size.width, 20),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::caption(), "NEEDS CLEAR SKY", hint_rect, theme::FG_MUTED,
        );

        // SYNC trigger: rendered disabled while a session runs;
        // gps_event drops the tap too (the same screen owns both
        // halves of the disabled contract).
        if matches!(data.gps_sync, GpsSyncState::Syncing { .. }) {
            chamfered_button(
                display, slots.sync_btn, "SYNCING",
                ButtonVariant::Ghost, theme::BORDER,
            );
        } else {
            chamfered_button(
                display, slots.sync_btn, "SYNC NOW",
                ButtonVariant::Primary, theme::ACCENT,
            );
        }

        // Tracking: enable toggle in the tag row, cadence radio row
        // below. The cadence buttons stay live while tracking is
        // off - they set a remembered preference, they don't act.
        chamfered_panel(display, slots.tracking_panel, NOTCH, theme::BORDER, 1);
        tag_label(
            display,
            slots.tracking_panel.top_left.x, slots.tracking_panel.top_left.y,
            "TRACKING", theme::BORDER, NOTCH,
        );
        let t = slots.tracking_toggle;
        toggle(
            display,
            Point::new(
                t.top_left.x + (t.size.width as i32 - TOGGLE_W) / 2,
                t.top_left.y + (t.size.height as i32 - TOGGLE_H) / 2,
            ),
            data.config.gps.tracking_enabled,
        );
        for (i, &(label, cadence)) in TRACKING_CADENCES.iter().enumerate() {
            let variant = if cadence == data.config.gps.tracking_cadence {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Ghost
            };
            chamfered_button(
                display, slots.tracking_buttons[i], label, variant, theme::ACCENT,
            );
        }
    }

    pub(super) fn gps_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Index;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let slots = gps_slots(&data.safe_area);
                if rect_hit(slots.sync_btn, *x, *y) {
                    // Visually disabled while syncing - the tap must
                    // die here too, not just look dead.
                    if matches!(data.gps_sync, GpsSyncState::Syncing { .. }) {
                        return Action::None;
                    }
                    return Action::GpsSync;
                }
                if rect_hit(slots.tracking_toggle, *x, *y) {
                    return Action::ToggleGpsTracking;
                }
                for (i, &(_, cadence)) in TRACKING_CADENCES.iter().enumerate() {
                    if rect_hit(slots.tracking_buttons[i], *x, *y) {
                        if cadence == data.config.gps.tracking_cadence {
                            return Action::None; // already selected
                        }
                        return Action::SetGpsCadence { cadence };
                    }
                }
                Action::None
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Index;
                Action::Redraw
            }
            // The NEXT IN countdown ticks on wall time - repaint per
            // second, but only while tracking makes it visible.
            SystemEvent::TimeUpdated { .. } if data.config.gps.tracking_enabled => {
                Action::Redraw
            }
            _ => Action::None,
        }
    }
}
