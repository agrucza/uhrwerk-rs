//! The C6 `system_core::board::Board` impl + AXP2101 sanity check.
//!
//! Mirrors `firmware-s3/src/system/power.rs` in role, but this board
//! has no SYS_OUT latch GPIO and no haptic motor: the AXP2101 manages
//! long-press shutdown internally and PWR is read via the inverting
//! MOSFET on GPIO18 (board.rs). So `shutdown` / `buzz` / `buzz_stop`
//! are no-ops. `arm_wake_sources` arms this board's wake GPIOs (touch
//! INT GPIO15, BOOT GPIO9); there is no RTC_INT pin here - periodic
//! RTC-cadence wake comes from the manager's internal timer source.
//!
//! AXP rails are NOT configured: the AXP retains rail state across
//! MCU resets and the reference firmware never enables them in
//! software. We `check_device` for reachability and `enable_all_adc`
//! (the board-independent ADC channels - vbus/vsys/die-temp - which
//! touch no rails), but skip the rail config. The returned `Pmu` goes
//! to the shared power task.

use drivers::pmu::{
    ChargeCurrent, ChargeVoltage, Config as PmuConfig, Pmu, PowerOffVoltage,
    TerminationCurrent,
};
use embedded_hal::i2c::I2c;
use esp_hal::peripherals::GPIO;
use system_core::board::{Board, CpuFreq};

/// Zero-sized board glue for the C6. All board-specific manager
/// operations are no-ops or register pokes; there is no per-board
/// state to hold.
pub struct C6Board;

impl C6Board {
    /// Sanity-check the AXP2101 (no rail config - it retains state
    /// across resets). Returns `(C6Board, Pmu)`; the caller wraps the
    /// `Pmu` in the shared `PowerTaskState`.
    pub fn init(i2c: &mut impl I2c) -> (Self, Pmu) {
        let pmu = Pmu::new(PmuConfig::default());
        match pmu.check_device(i2c) {
            Ok(chip_id) => log::info!(
                "AXP2101 chip ID: 0x{:02X} (rev {:02b})",
                chip_id, (chip_id >> 4) & 0x03,
            ),
            Err(_) => log::error!("AXP2101 check_device failed"),
        }

        // Enable the AXP2101 ADC channels (vbus/vsys/die-temp/TS/batt).
        // Board-independent - the same channels the S3 enables - and it
        // touches no rails, so it's safe even though we otherwise trust
        // the AXP's persisted rail state here. Without it, vbus/vsys/
        // die-temp read 0.
        if pmu.enable_all_adc(i2c).is_err() {
            log::error!("AXP2101: enable_all_adc failed");
        }
        // Fuel gauge + battery detection explicitly on (idempotent
        // RMW). S3 gets this via Pmu::init and the watch calls it
        // directly; this board was the only one trusting the AXP's
        // persisted state for it.
        if pmu.enable_battery_monitor(i2c).is_err() {
            log::error!("AXP2101: enable_battery_monitor failed");
        }

        // Charge + power-off profile for the board's 400 mAh cell -
        // same cell and reasoning as the S3 (see its power.rs): the
        // power-on defaults leave charging unconfigured and VOFF at
        // 2.6 V, which deep-discharges the cell and wipes the fuel
        // gauge's learned state on every collapse.
        let profile_ok = pmu.set_charge_voltage(i2c, ChargeVoltage::V4_2).is_ok()
            && pmu.set_charge_current(i2c, ChargeCurrent::from_ma(100)).is_ok()
            && pmu
                .set_termination_current(i2c, TerminationCurrent::from_ma(25), true)
                .is_ok()
            && pmu
                .set_power_off_voltage(i2c, PowerOffVoltage::from_mv(3200))
                .is_ok();
        if !profile_ok {
            log::warn!("PMU: charge profile configuration failed");
        }
        // Read BACK from silicon - the log must show what the chip
        // holds, not what we requested (a wrong ChargeCurrent
        // register mapping once wore the requested values as a
        // label; requested here: 4.2 V / 100 mA / 25 mA / 3200 mV).
        log::info!(
            "PMU: charge profile readback: CV {:?}, CC {:?} mA, term {:?} mA, VOFF {:?} mV",
            pmu.charge_voltage(i2c).ok().flatten(),
            pmu.charge_current(i2c).ok().map(|c| c.as_ma()),
            pmu.termination_current(i2c).ok().map(|(t, en)| (t.as_ma(), en)),
            pmu.power_off_voltage(i2c).ok().map(|v| v.as_mv()),
        );

        (Self, pmu)
    }
}

impl Board for C6Board {
    /// No haptic motor on this board.
    fn buzz(&mut self) {}

    /// No haptic motor on this board.
    fn buzz_stop(&mut self) {}

    /// No SYS_OUT latch GPIO: the AXP2101 handles long-press
    /// shutdown internally. Nothing for firmware to do.
    fn shutdown(&mut self) {
        log::info!("PWR: shutdown is AXP2101-managed on this board (no-op)");
    }

    /// Re-arm the C6 wake GPIOs (BOOT GPIO9, touch INT GPIO15). No
    /// RTC_INT pin on this board - the manager's `TimerWakeupSource`
    /// provides the periodic background-poll wake instead. `int_type
    /// = 4` is LowLevel (the INT lines are active-low), the only
    /// type esp-hal allows for wake-from-light-sleep.
    ///
    /// NOTE: C6 hardware light-sleep itself is not yet validated;
    /// this arms the right pins so it's correct when sleep is
    /// brought up. The manager only calls this on sleep entry.
    fn arm_wake_sources(&mut self) {
        for &gpio_num in &[crate::board::BTN_BOOT, crate::board::TOUCH_INT] {
            GPIO::regs().pin(gpio_num as usize).modify(|_, w| unsafe {
                w.wakeup_enable().set_bit();
                w.int_type().bits(4)
            });
        }
    }

