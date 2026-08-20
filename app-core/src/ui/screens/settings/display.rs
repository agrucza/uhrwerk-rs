//! Display sub-view: brightness slider, auto-lock option row, and
//! the NIGHT MODE / ALWAYS-ON toggle rows.

use embedded_graphics::{
    geometry::Point,
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::ui::{glyphs, layout, theme};
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    chamfered_button, chamfered_panel, row, slider, slider_value_from_x, tag_label,
    ButtonVariant, RowControl, NOTCH, ROW_H, SLIDER_BAR_H,
};

use crate::ui::primitives::{brightness_pct, BRIGHT_MIN_PCT};

use super::{draw_header, header_back_hit, leaf_top_y, SettingsScreen, SettingsView};

/// Auto-lock options surfaced in the Display sub-view, in the
/// order they appear (left -> right). `secs` is the off_timeout the
/// Model writes when this option is picked.
struct AutoLockOption {
    label: &'static str,
    secs: u32,
}

const AUTO_LOCK_OPTIONS: &[AutoLockOption] = &[
    AutoLockOption { label: "15S", secs: 15 },
    AutoLockOption { label: "30S", secs: 30 },
    AutoLockOption { label: "1M",  secs: 60 },
    AutoLockOption { label: "2M",  secs: 120 },
];

/// All rects the Display sub-view needs. Render and event handlers
/// both call [`display_slots`] and read the same fields, so geometry
/// can never drift between draw and hit-test.
struct DisplaySlots {
    /// Outer chamfered panel for the brightness section.
    brightness_panel: Rectangle,
    /// The slider trough inside `brightness_panel`. Same rect is
    /// passed to [`slider`] and [`slider_value_from_x`].
    brightness_bar: Rectangle,
    /// Outer chamfered panel for the auto-lock section.
    auto_lock_panel: Rectangle,
    /// One rect per [`AUTO_LOCK_OPTIONS`] entry, in order.
    auto_lock_buttons: [Rectangle; 4],
    night_mode_row: Rectangle,
    always_on_row: Rectangle,
}

fn display_slots(safe: &crate::data::SafeArea) -> DisplaySlots {
    // Top section: margined VStack for the two chamfered panels.
    let mut top = layout::VStack::new(leaf_top_y(safe));
    let brightness_panel = top.slot(70);
    top.gap(14);
    let auto_lock_panel = top.slot(80);
    top.gap(16);

    // Brightness slider hugs the bottom of its panel.
    let bar_y = brightness_panel.top_left.y
        + brightness_panel.size.height as i32 - SLIDER_BAR_H - 14;
    let brightness_bar = layout::VStack::inside(brightness_panel, 14, bar_y)
        .slot(SLIDER_BAR_H);

    // Auto-lock buttons: 4-up row hugging the bottom of the panel,
    // laid out via a sub-VStack scoped to the panel's interior.
    let btn_h = 30i32;
    let row_y = auto_lock_panel.top_left.y
        + auto_lock_panel.size.height as i32 - btn_h - 12;
    let mut inner = layout::VStack::inside(auto_lock_panel, 10, row_y);
    let auto_lock_buttons = inner.row::<4>(btn_h, 8);

    // Toggle rows below the panels: full-width, edge-to-edge, so
    // they read as list rows rather than floating panels - matches
    // the Settings index style.
    let mut rows = layout::VStack::with_margin(top.cursor_y(), 0);
    let night_mode_row = rows.slot(ROW_H);
    let always_on_row = rows.slot(ROW_H);

    DisplaySlots {
        brightness_panel,
        brightness_bar,
        auto_lock_panel,
        auto_lock_buttons,
        night_mode_row,
        always_on_row,
    }
}

