//! Application state machine - hardware-agnostic.
//!
//! `Model` owns everything the app logically *is*: cached sensor
//! snapshots, the current screen, nav stack, display mode, sleep
//! flag, buzz pattern, config. It exposes two main entry points:
//!
//!   * [`Model::handle_event`] - fold a [`SystemEvent`] into state
//!     and return a list of [`Effect`]s for the caller to enact.
//!   * [`Model::tick`] - advance time-driven state (buzz phase
//!     transitions, dim/sleep idle timers) and return effects.
//!
//! The manager on the firmware side executes the returned effects
//! by calling hardware (display transitions, RTC signal channels,
//! motor GPIO, shutdown, etc.). Nothing in this module touches
//! hardware directly, so the full dispatch loop can be unit-tested
//! on the host.

use embassy_time::{Duration, Instant};
use heapless::Vec;

use crate::buzz::{BuzzAction, BuzzPattern};
use crate::commands::{
    AudioCommand, GpsCommand, ImuCommand, RtcCommand, SleepState, WifiCommand,
};
use crate::config::Config;
use crate::data::TouchData;
use crate::events::{
    self, SwipeDir, SwipeRegion, SystemEvent, NUM_SELF_TESTS,
};
use crate::nav::NavStack;
use crate::ui::screens::ActiveScreen;
use crate::ui::types::{
    Action, AlarmReprogram, AlarmState, DisplayState, Notification, NotificationSeverity,
    NotificationSource, ScreenId, StopwatchState, SystemData, TimerState,
};

/// Upper bound on the number of [`Effect`]s produced by a single
/// event/tick. In practice even the heaviest handlers emit 2-3.
pub const MAX_EFFECTS_PER_CALL: usize = 8;


/// Fixed-size buffer of effects returned by `Model` methods.
pub type Effects = Vec<Effect, MAX_EFFECTS_PER_CALL>;

/// Grace window after a provisional wake (motion or bare GPIO): if
/// no user activity - a wrist-raise, touch, or button - confirms the
/// wake before this expires, the device goes straight back to sleep
/// instead of burning the full idle window on a false wake (e.g.
/// a motion sensor firing on typing).
const MOTION_WAKE_GRACE: Duration = Duration::from_secs(5);

/// What the caller should do to hardware after a `Model` call.
///
/// Each variant maps 1:1 to a concrete hardware action on the
/// manager side. Channel-delivered commands (`RtcCommand`,
/// `ImuCommand`) are carried verbatim so the manager's dispatch
/// is a direct pass-through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Transition the display between two power states (async on
    /// the manager side - issues DCS commands over SPI).
    TransitionDisplay { from: DisplayState, to: DisplayState },

    /// Broadcast a sleep-state change on `SLEEP_WATCH` for
    /// subscribers (touch/IMU/power tasks) to react.
    BroadcastSleep(SleepState),

    /// Motor GPIO on / off (one-shot edge).
    MotorOn,
    MotorOff,
    /// Short pulse: motor on, blocking-delay `duration_ms`, motor
    /// off. Used for the BOOT-press "going to sleep" haptic.
    MotorPulse { duration_ms: u32 },

    /// Forward a command to the RTC task via `RTC_COMMAND`.
    RtcCommand(RtcCommand),

    /// Forward a command to the IMU task via `IMU_COMMAND`.
    ImuCommand(ImuCommand),

    /// Forward a command to the audio task via `AUDIO_COMMAND`.
    /// Carries the alarm / timer alert tone start / stop. The manager
    /// gates `PlayAlarm` on `config.alerts.sound_enabled` (mirroring how
    /// `MotorOn` gates on `haptics_enabled`); `Stop` always forwards.
    AudioCommand(AudioCommand),

    /// Forward a command to the board's GPS task via `GPS_COMMAND`.
    /// Boards without a GPS task have no consumer; the UI entry
    /// point is capability-gated, so this only fires where hardware
    /// exists.
    GpsCommand(GpsCommand),

    /// Forward a command to the shared WiFi task via `WIFI_COMMAND`.
    /// Builds without the WiFi feature have no consumer; the UI entry
    /// point is capability-gated, so this only fires where the radio
    /// task exists.
    WifiCommand(WifiCommand),

    /// Immediate shutdown request (Action::Shutdown from a screen).
    Shutdown,

    /// Wipe user-visible persistent data (config, alarms, logs,
    /// uploaded sounds, etc.) back to defaults without reformatting
    /// the filesystem. Manager calls `FlashFs::reset_user_data`,
    /// re-summarises usage, and emits a fresh
    /// `SystemEvent::StorageUsageUpdated` (change-detected against
    /// the last known value).
    FactoryReset,

    /// User-triggered SD probe + back-fill. Manager calls
    /// `storage::probe_sd`, flips the mirror online flag, runs
    /// back-fill if the probe succeeded, and emits a fresh
    /// `SystemEvent::StorageUsageUpdated` so the settings screen
    /// sees the new status.
    ProbeSd,

    /// Restore flash-side config blobs from the SD mirror, then
    /// software-reset. The in-memory Model still holds pre-restore
    /// state, so proceeding without a reset would let the next
    /// save_blob clobber the freshly-restored flash. The reset also
    /// sidesteps mid-alarm / mid-timer edge cases.
    RestoreFromSd,

    /// Persist the settings tree (`Config`, alarms included) to
    /// `/system/config/config.bin` on flash. Triggered by
    /// `Action::PersistConfig` / `Action::PersistAlarms` after any
    /// change to `cached_data.config`.
    SaveConfig,

    /// Apply a new display brightness immediately. Value is the
    /// hardware register range (0..=255) after Model maps the
    /// slider percent. Fired by `Action::SetBrightness` so the
    /// change is visible before the next SaveConfig persists it.
    SetDisplayBrightness(u8),
}

/// Application state machine.
///
/// Fields are private. External mutation happens only through
/// [`Self::handle_event`], [`Self::tick`], and a small set of
/// explicit setters below. Read access goes through the named
/// accessors (`sleeping`, `needs_redraw`, `cached_data`, ...).
pub struct Model {
    cached_data: SystemData,
    screen: ActiveScreen,
    nav_stack: NavStack,
    display_state: DisplayState,
    last_activity: Instant,
    /// Boot timestamp captured in `Model::new`. Source of truth for
    /// `cached_data.uptime_secs`, recomputed on every `tick`.
    boot: Instant,
    sleeping: bool,
    needs_redraw: bool,
    config: Config,
    /// True when `config` has been mutated since the last
    /// `SaveConfig` emit. Any action that changes config flips
    /// this; the next `TouchReleased` flushes it to flash. Keeps
    /// flash writes down to one per gesture rather than one per
    /// drag-pixel.
    config_dirty: bool,
    buzz: Option<BuzzPattern>,
    /// Which mic-test audio mode is active. Lets the model stop the
    /// right session as a safety net if the user leaves Settings by
    /// any path (not just the view's Back button) or the device
    /// sleeps.
    mic_test: MicTestMode,
    /// Set when sleep entry force-stopped an active mic-test mode.
    /// Sleep is a pause, not an exit: the MicTest view is still on
    /// screen (and can't change while asleep), so `wake` restarts the
    /// stored mode instead of leaving a dead meter.
    mic_resume_on_wake: Option<MicTestMode>,
    /// Deadline of the provisional-wake grace window
    /// ([`MOTION_WAKE_GRACE`]), set when a motion / bare-GPIO wake
    /// turned the display on with no confirmed human behind it.
    /// Cleared by any user activity; expiry in `tick` re-enters
    /// sleep.
    motion_wake_grace: Option<Instant>,
    /// Last step-counter running total seen from the IMU. The first
    /// observation after boot is the baseline (no steps credited);
    /// afterwards each increase is added to `steps_today`. A total
    /// BELOW the previous one means the chip's engine restarted from
    /// zero - the new total then counts in full.
    last_step_total: Option<u32>,
    /// `uptime_secs` at the last tracking-session kick; `None` means
    /// the next due-check kicks immediately. Uptime (RTC slow clock)
    /// rather than an embassy Instant because the cadence must keep
    /// counting through light sleep.
    last_track_kick: Option<u32>,
    /// Consecutive tracking sessions that ended `NoSignal`. At
    /// [`TRACK_AUTO_OFF_FAILURES`] the model flips
    /// `gps_tracking_enabled` off - fixless sessions mean indoors,
    /// where the receiver burns acquisition current forever.
    track_failures: u8,
    /// A tracking kick is in flight but the task hasn't reported
    /// yet. Scheduler bookkeeping ONLY - `gps_sync` stays the
    /// task's reported truth. Cleared by the next `GpsSyncUpdated`
    /// (or by toggling tracking off, where a queued kick may have
    /// been overwritten by the Abort).
    track_pending: bool,
}

/// Consecutive fixless tracking sessions before tracking turns
/// itself off.
const TRACK_AUTO_OFF_FAILURES: u8 = 3;

/// Session budget handed to interval-mode tracking kicks. Outdoors
/// the receiver hot-starts from BBR in a few seconds; 30 s covers a
/// warm start without approaching the 120 s manual-sync hunt.
const TRACK_BUDGET_INTERVAL_SECS: u16 = 30;

/// Session budget for continuous-mode kicks - the full manual-sync
/// hunt; the model re-kicks the moment a session ends.
const TRACK_BUDGET_CONTINUOUS_SECS: u16 = 120;

/// Audio mode of the mic-test diagnostic, mirrored by the model so its
/// safety nets know which stop command ends the active session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MicTestMode {
    Off,
    /// Meter-only capture (`StartCapture` / `StopCapture`).
    Capture,
    /// One-shot speaker tone sweep (`PlayTones` / `StopTones`).
    Tones,
    /// Mic -> speaker loopback (`StartLoopback` / `StopLoopback`).
    Loopback,
}

impl MicTestMode {
    /// The command that stops this mode's session, if one is active.
    fn stop_command(self) -> Option<AudioCommand> {
        match self {
            MicTestMode::Off => None,
            MicTestMode::Capture => Some(AudioCommand::StopCapture),
            MicTestMode::Tones => Some(AudioCommand::StopTones),
            MicTestMode::Loopback => Some(AudioCommand::StopLoopback),
        }
    }
}

