//! AXP2101 PMU (Power Management Unit) driver - HAL-agnostic.
//!
//! Works with any I²C implementation that satisfies the `embedded-hal` traits.
//! The firmware passes in the concrete `esp_hal` I²C type; this driver never
//! imports `esp_hal` directly.
//!
//! The I²C bus is **not** owned by this struct - it is passed by mutable
//! reference on each call. This allows the same bus to be shared with the
//! touch controller, RTC, IMU, and any other I²C peripheral.
//!
//! Rail wiring on the ESP32-S3-Touch-AMOLED-2.06.
//!
//! Verified against the Waveshare schematic on 2026-04-14 by
//! reading each peripheral block (ESP32, AXP2101, USB, Motor,
//! Keys, SD card, QMI8658, PCF85063, AMOLED, Codec, ADC,
//! PA & Speaker, Mic):
//!
//!   DCDC1    → VCC3V3    Master system rail. Powers the ESP32-S3
//!                        + its SPI flash, the SD card, the
//!                        QMI8658 IMU, the entire display-plus-
//!                        touch FPC module (CO5300 display and
//!                        FT3168 touch are on the same flex; the
//!                        module has an internal boost that
//!                        generates the AMOLED panel rails from
//!                        VCC3V3), the NS4150B speaker amp, and
//!                        the ES8311 / ES7210 digital supplies
//!                        (PVDD / DVDD). Effectively every I²C
//!                        and SPI peripheral on the board. This
//!                        is the rail the ESP32 runs off of, so
//!                        **it MUST stay on anywhere the ESP32
//!                        is expected to keep running**.
//!   ALDO1    → A3V3      Audio analog supply **plus at least one
//!                        other consumer we haven't fully traced**.
//!                        Confirmed loads: ES8311 DAC AVDD (via
//!                        R29 0Ω) and ES7210 ADC AVDD / VDDA (via
//!                        R34 0Ω). A 2026-04-14 attempt to hold
//!                        this rail off at boot (because audio is
//!                        dormant) made the FT3168 touch controller
//!                        unresponsive on I²C, which means the
//!                        touch IC also depends on A3V3 somewhere
//!                        we didn't see in the AMOLED schematic
//!                        block. Until that dependency is traced,
//!                        **ALDO1 must stay on at boot** - leave
//!                        `enable_all_rails` alone.
//!                        [`Pmu::set_audio_rail`] still exists as
//!                        an explicit audio-rail toggle, but today
//!                        it's effectively "enable only" because
//!                        disabling the rail also kills touch.
//!   ALDO2    → signal    Not a power rail to any real load.
//!                        Drives the display FPC's DSI_PWR_EN
//!                        enable input via a 10 kΩ series resistor
//!                        (R10). Effectively a slow GPIO
//!                        implemented by toggling an LDO.
//!   ALDO3    → VCC3V     Haptic motor supply. Gated by GPIO18
//!                        via an MMBT3904 NPN switch - the LDO
//!                        stays on continuously, current only
//!                        flows through the motor while GPIO18
//!                        is driven high.
//!   RTC-LDO1 → VCC-RTC   PCF85063 VDD (pin 10). Also feeds the
//!                        10 kΩ pull-up reference (RP5) for the
//!                        AXP2101's own open-drain IRQ output
//!                        (AXP_IRQ, pin 38). Because RTC-LDO1 is
//!                        backup-battery-backed on the chip side
//!                        it stays alive through any sleep path,
//!                        so both RTC timekeeping and PMU
//!                        interrupt signalling continue to work
//!                        even if DCDC1 is disabled in a future
//!                        deep-sleep scenario.
//!
//! No schematic consumer found for: ALDO4, BLDO1, BLDO2, DCDC2,
//! DCDC3, DCDC4, CPUSLDO, DLDO1, DLDO2, RTC-LDO2. Earlier versions
//! of this comment guessed these powered AMOLED VDDIO / VCORE /
//! AVDD, but the 2026-04-14 schematic trace showed the display is
//! on VCC3V3 via its internal boost, so those rails are actually
//! unclaimed. They're still enabled at boot by `enable_all_rails`
//! because disabling them without PPK2 measurements + another
//! schematic pass carries more risk than reward; revisit when
//! idle current matters enough to measure.

pub mod interrupts;
pub mod power_states;
pub mod registers;
pub mod types;

use embedded_hal::i2c::I2c as I2cTrait;

/// Default I2C address for AXP2101 (0x34 when ADDR pin is low).
pub const DEFAULT_ADDRESS: u8 = 0x34;

/// Error type for PMU operations.
///
/// Generic over `E` - the I2C error type from whichever HAL is used.
#[derive(Debug)]
pub enum Error<E> {
    /// An I2C transaction failed; the inner value is the HAL's own error.
    I2c(E),
    /// The device did not respond with the expected chip ID.
    DeviceNotFound,
    /// A voltage value outside the 500–3500 mV range was requested.
    InvalidValue,
}

/// AXP2101 PMU driver.
///
/// Holds only the I²C address (and any future driver state). The I²C bus
/// itself is passed by mutable reference on every call so it can be freely
/// shared with other peripherals on the same bus.
pub struct Pmu {
    addr: u8,
}

/// PMU configuration.
pub struct Config {
    /// I2C device address (default: [`DEFAULT_ADDRESS`]).
    pub address: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self { address: DEFAULT_ADDRESS }
    }
}

impl Pmu {
    /// Create a new PMU driver instance.
    pub fn new(config: Config) -> Self {
        Self { addr: config.address }
    }

