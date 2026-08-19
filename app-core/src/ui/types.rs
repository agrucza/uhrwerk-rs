//! Core UI types - Screen trait, actions, and shared data.

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};

// Re-export self-test types so screens can pull them from a single
// place alongside the other UI data structs below. `pub use` also
// brings them into local scope for `SystemData`'s field types, so
// no separate `use` is needed.
pub use crate::events::{
    NUM_SELF_TESTS, SelfTestId, SelfTestResult, SystemEvent,
};

// The render-target capability trait every `Screen::render` is bound
// on. Defined next to the display driver (the one implementor);
// re-exported here so UI code imports it from the same place as the
// rest of the render types and never names the drivers crate.
pub use drivers::display::{BlendClipped, BlendTarget};

// -- Display power-management state ------------------------------------------

/// Display power-management state. Transitions are driven by idle
/// time since the last user-input event (touch / swipe / button).
///
/// * `Active`: normal running state at full brightness.
/// * `Dim`: brightness register dropped, rendering continues normally.
///   This is the first-stage power save and is the cheapest to enter
///   and leave (single DCS command over SPI).
/// * `Off`: `DISPOFF` issued, the entire render path is skipped until
///   a user event wakes the display again. Deepest power save short
///   of a full light-sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayState {
    Active,
    Dim,
    Off,
}

// -- Screen IDs --------------------------------------------------------------

/// Identifies which screen to switch to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenId {
    Clock,
    /// Count-up stopwatch (HH:MM:SS). Panel-only app.
    Stopwatch,
    /// Count-down timer with numpad duration entry. Panel-only app,
    /// also reachable by tapping the TIMER circle on the clock face.
    Timer,
    /// Alarm manager. Reachable from the clock face's ALARM circle
    /// and the panel.
    Alarm,
    /// Device settings - internally state-machined into sub-views
    /// (IMU, RTC, Power, ...) via `SettingsScreen`'s own enum, so
    /// from the outside there is only one screen id.
    Settings,
    /// Quick Access pull-down overlay (brightness + toggle tiles).
    /// Reached via swipe-down-from-top. Not part of the home-row
    /// rotation. Constructed via `ActiveScreen::new_quick_access(previous)`
    /// because it needs context that plain `new(id)` doesn't provide.
    QuickAccess,
    /// Pull-up app drawer (3x3 tile launcher). Reached via
    /// swipe-up-from-bottom and by tapping the watch face. Also
    /// overlay-like: constructed via `ActiveScreen::new_app_drawer(previous)`.
    AppDrawer,
    /// Global ALERTS overlay. Reached via left-edge swipe-right
    /// (`Swipe { dir: Right, region: Left }`); the model pushes the
    /// pre-overlay screen onto the nav stack so `Action::Back`
    /// returns to wherever the user came from.
    Notifications,
}

// -- Actions -----------------------------------------------------------------

