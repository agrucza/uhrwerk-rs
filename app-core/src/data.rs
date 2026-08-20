//! Snapshot data types produced by peripheral tasks and consumed
//! by the UI.
//!
//! These were originally defined inside each task file in
//! `firmware/src/system/tasks/`, but they're pure value types
//! shared by both sides and belong in `app-core` where the UI can
//! reach them without pulling in hardware. The tasks re-export
//! them via `pub use` so existing task-path imports keep working.

use drivers::imu::ImuData;
use drivers::pmu::{ChargeVoltage, ChargerPhase, CurrentDirection, InputCurrentLimit};
use drivers::rtc::DateTime as RtcDateTime;

// ============================================================================
// Capabilities - which optional hardware this board carries.
// ============================================================================

/// Optional hardware present on the running board, provided by the
/// bin at boot through the `Bringup` seam and cached in `SystemData`.
/// The shared UI gates board-specific rows/views on these flags -
/// screens never reference board names, only capabilities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// A GNSS receiver with a sync task listening on the GPS command
    /// channel (currently the T-Watch Ultra's MIA-M10Q).
    pub gps: bool,
    /// An IMU whose step-counter engine is wired through the motion
    /// pipeline (currently the T-Watch Ultra's BHI260AP). Gates the
    /// clock-face steps readout and the MOTION view's STEPS panel.
    pub steps: bool,
    /// A WiFi radio with the shared session task spawned behind the
    /// WiFi command channel. Set by the system layer from its own
    /// build feature (a bin can't claim a radio it didn't wire), not
    /// by the bin. Gates the settings WIFI row.
    pub wifi: bool,
}

// ============================================================================
// SafeArea - pixels of the panel hidden under the device's case/bezel.
// ============================================================================

/// Per-edge pixel counts of the panel physically masked by the
/// device's case or glass edge, provided by the bin at boot through
/// the `Bringup` seam and cached in `SystemData`. Measured per board
/// with the bezel-ruler probe (2026-08-15): every unit hides the
/// outermost ~1-3 px under its glass edge; the T-Watch Ultra's case
/// lip additionally swallows ~8 px on the top and sides.
///
/// Only edge-hugging chrome consumes these (status bar, clock swipe
/// hint) - regular content already starts well
/// inboard and screens should keep deriving layout from the
/// `theme::*` geometry, not from these insets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SafeArea {
    pub top: i32,
    pub bottom: i32,
    /// Left/right are COMFORT insets: raised beyond the physical
    /// case edge so content stays readable at wrist viewing angles
    /// (the raised case lip's occlusion sweeps inward horizontally
    /// with the angle).
    pub left: i32,
    pub right: i32,
    /// Effective corner radius of the case aperture, in pixels,
    /// measured from the *visible* glass corner (i.e. inside the
    /// edge insets above). `0` = no corner data - edge-adjacent
    /// chrome then keeps its legacy constant padding, which is
    /// correct for the Waveshare boards whose layouts were tuned
    /// against their printed bezels directly.
    pub corner_r: i32,
}

impl SafeArea {
    /// Horizontal case intrusion at panel row `y` on the left edge:
    /// the edge inset plus the corner arc's extra bite when `y` is
    /// within `corner_r` of the top or bottom of the visible glass.
    pub fn left_inset_at(&self, y: i32, panel_h: i32) -> i32 {
        self.left + self.corner_extra(y, panel_h)
    }

    /// Corner-aware left padding for a text run whose ink centers
    /// on row `y`: the design padding `base`, widened to clear the
    /// case plus a 2 px margin. Boards without case data return
    /// `base` unchanged.
    pub fn left_pad(&self, base: i32, y: i32, panel_h: i32) -> i32 {
        base.max(self.left_inset_at(y, panel_h) + 2)
    }

    /// Right-edge counterpart of [`Self::left_pad`].
    pub fn right_pad(&self, base: i32, y: i32, panel_h: i32) -> i32 {
        base.max(self.right_inset_at(y, panel_h) + 2)
    }

    /// Right-edge counterpart of [`Self::left_inset_at`].
    pub fn right_inset_at(&self, y: i32, panel_h: i32) -> i32 {
        self.right + self.corner_extra(y, panel_h)
    }

    /// Circular-arc corner model: how much deeper than the straight
    /// edge the aperture cuts in at row `y`. Zero outside the corner
    /// bands or when no corner radius is declared.
    fn corner_extra(&self, y: i32, panel_h: i32) -> i32 {
        let r = self.corner_r;
        if r <= 0 {
            return 0;
        }
        // Distance from the nearer horizontal edge of the visible
        // glass (edge insets already excluded).
        let d = (y - self.top).min(panel_h - 1 - self.bottom - y);
        if d >= r {
            return 0;
        }
        if d < 0 {
            return r;
        }
        let chord = (r - d) as u64;
        r - isqrt(r as u64 * r as u64 - chord * chord) as i32
    }
}