impl Model {
    /// Build a fresh model with the supplied initial snapshot and
    /// config. The initial screen is Clock; its `on_mount` hook is
    /// fired so any state it seeds from `cached_data` is ready
    /// before the first render.
    pub fn new(mut cached_data: SystemData, config: Config, now: Instant) -> Self {
        // Seed the SystemData config snapshot so screens can read
        // current config through `data.config.*` without any
        // per-screen plumbing. Model keeps this in sync on every
        // config mutation from here on.
        cached_data.config = config.clone();
        let mut screen = ActiveScreen::new(ScreenId::Clock);
        screen.mount(&cached_data);
        Self {
            cached_data,
            screen,
            nav_stack: NavStack::new(),
            display_state: DisplayState::Active,
            last_activity: now,
            boot: now,
            sleeping: false,
            needs_redraw: true, // first frame always draws
            config,
            config_dirty: false,
            buzz: None,
            mic_test: MicTestMode::Off,
            mic_resume_on_wake: None,
            motion_wake_grace: None,
            last_step_total: None,
            last_track_kick: None,
            track_failures: 0,
            track_pending: false,
        }
    }

    // --- accessors -----------------------------------------------------------

    /// Current render-needed flag. Set internally by event
    /// handlers that mutate visible state.
    pub fn needs_redraw(&self) -> bool {
        self.needs_redraw
    }

    /// Reset the redraw flag. Called by the manager after a
    /// successful render.
    pub fn clear_redraw(&mut self) {
        self.needs_redraw = false;
    }

    /// Whether the system is in the sleep state (display Off,
    /// subscriber tasks in low-power mode). The manager's tick
    /// loop reads this to decide whether to enter hardware
    /// light sleep.
    pub fn sleeping(&self) -> bool {
        self.sleeping
    }

    /// Read-only view of the cached system snapshot. Screens
    /// render against this, the manager's render path reads it
    /// to decide whether to draw the battery-warning frame.
    pub fn cached_data(&self) -> &SystemData {
        &self.cached_data
    }

    /// Mutable handle to the active screen. Only the render path
    /// and `handle_event` need this; expose `&mut` so the caller
    /// can call `render(...)` on the screen.
    pub fn screen_mut(&mut self) -> &mut ActiveScreen {
        &mut self.screen
    }

    /// Read-only view of runtime config. The manager passes
    /// `config().display` to display transitions.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Update the loop-iteration counter (diagnostics only).
    /// Owned by the manager's tick loop.
    pub fn set_tick_count(&mut self, count: u32) {
        self.cached_data.tick_count = count;
    }

    /// Fold one event into state and return effects for the
    /// caller to apply to hardware.
    pub fn handle_event(&mut self, event: &SystemEvent, now: Instant) -> Effects {
        let mut out: Effects = Vec::new();

        // 1. Snapshot events: update the cached fields.
        self.apply_snapshot(event, &mut out);

        // 2. Non-user wake sources: wake the device, then let the
        // event continue through to screen dispatch (except WoM,
        // which just wakes).
        if self.sleeping && events::is_wake_source(event) {
            self.wake(now, &mut out);
            // WoM and the manager's synthesized GPIO wake carry no
            // payload for a screen - they only wake. Nothing human
            // is confirmed behind them either, so the wake stays
            // provisional: without user activity inside the grace
            // window, `tick` re-enters sleep.
            if matches!(
                event,
                SystemEvent::WakeOnMotion | SystemEvent::WakeInterrupt
            ) {
                self.motion_wake_grace = Some(now + MOTION_WAKE_GRACE);
                return out;
            }
        }

        // 3. User activity: resets idle timer; wakes the device or
        // (if BOOT while awake) triggers a "sleep now" shortcut.
        if events::is_user_activity(event) {
            self.last_activity = now;
            self.motion_wake_grace = None;
            if self.sleeping {
                self.wake(now, &mut out);
                return out; // consume the event so accidental
                            // taps/swipes on wake don't dispatch
                            // to the screen.
            }
            if matches!(event, SystemEvent::BootButtonPressed) {
                let _ = out.push(Effect::MotorPulse { duration_ms: 100 });
                self.sleep(&mut out);
                return out;
            }
        }

        // 3b. Wrist lowered: the wrist left the viewing pose - the
        // user is done looking. Sleep now instead of waiting out
        // the idle timer. Never mid-alert: an alarm keeps ringing
        // until acknowledged.
        if matches!(event, SystemEvent::WristLowered) {
            if !self.sleeping && self.buzz.is_none() {
                self.sleep(&mut out);
            }
            return out;
        }

        // From here on we only dispatch to the screen when awake.
        if self.sleeping {
            return out;
        }

        // 4. System-level edge gestures open the two overlays, but
        // only when the active screen is not already an overlay.
        // When on an overlay, edge gestures reach the overlay's own
        // on_event so it can use them as a close affordance (e.g.
        // swipe-down-from-top inside the drawer means "close", not
        // "switch to Quick Access").
        //
        //   swipe-down-from-top    -> Quick Access
        //   swipe-up-from-bottom   -> App Drawer
        //   swipe-right-from-left  -> Notifications
        //
        // Each pushes the pre-overlay screen onto the nav stack so
        // `Action::Back` from an app launched via the overlay returns
        // to the original screen, not a hardcoded home.
        if !matches!(self.screen.id(),
            ScreenId::QuickAccess | ScreenId::AppDrawer | ScreenId::Notifications)
        {
            if let SystemEvent::Swipe { dir, region, .. } = event {
                match (dir, region) {
                    (SwipeDir::Down, SwipeRegion::Top) => {
                        let previous = self.screen.id();
                        self.nav_stack.push(previous);
                        self.screen.open_quick_access(previous, &self.cached_data);
                        self.needs_redraw = true;
                        return out;
                    }
                    (SwipeDir::Up, SwipeRegion::Bottom) => {
                        let previous = self.screen.id();
                        self.nav_stack.push(previous);
                        self.screen.open_app_drawer(previous, &self.cached_data);
                        self.needs_redraw = true;
                        return out;
                    }
                    (SwipeDir::Right, SwipeRegion::Left) => {
                        let previous = self.screen.id();
                        self.nav_stack.push(previous);
                        self.screen.open_notifications(&self.cached_data);
                        self.needs_redraw = true;
                        return out;
                    }
                    _ => {}
                }
            }
        }

        // 5. Forward to the active screen and dispatch its Action.
        let action = self.screen.on_event(event, &mut self.cached_data);
        self.dispatch_action(action, &mut out);

        // 6. Config flush: any action above may have dirtied the
        // config (e.g. `SetBrightness`). We deliberately defer the
        // flash write until the gesture ends so a drag scrub hits
        // flash once on release instead of once per pixel.
        if self.config_dirty && matches!(event, SystemEvent::TouchReleased) {
            let _ = out.push(Effect::SaveConfig);
            self.config_dirty = false;
        }

        out
    }

    /// Time-driven advance: buzz-pattern tick, dim/idle-sleep
    /// checks. Call once per loop iteration.
    ///
    /// `now` is `Instant::now()` (drives buzz/dim/idle timers, and
    /// the `active_secs` field which pauses with embassy time during
    /// light sleep). `wall_uptime_secs` is the wall-time-since-power-
    /// on value the bin reads from the SoC RTC counter - it survives
    /// light sleep and drives the `uptime_secs` field. Passing it in
    /// keeps `Model` hardware-free / host-testable (no `Rtc` handle).
    pub fn tick(&mut self, now: Instant, wall_uptime_secs: u32) -> Effects {
        let mut out: Effects = Vec::new();
        self.tick_buzz(now, &mut out);
        self.apply_dim_state(now, &mut out);
        self.check_idle_sleep(now, &mut out);
        self.check_motion_wake_grace(now, &mut out);
        // Update the two time-since-boot snapshots screens render.
        // Both are cheap (one duration_since + cast each); keeps the
        // values accurate to the current tick without screens needing
        // any hardware access of their own.
        self.cached_data.uptime_secs = wall_uptime_secs;
        self.cached_data.active_secs =
            now.duration_since(self.boot).as_secs() as u32;
        // Safety net for the mic-test diagnostic: stop capture if the
        // user left Settings by any path (back to clock, an overlay,
        // etc.) OR if the device is going to sleep. The sleep case is
        // critical: an active capture leaves the audio task waiting on
        // a DMA-completion future, whose interrupt keeps firing during
        // light sleep and starves the executor of real idle - the CPU
        // ends up unable to truly sleep and the wake path falls over.
        // Parking the audio task on `AUDIO_COMMAND.receive()` (no DMA
        // waker) lets light sleep work normally.
        if self.mic_test != MicTestMode::Off
            && (self.sleeping || self.screen.id() != ScreenId::Settings)
        {
            if let Some(stop) = self.mic_test.stop_command() {
                let _ = out.push(Effect::AudioCommand(stop));
            }
            // Stopped by sleep while still on Settings: pause, resume
            // on wake. Stopped by leaving the screen: a real exit. A
            // cancelled tone sweep resumes as the meter, not a replay
            // (the sweep is one-shot; its TonesDone will never come).
            self.mic_resume_on_wake =
                if self.sleeping && self.screen.id() == ScreenId::Settings {
                    Some(match self.mic_test {
                        MicTestMode::Tones => MicTestMode::Capture,
                        m => m,
                    })
                } else {
                    None
                };
            self.mic_test = MicTestMode::Off;
            self.cached_data.mic_level = 0;
        }
        out
    }

    /// Inject the manager's completed light-sleep cycle count into the
    /// cached snapshot. Kept off `tick` so that signature stays
    /// host-test-stable; the manager calls this right after `tick`
    /// each loop. The cycle rate vs uptime distinguishes "really
    /// sleeping" from "active_secs is wrong".
    pub fn set_sleep_telemetry(&mut self, sleep_cycles: u32) {
        self.cached_data.sleep_cycles = sleep_cycles;
    }

    // --- internals -----------------------------------------------------------