/// What a screen wants the system to do after processing an event.
///
/// Screens return an `Action` from `on_event` to tell the outer
/// navigator what to do next. The navigator is the only thing
/// allowed to mutate global system state (switch screens, signal
/// tasks, shut down) - screens never touch those directly. This
/// keeps screens portable and the control flow easy to trace.
///
/// ## Forward vs. back navigation
///
/// Screens that want to *go somewhere specific* return
/// [`SwitchScreen`]. The navigator pushes the current screen onto
/// its nav stack (unless the current screen is the Panel modal,
/// which is replaced rather than pushed) and switches to the
/// requested target.
///
/// Screens that want to *close and return to whatever opened them*
/// return [`Back`]. The navigator pops the nav stack and switches
/// to the popped id. When the stack is empty it falls back to
/// [`ScreenId::Clock`]. This is the right return for close X
/// buttons and swipe-to-dismiss gestures - screens don't hard-code
/// a target, so "close Stopwatch" goes back to Settings if that's
/// where the panel was opened from, or Clock if the user was there
/// before.
///
/// [`SwitchScreen`]: Action::SwitchScreen
/// [`Back`]: Action::Back
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do.
    None,
    /// Screen content changed, request a display refresh.
    Redraw,
    /// Switch to a specific target screen. Pushes the current
    /// screen onto the nav stack unless the current screen is
    /// [`ScreenId::QuickAccess`] or [`ScreenId::AppDrawer`] (modal
    /// replace-top semantics for both overlays).
    SwitchScreen(ScreenId),
    /// Pop the nav stack and return to the previous screen. Falls
    /// back to [`ScreenId::Clock`] when the stack is empty.
    Back,
    /// Request system shutdown.
    Shutdown,
    /// Run a hardware self-test. The main loop routes the id to the
    /// task that owns the underlying hardware and fires that task's
    /// command signal; the screen doesn't know which task handles it.
    /// Progress and results come back asynchronously as
    /// [`SystemEvent::SelfTestUpdated`] events.
    RunSelfTest(SelfTestId),
    /// Start the RTC hardware countdown timer. Duration is capped
    /// at 15300 seconds (4h15m) by the UI. When the timer expires
    /// the RTC task emits `SystemEvent::TimerExpired`.
    StartTimer { seconds: u32 },
    /// Cancel a running RTC countdown timer.
    CancelTimer,
    /// Set an RTC alarm at the given time. Optionally restrict to
    /// a single weekday (0=Sunday..6=Saturday). Fires
    /// `SystemEvent::AlarmFired` when matched.
    #[allow(dead_code)]
    SetAlarm { hour: u8, minute: u8, weekday: Option<u8> },
    /// Cancel a set RTC alarm.
    #[allow(dead_code)]
    CancelAlarm,
    /// Set the RTC date and time.
    SetTime { year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8 },
    /// Start a repeating haptic buzz pattern. The manager buzzes
    /// `on_ms` on, `off_ms` off, repeated until `StopBuzz` is sent.
    StartBuzz { on_ms: u16, off_ms: u16 },
    /// Stop an active buzz pattern.
    StopBuzz,
    /// Snooze the active alarm. The manager stops the buzz, sets
    /// the snoozed flag, and programs the RTC with now + 10 minutes.
    SnoozeAlarm,
    /// Dismiss the active alarm. The manager stops the buzz and
    /// navigates back to the previous screen.
    DismissAlarm,
    /// Wipe user-visible persistent data (config, alarms, logs,
    /// uploaded sounds) back to defaults. The manager calls
    /// `FlashFs::reset_user_data`, re-summarises usage, and emits
    /// a fresh `SystemEvent::StorageUsageUpdated`. Irrecoverable -
    /// wrap in a confirmation dialog on the UI side.
    FactoryReset,
    /// (Re-)probe the SD card. Emitted by the Storage settings
    /// screen when the user taps the "SD CARD" row. The manager
    /// probes the card, flips the mirror online flag, runs back-fill
    /// if newly online, and emits a fresh
    /// `SystemEvent::StorageUsageUpdated` so the screen reflects
    /// the new status.
    InitSd,
    /// Restore the in-flash config blobs from the SD mirror, then
    /// software-reset. Destructive; the UI wraps the trigger in a
    /// confirm tap. Requires SD to be online - the Settings row is
    /// disabled otherwise.
    RestoreFromSd,
    /// Persist the settings tree after an alarm edit. Emitted by
    /// screens after they mutate `data.config.alarms`. Subsumes
    /// Redraw - returning this also triggers a redraw, so screens
    /// don't need to emit both.
    PersistAlarms,
    /// Persist the current `Config` to flash. Same subsumes-Redraw
    /// semantics as `PersistAlarms`. Emitted internally by the
    /// Model after a `SetBrightness`-style mutation; screens can
    /// also return this directly after editing other Config fields.
    PersistConfig,

    /// Set the display brightness to `percent` (5..=100 slider range).
    /// Applies live to the panel and updates the in-memory `Config`
    /// + `SystemData` snapshot. Model also marks config dirty; the
    /// eventual save happens on the next `TouchReleased`, so finger
    /// scrubbing doesn't hammer flash.
    SetBrightness { percent: u8 },

    /// Flip `config.display.night_mode`. Model applies the new
    /// effective brightness to the panel immediately (night mode
    /// caps at `DisplayConfig::NIGHT_MODE_MAX_HW`) and marks config
    /// dirty so the change persists on the next `TouchReleased`.
    ToggleNightMode,

    /// Flip `config.display.always_on`. Model marks config dirty;
    /// the manager's idle-dim / idle-off path checks the live config
    /// each tick so the change takes effect on the next idle window.
    ToggleAlwaysOn,

    /// Flip `config.alerts.haptics_enabled`. Model marks config dirty;
    /// the manager gates motor effects on the live value.
    ToggleHaptics,

    /// Flip `config.alerts.sound_enabled`. Model marks config dirty; the
    /// manager gates the alarm / timer alert tone on the live value.
    ToggleSound,

    /// Begin the mic-test diagnostic: Model emits
    /// `Effect::AudioCommand(StartCapture)` so the audio task starts
    /// streaming `SystemEvent::MicLevel`. Emitted when the Mic Test
    /// settings view is opened.
    StartMicTest,
    /// End the mic-test diagnostic: Model emits the stop command for
    /// whichever mic-test audio mode is active (meter capture, tone
    /// sweep, or loopback). Emitted when the Mic Test view is left
    /// (leaving Settings any other way is caught by the model's
    /// safety net).
    StopMicTest,
    /// Play the speaker tone sweep (440 / 1000 / 880 Hz) from the Mic
    /// Test view. Model emits `Effect::AudioCommand(PlayTones)`; the
    /// audio task interrupts the running meter session, plays the
    /// sweep, and emits `SystemEvent::TonesDone`, which the view
    /// answers by restarting the meter.
    PlayToneTest,
    /// Switch the mic test to the LOOP test: record-then-playback
    /// "parrot" cycles that replay short mic recordings through the
    /// speaker. Model emits `Effect::AudioCommand(StartLoopback)`; the
    /// running capture session hands the I2S to the parrot session and
    /// the level meter keeps updating once per cycle. `StartMicTest`
    /// switches back to meter-only capture.
    StartLoopbackTest,

    /// Flip `config.alerts.dnd`. Pure config flip today - the alarm and
    /// notification routing that should respect this lands when
    /// those screens get real backing.
    ToggleDnd,

    /// Set the idle "auto-lock" duration. `secs` is the new value
    /// for `config.display.off_timeout_s` (when the display blanks);
    /// the Model also recomputes `dim_timeout_s` to a 2/3 fraction
    /// of `secs` (floored at 5s) so the dim stage scales with it.
    /// Marks config dirty; persisted on the next `TouchReleased`.
    SetAutoLock { secs: u32 },

    /// Enter idle-sleep immediately. Same effect as the BOOT button
    /// while awake (haptic pulse + display off + SLEEP_WATCH
    /// broadcast). Emitted by Quick Access's LOCK tile so the user
    /// can sleep the watch without reaching for the side button.
    /// The Model closes any active overlay before sleeping so the
    /// next wake lands on the underlying app, not the dismissed QA.
    Sleep,

    /// Start a GPS time-sync session. The Model forwards this as
    /// `Effect::GpsCommand(SyncOnce)` carrying the configured
    /// timezone offset; progress comes back as
    /// `SystemEvent::GpsSyncUpdated`. Emitted by the settings GPS
    /// view's SYNC button (which the view disables while a session
    /// is already running).
    GpsSync,

    /// Shift `config.time.tz_offset_minutes` by `delta_min` (the settings
    /// GPS view's +/- 15-minute stepper). The Model clamps to the
    /// real-world UTC offset range and marks config dirty; the next
    /// `TouchReleased` persists it.
    AdjustTimezone { delta_min: i16 },

    /// Flip `config.gps.tracking_enabled` (the settings GPS view's
    /// TRACKING toggle). Enabling kicks the first session
    /// immediately; disabling aborts one in flight.
    ToggleGpsTracking,

    /// Select the tracking cadence (the settings GPS view's
    /// CONT/15S/30S/60S buttons). A remembered preference - takes
    /// effect from the next kick when tracking is enabled.
    SetGpsCadence { cadence: crate::config::GpsTrackingCadence },
}

