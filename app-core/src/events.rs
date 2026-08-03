use drivers::pmu::{ChargerPhase, CurrentDirection};

/// All system-level events produced by polling subsystems.
///
/// The main loop collects these each tick, then dispatches them
/// to handlers. This separates event production (polling) from
/// event handling (actions/state changes).
#[allow(dead_code)] // event payload fields are part of the public API
#[derive(Debug, Clone)]
pub enum SystemEvent {
    // -- Input --
    /// BOOT button pressed (falling edge)
    BootButtonPressed,
    /// Power button short press (from PMU interrupt)
    PowerButtonShort,
    /// Power button long press (from PMU interrupt)
    PowerButtonLong,
    /// Touch screen pressed at coordinates. Fires on initial contact
    /// and during drag with updated positions; use for live
    /// drag-tracking (sliders, scrolling). **Do not treat a single
    /// `TouchPressed` as a click** - use `Tap` for that.
    TouchPressed { x: u16, y: u16 },
    /// Touch screen released (finger lifted).
    TouchReleased,
    /// Tap gesture: press and release with minimal movement. Fires on
    /// release at the start position of the press. This is what
    /// screens should use for "click" semantics.
    Tap { x: u16, y: u16 },
    /// Swipe gesture completed on release. `start_x`/`start_y` are
    /// the touch-down coordinates the gesture began at - useful for
    /// per-row hit-tests (e.g. "swipe-right on which notification?")
    /// when the touched UI element matters as well as the direction.
    Swipe {
        dir: SwipeDir,
        region: SwipeRegion,
        start_x: u16,
        start_y: u16,
    },

    // -- Time --
    /// RTC alarm fired (set via `Rtc::set_alarm`). The RTC task
    /// reads Control_2 on INT# fall and clears the alarm flag
    /// before emitting this event.
    /// PCF85063 alarm match fired. `time` is the RTC reading at the
    /// moment AF was observed - use it for the fire timestamp
    /// rather than any cached time snapshot, which may lag.
    AlarmFired { time: crate::data::TimeData },
    /// RTC countdown timer expired (set via `Rtc::set_timer`).
    /// PCF85063 countdown timer expired. `time` is the RTC reading
    /// at expiry, same rationale as [`AlarmFired`].
    ///
    /// [`AlarmFired`]: SystemEvent::AlarmFired
    TimerExpired { time: crate::data::TimeData },

    // -- Power / battery --
    /// Battery state-of-charge changed. The power task emits this
    /// whenever the fuel-gauge percentage differs from its last
    /// poll; the new value is in the payload.
    BatteryChanged { percent: u8 },
    /// VBUS (USB power) was just plugged in (status1.vbus_good
    /// transitioned false → true).
    VbusInserted,
    /// VBUS (USB power) was just removed (status1.vbus_good
    /// transitioned true → false).
    VbusRemoved,
    /// Charger phase changed (e.g. entered CC, entered CV, charge
    /// done, stopped charging). The new phase is in the payload.
    ChargerPhaseChanged { phase: ChargerPhase },
    /// Battery current direction changed (standby / charging /
    /// discharging). The new direction is in the payload.
    CurrentDirectionChanged { direction: CurrentDirection },

    // -- Motion --
    /// IMU Wake-on-Motion fired while the system was sleeping.
    /// Emitted by the IMU task when GPIO21 goes high. Drives
    /// the wake path on the main loop.
    WakeOnMotion,
    /// A hardware GPIO wake line ended light sleep but no task
    /// delivered a concrete event for it. Synthesized by the manager
    /// so a short tap - whose touch-INT pulse ends before the touch
    /// task can service it post-wake - still turns the UI on instead
    /// of being silently lost back into sleep.
    WakeInterrupt,
    /// The wrist entered the user's viewing pose and settled there
    /// (classified by the IMU adapter against the bin's wear
    /// calibration). User activity: undims, resets the idle timer,
    /// and confirms a provisional motion wake.
    WristRaised,
    /// The wrist left the viewing pose and settled elsewhere - the
    /// user is done looking. The model answers by entering sleep
    /// (unless an alert is playing).
    WristLowered,
    /// Fresh IMU snapshot (accel + gyro + die temperature).
    /// Emitted by the IMU task at a fixed cadence while awake
    /// so screens that display live motion data stay current.
    /// The main loop's handler just replaces
    /// `cached_data.motion` with the payload.
    MotionUpdated {
        data: crate::data::MotionData,
    },
    /// The board's IMU chip identified itself (sent once by the IMU
    /// task at startup). Cached into `SystemData::imu_name` for the
    /// settings MOTION row - the UI must not hardcode chip names.
    ImuIdentified {
        name: &'static str,
    },

