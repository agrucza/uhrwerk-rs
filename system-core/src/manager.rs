use app_core::config::Config;
use firmware_hal::display::{HEIGHT, NUM_TILES, TILE_H, WIDTH};
use app_core::events::SystemEvent;
use crate::board::{Board, CpuFreq};
use crate::bus::{AudioCommand, AUDIO_COMMAND, EVENTS, IMU_COMMAND, RTC_COMMAND, RtcCommand, SLEEP_WATCH, SleepState};
use crate::display::Display;
use crate::tasks::boot_button::BootButtonTaskState;
use crate::tasks::imu::ImuTaskState;
use crate::tasks::power::PowerTaskState;
use crate::tasks::rtc::RtcTaskState;
use crate::tasks::touch::TouchTaskState;
use app_core::ui::primitives;
use app_core::ui::types::{DirtyRegion, RenderCtx, SystemData};
use embedded_graphics::draw_target::DrawTarget;
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::gpio::Input;
use esp_hal::i2c::master::I2c;
use esp_hal::Blocking;

// -- Persistence path + version ----------------------------------------------
//
// Used by the boot-time load (in `run()`) and the save-on-change
// handler (`Effect::SaveConfig`).
// Bump a version whenever the on-disk shape of the value type
// changes so old records get ignored rather than silently
// misinterpreted.
// THE settings blob: the whole config tree, alarms included (one
// store, tagged per leaf - see app-core config.rs).
const CONFIG_PATH:    &str = "/system/config/config.bin";
// v2 = the tagged field store (TLV payload, see app-core config.rs).
// v1 was the positional postcard struct; rejecting it here is the
// one-time settings reset that buys per-field schema tolerance -
// with the tagged payload this version should never move again.
const CONFIG_VERSION: u8   = 2;


/// Framebuffer row stride in bytes. Used when sizing the per-tile
/// FB slice handed to the hash function.
const ROW_STRIDE: usize = WIDTH as usize * 2;