// -- Persistent app state ----------------------------------------------------

use embassy_time::{Duration, Instant};

/// Stopwatch run state, persisted across screen switches.
#[derive(Debug, Clone, Copy)]
pub enum StopwatchState {
    Idle,
    /// Counting. `start`/`accumulated` are the embassy-clock view
    /// that `elapsed()` reads; the embassy clock freezes across
    /// light sleep, so the model re-derives `start` from
    /// `anchor_secs` - the RTC seconds-since-midnight at this run
    /// segment's start - on every TimeUpdated (see
    /// `Model::resync_stopwatch`).
    Running { start: Instant, accumulated: Duration, anchor_secs: u32 },
    Paused { accumulated: Duration },
}

impl StopwatchState {
    /// Total elapsed duration regardless of current state.
    pub fn elapsed(&self) -> Duration {
        match self {
            Self::Idle => Duration::from_ticks(0),
            Self::Running { start, accumulated, .. } => {
                *accumulated + Instant::now().duration_since(*start)
            }
            Self::Paused { accumulated } => *accumulated,
        }
    }

    /// True if the stopwatch is actively counting.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

impl Default for StopwatchState {
    fn default() -> Self { Self::Idle }
}

/// Timer run state, persisted across screen switches.
#[derive(Debug, Clone, Copy)]
pub enum TimerState {
    /// Idle with a set duration (may be zero).
    Idle { duration: Duration },
    /// Counting down toward a deadline. The embassy Instant is
    /// resynced from RTC time on every TimeUpdated event.
    /// `target_secs` is the absolute target in seconds-since-midnight,
    /// used for the RTC resync calculation. `total_secs` is the
    /// duration the countdown was started with - the 100% reference
    /// for the timer dial, carried through pause/resume unchanged.
    Running { deadline: Instant, target_secs: u32, total_secs: u32 },
    /// Paused with time remaining. `total_secs` as in `Running`.
    Paused { remaining: Duration, total_secs: u32 },
}

impl TimerState {
    /// Remaining time, clamped to zero.
    pub fn remaining(&self) -> Duration {
        match self {
            Self::Idle { duration } => *duration,
            Self::Running { deadline, .. } => {
                let now = Instant::now();
                if now >= *deadline {
                    Duration::from_ticks(0)
                } else {
                    deadline.duration_since(now)
                }
            }
            Self::Paused { remaining, .. } => *remaining,
        }
    }

    /// The countdown's 100% reference: the started-with duration
    /// while running/paused, else the set duration (so an idle,
    /// armed timer reads as full).
    pub fn total_secs(&self) -> u64 {
        match self {
            Self::Idle { duration } => duration.as_secs(),
            Self::Running { total_secs, .. }
            | Self::Paused { total_secs, .. } => *total_secs as u64,
        }
    }