    // -- Audio --
    /// Live microphone input level (0..=255), emitted by the audio
    /// task while capture is running (the mic-test diagnostic). The
    /// main loop caches it in `cached_data.mic_level` for the level
    /// bar. Not user activity and not a wake source - it must not
    /// reset idle timers or pull the device out of sleep.
    MicLevel { level: u8 },
    /// The speaker-test tone sweep finished playing naturally. The
    /// mic-test view answers this by restarting the level meter it
    /// paused for the sweep. Not emitted when a sweep is cancelled
    /// via `StopTones`. Like `MicLevel`, neither user activity nor a
    /// wake source.
    TonesDone,

    // -- Snapshot refreshes --
    /// Fresh RTC snapshot (calendar date + time of day). Emitted
    /// by the RTC task every `poll_secs` from the software poll,
    /// and also after any alarm/timer INT# so `cached_data.time`
    /// is fresh alongside the triggering event.
    TimeUpdated {
        data: crate::data::TimeData,
    },
    /// Fresh PMU snapshot (battery, charger, ADC channels, ...).
    /// Emitted by the power task every poll interval so
    /// `cached_data.power` stays current without the main loop
    /// ever touching the bus.
    PowerUpdated {
        data: crate::data::PowerData,
    },

    // -- Self-tests --
    /// A self-test's state changed. Emitted by whichever task owns
    /// the hardware under test - Running before the test starts,
    /// Pass/Fail/Error when it completes. The main loop caches the
    /// result in `cached_data.self_tests[id as usize]` and flags a
    /// redraw so any visible self-test screen updates.
    ///
    /// This is the only self-test related `SystemEvent` variant -
    /// requests to *run* a test come from the UI via `Action::RunSelfTest`
    /// and are routed directly to the owning task's command signal
    /// without ever entering the event channel.
    SelfTestUpdated {
        id: SelfTestId,
        result: SelfTestResult,
    },

    // -- GPS --
    /// Progress of a GPS time-sync session, emitted by the board's
    /// GPS task: `Syncing` at session start and on every status
    /// cadence, then exactly one terminal `Synced` / `NoSignal`.
    /// The main loop caches it in `cached_data.gps_sync` for the
    /// settings GPS view. Like `MicLevel`: neither user activity
    /// nor a wake source - a session running while the display
    /// sleeps must not turn the screen on.
    GpsSyncUpdated {
        state: crate::data::GpsSyncState,
    },

    // -- Flash-backed storage --
    /// Fresh flash-filesystem usage snapshot. Emitted by the
    /// manager once at boot (after the initial load), and after
    /// every save / reset. The main loop caches the result in
    /// `cached_data.storage` for the settings screen to render.
    StorageUsageUpdated {
        usage: crate::data::StorageUsage,
    },
}

// -- Self-tests -----------------------------------------------------------

/// Identifier for one hardware self-test. Used as an index into the
/// `SystemData::self_tests` array and as the routing key the main
/// loop uses to decide which task's command signal a
/// [`SystemEvent::RunSelfTestRequested`] should reach.
///
/// Adding a new test:
/// 1. Add a variant here.
/// 2. Bump [`NUM_SELF_TESTS`] by 1.
/// 3. Emit `SelfTestUpdated` events from the task that owns the
///    hardware under test (Running before the measurement,
///    Pass/Fail/Error after).
/// 4. Route `RunSelfTestRequested { id }` in the main loop to that
///    task's command signal.
/// 5. Add the id to the appropriate screen's card list.
///
/// No other changes should be required - the screen rendering is
/// driven by a const table of `(SelfTestId, label)` entries.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfTestId {
    /// QMI8658 accelerometer electrostatic self-test (section 11.1).
    ImuAccel = 0,
    /// QMI8658 gyroscope electrostatic self-test (section 11.2).
    ImuGyro = 1,
}

/// Number of self-test slots, sized to fit every current
/// [`SelfTestId`] variant. Must be kept in sync with the highest
/// variant index plus one - bump this when adding a new variant.
pub const NUM_SELF_TESTS: usize = 2;

