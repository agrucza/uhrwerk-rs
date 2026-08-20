//! Power subsystem GPIO controls - the S3 `Board` impl.
//!
//! Holds the non-I2C, board-specific power hardware:
//!
//!   * **SYS_OUT latch** (GPIO10) - holds the board power rail on;
//!     released on `shutdown()` to power down.
//!   * **Motor** (GPIO18) - haptic feedback for button presses.
//!
//! Plus the S3 light-sleep wake-pin arming (touch INT GPIO38, BOOT
//! GPIO0, RTC INT GPIO39). This is the concrete
//! `system_core::board::Board` implementation for this board; the
//! shared manager calls it through that trait and is otherwise
//! board-blind.
//!
//! The I2C side of the PMU (AXP2101 register access, battery
//! readings, interrupt polling) lives in
//! `system_core::tasks::power::PowerTaskState`. PMU init happens
//! here in [`PowerControls::init`] since the rails must be enabled
//! before any other I2C subsystem; the caller wraps the returned
//! `Pmu` in a `PowerTaskState` for the polling task.

use drivers::pmu::{
    ChargeCurrent, ChargeVoltage, Config as PmuConfig, Pmu, PowerOffVoltage,
    TerminationCurrent,
};
use embedded_hal::i2c::I2c;
use esp_hal::gpio::Output;
use esp_hal::peripherals::GPIO;
use system_core::board::{Board, CpuFreq};

/// Snapshot of power-related readings at one point in time.
/// Produced by `PowerTaskState::snapshot`; consumed by the UI
/// data builder.
#[derive(Default)]
#[allow(dead_code)]
pub struct PowerSnapshot {
    /// Battery state of charge (0-100%) from the fuel gauge.
    pub battery_percent: Option<u8>,
    /// Battery terminal voltage in millivolts.
    pub battery_voltage_mv: Option<u16>,
}

pub struct PowerControls<'d> {
    sys_out: Output<'d>,
    motor: Output<'d>,
}

impl<'d> PowerControls<'d> {
    /// Initialize the power subsystem. Must be the first peripheral
    /// brought up at boot:
    ///
    /// 1. Latches SYS_OUT rail on (GPIO10 LOW)
    /// 2. Initializes the AXP2101 PMU and enables all power rails
    ///
    /// Returns `(PowerControls, Pmu)` on success. The caller wraps
    /// the `Pmu` in a `PowerTaskState` for the polling task.
    pub fn init(
        sys_out_pin: impl Into<Output<'d>>,
        motor_pin: impl Into<Output<'d>>,
        i2c: &mut impl I2c,
    ) -> Result<(Self, Pmu), ()> {
        let sys_out = sys_out_pin.into();
        let motor = motor_pin.into();

        let pmu = Pmu::new(PmuConfig::default());
        log::info!("PMU: initializing AXP2101...");
        match pmu.init(i2c) {
            Ok(raw_id) => {
                let version = (raw_id >> 4) & 0x03;
                log::info!(
                    "PMU: AXP2101 rev {} (0x{:02X}) - all rails enabled",
                    version, raw_id,
                );
            }
            Err(_) => {
                log::error!("PMU: initialization failed");
                return Err(());
            }
        }

        // Charge + power-off profile for the board's 400 mAh cell.
        // The AXP2101's power-on defaults leave charging largely
        // unconfigured and VOFF at 2.6 V - deep-discharge territory
        // that also wipes the fuel gauge's learned state on every
        // collapse (observed as 0% -> dead -> 87%-on-charger).
        // 0.25 C charge (gentle on the cell), explicit 4.2 V CV,
        // 25 mA termination so charge-done anchors the gauge at a
        // true 100%, 3.2 V VOFF.
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

        Ok((Self { sys_out, motor }, pmu))
    }
}

impl<'d> Board for PowerControls<'d> {
    /// Drive the haptic motor high (start buzz).
    fn buzz(&mut self) {
        self.motor.set_high();
    }

    /// Drive the haptic motor low (stop buzz).
    fn buzz_stop(&mut self) {
        self.motor.set_low();
    }

    /// Release the SYS_OUT latch - powers down the board.
    fn shutdown(&mut self) {
        log::info!("PWR: releasing SYS_OUT latch - powering off");
        self.sys_out.set_high();
    }

