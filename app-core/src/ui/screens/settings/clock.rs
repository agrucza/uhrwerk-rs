//! Clock sub-view: the TIME and DATE metric panels. Tapping either
//! panel seeds and opens the matching wheel picker in
//! [`super::pickers`].

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::ui::{fmt, fonts, layout, theme};
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{chamfered_panel, tag_label, NOTCH, TAG_LABEL_H};

use super::{draw_header, header_back_hit, SettingsScreen, SettingsView, HDR_H};

/// Y of the first clock metric panel below the settings header.
fn clock_panel_top(safe: &crate::data::SafeArea) -> i32 {
    crate::ui::widgets::app_header_top(safe) + HDR_H + 12
}
/// Height of one clock metric panel.
const CLOCK_PANEL_H: i32 = 84;
/// Vertical gap between the two clock metric panels.
const CLOCK_PANEL_GAP: i32 = 12;

fn clock_panel_rect(slot: usize, safe: &crate::data::SafeArea) -> Rectangle {
    let y = clock_panel_top(safe) + slot as i32 * (CLOCK_PANEL_H + CLOCK_PANEL_GAP);
    let x = layout::VSTACK_SIDE_MARGIN;
    let w = theme::SCREEN_W as i32 - layout::VSTACK_SIDE_MARGIN * 2;
    Rectangle::new(
        Point::new(x, y),
        Size::new(w as u32, CLOCK_PANEL_H as u32),
    )
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

        let time_buf = fmt::hms_parts(
            data.time.hour as u64, data.time.minute as u64, data.time.second as u64,
        );
        draw_clock_panel(display, clock_panel_rect(0, &data.safe_area), "TIME", time_buf.as_str());

        let mut date_buf: String<12> = String::new();
        let _ = write!(date_buf, "{:02}.{:02}.{:04}",
            data.time.day, data.time.month, data.time.year);
        draw_clock_panel(display, clock_panel_rect(1, &data.safe_area), "DATE", date_buf.as_str());
    }

    pub(super) fn clock_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            // Keep the display fresh.
            SystemEvent::TimeUpdated { .. } => Action::Redraw,

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
                let p = Point::new(*x as i32, *y as i32);
                if clock_panel_rect(0, &data.safe_area).contains(p) {
                    // Open time picker, seed from current time.
                    let t = &data.time;
                    self.time_picker.wheels[0].set_value(t.hour as i32);
                    self.time_picker.wheels[1].set_value(t.minute as i32);
                    self.time_picker.wheels[2].set_value(t.second as i32);
                    self.view = SettingsView::TimeEntry;
                    Action::Redraw
                } else if clock_panel_rect(1, &data.safe_area).contains(p) {
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