    /// True if the timer is actively counting down.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

impl Default for TimerState {
    fn default() -> Self {
        Self::Idle { duration: Duration::from_ticks(0) }
    }
}

// -- Alarm state -------------------------------------------------------------

/// Maximum number of user-configurable alarms.
pub const MAX_ALARMS: usize = 8;

/// One alarm entry. Persisted per slot as its own tagged config
/// field (see the config module docs), hence the serde derive on
/// the ENTRY while [`AlarmState`] itself is never serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AlarmEntry {
    pub hour: u8,
    pub minute: u8,
    /// Bitmask of active days: bit 0 = Sunday, bit 1 = Monday, ...
    /// bit 6 = Saturday. 0x7F = every day, 0x3E = Mon-Fri,
    /// 0x41 = Sat+Sun, 0x00 = disabled.
    pub days: u8,
    pub enabled: bool,
}

impl Default for AlarmEntry {
    fn default() -> Self {
        Self { hour: 0, minute: 0, days: 0x7F, enabled: false }
    }
}

impl AlarmEntry {
    /// True if this alarm fires on the given weekday (0=Sunday..6=Saturday).
    pub fn fires_on(&self, weekday: u8) -> bool {
        self.enabled && (self.days & (1 << weekday)) != 0
    }
}

/// The alarm list plus its transient runtime flags. Lives in the
/// settings tree as `config.alarms`; screens mutate it there.
///
/// Persistence is per-entry (each slot is its own tagged config
/// field) - this struct as a whole is never serialized, so
/// `active_hw` / `alerting` / `snoozed` reset to defaults on every
/// boot by construction: a mid-alarm reboot must not resume ringing,
/// and the RTC programming is replanned from time + entries on the
/// first 1 Hz tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlarmState {
    pub entries: [AlarmEntry; MAX_ALARMS],
    /// Index of the alarm currently programmed into the RTC
    /// hardware, or None if no alarm is active. Recomputed on
    /// boot by `plan_reprogram` from current time + entries.
    pub active_hw: Option<usize>,
    /// True when an alarm has fired and the user hasn't dismissed it.
    pub alerting: bool,
    /// True when a snooze is active. The manager skips regular
    /// reprogramming while this is set. Cleared when the snooze
    /// alarm fires.
    pub snoozed: bool,
}

impl AlarmState {
    /// Compile-time default (all slots disabled, everyday mask) -
    /// const so `Config::DEFAULT` can embed it.
    pub const DEFAULT: Self = Self {
        entries: [AlarmEntry {
            hour: 0,
            minute: 0,
            days: 0x7F,
            enabled: false,
        }; MAX_ALARMS],
        active_hw: None,
        alerting: false,
        snoozed: false,
    };
}

impl Default for AlarmState {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl AlarmState {
    /// Count of enabled alarms.
    pub fn enabled_count(&self) -> usize {
        self.entries.iter().filter(|e| e.enabled).count()
    }

    /// Find the next alarm that should fire after the given time
    /// and weekday. Returns the index, or None if no alarms are enabled.
    pub fn next_alarm(&self, hour: u8, minute: u8, weekday: u8) -> Option<usize> {
        let now_mins = hour as u16 * 60 + minute as u16;
        let mut best: Option<(usize, u16)> = None; // (index, minutes_until)

        for (i, entry) in self.entries.iter().enumerate() {
            if !entry.enabled {
                continue;
            }
            let alarm_mins = entry.hour as u16 * 60 + entry.minute as u16;

            // Check each of the next 7 days.
            for day_offset in 0u8..7 {
                let check_day = (weekday + day_offset) % 7;
                if !entry.fires_on(check_day) {
                    continue;
                }
                let mut mins_until = day_offset as u16 * 24 * 60 + alarm_mins;
                if day_offset == 0 && alarm_mins <= now_mins {
                    // Already passed today, try next week.
                    continue;
                }
                if day_offset == 0 {
                    mins_until = alarm_mins - now_mins;
                }
                match best {
                    Some((_, best_mins)) if mins_until < best_mins => {
                        best = Some((i, mins_until));
                    }
                    None => {
                        best = Some((i, mins_until));
                    }
                    _ => {}
                }
                break; // found the earliest firing day for this entry
            }
        }
        best.map(|(i, _)| i)
    }

    /// Compute the clock-time (hour, minute) for a snooze alarm
    /// firing `minutes_from_now` minutes after the given
    /// `hour`:`minute`. Handles hour and midnight wraparound.
    ///
    /// Associated function (no `self`) so the snooze duration is
    /// decided by the caller and the calculation is easy to host-test.
    pub fn compute_snooze(hour: u8, minute: u8, minutes_from_now: u8) -> (u8, u8) {
        let total = minute as u16 + minutes_from_now as u16;
        let snooze_minute = (total % 60) as u8;
        let hours_added = (total / 60) as u8;
        let snooze_hour = (hour + hours_added) % 24;
        (snooze_hour, snooze_minute)
    }

    /// Decide what the RTC should be programmed to based on the
    /// current time and the list of enabled alarms. Mutates
    /// `active_hw` to the new target (so subsequent ticks with
    /// the same inputs return `None`) and returns the command
    /// the caller should forward to the RTC driver.
    ///
    /// Returns `None` when snoozed (the snooze alarm is already
    /// in the RTC and must not be overwritten) or when nothing
    /// changed since the last call.
    pub fn plan_reprogram(
        &mut self,
        hour: u8,
        minute: u8,
        weekday: u8,
    ) -> Option<AlarmReprogram> {
        self.plan_reprogram_inner(hour, minute, weekday, false)
    }

    /// Like [`plan_reprogram`] but always emits a command even when
    /// the next-alarm index hasn't changed. Use this when the user
    /// just edited entries: the index might still point at the same
    /// slot, but its HH:MM may have changed and the chip's registers
    /// would otherwise stay stuck at the old value.
    pub fn plan_reprogram_force(
        &mut self,
        hour: u8,
        minute: u8,
        weekday: u8,
    ) -> Option<AlarmReprogram> {
        self.plan_reprogram_inner(hour, minute, weekday, true)
    }