/// FNV-1a 32-bit hash over a contiguous slice of the framebuffer.
/// Used per-tile to detect whether the rendered tile differs from
/// the previous frame's, so unchanged tiles aren't pushed over QSPI.
/// `WIDTH * 2 = 820` is a multiple of 4, so every row's byte count
/// is 4-aligned and `chunks_exact(4)` has no tail to worry about.
#[inline]
fn fb_hash(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for chunk in bytes.chunks_exact(4) {
        let v = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        h ^= v;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Bitmask over [`NUM_TILES`] tile indices: bit `i` set means tile `i`
/// needs to be rendered + hashed this frame.
type TileMask = u16;

/// Convert a [`DirtyRegion`] into the bitmask of tiles it touches.
///
/// [`DirtyRegion::FullScreen`] maps to all-tiles-dirty. Each rectangle
/// in the [`DirtyRegion::Rects`] variant adds the tiles its y-range
/// intersects to the mask. Rectangles with x extents outside the panel
/// are still honored vertically - the renderer will draw them clipped
/// at the per-pixel level inside the driver.
fn dirty_to_tile_mask(dirty: &DirtyRegion) -> TileMask {
    match dirty {
        DirtyRegion::FullScreen => {
            // `((1 << NUM_TILES) - 1)` fits in u16 for any NUM_TILES <= 16.
            // NUM_TILES is 11 today so this is well in range.
            (1u16 << NUM_TILES) - 1
        }
        DirtyRegion::Rects(rects) => {
            let mut mask: TileMask = 0;
            for r in rects {
                let y0 = r.top_left.y;
                let y1 = y0 + r.size.height as i32; // exclusive
                if y1 <= 0 || y0 >= HEIGHT as i32 { continue; }
                let y0 = y0.max(0) as u16;
                let y1 = (y1 as u16).min(HEIGHT) - 1; // inclusive last row
                let tile_start = (y0 / TILE_H) as usize;
                let tile_end   = (y1 / TILE_H) as usize;
                for t in tile_start..=tile_end {
                    if t < NUM_TILES {
                        mask |= 1u16 << t;
                    }
                }
            }
            mask
        }
    }
}

/// Bundle of per-device task state structs produced by
/// [`SystemManager::init`] and handed to `main` so it can spawn
/// one embassy task per peripheral.
///
/// Each field owns everything a task needs to run independently
/// (driver handle, GPIO lines, buffers). The shared I2C bus is
/// handed to each task separately as a `&'static SharedI2c` via
/// [`SystemManager::i2c_bus`] and is intentionally not part of
/// this bundle.
pub struct TaskBundle {
    pub touch: TouchTaskState<'static>,
    pub boot_button: BootButtonTaskState<'static>,
    pub rtc: RtcTaskState<'static>,
    pub imu: ImuTaskState<'static>,
    pub power: PowerTaskState,
}

/// Everything the bin constructs (per-board, chip-specific) and hands
/// to [`SystemManager::new`] / [`run`]. A parameter object so the
/// call sites stay readable and adding/removing a piece is a single
/// named-field edit, not a positional reshuffle in every bin.
///
/// `rtc` is the esp-hal RTC controller (used for light sleep);
/// `rtc_state` is the PCF85063 task state - distinct things.
pub struct SystemParts<B: Board> {
    pub i2c_bus: &'static crate::bus::SharedI2c,
    pub board: B,
    pub display: Display<'static>,
    /// Full-panel pixel canvas on boards with PSRAM (see
    /// [`Bringup::take_fb_canvas`]); `None` on tile-only boards.
    pub fb_canvas: Option<&'static mut [u8]>,
    pub lcd_te: Option<Input<'static>>,
    pub rtc: esp_hal::rtc_cntl::Rtc<'static>,
    pub store: crate::storage::Store<'static>,
    pub initial_time: app_core::data::TimeData,
    pub initial_power: app_core::data::PowerData,
    pub touch: TouchTaskState<'static>,
    pub boot_button: BootButtonTaskState<'static>,
    pub rtc_state: RtcTaskState<'static>,
    pub imu: ImuTaskState<'static>,
    pub power: PowerTaskState,
    /// The board's optional-hardware flags (from
    /// [`Bringup::capabilities`]), cached into `SystemData`.
    pub capabilities: app_core::data::Capabilities,
    /// Panel pixels masked by this device's case (from
    /// [`Bringup::safe_area`]), cached into `SystemData`.
    pub safe_area: app_core::data::SafeArea,
    /// The settings tree, loaded from flash by `run()` BEFORE any
    /// hardware that consumes a persisted setting was initialized
    /// (config-first boot).
    pub config: Config,
}

/// The board-agnostic system brain. Generic over the [`Board`] seam;
/// everything board-specific is injected via `board` or constructed
/// in the bin and passed to [`SystemManager::new`].
///
/// The audio *stack* lives in [`crate::audio`] (shared, generic).
/// Audio bring-up wiring (concrete peripheral tokens) is deferred to
/// the bin until there's a real caller - it was dead code here.
#[allow(dead_code)]
pub struct SystemManager<'d, B: Board> {
    // Shared I2C bus behind an async mutex. Lives in the global
    // `I2C_BUS` StaticCell; tasks and the main loop lock it before
    // each access. See `crate::bus` for details.
    i2c_bus: &'static crate::bus::SharedI2c,

    // The board seam: haptic, power-off, wake-source arming.
    pub board: B,

    // Peripherals
    pub display: Display<'d>,

    // Tearing-effect line from the display. We wait for its rising
    // edge (vblank start) before pushing pixels so partial flushes
    // don't land mid-scanout. `None` on boards with no TE GPIO -
    // the wait is then skipped entirely (no timeout penalty).
    lcd_te: Option<Input<'d>>,

    // UI + app state: screen, nav stack, sleep flag, display
    // state, cached snapshots, dim/idle timers, buzz pattern,
    // config. All moved into `app_core::model::Model` where the
    // event->effect dispatch is host-testable.
    model: app_core::model::Model,

    // Periodic loop counter (diagnostics).
    tick_count: u32,

    // Per-tile FNV-1a hashes from the previous frame. After each tile is
    // rendered we hash its FB contents; if the hash differs from this
    // entry the tile is dirty and gets pushed over QSPI. Sized to
    // [`NUM_TILES`] so every tile (including the short final tile that
    // overlaps the bottom of the panel) has its own slot.
    tile_hashes: [u32; NUM_TILES],

    // Full-panel pixel canvas (PSRAM boards, `None` elsewhere). Every
    // pushed tile is mirrored into it so it always holds what the
    // panel shows; compositing effects read it. Never rendered into
    // directly - see `render()`.
    fb_canvas: Option<&'static mut [u8]>,

    // Canvas tiles whose mirror copy was deferred (bit per tile, same
    // layout as `render()`'s tile mask). While a finger is on the
    // glass the per-tile canvas memcpy is skipped: on quad-PSRAM
    // boards it costs ~6-8 ms per tile, which turned ~47 ms
    // interactive frames into ~115-130 ms. `sync_canvas()` re-mirrors
    // the stale tiles at the first quiet tick after the touch ends.
    canvas_stale: u16,

    // A finger is currently on the glass (TouchPressed seen, no
    // release yet). Gates the canvas mirror; cleared by any
    // release-shaped event and by WakeInterrupt, so a release
    // swallowed around a sleep transition can't wedge the gate shut.
    touch_active: bool,

    // Force a full re-render on the next render() call, ignoring the
    // active screen's `dirty_rects()` opinion. Set after wake / display-
    // power-on (the panel's GRAM may not match anything we've drawn yet,
    // so the screen's "last rendered" snapshot can't be trusted).
    // Cleared by render() after the frame.
    force_full_redraw: bool,

    // RTC controller, used to enter/exit hardware light sleep.
    rtc: esp_hal::rtc_cntl::Rtc<'d>,

    // SoC RTC slow-clock counter (whole seconds) captured at
    // construction. `uptime_secs` is reported relative to this so it
    // means "since this boot", not since the last hard power-on: the
    // RTC timer lives in the always-on RTC domain and survives digital
    // resets (reflash / software reset), so the raw counter keeps
    // climbing across reboots. Subtracting this baseline still counts
    // time spent in light sleep (the counter advances during sleep).
    boot_uptime_secs: u32,

    // Light-sleep diagnostic: count of completed sleep cycles since
    // boot (incremented after each rtc.sleep() returns). Surfaced via
    // Model::set_sleep_telemetry each tick; the cycle rate vs uptime
    // tells whether the CPU is really gating off.
    sleep_cycles: u32,

    // Light-sleep diagnostic (remove with the other probes): the
    // last UI-waking sleep cycle's (cycle, wake-cause bits,
    // slept_ms), re-logged at the end of the first awake tick. The
    // summary printed right at the wake often lands in the
    // USB-Serial-JTAG resync blackout and comes out shredded;
    // delaying it there would lag the wake render, so it is
    // deferred to a point where output demonstrably survives.
    pending_wake_summary: Option<(u32, u32, u64)>,

    // Unified persistent-storage facade. Owns the on-flash
    // LittleFS and the SD volume manager together with the
    // SD-mirror online flag. Mirrored writes, flash-only escape
    // hatch (`flash_mut`) and SD-only escape hatch (`sd_mut`) all
    // live on this one handle. See `system::storage` for the API.
    store: crate::storage::Store<'d>,

    // Last storage usage pushed into the event channel. Compared
    // against a fresh summary after each save / boot; we only
    // emit `SystemEvent::StorageUsageUpdated` when the value
    // actually differs. Keeps the fire-hose out of the event
    // pipeline.
    last_storage_usage: app_core::data::StorageUsage,

    // Last time the periodic SD-recovery hook (in `tick`) attempted
    // a re-probe. Throttles the probe so we don't hammer the slot
    // every tick when the card is genuinely absent. `None` until
    // the first attempt fires.
    last_sd_recover_attempt: Option<Instant>,
    /// Throttle for [`Self::service_sd_slot`]'s detect-line read
    /// (ticks fire per event; the expander read must not run at
    /// touch-drag rate).
    last_sd_detect_check: Option<Instant>,
}

/// How often the tick loop will retry an SD probe when the mirror
/// is currently believed offline. Tuned so a hot-replug recovers
/// within a few seconds without flooding the slot with probes.
const SD_RECOVER_INTERVAL: Duration = Duration::from_secs(5);

// `NAV_STACK_DEPTH` and the `NavStack` type live in
// `app_core::nav` so the stack's push/pop semantics are
// host-testable.

impl<B: Board> SystemManager<'static, B> {
    /// Assemble the manager from already-constructed, board-specific
    /// pieces. The bin builds all hardware (I2C bus + tasks, power /
    /// `Board` impl, display, TE input, RTC controller, `Store` with
    /// its `FlashRegion`) and takes the initial sensor snapshots;
    /// `new` does the board-agnostic remainder: load Config/Alarms
    /// from flash, recover the event-log seq, probe SD, log boot,
    /// seed the UI snapshot, build the `Model`, and assemble.
    ///
    /// Returns `(SystemManager, TaskBundle)` - the bin spawns one
    /// task per peripheral from the bundle.
    pub fn new(parts: SystemParts<B>) -> (Self, TaskBundle) {
        let SystemParts {
            i2c_bus,
            mut board,
            display,
            fb_canvas,
            lcd_te,
            rtc,
            mut store,
            initial_time,
            initial_power,
            touch: touch_state,
            boot_button: boot_button_state,
            rtc_state,
            imu: imu_state,
            power: power_state,
            capabilities,
            safe_area,
            config: loaded_config,
        } = parts;

        // Seed the shared wall clock so any SD writes before the
        // first `TimeUpdated` (e.g. the boot line) see real calendar
        // time.
        drivers::sdcard::update_wall_clock(
            initial_time.year, initial_time.month, initial_time.day,
            initial_time.hour, initial_time.minute, initial_time.second,
        );

        // Recover the monotonic seq counter before the first log line.
        crate::event_log::init_seq_from_flash(&mut store);

        // Probe SD; on success the store backfills the event log.
        // Boards with a card-detect line skip the probe when the
        // slot is empty - a cardless probe costs seconds of
        // embedded-sdmmc retries. Pre-task-spawn, so the bus
        // try_lock cannot be contended; if it somehow fails anyway,
        // fall through to the probe (the old behavior).
        let slot_empty = i2c_bus
            .try_lock()
            .ok()
            .and_then(|mut i2c| board.sd_detect(&mut i2c))
            == Some(false);
        let sd_online = if slot_empty {
            log::info!("SD: slot empty - boot probe skipped");
            false
        } else {
            store.probe_sd()
        };

        crate::event_log::log_boot(&mut store, &initial_time);

        let fs_usage = store.usage();
        let initial_usage = app_core::data::StorageUsage {
            files: fs_usage.files,
            total_bytes: fs_usage.total_bytes,
            sd_online,
        };
        // (Whether the config loaded or defaulted is logged by
        // `run()` right where the load happens.)
        log::info!(
            "store: files={} region={}KB sd={}",
            fs_usage.files,
            fs_usage.total_bytes / 1024,
            if sd_online { "online" } else { "offline" },
        );

        // Seed the first cached-data snapshot before task events.
        let mut cached_data = SystemData::default();
        cached_data.time = initial_time;
        cached_data.power = initial_power;
        cached_data.storage = initial_usage;
        cached_data.capabilities = capabilities;
        cached_data.safe_area = safe_area;
        crate::event_log::load_battery_history(
            &mut store, &mut cached_data.battery_history,
        );

        let model = app_core::model::Model::new(
            cached_data, loaded_config, Instant::now(),
        );

        // Baseline the always-on RTC counter at boot (see the field
        // docs) so uptime is measured from this boot.
        let boot_uptime_secs = rtc.time_since_power_up().as_secs() as u32;

        let mut manager = Self {
            i2c_bus,
            board,
            display,
            lcd_te,
            model,
            tick_count: 0,
            tile_hashes: [0u32; NUM_TILES],
            fb_canvas,
            canvas_stale: 0,
            touch_active: false,
            // First render after boot has no snapshot to diff; force
            // a full pass before dirty_rects takes over.
            force_full_redraw: true,
            rtc,
            boot_uptime_secs,
            sleep_cycles: 0,
            pending_wake_summary: None,
            store,
            last_storage_usage: initial_usage,
            last_sd_recover_attempt: None,
            last_sd_detect_check: None,
        };

        let bundle = TaskBundle {
            touch: touch_state,
            boot_button: boot_button_state,
            rtc: rtc_state,
            imu: imu_state,
            power: power_state,
        };

        // Baseline clock; render boosts to 160 MHz per-frame.
        manager.board.set_cpu_freq(CpuFreq::Mhz80);

        (manager, bundle)
    }

    /// Accessor for the shared I2C bus reference.
    ///
    /// `main` needs this so it can hand the same `&'static SharedI2c`
    /// to each spawned peripheral task after `init` returns. The
    /// underlying field stays private; every runtime I2C user in
    /// manager goes through `self.i2c_bus.lock().await` directly.
    pub fn i2c_bus(&self) -> &'static crate::bus::SharedI2c {
        self.i2c_bus
    }

    // ================= Effect executor ===========================================

    /// Apply the effects emitted by `Model::handle_event` /
    /// `Model::tick`. Each variant maps to the concrete hardware
    /// path (signal channel, async display transition, GPIO
    /// toggle, shutdown).
    async fn execute_effects(&mut self, effects: app_core::model::Effects) {
        use app_core::model::Effect;
        for effect in effects {
            match effect {
                Effect::TransitionDisplay { from, to } => {
                    // WAKE PROBE - remove after the wake-gap diagnosis.
                    log::info!("wakeprobe: rtc {} ms - display transition start", self.rtc_now_us() / 1000);
                    let waking_from_off = crate::display::transition(
                        &mut self.display,
                        from,
                        to,
                        &self.model.config().display,
                    ).await;
                    log::info!("wakeprobe: rtc {} ms - display transition done", self.rtc_now_us() / 1000);
                    // Coming out of DISPOFF the panel's GRAM is whatever
                    // garbage it held while powered down. Ignore the
                    // per-screen dirty_rects on the next frame and push
                    // everything so the panel is repopulated from scratch.
                    if waking_from_off {
                        self.tile_hashes.fill(0);
                        self.force_full_redraw = true;
                    }
                }
                Effect::BroadcastSleep(state) => {
                    SLEEP_WATCH.sender().send(state);
                    match state {
                        SleepState::Sleeping => log::info!("system: sleep"),
                        SleepState::Awake => {
                            // WAKE PROBE - remove after the wake-gap diagnosis.
                            log::info!("wakeprobe: rtc {} ms - awake broadcast + touch wake", self.rtc_now_us() / 1000);
                            // Kick the touch controller awake first so
                            // its boot overlaps the repaint below.
                            // No-op on controllers that wake on their
                            // own; see `Board::touch_wake`.
                            {
                                let mut i2c = self.i2c_bus.lock().await;
                                self.board.touch_wake(&mut i2c);
                            }
                            log::info!("wakeprobe: rtc {} ms - touch wake done", self.rtc_now_us() / 1000);
                            // Same reasoning as the DISPOFF -> Active
                            // transition above: stale GRAM + stale
                            // screen snapshot, so force a full repaint.
                            self.tile_hashes.fill(0);
                            self.force_full_redraw = true;
                            log::info!("system: wake");
                        }
                    }
                }
                // All three motor effects gate on `config.alerts.haptics_enabled`
                // so the user-facing toggle in Settings actually
                // suppresses haptic feedback when disabled.
                Effect::MotorOn => {
                    if self.model.config().alerts.haptics_enabled {
                        self.board.buzz();
                    }
                }
                Effect::MotorOff => self.board.buzz_stop(),
                Effect::MotorPulse { duration_ms } => {
                    if self.model.config().alerts.haptics_enabled {
                        self.board.buzz();
                        Timer::after(Duration::from_millis(duration_ms as u64)).await;
                        self.board.buzz_stop();
                    }
                }
                Effect::RtcCommand(cmd) => RTC_COMMAND.signal(cmd),
                Effect::ImuCommand(cmd) => IMU_COMMAND.signal(cmd),
                Effect::AudioCommand(cmd) => {
                    // Only the alert tone is gated on the live
                    // `sound_enabled` toggle (mirroring how `MotorOn`
                    // gates on `haptics_enabled` just above). StopAlarm
                    // always forwards so toggling sound off mid-alert
                    // still silences the speaker, and the mic-test
                    // commands (capture, tone sweep, loopback) are a
                    // separate user-initiated diagnostic, unaffected
                    // by the alert-sound setting.
                    let forward = match cmd {
                        AudioCommand::PlayAlarm => self.model.config().alerts.sound_enabled,
                        AudioCommand::StopAlarm
                        | AudioCommand::StartCapture
                        | AudioCommand::StopCapture
                        | AudioCommand::PlayTones
                        | AudioCommand::StopTones
                        | AudioCommand::StartLoopback
                        | AudioCommand::StopLoopback => true,
                    };
                    // Non-blocking: a full queue means the audio task
                    // has been wedged for several commands' worth of
                    // time; dropping (loudly) beats stalling the main
                    // loop behind it.
                    if forward && AUDIO_COMMAND.try_send(cmd).is_err() {
                        log::warn!("audio: command queue full, dropped {:?}", cmd);
                    }
                }
                Effect::GpsCommand(cmd) => crate::bus::GPS_COMMAND.signal(cmd),
                Effect::Shutdown => {
                    log::info!("System: shutdown requested");
                    self.board.shutdown();
                }
                Effect::FactoryReset => {
                    log::info!("factory reset: wiping flash + SD mirror");
                    self.store.reset_user_data();
                    self.refresh_storage_usage().await;
                }
                Effect::ProbeSd => {
                    log::info!("SD: re-probing on user request");
                    self.store.probe_sd();
                    self.refresh_storage_usage().await;
                }
                Effect::RestoreFromSd => {
                    log::info!("restore: copying config blob from SD to flash");
                    let (copied, skipped) = self.store
                        .restore_config_from_sd(&[CONFIG_PATH]);
                    log::info!(
                        "restore: {} copied, {} skipped - resetting",
                        copied, skipped,
                    );
                    // Brief yield so pending log writes and the
                    // final UI render can land before the reset.
                    Timer::after(Duration::from_millis(200)).await;
                    esp_hal::system::software_reset();
                }
                Effect::SaveConfig => {
                    self.store.save_blob(
                        CONFIG_PATH,
                        CONFIG_VERSION,
                        self.model.config(),
                    );
                    self.refresh_storage_usage().await;
                }
                Effect::SetDisplayBrightness(value) => {
                    self.display.set_brightness(value).await;
                }
            }
        }
    }

    /// Periodic SD slot service - the hotplug driver. Two modes,
    /// picked by whether the board has a card-detect line:
    ///
    /// * `Board::sd_detect` = `Some(_)`: the line is the truth.
    ///   Card gone while the mirror is online -> flip offline
    ///   immediately and tell the UI (no probe, no waiting for a
    ///   write to fail). Card present while offline -> throttled
    ///   probe + backfill below. Both directions of hotplug, no
    ///   user action, and an empty slot costs one bus read per
    ///   tick instead of a seconds-long failed probe.
    /// * `None` (no detect line): blind throttled re-probe when
    ///   offline - the pre-hotplug behavior, unchanged.
    ///
    /// Runs from the awake tick only; a card swapped during sleep
    /// is picked up on the first tick after wake. The detect read
    /// is throttled to once per `SD_RECOVER_INTERVAL`: ticks fire
    /// per event, and an unthrottled read would hit the expander at
    /// touch-drag rate, contending with the touch task for the bus.
    async fn service_sd_slot(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_sd_detect_check {
            if now.duration_since(last) < SD_RECOVER_INTERVAL {
                return;
            }
        }
        self.last_sd_detect_check = Some(now);
        let detect = {
            let mut i2c = self.i2c_bus.lock().await;
            self.board.sd_detect(&mut i2c)
        };
        match detect {
            Some(false) => {
                if self.store.sd_online() {
                    log::info!("SD: card removed - mirror offline");
                    self.store.on_sd_removed();
                    self.refresh_storage_usage().await;
                }
            }
            Some(true) | None => self.try_sd_auto_recovery().await,
        }
    }

    /// Throttled probe half of [`Self::service_sd_slot`]: when the
    /// mirror is currently offline, re-probe the slot at most once
    /// per `SD_RECOVER_INTERVAL`. On a successful recovery, push a
    /// `StorageUsageUpdated` event so the UI's "SD online"
    /// indicator updates without waiting for the next save.
    async fn try_sd_auto_recovery(&mut self) {
        if self.store.sd_online() {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_sd_recover_attempt {
            if now.duration_since(last) < SD_RECOVER_INTERVAL {
                return;
            }
        }
        self.last_sd_recover_attempt = Some(now);
        if self.store.try_recover_sd() {
            log::info!("SD: auto-recovered (card detected)");
            self.refresh_storage_usage().await;
        }
    }

    /// Recompute filesystem usage and push a `StorageUsageUpdated`
    /// event if the value actually changed. Called after any
    /// operation that could affect the store (save / reset /
    /// boot). Keeps the fire-hose out of the event pipeline when
    /// nothing visibly changed.
    async fn refresh_storage_usage(&mut self) {
        let fs_usage = self.store.usage();
        let fresh = app_core::data::StorageUsage {
            files: fs_usage.files,
            total_bytes: fs_usage.total_bytes,
            sd_online: self.store.sd_online(),
        };
        if fresh != self.last_storage_usage {
            self.last_storage_usage = fresh;
            EVENTS.send(SystemEvent::StorageUsageUpdated { usage: fresh }).await;
        }
    }

    /// The old sleep/wake docs preserved for reference.
    ///
    /// Sleep broadcasts [`SLEEP_WATCH`] so subscribers can enter
    /// low-power modes (IMU -> WoM, touch -> Monitor, power task
    /// -> slow poll), blanks the display, and sets Model's
    /// `sleeping` flag. Wake reverses it. All of that now lives
    /// in `app_core::model::Model::{sleep, wake}` emitting
    /// effects that `execute_effects` applies here.
    // `sleep`, `wake`, `apply_dim_state`, `check_idle_sleep`,
    // `tick_buzz`, `check_alarm_reprogram` all live on
    // `app_core::model::Model` now. The manager's role shrinks to
    // `execute_effects` (above) plus the tick/render loop (below).

    // ================= Light sleep ================================================

    /// GPIO bit in the sleep-wakeup-cause register
    /// (`Board::wake_cause_raw`) - the one position that is uniform
    /// across every chip family we run, so the shared manager owns
    /// the mask. The timer bit is NOT uniform (bit 3 on the s3
    /// family, bit 4 on the c6) - the diagnostics print the raw bits
    /// instead of decoding it.
    const WAKE_CAUSE_GPIO: u32 = 1 << 2;

    /// Read the RTC-domain wall clock (us since boot), fresh.
    /// `Rtc::current_time_us` triggers the counter-to-buffer latch
    /// and reads the buffer immediately; observed on the s3: right
    /// after `rtc.sleep()` returns, that read still yields the
    /// PRE-sleep value (a proven 5 s heartbeat sleep measured as
    /// "slept 0 ms"), while a read a few hundred ms later is
    /// correct. The exact mechanism is unclear (ESP-IDF uses the
    /// same immediate-read pattern on this family), so instead of a
    /// fixed delay, re-read until the buffer visibly moves - the RTC
    /// counter ticks every ~7 us, so two completed latches never
    /// compare equal. Bounded at ~2 ms in case the buffer is frozen
    /// longer; then the stale value is returned rather than hanging.
    fn rtc_now_us(&self) -> u64 {
        let first = self.rtc.current_time_us();
        let delay = esp_hal::delay::Delay::new();
        for _ in 0..100 {
            delay.delay_micros(20);
            let again = self.rtc.current_time_us();
            if again != first {
                return again;
            }
        }
        first
    }

    /// Enter hardware light sleep. Blocks until a configured wake
    /// source (any of the armed GPIO INTs, or the heartbeat timer)
    /// fires, then returns `(wake_cause, slept_ms)`: the raw bits of
    /// the chip's sleep-wakeup-cause register (via
    /// `Board::wake_cause_raw`; GPIO = `Self::WAKE_CAUSE_GPIO`) and
    /// how long the chip actually slept (RTC-domain clock).
    ///
    /// The three INT pins are armed for RTC wake at init time via
    /// `wakeup_enable(true, WakeEvent::LowLevel)`. The timer is
    /// configured per-call here.
    ///
    /// Only a minimal config is used - defaults from esp-hal 1.1
    /// pick up the BBPLL-force-pu fix that was missing in 1.0.0.
    /// `light_slp_reject(false)` stops a pending interrupt (e.g.
    /// a half-minute PCF85063 INT still latched) from silently
    /// cancelling sleep entry.
    async fn enter_light_sleep(&mut self) -> (u32, u64) {
        use esp_hal::delay::Delay;
        use esp_hal::rtc_cntl::sleep::{
            GpioWakeupSource, RtcSleepConfig, TimerWakeupSource,
        };

        // Drop the touch controller to its low-power state
        // synchronously before sleep entry, bus lock held. Doing it
        // here rather than in the touch task off SLEEP_WATCH closes
        // a race: if `rtc.sleep()` fires before the task acquires
        // the I²C lock, the chip is still in full-scan mode at sleep
        // entry (observed as intermittent wake-from-touch). What
        // "low-power" means for the board's controller - and whether
        // touch remains a wake source - lives behind the seam; see
        // `Board::touch_sleep`. Re-runs on every heartbeat re-entry.
        {
            let mut i2c = self.i2c_bus.lock().await;
            self.board.touch_sleep(&mut i2c);
        }

        // Re-arm this board's wake sources. The embassy async GPIO
        // drivers used by the INT-pin tasks call `listen_with_options
        // (..., wake_up_from_light_sleep=false)` on every wait, which
        // clears the `wakeup_enable` bits set at init; the board sets
        // them back here, immediately before `rtc.sleep()`.
        self.board.arm_wake_sources();

        let gpio_wake = GpioWakeupSource::new();
        // 5 s matches the power task's sleep-mode poll interval,
        // so every wake cycle lines up with a real background poll
        // (battery, VBUS, charging state). At ~150 ms active per
        // wake this is ~3 % duty cycle - low overhead while still
        // letting background tasks make forward progress during
        // sleep. Touch/BOOT/RTC INT wake-from-sleep still fires
        // immediately regardless of this timer.
        let timer_wake = TimerWakeupSource::new(
            core::time::Duration::from_secs(5),
        );

        // Start from the default and let the board apply its
        // chip-specific reliability tuning (the available knobs - XTAL
        // / RTC-regulator force-power-up, light-sleep-reject - differ
        // per chip family, so they live behind the seam).
        let mut config = RtcSleepConfig::default();
        self.board.tune_sleep_config(&mut config);

        // Drop to baseline before sleep: at max clock the slow-clock
        // alarm programming in TimerWakeupSource can't latch before
        // the CPU gates off and timer wake never fires.
        self.board.set_cpu_freq(CpuFreq::Mhz80);

        // Settle delay before sleep - lets the slow-clock alarm
        // writes latch into the RTC domain before the CPU gates
        // off. 100 ms matches the official esp-hal sleep_timer
        // example; shorter values have been observed to miss wake.
        let delay = Delay::new();
        delay.delay_millis(100);

        // Diagnostic probe: how long the chip actually slept, off
        // the RTC-domain clock (which keeps counting through light
        // sleep, unlike embassy/systimer time). A GPIO-cause wake at
        // ~heartbeat duration would mean the cause register is lying
        // (stale bits); one at ~0 ms means a wake line pulsed right
        // at sleep entry. Both ends use the stale-flushing read (see
        // `rtc_now_us`) - the plain read right after `rtc.sleep()`
        // returns the PRE-sleep latch and fakes a 0 ms sleep.
        let entered_us = self.rtc_now_us();
        self.rtc.sleep(&config, &[&gpio_wake, &timer_wake]);
        let slept_ms = (self.rtc_now_us() - entered_us) / 1000;
        self.sleep_cycles += 1;

        // Why did we wake? Read before anything else can re-enter
        // sleep and overwrite the cause register. GPIO bit = one of
        // the armed wake lines fired (user-facing); timer bit = the
        // heartbeat. Behind the board seam - the cause register is
        // chip-specific (and esp-hal's `wakeup_cause()` only works
        // for deep sleep; see the hook's doc).
        let wake_cause = self.board.wake_cause_raw();

        // Post-wake settle delay. USB-Serial-JTAG in particular loses
        // the first ~tens of ms of output after light-sleep wake
        // because the USB host has to re-sync. Without this delay the
        // first `log::info!` below gets partially eaten by the host
        // and reads as a bare "INFO - " line. Costs a bit of idle
        // power but makes on-device debugging usable; drop or shrink
        // once we're confident this is shipping.
        delay.delay_millis(100);

        // NOTE: printed this close to the wake, this line is often
        // eaten by the USB-Serial-JTAG resync blackout - the tick
        // loop re-logs a summary after the wake transition, where
        // output reliably survives.
        log::info!(
            "light_sleep: woke (cycle {}, cause {:#b}, slept {} ms)",
            self.sleep_cycles,
            wake_cause,
            slept_ms,
        );
        (wake_cause, slept_ms)
    }

    // ================= Event loop ================================================

    /// Handle a single event received from the global event channel.
    ///
    /// The pure logic lives on `app_core::model::Model::handle_event`
    /// (snapshot caching, sleep/wake decisions, screen dispatch,
    /// action interpretation). This wrapper hands the event to the
    /// model and executes the returned effects on hardware.
    async fn handle_event(&mut self, event: SystemEvent) {
        // Interaction tracking for the canvas-mirror gate (see the
        // `canvas_stale` field docs): finger down defers per-tile
        // mirroring, anything release-shaped re-enables it.
        match &event {
            SystemEvent::TouchPressed { .. } => self.touch_active = true,
            SystemEvent::TouchReleased
            | SystemEvent::Tap { .. }
            | SystemEvent::Swipe { .. }
            | SystemEvent::WakeInterrupt => self.touch_active = false,
            _ => {}
        }

        // Bridge fresh RTC time into the SD-card wall clock so file
        // mtimes and log lines see real calendar time. Cheap - a
        // single atomic store.
        if let SystemEvent::TimeUpdated { data } = &event {
            drivers::sdcard::update_wall_clock(
                data.year, data.month, data.day,
                data.hour, data.minute, data.second,
            );
        }

        // Best-effort append to /system/logs/events.log on flash
        // (always) and on SD if a card was detected at boot. No-op
        // if the event isn't loggable. Runs before the model so the
        // log captures the triggering event even if the model
        // transitions into sleep or shuts down.
        crate::event_log::try_log(
            &mut self.store,
            &self.model.cached_data().time,
            &event,
        );

        let was_sleeping = self.model.sleeping();
        let effects = self.model.handle_event(&event, Instant::now());
        self.execute_effects(effects).await;
        // Diagnostic probe: name the event that ended sleep. Logged
        // AFTER the effects so the wake's display transition has
        // already bought the USB host time to resync - printed any
        // earlier, this line lands in the post-wake blackout and is
        // never seen.
        if was_sleeping && !self.model.sleeping() {
            log::info!("system: wake cause: {:?}", event);
        }
    }

    /// Run one iteration of the main event loop.
    ///
    /// Waits on the global event channel with an idle-timeout, then
    /// applies dim/idle-sleep transitions and renders if anything
    /// flagged a redraw. The timeout gives the idle timer a heartbeat
    /// even when no events are arriving. `main` calls this in a loop.
    pub async fn tick(&mut self) {
        // Sleeping path: drain any pending events first, then halt
        // the CPU in hardware light sleep until a wake source fires.
        if self.model.sleeping() {
            while let Ok(event) = EVENTS.try_receive() {
                self.handle_event(event).await;
                if !self.model.sleeping() {
                    return;
                }
            }
            // An active hardware session holds a wake lock (a GPS
            // sync today; audio is the planned second holder): its
            // peripheral needs continuously running clocks, and
            // hardware light sleep gates the UART/I2S clocks and
            // silently drops their data. Idle here instead - the
            // display stays dark, embassy time keeps running, the
            // session's task and events flow - and real light sleep
            // resumes on the first pass after the hold is released.
            // Deliberately BEFORE enter_light_sleep so none of its
            // wake diagnostics (cycle counts, slept-ms probes,
            // summary lines) fire for these idle passes.
            if crate::bus::wake_held() {
                match select(
                    EVENTS.receive(),
                    Timer::after(Duration::from_millis(500)),
                ).await {
                    Either::First(event) => self.handle_event(event).await,
                    Either::Second(_) => {}
                }
                // Same hole as the post-light-sleep path below: if
                // that event woke the UI, this pass returns before
                // the tail render and the next pass blocks on its
                // select for up to a full idle tick - paint the wake
                // frame before returning.
                if !self.model.sleeping() && self.model.needs_redraw() {
                    self.board.set_cpu_freq(CpuFreq::Mhz160);
                    self.render().await;
                    self.sync_canvas();
                    self.board.set_cpu_freq(CpuFreq::Mhz80);
                }
                return;
            }
            let (wake_cause, slept_ms) = self.enter_light_sleep().await;
            let gpio_wake = wake_cause & Self::WAKE_CAUSE_GPIO != 0;
            // WAKE PROBE - remove after the wake-gap diagnosis.
            log::info!("wakeprobe: rtc {} ms - light sleep returned", self.rtc_now_us() / 1000);

            // Kick the RTC task to check for an alarm / timer flag
            // latched while we slept. Boards with no RTC INT line (the
            // C6) can't wake on expiry at all, and the RTC task's own
            // software poll is embassy-timed - so it's frozen across
            // light sleep and never fires on its own here. This makes
            // detection happen on every heartbeat instead. Harmless on
            // boards that woke via the RTC GPIO: the flag is read and
            // cleared exactly once, whichever path reaches it first.
            RTC_COMMAND.signal(RtcCommand::Poll);

            // CPU just woke. The wake-source ISR marked its owning
            // task ready but the task hasn't run yet, so `try_receive`
            // here would race ahead of it. Wait with a short timeout
            // for the first event, then drain any follow-ups. Timer
            // wake with nothing else happening just falls through.
            match select(
                EVENTS.receive(),
                Timer::after(Duration::from_millis(50)),
            ).await {
                Either::First(event) => self.handle_event(event).await,
                Either::Second(_) => {}
            }
            while let Ok(event) = EVENTS.try_receive() {
                self.handle_event(event).await;
            }

            // A wake line fired but no task delivered an event for it
            // (a short tap: the touch-INT pulse ended before the
            // touch task could service it, so nothing above woke the
            // model). Don't lose the interaction back into sleep -
            // synthesize the wake. Concrete events win when they
            // arrive; this only fires when none did.
            if gpio_wake && self.model.sleeping() {
                // WAKE PROBE - remove after the wake-gap diagnosis.
                log::info!("wakeprobe: rtc {} ms - no event within 50 ms, synthesizing", self.rtc_now_us() / 1000);
                self.handle_event(SystemEvent::WakeInterrupt).await;
            }

            // Late re-log of the wake diagnostics: by now any wake
            // transition has run and the USB host has resynced, so
            // this line survives where the in-`enter_light_sleep`
            // one gets eaten. A wake that stays dark after LESS than
            // a full heartbeat is the anomaly under investigation
            // (interrupted sleep attributed to the timer) and has no
            // display transition to outlast the USB blackout - buy
            // it the time explicitly (diagnosis probe; remove with
            // the other probes).
            if self.model.sleeping() && slept_ms < 4500 {
                Timer::after(Duration::from_millis(300)).await;
            }
            // `rtc` is the absolute RTC-domain clock in ms - unlike
            // embassy time it keeps counting through light sleep, so
            // the line-to-line delta shows the true wall-clock
            // cadence of the heartbeat (discriminates "sleeps are
            // really ~0 ms" from "the slept probe is lying while the
            // chip sleeps fine").
            log::info!(
                "light_sleep: summary (cycle {}, cause {:#b}, slept {} ms, rtc {} ms, {})",
                self.sleep_cycles,
                wake_cause,
                slept_ms,
                self.rtc_now_us() / 1000,
                if self.model.sleeping() { "staying asleep" } else { "woke UI" },
            );
            // A UI wake's line above still races the USB resync -
            // stash a copy for the first awake tick (see the field
            // doc).
            if !self.model.sleeping() {
                self.pending_wake_summary =
                    Some((self.sleep_cycles, wake_cause, slept_ms));
                // Paint the wake frame NOW. This pass returns before
                // the render at the tail of the awake path, and the
                // next pass blocks on the event select for up to a
                // full idle tick - without this the panel sits on the
                // stale pre-sleep canvas for that second. Time may be
                // one heartbeat (<= 5 s) stale in this frame; the
                // next TimeUpdated tick repaints it.
                if self.model.needs_redraw() {
                    // WAKE PROBE - remove after the wake-gap diagnosis.
                    log::info!("wakeprobe: rtc {} ms - wake render start", self.rtc_now_us() / 1000);
                    self.board.set_cpu_freq(CpuFreq::Mhz160);
                    self.render().await;
                    log::info!("wakeprobe: rtc {} ms - wake render done", self.rtc_now_us() / 1000);
                    self.sync_canvas();
                    self.board.set_cpu_freq(CpuFreq::Mhz80);
                    log::info!("wakeprobe: rtc {} ms - canvas sync done", self.rtc_now_us() / 1000);
                }
            }
            return;
        }

        // Awake path: wait for events with an idle-tick heartbeat so
        // the Model's dim / idle-sleep timers keep advancing even if
        // nothing is happening. 1 s is plenty - thresholds are in
        // multi-second territory.
        const IDLE_TICK: Duration = Duration::from_secs(1);
        match select(EVENTS.receive(), Timer::after(IDLE_TICK)).await {
            Either::First(event) => {
                self.handle_event(event).await;
                // Drain the whole backlog BEFORE rendering, so the one
                // frame below reflects the latest state instead of
                // replaying history one frame per stale event. Touch
                // drags produce position samples several times faster
                // than a frame renders; handling one event per frame
                // let the 32-slot channel back up, which showed as
                // ~1 s of scroll lag plus a phantom "glide" after
                // finger lift (the queue finishing its replay).
                // Event handling is micro-seconds; the producers are
                // rate-limited by their hardware, so this loop always
                // empties quickly and cannot starve the render.
                while let Ok(event) = EVENTS.try_receive() {
                    self.handle_event(event).await;
                }
            }
            Either::Second(_) => {} // idle heartbeat
        }

        // Advance time-driven Model state (buzz phase, dim/idle
        // sleep transitions) and execute the resulting effects.
        // `wall_uptime_secs` is the SoC RTC counter rebaselined to this
        // boot (`boot_uptime_secs`); it survives light sleep, unlike
        // `Instant::now()` which pauses. `Model::tick` uses each for its
        // respective snapshot field. `saturating_sub` is belt-and-braces
        // - the counter is monotonic so it can't actually underflow.
        let wall_uptime_secs = (self.rtc.time_since_power_up().as_secs() as u32)
            .saturating_sub(self.boot_uptime_secs);
        let effects = self.model.tick(Instant::now(), wall_uptime_secs);
        self.model.set_sleep_telemetry(self.sleep_cycles);
        self.execute_effects(effects).await;

        // SD hotplug / recovery: detect-line boards notice inserts
        // AND removals here; detect-less boards fall back to the
        // throttled blind re-probe (a probe involves a full
        // CMD0/CMD8/ACMD41 sequence + MBR read - hence the
        // SD_RECOVER_INTERVAL throttle on that path).
        self.service_sd_slot().await;

        if !self.model.sleeping() && self.model.needs_redraw() {
            self.board.set_cpu_freq(CpuFreq::Mhz160);
            self.render().await;
            // The frame above may have been the tail of an
            // interaction - re-mirror any deferred canvas tiles while
            // the clock is still raised. No-op unless tiles are stale
            // and the finger is up.
            self.sync_canvas();
            self.board.set_cpu_freq(CpuFreq::Mhz80);
        } else if self.canvas_stale != 0
            && !self.touch_active
            && !self.model.sleeping()
        {
            // Interaction ended without a final redraw (plain
            // release) - the deferred mirror work is all that's left.
            self.board.set_cpu_freq(CpuFreq::Mhz160);
            self.sync_canvas();
            self.board.set_cpu_freq(CpuFreq::Mhz80);
        }

        // Deferred re-log of a UI wake's diagnostics (see the field
        // doc): by the end of the first awake tick the USB host has
        // resynced, so this copy survives where the summary printed
        // at the wake itself gets shredded.
        if let Some((cycle, cause, slept)) = self.pending_wake_summary.take() {
            log::info!(
                "light_sleep: wake summary (cycle {}, cause {:#b}, slept {} ms)",
                cycle,
                cause,
                slept,
            );
        }

        self.log_diagnostics();
        self.tick_count = self.tick_count.wrapping_add(1);
        self.model.set_tick_count(self.tick_count);
    }

    /// Render the active screen using per-tile rendering and dirty-tile
    /// detection. Only runs when awake and `Model::needs_redraw()` is true.
    ///
    /// The FB covers `TILE_H` rows. Each iteration parks the FB at a
    /// different `tile_y` along the panel, clears it, lets the screen
    /// draw at panel-absolute coordinates (the CO5300 driver translates
    /// + clips), hashes the rendered tile, and only pushes the tile if
    /// its hash differs from last frame. The loop is board-agnostic: the
    /// only thing that varies is whether DMA from the bus's TX buffer is
    /// pipelined with the next tile's CPU work, which is decided inside
    /// the bus implementation, not here.
    ///
    /// Boards with PSRAM additionally provide `fb_canvas`, a full-panel
    /// pixel store: every pushed tile is mirrored into it with one
    /// sequential copy (PSRAM's fast access pattern), so the canvas
    /// always holds what the panel shows and compositing effects can
    /// read it. While a finger is on the glass the mirror is deferred
    /// (`canvas_stale` / `sync_canvas()`): quad-PSRAM boards pay
    /// ~6-8 ms per mirrored tile, which made interactive scroll frames
    /// ~115-130 ms vs ~47 ms unmirrored. Compositing only reads the
    /// canvas from rest states, so re-syncing at the first quiet tick
    /// keeps the contract. Rendering INTO the canvas directly was
    /// tried and measured at ~130 ms per settings-scroll frame (vs
    /// ~47 ms tiled) - external RAM pays per access, so all drawing
    /// stays in the internal staging tile. The canvas deliberately does NOT need to
    /// survive light sleep (its rail may be gated): sleep is only
    /// entered with the display off, and every wake sets
    /// `force_full_redraw`, which re-renders + re-mirrors everything.
    async fn render(&mut self) {
        let render_start = Instant::now();
        // Clone the cache so we can freely borrow `&mut self.model`
        // (for `screen_mut()`) alongside `&mut self.display` below.
        // `SystemData` was `Copy` before `battery_history` landed;
        // the per-frame clone cost is ~400 bytes, well below the
        // render-time budget at any realistic frame cadence.
        let data = self.model.cached_data().clone();
        let battery_pct = data.power.battery_percent;

        // Decide which tiles need to be rendered this frame.
        //
        // `force_full_redraw` (set after wake / display-power-on / boot)
        // overrides the screen's opinion: the panel's GRAM is stale and
        // the screen's "last rendered" snapshot can't be trusted, so we
        // repaint everything.
        //
        // Otherwise the active screen describes the regions it knows
        // would differ from what's already on screen via `dirty_rects`.
        // Screens that don't override the default return `FullScreen`
        // and behave like the pre-invalidation renderer.
        let dirty = if self.force_full_redraw {
            self.force_full_redraw = false;
            DirtyRegion::FullScreen
        } else {
            self.model.screen_mut().dirty_rects(&data)
        };
        let tile_mask = dirty_to_tile_mask(&dirty);
        // On FullScreen frames (scroll, wake, screen-switch) we expect
        // every rendered tile's content to differ from the panel, so
        // hashing is wasted work - just push everything. The hash is
        // only profitable for sparse-dirty frames where some tiles
        // happen to land on identical pixels (e.g. seconds-tick area
        // overlapping a static LAT/LON row). One trade: the frame
        // *after* a FullScreen push has stale `tile_hashes`, so it may
        // push tiles needlessly if it's a partial-dirty frame. Worth
        // ~17 ms during continuous scroll for one ~2 ms tax on the
        // first post-scroll frame.
        let skip_hash = matches!(dirty, DirtyRegion::FullScreen);

        // Empty dirty region -> nothing to do. Still clear `needs_redraw`
        // and update the screen's snapshot so the next on_event with no
        // visual effect doesn't keep waking us up.
        if tile_mask == 0 {
            self.model.screen_mut().clear_dirty(&data);
            self.model.clear_redraw();
            return;
        }

        let mut waited_for_te = false;

        for tile_idx in 0..NUM_TILES {
            if tile_mask & (1u16 << tile_idx) == 0 {
                // Not in this frame's dirty set - skip render, skip hash,
                // skip push. The previous frame's contents stay on the
                // panel; our `tile_hashes[tile_idx]` is the hash of
                // whatever we last rendered into this tile and remains
                // valid for next-frame comparison.
                continue;
            }

            let tile_y = (tile_idx as u16) * TILE_H;
            // Short final tile clips to HEIGHT; the rest are full TILE_H.
            let tile_h = (HEIGHT - tile_y).min(TILE_H);
            let ctx = RenderCtx { tile_y, tile_h };

            // Park the FB at this tile, clear it, render the whole UI.
            // The screen looks at `ctx` to skip widget setup for things
            // entirely outside this tile (settings list rows, etc.);
            // off-tile widgets that the screen *did* try to draw still
            // get rejected per-pixel inside the driver.
            self.display.set_tile_y(tile_y);
            self.display.clear(app_core::ui::theme::BG).ok();

            self.model.screen_mut().render(&mut self.display, &data, &ctx);
            if let Some(pct) = battery_pct {
                primitives::battery_warning_frame(&mut self.display, pct, &data.safe_area);
            }

            // Decide whether this tile needs to go over QSPI. For
            // `FullScreen` dirty frames we already know the answer (yes)
            // - skip the hash entirely. For sparse-dirty frames hash and
            // compare; tiles whose pixels happen to match the last frame
            // (e.g. an unchanged LAT/LON row in a seconds-tick) don't
            // need to be pushed.
            let should_push = if skip_hash {
                // Zero the stored hash so the *next* sparse-dirty frame's
                // hash compare sees a mismatch (any nonzero hash) and
                // re-pushes correctly. Worst case: one needless push per
                // tile on the first post-FullScreen frame.
                self.tile_hashes[tile_idx] = 0;
                true
            } else {
                // Hash only the panel-visible portion of the FB. For the
                // short final tile (e.g. 2 rows on a 502 / 50 layout) the
                // unused tail of the FB stays at BG from the clear, but
                // hashing it would just waste cycles.
                let panel_rows = (HEIGHT - tile_y).min(TILE_H) as usize;
                let visible_bytes = panel_rows * ROW_STRIDE;
                let h = fb_hash(&self.display.framebuffer()[..visible_bytes]);
                if h != self.tile_hashes[tile_idx] {
                    self.tile_hashes[tile_idx] = h;
                    true
                } else {
                    false
                }
            };

            if should_push {
                // Wait for TE rising edge (vblank) before the first push
                // of this frame. Subsequent tiles ride the same vblank
                // window since a 60 Hz panel gives us ~16 ms before
                // scanout begins. Timeout at ~2 refresh periods in case
                // TE is silent. Boards with no TE GPIO (`lcd_te:
                // None`) skip the wait entirely - zero delay, no
                // timeout penalty; nothing to sync to anyway.
                if !waited_for_te {
                    if let Some(te) = &mut self.lcd_te {
                        let _ = with_timeout(
                            Duration::from_millis(30),
                            te.wait_for_rising_edge(),
                        ).await;
                    }
                    waited_for_te = true;
                }
                self.display.flush_tile().await;

                // Mirror the pushed tile into the full-panel canvas
                // (PSRAM boards only). One sequential copy per tile -
                // the access pattern external RAM is fast at - and it
                // overlaps the tile's still-running flush DMA, which
                // reads from the bus's own TX buffer, not this FB.
                // Mirroring only pushed tiles keeps the canvas exactly
                // in sync with the panel: a hash-skipped tile was
                // mirrored when it was last pushed. During touch
                // interaction the copy is deferred to `sync_canvas()`
                // - the tile is only marked stale here.
                if let Some(canvas) = self.fb_canvas.as_deref_mut() {
                    let bit = 1u16 << tile_idx;
                    if self.touch_active {
                        self.canvas_stale |= bit;
                    } else {
                        let rows = (HEIGHT - tile_y).min(TILE_H) as usize;
                        let start = tile_y as usize * ROW_STRIDE;
                        let bytes = rows * ROW_STRIDE;
                        canvas[start..start + bytes]
                            .copy_from_slice(&self.display.framebuffer()[..bytes]);
                        self.canvas_stale &= !bit;
                    }
                }
            }
        }

        // Drain any DMA that's still in flight from the last tile's
        // flush. With pipelined `EspQspi`, `flush_tile().await` returns
        // before the SPI transfer is finished; this awaits the final
        // tile's DMA-complete interrupt so the bus is idle by the time
        // `render` returns and other code paths (display sleep, sleep
        // power transitions) can talk to the panel.
        self.display.flush_pending().await;

        // Update the screen's "last rendered" snapshot now that the
        // frame is on the panel - the next dirty_rects call diffs from
        // this baseline.
        self.model.screen_mut().clear_dirty(&data);
        self.model.clear_redraw();

        let render_ms = render_start.elapsed().as_millis();
        if render_ms > 10 {
            log::info!("render: {}ms", render_ms);
        }
    }

    /// Re-mirror canvas tiles whose copy was deferred while a finger
    /// was on the glass. The staging FB is transient (every tile
    /// overwrites it), so a deferred tile can't be copied after the
    /// fact - it is re-rendered here and then mirrored. Runs only at
    /// rest (no touch, no pending redraw, awake), where the model
    /// state still matches what `render()` last pushed, so the
    /// repaint reproduces the panel's pixels exactly. Pure CPU +
    /// PSRAM work: nothing is pushed to the panel and `tile_hashes`
    /// stay untouched (same pixels, same hashes).
    fn sync_canvas(&mut self) {
        if self.canvas_stale == 0
            || self.fb_canvas.is_none()
            || self.touch_active
            || self.model.sleeping()
            || self.model.needs_redraw()
        {
            return;
        }
        let sync_start = Instant::now();
        let data = self.model.cached_data().clone();
        let battery_pct = data.power.battery_percent;

        for tile_idx in 0..NUM_TILES {
            if self.canvas_stale & (1u16 << tile_idx) == 0 {
                continue;
            }
            let tile_y = (tile_idx as u16) * TILE_H;
            let tile_h = (HEIGHT - tile_y).min(TILE_H);
            let ctx = RenderCtx { tile_y, tile_h };

            self.display.set_tile_y(tile_y);
            self.display.clear(app_core::ui::theme::BG).ok();
            self.model.screen_mut().render(&mut self.display, &data, &ctx);
            if let Some(pct) = battery_pct {
                primitives::battery_warning_frame(&mut self.display, pct, &data.safe_area);
            }

            let canvas = self.fb_canvas.as_deref_mut().unwrap();
            let start = tile_y as usize * ROW_STRIDE;
            let bytes = tile_h as usize * ROW_STRIDE;
            canvas[start..start + bytes]
                .copy_from_slice(&self.display.framebuffer()[..bytes]);
        }
        self.canvas_stale = 0;
        log::info!("canvas sync: {}ms", sync_start.elapsed().as_millis());
    }

    /// Log heap watermark every ~2000 loop iterations. Useful
    /// for spotting allocation churn in the hot path.
    fn log_diagnostics(&self) {
        if self.tick_count % 2000 == 0 {
            log::info!(
                "heap: used={} free={}",
                esp_alloc::HEAP.used(),
                esp_alloc::HEAP.free(),
            );
        }
    }
}

