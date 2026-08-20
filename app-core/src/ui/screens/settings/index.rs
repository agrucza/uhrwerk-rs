//! Settings Index sub-view: the scrollable row list that routes to
//! every other sub-view, plus the shared [`IndexRow`] row model and
//! its render / hit-test helpers (also used by the Storage sub-index).

use embedded_graphics::{
    geometry::Point,
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;
use heapless::String;
use core::fmt::Write;

use crate::ui::{fmt, glyphs, theme, widgets};
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    handle_scroll_drag, render_scrolled, row, RowControl, ROW_H,
};

use super::{draw_header, header_back_hit, row_rect, rows_top, SettingsScreen, SettingsView};

// -- Index row metadata ------------------------------------------------------

/// Per-row icon. Rust can't coerce a generic `fn(..D..)` into a
/// non-generic function pointer, so we enum-dispatch to pick one of
/// a closed set of glyphs at render time (same pattern the App
/// Drawer uses).
#[derive(Clone, Copy)]
pub(super) enum RowIcon {
    Clock,
    Battery,
    Imu,
    Mic,
    Storage,
    Flash,
    SdCard,
    Restore,
    Skull,
    Sounds,
    Vibrate,
    Dnd,
    Display,
    Wifi,
    Bluetooth,
    Zigbee,
    Gps,
}

fn draw_row_icon<D: BlendTarget>(
    display: &mut D, kind: RowIcon, cx: i32, cy: i32, color: Color,
) {
    let r = 8;
    match kind {
        RowIcon::Clock     => glyphs::clock(display, cx, cy, r, color),
        RowIcon::Battery   => glyphs::battery(display, cx, cy, r, color),
        RowIcon::Imu       => glyphs::imu(display, cx, cy, r, color),
        RowIcon::Mic       => glyphs::bell(display, cx, cy, r, color),
        RowIcon::Storage   => glyphs::chip(display, cx, cy, r, color),
        RowIcon::Flash     => glyphs::chip(display, cx, cy, r, color),
        RowIcon::SdCard    => glyphs::sd_card(display, cx, cy, r, color),
        RowIcon::Restore   => glyphs::chip(display, cx, cy, r, color),
        RowIcon::Skull     => glyphs::skull(display, cx, cy, r, color),
        RowIcon::Sounds    => glyphs::bell(display, cx, cy, r, color),
        RowIcon::Vibrate   => glyphs::phone(display, cx, cy, r, color),
        RowIcon::Dnd       => glyphs::dnd(display, cx, cy, r, color),
        RowIcon::Display   => glyphs::bolt(display, cx, cy, r, color),
        RowIcon::Wifi      => glyphs::signal_small(display, cx, cy, r, color),
        RowIcon::Bluetooth => glyphs::bluetooth_small(display, cx, cy, r, color),
        RowIcon::Zigbee    => glyphs::zigbee(display, cx, cy, r, color),
        RowIcon::Gps       => glyphs::signal_small(display, cx, cy, r, color),
    }
}

/// What an index row does when tapped, plus how its right-control
/// renders. Navigate rows open a sub-view and show an inline status
/// value; toggle rows flip a config bool inline (no nav).
#[derive(Clone)]
pub(super) enum RowKind {
    /// Tap opens `target`; the right side shows the inline value
    /// returned by `value_fn` (empty string => bare row, renders a
    /// chevron instead).
    Navigate {
        target: SettingsView,
        value_fn: fn(&SystemData) -> String<20>,
    },
    /// Tap fires `action` (typically a `Toggle*` config mutation).
    /// The right side shows a Nightwatch toggle reflecting `is_on`.
    Toggle {
        is_on: fn(&SystemData) -> bool,
        action: Action,
    },
}

pub(super) struct IndexRow {
    pub(super) label: &'static str,
    pub(super) icon: RowIcon,
    /// Capability gate: the row renders and hit-tests only when this
    /// returns true (rows below it shift up). `always` for standard
    /// rows; capability probes (e.g. [`has_gps`]) for rows whose
    /// hardware only some boards carry.
    pub(super) visible: fn(&SystemData) -> bool,
    pub(super) kind: RowKind,
}

/// Standard-row visibility: every board has this hardware.
pub(super) fn always(_data: &SystemData) -> bool {
    true
}

/// GPS rows exist only on boards whose bin declared the capability.
fn has_gps(data: &SystemData) -> bool {
    data.capabilities.gps
}

/// The WIFI row exists only in builds that spawn the radio task.
fn has_wifi(data: &SystemData) -> bool {
    data.capabilities.wifi
}

fn clock_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = buf.push_str(
        fmt::hms_parts(
            data.time.hour as u64, data.time.minute as u64, data.time.second as u64,
        ).as_str(),
    );
    buf
}