    fn plan_reprogram_inner(
        &mut self,
        hour: u8,
        minute: u8,
        weekday: u8,
        force: bool,
    ) -> Option<AlarmReprogram> {
        if self.snoozed {
            return None;
        }
        let next = self.next_alarm(hour, minute, weekday);
        if !force && next == self.active_hw {
            return None;
        }
        self.active_hw = next;
        Some(match next {
            Some(idx) => {
                let e = &self.entries[idx];
                AlarmReprogram::SetAlarm { hour: e.hour, minute: e.minute }
            }
            None => AlarmReprogram::CancelAlarm,
        })
    }
}

/// Result of [`AlarmState::plan_reprogram`]: what the caller
/// should command the RTC driver to do now. Opaque to `app-core`
/// beyond being the enum; `firmware` translates this into the
/// concrete `RtcCommand` channel messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmReprogram {
    /// Program the RTC to fire at the given hour:minute.
    SetAlarm { hour: u8, minute: u8 },
    /// Clear the RTC alarm (no enabled alarms).
    CancelAlarm,
}

#[cfg(test)]
mod alarm_tests {
    use super::*;

    fn state_with(entries: &[(u8, u8, u8 /* days mask */, bool /* enabled */)]) -> AlarmState {
        let mut s = AlarmState::default();
        for (i, &(h, m, days, enabled)) in entries.iter().enumerate() {
            s.entries[i] = AlarmEntry { hour: h, minute: m, days, enabled };
        }
        s
    }

    #[test]
    fn snoozed_blocks_reprogram() {
        let mut s = state_with(&[(7, 0, 0b0111_1111, true)]);
        s.snoozed = true;
        assert_eq!(s.plan_reprogram(6, 0, 0), None);
    }

    #[test]
    fn fresh_alarm_produces_set_command() {
        let mut s = state_with(&[(7, 30, 0b0111_1111, true)]);
        assert_eq!(
            s.plan_reprogram(6, 0, 0),
            Some(AlarmReprogram::SetAlarm { hour: 7, minute: 30 }),
        );
        assert_eq!(s.active_hw, Some(0));
    }

    #[test]
    fn idempotent_when_nothing_changed() {
        let mut s = state_with(&[(7, 30, 0b0111_1111, true)]);
        assert!(s.plan_reprogram(6, 0, 0).is_some());
        // Subsequent call with no change returns None.
        assert_eq!(s.plan_reprogram(6, 0, 0), None);
    }

    #[test]
    fn snooze_adds_minutes_no_wrap() {
        assert_eq!(AlarmState::compute_snooze(7, 30, 10), (7, 40));
    }

    #[test]
    fn snooze_wraps_minutes_into_next_hour() {
        assert_eq!(AlarmState::compute_snooze(7, 55, 10), (8, 5));
    }

    #[test]
    fn snooze_wraps_past_midnight() {
        // 23:55 + 10 min = 00:05
        assert_eq!(AlarmState::compute_snooze(23, 55, 10), (0, 5));
    }

    #[test]
    fn snooze_with_larger_delay_spans_multiple_hours() {
        // 22:00 + 180 min = 01:00 next day
        assert_eq!(AlarmState::compute_snooze(22, 0, 180), (1, 0));
    }

    #[test]
    fn disabling_all_alarms_produces_cancel() {
        let mut s = state_with(&[(7, 30, 0b0111_1111, true)]);
        assert!(s.plan_reprogram(6, 0, 0).is_some());
        // Disable the alarm.
        s.entries[0].enabled = false;
        assert_eq!(s.plan_reprogram(6, 0, 0), Some(AlarmReprogram::CancelAlarm));
        assert_eq!(s.active_hw, None);
    }
}

// -- System data snapshot ----------------------------------------------------

// Per-peripheral snapshot data structs live in `app-core::data`.
// Re-exported here so screens can `use crate::ui::types::{TimeData,
// PowerData, MotionData, TouchData, StorageUsage, SystemData}` from
// one place.
pub use crate::data::{MotionData, PowerData, StorageUsage, TimeData, TouchData};

/// System state, passed to screens on render and events.
///
/// Peripheral snapshots are updated by the manager's event handler.
/// App state (stopwatch, timer) is mutated directly by screens
/// via `&mut SystemData` in `on_event`.
///
/// Intentionally not `Copy`: [`crate::data::BatteryHistory`] owns a
/// ring buffer that can't live on the stack silently on every pass.
/// The struct is always accessed through `&SystemData` / `&mut
// -- Notifications -----------------------------------------------------------

/// Maximum simultaneous active notifications. Past this the oldest
/// is dropped to make room - the overlay is a recent-events queue,
/// not a long-term history.
pub const MAX_NOTIFICATIONS: usize = 16;

/// Severity classification for a notification, drives the row's
/// border + label + badge colour. Mirrors the Nightwatch spec's
/// four-level taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationSeverity {
    /// Critical alert. signal red. Alarm fired, fault, etc.
    Critical,
    /// Warning. yellow. Timer expired, low battery soon.
    Warning,
    /// Success. green. Self-test passed, save committed.
    Ok,
    /// Informational. cyan. Connectivity changes, generic info.
    Info,
}

/// Where a notification originated. Drives the tap-to-route target
/// and the row's title/badge defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationSource {
    Alarm,
    Timer,
}