/// Current state / last result of a hardware self-test.
///
/// Kept deliberately `Copy` (no heap, no owned strings) so the
/// whole array of per-test results can live in `SystemData` without
/// forcing a `Clone` on it. Screens format values at render time
/// using per-test metadata (label, unit) from a const table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelfTestResult {
    /// Test has never been attempted this session. Initial state
    /// for every slot until the owning task emits its first update.
    #[default]
    NotRun,
    /// Test is currently in progress. Screens render the card as
    /// visibly dimmed and non-interactive while in this state.
    Running,
    /// Test passed, carrying a 3-axis result vector (mg for accel,
    /// dps for gyro). The unit comes from the per-test metadata
    /// table the screen uses - not stored here.
    PassAxes3([i32; 3]),
    /// Test failed, same 3-axis result shape as Pass.
    FailAxes3([i32; 3]),
    /// Test could not be completed (I2C error, timeout, etc.).
    Error(SelfTestError),
}

/// Failure modes for a self-test that couldn't complete.
/// Kept small and `Copy` so `SelfTestResult` stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SelfTestError {
    /// Test did not signal completion within the retry budget.
    Timeout,
    /// An I2C transaction failed during the test.
    I2cFailure,
}

/// Direction of a swipe gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeDir {
    Left,
    Right,
    Up,
    Down,
}

/// Screen region where a swipe gesture *started*, classified by the
/// system gesture edge zones (see `theme::EDGE_GESTURE_ZONE`).
///
/// Edge variants are reserved for system-level actions (e.g.
/// swipe-down-from-top opens the panel). Content is the middle
/// band that belongs to the active screen. All four edges are
/// separate variants so handlers can distinguish direction of
/// origin if they need to.
///
/// When a gesture starts in a corner, vertical edges (Top/Bottom)
/// take precedence over horizontal ones (Left/Right).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeRegion {
    /// Gesture started within `EDGE_GESTURE_ZONE` pixels of the top
    /// edge.
    Top,
    /// Gesture started within `EDGE_GESTURE_ZONE` pixels of the
    /// bottom edge.
    Bottom,
    /// Gesture started within `EDGE_GESTURE_ZONE` pixels of the left
    /// edge (but not in the top or bottom zones).
    Left,
    /// Gesture started within `EDGE_GESTURE_ZONE` pixels of the right
    /// edge (but not in the top or bottom zones).
    Right,
    /// Gesture started in the central content band.
    Content,
}

// ============================================================================
// Event classifiers
// ============================================================================

/// Returns `true` if the event should count as user activity: it
/// resets the idle timer and, if the system is sleeping, brings
/// it out of sleep. These are events produced by direct human
/// interaction with the device (touch, buttons).
pub fn is_user_activity(event: &SystemEvent) -> bool {
    matches!(
        event,
        SystemEvent::TouchPressed { .. }
            | SystemEvent::TouchReleased
            | SystemEvent::Tap { .. }
            | SystemEvent::Swipe { .. }
            | SystemEvent::BootButtonPressed
            | SystemEvent::PowerButtonShort
            | SystemEvent::WristRaised
    )
}

/// Returns `true` if the event is a non-user wake source:
/// something that should bring the device out of sleep even
/// though no one touched it. Covers IMU wake-on-motion and RTC
/// alarm / countdown-timer expiries.
pub fn is_wake_source(event: &SystemEvent) -> bool {
    matches!(
        event,
        SystemEvent::WakeOnMotion
            | SystemEvent::WakeInterrupt
            | SystemEvent::AlarmFired { .. }
            | SystemEvent::TimerExpired { .. }
    )
}

/// One loggable event in a form the firmware can write directly:
/// a short static tag plus an optional integer detail that becomes
/// the third CSV column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoggedEvent {
    pub tag: &'static str,
    pub detail: Option<u32>,
}

