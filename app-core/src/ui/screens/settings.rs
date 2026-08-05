//! Settings screen - device configuration and diagnostics, organised
//! by hardware subsystem.
//!
//! Uses the **internal state machine** pattern: one [`SettingsScreen`]
//! struct holds a [`SettingsView`] enum that tracks which sub-view is
//! currently shown. Tapping a row in the Index sub-view switches
//! `view` to the corresponding sub-view; tapping the back chevron
//! returns to Index.
//!
//! Chrome follows the Nightwatch vocabulary: every sub-view shares a
//! [`header`] bar with chevron-left + title + right-aligned
//! telemetry + a 1-px signal hairline underline. The Index itself is
//! a stack of [`row`]s (icon / uppercase label / right control);
//! the leaf sub-views use the chamfered metric-panel pattern
//! vocabulary inside, since those fit the tabular diagnostic data
//! better than a flat row list.

use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{Point, Size},
    pixelcolor::Rgb565,
    prelude::Primitive,
    primitives::{Line, PrimitiveStyle, Rectangle, StyledDrawable},
    Drawable,
};
use heapless::String;
use core::fmt::Write;

use crate::data::GpsSyncState;
use crate::ui::{fonts, glyphs, layout, theme};
use crate::ui::types::{
    Action, RenderCtx, Screen, SelfTestId, SelfTestResult, SystemData, SystemEvent,
};
use crate::ui::widgets::{
    action_row_rects, chamfered_button, chamfered_panel, fmt_2digit, handle_scroll_drag,
    header, header_icon_hit, home_indicator, render_action_row, render_scrolled, row,
    slider, slider_value_from_x, status_bar, tag_label, ButtonVariant, Picker, RowControl,
    Wheel, NOTCH, ROW_H, SCROLLBAR_GUTTER, SLIDER_BAR_H, STATUS_BAR_H, TAG_LABEL_H,
    WHEEL_TOTAL_H,
};

/// Slider lower bound for brightness in the Display sub-view -
/// matches Quick Access. Anything dimmer is unreadable in practice
/// so the slider never goes below 5 %.
const BRIGHT_MIN_PCT: u8 = 5;

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

// -- Settings chrome helpers -------------------------------------------------

/// Y of the top status bar shared by every sub-view.
const STATUS_Y: i32 = 0;
/// Horizontal inset for status-bar content to clear the bezel arc.
const STATUS_X_INSET: i32 = 85;

/// Top of the Nightwatch header bar on settings sub-views. Sits
/// below the status bar with an 8 px gap so the two read as
/// separated.
const HDR_TOP: i32 = STATUS_Y + STATUS_BAR_H + 8;
/// Height of the Nightwatch header bar (see [`widgets::HEADER_H`]).
const HDR_H: i32 = 28;
/// Y of the bottom home-indicator bar.
const HOME_BAR_Y: i32 = theme::SCREEN_H as i32 - 18;

/// Header rect shared by every settings sub-view.
fn hdr_rect() -> Rectangle {
    Rectangle::new(
        Point::new(0, HDR_TOP),
        Size::new(theme::SCREEN_W as u32, HDR_H as u32),
    )
}

/// Draw the full Settings chrome: top status bar (tinted by `accent`,
/// carrying live HH:MM + battery% from `data`), Nightwatch header
/// with `title` + `SYS.CFG` telemetry, and bottom home-indicator bar.
fn draw_header<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    data: &SystemData,
    title: &str,
    accent: Rgb565,
    ctx: &RenderCtx,
) {
    // The three chrome pieces sit at fixed y-positions: status bar at
    // the very top, title header just below it, home indicator at the
    // very bottom. For each piece, only do its work when this tile's
    // y-range actually overlaps the piece - otherwise the per-call
    // setup (string format, glyph lookup, fill_contiguous iterator)
    // is wasted, since the driver's per-pixel clip would reject every
    // write anyway.
    if ctx.intersects_y(STATUS_Y, STATUS_Y + STATUS_BAR_H) {
        let mut time_buf: heapless::String<8> = heapless::String::new();
        let _ = core::fmt::Write::write_fmt(
            &mut time_buf,
            format_args!("{:02}:{:02}", data.time.hour, data.time.minute),
        );
        status_bar(
            display,
            STATUS_Y,
            time_buf.as_str(),
            data.power.battery_percent,
            accent,
            STATUS_X_INSET,
        );
    }

    if ctx.intersects_y(HDR_TOP, HDR_TOP + HDR_H) {
        header(display, hdr_rect(), title, "SYS.CFG", accent);
    }

    // Home indicator is an 18 px pill at the bottom edge.
    if ctx.intersects_y(HOME_BAR_Y, HOME_BAR_Y + 18) {
        home_indicator(display, HOME_BAR_Y, accent);
    }
}

/// Y of the first row below the settings header.
const ROWS_TOP: i32 = HDR_TOP + HDR_H + 8;

/// Rect for the Nth row in the settings Index / Storage sub-index,
/// adjusted by the current scroll offset. `scroll = 0` returns the
/// row's natural position; positive `scroll` shifts everything up
/// (rows below come into view). Width leaves a [`SCROLLBAR_GUTTER`]
/// inset on the right so the row's right-aligned controls have room
/// before the scrollbar.
fn row_rect(index: usize, scroll: i32) -> Rectangle {
    let y = ROWS_TOP + index as i32 * ROW_H - scroll;
    Rectangle::new(
        Point::new(0, y),
        Size::new(
            (theme::SCREEN_W as i32 - SCROLLBAR_GUTTER) as u32,
            ROW_H as u32,
        ),
    )
}

/// Hit test the back chevron in the settings Nightwatch header.
fn header_back_hit(x: u16, y: u16) -> bool {
    header_icon_hit(x, y, hdr_rect())
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
    /// Wi-Fi configuration / status. Stub for now; real contents
    /// land when networking is wired up.
    Wifi,
    /// Bluetooth pairing / status. Stub for now; real contents
    /// land when BLE is wired up.
    Bluetooth,
    /// Zigbee mesh status. Only meaningful on the C6 board variant
    /// (S3 has no 802.15.4 radio); shown as a stub on S3 for now,
    /// to be feature-gated when the C6 build path lands.
    Zigbee,
}

// -- Index row metadata ------------------------------------------------------

/// Per-row icon. Rust can't coerce a generic `fn(..D..)` into a
/// non-generic function pointer, so we enum-dispatch to pick one of
/// a closed set of glyphs at render time (same pattern the App
/// Drawer uses).
#[derive(Clone, Copy)]
enum RowIcon {
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

fn draw_row_icon<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, kind: RowIcon, cx: i32, cy: i32, color: Rgb565,
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
#[derive(Clone, Copy)]
enum RowKind {
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

struct IndexRow {
    label: &'static str,
    icon: RowIcon,
    /// Capability gate: the row renders and hit-tests only when this
    /// returns true (rows below it shift up). `always` for standard
    /// rows; capability probes (e.g. [`has_gps`]) for rows whose
    /// hardware only some boards carry.
    visible: fn(&SystemData) -> bool,
    kind: RowKind,
}

/// Standard-row visibility: every board has this hardware.
fn always(_data: &SystemData) -> bool {
    true
}

/// GPS rows exist only on boards whose bin declared the capability.
fn has_gps(data: &SystemData) -> bool {
    data.capabilities.gps
}

fn clock_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = write!(buf, "{:02}:{:02}:{:02}", data.time.hour, data.time.minute, data.time.second);
    buf
}

/// Index-row inline value for GPS: the live session state while one
/// runs, otherwise the configured timezone.
fn gps_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    match data.gps_sync {
        crate::data::GpsSyncState::Syncing { .. } => {
            let _ = buf.push_str("SYNCING");
        }
        _ => {
            let m = data.config.tz_offset_minutes;
            let a = m.unsigned_abs();
            let _ = write!(buf, "UTC{}{}:{:02}", if m < 0 { '-' } else { '+' }, a / 60, a % 60);
        }
    }
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
    // Summary shown on the top-level settings index: "<files> / <size>K".
    let mut buf = String::new();
    let _ = write!(
        buf,
        "{} / {}K",
        data.storage.files,
        data.storage.total_bytes / 1024,
    );
    buf
}

