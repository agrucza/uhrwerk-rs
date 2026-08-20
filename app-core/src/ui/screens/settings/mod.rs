//! Settings screen - device configuration and diagnostics, organised
//! by hardware subsystem.
//!
//! Uses the **internal state machine** pattern: one [`SettingsScreen`]
//! struct holds a [`SettingsView`] enum that tracks which sub-view is
//! currently shown. Tapping a row in the Index sub-view switches
//! `view` to the corresponding sub-view; tapping the back chevron
//! returns to Index.
//!
//! This module owns the state machine, the per-view state fields and
//! the shared chrome helpers; each sub-view lives in its own file and
//! adds its `render_*` / `*_event` pair via an `impl SettingsScreen`
//! block, so [`Screen::render`] / [`Screen::on_event`] here stay a
//! flat dispatch table.
//!
//! Chrome follows the Nightwatch vocabulary: every sub-view shares a
//! [`draw_header`] bar with chevron-left + title + right-aligned
//! telemetry + a 1-px signal hairline underline. The Index itself is
//! a stack of rows (icon / uppercase label / right control);
//! the leaf sub-views use the chamfered metric-panel pattern
//! vocabulary inside, since those fit the tabular diagnostic data
//! better than a flat row list.

mod battery;
mod clock;
mod display;
mod gps;
mod index;
mod mic;
mod motion;
mod pickers;
mod storage;
mod wifi;

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;
use heapless::String;

use crate::ui::{fonts, layout, theme, widgets};
use crate::ui::types::{Action, RenderCtx, Screen, SystemData, SystemEvent};
use crate::ui::widgets::{
    chamfered_panel, draw_app_chrome, header_icon_hit, tag_label, Keyboard, Picker,
    Wheel, NOTCH, ROW_H, SCROLLBAR_GUTTER,
};

use pickers::{days_in_month, DATE_YEAR_MAX, DATE_YEAR_MIN};

// -- Settings chrome helpers -------------------------------------------------

/// Top row of the settings content area (below the shared header),
/// derived from the device safe area.
fn rows_top(safe: &crate::data::SafeArea) -> i32 {
    widgets::app_content_top(safe)
}

/// Top y for every leaf sub-view's first slot. Sits below the
/// header hairline with breathing room.
fn leaf_top_y(safe: &crate::data::SafeArea) -> i32 {
    rows_top(safe) + 18
}

/// Draw the full Settings chrome: top status bar (tinted by `accent`,
/// carrying live HH:MM + battery% from `data`), Nightwatch header
/// with `title` + `SYS.CFG` telemetry, and bottom home-indicator bar.
fn draw_header<D: BlendTarget>(
    display: &mut D,
    data: &SystemData,
    title: &str,
    accent: Color,
    ctx: &RenderCtx,
) {
    // Thin wrapper: settings' only chrome delta is the fixed
    // "SYS.CFG" telemetry string. Everything else - per-tile
    // gating, safe-area insets, corner-safe header rect - lives in
    // the one shared path.
    draw_app_chrome(display, data, title, "SYS.CFG", accent, ctx);
}

/// Rect for the Nth row in the settings Index / Storage sub-index,
/// adjusted by the current scroll offset. `scroll = 0` returns the
/// row's natural position; positive `scroll` shifts everything up
/// (rows below come into view). Width leaves a [`SCROLLBAR_GUTTER`]
/// inset on the right so the row's right-aligned controls have room
/// before the scrollbar.
fn row_rect(index: usize, scroll: i32, safe: &crate::data::SafeArea) -> Rectangle {
    let y = rows_top(safe) + index as i32 * ROW_H - scroll;
    Rectangle::new(
        Point::new(0, y),
        Size::new(
            (theme::SCREEN_W as i32 - SCROLLBAR_GUTTER) as u32,
            ROW_H as u32,
        ),
    )
}

/// Hit test the back chevron in the settings Nightwatch header.
fn header_back_hit(x: u16, y: u16, safe: &crate::data::SafeArea) -> bool {
    header_icon_hit(x, y, widgets::app_header_rect(safe))
}