    /// Update cached snapshot fields from snapshot-carrying events.
    /// Also handles the TimerExpired / AlarmFired screen switches.
    fn apply_snapshot(&mut self, event: &SystemEvent, out: &mut Effects) {
        match event {
            SystemEvent::TimeUpdated { data } => {
                // Day rollover zeroes the daily step count. Covers
                // midnight and any time-set that lands on another
                // day (GPS sync, manual set).
                let day_rolled = data.day != self.cached_data.time.day;
                if day_rolled {
                    self.cached_data.steps_today = 0;
                }
                self.resync_stopwatch(data, day_rolled);
                self.cached_data.time = *data;
                // Re-evaluate the next-firing alarm against the
                // new time. Catches alarms whose fire-time the
                // clock just crossed.
                self.replan_alarms(out, false);
                self.maybe_kick_tracking(out);
            }
            SystemEvent::PowerUpdated { data } => {
                self.cached_data.power = *data;
            }
            SystemEvent::MotionUpdated { data } => {
                self.cached_data.motion = *data;
                if let Some(total) = data.steps {
                    // See `last_step_total` for the baseline /
                    // restart semantics. No redraw here: the clock
                    // face repaints on the 1 Hz TimeUpdated tick and
                    // picks the new value up within a second.
                    if let Some(prev) = self.last_step_total {
                        let delta = if total >= prev { total - prev } else { total };
                        self.cached_data.steps_today =
                            self.cached_data.steps_today.saturating_add(delta);
                    }
                    self.last_step_total = Some(total);
                }
            }
            SystemEvent::ImuIdentified { name } => {
                self.cached_data.imu_name = name;
            }
            SystemEvent::GpsFixUpdated { fix } => {
                // No redraw: the clock face picks the new value up
                // on the 1 Hz TimeUpdated tick, same as steps.
                self.cached_data.gps_fix = Some(*fix);
            }
            SystemEvent::WifiStatusUpdated { state } => {
                if self.cached_data.wifi != *state {
                    // Tactile end-of-session milestone, like the GPS
                    // sync: the screen may well be dark by the time a
                    // mid-use session completes. Manager gates the
                    // pulse on haptics_enabled.
                    if matches!(state, crate::data::WifiState::Synced { .. }) {
                        let _ = out.push(Effect::MotorPulse { duration_ms: 350 });
                    }
                    self.cached_data.wifi = *state;
                    self.needs_redraw = true;
                }
            }
            SystemEvent::WifiScanEntry { network } => {
                // Merge: a network already listed keeps its row and
                // updates its signal; a new one appends. Per-pass
                // strongest-first order and SSID dedup are the task's
                // job, so a fresh list arrives sorted and later passes
                // only grow it - rows never jump under the finger.
                // The list only resets on a fresh `Action::WifiScan`.
                // A full list drops newcomers (the task caps at the
                // same bound, so only a long merge run gets here).
                let list = &mut self.cached_data.wifi_scan;
                if let Some(known) = list.iter_mut().find(|n| n.ssid == network.ssid) {
                    known.rssi = network.rssi;
                    known.secured = network.secured;
                    self.needs_redraw = true;
                } else if list.push(network.clone()).is_ok() {
                    self.needs_redraw = true;
                }
            }
            SystemEvent::MicLevel { level } => {
                // Drop stale MicLevel events that arrive after capture
                // has already been stopped. There's an inherent race
                // on the stop path: the safety net pushes
                // StopCapture + clears mic_level=0 inside `tick`, but
                // the audio task may have queued one or two MicLevel
                // events into EVENTS before the StopCapture signal
                // reaches it. Without this gate those late events
                // would overwrite the just-cleared mic_level and the
                // mic-test bar would freeze on whatever capture last
                // reported (~90% in the bug we hit).
                // Also only repaint on actual change, so the meter
                // doesn't drive a redraw on every chunk in silence.
                if matches!(self.mic_test, MicTestMode::Capture | MicTestMode::Loopback)
                    && self.cached_data.mic_level != *level
                {
                    self.cached_data.mic_level = *level;
                    self.needs_redraw = true;
                }
            }
            SystemEvent::TonesDone => {
                // Sweep finished naturally. Clear the mode here; the
                // event then dispatches to the settings screen, which
                // (if still on the MicTest view) answers with
                // StartMicTest / StartLoopbackTest to bring the meter
                // back.
                if self.mic_test == MicTestMode::Tones {
                    self.mic_test = MicTestMode::Off;
                }
            }
            SystemEvent::TimerExpired { time } => {
                self.cached_data.time = *time;
                self.cached_data.timer = TimerState::Idle { duration: Duration::from_ticks(0) };
                self.push_notification(
                    NotificationSeverity::Warning,
                    NotificationSource::Timer,
                    "EXPIRED",
                );
                self.start_attention_buzz(out);
                self.surface_notifications();
                self.needs_redraw = true;
            }
            SystemEvent::AlarmFired { time } => {
                self.cached_data.time = *time;
                let subtitle = self
                    .cached_data
                    .config
                    .alarms
                    .active_hw
                    .map(|idx| {
                        let e = &self.cached_data.config.alarms.entries[idx];
                        let mut s: heapless::String<32> = heapless::String::new();
                        let _ = s.push_str(crate::ui::fmt::hm(e.hour, e.minute).as_str());
                        s
                    })
                    .unwrap_or_default();
                self.push_notification_owned(
                    NotificationSeverity::Critical,
                    NotificationSource::Alarm,
                    subtitle,
                );
                self.cached_data.config.alarms.alerting = true;
                self.start_attention_buzz(out);
                self.surface_notifications();
                self.needs_redraw = true;
            }
            SystemEvent::SelfTestUpdated { id, result } => {
                let idx = *id as usize;
                if idx < NUM_SELF_TESTS {
                    self.cached_data.self_tests[idx] = *result;
                }
                self.needs_redraw = true;
            }
            SystemEvent::StorageUsageUpdated { usage } => {
                self.cached_data.storage = *usage;
                self.needs_redraw = true;
            }
            SystemEvent::GpsSyncUpdated { state } => {
                // The task has spoken for the kicked session (any
                // event counts - even an unchanged one proves it is
                // alive and reporting).
                self.track_pending = false;
                // Cache + repaint only on change: the task re-emits
                // Syncing on a fixed cadence even when nothing moved,
                // and a dark sleeping display doesn't need frames at
                // all (needs_redraw is harmless then - render is
                // display-state gated).
                if self.cached_data.gps_sync != *state {
                    use crate::data::GpsSyncState;
                    let tracking = self.config.gps.tracking_enabled;
                    // Tactile milestones for sessions run at arm's
                    // length outdoors (screen dark or unreadable):
                    // short pulse when the first position fix lands,
                    // longer one when the session ends time-synced.
                    // The manager gates MotorPulse on
                    // haptics_enabled like every other buzz.
                    // Suppressed while tracking - a buzz per session
                    // at 15-60 s cadence would be a nuisance, and
                    // tracking sessions are nobody's milestone.
                    let had_fix = matches!(
                        self.cached_data.gps_sync,
                        GpsSyncState::Syncing { fix_ok: true, .. },
                    );
                    match state {
                        GpsSyncState::Syncing { fix_ok: true, .. }
                            if !had_fix && !tracking =>
                        {
                            let _ = out.push(Effect::MotorPulse { duration_ms: 120 });
                        }
                        GpsSyncState::Synced { .. } if !tracking => {
                            let _ = out.push(Effect::MotorPulse { duration_ms: 350 });
                        }
                        _ => {}
                    }
                    // Tracking auto-off: consecutive fixless
                    // sessions mean indoors, where each session
                    // burns the full budget in acquisition. The
                    // cadence preference survives; only the enable
                    // flips.
                    if tracking {
                        match state {
                            GpsSyncState::NoSignal => {
                                self.track_failures += 1;
                                if self.track_failures >= TRACK_AUTO_OFF_FAILURES {
                                    self.config.gps.tracking_enabled = false;
                                    self.cached_data.config = self.config.clone();
                                    self.config_dirty = true;
                                    self.track_failures = 0;
                                }
                            }
                            GpsSyncState::Syncing { fix_ok: true, .. }
                            | GpsSyncState::Synced { .. } => {
                                self.track_failures = 0;
                            }
                            _ => {}
                        }
                    }
                    self.cached_data.gps_sync = *state;
                    self.needs_redraw = true;
                }
            }
            SystemEvent::TouchPressed { x, y } => {
                self.cached_data.touch = TouchData { x: Some(*x), y: Some(*y) };
            }
            SystemEvent::TouchReleased => {
                self.cached_data.touch = TouchData::default();
            }
            SystemEvent::BatteryChanged { percent } => {
                self.cached_data.battery_history.push(
                    crate::data::BatterySample {
                        time: self.cached_data.time,
                        percent: *percent,
                    },
                );
                self.needs_redraw = true;
            }
            _ => {}
        }
    }