// -- Storage sub-index rows --------------------------------------------------
//
// Same IndexRow pattern as the top-level settings index, one level
// deeper. Each row taps into a storage leaf view.

fn storage_flash_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = write!(
        buf,
        "{} FILES / {}K",
        data.storage.files,
        data.storage.total_bytes / 1024,
    );
    buf
}

fn storage_sd_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = buf.push_str(if data.storage.sd_online { "ONLINE" } else { "NOT PRESENT" });
    buf
}

fn storage_reset_value(_data: &SystemData) -> String<20> {
    String::new()
}

fn storage_restore_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = buf.push_str(if data.storage.sd_online { "" } else { "SD NOT PRESENT" });
    buf
}

fn haptics_is_on(data: &SystemData) -> bool {
    data.config.haptics_enabled
}

fn sound_is_on(data: &SystemData) -> bool {
    data.config.sound_enabled
}

fn dnd_is_on(data: &SystemData) -> bool {
    data.config.dnd
}

const STORAGE_INDEX_ROWS: &[IndexRow] = &[
    IndexRow {
        label: "FLASH",
        icon: RowIcon::Flash,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::StorageFlash, value_fn: storage_flash_value },
    },
    IndexRow {
        label: "SD CARD",
        icon: RowIcon::SdCard,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::StorageSd, value_fn: storage_sd_value },
    },
    IndexRow {
        label: "RESTORE FROM SD",
        icon: RowIcon::Restore,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::StorageRestoreFlash, value_fn: storage_restore_value },
    },
    IndexRow {
        label: "FACTORY RESET",
        icon: RowIcon::Skull,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::StorageFactoryReset, value_fn: storage_reset_value },
    },
];

/// Empty inline value - causes navigate rows to render a chevron
/// instead of an inline string.
fn empty_value(_data: &SystemData) -> String<20> { String::new() }

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
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::Wifi, value_fn: empty_value },
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

// -- IMU sub-view test list --------------------------------------------------

struct ImuTestRow {
    label: &'static str,
    id: SelfTestId,
    unit: &'static str,
}

const IMU_TESTS: &[ImuTestRow] = &[
    ImuTestRow {
        label: "ACCEL SELF-TEST",
        id: SelfTestId::ImuAccel,
        unit: "mg",
    },
    ImuTestRow {
        label: "GYRO SELF-TEST",
        id: SelfTestId::ImuGyro,
        unit: "dps",
    },
];

// -- Picker layout ----------------------------------------------------------

/// Top y of the wheel picker in time/date entry views, vertically
/// centred between the header bottom and the action row.
const PICKER_TOP: i32 = HDR_TOP + HDR_H + 8
    + (layout::BOTTOM_TILE_Y - (HDR_TOP + HDR_H + 8) - WHEEL_TOTAL_H) / 2;

/// Width of one wheel column in the time/date picker.
const PICKER_COL_W: i32 = 72;

/// Horizontal gap between adjacent picker columns.
const PICKER_GAP: i32 = 28;

/// Total horizontal extent of a three-column picker.
const PICKER_TOTAL_W: i32 = PICKER_COL_W * 3 + PICKER_GAP * 2;

/// Year range surfaced in the date picker. PCF85063 is good past
/// 2099 but the year wheel scrolls become tedious past then; pick
/// the same century the firmware was built in.
const DATE_YEAR_MIN: i32 = 2000;
const DATE_YEAR_MAX: i32 = 2099;

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
    /// Counter that throttles MOTION-sub-view redraws to a fraction
    /// of the IMU's 20 Hz `MotionUpdated` cadence.
    motion_phase: u8,
    /// Last MotionData rendered into the MOTION sub-view, used to
    /// suppress redraws when the values haven't changed materially.
    motion_last: Option<crate::data::MotionData>,
    /// MicTest sub-view LOOP toggle: true while mic -> speaker
    /// loopback is the active meter mode (vs. meter-only capture).
    mic_loopback: bool,
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
            motion_phase: 0,
            motion_last: None,
            mic_loopback: false,
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
    fn render<D: DrawTarget<Color = Rgb565>>(
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
            SettingsView::Wifi      => self.render_stub(display, data, "WIFI", ctx),
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
            SettingsView::Wifi
            | SettingsView::Bluetooth
            | SettingsView::Zigbee => self.stub_event(event, data),
        }
    }
}

// -- Index sub-view ----------------------------------------------------------