// -- View enum ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsView {
    Index,
    Imu,
    /// Live microphone level meter - proves the ES7210 capture path.
    MicTest,
    Clock,
    TimeEntry,
    DateEntry,
    /// GPS time sync: status, the SYNC trigger, and the timezone
    /// stepper. Only reachable on boards with the gps capability
    /// (the index row is gated).
    Gps,
    /// Battery status + history graph (samples from the flash event log).
    Battery,
    /// Storage sub-index. Routes to the storage leaves below.
    Storage,
    StorageFlash,
    StorageSd,
    StorageRestoreFlash,
    StorageFactoryReset,
    /// Display preferences (brightness slider + auto-lock). Stub
    /// for now; real contents land in W3d.
    Display,
    /// Wi-Fi: the stored network + session status, SCAN / CONNECT /
    /// FORGET. Gated on the wifi capability.
    Wifi,
    /// The scan result list (NETWORKS); tap an entry to provision it.
    WifiScan,
    /// Full-screen passphrase keyboard for `wifi_ssid_draft`, opened
    /// from the scan list or the stored-network panel.
    WifiPassphrase,
    /// Bluetooth pairing / status. Stub for now; real contents
    /// land when BLE is wired up.
    Bluetooth,
    /// Zigbee mesh status. Only meaningful on the C6 board variant
    /// (S3 has no 802.15.4 radio); shown as a stub on S3 for now,
    /// to be feature-gated when the C6 build path lands.
    Zigbee,
}

// -- SettingsScreen ----------------------------------------------------------

pub struct SettingsScreen {
    view: SettingsView,
    /// HH:MM:SS picker for the TimeEntry sub-view.
    time_picker: Picker<3>,
    /// DD/MM/YYYY picker for the DateEntry sub-view. The DD wheel's
    /// range is recomputed every event so leap-day / 30-vs-31
    /// boundaries stay consistent with the current month + year.
    date_picker: Picker<3>,
    /// Vertical scroll state for the index sub-view.
    index_scroll: layout::ScrollState,
    /// Vertical scroll state for the MOTION sub-view (live readouts +
    /// self-tests stacked together overflow the viewport).
    imu_scroll: layout::ScrollState,
    /// Vertical scroll state for the BATTERY sub-view (NOW / CHARGE /
    /// graph / UPTIME / ACTIVE / SLEEPS overflow the viewport).
    battery_scroll: layout::ScrollState,
    /// Counter that throttles MOTION-sub-view redraws to a fraction
    /// of the IMU's 20 Hz `MotionUpdated` cadence.
    motion_phase: u8,
    /// Last MotionData rendered into the MOTION sub-view, used to
    /// suppress redraws when the values haven't changed materially.
    motion_last: Option<crate::data::MotionData>,
    /// MicTest sub-view LOOP toggle: true while mic -> speaker
    /// loopback is the active meter mode (vs. meter-only capture).
    mic_loopback: bool,
    /// Entry state of the WifiPassphrase sub-view's text field. 63 =
    /// the WPA2 passphrase maximum.
    wifi_passphrase_keyboard: Keyboard,
    /// The network the passphrase keyboard is provisioning: the
    /// scan entry that was tapped, or the stored SSID when re-editing
    /// its passphrase. Committed together with the keyboard text as
    /// `Action::SetWifiCredentials`.
    wifi_ssid_draft: String<{ crate::config::WifiConfig::SSID_MAX }>,
    /// Vertical scroll state for the NETWORKS list.
    wifi_scroll: layout::ScrollState,
}

impl SettingsScreen {
    pub fn new() -> Self {
        Self {
            view: SettingsView::Index,
            time_picker: Picker::new([
                Wheel::new(0, 23, 0).with_wrap(true),
                Wheel::new(0, 59, 0).with_wrap(true),
                Wheel::new(0, 59, 0).with_wrap(true),
            ]),
            date_picker: Picker::new([
                Wheel::new(1, 31, 1),
                Wheel::new(1, 12, 1).with_wrap(true),
                Wheel::new(DATE_YEAR_MIN, DATE_YEAR_MAX, DATE_YEAR_MIN),
            ]),
            index_scroll: layout::ScrollState::new(),
            imu_scroll: layout::ScrollState::new(),
            battery_scroll: layout::ScrollState::new(),
            motion_phase: 0,
            motion_last: None,
            mic_loopback: false,
            wifi_passphrase_keyboard: Keyboard::new(
                crate::config::WifiConfig::PASSPHRASE_MAX,
            ),
            wifi_ssid_draft: String::new(),
            wifi_scroll: layout::ScrollState::new(),
        }
    }