    /// Dispatch a screen-returned `Action` into state mutations
    /// and effects.
    fn dispatch_action(&mut self, action: Action, out: &mut Effects) {
        match action {
            Action::None => {}
            Action::Redraw => self.needs_redraw = true,
            Action::SwitchScreen(id) => {
                // Modal replace-top: when leaving an overlay the
                // pre-overlay screen is already on the nav stack.
                let current_is_overlay = matches!(
                    self.screen.id(),
                    ScreenId::QuickAccess | ScreenId::AppDrawer | ScreenId::Notifications,
                );
                if !current_is_overlay {
                    self.nav_stack.push(self.screen.id());
                }
                // Overlay targets route through their dedicated
                // constructors so the overlay gets the right
                // `previous` context. `switch_to` would call
                // `ActiveScreen::new(overlay_id)`, which panics on
                // purpose (overlays can't be built without a
                // previous).
                match id {
                    ScreenId::QuickAccess => {
                        let prev = if current_is_overlay {
                            self.nav_stack.peek_or_home()
                        } else {
                            // We just pushed `self.screen.id()` above,
                            // so that's the pre-overlay screen.
                            self.nav_stack.peek_or_home()
                        };
                        self.screen.open_quick_access(prev, &self.cached_data);
                    }
                    ScreenId::AppDrawer => {
                        let prev = self.nav_stack.peek_or_home();
                        self.screen.open_app_drawer(prev, &self.cached_data);
                    }
                    _ => {
                        self.screen.switch_to(id, &self.cached_data);
                    }
                }
                self.needs_redraw = true;
            }
            Action::Back => {
                let target = self.nav_stack.pop_or_home();
                self.screen.switch_to(target, &self.cached_data);
                self.needs_redraw = true;
            }
            Action::Shutdown => {
                let _ = out.push(Effect::Shutdown);
            }
            Action::RunSelfTest(id) => {
                let _ = out.push(Effect::ImuCommand(ImuCommand::RunSelfTest(id)));
                self.needs_redraw = true;
            }
            Action::StartTimer { seconds } => {
                let _ = out.push(Effect::RtcCommand(RtcCommand::StartTimer { seconds }));
                self.needs_redraw = true;
            }
            Action::CancelTimer => {
                let _ = out.push(Effect::RtcCommand(RtcCommand::CancelTimer));
                self.needs_redraw = true;
            }
            Action::SetAlarm { hour, minute, weekday } => {
                let _ = out.push(Effect::RtcCommand(RtcCommand::SetAlarm { hour, minute, weekday }));
                self.needs_redraw = true;
            }
            Action::CancelAlarm => {
                let _ = out.push(Effect::RtcCommand(RtcCommand::CancelAlarm));
                self.needs_redraw = true;
            }
            Action::SetTime { year, month, day, hour, minute, second } => {
                let _ = out.push(Effect::RtcCommand(RtcCommand::SetTime {
                    year, month, day, hour, minute, second,
                }));
                self.needs_redraw = true;
            }
            Action::StartBuzz { on_ms, off_ms } => {
                self.buzz = Some(BuzzPattern::start(
                    on_ms as u64,
                    off_ms as u64,
                    self.last_activity, // any Instant; tick() will
                                        // re-anchor on the first
                                        // call.
                ));
                let _ = out.push(Effect::MotorOn);
            }
            Action::StopBuzz => {
                self.buzz = None;
                let _ = out.push(Effect::MotorOff);
                let _ = out.push(Effect::AudioCommand(AudioCommand::StopAlarm));
                self.needs_redraw = true;
            }
            Action::DismissAlarm => {
                self.buzz = None;
                let _ = out.push(Effect::MotorOff);
                let _ = out.push(Effect::AudioCommand(AudioCommand::StopAlarm));
                self.cached_data.config.alarms.alerting = false;
                self.cached_data.config.alarms.snoozed = false;
                self.needs_redraw = true;
            }
            Action::SnoozeAlarm => {
                self.buzz = None;
                let _ = out.push(Effect::MotorOff);
                let _ = out.push(Effect::AudioCommand(AudioCommand::StopAlarm));
                self.cached_data.config.alarms.alerting = false;
                self.cached_data.config.alarms.snoozed = true;
                let t = &self.cached_data.time;
                let (hour, minute) = AlarmState::compute_snooze(t.hour, t.minute, 10);
                let _ = out.push(Effect::RtcCommand(RtcCommand::SetAlarm {
                    hour, minute, weekday: None,
                }));
                // Leave a visible breadcrumb in the overlay so the
                // user can see the snoozed alarm is still queued
                // and what time it'll fire at.
                let mut subtitle: heapless::String<32> = heapless::String::new();
                let _ = core::fmt::Write::write_fmt(
                    &mut subtitle,
                    format_args!("SNOOZED -> {}", crate::ui::fmt::hm(hour, minute).as_str()),
                );
                self.push_notification_owned(
                    NotificationSeverity::Info,
                    NotificationSource::Alarm,
                    subtitle,
                );
                self.needs_redraw = true;
            }
            Action::FactoryReset => {
                let _ = out.push(Effect::FactoryReset);
                self.needs_redraw = true;
            }
            Action::InitSd => {
                let _ = out.push(Effect::ProbeSd);
                self.needs_redraw = true;
            }
            Action::RestoreFromSd => {
                let _ = out.push(Effect::RestoreFromSd);
                self.needs_redraw = true;
            }
            Action::PersistAlarms => {
                // Screens edit the cached tree directly; adopt their
                // alarm edits into the authoritative config before
                // the save reads it.
                self.config.alarms = self.cached_data.config.alarms;
                let _ = out.push(Effect::SaveConfig);
                // Force-replan: editing the active entry's HH:MM
                // doesn't move `active_hw`, so a non-forced replan
                // would skip the SetAlarm and leave the chip stuck
                // at the old time.
                self.replan_alarms(out, true);
                self.needs_redraw = true;
            }
            Action::PersistConfig => {
                let _ = out.push(Effect::SaveConfig);
                self.needs_redraw = true;
            }
            Action::SetBrightness { percent } => {
                // Apply to hardware + in-memory config immediately,
                // mark config dirty. The save is deferred to the
                // next `TouchReleased` so a drag scrub doesn't
                // hammer flash.
                self.apply_brightness(percent, out);
                self.config_dirty = true;
                self.needs_redraw = true;
            }
            Action::ToggleNightMode => {
                self.config.display.night_mode = !self.config.display.night_mode;
                // Re-apply the current brightness through the shared
                // path so the new `max_brightness_pct` clamps the
                // value (turn-on caps down, turn-off is a no-op).
                let current_pct =
                    (self.config.display.brightness_active as u16 * 100 / 255) as u8;
                self.apply_brightness(current_pct, out);
                self.config_dirty = true;
                self.needs_redraw = true;
            }
            Action::ToggleAlwaysOn => {
                self.config.display.always_on = !self.config.display.always_on;
                self.cached_data.config = self.config.clone();
                self.config_dirty = true;
                self.needs_redraw = true;
            }
            Action::ToggleHaptics => {
                self.config.alerts.haptics_enabled = !self.config.alerts.haptics_enabled;
                self.cached_data.config = self.config.clone();
                self.config_dirty = true;
                self.needs_redraw = true;
            }
            Action::ToggleSound => {
                self.config.alerts.sound_enabled = !self.config.alerts.sound_enabled;
                self.cached_data.config = self.config.clone();
                self.config_dirty = true;
                self.needs_redraw = true;
            }
            Action::StartMicTest => {
                self.mic_test = MicTestMode::Capture;
                let _ = out.push(Effect::AudioCommand(AudioCommand::StartCapture));
                // Without this the SettingsScreen's view-flip to
                // MicTest (set in `row_hit` before we got here) never
                // hits the display: index_event overrides the
                // row's normal `Action::Redraw` with this StartMicTest
                // so the model can fire StartCapture, which means we
                // lose the redraw signal unless we re-assert it here.
                self.needs_redraw = true;
            }
            Action::StopMicTest => {
                if let Some(stop) = self.mic_test.stop_command() {
                    let _ = out.push(Effect::AudioCommand(stop));
                }
                self.mic_test = MicTestMode::Off;
                self.cached_data.mic_level = 0;
                // Same reasoning as StartMicTest above - mic_test_event
                // returns StopMicTest after flipping view back to
                // Index, but the screen doesn't repaint without this.
                self.needs_redraw = true;
            }
            Action::PlayToneTest => {
                // The running capture/loopback session hands the I2S
                // to the sweep on its own (interrupt handoff); no stop
                // command needed first.
                self.mic_test = MicTestMode::Tones;
                let _ = out.push(Effect::AudioCommand(AudioCommand::PlayTones));
                self.needs_redraw = true;
            }
            Action::StartLoopbackTest => {
                self.mic_test = MicTestMode::Loopback;
                let _ = out.push(Effect::AudioCommand(AudioCommand::StartLoopback));
                self.needs_redraw = true;
            }
            Action::ToggleDnd => {
                self.config.alerts.dnd = !self.config.alerts.dnd;
                self.cached_data.config = self.config.clone();
                self.config_dirty = true;
                self.needs_redraw = true;
            }
            Action::Sleep => {
                // Close any active overlay so the next wake lands on
                // the underlying app, not the still-open QA.
                if matches!(
                    self.screen.id(),
                    ScreenId::QuickAccess | ScreenId::AppDrawer,
                ) {
                    let target = self.nav_stack.pop_or_home();
                    self.screen.switch_to(target, &self.cached_data);
                }
                let _ = out.push(Effect::MotorPulse { duration_ms: 100 });
                self.sleep(out);
            }
            Action::SetAutoLock { secs } => {
                self.config.display.off_timeout_s = secs as u64;
                // Dim fires ~2/3 of the way into the idle window, so
                // the dim stage scales with the auto-lock setting
                // rather than sitting at a fixed offset. Floored at
                // 5s so the dim isn't instantaneous on a short
                // auto-lock.
                self.config.display.dim_timeout_s =
                    ((secs as u64 * 2 / 3)).max(5);
                self.cached_data.config = self.config.clone();
                self.config_dirty = true;
                self.needs_redraw = true;
            }
            Action::GpsSync => {
                let _ = out.push(Effect::GpsCommand(GpsCommand::SyncOnce {
                    tz_offset_minutes: self.config.time.tz_offset_minutes,
                }));
                // No optimistic status write: `gps_sync` is the
                // task's reported state, and the task publishes
                // Syncing as its first act of the session -
                // milliseconds behind the tap.
            }
            Action::AdjustTimezone { delta_min } => {
                self.config.time.tz_offset_minutes = (self.config.time.tz_offset_minutes
                    + delta_min)
                    .clamp(Config::TZ_OFFSET_MIN, Config::TZ_OFFSET_MAX);
                self.cached_data.config = self.config.clone();
                self.config_dirty = true;
                self.needs_redraw = true;
            }
            Action::ToggleGpsTracking => {
                self.config.gps.tracking_enabled = !self.config.gps.tracking_enabled;
                self.cached_data.config = self.config.clone();
                self.config_dirty = true;
                self.track_failures = 0;
                self.last_track_kick = None;
                if self.config.gps.tracking_enabled {
                    // First session right away - the toggle should
                    // answer with visible activity, not after one
                    // full interval.
                    self.kick_track_session(out);
                } else {
                    // Abort whatever may be running or queued. A
                    // fast on-off can overwrite the still-queued
                    // kick (single-slot signal) - the task then
                    // consumes the Abort as a no-op and no state is
                    // left dangling, because the status cache only
                    // ever holds what the task actually reported.
                    if self.track_pending
                        || matches!(
                            self.cached_data.gps_sync,
                            crate::data::GpsSyncState::Syncing { .. },
                        )
                    {
                        let _ = out.push(Effect::GpsCommand(GpsCommand::Abort));
                    }
                    self.track_pending = false;
                    self.cached_data.gps_next_session_secs = None;
                }
                self.needs_redraw = true;
            }
            Action::SetGpsCadence { cadence } => {
                if self.config.gps.tracking_cadence != cadence {
                    self.config.gps.tracking_cadence = cadence;
                    self.cached_data.config = self.config.clone();
                    self.config_dirty = true;
                    self.needs_redraw = true;
                }
            }
            Action::WifiScan | Action::WifiRescan => {
                // One session at a time: the task serializes on its
                // signal anyway, but a second kick would replace the
                // pending command and confuse the status the UI shows.
                if self.cached_data.wifi.is_busy() {
                    return;
                }
                // Fresh scan = empty list (the view shows SCANNING
                // until the first entry); a refresh merges into it.
                if matches!(action, Action::WifiScan) {
                    self.cached_data.wifi_scan.clear();
                    self.needs_redraw = true;
                }
                let _ = out.push(Effect::WifiCommand(WifiCommand::Scan));
                // No optimistic status write - the task publishes
                // Scanning as its first act (the GPS precedent).
            }
            Action::SetWifiCredentials { ssid, passphrase } => {
                // Store first, verify by joining. A wrong passphrase
                // is stored too - the status line says AUTH FAILED
                // and the user re-enters; with a single stored
                // network there is nothing else to protect.
                self.config.wifi.ssid = ssid;
                self.config.wifi.passphrase = passphrase;
                self.cached_data.config = self.config.clone();
                self.config_dirty = true;
                self.needs_redraw = true;
                self.kick_wifi_sync(out);
            }
            Action::WifiConnect => {
                self.kick_wifi_sync(out);
            }
            Action::WifiForget => {
                self.config.wifi = crate::config::WifiConfig::DEFAULT;
                self.cached_data.config = self.config.clone();
                self.config_dirty = true;
                self.needs_redraw = true;
            }
        }
    }