impl NotificationSource {
    /// Screen to switch to when the notification row is tapped.
    pub fn target(self) -> ScreenId {
        match self {
            Self::Alarm => ScreenId::Alarm,
            Self::Timer => ScreenId::Timer,
        }
    }

    /// UPPERCASE row title shown in the overlay.
    pub fn title(self) -> &'static str {
        match self {
            Self::Alarm => "ALARM",
            Self::Timer => "TIMER",
        }
    }

    /// Single-character badge glyph rendered top-left of the row.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Alarm => "!",
            Self::Timer => "*",
        }
    }
}

/// One notification entry in the overlay's row list.
#[derive(Debug, Clone)]
pub struct Notification {
    pub severity: NotificationSeverity,
    pub source: NotificationSource,
    /// Free-form subtitle line drawn under the title. Snapshot of
    /// the relevant context at notification-creation time, e.g.
    /// `"WAKE @ 06:30"` for an alarm fire.
    pub subtitle: heapless::String<32>,
    /// Hour-of-day (0..=23) the notification was created.
    pub ts_hour: u8,
    /// Minute-of-hour (0..=59) the notification was created.
    pub ts_minute: u8,
}

/// Active notification queue. Newest entries pushed to the end;
/// rendered top-to-bottom in the overlay (newest on top via reverse
/// iteration at render time).
#[derive(Debug, Clone, Default)]
pub struct NotificationState {
    pub entries: heapless::Vec<Notification, MAX_NOTIFICATIONS>,
}

impl NotificationState {
    /// Push a new notification. Drops the oldest entry first if
    /// the queue is at capacity.
    pub fn push(&mut self, n: Notification) {
        if self.entries.is_full() {
            self.entries.remove(0);
        }
        let _ = self.entries.push(n);
    }

    /// Remove the notification at `idx`. No-op if `idx` is out of
    /// range.
    pub fn dismiss(&mut self, idx: usize) {
        if idx < self.entries.len() {
            self.entries.remove(idx);
        }
    }
}

/// SystemData` references, so the missing `Copy` costs nothing at
/// call sites - see `Model::new` (only by-value use).
#[derive(Debug, Clone, Default)]
pub struct SystemData {
    pub time: TimeData,
    pub power: PowerData,
    pub motion: MotionData,
    /// Panel pixels hidden under this device's case/bezel (from
    /// [`crate::data::SafeArea`], bin-provided). Consumed by the
    /// edge-hugging chrome only.
    pub safe_area: crate::data::SafeArea,
    /// The board's IMU chip name, from `SystemEvent::ImuIdentified`;
    /// empty until the IMU task announces it.
    pub imu_name: &'static str,
    pub touch: TouchData,
    /// Live microphone input level (0..=255) while the mic-test
    /// diagnostic is capturing; 0 otherwise. Updated from
    /// `SystemEvent::MicLevel`.
    pub mic_level: u8,
    /// Flash-filesystem occupancy. Updated at boot and after
    /// every save / reset via `SystemEvent::StorageUsageUpdated`.
    pub storage: StorageUsage,
    /// Recent battery-percent samples for the settings battery
    /// graph. Seeded at boot from the flash event log, appended
    /// on every `SystemEvent::BatteryChanged`.
    pub battery_history: crate::data::BatteryHistory,
    pub tick_count: u32,
    pub stopwatch: StopwatchState,
    pub timer: TimerState,
    /// Active notification queue surfaced in the global Notifications
    /// overlay. Producers are the alarm-fired and timer-expired
    /// hooks in [`crate::Model::apply_snapshot`]; consumers is the
    /// overlay screen (renders rows, tap-to-route, swipe-to-dismiss).
    pub notifications: NotificationState,
    /// Per-test latest result, indexed by [`SelfTestId`] cast to
    /// `usize`. Updated by the main loop whenever a
    /// [`SystemEvent::SelfTestUpdated`] arrives.
    pub self_tests: [SelfTestResult; NUM_SELF_TESTS],
    /// Read-only snapshot of the runtime `Config`. Kept in sync by
    /// the Model so any screen can render `data.config.*` without
    /// its own backing store or constructor parameter. Mutation
    /// goes through `Action::PersistConfig` / `Action::SetBrightness`
    /// etc., never by a screen editing this field.
    pub config: crate::config::Config,
    /// Wall-time seconds since this boot. Sourced by the manager from
    /// the SoC's RTC slow-clock counter (`Rtc::time_since_power_up`),
    /// rebaselined at boot - always-on through light sleep, so it
    /// advances continuously regardless of duty cycle, but resets on
    /// each boot/reflash (the raw counter itself survives digital
    /// resets, so the manager subtracts a boot baseline).
    pub uptime_secs: u32,

    /// Embassy-active seconds since the Model was constructed: the
    /// `Instant::now() - boot` delta, which **pauses during light
    /// sleep** because the time driver (TIMG0) is gated then. Pair
    /// with `uptime_secs` to read off the device's duty cycle.
    pub active_secs: u32,

    /// Diagnostic: count of completed light-sleep cycles since boot.
    /// Paced by the ~5 s heartbeat when really sleeping (~12/min); a
    /// far higher rate relative to `uptime_secs` means the CPU is
    /// instant-waking instead of gating off.
    pub sleep_cycles: u32,

    /// Which optional hardware this board carries. Set once by the
    /// manager from the bin's `Bringup::capabilities` before the
    /// model is constructed; never changes at runtime. Screens gate
    /// board-specific rows/views on it.
    pub capabilities: crate::data::Capabilities,