/// Integer square root (floor), bit-pair method.
fn isqrt(v: u64) -> u32 {
    let mut x = v;
    let mut res: u64 = 0;
    let mut bit: u64 = 1 << 62;
    while bit > x {
        bit >>= 2;
    }
    while bit != 0 {
        if x >= res + bit {
            x -= res + bit;
            res = (res >> 1) + bit;
        } else {
            res >>= 1;
        }
        bit >>= 2;
    }
    res as u32
}

// ============================================================================
// TimeSync - which source last set the RTC, and when.
// ============================================================================

/// Where a completed time sync got its time from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSource {
    /// An NTP exchange over WiFi.
    Wifi,
    /// A GNSS session.
    Gps,
}

impl TimeSource {
    /// Uppercase label for the settings CLOCK status line.
    pub fn label(self) -> &'static str {
        match self {
            TimeSource::Wifi => "WIFI",
            TimeSource::Gps => "GPS",
        }
    }
}

/// The last sync that actually wrote the RTC: which source did it and
/// the local time that was written.
///
/// `WifiState::Synced` and `GpsSyncState::Synced` each carry their own
/// outcome, but neither is timestamped, so with both populated there is
/// no way to tell which ran later. This field records that ordering:
/// the Model overwrites it whenever either source reports Synced, so
/// it always describes the most recent successful sync. RAM-only - a
/// reboot clears it (the RTC keeps the time itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSyncOutcome {
    pub source: TimeSource,
    pub hour: u8,
    pub minute: u8,
}

// ============================================================================
// GpsSyncState - progress of a GPS time-sync session.
// ============================================================================

/// State of the most recent GPS sync session, cached from
/// `SystemEvent::GpsSyncUpdated` for the settings GPS view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GpsSyncState {
    /// No session since boot.
    #[default]
    Idle,
    /// Session running; live satellite count and whether a position
    /// fix is currently held.
    Syncing { sats: u8, fix_ok: bool },
    /// Session finished having set the RTC; payload is the local
    /// time that was written (the view renders "SYNCED HH:MM").
    Synced { hour: u8, minute: u8 },
    /// Session ended without a trustworthy time (budget exhausted,
    /// no signal, or the receiver failed to come up).
    NoSignal,
}

/// Last usable position fix from a GPS sync session, in the
/// receiver's native 1e-7-degree units (positive = north / east).
/// A snapshot of wherever the last outdoor sync happened - the
/// receiver is rail-gated between sessions, so this is not live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpsFix {
    pub lat_e7: i32,
    pub lon_e7: i32,
}

// ============================================================================
// WifiState - progress of a WiFi session (scan or sync).
// ============================================================================

/// State of the most recent WiFi session, cached from
/// `SystemEvent::WifiStatusUpdated` for the settings WIFI views.
/// Both session kinds report through the same state: a session is
/// the unit of radio time, whatever it does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WifiState {
    /// No session since boot.
    #[default]
    Idle,
    /// A scan session is running; entries are streaming in.
    Scanning,
    /// A scan session finished with `count` networks listed.
    Scanned { count: u8 },
    /// A sync session is associating / leasing / resolving.
    Connecting,
    /// A sync session finished having set the RTC; payload is the
    /// local time that was written (the view renders "SYNCED HH:MM").
    Synced { hour: u8, minute: u8 },
    /// The session ended without doing its job.
    Failed(WifiFailure),
}

impl WifiState {
    /// A session is in flight - the UI refuses a second kick.
    pub fn is_busy(self) -> bool {
        matches!(self, WifiState::Scanning | WifiState::Connecting)
    }
}

/// Why a WiFi session failed, classified by the task from the
/// driver's result so the UI can say something actionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiFailure {
    /// The radio driver did not come up (init or station config).
    RadioInit,
    /// The scan itself errored or ran past its budget.
    ScanFailed,
    /// No access point with the stored SSID answered.
    NoAp,
    /// The AP rejected the credentials (auth / handshake failure).
    AuthFailed,
    /// Association failed for another reason (beacon loss, AP-side
    /// disconnect, ...).
    ConnectFailed,
    /// Associated, but no DHCP lease arrived.
    NoLease,
    /// Online, but DNS or the NTP exchange failed.
    NoNtp,
    /// The whole-session budget ran out.
    Timeout,
}