    /// Verify the AXP2101 is present on the I²C bus and has the correct chip ID.
    ///
    /// Reads REG 03h and checks that `chip_id_h` and `chip_id_l` match the
    /// AXP2101 signature (ignoring the version bits). Returns the raw register
    /// byte on success so the caller can extract the version if needed:
    ///
    /// ```ignore
    /// let raw = pmu.check_device(&mut i2c)?;
    /// let version = (raw >> 4) & 0x03; // 0=A, 1=B
    /// ```
    pub fn check_device<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let raw = self.read_register(i2c, registers::CHIP_ID)?;
        if (raw & registers::CHIP_ID_MASK) != registers::CHIP_ID_VALUE {
            return Err(Error::DeviceNotFound);
        }
        Ok(raw)
    }

    /// Initialise the PMU: verify presence, set rail voltages,
    /// enable the boot-time rails.
    ///
    /// Returns the raw chip ID byte on success. The version can be
    /// extracted from bits 5:4 of that byte (0=A, 1=B).
    ///
    /// Call this early in `main`, **before** accessing the display
    /// or any other peripheral that depends on these power rails.
    /// After returning `Ok(...)`, wait at least 20 ms for the rails
    /// to stabilise.
    pub fn init<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Verify device presence and chip ID.
        let chip_id = self.check_device(i2c)?;

        // Set all output voltages while LDOs are still disabled.
        self.set_aldo1_voltage(i2c, 3300)?; // A3V3     - touch + audio analog
        self.set_aldo2_voltage(i2c, 3300)?; // VL2_3.3V - display DSI_PWR_EN enable
        self.set_aldo3_voltage(i2c, 3300)?; // VCC3V    - haptic motor
        self.set_aldo4_voltage(i2c, 1800)?; // VL3_1.8V - unclaimed (no consumer found)
        self.set_bldo1_voltage(i2c, 1200)?; // VL_1.2V  - unclaimed (no consumer found)
        self.set_bldo2_voltage(i2c, 2800)?; // VL_2.8V  - unclaimed (no consumer found)

        // Enable all six boot-time LDOs in one register write.
        self.enable_all_rails(i2c)?;

        // Enable all ADC channels (battery, VBUS, Vsys, die temp, TS)
        // and the fuel gauge for battery monitoring. Enabling early
        // gives the ADC time to produce valid readings by the time
        // the rest of the system finishes initializing.
        self.enable_all_adc(i2c)?;
        self.enable_battery_monitor(i2c)?;

        Ok(chip_id)
    }

    // ---- Interrupt handling -------------------------------------------------

    /// Read all three IRQ status registers and return a combined snapshot.
    ///
    /// Call this when the IRQ pin goes low. After inspecting the result,
    /// call [`clear_interrupts`] to acknowledge and re-arm the IRQ pin.
    ///
    /// [`clear_interrupts`]: Pmu::clear_interrupts
    pub fn read_interrupts<I2C, E>(&self, i2c: &mut I2C) -> Result<interrupts::InterruptStatus, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r0 = self.read_register(i2c, registers::REG_IRQ_STATUS0)?;
        let r1 = self.read_register(i2c, registers::REG_IRQ_STATUS1)?;
        let r2 = self.read_register(i2c, registers::REG_IRQ_STATUS2)?;
        Ok(interrupts::InterruptStatus::new(r0, r1, r2))
    }

    /// Clear the interrupt flags that were set in `status` (write 1 to clear, RW1C).
    ///
    /// Pass the same `InterruptStatus` returned by [`read_interrupts`] so only
    /// the bits that were active get cleared - any new events that arrived in
    /// between are preserved.
    ///
    /// [`read_interrupts`]: Pmu::read_interrupts
    pub fn clear_interrupts<I2C, E>(&self, i2c: &mut I2C, status: &interrupts::InterruptStatus) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::REG_IRQ_STATUS0, status.reg48_byte())?;
        self.write_register(i2c, registers::REG_IRQ_STATUS1, status.reg49_byte())?;
        self.write_register(i2c, registers::REG_IRQ_STATUS2, status.reg4a_byte())
    }

    /// Configure which interrupt sources drive the IRQ pin.
    ///
    /// Build an [`InterruptConfig`] using the builder pattern, then pass it here:
    ///
    /// ```ignore
    /// use drivers::pmu::interrupts::{InterruptConfig, InterruptSource};
    /// let cfg = InterruptConfig::none()
    ///     .enable(InterruptSource::VbusInsert)
    ///     .enable(InterruptSource::PowerOnShortPress);
    /// pmu.configure_interrupts(&mut i2c, &cfg)?;
    /// ```
    ///
    /// [`InterruptConfig`]: interrupts::InterruptConfig
    pub fn configure_interrupts<I2C, E>(&self, i2c: &mut I2C, cfg: &interrupts::InterruptConfig) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::REG_IRQ_EN0, cfg.reg40_byte())?;
        self.write_register(i2c, registers::REG_IRQ_EN1, cfg.reg41_byte())?;
        self.write_register(i2c, registers::REG_IRQ_EN2, cfg.reg42_byte())
    }

    /// Set ALDO1 voltage in millivolts (valid range: 500–3500 mV, 100 mV steps).
    pub fn set_aldo1_voltage<I2C, E>(&self, i2c: &mut I2C, millivolts: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::REG_ALDO1_VOLT, Self::mv_to_register(millivolts)?)
    }

    /// Set ALDO2 voltage in millivolts.
    pub fn set_aldo2_voltage<I2C, E>(&self, i2c: &mut I2C, millivolts: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::REG_ALDO2_VOLT, Self::mv_to_register(millivolts)?)
    }

    /// Set ALDO3 voltage in millivolts.
    pub fn set_aldo3_voltage<I2C, E>(&self, i2c: &mut I2C, millivolts: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::REG_ALDO3_VOLT, Self::mv_to_register(millivolts)?)
    }

    /// Set ALDO4 voltage in millivolts.
    pub fn set_aldo4_voltage<I2C, E>(&self, i2c: &mut I2C, millivolts: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::REG_ALDO4_VOLT, Self::mv_to_register(millivolts)?)
    }

    /// Set BLDO1 voltage in millivolts.
    pub fn set_bldo1_voltage<I2C, E>(&self, i2c: &mut I2C, millivolts: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::REG_BLDO1_VOLT, Self::mv_to_register(millivolts)?)
    }

    /// Set BLDO2 voltage in millivolts.
    pub fn set_bldo2_voltage<I2C, E>(&self, i2c: &mut I2C, millivolts: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::REG_BLDO2_VOLT, Self::mv_to_register(millivolts)?)
    }

    /// Enable ALDO1-ALDO4 and BLDO1-BLDO2 (the six rails used at
    /// boot on this board).
    ///
    /// Writes `ALDO1 | ALDO2 | ALDO3 | ALDO4 | BLDO1 | BLDO2`
    /// (= 0x3F) to REG 90h. CPUSLDO and DLDO1/2 are left off.
    ///
    /// NOTE: on 2026-04-14 ALDO1 was briefly removed from this
    /// mask under the (wrong) assumption that it only powered the
    /// audio codec + ADC analog supplies. It turned out touch also
    /// depends on ALDO1 - disabling it at boot made the FT3168
    /// unresponsive on I²C. ALDO1 must stay on at boot until the
    /// "what else is on A3V3" question is properly answered with
    /// a more careful schematic trace. See `set_audio_rail` below
    /// if you still want to toggle just the audio use of this rail.
    pub fn enable_all_rails<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        use registers::ldo_en0;
        let mask = ldo_en0::ALDO1 | ldo_en0::ALDO2 | ldo_en0::ALDO3
                 | ldo_en0::ALDO4 | ldo_en0::BLDO1 | ldo_en0::BLDO2;
        self.write_register(i2c, registers::REG_LDO_EN0, mask)
    }

    /// Enable or disable ALDO1 (net `A3V3`).
    ///
    /// ALDO1 is enabled at boot by [`enable_all_rails`] because
    /// the FT3168 touch controller depends on it. The audio codec
    /// (ES8311 AVDD) and ADC (ES7210 AVDD / VDDA) also sit on this
    /// rail, so any future audio bring-up sequence should make
    /// sure the rail is on before touching those chips over I²C.
    /// Since the rail is already on after boot, `set_audio_rail`
    /// is idempotent in practice - calling it with `enable = true`
    /// just re-asserts the bit via a read-modify-write.
    ///
    /// **Passing `false` will also kill touch**, so only use that
    /// path if you've first confirmed with a proper schematic trace
    /// that nothing on the board other than the audio chips is on
    /// A3V3. As of 2026-04-14 that confirmation has NOT been done.
    ///
    /// Contract for audio initialisation (still valid for future
    /// audio wire-up, even though the rail is currently already
    /// on):
    ///
    /// 1. Call `set_audio_rail(i2c, true)`.
    /// 2. Wait at least **10 ms** for the LDO to settle before
    ///    touching the codec or ADC over I²C - the ES8311 /
    ///    ES7210 need a stable analog supply before their I²C
    ///    state machines will answer reliably.
    /// 3. Build a `system_core::audio` session (see `run_session`).
    ///
    /// Only ALDO1 is touched - all other rail enables in REG 90h
    /// are preserved via a read-modify-write.
    pub fn set_audio_rail<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        use registers::ldo_en0;
        let cur = self.read_register(i2c, registers::REG_LDO_EN0)?;
        let new = if enable {
            cur | ldo_en0::ALDO1
        } else {
            cur & !ldo_en0::ALDO1
        };
        self.write_register(i2c, registers::REG_LDO_EN0, new)
    }

    /// Enable or disable BLDO1.
    ///
    /// The consumer is board-specific: unclaimed on the original
    /// board (enabled at a safe voltage by [`enable_all_rails`]),
    /// the GPS receiver's main rail on the T-Watch Ultra - whose
    /// bin turns it off at boot: the receiver's backup domain hangs
    /// on the always-on VRTC, so a dark main rail is u-blox
    /// hardware backup mode (RTC time and ephemeris retained on
    /// backup current, acquisition engine off).
    ///
    /// Only BLDO1 is touched - all other rail enables in REG 90h
    /// are preserved via a read-modify-write.
    ///
    /// [`enable_all_rails`]: Pmu::enable_all_rails
    pub fn set_bldo1_enable<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        use registers::ldo_en0;
        let cur = self.read_register(i2c, registers::REG_LDO_EN0)?;
        let new = if enable {
            cur | ldo_en0::BLDO1
        } else {
            cur & !ldo_en0::BLDO1
        };
        self.write_register(i2c, registers::REG_LDO_EN0, new)
    }

    /// Enable or disable DLDO1. Board-specific consumer; on the
    /// T-Watch Ultra it is the NFC reader's whole power domain,
    /// rail-gated per session. Only DLDO1 is touched - all other
    /// rail enables in REG 90h are preserved via a
    /// read-modify-write.
    pub fn set_dldo1_enable<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        use registers::ldo_en0;
        let cur = self.read_register(i2c, registers::REG_LDO_EN0)?;
        let new = if enable {
            cur | ldo_en0::DLDO1
        } else {
            cur & !ldo_en0::DLDO1
        };
        self.write_register(i2c, registers::REG_LDO_EN0, new)
    }

    /// Enable or disable ALDO2 (net `DSI_PWR_EN`, the AMOLED display rail).
    ///
    /// ALDO2 is enabled at boot by [`enable_all_rails`] and powers the
    /// CO5300 display module via the `DSI_PWR_EN` enable line on the
    /// display FPC. Toggling this rail off cuts the panel; firmware
    /// must keep it on while the display is in use.
    ///
    /// On the S3 variant this rail is dedicated to the display; on the
    /// C6 variant the same ALDO2 net feeds the display FPC, and whether
    /// it also powers anything else on that FPC (e.g. the FT3168 touch
    /// IC) is still TBD - approach incrementally when bringing up the
    /// C6 board.
    ///
    /// Contract:
    /// 1. Call `set_aldo2_voltage(i2c, 3300)` before enabling the rail
    ///    (the LDO voltage register has no guaranteed default).
    /// 2. Call `set_display_rail(i2c, true)`.
    /// 3. Wait at least 20 ms for the rail to stabilise before sending
    ///    QSPI commands to the panel.
    ///
    /// Only ALDO2 is touched - all other rail enables in REG 90h are
    /// preserved via a read-modify-write.
    pub fn set_display_rail<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        use registers::ldo_en0;
        let cur = self.read_register(i2c, registers::REG_LDO_EN0)?;
        let new = if enable {
            cur | ldo_en0::ALDO2
        } else {
            cur & !ldo_en0::ALDO2
        };
        self.write_register(i2c, registers::REG_LDO_EN0, new)
    }

    /// Disable all LDOs (REG 90h and REG 91h both cleared).
    pub fn disable_all_rails<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::REG_LDO_EN0, 0x00)?;
        self.write_register(i2c, registers::REG_LDO_EN1, 0x00)
    }

    // ---- Battery monitoring ---------------------------------------------------

    /// Enable battery voltage ADC and fuel gauge.
    ///
    /// Called automatically during `init()`. The fuel gauge needs a few
    /// seconds after enabling to produce an accurate percentage reading.
    pub fn enable_battery_monitor<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Enable battery voltage ADC (bit 0 of REG_ADC_EN)
        let adc = self.read_register(i2c, registers::REG_ADC_EN)?;
        self.write_register(i2c, registers::REG_ADC_EN, adc | registers::adc_en::BAT_VOLT)?;

        // Enable fuel gauge (bit 3 of REG_CHARGER_GAUGE_WDT_EN)
        let gauge = self.read_register(i2c, registers::REG_CHARGER_GAUGE_WDT_EN)?;
        self.write_register(i2c, registers::REG_CHARGER_GAUGE_WDT_EN, gauge | (1 << 3))
    }

    /// Read battery state of charge (0-100%).
    ///
    /// Returns `None` if the fuel gauge hasn't produced a reading yet.
    pub fn battery_percent<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_register(i2c, registers::REG_BAT_PERCENT)
    }

    /// Read battery voltage in millivolts.
    ///
    /// The ADC produces a 14-bit value with 1 mV per LSB.
    pub fn battery_voltage_mv<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let hi = self.read_register(i2c, registers::REG_VBAT_H)? as u16;
        let lo = self.read_register(i2c, registers::REG_VBAT_L)? as u16;
        Ok((hi << 8) | lo)
    }

    // ---- Status registers (REG 00h-01h) ------------------------------------

    /// Read PMU Status 1 - power-path and battery state.
    pub fn read_status1<I2C, E>(&self, i2c: &mut I2C) -> Result<types::PmuStatus1, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_PMU_STATUS1)?;
        Ok(types::PmuStatus1 {
            vbus_good:            (r & (1 << 5)) != 0,
            batfet_active:        (r & (1 << 4)) != 0,
            battery_present:      (r & (1 << 3)) != 0,
            battery_active:       (r & (1 << 2)) != 0,
            thermal_active:       (r & (1 << 1)) != 0,
            current_limit_active: (r & (1 << 0)) != 0,
        })
    }

    /// Read PMU Status 2 - charging state and system status.
    pub fn read_status2<I2C, E>(&self, i2c: &mut I2C) -> Result<types::PmuStatus2, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_PMU_STATUS2)?;
        Ok(types::PmuStatus2 {
            current_direction: types::CurrentDirection::from_bits((r >> 5) & 0x03),
            system_on:         (r & (1 << 4)) != 0,
            vindpm_active:     (r & (1 << 3)) != 0,
            charger_phase:     types::ChargerPhase::from_bits(r & 0x07),
        })
    }

    // ---- Extended ADC readings ---------------------------------------------

    /// Enable all ADC channels: battery, VBUS, Vsys, die temperature, TS.
    ///
    /// Call this once (or rely on `init()` which enables battery only).
    /// Each channel adds a small amount of current draw from the ADC
    /// sampling, but the readings become available immediately.
    pub fn enable_all_adc<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        use registers::adc_en;
        let mask = adc_en::BAT_VOLT | adc_en::TS_PIN | adc_en::VBUS_VOLT
                 | adc_en::VSYS_VOLT | adc_en::DIE_TEMP;
        self.write_register(i2c, registers::REG_ADC_EN, mask)
    }

    /// Read VBUS voltage in millivolts (1 mV per LSB).
    ///
    /// Returns 0 if VBUS is not connected. Requires the VBUS ADC
    /// channel to be enabled (see `enable_all_adc`).
    pub fn vbus_voltage_mv<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_adc_14bit(i2c, registers::REG_VBUS_H, registers::REG_VBUS_L)
    }

    /// Read system voltage in millivolts (1 mV per LSB).
    ///
    /// Vsys is the output of the power-path MUX (VBUS or battery).
    pub fn system_voltage_mv<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_adc_14bit(i2c, registers::REG_VSYS_H, registers::REG_VSYS_L)
    }

    /// Read die temperature as a raw 14-bit ADC value.
    ///
    /// The conversion to degrees Celsius depends on the AXP2101
    /// internal reference and is not documented with a public formula.
    /// Use this primarily for relative comparisons and thermal
    /// throttling detection.
    pub fn die_temperature_raw<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_adc_14bit(i2c, registers::REG_TDIE_H, registers::REG_TDIE_L)
    }

    /// Read TS (thermistor) pin voltage as a raw 14-bit ADC value.
    ///
    /// The TS pin is typically connected to an NTC thermistor on the
    /// battery pack for temperature monitoring during charging.
    pub fn ts_voltage_raw<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_adc_14bit(i2c, registers::REG_TS_H, registers::REG_TS_L)
    }

    // ---- Input limits (REG 14h-16h) ----------------------------------------

    /// Set the minimum system voltage (REG 14h bits 2:0).
    ///
    /// Vsys_min = 3.2 + N * 0.1 V, where N = 0-7.
    /// Valid range: 3200-3900 mV. Default: 3700 mV (N=5).
    pub fn set_min_system_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mv = mv.max(3200).min(3900);
        let n = ((mv - 3200) / 100) as u8;
        let reg = self.read_register(i2c, registers::REG_SYS_VMIN)?;
        self.write_register(i2c, registers::REG_SYS_VMIN, (reg & !0x07) | (n & 0x07))
    }

    /// Read the minimum system voltage in millivolts.
    pub fn min_system_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_SYS_VMIN)?;
        Ok(3200 + (reg & 0x07) as u16 * 100)
    }

    /// Set the input voltage limit - VINDPM (REG 15h bits 3:0).
    ///
    /// VINDPM = 3.88 + N * 0.08 V, where N = 0-15.
    /// Valid range: 3880-5080 mV. Default: 4360 mV (N=6).
    pub fn set_input_voltage_limit<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mv = mv.max(3880).min(5080);
        let n = ((mv - 3880) / 80) as u8;
        let reg = self.read_register(i2c, registers::REG_VINDPM)?;
        self.write_register(i2c, registers::REG_VINDPM, (reg & !0x0F) | (n & 0x0F))
    }

    /// Read the input voltage limit (VINDPM) in millivolts.
    pub fn input_voltage_limit<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_VINDPM)?;
        Ok(3880 + (reg & 0x0F) as u16 * 80)
    }

    /// Set the input current limit (REG 16h bits 2:0).
    pub fn set_input_current_limit<I2C, E>(&self, i2c: &mut I2C, limit: types::InputCurrentLimit) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_ILIM)?;
        self.write_register(i2c, registers::REG_ILIM, (reg & !0x07) | (limit as u8))
    }

    /// Read the input current limit.
    pub fn input_current_limit<I2C, E>(&self, i2c: &mut I2C) -> Result<types::InputCurrentLimit, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_ILIM)?;
        Ok(types::InputCurrentLimit::from_bits(reg))
    }

    // ---- Charger configuration (REG 61h-6Ah) -------------------------------

    /// Set the pre-charge current limit (REG 61h bits 3:0).
    pub fn set_precharge_current<I2C, E>(&self, i2c: &mut I2C, cur: types::PreChargeCurrent) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_IPRECHG)?;
        self.write_register(i2c, registers::REG_IPRECHG, (reg & !0x0F) | (cur.0 & 0x0F))
    }

    /// Read the pre-charge current limit.
    pub fn precharge_current<I2C, E>(&self, i2c: &mut I2C) -> Result<types::PreChargeCurrent, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_IPRECHG)?;
        Ok(types::PreChargeCurrent(reg & 0x0F))
    }

    /// Set the constant-current charge current (REG 62h bits 4:0).
    pub fn set_charge_current<I2C, E>(&self, i2c: &mut I2C, cur: types::ChargeCurrent) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_ICC)?;
        self.write_register(i2c, registers::REG_ICC, (reg & !0x1F) | (cur.0 & 0x1F))
    }

    /// Read the constant-current charge current.
    pub fn charge_current<I2C, E>(&self, i2c: &mut I2C) -> Result<types::ChargeCurrent, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_ICC)?;
        Ok(types::ChargeCurrent(reg & 0x1F))
    }

    /// Set the termination current and enable/disable charge termination
    /// (REG 63h). Bit 4 = enable, bits 3:0 = current.
    pub fn set_termination_current<I2C, E>(
        &self,
        i2c: &mut I2C,
        cur: types::TerminationCurrent,
        enable: bool,
    ) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_ITERM)?;
        let val = (reg & !0x1F) | (cur.0 & 0x0F) | if enable { 1 << 4 } else { 0 };
        self.write_register(i2c, registers::REG_ITERM, val)
    }

    /// Read the termination current and whether termination is enabled.
    pub fn termination_current<I2C, E>(&self, i2c: &mut I2C) -> Result<(types::TerminationCurrent, bool), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_ITERM)?;
        Ok((types::TerminationCurrent(reg & 0x0F), (reg & (1 << 4)) != 0))
    }

    /// Set the constant-voltage charge target (REG 64h bits 2:0).
    pub fn set_charge_voltage<I2C, E>(&self, i2c: &mut I2C, cv: types::ChargeVoltage) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_CV_VOLT)?;
        self.write_register(i2c, registers::REG_CV_VOLT, (reg & !0x07) | (cv as u8))
    }

    /// Set the battery voltage at which the PMU powers the system
    /// off (VOFF, REG 24h bits 2:0). See [`types::PowerOffVoltage`]
    /// for why the 2.6 V power-on default should be raised.
    pub fn set_power_off_voltage<I2C, E>(
        &self,
        i2c: &mut I2C,
        v: types::PowerOffVoltage,
    ) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_VSYS_PWROFF)?;
        self.write_register(i2c, registers::REG_VSYS_PWROFF, (reg & !0x07) | (v.0 & 0x07))
    }

    /// Read the configured power-off voltage threshold.
    pub fn power_off_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<types::PowerOffVoltage, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_VSYS_PWROFF)?;
        Ok(types::PowerOffVoltage(reg & 0x07))
    }

    /// Read the constant-voltage charge target.
    pub fn charge_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<Option<types::ChargeVoltage>, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_CV_VOLT)?;
        Ok(types::ChargeVoltage::from_bits(reg))
    }

    /// Set the thermal regulation threshold (REG 65h bits 1:0).
    pub fn set_thermal_threshold<I2C, E>(&self, i2c: &mut I2C, th: types::ThermalThreshold) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_THERMAL_REG)?;
        self.write_register(i2c, registers::REG_THERMAL_REG, (reg & !0x03) | (th as u8))
    }

    /// Read the thermal regulation threshold.
    pub fn thermal_threshold<I2C, E>(&self, i2c: &mut I2C) -> Result<types::ThermalThreshold, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_THERMAL_REG)?;
        Ok(types::ThermalThreshold::from_bits(reg))
    }

    /// Enable or disable battery detection (REG 68h bit 0).
    pub fn set_battery_detection<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_BAT_DET)?;
        let val = if enable { reg | 1 } else { reg & !1 };
        self.write_register(i2c, registers::REG_BAT_DET, val)
    }

    /// Set the button battery termination voltage (REG 6Ah bits 2:0).
    ///
    /// Voltage = 2.6 + N * 0.1 V, where N = 0-7.
    /// Valid range: 2600-3300 mV.
    pub fn set_button_battery_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mv = mv.max(2600).min(3300);
        let n = ((mv - 2600) / 100) as u8;
        let reg = self.read_register(i2c, registers::REG_BTN_BAT_VTERM)?;
        self.write_register(i2c, registers::REG_BTN_BAT_VTERM, (reg & !0x07) | (n & 0x07))
    }

    /// Read back the button battery termination voltage (REG 6Ah
    /// bits 2:0), in millivolts.
    pub fn button_battery_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let n = self.read_register(i2c, registers::REG_BTN_BAT_VTERM)? & 0x07;
        Ok(2600 + n as u16 * 100)
    }

    /// Read back the button battery charger enable (REG 18h bit 2).
    pub fn button_battery_charge_enabled<I2C, E>(&self, i2c: &mut I2C) -> Result<bool, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        Ok(self.read_register(i2c, registers::REG_CHARGER_GAUGE_WDT_EN)? & (1 << 2) != 0)
    }

    /// Enable or disable button battery charging (REG 18h bit 2).
    pub fn set_button_battery_charge<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_CHARGER_GAUGE_WDT_EN)?;
        let val = if enable { reg | (1 << 2) } else { reg & !(1 << 2) };
        self.write_register(i2c, registers::REG_CHARGER_GAUGE_WDT_EN, val)
    }

    /// Enable or disable cell (main battery) charging (REG 18h bit 1).
    pub fn set_charging<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_CHARGER_GAUGE_WDT_EN)?;
        let val = if enable { reg | (1 << 1) } else { reg & !(1 << 1) };
        self.write_register(i2c, registers::REG_CHARGER_GAUGE_WDT_EN, val)
    }

    // ---- Power key timing (REG 27h) ----------------------------------------

    /// Set the power key timing configuration.
    pub fn set_power_key_config<I2C, E>(&self, i2c: &mut I2C, cfg: &types::PowerKeyConfig) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let val = ((cfg.irq_time as u8) << 4)
                | ((cfg.off_time as u8) << 2)
                | (cfg.on_time as u8);
        self.write_register(i2c, registers::REG_LEVEL_CFG, val)
    }

    /// Read the power key timing configuration.
    pub fn power_key_config<I2C, E>(&self, i2c: &mut I2C) -> Result<types::PowerKeyConfig, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_LEVEL_CFG)?;
        Ok(types::PowerKeyConfig {
            irq_time: types::PowerKeyIrqTime::from_bits((r >> 4) & 0x03),
            off_time: types::PowerKeyOffTime::from_bits((r >> 2) & 0x03),
            on_time:  types::PowerKeyOnTime::from_bits(r & 0x03),
        })
    }

    // ---- Power control (REG 10h, 12h, 13h, 26h) ---------------------------

    /// Trigger a soft power-off via REG 10h bit 0.
    ///
    /// This immediately powers off the system (all LDOs and DCDCs
    /// are disabled). The power-off source will be recorded as
    /// "software" in REG 21h.
    pub fn soft_power_off<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_PMU_CFG)?;
        self.write_register(i2c, registers::REG_PMU_CFG, reg | (1 << 0))
    }

    /// Trigger a SoC restart via REG 10h bit 1.
    pub fn soc_restart<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_PMU_CFG)?;
        self.write_register(i2c, registers::REG_PMU_CFG, reg | (1 << 1))
    }

    /// Set die over-temperature protection level (REG 13h bits 2:1)
    /// and enable/disable detection (bit 0).
    pub fn set_die_temp_protection<I2C, E>(
        &self,
        i2c: &mut I2C,
        level: types::DieOtpLevel,
        enable: bool,
    ) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let val = ((level as u8) << 1) | if enable { 1 } else { 0 };
        self.write_register(i2c, registers::REG_DIE_TEMP_CFG, val)
    }

    /// Read the BATFET enable state (REG 12h bit 3).
    ///
    /// When enabled, the battery FET stays on during power-off in
    /// battery-only mode (no VBUS). When disabled, the FET opens and
    /// the battery is fully disconnected.
    pub fn batfet_enabled<I2C, E>(&self, i2c: &mut I2C) -> Result<bool, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_BATFET_CFG)?;
        Ok((reg & (1 << 3)) != 0)
    }

    /// Set the BATFET enable state (REG 12h bit 3).
    pub fn set_batfet<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_BATFET_CFG)?;
        let val = if enable { reg | (1 << 3) } else { reg & !(1 << 3) };
        self.write_register(i2c, registers::REG_BATFET_CFG, val)
    }

    // ---- DCDC converters (REG 80h-85h) -------------------------------------

    /// Enable or disable individual DCDC converters (REG 80h bits 3:0).
    ///
    /// `mask` is a bitmask: bit 0 = DCDC1, bit 1 = DCDC2, bit 2 = DCDC3,
    /// bit 3 = DCDC4. Only bits in `mask` are changed; other DCDCs and
    /// the DVM bits (bits 7:4) are preserved.
    pub fn set_dcdc_enable<I2C, E>(&self, i2c: &mut I2C, mask: u8, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_DCDC_EN)?;
        let val = if enable { reg | (mask & 0x0F) } else { reg & !(mask & 0x0F) };
        self.write_register(i2c, registers::REG_DCDC_EN, val)
    }

    /// Read the DCDC enable state (REG 80h bits 3:0).
    pub fn dcdc_enabled<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_DCDC_EN)?;
        Ok(reg & 0x0F)
    }

    /// Set DCDC1 voltage (REG 82h bits 4:0).
    ///
    /// Voltage = 1.5 + N * 0.1 V, where N = 0-31.
    /// Valid range: 1500-3400 mV.
    pub fn set_dcdc1_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mv = mv.max(1500).min(3400);
        let n = ((mv - 1500) / 100) as u8;
        self.write_register(i2c, registers::REG_DCDC1_VOLT, n & 0x1F)
    }

    /// Read DCDC1 voltage in millivolts.
    pub fn dcdc1_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_DCDC1_VOLT)?;
        Ok(1500 + (reg & 0x1F) as u16 * 100)
    }

    /// Set DCDC2 voltage (REG 83h bits 6:0).
    ///
    /// Two ranges:
    ///   N = 0-70:  0.5 + N * 0.01 V   (500-1200 mV in 10 mV steps)
    ///   N = 71-87: 1.22 + (N-71) * 0.02 V  (1220-1540 mV in 20 mV steps)
    /// Valid range: 500-1540 mV.
    ///
    /// Bit 7 is the DVM enable bit and is preserved across writes.
    pub fn set_dcdc2_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mv = mv.max(500).min(1540);
        let n = if mv <= 1200 {
            ((mv - 500) / 10) as u8
        } else {
            (71 + (mv - 1220) / 20) as u8
        };
        let reg = self.read_register(i2c, registers::REG_DCDC2_VOLT)?;
        self.write_register(i2c, registers::REG_DCDC2_VOLT, (reg & 0x80) | (n & 0x7F))
    }

    /// Read DCDC2 voltage in millivolts.
    pub fn dcdc2_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_DCDC2_VOLT)?;
        let n = (reg & 0x7F) as u16;
        if n <= 70 {
            Ok(500 + n * 10)
        } else {
            Ok(1220 + (n - 71) * 20)
        }
    }

    /// Set DCDC3 voltage (REG 84h bits 6:0).
    ///
    /// Two-range encoding (same as DCDC2):
    ///   N = 0-70:  0.5 + N * 0.01 V   (500-1200 mV, 10 mV steps)
    ///   N = 71-87: 1.22 + (N-71) * 0.02 V  (1220-1540 mV, 20 mV steps)
    ///   N = 88-127: reserved
    ///
    /// Bit 7 is the DVM (Dynamic Voltage Management) enable bit and
    /// is preserved across writes.
    pub fn set_dcdc3_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mv = mv.max(500).min(1540);
        let n = if mv <= 1200 {
            ((mv - 500) / 10) as u8
        } else {
            (71 + (mv - 1220) / 20) as u8
        };
        let reg = self.read_register(i2c, registers::REG_DCDC3_VOLT)?;
        self.write_register(i2c, registers::REG_DCDC3_VOLT, (reg & 0x80) | (n & 0x7F))
    }

    /// Read DCDC3 voltage in millivolts.
    pub fn dcdc3_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_DCDC3_VOLT)?;
        let n = (reg & 0x7F) as u16;
        if n <= 70 {
            Ok(500 + n * 10)
        } else {
            Ok(1220 + (n - 71) * 20)
        }
    }

    /// Set DCDC4 voltage (REG 85h bits 6:0).
    ///
    /// Two-range encoding:
    ///   N = 0-70:  0.5 + N * 0.01 V    (500-1200 mV, 10 mV steps)
    ///   N = 71+:   1.22 + (N-71) * 0.02 V  (1220-1840 mV, 20 mV steps)
    pub fn set_dcdc4_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mv = mv.max(500).min(1840);
        let n = if mv <= 1200 {
            ((mv - 500) / 10) as u8
        } else {
            (71 + (mv - 1220) / 20) as u8
        };
        self.write_register(i2c, registers::REG_DCDC4_VOLT, n & 0x7F)
    }

    /// Read DCDC4 voltage in millivolts.
    pub fn dcdc4_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_DCDC4_VOLT)?;
        let n = (reg & 0x7F) as u16;
        if n <= 70 {
            Ok(500 + n * 10)
        } else {
            Ok(1220 + (n - 71) * 20)
        }
    }

    // ---- Fuel gauge, watchdog, low-battery, data buffers -------------------

    /// Set the low-battery warning thresholds (REG 1Ah).
    ///
    /// Level 1: bits 3:0, range 0-15%.
    /// Level 2: bits 7:4, value = percent - 5, range 5-20%.
    pub fn set_low_battery_warning<I2C, E>(
        &self,
        i2c: &mut I2C,
        cfg: &types::LowBatteryWarning,
    ) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let l1 = cfg.level1_percent.min(15);
        let l2 = cfg.level2_percent.max(5).min(20) - 5;
        self.write_register(i2c, registers::REG_LOWBAT_WARN, (l2 << 4) | l1)
    }

    /// Read the low-battery warning thresholds.
    pub fn low_battery_warning<I2C, E>(&self, i2c: &mut I2C) -> Result<types::LowBatteryWarning, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_LOWBAT_WARN)?;
        Ok(types::LowBatteryWarning {
            level1_percent: reg & 0x0F,
            level2_percent: ((reg >> 4) & 0x0F) + 5,
        })
    }

    /// Reset the fuel gauge (REG 17h).
    ///
    /// Forces the gauge to re-learn battery capacity from scratch.
    /// The percentage reading will be inaccurate for several charge
    /// cycles after a reset.
    ///
    /// REG 17h bit 3 = "reset the gauge" (bit 2 would reset it
    /// "besides registers"); the previous implementation wrote
    /// bit 0, which is undocumented and did nothing.
    pub fn reset_fuel_gauge<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_GAUGE_RESET)?;
        self.write_register(i2c, registers::REG_GAUGE_RESET, reg | (1 << 3))
    }

    /// Configure the watchdog timer (REG 19h).
    pub fn set_watchdog<I2C, E>(&self, i2c: &mut I2C, cfg: &types::WatchdogConfig) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let val = ((cfg.action as u8) << 4) | (cfg.timeout as u8);
        self.write_register(i2c, registers::REG_WDT_CFG, val)
    }

    /// Read the watchdog configuration.
    pub fn watchdog<I2C, E>(&self, i2c: &mut I2C) -> Result<types::WatchdogConfig, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_WDT_CFG)?;
        Ok(types::WatchdogConfig {
            timeout: types::WatchdogTimeout::from_bits(r & 0x07),
            action:  types::WatchdogAction::from_bits((r >> 4) & 0x03),
        })
    }

    /// Enable or disable the watchdog (REG 18h bit 0).
    pub fn set_watchdog_enable<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_CHARGER_GAUGE_WDT_EN)?;
        let val = if enable { reg | 1 } else { reg & !1 };
        self.write_register(i2c, registers::REG_CHARGER_GAUGE_WDT_EN, val)
    }

    /// Read the power-on source that caused the last boot (REG 20h).
    pub fn power_on_status<I2C, E>(&self, i2c: &mut I2C) -> Result<types::PowerOnStatus, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_PWRON_STATUS)?;
        Ok(types::PowerOnStatus {
            en_mode:         (r & (1 << 5)) != 0,
            battery_insert:  (r & (1 << 4)) != 0,
            battery_charged: (r & (1 << 3)) != 0,
            vbus:            (r & (1 << 2)) != 0,
            irq_pin:         (r & (1 << 1)) != 0,
            button:          (r & (1 << 0)) != 0,
        })
    }

    /// Read the power-off source that caused the last shutdown (REG 21h).
    pub fn power_off_status<I2C, E>(&self, i2c: &mut I2C) -> Result<types::PowerOffStatus, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_PWROFF_STATUS)?;
        Ok(types::PowerOffStatus {
            die_overtemp:      (r & (1 << 7)) != 0,
            dcdc_overvolt:     (r & (1 << 6)) != 0,
            dcdc_undervolt:    (r & (1 << 5)) != 0,
            vbus_overvolt:     (r & (1 << 4)) != 0,
            vsys_undervolt:    (r & (1 << 3)) != 0,
            en_mode:           (r & (1 << 2)) != 0,
            software:          (r & (1 << 1)) != 0,
            button_long_press: (r & (1 << 0)) != 0,
        })
    }

    /// Write one of the four non-volatile data buffer bytes (REG 04h-07h).
    ///
    /// These bytes survive soft-resets (but not hard power cycles).
    /// `index` must be 0-3.
    pub fn write_data_buffer<I2C, E>(&self, i2c: &mut I2C, index: u8, value: u8) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        if index > 3 {
            return Err(Error::InvalidValue);
        }
        self.write_register(i2c, registers::REG_DATA_BUF0 + index, value)
    }

    /// Read one of the four non-volatile data buffer bytes (REG 04h-07h).
    pub fn read_data_buffer<I2C, E>(&self, i2c: &mut I2C, index: u8) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        if index > 3 {
            return Err(Error::InvalidValue);
        }
        self.read_register(i2c, registers::REG_DATA_BUF0 + index)
    }

    // ---- GPIO1 (REG 1Bh) --------------------------------------------------

    /// Set GPIO1 output mode (REG 1Bh bits 3:2).
    pub fn set_gpio1_output<I2C, E>(&self, i2c: &mut I2C, mode: types::Gpio1Output) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_GPIO1_CFG)?;
        self.write_register(i2c, registers::REG_GPIO1_CFG, (reg & !0x0C) | ((mode as u8) << 2))
    }

    /// Read GPIO1 output mode.
    pub fn gpio1_output<I2C, E>(&self, i2c: &mut I2C) -> Result<types::Gpio1Output, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_GPIO1_CFG)?;
        Ok(types::Gpio1Output::from_bits((reg >> 2) & 0x03))
    }

    // ---- PWROFF enable (REG 22h) -------------------------------------------

    /// Set power-off source enable configuration (REG 22h).
    pub fn set_power_off_enable<I2C, E>(&self, i2c: &mut I2C, cfg: &types::PowerOffEnable) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let val = if cfg.die_overtemp_en { 1 << 2 } else { 0 }
                | if cfg.button_off_en { 1 << 1 } else { 0 }
                | if cfg.button_off_restart { 1 } else { 0 };
        self.write_register(i2c, registers::REG_PWROFF_EN, val)
    }

    /// Read power-off source enable configuration.
    pub fn power_off_enable<I2C, E>(&self, i2c: &mut I2C) -> Result<types::PowerOffEnable, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_PWROFF_EN)?;
        Ok(types::PowerOffEnable {
            die_overtemp_en:    (r & (1 << 2)) != 0,
            button_off_en:      (r & (1 << 1)) != 0,
            button_off_restart: (r & (1 << 0)) != 0,
        })
    }

    // ---- DCDC OVP/UVP (REG 23h) -------------------------------------------

    /// Set DCDC over/under-voltage protection config (REG 23h).
    pub fn set_dcdc_protection<I2C, E>(&self, i2c: &mut I2C, cfg: &types::DcdcProtection) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let val = if cfg.overvolt_off_en { 1 << 5 } else { 0 }
                | (1 << 4) // reserved bit, default 1
                | if cfg.dcdc4_undervolt_en { 1 << 3 } else { 0 }
                | if cfg.dcdc3_undervolt_en { 1 << 2 } else { 0 }
                | if cfg.dcdc2_undervolt_en { 1 << 1 } else { 0 }
                | if cfg.dcdc1_undervolt_en { 1 } else { 0 };
        self.write_register(i2c, registers::REG_DCDC_OVP_UVP, val)
    }

    /// Read DCDC over/under-voltage protection config.
    pub fn dcdc_protection<I2C, E>(&self, i2c: &mut I2C) -> Result<types::DcdcProtection, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_DCDC_OVP_UVP)?;
        Ok(types::DcdcProtection {
            overvolt_off_en:    (r & (1 << 5)) != 0,
            dcdc4_undervolt_en: (r & (1 << 3)) != 0,
            dcdc3_undervolt_en: (r & (1 << 2)) != 0,
            dcdc2_undervolt_en: (r & (1 << 1)) != 0,
            dcdc1_undervolt_en: (r & (1 << 0)) != 0,
        })
    }

    // ---- Vsys power-off threshold (REG 24h) --------------------------------

    /// Set the Vsys voltage for power-off threshold (REG 24h bits 2:0).
    ///
    /// Voltage = 2.6 + N * 0.1 V, N = 0-7.
    /// Valid range: 2600-3300 mV.
    pub fn set_vsys_poweroff_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mv = mv.max(2600).min(3300);
        let n = ((mv - 2600) / 100) as u8;
        let reg = self.read_register(i2c, registers::REG_VSYS_PWROFF)?;
        self.write_register(i2c, registers::REG_VSYS_PWROFF, (reg & !0x07) | (n & 0x07))
    }

    /// Read the Vsys power-off threshold in millivolts.
    pub fn vsys_poweroff_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::REG_VSYS_PWROFF)?;
        Ok(2600 + (reg & 0x07) as u16 * 100)
    }

    // ---- PWROK settings (REG 25h) ------------------------------------------

    /// Set PWROK and power-off sequence config (REG 25h).
    pub fn set_pwrok_config<I2C, E>(&self, i2c: &mut I2C, cfg: &types::PwrokConfig) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let val = if cfg.check_pwrok_en { 1 << 4 } else { 0 }
                | if cfg.pwroff_delay_en { 1 << 3 } else { 0 }
                | if cfg.reverse_sequence { 1 << 2 } else { 0 }
                | (cfg.delay as u8);
        self.write_register(i2c, registers::REG_PWROK_CFG, val)
    }

    /// Read PWROK and power-off sequence config.
    pub fn pwrok_config<I2C, E>(&self, i2c: &mut I2C) -> Result<types::PwrokConfig, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_PWROK_CFG)?;
        Ok(types::PwrokConfig {
            check_pwrok_en:  (r & (1 << 4)) != 0,
            pwroff_delay_en: (r & (1 << 3)) != 0,
            reverse_sequence:(r & (1 << 2)) != 0,
            delay:           types::PwrokDelay::from_bits(r & 0x03),
        })
    }

    // ---- Sleep / wakeup (REG 26h) ------------------------------------------

    /// Set sleep and wakeup control (REG 26h).
    pub fn set_sleep_wake<I2C, E>(&self, i2c: &mut I2C, cfg: &types::SleepWakeConfig) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let val = if cfg.irq_wakeup_en { 1 << 4 } else { 0 }
                | if cfg.pwrok_low_on_wake { 1 << 3 } else { 0 }
                | if cfg.restore_voltage { 1 << 2 } else { 0 }
                | if cfg.wakeup_en { 1 << 1 } else { 0 }
                | if cfg.sleep_en { 1 } else { 0 };
        self.write_register(i2c, registers::REG_SLEEP_WAKEUP, val)
    }

    /// Read sleep and wakeup control.
    pub fn sleep_wake<I2C, E>(&self, i2c: &mut I2C) -> Result<types::SleepWakeConfig, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_SLEEP_WAKEUP)?;
        Ok(types::SleepWakeConfig {
            irq_wakeup_en:    (r & (1 << 4)) != 0,
            pwrok_low_on_wake:(r & (1 << 3)) != 0,
            restore_voltage:  (r & (1 << 2)) != 0,
            wakeup_en:        (r & (1 << 1)) != 0,
            sleep_en:         (r & (1 << 0)) != 0,
        })
    }

    // ---- Fast power-on sequence (REG 28h-2Bh) ------------------------------

    /// Write a fast power-on register directly (REG 28h-2Bh).
    ///
    /// `index` 0-3 maps to REG 28h-2Bh. Each register packs four
    /// 2-bit sequence codes for different rails. See the datasheet
    /// for the rail-to-bit mapping per register.
    pub fn write_fast_pwron<I2C, E>(&self, i2c: &mut I2C, index: u8, value: u8) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        if index > 3 {
            return Err(Error::InvalidValue);
        }
        self.write_register(i2c, registers::REG_FAST_PWRON0 + index, value)
    }

    /// Read a fast power-on register (REG 28h-2Bh).
    pub fn read_fast_pwron<I2C, E>(&self, i2c: &mut I2C, index: u8) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        if index > 3 {
            return Err(Error::InvalidValue);
        }
        self.read_register(i2c, registers::REG_FAST_PWRON0 + index)
    }

    // ---- TS pin (REG 50h, 52h-57h) -----------------------------------------

    /// Set TS pin configuration (REG 50h).
    pub fn set_ts_pin_config<I2C, E>(&self, i2c: &mut I2C, cfg: &types::TsPinConfig) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let val = ((cfg.function as u8) << 4)
                | ((cfg.current_mode as u8) << 2)
                | (cfg.current_value as u8);
        self.write_register(i2c, registers::REG_TS_CFG, val)
    }

    /// Read TS pin configuration.
    pub fn ts_pin_config<I2C, E>(&self, i2c: &mut I2C) -> Result<types::TsPinConfig, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r = self.read_register(i2c, registers::REG_TS_CFG)?;
        Ok(types::TsPinConfig {
            function:      if (r & (1 << 4)) != 0 { types::TsPinFunction::ExternalInput } else { types::TsPinFunction::BatteryTemp },
            current_mode:  types::TsCurrentMode::from_bits((r >> 2) & 0x03),
            current_value: types::TsCurrentValue::from_bits(r & 0x03),
        })
    }

    /// Set TS hysteresis low-to-high (REG 52h). Value = N * 16 mV.
    pub fn set_ts_hys_l2h<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.write_register(i2c, registers::REG_TS_HYSL2H, value)
    }

    /// Read TS hysteresis low-to-high (REG 52h).
    pub fn ts_hys_l2h<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_register(i2c, registers::REG_TS_HYSL2H)
    }

    /// Set TS hysteresis high-to-low (REG 53h). Value = N * 4 mV.
    pub fn set_ts_hys_h2l<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.write_register(i2c, registers::REG_TS_HYSH2L, value)
    }

    /// Read TS hysteresis high-to-low (REG 53h).
    pub fn ts_hys_h2l<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_register(i2c, registers::REG_TS_HYSH2L)
    }

    /// Set VLTF charge threshold (REG 54h). Value = N * 32 mV (~0 deg C default).
    pub fn set_vltf_charge<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.write_register(i2c, registers::REG_VLTF_CHG, value)
    }

    /// Read VLTF charge threshold (REG 54h).
    pub fn vltf_charge<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_register(i2c, registers::REG_VLTF_CHG)
    }

    /// Set VHTF charge threshold (REG 55h). Value = N * 2 mV (~55 deg C default).
    pub fn set_vhtf_charge<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.write_register(i2c, registers::REG_VHTF_CHG, value)
    }

    /// Read VHTF charge threshold (REG 55h).
    pub fn vhtf_charge<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_register(i2c, registers::REG_VHTF_CHG)
    }

    /// Set VLTF work threshold (REG 56h). Value = N * 32 mV (~-10 deg C default).
    pub fn set_vltf_work<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.write_register(i2c, registers::REG_VLTF_WORK, value)
    }

    /// Read VLTF work threshold (REG 56h).
    pub fn vltf_work<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_register(i2c, registers::REG_VLTF_WORK)
    }

    /// Set VHTF work threshold (REG 57h). Value = N * 2 mV (~60 deg C default).
    pub fn set_vhtf_work<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.write_register(i2c, registers::REG_VHTF_WORK, value)
    }

    /// Read VHTF work threshold (REG 57h).
    pub fn vhtf_work<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_register(i2c, registers::REG_VHTF_WORK)
    }

    // ---- JEITA (REG 58h-5Bh) -----------------------------------------------

    /// Enable or disable JEITA standard (REG 58h bit 0).
    pub fn set_jeita_enable<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let reg = self.read_register(i2c, registers::REG_JEITA_EN)?;
        self.write_register(i2c, registers::REG_JEITA_EN, if enable { reg | 1 } else { reg & !1 })
    }

    /// Read JEITA enable state.
    pub fn jeita_enabled<I2C, E>(&self, i2c: &mut I2C) -> Result<bool, Error<E>>
    where I2C: I2cTrait<Error = E> {
        let reg = self.read_register(i2c, registers::REG_JEITA_EN)?;
        Ok((reg & 1) != 0)
    }

    /// Set JEITA CV configuration (REG 59h).
    pub fn set_jeita_cv_config<I2C, E>(&self, i2c: &mut I2C, cfg: &types::JeitaCvConfig) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let val = ((cfg.warm_current as u8) << 6)
                | ((cfg.cool_current as u8) << 4)
                | ((cfg.warm_voltage as u8) << 2)
                | (cfg.cool_voltage as u8);
        self.write_register(i2c, registers::REG_JEITA_CV_CFG, val)
    }

    /// Read JEITA CV configuration.
    pub fn jeita_cv_config<I2C, E>(&self, i2c: &mut I2C) -> Result<types::JeitaCvConfig, Error<E>>
    where I2C: I2cTrait<Error = E> {
        let r = self.read_register(i2c, registers::REG_JEITA_CV_CFG)?;
        Ok(types::JeitaCvConfig {
            warm_current: if (r & (1 << 6)) != 0 { types::JeitaCurrentFall::Half } else { types::JeitaCurrentFall::Full },
            cool_current: if (r & (1 << 4)) != 0 { types::JeitaCurrentFall::Half } else { types::JeitaCurrentFall::Full },
            warm_voltage: types::JeitaVoltageFall::from_bits((r >> 2) & 0x03),
            cool_voltage: types::JeitaVoltageFall::from_bits(r & 0x03),
        })
    }

    /// Set JEITA cool temperature threshold (REG 5Ah). Value = N * 16 mV (~10 deg C default).
    pub fn set_jeita_cool_threshold<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.write_register(i2c, registers::REG_JEITA_COOL, value)
    }

    /// Read JEITA cool temperature threshold (REG 5Ah).
    pub fn jeita_cool_threshold<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_register(i2c, registers::REG_JEITA_COOL)
    }

    /// Set JEITA warm temperature threshold (REG 5Bh). Value = N * 8 mV (~45 deg C default).
    pub fn set_jeita_warm_threshold<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.write_register(i2c, registers::REG_JEITA_WARM, value)
    }

    /// Read JEITA warm temperature threshold (REG 5Bh).
    pub fn jeita_warm_threshold<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_register(i2c, registers::REG_JEITA_WARM)
    }

    /// Read the MCU-configured TS voltage (REG 5Ch-5Dh).
    pub fn ts_cfg_data<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_adc_14bit(i2c, registers::REG_TS_CFG_H, registers::REG_TS_CFG_L)
    }

    // ---- Charger timeout (REG 67h) -----------------------------------------

    /// Set charger timeout configuration (REG 67h).
    pub fn set_charger_timeout<I2C, E>(&self, i2c: &mut I2C, cfg: &types::ChargerTimeout) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let val = if cfg.slow_during_dpm { 1 << 7 } else { 0 }
                | if cfg.done_timer_en { 1 << 6 } else { 0 }
                | ((cfg.done_timeout as u8) << 4)
                | if cfg.precharge_timer_en { 1 << 2 } else { 0 }
                | (cfg.precharge_timeout as u8);
        self.write_register(i2c, registers::REG_CHG_TIMER, val)
    }

    /// Read charger timeout configuration.
    pub fn charger_timeout<I2C, E>(&self, i2c: &mut I2C) -> Result<types::ChargerTimeout, Error<E>>
    where I2C: I2cTrait<Error = E> {
        let r = self.read_register(i2c, registers::REG_CHG_TIMER)?;
        Ok(types::ChargerTimeout {
            slow_during_dpm:    (r & (1 << 7)) != 0,
            done_timer_en:      (r & (1 << 6)) != 0,
            done_timeout:       types::ChargeDoneTimeout::from_bits((r >> 4) & 0x03),
            precharge_timer_en: (r & (1 << 2)) != 0,
            precharge_timeout:  types::PreChargeTimeout::from_bits(r & 0x03),
        })
    }

    // ---- CHGLED (REG 69h) --------------------------------------------------

    /// Set CHGLED configuration (REG 69h).
    pub fn set_chgled_config<I2C, E>(&self, i2c: &mut I2C, cfg: &types::ChgLedConfig) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let val = ((cfg.output as u8) << 4)
                | ((cfg.function as u8) << 1)
                | if cfg.enabled { 1 } else { 0 };
        self.write_register(i2c, registers::REG_CHGLED, val)
    }

    /// Read CHGLED configuration.
    pub fn chgled_config<I2C, E>(&self, i2c: &mut I2C) -> Result<types::ChgLedConfig, Error<E>>
    where I2C: I2cTrait<Error = E> {
        let r = self.read_register(i2c, registers::REG_CHGLED)?;
        Ok(types::ChgLedConfig {
            output:   types::ChgLedOutput::from_bits((r >> 4) & 0x03),
            function: types::ChgLedFunction::from_bits((r >> 1) & 0x03),
            enabled:  (r & 1) != 0,
        })
    }

    // ---- DCDC force PWM (REG 81h) ------------------------------------------

    /// Set DCDC PWM/PFM and frequency spread config (REG 81h).
    pub fn set_dcdc_pwm_config<I2C, E>(&self, i2c: &mut I2C, cfg: &types::DcdcPwmConfig) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let val = if cfg.freq_spread_en { 1 << 7 } else { 0 }
                | if cfg.freq_spread_100k { 1 << 6 } else { 0 }
                | if cfg.dcdc4_force_pwm { 1 << 5 } else { 0 }
                | if cfg.dcdc3_force_pwm { 1 << 4 } else { 0 }
                | if cfg.dcdc2_force_pwm { 1 << 3 } else { 0 }
                | if cfg.dcdc1_force_pwm { 1 << 2 } else { 0 }
                | (cfg.uvp_debounce as u8);
        self.write_register(i2c, registers::REG_DCDC_PWM, val)
    }

    /// Read DCDC PWM/PFM and frequency spread config.
    pub fn dcdc_pwm_config<I2C, E>(&self, i2c: &mut I2C) -> Result<types::DcdcPwmConfig, Error<E>>
    where I2C: I2cTrait<Error = E> {
        let r = self.read_register(i2c, registers::REG_DCDC_PWM)?;
        Ok(types::DcdcPwmConfig {
            freq_spread_en:  (r & (1 << 7)) != 0,
            freq_spread_100k:(r & (1 << 6)) != 0,
            dcdc4_force_pwm: (r & (1 << 5)) != 0,
            dcdc3_force_pwm: (r & (1 << 4)) != 0,
            dcdc2_force_pwm: (r & (1 << 3)) != 0,
            dcdc1_force_pwm: (r & (1 << 2)) != 0,
            uvp_debounce:    types::DcdcUvpDebounce::from_bits(r & 0x03),
        })
    }

    // ---- Additional LDO voltage settings (REG 98h-9Ah) ---------------------

    /// Set CPUSLDO voltage (REG 98h bits 4:0).
    ///
    /// 0.5-1.4V, 50 mV/step, 20 steps.
    pub fn set_cpusldo_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let mv = mv.max(500).min(1400);
        let n = ((mv - 500) / 50) as u8;
        self.write_register(i2c, registers::REG_CPUSLDO_VOLT, n & 0x1F)
    }

    /// Read CPUSLDO voltage in millivolts.
    pub fn cpusldo_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where I2C: I2cTrait<Error = E> {
        let reg = self.read_register(i2c, registers::REG_CPUSLDO_VOLT)?;
        Ok(500 + (reg & 0x1F) as u16 * 50)
    }

    /// Set DLDO1 voltage (REG 99h bits 4:0).
    ///
    /// 0.5-3.4V, 100 mV/step, 29 steps.
    pub fn set_dldo1_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let mv = mv.max(500).min(3400);
        let n = ((mv - 500) / 100) as u8;
        self.write_register(i2c, registers::REG_DLDO1_VOLT, n.min(0x1C) & 0x1F)
    }

    /// Read DLDO1 voltage in millivolts.
    pub fn dldo1_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where I2C: I2cTrait<Error = E> {
        let reg = self.read_register(i2c, registers::REG_DLDO1_VOLT)?;
        Ok(500 + (reg & 0x1F) as u16 * 100)
    }

    /// Set DLDO2 voltage (REG 9Ah bits 4:0).
    ///
    /// 0.5-1.4V, 50 mV/step, 20 steps.
    pub fn set_dldo2_voltage<I2C, E>(&self, i2c: &mut I2C, mv: u16) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let mv = mv.max(500).min(1400);
        let n = ((mv - 500) / 50) as u8;
        self.write_register(i2c, registers::REG_DLDO2_VOLT, n & 0x1F)
    }

    /// Read DLDO2 voltage in millivolts.
    pub fn dldo2_voltage<I2C, E>(&self, i2c: &mut I2C) -> Result<u16, Error<E>>
    where I2C: I2cTrait<Error = E> {
        let reg = self.read_register(i2c, registers::REG_DLDO2_VOLT)?;
        Ok(500 + (reg & 0x1F) as u16 * 50)
    }

    // ---- Fuel gauge (REG A1h, A2h) -----------------------------------------

    /// Read the battery parameter ROM value (REG A1h, read-only).
    pub fn battery_parameter<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where I2C: I2cTrait<Error = E> {
        self.read_register(i2c, registers::REG_BAT_PARAM)
    }

    /// Set fuel gauge ROM/SRAM select (REG A2h bit 4).
    /// false = ROM, true = SRAM.
    pub fn set_gauge_rom_select<I2C, E>(&self, i2c: &mut I2C, use_sram: bool) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let reg = self.read_register(i2c, registers::REG_GAUGE_CFG)?;
        let val = if use_sram { reg | (1 << 4) } else { reg & !(1 << 4) };
        self.write_register(i2c, registers::REG_GAUGE_CFG, val)
    }

    /// Set fuel gauge BROM writer control (REG A2h bit 0).
    pub fn set_gauge_brom_write<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where I2C: I2cTrait<Error = E> {
        let reg = self.read_register(i2c, registers::REG_GAUGE_CFG)?;
        let val = if enable { reg | 1 } else { reg & !1 };
        self.write_register(i2c, registers::REG_GAUGE_CFG, val)
    }

    // ---- private helpers ----------------------------------------------------

    fn read_register<I2C, E>(&self, i2c: &mut I2C, reg: u8) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mut buf = [0u8; 1];
        i2c.write_read(self.addr, &[reg], &mut buf)
            .map_err(Error::I2c)?;
        Ok(buf[0])
    }

    fn write_register<I2C, E>(&self, i2c: &mut I2C, reg: u8, val: u8) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        i2c.write(self.addr, &[reg, val])
            .map_err(Error::I2c)
    }

    /// Convert millivolts to the 5-bit AXP2101 register value.
    /// Formula: `(mV − 500) / 100`. Valid range: 500–3500 mV.
    fn mv_to_register<E>(millivolts: u16) -> Result<u8, Error<E>> {
        if millivolts < 500 || millivolts > 3500 {
            return Err(Error::InvalidValue);
        }
        Ok(((millivolts - 500) / 100) as u8)
    }

    /// Read a 14-bit ADC value from a high/low register pair.
    /// High byte bits 5:0 = value[13:8], low byte = value[7:0].
    fn read_adc_14bit<I2C, E>(&self, i2c: &mut I2C, reg_h: u8, reg_l: u8) -> Result<u16, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let hi = self.read_register(i2c, reg_h)? as u16;
        let lo = self.read_register(i2c, reg_l)? as u16;
        Ok((hi << 8) | lo)
    }
}

// Re-export frequently used types for convenience.
pub use interrupts::{InterruptConfig, InterruptSource, InterruptStatus};
pub use types::*;