    /// Progress of the latest GPS time-sync session, from
    /// `SystemEvent::GpsSyncUpdated`. Only ever leaves `Idle` on
    /// boards whose capabilities include GPS.
    pub gps_sync: crate::data::GpsSyncState,

    /// Steps taken today, accumulated by the Model from the IMU's
    /// running total (`MotionData::steps`) and zeroed at the local-
    /// midnight day rollover. RAM-only: a reboot loses the day so
    /// far. Stays 0 on boards without the steps capability.
    pub steps_today: u32,

    /// Position from the last GPS sync session that got a usable
    /// fix, from `SystemEvent::GpsFixUpdated`; `None` until then
    /// (and always on boards without GPS). Not live - see
    /// [`crate::data::GpsFix`].
    pub gps_fix: Option<crate::data::GpsFix>,

    /// Seconds until the tracking scheduler kicks the next GPS
    /// session; `None` while a session is running (or kicked), when
    /// tracking is off, or on boards without GPS. Recomputed by the
    /// Model on every `TimeUpdated` - lets the settings STATUS
    /// panel say what the receiver is doing between sessions
    /// (powered down, waiting) instead of parroting the previous
    /// session's outcome.
    pub gps_next_session_secs: Option<u32>,
}

// -- Screen trait -------------------------------------------------------------

// -- Dirty-region tracking ---------------------------------------------------

/// Maximum number of explicit dirty rectangles a screen can declare
/// per frame before the renderer falls back to a full redraw. Sized
/// for the worst case in the current UI (clock face: telemetry,
/// hour digits, minute digits, meta row, GPS coords row, bottom
/// tiles) without burning stack for a wider list.
pub const MAX_DIRTY_RECTS: usize = 6;

/// What part of the screen needs to be re-rendered this frame.
///
/// The renderer walks the panel in tile-sized horizontal bands. Without
/// per-screen invalidation it has to render every tile in case something
/// changed inside it (and then hashes the result to decide whether to push
/// the tile over QSPI). That CPU cost is the same whether anything changed
/// or not, and the per-tile widget-tree walk dominates render time.
///
/// `DirtyRegion` lets a screen narrow that down: it returns the rectangles
/// where its state actually changed since the last frame, and the renderer
/// renders only the tiles those rectangles touch. A screen that doesn't
/// override [`Screen::dirty_rects`] keeps the safe default of [`FullScreen`]
/// and the renderer behaves exactly as it did before invalidation existed.
///
/// [`FullScreen`]: DirtyRegion::FullScreen
#[derive(Debug, Clone)]
pub enum DirtyRegion {
    /// Everything is dirty - render every tile. Safe fallback used by
    /// the trait's default impl and by the renderer at wake / screen-
    /// transition (where the panel's GRAM may not match anything we've
    /// drawn yet).
    FullScreen,
    /// A short list of dirty rectangles. Empty means nothing changed
    /// and no tiles get rendered or pushed.
    Rects(heapless::Vec<Rectangle, MAX_DIRTY_RECTS>),
}

impl DirtyRegion {
    /// Empty region - nothing changed, skip the frame entirely.
    pub fn empty() -> Self {
        Self::Rects(heapless::Vec::new())
    }

    /// Convenience: one-rect region.
    pub fn rect(x: i32, y: i32, w: u32, h: u32) -> Self {
        let mut v = heapless::Vec::new();
        let _ = v.push(Rectangle::new(Point::new(x, y), Size::new(w, h)));
        Self::Rects(v)
    }

    /// True when the region has no rectangles.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Rects(v) if v.is_empty())
    }

    /// Add a rectangle to the region. Overflows past
    /// [`MAX_DIRTY_RECTS`] are absorbed by switching to [`FullScreen`]
    /// so the renderer stays correct without the screen having to
    /// remember the cap.
    ///
    /// [`FullScreen`]: DirtyRegion::FullScreen
    pub fn add(&mut self, rect: Rectangle) {
        match self {
            Self::FullScreen => {}
            Self::Rects(v) => {
                if v.push(rect).is_err() {
                    *self = Self::FullScreen;
                }
            }
        }
    }
}

// -- Render context ----------------------------------------------------------

/// Per-call context handed to [`Screen::render`].
///
/// The renderer walks the panel in horizontal tiles - one render call
/// fires per tile. `RenderCtx` tells the screen which slice of the
/// panel its current call is targeting, so a screen that knows the
/// y-range of every widget it draws can skip widget setup entirely
/// for widgets outside the current tile.
///
/// Screens that don't care (most: clock, stopwatch, dial-shaped
/// screens) can ignore the field entirely; the driver still clips
/// per-pixel inside its draw methods. Screens with virtualizable lists
/// (settings, alarm list, future scrollable views) use it to skip the
/// per-row format-string / icon-lookup / iterator-construction work
/// for rows that fall off this tile.
///
/// Wrapping the parameters in a struct keeps the signature
/// forward-compatible: adding new fields (e.g. `is_scrolling`,
/// `panel_dims`) later won't break every screen impl.
#[derive(Debug, Clone, Copy)]
pub struct RenderCtx {
    /// Panel-y of the top row of the current tile.
    pub tile_y: u16,
    /// Number of rows in the current tile. Equals
    /// `firmware_hal::display::TILE_H` for every full tile and is
    /// smaller for the short final tile at the bottom of the panel.
    pub tile_h: u16,
}