/// Index-row inline value for GPS: what the receiver is doing. The
/// timezone offset used to sit here, but it is a clock property and
/// now lives in the CLOCK view with the rest of the time settings -
/// this row must not advertise it any more.
fn gps_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    if matches!(data.gps_sync, crate::data::GpsSyncState::Syncing { .. }) {
        let _ = buf.push_str("SYNCING");
    } else if data.config.gps.tracking_enabled {
        let _ = buf.push_str("TRACKING");
    }
    // Otherwise empty - the row renders a chevron like the other
    // plain navigate rows.
    buf
}

fn imu_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = buf.push_str(if data.imu_name.is_empty() { "IMU" } else { data.imu_name });
    buf
}

fn battery_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    match data.power.battery_percent {
        Some(pct) => { let _ = write!(buf, "{}%", pct); }
        None      => { let _ = buf.push_str("--"); }
    }
    buf
}

fn storage_value(data: &SystemData) -> String<20> {
    // Summary shown on the top-level settings index: "<files> / <size> KB".
    let mut buf = String::new();
    let _ = write!(
        buf,
        "{} / {} KB",
        data.storage.files,
        data.storage.total_bytes / 1024,
    );
    buf
}

fn haptics_is_on(data: &SystemData) -> bool {
    data.config.alerts.haptics_enabled
}

fn sound_is_on(data: &SystemData) -> bool {
    data.config.alerts.sound_enabled
}

fn dnd_is_on(data: &SystemData) -> bool {
    data.config.alerts.dnd
}

/// Empty inline value - causes navigate rows to render a chevron
/// instead of an inline string.
fn empty_value(_data: &SystemData) -> String<20> { String::new() }

/// The stored network name on the WIFI row (chevron when none).
fn wifi_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    for c in data.config.wifi.ssid.chars().take(16) {
        let _ = buf.push(c);
    }
    buf
}

const INDEX_ROWS: &[IndexRow] = &[
    // Spec prefs first - most-used live up top.
    IndexRow {
        label: "DISPLAY",
        icon: RowIcon::Display,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::Display, value_fn: empty_value },
    },
    IndexRow {
        label: "SOUNDS",
        icon: RowIcon::Sounds,
        visible: always,
        kind: RowKind::Toggle { is_on: sound_is_on, action: Action::ToggleSound },
    },
    IndexRow {
        label: "VIBRATE",
        icon: RowIcon::Vibrate,
        visible: always,
        kind: RowKind::Toggle { is_on: haptics_is_on, action: Action::ToggleHaptics },
    },
    IndexRow {
        label: "DND",
        icon: RowIcon::Dnd,
        visible: always,
        kind: RowKind::Toggle { is_on: dnd_is_on, action: Action::ToggleDnd },
    },
    IndexRow {
        label: "WIFI",
        icon: RowIcon::Wifi,
        visible: has_wifi,
        kind: RowKind::Navigate { target: SettingsView::Wifi, value_fn: wifi_value },
    },
    IndexRow {
        label: "BLUETOOTH",
        icon: RowIcon::Bluetooth,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::Bluetooth, value_fn: empty_value },
    },
    IndexRow {
        label: "ZIGBEE",
        icon: RowIcon::Zigbee,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::Zigbee, value_fn: empty_value },
    },
    // Diagnostic / drill rows.
    IndexRow {
        label: "CLOCK",
        icon: RowIcon::Clock,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::Clock, value_fn: clock_value },
    },
    IndexRow {
        label: "GPS",
        icon: RowIcon::Gps,
        visible: has_gps,
        kind: RowKind::Navigate { target: SettingsView::Gps, value_fn: gps_value },
    },
    IndexRow {
        label: "BATTERY",
        icon: RowIcon::Battery,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::Battery, value_fn: battery_value },
    },
    IndexRow {
        label: "MOTION",
        icon: RowIcon::Imu,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::Imu, value_fn: imu_value },
    },
    IndexRow {
        label: "MIC TEST",
        icon: RowIcon::Mic,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::MicTest, value_fn: empty_value },
    },
    IndexRow {
        label: "STORAGE",
        icon: RowIcon::Storage,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::Storage, value_fn: storage_value },
    },
    // Destructive action - last, danger-tinted icon. Re-uses the
    // existing Factory Reset sub-view (the spec's Purge+Reset and
    // our Factory Reset are the same destructive action).
    IndexRow {
        label: "PURGE+RESET",
        icon: RowIcon::Skull,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::StorageFactoryReset, value_fn: empty_value },
    },
];

// -- Index sub-view ----------------------------------------------------------