impl SettingsScreen {
    fn render_index<D: DrawTarget<Color = Rgb565>>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "SETTINGS", theme::SIGNAL, ctx);
        render_scrolled(
            display, self.index_scroll.offset(),
            index_viewport_rect(), index_content_h(data), theme::SIGNAL, ctx,
            |clipped, scroll| render_rows(clipped, data, INDEX_ROWS, scroll, ctx),
        );
    }

    fn index_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
                Action::Back
            }
            SystemEvent::Tap { x, y } => {
                if let Some(action) = row_hit(
                    *x, *y, INDEX_ROWS, data,
                    self.index_scroll.offset(),
                    &index_viewport_rect(),
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
                let viewport_h = index_viewport_rect().size.height as i32;
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
fn index_viewport_rect() -> Rectangle {
    let top = ROWS_TOP;
    let bot = HOME_BAR_Y - 4;
    Rectangle::new(
        Point::new(0, top),
        Size::new(theme::SCREEN_W as u32, (bot - top) as u32),
    )
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
fn render_rows<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, data: &SystemData, rows: &[IndexRow], scroll: i32, ctx: &RenderCtx,
) {
    let mut pos = 0;
    for r in rows.iter() {
        // Capability-gated rows vanish entirely; rows below shift up
        // (`pos` counts only visible rows).
        if !(r.visible)(data) {
            continue;
        }
        let rect = row_rect(pos, scroll);
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
                    RowControl::Chevron(theme::CYAN)
                } else {
                    RowControl::Inline(val.as_str(), theme::FG_MUTED)
                };
                row(
                    display, rect,
                    |d, cx, cy, c| draw_row_icon(d, kind, cx, cy, c),
                    theme::CYAN,
                    r.label,
                    control,
                );
            }
            RowKind::Toggle { is_on, .. } => {
                row(
                    display, rect,
                    |d, cx, cy, c| draw_row_icon(d, kind, cx, cy, c),
                    theme::CYAN,
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
fn row_hit(
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
        let rect = row_rect(pos, scroll);
        pos += 1;
        if !rect.contains(pt) { continue; }
        return Some(match r.kind {
            RowKind::Navigate { target, .. } => {
                *view = target;
                Action::Redraw
            }
            RowKind::Toggle { action, .. } => action,
        });
    }
    None
}

// -- Clock sub-view (time + date panels) -------------------------------------

/// Y of the first clock metric panel below the settings header.
const CLOCK_PANEL_TOP: i32 = HDR_TOP + HDR_H + 12;
/// Height of one clock metric panel.
const CLOCK_PANEL_H: i32 = 84;
/// Vertical gap between the two clock metric panels.
const CLOCK_PANEL_GAP: i32 = 12;

fn clock_panel_rect(slot: usize) -> Rectangle {
    let y = CLOCK_PANEL_TOP + slot as i32 * (CLOCK_PANEL_H + CLOCK_PANEL_GAP);
    let x = layout::VSTACK_SIDE_MARGIN;
    let w = theme::SCREEN_W as i32 - layout::VSTACK_SIDE_MARGIN * 2;
    Rectangle::new(
        Point::new(x, y),
        Size::new(w as u32, CLOCK_PANEL_H as u32),
    )
}

fn draw_clock_panel<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    rect: Rectangle,
    tag: &str,
    value: &str,
) {
    chamfered_panel(display, rect, NOTCH, theme::SIGNAL, 1);
    tag_label(
        display,
        rect.top_left.x, rect.top_left.y,
        tag, theme::SIGNAL, NOTCH,
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
    fn render_clock<D: DrawTarget<Color = Rgb565>>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "CLOCK", theme::SIGNAL, ctx);

        let mut time_buf: String<12> = String::new();
        let _ = write!(time_buf, "{:02}:{:02}:{:02}",
            data.time.hour, data.time.minute, data.time.second);
        draw_clock_panel(display, clock_panel_rect(0), "TIME", time_buf.as_str());

        let mut date_buf: String<12> = String::new();
        let _ = write!(date_buf, "{:02}.{:02}.{:04}",
            data.time.day, data.time.month, data.time.year);
        draw_clock_panel(display, clock_panel_rect(1), "DATE", date_buf.as_str());
    }

    fn clock_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            // Keep the display fresh.
            SystemEvent::TimeUpdated { .. } => Action::Redraw,

            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
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
                if clock_panel_rect(0).contains(p) {
                    // Open time picker, seed from current time.
                    let t = &data.time;
                    self.time_picker.wheels[0].set_value(t.hour as i32);
                    self.time_picker.wheels[1].set_value(t.minute as i32);
                    self.time_picker.wheels[2].set_value(t.second as i32);
                    self.view = SettingsView::TimeEntry;
                    Action::Redraw
                } else if clock_panel_rect(1).contains(p) {
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

// -- Time entry picker -------------------------------------------------------

impl SettingsScreen {
    fn render_time_entry<D: DrawTarget<Color = Rgb565>>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "SET TIME", theme::SIGNAL, ctx);

        let cells = picker_cell_rects();
        self.time_picker.wheels[0].render(display, cells[0], theme::SIGNAL, fmt_2digit);
        self.time_picker.wheels[1].render(display, cells[1], theme::SIGNAL, fmt_2digit);
        self.time_picker.wheels[2].render(display, cells[2], theme::SIGNAL, fmt_2digit);
        draw_picker_separators(display, &cells, ":", theme::SIGNAL);

        render_action_row(display, theme::SIGNAL);
    }

    fn time_entry_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
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

                let cells = picker_cell_rects();
                if self.time_picker.handle_event(event, &cells) {
                    return Action::Redraw;
                }
                Action::None
            }
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let cells = picker_cell_rects();
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
    fn render_date_entry<D: DrawTarget<Color = Rgb565>>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "SET DATE", theme::SIGNAL, ctx);

        let cells = picker_cell_rects();
        self.date_picker.wheels[0].render(display, cells[0], theme::SIGNAL, fmt_2digit);
        self.date_picker.wheels[1].render(display, cells[1], theme::SIGNAL, fmt_2digit);
        // Year wheel has 4-digit values - format unpadded to fit
        // the 72 px column at value-font size.
        self.date_picker.wheels[2].render(display, cells[2], theme::SIGNAL, |v, buf| {
            let _ = write!(buf, "{}", v);
        });
        draw_picker_separators(display, &cells, ".", theme::SIGNAL);

        render_action_row(display, theme::SIGNAL);
    }

    fn date_entry_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
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

                let cells = picker_cell_rects();
                if self.date_picker.handle_event(event, &cells) {
                    self.refresh_date_day_range();
                    return Action::Redraw;
                }
                Action::None
            }
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let cells = picker_cell_rects();
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

// -- MOTION sub-view ---------------------------------------------------------
//
// Live IMU + temperature readouts at the top, self-test panels with
// RUN buttons below. Stacked tall enough to need smooth scrolling.

/// Tag-labels for the live readout panels (3 accel axes, 3 gyro
/// axes, 1 environment temperature, plus the raw step-counter total
/// on boards with the steps capability - the layout drops the last
/// entry elsewhere).
const MOTION_LABELS: [&str; 8] = [
    "ACCEL X", "ACCEL Y", "ACCEL Z",
    "GYRO X",  "GYRO Y",  "GYRO Z",
    "TEMP",    "STEPS",
];

/// Height of one live readout panel.
const MOTION_READOUT_H: i32 = 56;
/// Vertical gap between adjacent live readout panels.
const MOTION_READOUT_GAP: i32 = 6;
/// Vertical break between the readouts band and the self-test band.
const MOTION_SECTION_GAP: i32 = 16;

/// IMU `MotionUpdated` arrives at ~20 Hz; only redraw on every Nth
/// sample to keep the MOTION sub-view legible without bottlenecking
/// the render loop. 4 → ~5 Hz redraw cadence.
const MOTION_REDRAW_DIVIDER: u8 = 4;

/// Per-axis change threshold (raw i16 units) below which a fresh
/// `MotionUpdated` is treated as "no visible change" and skipped.
/// Suppresses sensor noise on a still device.
const MOTION_DIFF_THRESHOLD: i32 = 16;

/// Per-temperature change threshold. `temp_raw` is in 1/256 °C so 64
/// raw ≈ 0.25 °C, well above sensor self-heat noise.
const MOTION_TEMP_THRESHOLD: i32 = 64;

/// Self-test panel + button geometry inside the MOTION sub-view.
const MOTION_TEST_PANEL_H: i32 = 80;
const MOTION_TEST_PANEL_BTN_GAP: i32 = 8;
const MOTION_TEST_BUTTON_H: i32 = 36;
const MOTION_INTER_TEST_GAP: i32 = 16;

struct MotionLayout {
    /// One rect per `MOTION_LABELS` entry; the STEPS slot is a
    /// zero-size placeholder when the board lacks the capability.
    readouts: [Rectangle; 8],
    /// How many leading `readouts` entries are real (7 or 8).
    readout_count: usize,
    /// `(panel, button)` pairs in `IMU_TESTS` order.
    tests: [(Rectangle, Rectangle); 2],
    /// Total content height; passed to the smooth-scroll helpers.
    content_h: i32,
}

fn motion_layout(scroll: i32, has_steps: bool) -> MotionLayout {
    let mut s = layout::VStack::new(LEAF_TOP_Y - scroll);

    let r0 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r1 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r2 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r3 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r4 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r5 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r6 = s.slot(MOTION_READOUT_H);
    let r7 = if has_steps {
        s.gap(MOTION_READOUT_GAP);
        s.slot(MOTION_READOUT_H)
    } else {
        Rectangle::zero()
    };
    s.gap(MOTION_SECTION_GAP);

    let p0 = s.slot(MOTION_TEST_PANEL_H);
    s.gap(MOTION_TEST_PANEL_BTN_GAP);
    let b0 = s.slot(MOTION_TEST_BUTTON_H);
    s.gap(MOTION_INTER_TEST_GAP);
    let p1 = s.slot(MOTION_TEST_PANEL_H);
    s.gap(MOTION_TEST_PANEL_BTN_GAP);
    let b1 = s.slot(MOTION_TEST_BUTTON_H);

    let content_h = s.cursor_y() + scroll - LEAF_TOP_Y;
    MotionLayout {
        readouts: [r0, r1, r2, r3, r4, r5, r6, r7],
        readout_count: if has_steps { 8 } else { 7 },
        tests: [(p0, b0), (p1, b1)],
        content_h,
    }
}

/// Visible viewport for MOTION's scrollable area: from the row of
/// section content (LEAF_TOP_Y) down to just above the home bar.
fn motion_viewport_rect() -> Rectangle {
    let top = LEAF_TOP_Y;
    let bot = HOME_BAR_Y - 4;
    Rectangle::new(
        Point::new(0, top),
        Size::new(theme::SCREEN_W as u32, (bot - top) as u32),
    )
}

