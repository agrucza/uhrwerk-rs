//! Reusable UI widgets, grouped by kind.
//!
//! * [`containers`] - chamfered Nightwatch surfaces: `chamfered_panel`,
//!   `tile`, `info_tile`, `tag_label`.
//! * [`bodies`] - content layouts drawn into a rect: `row` and its
//!   `RowControl` variants.
//! * [`chrome`] - screen-level decorations: Nightwatch `header`, top
//!   `status_bar`, bottom `home_indicator`, plus the
//!   `draw_app_chrome` convenience helper.
//! * [`controls`] - interactive primitives: `toggle`, `slider`,
//!   `chamfered_button`.
//! * [`gauge`] - anti-aliased ring gauge (`ring_gauge`).
//! * [`scrollable`] - smooth-scroll body + scrollbar helper.
//! * [`wheel`] - vertical scroll-wheel column for bounded-integer
//!   picking. Composed into multi-column [`picker`]s for HH:MM,
//!   HH:MM:SS, DD/MM/YYYY entry.
//! * [`picker`] - multi-column [`Picker`] that routes drags to the
//!   correct [`wheel::Wheel`] column, plus the standard
//!   `CANCEL | SET` action row used by every picker view.

pub mod bodies;
pub mod chrome;
pub mod containers;
pub mod controls;
pub mod gauge;
pub mod keyboard;
pub mod picker;
pub mod scrollable;
pub mod wheel;

pub use bodies::{row, RowControl, ROW_H};
pub use chrome::{
    app_chrome_back_hit, app_header_rect, corner_safe_header_rect, draw_app_chrome,
    draw_overlay_chrome, header, header_icon_hit,
    home_indicator, status_bar,
    APP_CONTENT_TOP, APP_HEADER_TOP, APP_HOME_BAR_Y, APP_STATUS_X_INSET, APP_STATUS_Y,
    HEADER_H, HOME_INDICATOR_H, OVERLAY_TITLE_Y,
    STATUS_BAR_H,
};
pub use containers::{chamfered_panel, info_tile, tag_label, tile, NOTCH, TAG_LABEL_H};
pub use gauge::ring_gauge;
pub use keyboard::{Keyboard, KeyboardResult};
pub use controls::{
    chamfered_button, slider, slider_value_from_x, toggle, ButtonVariant,
    SLIDER_BAR_H, TOGGLE_H, TOGGLE_W,
};
pub use picker::{action_row_rects, render_action_row, Picker};
pub use scrollable::{handle_scroll_drag, render_scrolled, scroll_max, SCROLLBAR_GUTTER};
pub use wheel::{fmt_2digit, Wheel, WHEEL_CELL_H, WHEEL_TOTAL_H};