impl SettingsScreen {
    pub(super) fn render_index<D: BlendTarget>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "SETTINGS", theme::ACCENT, ctx);
        render_scrolled(
            display, self.index_scroll.offset(),
            index_viewport_rect(&data.safe_area), index_content_h(data), theme::ACCENT, ctx,
            |clipped, scroll| render_rows(clipped, data, INDEX_ROWS, scroll, ctx),
        );
    }

    pub(super) fn index_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                Action::Back
            }
            SystemEvent::Tap { x, y } => {
                if let Some(action) = row_hit(
                    *x, *y, INDEX_ROWS, data,
                    self.index_scroll.offset(),
                    &index_viewport_rect(&data.safe_area),
                    &mut self.view,
                ) {
                    // Opening the mic-test view starts capture: the
                    // model turns this into StartCapture so the audio
                    // task begins streaming MicLevel. The LOOP toggle
                    // always starts fresh (off).
                    if matches!(self.view, SettingsView::MicTest) {
                        self.mic_loopback = false;
                        return Action::StartMicTest;
                    }
                    return action;
                }
                Action::None
            }
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let viewport_h = index_viewport_rect(&data.safe_area).size.height as i32;
                if handle_scroll_drag(
                    &mut self.index_scroll, event, viewport_h, index_content_h(data),
                ) {
                    return Action::Redraw;
                }
                Action::None
            }
            _ => Action::None,
        }
    }
}

/// Visible-row viewport rect for the index. Spans from just below
/// the header hairline to just above the home-indicator bar.
pub(super) fn index_viewport_rect(safe: &crate::data::SafeArea) -> Rectangle {
    widgets::viewport_to_home_bar(rows_top(safe), safe)
}

/// Total content height of the index row list - visible rows only,
/// so scroll extents track capability gating.
fn index_content_h(data: &SystemData) -> i32 {
    INDEX_ROWS.iter().filter(|r| (r.visible)(data)).count() as i32 * ROW_H
}

// -- Shared row rendering / hit-testing for index + storage sub-index --------

/// Render a stack of [`IndexRow`]s using `nightwatch::row`. Navigate
/// rows show an inline value (or a chevron when the value is empty);
/// toggle rows show a Nightwatch toggle. `scroll` shifts each row's
/// y by `-scroll` so the caller can render into a clipped viewport.
pub(super) fn render_rows<D: BlendTarget>(
    display: &mut D, data: &SystemData, rows: &[IndexRow], scroll: i32, ctx: &RenderCtx,
) {
    let mut pos = 0;
    for r in rows.iter() {
        // Capability-gated rows vanish entirely; rows below shift up
        // (`pos` counts only visible rows).
        if !(r.visible)(data) {
            continue;
        }
        let rect = row_rect(pos, scroll, &data.safe_area);
        pos += 1;
        // Skip rows whose y-range falls entirely outside this tile.
        // This is where the tile-aware optimization lives: without it,
        // a 10-row index walks all 10 rows for each of the 11 tiles
        // during scroll, paying per-row format/icon/iterator-construction
        // cost. With it, each tile walks ~2 rows.
        let row_y0 = rect.top_left.y;
        let row_y1 = row_y0 + rect.size.height as i32;
        if !ctx.intersects_y(row_y0, row_y1) {
            continue;
        }
        let kind = r.icon;
        match r.kind {
            RowKind::Navigate { value_fn, .. } => {
                let val = value_fn(data);
                let control = if val.is_empty() {
                    RowControl::Chevron(theme::INFO)
                } else {
                    RowControl::Inline(val.as_str(), theme::FG_MUTED)
                };
                row(
                    display, rect,
                    |d, cx, cy, c| draw_row_icon(d, kind, cx, cy, c),
                    theme::INFO,
                    r.label,
                    control,
                );
            }
            RowKind::Toggle { is_on, .. } => {
                row(
                    display, rect,
                    |d, cx, cy, c| draw_row_icon(d, kind, cx, cy, c),
                    theme::INFO,
                    r.label,
                    RowControl::Toggle(is_on(data)),
                );
            }
        }
    }
}

/// Row hit test, scroll-aware. Returns the `Action` the tap should
/// produce, or `None` if the tap missed every row. Taps outside
/// `viewport` are rejected (so a tap landing on the chrome area
/// doesn't accidentally trigger a row that happens to be scrolled
/// into the chrome's pixels). Navigate rows update the caller's
/// `view` via the `&mut SettingsView` and return `Action::Redraw`;
/// toggle rows return their own action variant.
pub(super) fn row_hit(
    x: u16, y: u16, rows: &[IndexRow], data: &SystemData,
    scroll: i32, viewport: &Rectangle,
    view: &mut SettingsView,
) -> Option<Action> {
    let pt = Point::new(x as i32, y as i32);
    if !viewport.contains(pt) { return None; }
    let mut pos = 0;
    for r in rows.iter() {
        // Mirror render_rows exactly: same visible-position
        // indexing, or draw and hit-test drift apart.
        if !(r.visible)(data) {
            continue;
        }
        let rect = row_rect(pos, scroll, &data.safe_area);
        pos += 1;
        if !rect.contains(pt) { continue; }
        return Some(match &r.kind {
            RowKind::Navigate { target, .. } => {
                *view = *target;
                Action::Redraw
            }
            RowKind::Toggle { action, .. } => action.clone(),
        });
    }
    None
}