/// True when at least one axis or the temperature changed by more
/// than the per-channel noise threshold. Used to gate redraws so a
/// motionless device doesn't trigger frames on every IMU sample.
fn motion_changed(prev: &crate::data::MotionData, curr: &crate::data::MotionData) -> bool {
    let pairs = [
        (prev.accel_x, curr.accel_x, MOTION_DIFF_THRESHOLD),
        (prev.accel_y, curr.accel_y, MOTION_DIFF_THRESHOLD),
        (prev.accel_z, curr.accel_z, MOTION_DIFF_THRESHOLD),
        (prev.gyro_x,  curr.gyro_x,  MOTION_DIFF_THRESHOLD),
        (prev.gyro_y,  curr.gyro_y,  MOTION_DIFF_THRESHOLD),
        (prev.gyro_z,  curr.gyro_z,  MOTION_DIFF_THRESHOLD),
        (prev.temp_raw, curr.temp_raw, MOTION_TEMP_THRESHOLD),
    ];
    pairs.iter().any(|(p, c, t)| ((*p as i32) - (*c as i32)).abs() >= *t)
        || prev.steps != curr.steps
}

/// Format the live value for the readout at `idx`, into `buf`.
fn motion_value(idx: usize, data: &SystemData, buf: &mut heapless::String<12>) {
    use core::fmt::Write;
    let m = &data.motion;
    let _ = match idx {
        0 => write!(buf, "{}", m.accel_x),
        1 => write!(buf, "{}", m.accel_y),
        2 => write!(buf, "{}", m.accel_z),
        3 => write!(buf, "{}", m.gyro_x),
        4 => write!(buf, "{}", m.gyro_y),
        5 => write!(buf, "{}", m.gyro_z),
        6 => write!(buf, "{} C", m.temp_raw / 256),
        // Raw hub total (diagnostic) - the daily figure lives on the
        // clock face. "--" until the first step event arrives.
        7 => match m.steps {
            Some(s) => write!(buf, "{}", s),
            None => write!(buf, "--"),
        },
        _ => Ok(()),
    };
}

fn draw_motion_panel<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, rect: Rectangle, tag: &str, value: &str,
) {
    chamfered_panel(display, rect, NOTCH, theme::CYAN, 1);
    tag_label(
        display,
        rect.top_left.x, rect.top_left.y,
        tag, theme::CYAN, NOTCH,
    );
    let inner = Rectangle::new(
        Point::new(rect.top_left.x, rect.top_left.y + TAG_LABEL_H),
        Size::new(rect.size.width, rect.size.height - TAG_LABEL_H as u32),
    );
    fonts::draw_centered_in_rect(display, &fonts::value(), value, inner, theme::FG);
}

impl SettingsScreen {
    fn render_imu<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "MOTION", theme::SIGNAL, ctx);

        let scroll = self.imu_scroll.offset();
        let layout = motion_layout(scroll, data.capabilities.steps);

        render_scrolled(
            display, scroll, motion_viewport_rect(), layout.content_h, theme::SIGNAL, ctx,
            |clipped, _| {
                // Live readouts.
                let mut value_buf: heapless::String<12> = heapless::String::new();
                for i in 0..layout.readout_count {
                    value_buf.clear();
                    motion_value(i, data, &mut value_buf);
                    draw_motion_panel(
                        clipped, layout.readouts[i],
                        MOTION_LABELS[i], value_buf.as_str(),
                    );
                }

                // Self-tests: state panel + RUN button per test.
                for (i, test) in IMU_TESTS.iter().enumerate() {
                    let (panel_rect, button_rect) = layout.tests[i];
                    let result = data.self_tests[test.id as usize];
                    let (test_buf, _, _) = format_result(&result, test.unit);
                    let accent = imu_result_accent(&result);

                    chamfered_panel(clipped, panel_rect, NOTCH, accent, 1);
                    tag_label(
                        clipped,
                        panel_rect.top_left.x, panel_rect.top_left.y,
                        test.label, accent, NOTCH,
                    );
                    fonts::draw_centered_in_rect(
                        clipped, &fonts::value(),
                        test_buf.as_str(), panel_rect, accent,
                    );

                    // Ghost while running (drops re-taps); Primary
                    // when idle / finished.
                    let running = matches!(result, SelfTestResult::Running);
                    let variant = if running {
                        ButtonVariant::Ghost
                    } else {
                        ButtonVariant::Primary
                    };
                    chamfered_button(
                        clipped, button_rect, "RUN SELF-TEST",
                        variant, theme::SIGNAL,
                    );
                }
            },
        );
    }

    fn imu_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            // Live readouts: throttle the IMU's 20 Hz MotionUpdated to
            // ~5 Hz (every 4th sample) and additionally gate on
            // material change so a still device doesn't churn frames.
            SystemEvent::MotionUpdated { .. } => {
                self.motion_phase = (self.motion_phase + 1) % MOTION_REDRAW_DIVIDER;
                if self.motion_phase != 0 {
                    return Action::None;
                }
                let curr = data.motion;
                let changed = match self.motion_last {
                    None => true,
                    Some(prev) => motion_changed(&prev, &curr),
                };
                if !changed {
                    return Action::None;
                }
                self.motion_last = Some(curr);
                Action::Redraw
            }

            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
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

            // Drag scroll for the live + self-tests stack.
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let layout = motion_layout(0, data.capabilities.steps);
                let viewport_h = motion_viewport_rect().size.height as i32;
                if handle_scroll_drag(
                    &mut self.imu_scroll, event, viewport_h, layout.content_h,
                ) {
                    return Action::Redraw;
                }
                Action::None
            }

            // Self-test button tap.
            SystemEvent::Tap { x, y } => {
                let scroll = self.imu_scroll.offset();
                let layout = motion_layout(scroll, data.capabilities.steps);
                let pt = Point::new(*x as i32, *y as i32);
                for (i, test) in IMU_TESTS.iter().enumerate() {
                    let (_, button_rect) = layout.tests[i];
                    if !button_rect.contains(pt) { continue; }
                    // Mirror the Ghost-while-running visual by ignoring
                    // re-taps mid-run.
                    if matches!(data.self_tests[test.id as usize], SelfTestResult::Running) {
                        return Action::None;
                    }
                    return Action::RunSelfTest(test.id);
                }
                Action::None
            }
            SystemEvent::SelfTestUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }

    // -- Battery sub-view ----------------------------------------------------

    fn render_battery<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "BATTERY", theme::SIGNAL, ctx);

        // Top: chamfered tag-labeled BATTERY panel with live
        // percent/voltage centered inside.
        let panel_w = theme::SCREEN_W as i32 - 56;
        let panel_x = (theme::SCREEN_W as i32 - panel_w) / 2;
        let panel_y = ROWS_TOP + 18;
        let panel_h = 60i32;
        let panel_rect = Rectangle::new(
            Point::new(panel_x, panel_y),
            Size::new(panel_w as u32, panel_h as u32),
        );
        chamfered_panel(display, panel_rect, NOTCH, theme::SIGNAL, 1);
        tag_label(
            display,
            panel_rect.top_left.x,
            panel_rect.top_left.y,
            "NOW",
            theme::SIGNAL,
            NOTCH,
        );
        let mut val: String<20> = String::new();
        match (data.power.battery_percent, data.power.battery_voltage_mv) {
            (Some(pct), Some(mv)) => {
                let _ = write!(val, "{}% / {}.{:02}V", pct, mv / 1000, (mv % 1000) / 10);
            }
            (Some(pct), None) => { let _ = write!(val, "{}%", pct); }
            _                  => { let _ = val.push_str("--"); }
        }
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            val.as_str(), panel_rect, theme::FG,
        );

        // Sparkline: full screen width, edge-to-edge, no card around.
        let graph_y = panel_y + panel_h + 14;
        let graph_h = 96i32;
        let graph_rect = Rectangle::new(
            Point::new(0, graph_y),
            Size::new(theme::SCREEN_W as u32, graph_h as u32),
        );
        draw_battery_sparkline(display, graph_rect, &data.battery_history);

        // Below the sparkline: UPTIME (wall-time since power-on, from
        // the SoC RTC counter - survives light sleep) and below that
        // ACTIVE (embassy time since boot - pauses during light
        // sleep). Together they let the user read off duty cycle:
        // active / uptime ~= fraction of time the chip was awake.
        let uptime_y = graph_y + graph_h + 14;
        let uptime_rect = Rectangle::new(
            Point::new(panel_x, uptime_y),
            Size::new(panel_w as u32, panel_h as u32),
        );
        chamfered_panel(display, uptime_rect, NOTCH, theme::CYAN, 1);
        tag_label(
            display,
            uptime_rect.top_left.x,
            uptime_rect.top_left.y,
            "UPTIME",
            theme::CYAN,
            NOTCH,
        );
        let mut up_buf: String<16> = String::new();
        let up = data.uptime_secs;
        let _ = write!(
            up_buf, "{:02}:{:02}:{:02}",
            up / 3600, (up % 3600) / 60, up % 60,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            up_buf.as_str(), uptime_rect, theme::FG,
        );

        let active_y = uptime_y + panel_h + 12;
        let active_rect = Rectangle::new(
            Point::new(panel_x, active_y),
            Size::new(panel_w as u32, panel_h as u32),
        );
        chamfered_panel(display, active_rect, NOTCH, theme::CYAN, 1);
        tag_label(
            display,
            active_rect.top_left.x,
            active_rect.top_left.y,
            "ACTIVE",
            theme::CYAN,
            NOTCH,
        );
        let mut act_buf: String<16> = String::new();
        let act = data.active_secs;
        let _ = write!(
            act_buf, "{:02}:{:02}:{:02}",
            act / 3600, (act % 3600) / 60, act % 60,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            act_buf.as_str(), active_rect, theme::FG,
        );

        // SLEEPS: count of completed light-sleep cycles. Diagnostic -
        // paced by the ~5 s heartbeat when really sleeping (~12/min); a
        // far higher rate vs UPTIME means the CPU is instant-waking
        // instead of gating off. 4th panel below ACTIVE, same geometry;
        // scroll down to see it.
        let slept_y = active_y + panel_h + 12;
        let slept_rect = Rectangle::new(
            Point::new(panel_x, slept_y),
            Size::new(panel_w as u32, panel_h as u32),
        );
        chamfered_panel(display, slept_rect, NOTCH, theme::CYAN, 1);
        tag_label(
            display,
            slept_rect.top_left.x,
            slept_rect.top_left.y,
            "SLEEPS",
            theme::CYAN,
            NOTCH,
        );
        let mut slept_buf: String<16> = String::new();
        let _ = write!(slept_buf, "{}", data.sleep_cycles);
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            slept_buf.as_str(), slept_rect, theme::FG,
        );
    }

    fn battery_event(&mut self, event: &SystemEvent, _data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
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
            // Any live snapshot refresh or new sample should repaint.
            SystemEvent::PowerUpdated { .. }
            | SystemEvent::BatteryChanged { .. }
            | SystemEvent::TimeUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }
}