    /// Emit one tracking-session kick and stamp the schedule.
    /// `gps_sync` is deliberately NOT touched - it holds only what
    /// the task reports; `track_pending` bridges the gap until the
    /// task's first event lands.
    fn kick_track_session(&mut self, out: &mut Effects) {
        let continuous = self.config.gps.tracking_cadence
            == crate::config::GpsTrackingCadence::Continuous;
        let _ = out.push(Effect::GpsCommand(GpsCommand::TrackOnce {
            tz_offset_minutes: self.config.time.tz_offset_minutes,
            budget_secs: if continuous {
                TRACK_BUDGET_CONTINUOUS_SECS
            } else {
                TRACK_BUDGET_INTERVAL_SECS
            },
        }));
        self.track_pending = true;
        self.last_track_kick = Some(self.cached_data.uptime_secs);
    }

    /// Run one WiFi sync session with the stored credentials. Refused
    /// (silently - the button is drawn disabled for both cases) when
    /// no network is stored or a sync is already running. A scan
    /// pass in flight is NOT a reason to refuse: the command signal
    /// holds the request and the task runs it right after the pass -
    /// the keyboard's DONE must join even while the list is still
    /// auto-refreshing behind it.
    fn kick_wifi_sync(&mut self, out: &mut Effects) {
        if !self.config.wifi.is_set()
            || matches!(self.cached_data.wifi, crate::data::WifiState::Connecting)
        {
            return;
        }
        let _ = out.push(Effect::WifiCommand(WifiCommand::SyncOnce {
            ssid: self.config.wifi.ssid.clone(),
            passphrase: self.config.wifi.passphrase.clone(),
            tz_offset_minutes: self.config.time.tz_offset_minutes,
        }));
    }

    /// Kick the next tracking session when one is due. Runs on every
    /// `TimeUpdated` (1 Hz awake, heartbeat cadence asleep - so
    /// intervals quantize to ~5 s while sleeping). Uptime-based:
    /// embassy time pauses during light sleep, the RTC slow clock
    /// doesn't.
    fn maybe_kick_tracking(&mut self, out: &mut Effects) {
        if !self.config.gps.tracking_enabled || !self.cached_data.capabilities.gps {
            self.cached_data.gps_next_session_secs = None;
            return;
        }
        // A session is running (task-reported) or kicked-but-not-
        // yet-reported - either way, don't stack another.
        if self.track_pending
            || matches!(
                self.cached_data.gps_sync,
                crate::data::GpsSyncState::Syncing { .. },
            )
        {
            self.cached_data.gps_next_session_secs = None;
            return;
        }
        let remaining = match self.last_track_kick {
            None => 0,
            Some(at) => self
                .config
                .gps
                .tracking_cadence
                .interval_secs()
                .saturating_sub(self.cached_data.uptime_secs.wrapping_sub(at)),
        };
        if remaining == 0 {
            self.kick_track_session(out);
            self.cached_data.gps_next_session_secs = None;
        } else {
            self.cached_data.gps_next_session_secs = Some(remaining);
        }
    }

    /// Push a notification with a static-string subtitle. Convenience
    /// for sources that don't need to format anything (e.g. timer
    /// expired -> "EXPIRED"). Snapshots the current wall-clock for
    /// the timestamp.
    fn push_notification(
        &mut self,
        severity: NotificationSeverity,
        source: NotificationSource,
        subtitle: &str,
    ) {
        let mut s: heapless::String<32> = heapless::String::new();
        let _ = s.push_str(subtitle);
        self.push_notification_owned(severity, source, s);
    }

    /// Push a notification with a caller-built subtitle string.
    /// Used by sources whose subtitle has dynamic context that's
    /// already been formatted (e.g. alarm fired -> "ALARM: 06:30").
    fn push_notification_owned(
        &mut self,
        severity: NotificationSeverity,
        source: NotificationSource,
        subtitle: heapless::String<32>,
    ) {
        let t = &self.cached_data.time;
        self.cached_data.notifications.push(Notification {
            severity,
            source,
            subtitle,
            ts_hour: t.hour,
            ts_minute: t.minute,
        });
    }

    /// Start the standard "demand attention" buzz pattern fired by
    /// alarms and timer expiry. Sourced by both
    /// `apply_snapshot::AlarmFired` and `TimerExpired`. Stopped by
    /// any of `Action::DismissAlarm` / `SnoozeAlarm` / `StopBuzz`,
    /// which are emitted by the notification overlay's row gestures.
    fn start_attention_buzz(&mut self, out: &mut Effects) {
        self.buzz = Some(BuzzPattern::start(
            200, 100, self.last_activity,
        ));
        let _ = out.push(Effect::MotorOn);
        // Audible alert runs in parallel with the buzz: the haptic is
        // a one-shot edge cycled by `tick_buzz`, the tone is a single
        // "play until stopped" command owned by the audio task. The
        // manager gates this on `sound_enabled`; the buzz gates on
        // `haptics_enabled`, so the two alert independently.
        let _ = out.push(Effect::AudioCommand(AudioCommand::PlayAlarm));
    }

    /// Auto-open the Notifications overlay so the just-pushed
    /// notification is the first thing the user sees on wake.
    /// No-op when already on Notifications.
    fn surface_notifications(&mut self) {
        if matches!(self.screen.id(), ScreenId::Notifications) {
            return;
        }
        let previous = self.screen.id();
        self.nav_stack.push(previous);
        self.screen.open_notifications(&self.cached_data);
    }

    /// Re-derive a running stopwatch's embassy view from its RTC
    /// anchor. The embassy clock freezes across light sleep, so
    /// `elapsed()` would silently exclude every slept second;
    /// rewriting `start` against the segment's FIXED `anchor_secs`
    /// on each wall-clock tick makes the first tick after any sleep
    /// fold the whole gap in - before any screen renders - and,
    /// because the anchor never moves, repeated corrections cannot
    /// accumulate rounding error. Runs before `cached_data.time` is
    /// overwritten (`day_rolled` comes from the old value).
    fn resync_stopwatch(&mut self, now_time: &crate::data::TimeData, day_rolled: bool) {
        let StopwatchState::Running { start, accumulated, anchor_secs } =
            self.cached_data.stopwatch
        else {
            return;
        };
        let now = Instant::now();
        let now_secs = now_time.secs_of_day();
        self.cached_data.stopwatch = if day_rolled {
            // Fold the segment across midnight (anchor..24:00 plus
            // 00:00..now, RTC-exact) and re-anchor. Handles multi-day
            // runs one midnight at a time.
            StopwatchState::Running {
                start: now,
                accumulated: accumulated
                    + Duration::from_secs((86_400 - anchor_secs + now_secs) as u64),
                anchor_secs: now_secs,
            }
        } else if now_secs >= anchor_secs {
            let run = Duration::from_secs((now_secs - anchor_secs) as u64);
            match now.checked_sub(run) {
                // Normal path: same accumulated/anchor, embassy view
                // re-derived so elapsed() agrees with the RTC.
                Some(start) => StopwatchState::Running { start, accumulated, anchor_secs },
                // Segment is longer than the embassy clock has been
                // alive (long sleeps early after boot): fold the
                // RTC-exact segment instead of anchoring before zero.
                None => StopwatchState::Running {
                    start: now,
                    accumulated: accumulated + run,
                    anchor_secs: now_secs,
                },
            }
        } else {
            // Clock moved backwards without a day change (manual
            // set / GPS sync): the RTC delta is meaningless, so keep
            // the embassy-measured elapsed and re-anchor.
            StopwatchState::Running {
                start: now,
                accumulated: accumulated + now.duration_since(start),
                anchor_secs: now_secs,
            }
        };
    }