/// The per-board boot-construction seam.
///
/// Implemented once per bin. Peripheral construction names
/// chip-specific esp-hal types that only exist in that bin's build,
/// so it is irreducibly per-bin - but the *sequence* (ordering, the
/// `&mut i2c` borrow choreography, snapshots, `SystemManager::new`,
/// task spawn, the loop) lives once in [`run`]. Adding a board = one
/// `Bringup` impl; the orchestration is never duplicated.
///
/// The impl holds its raw peripheral tokens - esp-hal singletons
/// can't be partial-moved through `&mut self`, so typically as
/// `Option<_>` `.take()`-n in each method. `esp_hal::init` / heap /
/// `esp_rtos` / logger stay in the bin's `main` (macros/attributes
/// produce statics); `main` then builds the `Bringup` and calls
/// [`run`].
// The async methods never need `Send`: their futures are polled on a
// single embassy executor (one core, esp-rtos) and never cross
// threads, so the `async_fn_in_trait` Send-bound caveat doesn't apply
// here. Consciously suppressed (see the AFIT constraint analysis).
#[allow(async_fn_in_trait)]
pub trait Bringup {
    type Board: Board;

    /// Build the shared I2C bus (board-specific pins, uniform type).
    fn make_i2c(&mut self) -> I2c<'static, Blocking>;