/// One access point from a scan session, as shown in the settings
/// network list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: heapless::String<{ crate::config::WifiConfig::SSID_MAX }>,
    /// Signal strength in dBm.
    pub rssi: i8,
    /// Any authentication at all (open networks join without a
    /// passphrase).
    pub secured: bool,
}

/// Most networks a scan keeps: enough for a flat or a street, and
/// ~36 B each inside `SystemData`.
pub const MAX_WIFI_NETWORKS: usize = 12;

/// The scan list, strongest signal first.
pub type WifiScanList = heapless::Vec<WifiNetwork, MAX_WIFI_NETWORKS>;

// ============================================================================
// TimeData - calendar time of day, consumed by clock-style screens.
// ============================================================================

/// Calendar time of day. Defaults to an arbitrary recent date so
/// screens have something reasonable to render before the first
/// RTC read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `second` is read by future screens (seconds face)
pub struct TimeData {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

impl TimeData {
    /// Seconds since midnight of the wall-clock time.
    pub fn secs_of_day(&self) -> u32 {
        self.hour as u32 * 3600 + self.minute as u32 * 60 + self.second as u32
    }
}

impl Default for TimeData {
    fn default() -> Self {
        Self { hour: 0, minute: 0, second: 0, year: 2026, month: 1, day: 1 }
    }
}

impl From<&RtcDateTime> for TimeData {
    fn from(dt: &RtcDateTime) -> Self {
        Self {
            hour: dt.hour,
            minute: dt.minute,
            second: dt.second,
            year: dt.year,
            month: dt.month,
            day: dt.day,
        }
    }
}

// ============================================================================
// PowerData - flat snapshot of everything the UI wants from the PMU.
// ============================================================================

/// Flat snapshot of everything the UI wants from the PMU, so
/// screens can read `data.power.vbus_good` directly without
/// going through a nested struct. Fields that come from an I2C
/// read that can fail are `Option<_>`; status flags default to
/// their inactive state when the read fails (screens treat that
/// as "nothing is happening").
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct PowerData {
    // --- Battery ---
    pub battery_percent: Option<u8>,
    pub battery_voltage_mv: Option<u16>,

    // --- Power path (from PMU status register 1) ---
    /// VBUS is present and above the VBUS good threshold.
    pub vbus_good: bool,
    /// BATFET is on (battery connected to the power path).
    pub batfet_active: bool,
    /// Battery is detected by the charger.
    pub battery_present: bool,
    /// Battery is in active (non-sleep) mode.
    pub battery_active: bool,
    /// Die is in thermal regulation (charging current reduced).
    pub thermal_active: bool,
    /// Input current limit regulation is active.
    pub current_limit_active: bool,

    // --- Charger state (from PMU status register 2) ---
    /// Battery current direction (standby / charging / discharging).
    pub current_direction: CurrentDirection,
    /// Charger phase (tri-charge / pre-charge / CC / CV / done / not charging).
    pub charger_phase: ChargerPhase,
    /// System is powered on (always true while we're running).
    pub system_on: bool,
    /// VINDPM regulation is active (input voltage at limit).
    pub vindpm_active: bool,

    // --- ADC readings ---
    pub vbus_voltage_mv: Option<u16>,
    pub system_voltage_mv: Option<u16>,
    pub die_temperature_raw: Option<u16>,

    // --- Charger config (typically static, read once to verify) ---
    pub charge_current_ma: Option<u16>,
    pub charge_voltage: Option<ChargeVoltage>,
    pub input_current_limit: Option<InputCurrentLimit>,
    pub input_voltage_limit_mv: Option<u16>,
}

// ============================================================================
// MotionData - IMU sample, consumed by the status screen motion panel.
// ============================================================================

/// Snapshot of raw IMU axes + die temperature. Defaults to zeros
/// so screens have something to render before the first read.
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct MotionData {
    pub accel_x: i16,
    pub accel_y: i16,
    pub accel_z: i16,
    pub gyro_x: i16,
    pub gyro_y: i16,
    pub gyro_z: i16,
    pub temp_raw: i16,
    /// Step-counter running total (cumulative since the chip's
    /// engine last started); `None` on boards without a wired
    /// pedometer. Daily semantics live in the Model, not here.
    pub steps: Option<u32>,
}

impl From<&ImuData> for MotionData {
    fn from(d: &ImuData) -> Self {
        Self {
            accel_x: d.accel_x,
            accel_y: d.accel_y,
            accel_z: d.accel_z,
            gyro_x: d.gyro_x,
            gyro_y: d.gyro_y,
            gyro_z: d.gyro_z,
            temp_raw: d.temp_raw,
            steps: d.steps,
        }
    }
}

// ============================================================================
// TouchData - current touch point, or `None` fields if idle.
// ============================================================================

/// Current touch point. Both fields are `None` when no finger is
/// down. Updated incrementally from `TouchPressed` / `TouchReleased`
/// events by the main event handler - no I2C reads required.
#[derive(Debug, Clone, Copy, Default)]
pub struct TouchData {
    pub x: Option<u16>,
    pub y: Option<u16>,
}

// ============================================================================
// StorageUsage - flash-backed filesystem occupancy, for the settings screen.
// ============================================================================

/// Summary of the firmware's flash-backed storage. Updated at boot
/// and after every save via
/// [`crate::events::SystemEvent::StorageUsageUpdated`].
///
/// `total_bytes` is the size of the LittleFS partition declared in
/// the board's `partitions-*.csv` (mirrored by
/// `firmware::system::flash_fs::FLASH_FS_SIZE`). `files` is the
/// count of regular files across our known directories
/// (`/config`, `/logs`, `/sounds`, ...).
///
/// Exact used-bytes isn't tracked - the UI only needs an
/// "anything going on?" hint, and file count is what a user
/// actually cares about ("how many things am I storing?").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageUsage {
    pub files: u32,
    pub total_bytes: u32,
    /// `true` if the SD mirror is currently usable for writes. Set
    /// by the manager after a successful `probe_sd`; auto-cleared
    /// if a subsequent SD write fails. The settings screen renders
    /// this as "SD: ONLINE" / "SD: NOT PRESENT".
    pub sd_online: bool,
}