    /// Re-evaluate which enabled alarm fires next given the
    /// cached time and emit the matching RTC command. The `force`
    /// flag controls whether to emit a command even when the
    /// next-alarm *index* is unchanged - needed on the persist
    /// path because editing the active entry's HH:MM doesn't move
    /// `active_hw`, but the chip still needs reprogramming.
    fn replan_alarms(&mut self, out: &mut Effects, force: bool) {
        let t = &self.cached_data.time;
        let weekday = crate::ui::screens::alarm::day_of_week(
            t.year as i32, t.month as i32, t.day as i32,
        );
        let plan = if force {
            self.cached_data.config.alarms.plan_reprogram_force(t.hour, t.minute, weekday)
        } else {
            self.cached_data.config.alarms.plan_reprogram(t.hour, t.minute, weekday)
        };
        match plan {
            None => {}
            Some(AlarmReprogram::SetAlarm { hour, minute }) => {
                let _ = out.push(Effect::RtcCommand(RtcCommand::SetAlarm {
                    hour, minute, weekday: None,
                }));
            }
            Some(AlarmReprogram::CancelAlarm) => {
                let _ = out.push(Effect::RtcCommand(RtcCommand::CancelAlarm));
            }
        }
    }

    /// Shared "apply a new brightness" path used by both the
    /// preview and commit brightness actions. Clamps the slider
    /// percent (5..=100), maps to the panel's 0..=255 register,
    /// updates the live `Config` and its `SystemData` snapshot so
    /// screens see the new value on the next render, and queues
    /// the `SetDisplayBrightness` effect so firmware applies it
    /// to the panel immediately. Does NOT emit `SaveConfig` - the
    /// caller decides whether this change is a preview (no save)
    /// or a commit (`SaveConfig` emitted alongside).
    fn apply_brightness(&mut self, percent: u8, out: &mut Effects) {
        // Slider range depends on night_mode (5..30 on, 5..100 off),
        // so the clamp here honours the current mode. The stored
        // `brightness_active` is always the real effective value -
        // no separate "user intent vs hardware" split.
        let max_pct = self.config.display.max_brightness_pct();
        let pct = percent.clamp(5, max_pct);
        let hw = (pct as u16 * 255 / 100) as u8;
        self.config.display.brightness_active = hw;
        self.cached_data.config = self.config.clone();
        let _ = out.push(Effect::SetDisplayBrightness(hw));
    }

    /// Enter low-power sleep. Idempotent. Queues the display-Off
    /// transition + SLEEP_WATCH broadcast; the manager then
    /// enters hardware light sleep on the next tick loop when it
    /// sees `sleeping = true`.
    fn sleep(&mut self, out: &mut Effects) {
        if self.sleeping {
            return;
        }
        self.sleeping = true;
        // Silence any in-flight attention alert (motor AND tone) on
        // the way to sleep. With `check_idle_sleep` holding sleep off
        // during an alert, reaching here mid-alert means an explicit
        // user action (the BOOT shortcut) - treat it as "shut up and
        // sleep". The tone matters as much as the motor: light sleep
        // freezes the I2S stream mid-session, and every heartbeat
        // wake would leak a beep fragment until the next command.
        if self.buzz.is_some() {
            self.buzz = None;
            let _ = out.push(Effect::MotorOff);
            let _ = out.push(Effect::AudioCommand(AudioCommand::StopAlarm));
        }
        let _ = out.push(Effect::BroadcastSleep(SleepState::Sleeping));
        let _ = out.push(Effect::TransitionDisplay {
            from: self.display_state,
            to: DisplayState::Off,
        });
        self.display_state = DisplayState::Off;
    }

    /// Exit low-power sleep. Idempotent.
    fn wake(&mut self, now: Instant, out: &mut Effects) {
        if !self.sleeping {
            return;
        }
        self.sleeping = false;
        self.last_activity = now;
        // An active alert (e.g. the alarm that caused this wake) owns
        // the speaker: any session start would interrupt the alarm
        // session, so the mic test must not auto-resume over it. Drop
        // the resume entirely; re-opening the view is one tap.
        if let Some(mode) = self.mic_resume_on_wake.take().filter(|_| self.buzz.is_none()) {
            let cmd = match mode {
                MicTestMode::Loopback => AudioCommand::StartLoopback,
                // Capture, or the Tones->Capture mapping the safety
                // net already applied. (If the paused mode was the
                // sweep while the LOOP toggle was on, the view's
                // toggle state may briefly disagree with the resumed
                // meter mode - it self-heals on the next LOOP tap.)
                _ => AudioCommand::StartCapture,
            };
            self.mic_test = match mode {
                MicTestMode::Loopback => MicTestMode::Loopback,
                _ => MicTestMode::Capture,
            };
            let _ = out.push(Effect::AudioCommand(cmd));
        }
        let _ = out.push(Effect::BroadcastSleep(SleepState::Awake));
        let _ = out.push(Effect::TransitionDisplay {
            from: self.display_state,
            to: DisplayState::Active,
        });
        self.display_state = DisplayState::Active;
        self.needs_redraw = true;
    }

    /// Advance the buzz pattern. Emits [`Effect::MotorOn`] /
    /// [`Effect::MotorOff`] when the phase flips.
    fn tick_buzz(&mut self, now: Instant, out: &mut Effects) {
        let Some(pattern) = self.buzz.as_mut() else {
            return;
        };
        match pattern.tick(now) {
            BuzzAction::None => {}
            BuzzAction::TurnOn => { let _ = out.push(Effect::MotorOn); }
            BuzzAction::TurnOff => { let _ = out.push(Effect::MotorOff); }
        }
    }

    /// Apply the Active <-> Dim transition when awake. No-op when
    /// sleeping (display is Off and [`Self::sleep`] / [`Self::wake`]
    /// handle that), and no-op when `config.display.always_on` is
    /// true (the user opted out of idle-dim).
    fn apply_dim_state(&mut self, now: Instant, out: &mut Effects) {
        if self.sleeping {
            return;
        }
        if self.config.display.always_on {
            // Force Active and skip the timer.
            if self.display_state != DisplayState::Active {
                let _ = out.push(Effect::TransitionDisplay {
                    from: self.display_state,
                    to: DisplayState::Active,
                });
                self.display_state = DisplayState::Active;
            }
            return;
        }
        let idle = now.duration_since(self.last_activity);
        let target = if idle >= Duration::from_secs(self.config.display.dim_timeout_s) {
            DisplayState::Dim
        } else {
            DisplayState::Active
        };
        if target != self.display_state {
            let _ = out.push(Effect::TransitionDisplay {
                from: self.display_state,
                to: target,
            });
            self.display_state = target;
        }
    }

    /// Trigger sleep if the idle timer has expired. No-op if
    /// already sleeping or if `config.display.always_on` is set.
    fn check_idle_sleep(&mut self, now: Instant, out: &mut Effects) {
        if self.sleeping || self.config.display.always_on {
            return;
        }
        // Never idle-sleep mid-alert. Light sleep gates the I2S
        // clocks, so the alert tone freezes and only a short beep
        // fragment leaks out on each ~5 s heartbeat wake - the user
        // hears a broken, cyclic chirp instead of the alarm. Hold the
        // device awake until they react (dismiss / snooze / stop all
        // clear `buzz`). Trade-off: a never-acknowledged alarm keeps
        // the device awake indefinitely; an alert auto-timeout is the
        // eventual fix for that.
        if self.buzz.is_some() {
            return;
        }
        let idle = now.duration_since(self.last_activity);
        if idle >= Duration::from_secs(self.config.display.off_timeout_s) {
            self.sleep(out);
        }
    }