// -- Storage sub-views -------------------------------------------------------
//
// Two-level hierarchy:
//
//   Settings → Storage (sub-index) → { Flash | SD Card | Factory Reset }
//
// The sub-index mirrors the top-level settings index layout (one
// row per leaf). Each leaf is a focused view for its single
// concern. Back navigation from a leaf returns to the Storage
// sub-index; back from the sub-index returns to the Settings index.

impl SettingsScreen {
    // -- Storage sub-index ---------------------------------------------------

    fn render_storage_index<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "STORAGE", theme::SIGNAL, ctx);
        // Storage sub-index doesn't scroll today (4 rows always
        // fit). Scroll = 0; if more storage rows land later,
        // give SettingsScreen a second `ScrollState` and viewport.
        render_rows(display, data, STORAGE_INDEX_ROWS, 0, ctx);
    }

    fn storage_index_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
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
                if let Some(action) = row_hit(
                    *x, *y, STORAGE_INDEX_ROWS, data,
                    0, &index_viewport_rect(),
                    &mut self.view,
                ) {
                    return action;
                }
                Action::None
            }
            SystemEvent::StorageUsageUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }

    // -- Flash leaf (read-only info) -----------------------------------------

    fn render_storage_flash<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "FLASH", theme::SIGNAL, ctx);

        // Chamfered HUD panel with a hanging FLASH tag ribbon - the
        // spec's "tag-labelled panel" idiom. Body carries the usage
        // numbers as mono-ish body text.
        let margin = 28i32;
        let panel_w = theme::SCREEN_W as i32 - margin * 2;
        let panel_h = 120i32;
        let panel_rect = Rectangle::new(
            Point::new(margin, ROWS_TOP + 24),
            Size::new(panel_w as u32, panel_h as u32),
        );
        // Symmetric chamfered panel (Nightwatch default - TL + BR both cut).
        chamfered_panel(display, panel_rect, NOTCH, theme::SIGNAL, 1);

        // Tag ribbon sits exactly at the panel's TL corner. Its own
        // TL chamfer of size NOTCH carves out the same triangular
        // area as the panel's TL chamfer so the two align pixel-
        // for-pixel.
        tag_label(
            display,
            panel_rect.top_left.x,
            panel_rect.top_left.y,
            "FLASH",
            theme::SIGNAL,
            NOTCH,
        );

        // Interior: usage line centered vertically in the full panel
        // rect. The tag sits in the top-left corner and doesn't
        // interfere with a single centered line of body text.
        let mut buf: String<32> = String::new();
        let _ = write!(
            buf,
            "{} FILES / {} KB",
            data.storage.files,
            data.storage.total_bytes / 1024,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            buf.as_str(), panel_rect, theme::FG,
        );
    }

    fn storage_flash_event(&mut self, event: &SystemEvent, _data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::StorageUsageUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }

    // -- SD card leaf (status + tap to probe) --------------------------------

    fn render_storage_sd<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "SD CARD", theme::SIGNAL, ctx);

        // Status: chamfered tag-labelled panel. Border + tag tint
        // tracks online/offline (green/signal). Read-only - the
        // button below triggers the probe.
        let (status_rect, probe_rect) = storage_sd_slots();
        let (accent, status_text) = if data.storage.sd_online {
            (theme::GREEN, "ONLINE")
        } else {
            (theme::SIGNAL, "NOT PRESENT")
        };
        chamfered_panel(display, status_rect, NOTCH, accent, 1);
        tag_label(
            display,
            status_rect.top_left.x,
            status_rect.top_left.y,
            "STATUS",
            accent,
            NOTCH,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            status_text, status_rect, accent,
        );

        // Probe action button (chamfered Primary), label depends on
        // state (initialize vs reprobe).
        let probe_text = if data.storage.sd_online { "REPROBE" } else { "INITIALIZE" };
        chamfered_button(
            display, probe_rect, probe_text,
            ButtonVariant::Primary, theme::SIGNAL,
        );
    }

    fn storage_sd_event(&mut self, event: &SystemEvent, _data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let pt = Point::new(*x as i32, *y as i32);
                let (_, probe_rect) = storage_sd_slots();
                if probe_rect.contains(pt) {
                    return Action::InitSd;
                }
                Action::None
            }
            SystemEvent::StorageUsageUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }

    // -- Restore-from-SD leaf (destructive, gated on SD online) --------------

    fn render_storage_restore<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "RESTORE FROM SD", theme::SIGNAL, ctx);

        // Warning panel: signal-bordered chamfered panel with a
        // RESTORE tag. Body explains what the action does.
        let (warn_rect, cancel_rect, primary_rect) = confirmation_slots();
        chamfered_panel(display, warn_rect, NOTCH, theme::SIGNAL, 1);
        tag_label(
            display,
            warn_rect.top_left.x,
            warn_rect.top_left.y,
            "RESTORE",
            theme::SIGNAL,
            NOTCH,
        );
        let body = if data.storage.sd_online {
            "FLASH CONFIG // REBOOT"
        } else {
            "SD NOT PRESENT"
        };
        let body_color = if data.storage.sd_online { theme::FG } else { theme::FG_DIM };
        fonts::draw_centered_in_rect(
            display, &fonts::body(),
            body, warn_rect, body_color,
        );

        // CANCEL / RESTORE buttons. Restore disabled (Ghost variant)
        // when SD isn't online.
        
        chamfered_button(
            display, cancel_rect, "CANCEL",
            ButtonVariant::Ghost, theme::STEEL,
        );
        if data.storage.sd_online {
            chamfered_button(
                display, primary_rect, "RESTORE",
                ButtonVariant::Primary, theme::SIGNAL,
            );
        } else {
            chamfered_button(
                display, primary_rect, "RESTORE",
                ButtonVariant::Ghost, theme::STEEL,
            );
        }
    }

    fn storage_restore_event(
        &mut self,
        event: &SystemEvent,
        data: &mut SystemData,
    ) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let pt = Point::new(*x as i32, *y as i32);
                let (_, cancel_rect, primary_rect) = confirmation_slots();
                if cancel_rect.contains(pt) {
                    self.view = SettingsView::Storage;
                    return Action::Redraw;
                }
                if primary_rect.contains(pt) && data.storage.sd_online {
                    // No bounce-back - the manager will software-
                    // reset shortly after this returns.
                    return Action::RestoreFromSd;
                }
                Action::None
            }
            SystemEvent::StorageUsageUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }

    // -- Factory reset leaf (destructive) ------------------------------------

    fn render_storage_factory_reset<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "FACTORY RESET", theme::DANGER, ctx);

        // Warning panel: danger-tinted chamfered panel with PURGE
        // tag and irreversible-action copy.
        let (warn_rect, cancel_rect, primary_rect) = confirmation_slots();
        chamfered_panel(display, warn_rect, NOTCH, theme::DANGER, 1);
        tag_label(
            display,
            warn_rect.top_left.x,
            warn_rect.top_left.y,
            "PURGE",
            theme::DANGER,
            NOTCH,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::body(),
            "WIPES CONFIG // LOGS", warn_rect, theme::FG,
        );

        // CANCEL (ghost) + PURGE (filled danger) button pair.
        
        chamfered_button(
            display, cancel_rect, "CANCEL",
            ButtonVariant::Ghost, theme::STEEL,
        );
        chamfered_button(
            display, primary_rect, "PURGE",
            ButtonVariant::Primary, theme::DANGER,
        );
    }

    fn storage_factory_reset_event(&mut self, event: &SystemEvent, _data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let pt = Point::new(*x as i32, *y as i32);
                let (_, cancel_rect, primary_rect) = confirmation_slots();
                if cancel_rect.contains(pt) {
                    self.view = SettingsView::Storage;
                    return Action::Redraw;
                }
                if primary_rect.contains(pt) {
                    // Bounce back to Storage sub-index on confirm
                    // so the user sees the refreshed usage counts
                    // land naturally.
                    self.view = SettingsView::Storage;
                    return Action::FactoryReset;
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    // -- Display sub-view ----------------------------------------------------

    fn render_display<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "DISPLAY", theme::SIGNAL, ctx);

        let slots = display_slots();
        let always_on = data.config.display.always_on;

        // Brightness panel: tag-labelled chamfered panel with the
        // generic slider widget inside.
        let panel = slots.brightness_panel;
        chamfered_panel(display, panel, NOTCH, theme::SIGNAL, 1);
        tag_label(
            display,
            panel.top_left.x,
            panel.top_left.y,
            "BRIGHTNESS",
            theme::SIGNAL,
            NOTCH,
        );
        let pct = brightness_pct(data);
        let max_pct = data.config.display.max_brightness_pct();
        let mut label: String<8> = String::new();
        let _ = write!(label, "{:02}%", pct);
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
        chamfered_panel(display, panel, NOTCH, theme::SIGNAL, 1);
        tag_label(
            display,
            panel.top_left.x,
            panel.top_left.y,
            "AUTO-LOCK",
            theme::SIGNAL,
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
                variant, theme::SIGNAL,
            );
        }

        // NIGHT MODE + ALWAYS-ON: full-width toggle rows below the
        // panels. Reuses the same `row` widget the Settings index
        // uses so they read as list items rather than another panel.
        row(
            display, slots.night_mode_row,
            |d, cx, cy, c| glyphs::moon(d, cx, cy, 8, c),
            theme::CYAN,
            "NIGHT MODE",
            RowControl::Toggle(data.config.display.night_mode),
        );
        row(
            display, slots.always_on_row,
            |d, cx, cy, c| glyphs::power(d, cx, cy, 8, c),
            theme::CYAN,
            "ALWAYS-ON",
            RowControl::Toggle(always_on),
        );
    }

    fn display_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
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
                    display_slots().brightness_bar,
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
                let slots = display_slots();
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

    // -- Stub sub-views (Wifi / Bluetooth / Zigbee) ------------------------
    //
    // Placeholders so the Settings index can navigate to these rows
    // before their real contents land. Renders a grey tag-labeled
    // panel saying "WIP" with the view's title; back chevron and
    // right-swipe both pop to the index.

    fn render_stub<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        title: &str,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, title, theme::SIGNAL, ctx);
        let mut s = layout::VStack::new(LEAF_TOP_Y);
        let panel = s.slot(80);
        chamfered_panel(display, panel, NOTCH, theme::STEEL, 1);
        tag_label(
            display,
            panel.top_left.x,
            panel.top_left.y,
            "WIP",
            theme::STEEL,
            NOTCH,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            "TODO", panel, theme::FG_DIM,
        );
    }

    /// Live microphone level meter plus the speaker-side tests. The
    /// bar's fill tracks `data.mic_level` (0..=255), updated from
    /// `SystemEvent::MicLevel` while capture or the LOOP test runs.
    /// TONES plays the 440/1000/880 Hz sweep once (the meter restarts
    /// itself off `TonesDone`); LOOP toggles the record-then-playback
    /// "parrot" test, which replays ~1.0 s mic snippets through the
    /// speaker. Together they prove the ES7210 RX and ES8311 TX paths
    /// on hardware before any networking is involved.
    fn render_mic_test<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "MIC TEST", theme::SIGNAL, ctx);

        let (panel, tones_rect, loop_rect) = mic_test_slots();
        chamfered_panel(display, panel, NOTCH, theme::STEEL, 1);
        tag_label(
            display, panel.top_left.x, panel.top_left.y,
            "LEVEL", theme::STEEL, NOTCH,
        );

        // Track + GREEN fill whose width tracks the live level.
        let inset: i32 = 14;
        let bar = Rectangle::new(
            Point::new(panel.top_left.x + inset, panel.top_left.y + 34),
            Size::new(panel.size.width - (inset as u32) * 2, 26),
        );
        bar.into_styled(PrimitiveStyle::with_fill(theme::INK)).draw(display).ok();
        let fill_w = bar.size.width * data.mic_level as u32 / 255;
        if fill_w > 0 {
            Rectangle::new(bar.top_left, Size::new(fill_w, bar.size.height))
                .into_styled(PrimitiveStyle::with_fill(theme::GREEN))
                .draw(display)
                .ok();
        }
        bar.into_styled(PrimitiveStyle::with_stroke(theme::FG, 1)).draw(display).ok();

        // Numeric percent under the bar, so a glance confirms capture.
        let pct = (data.mic_level as u32 * 100) / 255;
        let mut buf: String<8> = String::new();
        let _ = write!(buf, "{}%", pct);
        let label_rect = Rectangle::new(
            Point::new(panel.top_left.x, panel.top_left.y + 66),
            Size::new(panel.size.width, 24),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(), buf.as_str(), label_rect, theme::FG_DIM,
        );

        // Speaker-side tests: momentary TONES sweep + LOOP toggle,
        // Primary-filled while loopback is live.
        chamfered_button(
            display, tones_rect, "TONES",
            ButtonVariant::Ghost, theme::STEEL,
        );
        if self.mic_loopback {
            chamfered_button(
                display, loop_rect, "LOOP ON",
                ButtonVariant::Primary, theme::SIGNAL,
            );
        } else {
            chamfered_button(
                display, loop_rect, "LOOP",
                ButtonVariant::Ghost, theme::STEEL,
            );
        }
    }

    /// Mic-test back / swipe-right both leave to the Index and emit
    /// StopMicTest, which ends whichever audio mode is active.
    /// (Leaving Settings by any other path is caught by the model's
    /// `mic_test` safety net.) TONES / LOOP taps drive the speaker
    /// tests; `TonesDone` restarts the meter the sweep paused.
    fn mic_test_event(&mut self, event: &SystemEvent, _data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
                self.view = SettingsView::Index;
                self.mic_loopback = false;
                Action::StopMicTest
            }
            SystemEvent::Tap { x, y } => {
                let (_, tones_rect, loop_rect) = mic_test_slots();
                if rect_hit(tones_rect, *x, *y) {
                    return Action::PlayToneTest;
                }
                if rect_hit(loop_rect, *x, *y) {
                    self.mic_loopback = !self.mic_loopback;
                    return if self.mic_loopback {
                        Action::StartLoopbackTest
                    } else {
                        Action::StartMicTest
                    };
                }
                Action::None
            }
            SystemEvent::TonesDone => {
                // Sweep finished: restart whichever meter mode the
                // LOOP toggle says was active.
                if self.mic_loopback {
                    Action::StartLoopbackTest
                } else {
                    Action::StartMicTest
                }
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Index;
                self.mic_loopback = false;
                Action::StopMicTest
            }
            _ => Action::None,
        }
    }

    fn render_gps<D: DrawTarget<Color = Rgb565>>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "GPS", theme::SIGNAL, ctx);
        let slots = gps_slots();

        // Session status: state line + the field lesson as a hint
        // (low-e window glazing blocks GNSS outright - sessions need
        // real sky).
        chamfered_panel(display, slots.status_panel, NOTCH, theme::STEEL, 1);
        tag_label(
            display,
            slots.status_panel.top_left.x, slots.status_panel.top_left.y,
            "STATUS", theme::STEEL, NOTCH,
        );
        let mut line: String<24> = String::new();
        match data.gps_sync {
            GpsSyncState::Idle => { let _ = line.push_str("READY"); }
            GpsSyncState::Syncing { sats, fix_ok } => {
                let _ = write!(line, "SYNCING - {} SATS", sats);
                if fix_ok { let _ = line.push_str(" FIX"); }
            }
            GpsSyncState::Synced { hour, minute } => {
                let _ = write!(line, "SYNCED {:02}:{:02}", hour, minute);
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
                display, slots.sync_btn, "SYNCING...",
                ButtonVariant::Ghost, theme::FG_MUTED,
            );
        } else {
            chamfered_button(
                display, slots.sync_btn, "SYNC NOW",
                ButtonVariant::Primary, theme::SIGNAL,
            );
        }

        // Timezone stepper: +/- 15 min covers every real UTC offset
        // (Newfoundland, Nepal); the value between the steppers.
        chamfered_panel(display, slots.tz_panel, NOTCH, theme::STEEL, 1);
        tag_label(
            display,
            slots.tz_panel.top_left.x, slots.tz_panel.top_left.y,
            "TIMEZONE", theme::STEEL, NOTCH,
        );
        chamfered_button(
            display, slots.tz_minus, "-", ButtonVariant::Ghost, theme::STEEL,
        );
        chamfered_button(
            display, slots.tz_plus, "+", ButtonVariant::Ghost, theme::STEEL,
        );
        let m = data.config.tz_offset_minutes;
        let a = m.unsigned_abs();
        let mut tz: String<12> = String::new();
        let _ = write!(tz, "UTC{}{}:{:02}", if m < 0 { '-' } else { '+' }, a / 60, a % 60);
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
    }

    fn gps_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
                self.view = SettingsView::Index;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let slots = gps_slots();
                if rect_hit(slots.sync_btn, *x, *y) {
                    // Visually disabled while syncing - the tap must
                    // die here too, not just look dead.
                    if matches!(data.gps_sync, GpsSyncState::Syncing { .. }) {
                        return Action::None;
                    }
                    return Action::GpsSync;
                }
                if rect_hit(slots.tz_minus, *x, *y) {
                    return Action::AdjustTimezone { delta_min: -15 };
                }
                if rect_hit(slots.tz_plus, *x, *y) {
                    return Action::AdjustTimezone { delta_min: 15 };
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
            _ => Action::None,
        }
    }

    fn stub_event(&mut self, event: &SystemEvent, _data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y) => {
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

// -- Helpers -----------------------------------------------------------------

/// Per-column rects for the time/date wheel picker, centered horizontally
/// inside the SCREEN_W band.
fn picker_cell_rects() -> [Rectangle; 3] {
    let start_x = (theme::SCREEN_W as i32 - PICKER_TOTAL_W) / 2;
    core::array::from_fn(|i| {
        Rectangle::new(
            Point::new(
                start_x + i as i32 * (PICKER_COL_W + PICKER_GAP),
                PICKER_TOP,
            ),
            Size::new(PICKER_COL_W as u32, WHEEL_TOTAL_H as u32),
        )
    })
}

/// Draw a single-character separator (`":"` for time, `"."` for date)
/// between adjacent picker columns, sitting on the wheels' selection
/// band centerline.
fn draw_picker_separators<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    cells: &[Rectangle; 3],
    sep: &str,
    accent: Rgb565,
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
fn days_in_month(month: i32, year: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

/// Standalone hit-test - reused by the time/date entry sub-views to
/// match against `action_row_rects()` slots without pulling in the
/// alarm screen's helper.
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


fn format_result(
    result: &SelfTestResult,
    unit: &'static str,
) -> (String<32>, Rgb565, Option<Rgb565>) {
    let mut buf: String<32> = String::new();
    match result {
        SelfTestResult::NotRun => {
            let _ = buf.push_str("--");
            (buf, theme::FG_DIM, None)
        }
        SelfTestResult::Running => {
            let _ = buf.push_str("RUNNING");
            (buf, theme::FG_MUTED, Some(theme::SIGNAL))
        }
        SelfTestResult::PassAxes3(v) => {
            let _ = write!(&mut buf, "{} {} {} {}", v[0], v[1], v[2], unit);
            (buf, theme::FG, Some(theme::GREEN))
        }
        SelfTestResult::FailAxes3(v) => {
            let _ = write!(&mut buf, "{} {} {} {}", v[0], v[1], v[2], unit);
            (buf, theme::DANGER, Some(theme::DANGER))
        }
        SelfTestResult::Error(_) => {
            let _ = buf.push_str("ERROR");
            (buf, theme::DANGER, Some(theme::DANGER))
        }
    }
}

// -- Sub-view layout helpers -------------------------------------------------
//
// Each non-trivial leaf sub-view has a `*_slots()` function that
// returns ALL its rects via a [`layout::VStack`] cursor. Render and
// event handlers both call the same `*_slots()` function and
// destructure into named rects, so they're guaranteed to agree on
// geometry - no chance of the event-side hit-test rect drifting from
// the render-side draw rect.

/// Top y for every leaf sub-view's first slot. Sits below the
/// header hairline with breathing room.
const LEAF_TOP_Y: i32 = ROWS_TOP + 18;

// -- Storage SD: status panel + single full-width action button ------------

/// Storage-SD sub-view rects: (status_panel, action_button).
fn storage_sd_slots() -> (Rectangle, Rectangle) {
    let mut s = layout::VStack::new(LEAF_TOP_Y);
    let panel = s.slot(100);
    s.gap(18);
    let button = s.slot(36);
    (panel, button)
}

// -- Storage Restore / Factory Reset: panel + CANCEL/CONFIRM pair ----------

/// Restore / Factory-Reset sub-view rects: (warning_panel, cancel,
/// primary). Same layout for both; they differ only in colors and
/// labels.
fn confirmation_slots() -> (Rectangle, Rectangle, Rectangle) {
    let mut s = layout::VStack::new(LEAF_TOP_Y);
    let panel = s.slot(100);
    s.gap(18);
    let (cancel, primary) = s.pair(36, 12);
    (panel, cancel, primary)
}

/// MicTest sub-view layout: (level panel, TONES button, LOOP button).
/// Shared by render and hit-testing so the tap targets always match
/// what's drawn.
fn mic_test_slots() -> (Rectangle, Rectangle, Rectangle) {
    let mut s = layout::VStack::new(LEAF_TOP_Y);
    let panel = s.slot(96);
    s.gap(18);
    let (tones, loop_b) = s.pair(36, 12);
    (panel, tones, loop_b)
}

// -- GPS sub-view layout -----------------------------------------------------

/// All rects the GPS sub-view needs. Render and event handlers both
/// call [`gps_slots`] and read the same fields, so geometry can
/// never drift between draw and hit-test.
struct GpsSlots {
    /// Outer chamfered panel for the session status section.
    status_panel: Rectangle,
    /// The SYNC NOW trigger (disabled while a session runs).
    sync_btn: Rectangle,
    /// Outer chamfered panel for the timezone section.
    tz_panel: Rectangle,
    /// -15 min stepper inside `tz_panel`.
    tz_minus: Rectangle,
    /// +15 min stepper inside `tz_panel`.
    tz_plus: Rectangle,
}

fn gps_slots() -> GpsSlots {
    let mut s = layout::VStack::new(LEAF_TOP_Y);
    let status_panel = s.slot(96);
    s.gap(18);
    let sync_btn = s.slot(44);
    s.gap(18);
    let tz_panel = s.slot(84);
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
    GpsSlots { status_panel, sync_btn, tz_panel, tz_minus, tz_plus }
}

// -- Display sub-view layout ----------------------------------------------

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

fn display_slots() -> DisplaySlots {
    // Top section: margined VStack for the two chamfered panels.
    let mut top = layout::VStack::new(LEAF_TOP_Y);
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

/// Slider-percent view of the live brightness register (0..=255).
/// Mirrors the QA helper so both Display sub-view and QA agree on
/// the percent <-> register mapping.
fn brightness_pct(data: &SystemData) -> u8 {
    let hw = data.config.display.brightness_active as u16;
    ((hw * 100 / 255) as u8).clamp(BRIGHT_MIN_PCT, 100)
}

/// Pick the accent color (panel border + tag + result text) for a
/// given IMU test result. Visualises run state at a glance:
/// steel = inactive, signal = running, green = pass, danger =
/// fail/error.
fn imu_result_accent(result: &SelfTestResult) -> Rgb565 {
    match result {
        SelfTestResult::NotRun => theme::STEEL,
        SelfTestResult::Running => theme::SIGNAL,
        SelfTestResult::PassAxes3(_) => theme::GREEN,
        SelfTestResult::FailAxes3(_) | SelfTestResult::Error(_) => theme::DANGER,
    }
}

// -- Battery-graph helpers ---------------------------------------------------

/// Render the battery history as an edge-to-edge polyline inside
/// `rect`. Draws faint horizontal gridlines at 25/50/75% and
/// connects consecutive samples with short segments in the battery
/// color. Empty history gets a centered "NO DATA" caption.
///
/// `rect` is the full sparkline area (no surrounding card); the
/// polyline insets by a small horizontal margin so endpoints don't
/// land at the screen edge but is otherwise full width.
fn draw_battery_sparkline<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    rect: Rectangle,
    history: &crate::data::BatteryHistory,
) {
    // Small horizontal inset so the leftmost / rightmost samples
    // don't sit at the bezel arc; vertical inset is just visual
    // breathing room.
    const H_INSET: i32 = 24;
    const V_INSET: i32 = 6;
    let plot = Rectangle::new(
        Point::new(rect.top_left.x + H_INSET, rect.top_left.y + V_INSET),
        Size::new(
            (rect.size.width as i32 - 2 * H_INSET) as u32,
            (rect.size.height as i32 - 2 * V_INSET) as u32,
        ),
    );

    // Horizontal gridlines at 25 / 50 / 75 percent.
    let grid_style = PrimitiveStyle::with_stroke(theme::FG_DIM, 1);
    let left  = plot.top_left.x;
    let right = plot.top_left.x + plot.size.width as i32;
    for pct in [25, 50, 75] {
        let y = plot_y(pct, &plot);
        let _ = Line::new(Point::new(left, y), Point::new(right, y))
            .draw_styled(&grid_style, display);
    }

    // Empty state: centered caption.
    if history.is_empty() {
        fonts::draw_centered_in_rect(
            display, &fonts::body(), "NO DATA", plot, theme::FG_DIM,
        );
        return;
    }

    // Map each sample to a screen point, oldest on the left. When
    // there's only one sample the polyline has no segments - draw
    // a single-pixel dot via a length-1 Line so the view still
    // shows "there is data here."
    let n = history.len();
    let width = plot.size.width as i32;
    let sample_point = |i: usize, pct: u8| -> Point {
        let x = if n <= 1 {
            plot.top_left.x + width / 2
        } else {
            plot.top_left.x + (i as i32 * width) / (n as i32 - 1)
        };
        Point::new(x, plot_y(pct, &plot))
    };

    // Color each segment by the *lower* of its two endpoint
    // percents, so the line turns yellow / red the instant it drops
    // into a warning band. Matches the palette `battery_color` uses
    // for the battery icon elsewhere in the UI.
    let mut prev: Option<(Point, u8)> = None;
    for (i, sample) in history.iter().enumerate() {
        let p = sample_point(i, sample.percent);
        if let Some((q, prev_pct)) = prev {
            let color = crate::ui::primitives::battery_color(prev_pct.min(sample.percent));
            let stroke = PrimitiveStyle::with_stroke(color, 2);
            let _ = Line::new(q, p).draw_styled(&stroke, display);
        } else if n == 1 {
            // Single sample: draw it as a zero-length line so the
            // stroke still renders a small marker.
            let color = crate::ui::primitives::battery_color(sample.percent);
            let stroke = PrimitiveStyle::with_stroke(color, 2);
            let _ = Line::new(p, p).draw_styled(&stroke, display);
        }
        prev = Some((p, sample.percent));
    }
}

/// Map a battery percent (0-100, clamped) to the pixel Y inside
/// `plot`. 100% sits at the top edge, 0% at the bottom edge.
fn plot_y(percent: u8, plot: &Rectangle) -> i32 {
    let p = percent.min(100) as i32;
    let h = plot.size.height as i32;
    plot.top_left.y + (100 - p) * h / 100
}
