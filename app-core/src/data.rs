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