    /// Enforce the provisional-wake grace window: a motion /
    /// bare-GPIO wake whose deadline passes without any confirming
    /// user activity goes straight back to sleep. Mid-alert the
    /// window just dissolves - the alert owns the display, and the
    /// idle timer takes over once the alert clears.
    fn check_motion_wake_grace(&mut self, now: Instant, out: &mut Effects) {
        let Some(deadline) = self.motion_wake_grace else {
            return;
        };
        if self.sleeping {
            self.motion_wake_grace = None;
            return;
        }
        if now < deadline {
            return;
        }
        self.motion_wake_grace = None;
        if self.buzz.is_none() {
            self.sleep(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Model {
        Model::new(
            SystemData::default(),
            Config::default(),
            Instant::from_millis(0),
        )
    }

    /// A MotionUpdated event carrying only a step-counter total.
    fn step_event(total: u32) -> SystemEvent {
        SystemEvent::MotionUpdated {
            data: crate::data::MotionData {
                steps: Some(total),
                ..Default::default()
            },
        }
    }

    #[test]
    fn steps_accumulate_from_hub_total_deltas() {
        let mut m = fresh();
        // First total is the baseline - nothing credited yet.
        m.handle_event(&step_event(120), Instant::from_millis(0));
        assert_eq!(m.cached_data().steps_today, 0);
        // Increases count as their delta.
        m.handle_event(&step_event(150), Instant::from_millis(1_000));
        assert_eq!(m.cached_data().steps_today, 30);
        // A total below the previous one means the hub restarted at
        // zero - the new total counts in full on top of the bank.
        m.handle_event(&step_event(7), Instant::from_millis(2_000));
        assert_eq!(m.cached_data().steps_today, 37);
    }

    #[test]
    fn steps_reset_on_day_rollover_but_keep_hub_baseline() {
        let mut m = fresh();
        m.handle_event(&step_event(0), Instant::from_millis(0));
        m.handle_event(&step_event(500), Instant::from_millis(1_000));
        assert_eq!(m.cached_data().steps_today, 500);
        // Midnight: a TimeUpdated on a new day zeroes the count...
        let mut t = m.cached_data().time;
        t.day = t.day % 28 + 1;
        m.handle_event(&SystemEvent::TimeUpdated { data: t }, Instant::from_millis(2_000));
        assert_eq!(m.cached_data().steps_today, 0);
        // ...without disturbing the hub baseline: the next total
        // still credits only its delta.
        m.handle_event(&step_event(510), Instant::from_millis(3_000));
        assert_eq!(m.cached_data().steps_today, 10);
    }

    #[test]
    fn stopwatch_resyncs_from_rtc_across_sleep() {
        // The embassy clock effectively stands still in this test
        // (only test-execution time passes), standing in for a light
        // sleep; the RTC ticks are what must drive elapsed().
        let mut d = SystemData::default();
        d.time = crate::data::TimeData {
            hour: 10, minute: 0, second: 0,
            ..Default::default()
        };
        d.stopwatch = StopwatchState::Running {
            start: Instant::now(),
            accumulated: Duration::from_ticks(0),
            anchor_secs: 10 * 3600,
        };
        let mut m = Model::new(d, Config::default(), Instant::from_millis(0));

        // RTC says 90 s passed while embassy time stood still.
        let mut t = m.cached_data().time;
        t.minute = 1;
        t.second = 30;
        m.handle_event(&SystemEvent::TimeUpdated { data: t }, Instant::from_millis(1_000));
        let e = m.cached_data().stopwatch.elapsed().as_secs();
        assert!((90..=91).contains(&e), "elapsed {e}, expected ~90");

        // Midnight fold: 10:01:30 -> 00:00:10 next day is 50 410 s
        // from the run's 10:00:00 anchor. The anchor did NOT move on
        // the first resync, so the fold is exact against it.
        let mut t2 = t;
        t2.day += 1;
        t2.hour = 0;
        t2.minute = 0;
        t2.second = 10;
        m.handle_event(&SystemEvent::TimeUpdated { data: t2 }, Instant::from_millis(2_000));
        let e = m.cached_data().stopwatch.elapsed().as_secs();
        assert!((50_410..=50_411).contains(&e), "elapsed {e}, expected ~50410");
    }

    #[test]
    fn gps_tracking_kicks_and_auto_offs() {
        let mut d = SystemData::default();
        d.capabilities.gps = true;
        let mut c = Config::default();
        c.gps.tracking_enabled = true;
        c.gps.tracking_cadence = crate::config::GpsTrackingCadence::Continuous;
        let mut m = Model::new(d, c, Instant::from_millis(0));
        let tick = SystemEvent::TimeUpdated { data: m.cached_data().time };

        // Three kick -> session rounds: continuous cadence is due on
        // every TimeUpdated once the previous session ended. Each
        // simulated session follows the task's real protocol -
        // Syncing first, then the terminal state - because both the
        // change-gate and the failure counter assume that sequence.
        for _ in 0..3 {
            let fx = m.handle_event(&tick, Instant::from_millis(0));
            assert!(fx.iter().any(|e| matches!(
                e,
                Effect::GpsCommand(GpsCommand::TrackOnce { .. })
            )));
            m.handle_event(
                &SystemEvent::GpsSyncUpdated {
                    state: crate::data::GpsSyncState::Syncing { sats: 0, fix_ok: false },
                },
                Instant::from_millis(0),
            );
            m.handle_event(
                &SystemEvent::GpsSyncUpdated {
                    state: crate::data::GpsSyncState::NoSignal,
                },
                Instant::from_millis(0),
            );
        }
        // The third failure flips tracking off (cadence preference
        // survives); the scheduler stays quiet from here.
        assert!(!m.cached_data().config.gps.tracking_enabled);
        assert_eq!(
            m.cached_data().config.gps.tracking_cadence,
            crate::config::GpsTrackingCadence::Continuous,
        );
        let fx = m.handle_event(&tick, Instant::from_millis(0));
        assert!(!fx.iter().any(|e| matches!(
            e,
            Effect::GpsCommand(GpsCommand::TrackOnce { .. })
        )));
    }

    #[test]
    fn wifi_credentials_store_then_join_then_persist() {
        use crate::data::{WifiFailure, WifiState};
        let mut m = fresh();
        let mut out = Effects::new();
        // Nothing stored: CONNECT is a no-op at the model too, not
        // just a disabled button.
        m.dispatch_action(Action::WifiConnect, &mut out);
        assert!(out.is_empty());

        let ssid = heapless::String::try_from("Attic").unwrap();
        let pass = heapless::String::try_from("hunter22").unwrap();
        m.dispatch_action(
            Action::SetWifiCredentials { ssid: ssid.clone(), passphrase: pass.clone() },
            &mut out,
        );
        // Stored AND kicked in one action, tz offset from config.
        assert_eq!(m.cached_data().config.wifi.ssid, ssid);
        assert_eq!(m.cached_data().config.wifi.passphrase, pass);
        assert_eq!(
            out.as_slice(),
            &[Effect::WifiCommand(WifiCommand::SyncOnce {
                ssid: ssid.clone(),
                passphrase: pass.clone(),
                tz_offset_minutes: Config::DEFAULT.time.tz_offset_minutes,
            })]
        );
        // The DONE tap's release flushes the config.
        let fx = m.handle_event(&SystemEvent::TouchReleased, Instant::from_millis(0));
        assert!(fx.iter().any(|e| matches!(e, Effect::SaveConfig)));

        // A scan pass in flight does not block a join: the command
        // queues behind the pass (DONE while the list auto-refreshes).
        m.handle_event(
            &SystemEvent::WifiStatusUpdated { state: WifiState::Scanning },
            Instant::from_millis(0),
        );
        let mut out = Effects::new();
        m.dispatch_action(Action::WifiConnect, &mut out);
        assert!(matches!(
            out.as_slice(),
            [Effect::WifiCommand(WifiCommand::SyncOnce { .. })]
        ));

        // Busy: a second CONNECT is refused while the task reports
        // Connecting; allowed again after a terminal state.
        m.handle_event(
            &SystemEvent::WifiStatusUpdated { state: WifiState::Connecting },
            Instant::from_millis(0),
        );
        let mut out = Effects::new();
        m.dispatch_action(Action::WifiConnect, &mut out);
        assert!(out.is_empty());
        m.handle_event(
            &SystemEvent::WifiStatusUpdated {
                state: WifiState::Failed(WifiFailure::AuthFailed),
            },
            Instant::from_millis(0),
        );
        m.dispatch_action(Action::WifiConnect, &mut out);
        assert!(matches!(
            out.as_slice(),
            [Effect::WifiCommand(WifiCommand::SyncOnce { .. })]
        ));

        // FORGET clears both fields and dirties the config.
        let mut out = Effects::new();
        m.dispatch_action(Action::WifiForget, &mut out);
        assert!(!m.cached_data().config.wifi.is_set());
        assert!(m.cached_data().config.wifi.passphrase.is_empty());
        let fx = m.handle_event(&SystemEvent::TouchReleased, Instant::from_millis(0));
        assert!(fx.iter().any(|e| matches!(e, Effect::SaveConfig)));
    }

    #[test]
    fn wifi_scan_list_fills_and_resets_per_session() {
        use crate::data::{WifiNetwork, WifiState};
        let mut m = fresh();
        let net = |name: &str, rssi: i8| SystemEvent::WifiScanEntry {
            network: WifiNetwork {
                ssid: heapless::String::try_from(name).unwrap(),
                rssi,
                secured: true,
            },
        };
        let mut out = Effects::new();
        m.dispatch_action(Action::WifiScan, &mut out);
        assert_eq!(out.as_slice(), &[Effect::WifiCommand(WifiCommand::Scan)]);

        m.handle_event(
            &SystemEvent::WifiStatusUpdated { state: WifiState::Scanning },
            Instant::from_millis(0),
        );
        // Scanning refuses a second kick.
        let mut out = Effects::new();
        m.dispatch_action(Action::WifiScan, &mut out);
        assert!(out.is_empty());

        m.handle_event(&net("A", -40), Instant::from_millis(0));
        m.handle_event(&net("B", -70), Instant::from_millis(0));
        m.handle_event(
            &SystemEvent::WifiStatusUpdated { state: WifiState::Scanned { count: 2 } },
            Instant::from_millis(0),
        );
        assert_eq!(m.cached_data().wifi_scan.len(), 2);
        assert_eq!(m.cached_data().wifi_scan[0].ssid.as_str(), "A");

        // A refresh merges: B missed this pass but keeps its row, A
        // updates its signal in place, C appends at the end.
        let mut out = Effects::new();
        m.dispatch_action(Action::WifiRescan, &mut out);
        assert_eq!(out.as_slice(), &[Effect::WifiCommand(WifiCommand::Scan)]);
        assert_eq!(m.cached_data().wifi_scan.len(), 2);
        m.handle_event(
            &SystemEvent::WifiStatusUpdated { state: WifiState::Scanning },
            Instant::from_millis(0),
        );
        m.handle_event(&net("A", -45), Instant::from_millis(0));
        m.handle_event(&net("C", -50), Instant::from_millis(0));
        m.handle_event(
            &SystemEvent::WifiStatusUpdated { state: WifiState::Scanned { count: 2 } },
            Instant::from_millis(0),
        );
        let list = &m.cached_data().wifi_scan;
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].ssid.as_str(), "A");
        assert_eq!(list[0].rssi, -45);
        assert_eq!(list[1].ssid.as_str(), "B");
        assert_eq!(list[2].ssid.as_str(), "C");

        // A fresh scan starts from an empty list.
        let mut out = Effects::new();
        m.dispatch_action(Action::WifiScan, &mut out);
        assert!(m.cached_data().wifi_scan.is_empty());
        m.handle_event(
            &SystemEvent::WifiStatusUpdated { state: WifiState::Scanning },
            Instant::from_millis(0),
        );
        m.handle_event(&net("C", -50), Instant::from_millis(0));
        assert_eq!(m.cached_data().wifi_scan.len(), 1);
    }

    #[test]
    fn boot_button_while_awake_sleeps_with_buzz_pulse() {
        let mut m = fresh();
        let fx = m.handle_event(&SystemEvent::BootButtonPressed, Instant::from_millis(0));
        assert!(m.sleeping);
        // First effect: the BOOT haptic pulse.
        assert_eq!(fx[0], Effect::MotorPulse { duration_ms: 100 });
        // Then the sleep transitions.
        assert!(fx.contains(&Effect::BroadcastSleep(SleepState::Sleeping)));
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::TransitionDisplay { to: DisplayState::Off, .. }
        )));
    }

    #[test]
    fn touch_while_sleeping_wakes_and_consumes_event() {
        let mut m = fresh();
        // Go to sleep first (as if BOOT was pressed).
        m.handle_event(&SystemEvent::BootButtonPressed, Instant::from_millis(0));
        assert!(m.sleeping);

        // Touch wakes us and does NOT dispatch to the screen.
        let fx = m.handle_event(
            &SystemEvent::TouchPressed { x: 100, y: 100 },
            Instant::from_millis(5_000),
        );
        assert!(!m.sleeping);
        assert!(fx.contains(&Effect::BroadcastSleep(SleepState::Awake)));
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::TransitionDisplay { to: DisplayState::Active, .. }
        )));
    }

    #[test]
    fn wrist_lowered_while_awake_enters_sleep() {
        let mut m = fresh();
        let fx = m.handle_event(&SystemEvent::WristLowered, Instant::from_millis(0));
        assert!(m.sleeping);
        assert!(fx.contains(&Effect::BroadcastSleep(SleepState::Sleeping)));
    }

    #[test]
    fn wrist_raised_wakes_and_counts_as_activity() {
        let mut m = fresh();
        m.handle_event(&SystemEvent::WristLowered, Instant::from_millis(0));
        assert!(m.sleeping);
        let fx = m.handle_event(&SystemEvent::WristRaised, Instant::from_millis(5_000));
        assert!(!m.sleeping);
        assert!(fx.contains(&Effect::BroadcastSleep(SleepState::Awake)));
    }

    #[test]
    fn unconfirmed_motion_wake_resleeps_after_grace() {
        let mut m = fresh();
        m.handle_event(&SystemEvent::BootButtonPressed, Instant::from_millis(0));
        assert!(m.sleeping);
        // A motion wake turns the display on provisionally...
        m.handle_event(&SystemEvent::WakeOnMotion, Instant::from_millis(10_000));
        assert!(!m.sleeping);
        // ...and with no confirming activity the grace expiry
        // re-sleeps long before the idle-off timeout would.
        let fx = m.tick(Instant::from_millis(16_000), 16);
        assert!(m.sleeping);
        assert!(fx.contains(&Effect::BroadcastSleep(SleepState::Sleeping)));
    }

    #[test]
    fn confirmed_motion_wake_stays_awake_past_grace() {
        let mut m = fresh();
        m.handle_event(&SystemEvent::BootButtonPressed, Instant::from_millis(0));
        m.handle_event(&SystemEvent::WakeOnMotion, Instant::from_millis(10_000));
        // A wrist-raise inside the window confirms the wake.
        m.handle_event(&SystemEvent::WristRaised, Instant::from_millis(12_000));
        m.tick(Instant::from_millis(16_000), 16);
        assert!(!m.sleeping);
    }

    #[test]
    fn shutdown_action_produces_effect() {
        let mut m = fresh();
        // Poke directly via dispatch_action - bypasses screen.
        let mut out: Effects = Vec::new();
        m.dispatch_action(Action::Shutdown, &mut out);
        assert_eq!(out[0], Effect::Shutdown);
    }

    #[test]
    fn snooze_emits_motor_off_and_set_alarm_at_now_plus_10() {
        let mut m = fresh();
        m.cached_data.time.hour = 7;
        m.cached_data.time.minute = 55;
        let mut out: Effects = Vec::new();
        m.dispatch_action(Action::SnoozeAlarm, &mut out);
        assert!(out.contains(&Effect::MotorOff));
        assert!(out.contains(&Effect::RtcCommand(
            RtcCommand::SetAlarm { hour: 8, minute: 5, weekday: None }
        )));
        assert!(m.cached_data.config.alarms.snoozed);
    }

    #[test]
    fn idle_past_dim_threshold_emits_dim_transition() {
        let mut m = fresh();
        // dim_timeout_s defaults; just step well past it.
        let dim_timeout = m.config.display.dim_timeout_s;
        let fx = m.tick(Instant::from_millis((dim_timeout as u64 + 1) * 1000), 0);
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::TransitionDisplay { to: DisplayState::Dim, .. }
        )));
        assert_eq!(m.display_state, DisplayState::Dim);
    }

    #[test]
    fn idle_past_off_threshold_enters_sleep() {
        let mut m = fresh();
        let off_timeout = m.config.display.off_timeout_s;
        let fx = m.tick(Instant::from_millis((off_timeout as u64 + 1) * 1000), 0);
        assert!(m.sleeping);
        assert!(fx.contains(&Effect::BroadcastSleep(SleepState::Sleeping)));
    }

    #[test]
    fn active_alert_holds_off_idle_sleep() {
        let mut m = fresh();
        let t = crate::data::TimeData::default();
        let _ = m.handle_event(
            &SystemEvent::AlarmFired { time: t },
            Instant::from_millis(0),
        );
        // Far past the off threshold with no user activity: the
        // alert must keep the device awake (sleeping would freeze
        // the tone mid-alarm).
        let off_timeout = m.config.display.off_timeout_s;
        let fx = m.tick(Instant::from_millis((off_timeout as u64 + 10) * 1000), 0);
        assert!(!m.sleeping);
        assert!(!fx.contains(&Effect::BroadcastSleep(SleepState::Sleeping)));
        // Dismissing releases the hold: the next idle tick sleeps.
        let mut out: Effects = Vec::new();
        m.dispatch_action(Action::DismissAlarm, &mut out);
        let _ = m.tick(Instant::from_millis((off_timeout as u64 + 20) * 1000), 0);
        assert!(m.sleeping);
    }

    #[test]
    fn boot_sleep_during_alert_stops_tone_and_motor() {
        let mut m = fresh();
        let t = crate::data::TimeData::default();
        let _ = m.handle_event(
            &SystemEvent::AlarmFired { time: t },
            Instant::from_millis(0),
        );
        let fx = m.handle_event(
            &SystemEvent::BootButtonPressed,
            Instant::from_millis(1000),
        );
        assert!(m.sleeping);
        assert!(fx.contains(&Effect::MotorOff));
        assert!(fx.contains(&Effect::AudioCommand(AudioCommand::StopAlarm)));
    }

    #[test]
    fn sleep_pauses_mic_test_and_wake_resumes_it() {
        let mut m = fresh();
        let mut out: Effects = Vec::new();
        m.dispatch_action(Action::SwitchScreen(ScreenId::Settings), &mut out);
        m.dispatch_action(Action::StartMicTest, &mut out);
        // Idle past the off threshold: sleep entry must stop capture
        // (an active DMA session blocks light sleep).
        let off_timeout = m.config.display.off_timeout_s;
        let fx = m.tick(Instant::from_millis((off_timeout as u64 + 1) * 1000), 0);
        assert!(m.sleeping);
        assert!(fx.contains(&Effect::AudioCommand(AudioCommand::StopCapture)));
        // Waking resumes capture: the MicTest view is still on screen,
        // so the meter must come back alive without user action.
        let fx = m.handle_event(
            &SystemEvent::BootButtonPressed,
            Instant::from_millis((off_timeout as u64 + 2) * 1000),
        );
        assert!(!m.sleeping);
        assert!(fx.contains(&Effect::AudioCommand(AudioCommand::StartCapture)));
        // Leaving Settings afterwards is a real exit: capture stops
        // and nothing re-arms it on the next sleep/wake cycle.
        let mut out: Effects = Vec::new();
        m.dispatch_action(Action::Back, &mut out);
        let fx = m.tick(Instant::from_millis((off_timeout as u64 + 3) * 1000), 0);
        assert!(fx.contains(&Effect::AudioCommand(AudioCommand::StopCapture)));
        assert!(m.mic_resume_on_wake.is_none());
    }

    #[test]
    fn sleep_pauses_loopback_and_tones_resume_as_meter() {
        // Loopback pauses and resumes as loopback.
        let mut m = fresh();
        let mut out: Effects = Vec::new();
        m.dispatch_action(Action::SwitchScreen(ScreenId::Settings), &mut out);
        m.dispatch_action(Action::StartLoopbackTest, &mut out);
        let off_timeout = m.config.display.off_timeout_s;
        let fx = m.tick(Instant::from_millis((off_timeout as u64 + 1) * 1000), 0);
        assert!(fx.contains(&Effect::AudioCommand(AudioCommand::StopLoopback)));
        let fx = m.handle_event(
            &SystemEvent::BootButtonPressed,
            Instant::from_millis((off_timeout as u64 + 2) * 1000),
        );
        assert!(fx.contains(&Effect::AudioCommand(AudioCommand::StartLoopback)));

        // A cancelled tone sweep resumes as the meter (Capture), not
        // as a replay of the one-shot sweep.
        let mut m = fresh();
        let mut out: Effects = Vec::new();
        m.dispatch_action(Action::SwitchScreen(ScreenId::Settings), &mut out);
        m.dispatch_action(Action::PlayToneTest, &mut out);
        let fx = m.tick(Instant::from_millis((off_timeout as u64 + 1) * 1000), 0);
        assert!(fx.contains(&Effect::AudioCommand(AudioCommand::StopTones)));
        let fx = m.handle_event(
            &SystemEvent::BootButtonPressed,
            Instant::from_millis((off_timeout as u64 + 2) * 1000),
        );
        assert!(fx.contains(&Effect::AudioCommand(AudioCommand::StartCapture)));
    }

    #[test]
    fn alarm_wake_does_not_resume_mic_test_over_alert() {
        let mut m = fresh();
        let mut out: Effects = Vec::new();
        m.dispatch_action(Action::SwitchScreen(ScreenId::Settings), &mut out);
        m.dispatch_action(Action::StartMicTest, &mut out);
        // Idle sleep pauses the mic test and arms the wake-resume.
        let off_timeout = m.config.display.off_timeout_s;
        let _ = m.tick(Instant::from_millis((off_timeout as u64 + 1) * 1000), 0);
        assert!(m.sleeping);
        // An alarm fires and wakes the device: the alert owns the
        // speaker, so the mic test must NOT auto-resume (a session
        // start would interrupt the alarm session and silence it).
        let t = crate::data::TimeData::default();
        let fx = m.handle_event(
            &SystemEvent::AlarmFired { time: t },
            Instant::from_millis((off_timeout as u64 + 2) * 1000),
        );
        assert!(!m.sleeping);
        assert!(fx.contains(&Effect::AudioCommand(AudioCommand::PlayAlarm)));
        assert!(!fx.contains(&Effect::AudioCommand(AudioCommand::StartCapture)));
        assert!(m.mic_resume_on_wake.is_none());
    }
}