impl RenderCtx {
    /// A "render the whole panel" context. Useful for one-shot full
    /// renders (host tests, full-FB future fallback) where there is
    /// no tile loop driving the call.
    pub fn full_panel(panel_h: u16) -> Self {
        Self { tile_y: 0, tile_h: panel_h }
    }

    /// Inclusive-exclusive y-range covered by this tile.
    pub fn y_range(&self) -> (i32, i32) {
        (self.tile_y as i32, self.tile_y as i32 + self.tile_h as i32)
    }

    /// True if the rectangle `[y0, y1)` overlaps this tile's y-range.
    /// Cheap helper for "should I render this widget at all?" checks.
    pub fn intersects_y(&self, y0: i32, y1: i32) -> bool {
        let (t0, t1) = self.y_range();
        y1 > t0 && y0 < t1
    }
}

// -- Screen trait -------------------------------------------------------------

/// Trait that all UI screens implement.
///
/// Screens are stateful - they can track animations, scroll positions,
/// selection state, etc. The SystemManager doesn't know or care about
/// screen internals.
///
/// ## Lifecycle
///
/// - [`on_mount`] runs once right after the screen is switched to, with
///   the current [`SystemData`] available. Use it to read initial state
///   (e.g. a diagnostics screen that kicks off a self-test, a file
///   explorer that reads the current directory) before the first render.
/// - [`on_unmount`] runs once right before the screen is swapped out
///   or dropped. Use it to release resources or persist state.
/// - [`render`] is called every frame. Must be a pure function of the
///   screen's own state plus the provided [`SystemData`] snapshot.
/// - [`on_event`] is called for every [`SystemEvent`] the main loop
///   receives while this screen is active. Returns an [`Action`]
///   telling the outer navigator what to do next.
///
/// Default implementations are provided for the lifecycle hooks so
/// screens only override what they need - `render` and usually
/// `on_event` are the only methods most screens have to provide.
///
/// ## Partial rendering
///
/// [`dirty_rects`] and [`clear_dirty`] together let a screen tell the
/// renderer "only these regions changed since I was last rendered".
/// Default impls return [`DirtyRegion::FullScreen`] and a no-op
/// respectively, so screens that don't opt in render exactly as before
/// (every tile, every frame). Screens that do opt in track their own
/// "last rendered" snapshot (e.g. last second value, last alarm
/// string) and emit one rect per field whose value differs from the
/// snapshot.
///
/// [`on_mount`]: Screen::on_mount
/// [`on_unmount`]: Screen::on_unmount
/// [`render`]: Screen::render
/// [`on_event`]: Screen::on_event
/// [`dirty_rects`]: Screen::dirty_rects
/// [`clear_dirty`]: Screen::clear_dirty
pub trait Screen {
    /// Called once when this screen becomes active, before the first
    /// render. Read anything that needs to be loaded on open here.
    fn on_mount(&mut self, _data: &SystemData) {}

    /// Called once when this screen is about to be swapped out or
    /// dropped. Release resources or persist state here.
    fn on_unmount(&mut self) {}

    /// Render the screen to the display. Called once per dirty tile
    /// per frame (so a screen with `dirty_rects` returning `FullScreen`
    /// on an 11-tile panel sees 11 `render` calls per frame).
    ///
    /// `ctx` tells the screen which horizontal band of the panel this
    /// call is targeting. Screens with virtualizable list contents
    /// (settings index, alarm list, ...) use `ctx.intersects_y(...)`
    /// to skip the per-row format/icon-lookup work for rows entirely
    /// outside this tile. Screens without that optimization opportunity
    /// (clock face, dial-shaped views) ignore the field and let the
    /// driver's per-pixel clip handle out-of-tile writes.
    fn render<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    );

    /// Handle a system event. Return an Action to tell the manager what to do.
    /// `data` is mutable so screens can update shared persistent state
    /// (stopwatch, timer) directly.
    fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action;

    /// Compare the screen's own "last rendered" snapshot against the
    /// current [`SystemData`] and return the regions where the visible
    /// output would differ from what's already on the panel.
    ///
    /// The default impl returns [`DirtyRegion::FullScreen`] - safe, but
    /// gives up the per-tile-skip optimization. Screens that want the
    /// optimization should track per-field snapshot state and return a
    /// list of rectangles describing what would change.
    ///
    /// Called with `&self` so it cannot mutate the snapshot - mutation
    /// happens in [`clear_dirty`] after the renderer has actually
    /// re-drawn.
    ///
    /// [`clear_dirty`]: Screen::clear_dirty
    fn dirty_rects(&self, _data: &SystemData) -> DirtyRegion {
        DirtyRegion::FullScreen
    }

    /// Called by the renderer immediately after a frame has been
    /// flushed. The screen should update its "last rendered" snapshot
    /// to match `data` so the next [`dirty_rects`] call has a fresh
    /// baseline to compare against.
    ///
    /// Default impl is a no-op (matches the default
    /// `dirty_rects = FullScreen`: no snapshot to maintain).
    ///
    /// [`dirty_rects`]: Screen::dirty_rects
    fn clear_dirty(&mut self, _data: &SystemData) {}
}