    /// Bring up power/PMU - the first peripheral. Returns the board
    /// glue (`Board` impl) and the power task state (the `Pmu`
    /// handle wrapped for polling).
    fn make_power(
        &mut self,
        i2c: &mut I2c<'static, Blocking>,
    ) -> (Self::Board, PowerTaskState);

    /// Board-specific wait before touch I2C (e.g. the FT3168
    /// wake-from-MONITOR poll). Default: nothing.
    async fn wait_for_peripherals(
        &mut self,
        _i2c: &mut I2c<'static, Blocking>,
    ) {
    }

    /// Build the display (takes the framebuffer from the shared HAL).
    /// Receives the loaded settings tree so the panel's init sequence
    /// programs the stored brightness - hardware initializes from
    /// persisted settings, never from hardcoded defaults.
    async fn make_display(&mut self, config: &Config) -> Display<'static>;

    /// The tearing-effect input, when the board routes one to a GPIO.
    fn make_lcd_te(&mut self) -> Option<Input<'static>>;

    /// The full-panel pixel canvas, on boards that carry PSRAM. Called
    /// after [`make_display`](Bringup::make_display) (boards typically
    /// map PSRAM during display bring-up). The render loop mirrors
    /// every pushed tile into it; `None` (the default) means tile-only
    /// rendering with no mirror - the correct answer for boards
    /// without external RAM.
    fn take_fb_canvas(&mut self) -> Option<&'static mut [u8]> {
        None
    }

    /// Touch + BOOT-button task states; also arms their wake GPIOs.
    async fn make_input(
        &mut self,
        i2c: &mut I2c<'static, Blocking>,
    ) -> (TouchTaskState<'static>, BootButtonTaskState<'static>);

    /// Persistent storage (flash, plus SD where the board has a slot).
    fn make_store(&mut self) -> crate::storage::Store<'static>;

    /// RTC + IMU task states.
    async fn make_sensors(
        &mut self,
        i2c: &mut I2c<'static, Blocking>,
    ) -> (RtcTaskState<'static>, ImuTaskState<'static>);

    /// The esp-hal RTC controller (used for hardware light sleep).
    fn make_rtc_ctrl(&mut self) -> esp_hal::rtc_cntl::Rtc<'static>;

    /// Spawn the board's audio task, where the board has a speaker.
    /// Handed the executor and the shared I2C bus (the codec sits on
    /// it). The task owns the board's I2S peripheral, a DMA channel and
    /// the speaker GPIOs - all chip-specific concrete types - so the
    /// spawn lives in the bin (like the per-board pin structs), not in
    /// the generic orchestrator. It drives the alarm / timer alert tone
    /// in response to `AUDIO_COMMAND`, bringing the codec up lazily on
    /// the first tone. Default: no-op for boards without a speaker.
    fn spawn_audio(
        &mut self,
        _spawner: embassy_executor::Spawner,
        _i2c_bus: &'static crate::bus::SharedI2c,
    ) {
    }

    /// Which optional hardware this board carries. Cached into
    /// `SystemData` before the model is built; the shared UI gates
    /// board-specific rows/views on it. Default: none - a board
    /// declares only what it actually spawns a task for.
    fn capabilities(&self) -> app_core::data::Capabilities {
        app_core::data::Capabilities::default()
    }

    /// Panel pixels physically masked by this device's case/bezel,
    /// measured per board (bezel-ruler probe). Cached into
    /// `SystemData`; only edge-hugging chrome consumes it. Default:
    /// zero - correct for bezels that mask nothing that matters.
    fn safe_area(&self) -> app_core::data::SafeArea {
        app_core::data::SafeArea::default()
    }
}