    /// Re-clamp the date picker's day wheel to the days-in-month for
    /// the currently-selected month + year. Called after any event
    /// that may have changed the month or year wheel.
    fn refresh_date_day_range(&mut self) {
        let month = self.date_picker.wheels[1].value();
        let year = self.date_picker.wheels[2].value();
        let max = days_in_month(month, year);
        self.date_picker.wheels[0].set_range(1, max);
    }
}

// -- Screen impl -------------------------------------------------------------

impl Screen for SettingsScreen {
    fn render<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        // Only the scrolled-list sub-views (currently Index; Imu / Battery
        // soon) thread `ctx` through their renderers - other sub-views
        // have fixed-position content where the driver's per-pixel clip
        // already handles the off-tile case at zero CPU cost.
        match self.view {
            SettingsView::Index => self.render_index(display, data, ctx),
            SettingsView::Imu => self.render_imu(display, data, ctx),
            SettingsView::MicTest => self.render_mic_test(display, data, ctx),
            SettingsView::Clock => self.render_clock(display, data, ctx),
            SettingsView::Gps => self.render_gps(display, data, ctx),
            SettingsView::TimeEntry => self.render_time_entry(display, data, ctx),
            SettingsView::DateEntry => self.render_date_entry(display, data, ctx),
            SettingsView::Battery => self.render_battery(display, data, ctx),
            SettingsView::Storage => self.render_storage_index(display, data, ctx),
            SettingsView::StorageFlash => self.render_storage_flash(display, data, ctx),
            SettingsView::StorageSd => self.render_storage_sd(display, data, ctx),
            SettingsView::StorageRestoreFlash => self.render_storage_restore(display, data, ctx),
            SettingsView::StorageFactoryReset => self.render_storage_factory_reset(display, data, ctx),
            SettingsView::Display   => self.render_display(display, data, ctx),
            SettingsView::Wifi      => self.render_wifi(display, data, ctx),
            SettingsView::WifiScan  => self.render_wifi_scan(display, data, ctx),
            SettingsView::WifiPassphrase => self.render_wifi_passphrase(display, data, ctx),
            SettingsView::Bluetooth => self.render_stub(display, data, "BLUETOOTH", ctx),
            SettingsView::Zigbee    => self.render_stub(display, data, "ZIGBEE", ctx),
        }
    }

    fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        if matches!(event, SystemEvent::PowerButtonLong) {
            return Action::Shutdown;
        }

        match self.view {
            SettingsView::Index => self.index_event(event, data),
            SettingsView::Imu => self.imu_event(event, data),
            SettingsView::MicTest => self.mic_test_event(event, data),
            SettingsView::Clock => self.clock_event(event, data),
            SettingsView::Gps => self.gps_event(event, data),
            SettingsView::TimeEntry => self.time_entry_event(event, data),
            SettingsView::DateEntry => self.date_entry_event(event, data),
            SettingsView::Battery => self.battery_event(event, data),
            SettingsView::Storage => self.storage_index_event(event, data),
            SettingsView::StorageFlash => self.storage_flash_event(event, data),
            SettingsView::StorageSd => self.storage_sd_event(event, data),
            SettingsView::StorageRestoreFlash => self.storage_restore_event(event, data),
            SettingsView::StorageFactoryReset => self.storage_factory_reset_event(event, data),
            SettingsView::Display => self.display_event(event, data),
            SettingsView::Wifi => self.wifi_event(event, data),
            SettingsView::WifiScan => self.wifi_scan_event(event, data),
            SettingsView::WifiPassphrase => self.wifi_passphrase_event(event, data),
            SettingsView::Bluetooth
            | SettingsView::Zigbee => self.stub_event(event, data),
        }
    }
}

// -- Stub sub-views (Bluetooth / Zigbee) -------------------------------------
//
// Placeholders so the Settings index can navigate to these rows
// before their real contents land. Renders a grey tag-labeled
// panel saying "WIP" with the view's title; back chevron and
// right-swipe both pop to the index.

impl SettingsScreen {
    fn render_stub<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        title: &str,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, title, theme::ACCENT, ctx);
        let mut s = layout::VStack::new(leaf_top_y(&data.safe_area));
        let panel = s.slot(80);
        chamfered_panel(display, panel, NOTCH, theme::BORDER, 1);
        tag_label(
            display,
            panel.top_left.x,
            panel.top_left.y,
            "WIP",
            theme::BORDER,
            NOTCH,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            "TODO", panel, theme::FG_DIM,
        );
    }

    fn stub_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
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
            _ => Action::None,
        }
    }
}