// ============================================================================
// BatteryHistory - ring buffer of recent battery-percent samples.
// ============================================================================

/// One battery-percent reading at a specific wall-clock time.
///
/// Sourced from `SystemEvent::BatteryChanged` entries in the flash
/// event log (tag = `"battery"`, detail = percent). The settings
/// battery screen renders these as a time-ordered polyline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatterySample {
    pub time: TimeData,
    pub percent: u8,
}

/// Capacity of the battery-history ring buffer.
///
/// Battery events fire on every percent change, so at a typical
/// discharge rate one sample lands every few minutes. 48 samples
/// covers several hours of runtime - plenty for a "trend at a
/// glance" graph without growing `SystemData` excessively.
pub const BATTERY_HISTORY_CAP: usize = 48;

/// Ring buffer of the most recent battery samples, oldest at the
/// front. Pushed to by the model on every `BatteryChanged` event;
/// boot-seeded by the manager from the flash event log.
///
/// Not `Copy` (held by `SystemData` via clone), which forces
/// `SystemData` itself off `Copy` - that's fine, see the module docs.
#[derive(Debug, Clone, Default)]
pub struct BatteryHistory {
    samples: heapless::Deque<BatterySample, BATTERY_HISTORY_CAP>,
}

impl BatteryHistory {
    /// Append `sample`. Drops the oldest entry if the buffer is
    /// full so the view always reflects the most recent window.
    pub fn push(&mut self, sample: BatterySample) {
        if self.samples.is_full() {
            let _ = self.samples.pop_front();
        }
        // push_back only errors when full, which we just handled.
        let _ = self.samples.push_back(sample);
    }

    /// Iterate samples oldest-first. The battery screen walks this
    /// left-to-right to place graph points.
    pub fn iter(&self) -> impl Iterator<Item = &BatterySample> {
        self.samples.iter()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

#[cfg(test)]
mod battery_history_tests {
    use super::*;

    fn sample(hour: u8, percent: u8) -> BatterySample {
        BatterySample {
            time: TimeData { hour, ..Default::default() },
            percent,
        }
    }

    #[test]
    fn push_accumulates_then_drops_oldest() {
        let mut h = BatteryHistory::default();
        for i in 0..BATTERY_HISTORY_CAP {
            h.push(sample(i as u8, (100 - i) as u8));
        }
        assert_eq!(h.len(), BATTERY_HISTORY_CAP);
        assert_eq!(h.iter().next().unwrap().percent, 100);

        // Overflow: oldest (100) should drop, newest (say, 42) lands at tail.
        h.push(sample(99, 42));
        assert_eq!(h.len(), BATTERY_HISTORY_CAP);
        assert_eq!(h.iter().next().unwrap().percent, 99);
        assert_eq!(h.iter().last().unwrap().percent, 42);
    }

    #[test]
    fn empty_default() {
        let h = BatteryHistory::default();
        assert!(h.is_empty());
        assert_eq!(h.len(), 0);
    }
}