/// Shared boot orchestration: build every piece via the board's
/// [`Bringup`] in the canonical order, assemble the manager, spawn
/// the per-device tasks, and run the event loop forever. Identical
/// across boards, so it lives exactly once.
pub async fn run<T: Bringup>(
    mut bringup: T,
    spawner: embassy_executor::Spawner,
) -> ! {
    use crate::tasks::{
        boot_button::boot_button_task, imu::imu_task, power::power_task,
        rtc::rtc_task, touch::touch_task,
    };

    // Construction sequence. Order matters: I2C first (shared bus),
    // power the first peripheral (enables rails), snapshots taken
    // while we still hold raw `&mut i2c` - before it moves into the
    // global mutex.
    let mut i2c = bringup.make_i2c();

    let (board, power_state) = bringup.make_power(&mut i2c);
    // Post-PMU rail settle.
    Timer::after(Duration::from_millis(20)).await;

    bringup.wait_for_peripherals(&mut i2c).await;

    // Settings FIRST: everything below initializes from the stored
    // tree (config-first boot). An unreadable/missing blob defaults
    // and is immediately re-saved in the current format, so a failed
    // load is a one-time event per device (format cut-over or fresh
    // flash), never a recurring one.
    let mut store = bringup.make_store();
    let stored = store.load_blob::<Config>(CONFIG_PATH, CONFIG_VERSION);
    let loaded = stored.is_some();
    let config = stored.unwrap_or_else(Config::default);
    if !loaded {
        store.save_blob(CONFIG_PATH, CONFIG_VERSION, &config);
    }
    log::info!(
        "config: {}",
        if loaded { "loaded" } else { "default (re-saved)" },
    );

    let display = bringup.make_display(&config).await;
    let fb_canvas = bringup.take_fb_canvas();
    let lcd_te = bringup.make_lcd_te();
    let (touch, boot_button) = bringup.make_input(&mut i2c).await;
    let (rtc_state, imu) = bringup.make_sensors(&mut i2c).await;

    let initial_time = rtc_state.snapshot(&mut i2c);
    let initial_power = power_state.snapshot(&mut i2c);
    power_state.dump_status(&mut i2c);

    let i2c_bus: &'static crate::bus::SharedI2c =
        crate::bus::I2C_BUS.init(embassy_sync::mutex::Mutex::new(i2c));

    let rtc = bringup.make_rtc_ctrl();

    // Assemble + spawn + run - board-agnostic, single-sourced.
    let (mut manager, bundle) = SystemManager::new(SystemParts {
        i2c_bus,
        board,
        display,
        fb_canvas,
        lcd_te,
        rtc,
        store,
        initial_time,
        initial_power,
        touch,
        boot_button,
        rtc_state,
        imu,
        power: power_state,
        capabilities: bringup.capabilities(),
        safe_area: bringup.safe_area(),
        config,
    });

    // Each task is spawned exactly once at boot; `.unwrap()` on the
    // task fn is the "must succeed" shape (embassy-executor 0.10
    // returns a `Result<SpawnToken, SpawnError>` from the task macro).
    spawner.spawn(touch_task(i2c_bus, bundle.touch).unwrap());
    spawner.spawn(boot_button_task(bundle.boot_button).unwrap());
    spawner.spawn(rtc_task(i2c_bus, bundle.rtc).unwrap());
    spawner.spawn(imu_task(i2c_bus, bundle.imu).unwrap());
    spawner.spawn(power_task(i2c_bus, bundle.power).unwrap());
    // Board-specific: the speaker task, where the board has one. Owns
    // the I2S / DMA / speaker pins; lazy bring-up on the first tone.
    bringup.spawn_audio(spawner, i2c_bus);

    loop {
        manager.tick().await;
    }
}