impl SettingsScreen {
    pub(super) fn render_display<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "DISPLAY", theme::ACCENT, ctx);

        let slots = display_slots(&data.safe_area);
        let always_on = data.config.display.always_on;

        // Brightness panel: tag-labelled chamfered panel with the
        // generic slider widget inside.
        let panel = slots.brightness_panel;
        chamfered_panel(display, panel, NOTCH, theme::ACCENT, 1);
        tag_label(
            display,
            panel.top_left.x,
            panel.top_left.y,
            "BRIGHTNESS",
            theme::ACCENT,
            NOTCH,
        );
        let pct = brightness_pct(data);
        let max_pct = data.config.display.max_brightness_pct();
        let mut label: String<8> = String::new();
        let _ = write!(label, "{}%", pct);
        slider(
            display, slots.brightness_bar,
            pct, BRIGHT_MIN_PCT, max_pct,
            Some(label.as_str()),
        );

        // Auto-lock panel: chamfered with 4 buttons inside. Selected
        // option = Primary, others = Ghost. When always_on is on,
        // ALL buttons render Ghost (and reject taps in display_event)
        // because the auto-lock timer is bypassed entirely - keeping
        // a Primary highlight would lie about what's active.
        let panel = slots.auto_lock_panel;
        chamfered_panel(display, panel, NOTCH, theme::ACCENT, 1);
        tag_label(
            display,
            panel.top_left.x,
            panel.top_left.y,
            "AUTO-LOCK",
            theme::ACCENT,
            NOTCH,
        );
        let current_secs = data.config.display.off_timeout_s as u32;
        for (i, opt) in AUTO_LOCK_OPTIONS.iter().enumerate() {
            let variant = if always_on || opt.secs != current_secs {
                ButtonVariant::Ghost
            } else {
                ButtonVariant::Primary
            };
            chamfered_button(
                display, slots.auto_lock_buttons[i], opt.label,
                variant, theme::ACCENT,
            );
        }

        // NIGHT MODE + ALWAYS-ON: full-width toggle rows below the
        // panels. Reuses the same `row` widget the Settings index
        // uses so they read as list items rather than another panel.
        row(
            display, slots.night_mode_row,
            |d, cx, cy, c| glyphs::moon(d, cx, cy, 8, c),
            theme::INFO,
            "NIGHT MODE",
            RowControl::Toggle(data.config.display.night_mode),
        );
        row(
            display, slots.always_on_row,
            |d, cx, cy, c| glyphs::power(d, cx, cy, 8, c),
            theme::INFO,
            "ALWAYS-ON",
            RowControl::Toggle(always_on),
        );
    }

    pub(super) fn display_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
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
            // Live brightness scrubbing - same idiom as Quick Access.
            SystemEvent::TouchPressed { x, y } => {
                let max_pct = data.config.display.max_brightness_pct();
                if let Some(v) = slider_value_from_x(
                    display_slots(&data.safe_area).brightness_bar,
                    *x as i32, *y as i32,
                    BRIGHT_MIN_PCT, max_pct,
                ) {
                    if v != brightness_pct(data) {
                        return Action::SetBrightness { percent: v };
                    }
                }
                Action::None
            }
            SystemEvent::Tap { x, y } => {
                let pt = Point::new(*x as i32, *y as i32);
                let slots = display_slots(&data.safe_area);
                let always_on = data.config.display.always_on;

                // Auto-lock buttons: rejected when always_on is on
                // (matches the Ghost rendering). Otherwise switch to
                // the tapped option.
                if !always_on {
                    for (i, opt) in AUTO_LOCK_OPTIONS.iter().enumerate() {
                        if slots.auto_lock_buttons[i].contains(pt) {
                            if data.config.display.off_timeout_s as u32 == opt.secs {
                                return Action::None; // already selected
                            }
                            return Action::SetAutoLock { secs: opt.secs };
                        }
                    }
                }

                // Toggle rows.
                if slots.night_mode_row.contains(pt) {
                    return Action::ToggleNightMode;
                }
                if slots.always_on_row.contains(pt) {
                    return Action::ToggleAlwaysOn;
                }
                Action::None
            }
            _ => Action::None,
        }
    }
}