    /// C6 family: `PMU.slp_wakeup_status0`.
    ///
    /// Also the sleep-breadcrumb completion marker: the manager calls
    /// this exactly once, right after `rtc.sleep()` returns, so it
    /// closes the cycle the LP-SRAM record opened in
    /// `tune_sleep_config`.
    fn wake_cause_raw(&self) -> u32 {
        use esp_hal::peripherals::PMU;
        esp_hal::rtc_cntl::sleep::sleep_diag::note_returned();
        PMU::regs().slp_wakeup_status0().read().wakeup_cause().bits()
    }

    /// Switch CPU frequency at runtime. This chip (RISC-V) tops out
    /// at 160 MHz. With PLL as the root clock (left as configured at
    /// init - we do NOT touch the source) the hardware AUTODIV gives
    /// HP_ROOT_CLK = 160 MHz, and CPU_CLK = HP_ROOT / (cpu_hs_div_num
    /// + 1): `0` -> 160 MHz, `1` -> 80 MHz. Only the CPU high-speed
    /// divider changes (PLL/source stay up), which is the same
    /// low-risk class of poke esp-hal itself does in
    /// `configure_cpu_hs_div_impl` (esp-hal 1.1.0-rc.0,
    /// src/soc/esp32c6/clocks.rs) - a single `modify()`, no
    /// apply/poll bit, no flash-timing change. `Mhz240` is
    /// unreachable here; clamp to 160 (the manager never requests it
    /// at runtime).
    fn set_cpu_freq(&mut self, freq: CpuFreq) {
        use esp_hal::peripherals::PCR;
        let hs_div: u8 = match freq {
            CpuFreq::Mhz80 => 1,                          // 160 / 2 = 80
            CpuFreq::Mhz160 | CpuFreq::Mhz240 => 0,       // 160 / 1 = 160 (max)
        };
        PCR::regs()
            .cpu_freq_conf()
            .modify(|_, w| unsafe { w.cpu_hs_div_num().bits(hs_div) });
    }

    /// No chip-specific light-sleep tuning applied: the s3
    /// fpu/reject knobs don't exist on this chip's `RtcSleepConfig`,
    /// and C6 hardware light-sleep is not yet validated - the default
    /// config is the correct starting point until the C6 sleep
    /// recipe is brought up.
    ///
    /// Doubles as the sleep-breadcrumb arm point: the manager calls
    /// this once per sleep entry, right before `rtc.sleep()`, which
    /// is exactly where the LP-SRAM record (docs/esp-hal-patch.md)
    /// wants its "a sleep cycle is starting" marker. After a freeze,
    /// the next boot logs how far past this marker the cycle got.
    fn tune_sleep_config(
        &self,
        _cfg: &mut esp_hal::rtc_cntl::sleep::RtcSleepConfig,
    ) {
        esp_hal::rtc_cntl::sleep::sleep_diag::arm();
    }

    /// The C6 sleep path re-calibrates RC_FAST_DIV on every
    /// `rtc.sleep()` and divides by the result (PMU wait times). Ask
    /// the same calibration, same cycle count, so the manager sees
    /// what the sleep code is about to see. 0 = the TIMG calibration
    /// timed out - the exact condition that, unpatched, is a
    /// divide-by-zero inside `rtc.sleep()`. Via the vendored esp-hal's
    /// `calibrate_rc_fast_div` (see docs/esp-hal-patch.md); no
    /// register access of our own.
    fn sleep_clock_probe(&self) -> Option<u32> {
        let cal = esp_hal::clock::RtcClock::calibrate_rc_fast_div(2048);
        // Breadcrumb sub-stage: `calibrate` busy-waits on a TIMG
        // ready bit, making this probe itself a hang suspect. Stuck
        // at ARMED = died in the wait above; the vendored stages
        // from SLEEP_CALLED onward pinpoint `rtc.sleep()` itself.
        // (2026-08-24 capture: probe exonerated - the freeze died
        // after PROBE_DONE.)
        esp_hal::rtc_cntl::sleep::sleep_diag::note_probe_done();
        // Hand the value to the sleep path so `rtc.sleep()` skips
        // its own in-sleep measurement - whose clock-node restore
        // re-enables the PLL mid-entry and deadlocks (the post-WiFi
        // freeze, PATCH.md capture 3). This probe runs while the PLL
        // is still the root clock, where the same measurement is
        // safe. A 0 (measurement timeout) provides nothing: the
        // sleep path then measures as before, watchdog as the net.
        if cal != 0 {
            esp_hal::rtc_cntl::sleep::provide_fastclk_period(cal);
        }
        Some(cal)
    }

    /// 30 s guard around every sleep while the post-WiFi freeze is
    /// under investigation. The heartbeat wakes every 5 s, so 30 s of
    /// unbroken sleep means the chip is stuck; the LP watchdog then
    /// resets it. Unlike the PWR long-press (an AXP power cycle,
    /// which wipes LP SRAM), a watchdog reset keeps the sleep
    /// breadcrumb alive for the next boot to report. Remove together
    /// with the breadcrumb once the freeze is understood.
    fn sleep_watchdog_timeout(&self) -> Option<esp_hal::time::Duration> {
        Some(esp_hal::time::Duration::from_secs(30))
    }
}
