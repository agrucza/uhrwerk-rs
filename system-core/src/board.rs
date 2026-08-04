//! The board seam.
//!
//! The small set of genuinely per-board operations the shared
//! `SystemManager` invokes. Each bin crate provides exactly one
//! `Board` impl; pins, peripheral construction and the partition
//! geometry never cross this trait - they're constructed in the bin
//! and passed to `SystemManager::new`. The manager is written once,
//! generic over `B: Board`, and is otherwise board-blind.
//!
//! Kept deliberately tiny (verified against the manager's actual
//! board-specific call sites): haptic motor, power-off, wake-source
//! arming, CPU-frequency scaling, light-sleep config tuning - the
//! last two because the clock/sleep registers differ per chip family
//! (e.g. the s3 `SYSTEM` block vs the c6 `PCR` block) - and the touch
//! controller's sleep/wake transitions, because how (and whether) a
//! controller survives light sleep is a property of the chip the
//! board carries. The manager owns everything else - `rtc.sleep()`,
//! the render/event loop, and the *policy* of when to scale/sleep -
//! because none of that is board-specific.

/// CPU clock levels the manager scales between. A board-agnostic
/// concept; the actual register sequence to reach each level is
/// chip-specific and lives behind [`Board::set_cpu_freq`]. The
/// manager only uses `Mhz80` (idle/sleep baseline) and `Mhz160`
/// (render boost) at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuFreq {
    /// 80 MHz - baseline for idle / pre-sleep.
    Mhz80,
    /// 160 MHz - render boost. Highest level on chips capped here.
    Mhz160,
    /// 240 MHz - not reached at runtime by the manager, and not
    /// achievable on every chip (e.g. the c6 tops out at 160). A
    /// board may clamp this to its maximum.
    #[allow(dead_code)]
    Mhz240,
}

/// Per-board operations the shared manager calls.
pub trait Board {
    /// Start the haptic motor. Boards without one: no-op.
    fn buzz(&mut self);

    /// Stop the haptic motor. Boards without one: no-op.
    fn buzz_stop(&mut self);

    /// Power the board off. Either releases a soft-power latch or
    /// hands off to a PMU that manages long-press shutdown itself.
    fn shutdown(&mut self);

    /// Arm this board's hardware wake sources, synchronously, right
    /// before the manager enters light sleep. The manager owns *when*
    /// to sleep and the actual `rtc.sleep()` call (all
    /// board-agnostic); the board only declares *what wakes it* -
    /// which GPIO wake bits / internal timer to enable. Sync on
    /// purpose: no async-fn-in-trait across the two toolchains.
    fn arm_wake_sources(&mut self);

    /// Raw value of the chip's sleep-wakeup-cause register, read by
    /// the manager right after `rtc.sleep()` returns. The register
    /// differs per family (s3 `LPWR.slp_wakeup_cause` vs c6
    /// `PMU.slp_wakeup_status0`) - hence a board hook, like
    /// `set_cpu_freq` - but the bit layout the manager relies on is
    /// uniform: GPIO wake = 1 << 2 on every family we run, and the
    /// manager owns that interpretation. Returning the raw bits (not
    /// a pre-masked bool) also keeps the full cause visible in the
    /// wake diagnostics.
    ///
    /// esp-hal's `rtc_cntl::wakeup_cause()` cannot be used for this:
    /// it guards on reset-reason == deep-sleep-wake, and light-sleep
    /// wake is not a reset, so it always returns `Undefined` here
    /// (upstream-report candidate).
    fn wake_cause_raw(&self) -> u32;

    /// Put the touch controller into its low-power state. The manager
    /// calls this synchronously inside `enter_light_sleep`, bus lock
    /// held, immediately before `rtc.sleep()` - serialized there (not
    /// in the touch task off SLEEP_WATCH) so the chip is guaranteed
    /// in the right mode before the CPU gates off. Called again on
    /// every heartbeat re-entry while the system stays asleep; a
    /// board whose transition isn't idempotent-cheap must guard.
    ///
    /// Default: the FT3168 write - Monitor mode, a low-power scan
    /// that auto-returns to Active on touch and drives INT# low, so
    /// touch remains a wake source. Best-effort: if it NAKs (chip
    /// mid-transition after a recent touch) the chip self-manages to
    /// low power and the other wake sources still work.
    fn touch_sleep(
        &mut self,
        i2c: &mut esp_hal::i2c::master::I2c<'static, esp_hal::Blocking>,
    ) {
        use drivers::touch;
        let _ = i2c.write(
            touch::ADDR,
            &[touch::REG_POWER_MODE, touch::PowerMode::Monitor as u8],
        );
    }

    /// Bring the touch controller back to full operation on wake -
    /// the real, user-facing wake (`BroadcastSleep(Awake)`), not the
    /// 5 s heartbeat.
    ///
    /// Default: nothing - the FT3168 leaves Monitor mode by itself on
    /// the first touch. Boards whose controller needs host action to
    /// leave its sleep state (e.g. a reset pulse) override this.
    fn touch_wake(
        &mut self,
        i2c: &mut esp_hal::i2c::master::I2c<'static, esp_hal::Blocking>,
    ) {
        let _ = i2c;
    }

    /// Read the SD card-detect line, where the board has one:
    /// `Some(true)` = card physically present, `Some(false)` = slot
    /// empty, `None` = no detect line (the manager then falls back
    /// to blind throttled probing when the mirror is offline). The
    /// line is what makes hotplug cheap: an empty slot is one bus
    /// read instead of a seconds-long failed probe, and a yanked
    /// card is noticed without waiting for a write to fail. Called
    /// with the bus lock held - keep it to a single cheap
    /// transaction.
    fn sd_detect(
        &mut self,
        i2c: &mut esp_hal::i2c::master::I2c<'static, esp_hal::Blocking>,
    ) -> Option<bool> {
        let _ = i2c;
        None
    }

    /// Switch the CPU clock to `freq`. The chip's clock registers
    /// differ per family, so the sequence lives here. A board may
    /// clamp a level it can't reach (e.g. `Mhz240` -> its max). The
    /// manager owns the *policy* (boost for render, drop for idle/
    /// sleep); the board owns the *poke*.
    fn set_cpu_freq(&mut self, freq: CpuFreq);

    /// Apply this board's chip-specific reliability tuning to the
    /// `RtcSleepConfig` the manager is about to sleep with. The
    /// available knobs differ per chip family (the s3 fpu/reject
    /// fields don't exist on the c6 `RtcSleepConfig`), so each board
    /// tunes what its silicon supports; the manager owns the default
    /// config, the wake sources, and the `rtc.sleep()` call.
    fn tune_sleep_config(
        &self,
        cfg: &mut esp_hal::rtc_cntl::sleep::RtcSleepConfig,
    );
}
