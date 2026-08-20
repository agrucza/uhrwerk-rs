//! Time / date entry sub-views: the three-column wheel pickers plus
//! their shared column geometry. Both commit through
//! `Action::SetTime`, keeping the untouched half of the timestamp
//! from `data.time`.

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;
use core::fmt::Write;

use crate::ui::{fonts, layout, theme};
use crate::ui::layout::rect_hit;
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    action_row_rects, fmt_2digit, render_action_row, WHEEL_TOTAL_H,
};

use super::{draw_header, header_back_hit, rows_top, SettingsScreen, SettingsView};

// -- Picker layout ----------------------------------------------------------

/// Top y of the wheel picker in time/date entry views, vertically
/// centred between the header bottom and the action row.
fn picker_top(safe: &crate::data::SafeArea) -> i32 {
    let ct = rows_top(safe);
    ct + (layout::BOTTOM_TILE_Y - ct - WHEEL_TOTAL_H) / 2
}

/// Width of one wheel column in the time/date picker.
const PICKER_COL_W: i32 = 72;

/// Horizontal gap between adjacent picker columns.
const PICKER_GAP: i32 = 28;

/// Total horizontal extent of a three-column picker.
const PICKER_TOTAL_W: i32 = PICKER_COL_W * 3 + PICKER_GAP * 2;

/// Year range surfaced in the date picker. PCF85063 is good past
/// 2099 but the year wheel scrolls become tedious past then; pick
/// the same century the firmware was built in.
pub(super) const DATE_YEAR_MIN: i32 = 2000;
pub(super) const DATE_YEAR_MAX: i32 = 2099;

/// Per-column rects for the time/date wheel picker, centered horizontally
/// inside the SCREEN_W band.
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

/// Draw a single-character separator (`":"` for time, `"."` for date)
/// between adjacent picker columns, sitting on the wheels' selection
/// band centerline.
fn draw_picker_separators<D: BlendTarget>(
    display: &mut D,
    cells: &[Rectangle; 3],
    sep: &str,
    accent: Color,
) {
    let band_cy = cells[0].top_left.y + cells[0].size.height as i32 / 2;
    for i in 0..2 {
        let cx = (cells[i].top_left.x + cells[i].size.width as i32
            + cells[i + 1].top_left.x) / 2;
        let sep_rect = Rectangle::new(
            Point::new(cx - 8, band_cy - 16),
            Size::new(16, 32),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(), sep, sep_rect, accent,
        );
    }
}

/// Days in the given Gregorian month/year. Year is assumed to be in
/// [`DATE_YEAR_MIN`]..=[`DATE_YEAR_MAX`] (the 21st century, no
/// century-divisible-by-400 boundary in range), so leap-year reduces
/// to `year % 4 == 0`.
pub(super) fn days_in_month(month: i32, year: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

// -- Time entry picker -------------------------------------------------------

impl SettingsScreen {
    pub(super) fn render_time_entry<D: BlendTarget>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "SET TIME", theme::ACCENT, ctx);

        let cells = picker_cell_rects(&data.safe_area);
        self.time_picker.wheels[0].render(display, cells[0], theme::ACCENT, fmt_2digit);
        self.time_picker.wheels[1].render(display, cells[1], theme::ACCENT, fmt_2digit);
        self.time_picker.wheels[2].render(display, cells[2], theme::ACCENT, fmt_2digit);
        draw_picker_separators(display, &cells, ":", theme::ACCENT);

        render_action_row(display, theme::ACCENT);
    }

    pub(super) fn time_entry_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Clock;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let (cancel, set) = action_row_rects();
                if rect_hit(cancel, *x, *y) {
                    self.view = SettingsView::Clock;
                    return Action::Redraw;
                }
                if rect_hit(set, *x, *y) {
                    let h = self.time_picker.wheels[0].value() as u8;
                    let m = self.time_picker.wheels[1].value() as u8;
                    let s = self.time_picker.wheels[2].value() as u8;
                    self.view = SettingsView::Clock;
                    return Action::SetTime {
                        year: data.time.year,
                        month: data.time.month,
                        day: data.time.day,
                        hour: h,
                        minute: m,
                        second: s,
                    };
                }

                let cells = picker_cell_rects(&data.safe_area);
                if self.time_picker.handle_event(event, &cells) {
                    return Action::Redraw;
                }
                Action::None
            }
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let cells = picker_cell_rects(&data.safe_area);
                if self.time_picker.handle_event(event, &cells) {
                    return Action::Redraw;
                }
                Action::None
            }
            _ => Action::None,
        }
    }
}

// -- Date entry picker -------------------------------------------------------

impl SettingsScreen {
    pub(super) fn render_date_entry<D: BlendTarget>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "SET DATE", theme::ACCENT, ctx);

        let cells = picker_cell_rects(&data.safe_area);
        self.date_picker.wheels[0].render(display, cells[0], theme::ACCENT, fmt_2digit);
        self.date_picker.wheels[1].render(display, cells[1], theme::ACCENT, fmt_2digit);
        // Year wheel has 4-digit values - format unpadded to fit
        // the 72 px column at value-font size.
        self.date_picker.wheels[2].render(display, cells[2], theme::ACCENT, |v, buf| {
            let _ = write!(buf, "{}", v);
        });
        draw_picker_separators(display, &cells, ".", theme::ACCENT);

        render_action_row(display, theme::ACCENT);
    }

    pub(super) fn date_entry_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Clock;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let (cancel, set) = action_row_rects();
                if rect_hit(cancel, *x, *y) {
                    self.view = SettingsView::Clock;
                    return Action::Redraw;
                }
                if rect_hit(set, *x, *y) {
                    let d = self.date_picker.wheels[0].value() as u8;
                    let m = self.date_picker.wheels[1].value() as u8;
                    let y = self.date_picker.wheels[2].value() as u16;
                    self.view = SettingsView::Clock;
                    return Action::SetTime {
                        year: y,
                        month: m,
                        day: d,
                        hour: data.time.hour,
                        minute: data.time.minute,
                        second: data.time.second,
                    };
                }

                let cells = picker_cell_rects(&data.safe_area);
                if self.date_picker.handle_event(event, &cells) {
                    self.refresh_date_day_range();
                    return Action::Redraw;
                }
                Action::None
            }
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let cells = picker_cell_rects(&data.safe_area);
                if self.date_picker.handle_event(event, &cells) {
                    self.refresh_date_day_range();
                    return Action::Redraw;
                }
                Action::None
            }
            _ => Action::None,
        }
    }
}