/// Classify a [`SystemEvent`] for the SD-card event log, or return
/// `None` if the event is too chatty or too low-signal to record.
///
/// Loggable events are the ones a user might care about reconstructing
/// later (alarms that fired, timers that completed, power-path changes,
/// battery trend, shutdown requests). Fire-hose events (touch,
/// half-minute tick, snapshot refreshes other than BatteryChanged) are
/// intentionally skipped so the log doesn't drown out the interesting
/// lines.
pub fn classify_for_log(event: &SystemEvent) -> Option<LoggedEvent> {
    Some(match event {
        SystemEvent::AlarmFired { .. }        => LoggedEvent { tag: "alarm",         detail: None },
        SystemEvent::TimerExpired { .. }      => LoggedEvent { tag: "timer_expired", detail: None },
        SystemEvent::VbusInserted             => LoggedEvent { tag: "vbus_in",       detail: None },
        SystemEvent::VbusRemoved              => LoggedEvent { tag: "vbus_out",      detail: None },
        SystemEvent::PowerButtonLong          => LoggedEvent { tag: "shutdown_req",  detail: None },
        SystemEvent::BatteryChanged { percent } => LoggedEvent { tag: "battery",     detail: Some(*percent as u32) },
        // Only the terminal success is log-worthy; Syncing is cadence
        // chatter and NoSignal is the common indoor outcome.
        SystemEvent::GpsSyncUpdated { state: crate::data::GpsSyncState::Synced { .. } } =>
            LoggedEvent { tag: "gps_sync", detail: None },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_press_counts_as_user_activity() {
        assert!(is_user_activity(&SystemEvent::TouchPressed { x: 10, y: 20 }));
        assert!(is_user_activity(&SystemEvent::TouchReleased));
        assert!(is_user_activity(&SystemEvent::Tap { x: 0, y: 0 }));
        assert!(is_user_activity(&SystemEvent::BootButtonPressed));
    }

    #[test]
    fn wrist_intents_classify_as_designed() {
        // A settled raise is deliberate attention - full user
        // activity. A lower is neither activity nor a wake source;
        // the model gives it its own sleep-entry policy.
        assert!(is_user_activity(&SystemEvent::WristRaised));
        assert!(!is_user_activity(&SystemEvent::WristLowered));
        assert!(!is_wake_source(&SystemEvent::WristRaised));
        assert!(!is_wake_source(&SystemEvent::WristLowered));
    }

    #[test]
    fn wake_on_motion_is_not_user_activity() {
        // WoM wakes the system but doesn't count as a user tap -
        // we don't want to reset the idle timer just because the
        // device got moved on a table.
        assert!(!is_user_activity(&SystemEvent::WakeOnMotion));
        assert!(is_wake_source(&SystemEvent::WakeOnMotion));
    }

    #[test]
    fn alarm_and_timer_are_wake_sources_not_activity() {
        let t = crate::data::TimeData::default();
        assert!(!is_user_activity(&SystemEvent::AlarmFired { time: t }));
        assert!(!is_user_activity(&SystemEvent::TimerExpired { time: t }));
        assert!(is_wake_source(&SystemEvent::AlarmFired { time: t }));
        assert!(is_wake_source(&SystemEvent::TimerExpired { time: t }));
    }

    #[test]
    fn power_button_long_is_neither() {
        // PowerButtonLong is a shutdown request, not a wake or
        // activity event.
        assert!(!is_user_activity(&SystemEvent::PowerButtonLong));
        assert!(!is_wake_source(&SystemEvent::PowerButtonLong));
    }

    #[test]
    fn log_classifier_covers_alarm_timer_power_button() {
        let t = crate::data::TimeData::default();
        assert_eq!(
            classify_for_log(&SystemEvent::AlarmFired { time: t }),
            Some(LoggedEvent { tag: "alarm", detail: None }),
        );
        assert_eq!(
            classify_for_log(&SystemEvent::TimerExpired { time: t }),
            Some(LoggedEvent { tag: "timer_expired", detail: None }),
        );
        assert_eq!(
            classify_for_log(&SystemEvent::PowerButtonLong),
            Some(LoggedEvent { tag: "shutdown_req", detail: None }),
        );
    }

    #[test]
    fn log_classifier_captures_battery_percent() {
        assert_eq!(
            classify_for_log(&SystemEvent::BatteryChanged { percent: 73 }),
            Some(LoggedEvent { tag: "battery", detail: Some(73) }),
        );
    }

    #[test]
    fn log_classifier_skips_firehose_events() {
        // These fire at a rate that would swamp the log.
        assert!(classify_for_log(&SystemEvent::TouchPressed { x: 0, y: 0 }).is_none());
        assert!(classify_for_log(&SystemEvent::TouchReleased).is_none());
    }
}