    /// Re-arm GPIO wake for touch_int(38), boot_btn(0), and
    /// rtc_int(39). The embassy async GPIO drivers used by those
    /// pins' tasks call `listen_with_options(..., wake_up_from_
    /// light_sleep=false)` on every wait, which clears the
    /// `wakeup_enable` bit set at init. We set it back immediately
    /// before `rtc.sleep()`. `int_type=4` is LowLevel, which is what
    /// the INT lines drive when active (active-low on all three) and
    /// is the only type esp-hal allows for wake-from-light-sleep.
    fn arm_wake_sources(&mut self) {
        for &gpio_num in &[0u8, 38u8, 39u8] {
            GPIO::regs().pin(gpio_num as usize).modify(|_, w| unsafe {
                w.wakeup_enable().set_bit();
                w.int_type().bits(4)
            });
        }
    }

    /// S3 family: RTC_CNTL (`LPWR`) `slp_wakeup_cause`.
    fn wake_cause_raw(&self) -> u32 {
        use esp_hal::peripherals::LPWR;
        LPWR::regs().slp_wakeup_cause().read().wakeup_cause().bits()
    }

    /// Switch CPU frequency at runtime via the `SYSTEM.cpu_per_conf`
    /// divider (PLL stays the source - APB stays 80 MHz so I2C/SPI
    /// are unaffected; the XTAL-systimer-based embassy timers too).
    ///
    /// `esp_hal::clock::cpu_clock()` keeps reporting the init-time
    /// clock regardless - esp-hal caches it in a static `Clocks` at
    /// init and never re-reads. The silicon scales correctly; only
    /// esp-hal's view is stale. Verified on esp-hal 1.1.0-rc.0 by
    /// reading `cpu_per_conf` back after each write. (Relocated
    /// verbatim from the shared manager - behavior-identical.)
    fn set_cpu_freq(&mut self, freq: CpuFreq) {
        use esp_hal::peripherals::SYSTEM;
        let (period_sel, freq_mhz) = match freq {
            CpuFreq::Mhz80 => (0u8, 80u32),
            CpuFreq::Mhz160 => (1u8, 160u32),
            CpuFreq::Mhz240 => (2u8, 240u32),
        };
        SYSTEM::regs().cpu_per_conf().modify(|_, w| unsafe {
            w.pll_freq_sel().set_bit();
            w.cpuperiod_sel().bits(period_sel)
        });
        esp_hal::rom::ets_update_cpu_frequency_rom(freq_mhz);
    }

    /// The light-sleep config that reliably wakes on this chip
    /// (validated via `bin/sleep_test.rs`):
    /// - `xtal_fpu(true)` keeps the main XTAL powered (CPU can't
    ///   resume clocking on wake without it).
    /// - `rtc_regulator_fpu(true)` keeps the RTC regulator on so the
    ///   RTC domain stays functional during sleep.
    /// - `light_slp_reject(false)` tolerates a pending interrupt at
    ///   sleep entry (e.g. a latched PCF85063 INT line).
    fn tune_sleep_config(
        &self,
        cfg: &mut esp_hal::rtc_cntl::sleep::RtcSleepConfig,
    ) {
        // rtc_regulator_fpu REVERTED to true 2026-05-22: it woke fine
        // dropped (touch + heartbeat), but we're putting it back to the
        // recipe value to test whether the BOOT-button (GPIO0) wake-reset
        // is caused by this knob or was already there with it on.
        cfg.set_rtc_regulator_fpu(true);
        // EXPERIMENT 2026-05-22: was `set_xtal_fpu(true)`. Testing
        // whether the S3 wakes without keeping the 40 MHz main XTAL
        // powered through light sleep - if it does, that closes much of
        // the remaining sleep-current gap to the C6. The validated
        // recipe (project_light_sleep, rule 3) says this is REQUIRED and
        // that failure is a SILENT HANG (sleeps, never wakes). If the
        // device won't wake on touch after sleeping, revert this to
        // `true` and reflash.
        cfg.set_xtal_fpu(false);
        cfg.set_light_slp_reject(false);
    }
}
