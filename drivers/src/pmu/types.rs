//! Public data types for the AXP2101 PMU driver.
//!
//! Status snapshots, configuration structs, and enums for charger,
//! input limits, power key timing, and other PMU subsystems.

// ---- Status registers (REG 00h-01h) ----------------------------------------

/// Snapshot of PMU Status 1 (REG 00h) - power-path and battery state.
///
/// Bit layout (from XPowersLib / AXP2101 datasheet):
///   7:6  unused
///   5    VBUS good (present and above threshold)
///   4    BATFET state (on/off)
///   3    Battery present (detected by charger)
///   2    Battery in active mode
///   1    Thermal regulation active
///   0    Input current limit active
#[derive(Debug, Clone, Copy, Default)]
pub struct PmuStatus1 {
    /// VBUS is present and above the VBUS good threshold (bit 5).
    pub vbus_good: bool,
    /// BATFET is on (bit 4).
    pub batfet_active: bool,
    /// Battery is present (detected by the charger) (bit 3).
    pub battery_present: bool,
    /// Battery is in active mode (bit 2).
    pub battery_active: bool,
    /// Thermal regulation is active (die at limit) (bit 1).
    pub thermal_active: bool,
    /// Input current limit is active (bit 0).
    pub current_limit_active: bool,
}

/// Snapshot of PMU Status 2 (REG 01h) - charging state and system status.
///
/// Bit layout (from AXP2101 datasheet):
///   7      reserved (RO, 0)
///   6:5    Battery current direction
///            00=standby, 01=charge, 10=discharge, 11=reserved
///   4      System status: 0=power off, 1=power on
///   3      VINDPM status: 0=not in VINDPM, 1=VINDPM active
///   2:0    Charging status
///            000=tri_charge, 001=pre_charge, 010=CC, 011=CV,
///            100=charge done, 101=not charging, 11X=reserved
#[derive(Debug, Clone, Copy, Default)]
pub struct PmuStatus2 {
    /// Battery current direction from bits 6:5.
    pub current_direction: CurrentDirection,
    /// System is powered on (bit 4).
    pub system_on: bool,
    /// VINDPM regulation is active (bit 3).
    pub vindpm_active: bool,
    /// Detailed charger phase from bits 2:0.
    pub charger_phase: ChargerPhase,
}

/// Battery current direction, from REG 01h bits 6:5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CurrentDirection {
    /// No current flow (standby).
    #[default]
    Standby = 0,
    /// Battery is being charged.
    Charging = 1,
    /// Battery is discharging.
    Discharging = 2,
}

impl CurrentDirection {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Self::Charging,
            2 => Self::Discharging,
            _ => Self::Standby,
        }
    }
}

/// Detailed charger phase, from REG 01h bits 2:0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChargerPhase {
    /// Tri-charge (pre-charge below threshold).
    TriCharge = 0,
    /// Pre-charge phase.
    PreCharge = 1,
    /// Constant-current (CC) phase.
    ConstantCurrent = 2,
    /// Constant-voltage (CV) phase.
    ConstantVoltage = 3,
    /// Charge done.
    Done = 4,
    /// Not charging.
    #[default]
    NotCharging = 5,
}

impl ChargerPhase {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::TriCharge,
            1 => Self::PreCharge,
            2 => Self::ConstantCurrent,
            3 => Self::ConstantVoltage,
            4 => Self::Done,
            _ => Self::NotCharging,
        }
    }
}

// ---- Input limits ----------------------------------------------------------

/// Input current limit (REG 16h bits 2:0).
///
/// Limits the maximum current drawn from VBUS. Higher values allow
/// faster charging but require a capable USB source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputCurrentLimit {
    Ma100  = 0,
    Ma500  = 1,
    Ma900  = 2,
    Ma1000 = 3,
    Ma1500 = 4,
    Ma2000 = 5,
}

impl InputCurrentLimit {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x07 {
            0 => Self::Ma100,
            1 => Self::Ma500,
            2 => Self::Ma900,
            3 => Self::Ma1000,
            4 => Self::Ma1500,
            _ => Self::Ma2000,
        }
    }
}

// ---- Charger configuration -------------------------------------------------

/// Pre-charge current (REG 61h bits 3:0).
/// Value in mA = 25 * N, where N = register value (0-15).
/// Range: 0 mA (disabled) to 375 mA in 25 mA steps.
/// Default after reset: 0 (disabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreChargeCurrent(pub u8);

impl PreChargeCurrent {
    /// Create from milliamps (clamped to 0-375, rounded down to 25 mA steps).
    pub fn from_ma(ma: u16) -> Self {
        Self(((ma.min(375)) / 25) as u8)
    }

    /// Return the current in milliamps.
    pub fn as_ma(self) -> u16 {
        self.0 as u16 * 25
    }
}

/// Constant-current charge current (REG 62h bits 4:0).
///
/// Datasheet V1.4 (6.13.2.60) mapping:
///   N <= 8:  25 * N mA          (0, 25, 50 ... 200)
///   N > 8:   200 + (N-8) * 100  (300, 400 ... 1000 at N = 16)
///   N > 16:  reserved
///
/// (An earlier version of this type assumed `25 + N*25` and
/// code 8 = 300 mA - one code off in BOTH ranges, so every set and
/// decoded current was wrong: "100 mA" wrote 75, "300 mA" wrote
/// 200. Caught by a datasheet audit 2026-08-19.)
///
/// Use `from_ma()` to find the closest register value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeCurrent(pub u8);

impl ChargeCurrent {
    /// Create from milliamps. Picks the closest value that does not
    /// exceed the requested current. Clamped to 1000 mA (codes
    /// above it are reserved); 201-299 mA has no code and clamps
    /// to 200.
    pub fn from_ma(ma: u16) -> Self {
        let ma = ma.min(1000);
        let reg = if ma < 300 {
            (ma.min(200) / 25) as u8
        } else {
            // 100 mA steps starting at code 9 = 300 mA
            (8 + (ma - 200) / 100) as u8
        };
        Self(reg.min(16))
    }

    /// Return the current in milliamps.
    pub fn as_ma(self) -> u16 {
        let n = self.0 as u16;
        if n <= 8 {
            25 * n
        } else {
            200 + (n - 8) * 100 // code 9 = 300, 10 = 400, ...
        }
    }
}

/// Termination current (REG 63h bits 3:0).
/// Value in mA = 25 * N. Codes 0-8 (0-200 mA) are valid; 9-15 are
/// reserved per datasheet V1.4 (6.13.2.61), so requests clamp to
/// 200 mA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminationCurrent(pub u8);

impl TerminationCurrent {
    pub fn from_ma(ma: u16) -> Self {
        Self(((ma.min(200)) / 25) as u8)
    }

    pub fn as_ma(self) -> u16 {
        self.0 as u16 * 25
    }
}

/// Constant-voltage charge target (REG 64h bits 2:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeVoltage {
    /// 4.0 V
    V4_0 = 1,
    /// 4.1 V
    V4_1 = 2,
    /// 4.2 V (default, standard Li-ion)
    V4_2 = 3,
    /// 4.35 V
    V4_35 = 4,
    /// 4.4 V
    V4_4 = 5,
}

impl ChargeVoltage {
    pub(crate) fn from_bits(bits: u8) -> Option<Self> {
        match bits & 0x07 {
            1 => Some(Self::V4_0),
            2 => Some(Self::V4_1),
            3 => Some(Self::V4_2),
            4 => Some(Self::V4_35),
            5 => Some(Self::V4_4),
            _ => None,
        }
    }
}

/// Battery voltage for power-off (VOFF, REG 24h bits 2:0).
/// 2.6-3.3 V in 0.1 V steps (0b000 = 2.6 V ... 0b111 = 3.3 V).
///
/// The power-on default is 2.6 V - deep-discharge territory for a
/// Li-ion cell, and every collapse there wipes the fuel gauge's
/// learned state (watch-observed as 0% -> dead -> 87%-on-charger).
/// Daily-driver firmware raises this at init.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerOffVoltage(pub u8);

impl PowerOffVoltage {
    /// Create from millivolts (clamped to 2600-3300, rounded down
    /// to 100 mV steps).
    pub fn from_mv(mv: u16) -> Self {
        Self(((mv.clamp(2600, 3300) - 2600) / 100) as u8)
    }

    /// Return the threshold in millivolts.
    pub fn as_mv(self) -> u16 {
        2600 + self.0 as u16 * 100
    }
}

/// Thermal regulation threshold (REG 65h bits 1:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalThreshold {
    C60  = 0,
    C80  = 1,
    C100 = 2,
    C120 = 3,
}

impl ThermalThreshold {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::C60,
            1 => Self::C80,
            2 => Self::C100,
            _ => Self::C120,
        }
    }
}

// ---- Power key timing (REG 27h) --------------------------------------------

/// POWERON key press duration for IRQ (REG 27h bits 5:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerKeyIrqTime {
    Ms1000 = 0,
    Ms1500 = 1,
    Ms2000 = 2,
    Ms2500 = 3,
}

impl PowerKeyIrqTime {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Ms1000,
            1 => Self::Ms1500,
            2 => Self::Ms2000,
            _ => Self::Ms2500,
        }
    }
}

/// POWERON key press duration for power-off (REG 27h bits 3:2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerKeyOffTime {
    S4  = 0,
    S6  = 1,
    S8  = 2,
    S10 = 3,
}

impl PowerKeyOffTime {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::S4,
            1 => Self::S6,
            2 => Self::S8,
            _ => Self::S10,
        }
    }
}

/// POWERON key press duration for power-on (REG 27h bits 1:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerKeyOnTime {
    Ms128  = 0,
    Ms512  = 1,
    Ms1000 = 2,
    Ms2000 = 3,
}

impl PowerKeyOnTime {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Ms128,
            1 => Self::Ms512,
            2 => Self::Ms1000,
            _ => Self::Ms2000,
        }
    }
}

/// Power key timing configuration (REG 27h).
#[derive(Debug, Clone, Copy)]
pub struct PowerKeyConfig {
    /// Duration of POWERON press to generate the long-press IRQ.
    pub irq_time: PowerKeyIrqTime,
    /// Duration of POWERON press to trigger power-off.
    pub off_time: PowerKeyOffTime,
    /// Duration of POWERON press to trigger power-on (from off state).
    pub on_time: PowerKeyOnTime,
}

// ---- Watchdog (REG 19h) ----------------------------------------------------

/// Watchdog timeout (REG 19h bits 2:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogTimeout {
    S1   = 0,
    S2   = 1,
    S4   = 2,
    S8   = 3,
    S16  = 4,
    S32  = 5,
    S64  = 6,
    S128 = 7,
}

impl WatchdogTimeout {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x07 {
            0 => Self::S1,
            1 => Self::S2,
            2 => Self::S4,
            3 => Self::S8,
            4 => Self::S16,
            5 => Self::S32,
            6 => Self::S64,
            _ => Self::S128,
        }
    }
}

/// Watchdog reset action (REG 19h bits 5:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Generate IRQ only.
    Irq = 0,
    /// Generate IRQ then reset system after another timeout.
    IrqThenReset = 1,
    /// Reset system immediately.
    Reset = 2,
    /// Reset system and pull PWROK low.
    ResetPwrok = 3,
}

impl WatchdogAction {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Irq,
            1 => Self::IrqThenReset,
            2 => Self::Reset,
            _ => Self::ResetPwrok,
        }
    }
}

/// Watchdog configuration (REG 19h).
#[derive(Debug, Clone, Copy)]
pub struct WatchdogConfig {
    pub timeout: WatchdogTimeout,
    pub action: WatchdogAction,
}

// ---- Low battery warning (REG 1Ah) -----------------------------------------

/// Low battery warning thresholds.
///
/// The AXP2101 generates SocWarningLevel1 and SocWarningLevel2
/// interrupts when battery SOC drops below these thresholds.
///
/// Level 1 range: 0-15% (bits 3:0).
/// Level 2 range: 5-20% (bits 7:4, value = 5 + N).
#[derive(Debug, Clone, Copy)]
pub struct LowBatteryWarning {
    /// SOC threshold for warning level 1 (0-15%).
    pub level1_percent: u8,
    /// SOC threshold for warning level 2 (5-20%).
    pub level2_percent: u8,
}

// ---- Die temperature protection (REG 13h) ----------------------------------

/// Die over-temperature protection level (REG 13h bits 2:1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DieOtpLevel {
    C115 = 0,
    C125 = 1,
    C135 = 2,
}

impl DieOtpLevel {
    #[allow(dead_code)] // reserved for future read_die_temp_protection() getter
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::C115,
            1 => Self::C125,
            _ => Self::C135,
        }
    }
}

// ---- Power-on / power-off status (REG 20h-21h) ----------------------------

/// Sources that caused the last power-on (REG 20h).
///
/// Bit layout:
///   7:6  reserved
///   5    POWERON always high when EN mode
///   4    Battery insert and good
///   3    Battery voltage > 3.3V when charged
///   2    VBUS insert and good
///   1    IRQ pin pull-down
///   0    POWERON low for on-level (button press)
#[derive(Debug, Clone, Copy, Default)]
pub struct PowerOnStatus {
    /// POWERON always high when EN mode (bit 5).
    pub en_mode: bool,
    /// Battery insert and good as power-on source (bit 4).
    pub battery_insert: bool,
    /// Battery voltage > 3.3V when charged as source (bit 3).
    pub battery_charged: bool,
    /// VBUS insert and good as power-on source (bit 2).
    pub vbus: bool,
    /// IRQ pin pull-down as power-on source (bit 1).
    pub irq_pin: bool,
    /// POWERON button press (low for on-level) (bit 0).
    pub button: bool,
}

/// Sources that caused the last power-off (REG 21h).
///
/// Bit layout:
///   7    Die over-temperature
///   6    DCDC over-voltage
///   5    DCDC under-voltage
///   4    VBUS over-voltage
///   3    Vsys under-voltage
///   2    POWERON always low when EN mode
///   1    Software configuration (soft power-off)
///   0    POWERON pull-down for off-level (long press)
#[derive(Debug, Clone, Copy, Default)]
pub struct PowerOffStatus {
    /// Die over-temperature as power-off source (bit 7).
    pub die_overtemp: bool,
    /// DCDC over-voltage as power-off source (bit 6).
    pub dcdc_overvolt: bool,
    /// DCDC under-voltage as power-off source (bit 5).
    pub dcdc_undervolt: bool,
    /// VBUS over-voltage as power-off source (bit 4).
    pub vbus_overvolt: bool,
    /// Vsys under-voltage as power-off source (bit 3).
    pub vsys_undervolt: bool,
    /// POWERON always low when EN mode (bit 2).
    pub en_mode: bool,
    /// Software configuration as power-off source (bit 1).
    pub software: bool,
    /// POWERON button long press (off-level) (bit 0).
    pub button_long_press: bool,
}

// ---- GPIO1 output (REG 1Bh) -----------------------------------------------

/// GPIO1 output mode (REG 1Bh bits 3:2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gpio1Output {
    /// High-impedance (floating).
    HiZ = 0,
    /// Drive low.
    Low = 1,
}

impl Gpio1Output {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Self::Low,
            _ => Self::HiZ,
        }
    }
}

// ---- PWROFF enable (REG 22h) -----------------------------------------------

/// Power-off source enable configuration (REG 22h).
///
/// Bit layout:
///   7:3  reserved
///   2    Die over-temp level 2 as power-off source enable
///   1    POWERON > OFFLEVEL as power-off source enable
///   0    Function select when button power-off occurs:
///        0=power-off, 1=restart
#[derive(Debug, Clone, Copy)]
pub struct PowerOffEnable {
    /// Die over-temperature level 2 triggers power-off (bit 2).
    pub die_overtemp_en: bool,
    /// POWERON long press triggers power-off (bit 1).
    pub button_off_en: bool,
    /// When button power-off occurs: false=power-off, true=restart (bit 0).
    pub button_off_restart: bool,
}

// ---- DCDC OVP/UVP (REG 23h) -----------------------------------------------

/// DCDC over/under-voltage power-off control (REG 23h).
///
/// Bit layout:
///   7:6  reserved
///   5    DCDC 120%(130%) over-voltage turn off PMIC
///   4    reserved
///   3    DCDC4 85% under-voltage turn off PMIC
///   2    DCDC3 85% under-voltage turn off PMIC
///   1    DCDC2 85% under-voltage turn off PMIC
///   0    DCDC1 85% under-voltage turn off PMIC
#[derive(Debug, Clone, Copy)]
pub struct DcdcProtection {
    /// DCDC over-voltage (120%/130%) triggers power-off (bit 5).
    pub overvolt_off_en: bool,
    /// DCDC4 under-voltage (85%) triggers power-off (bit 3).
    pub dcdc4_undervolt_en: bool,
    /// DCDC3 under-voltage (85%) triggers power-off (bit 2).
    pub dcdc3_undervolt_en: bool,
    /// DCDC2 under-voltage (85%) triggers power-off (bit 1).
    pub dcdc2_undervolt_en: bool,
    /// DCDC1 under-voltage (85%) triggers power-off (bit 0).
    pub dcdc1_undervolt_en: bool,
}

// ---- PWROK settings (REG 25h) ----------------------------------------------

/// PWROK delay after all outputs good (REG 25h bits 1:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PwrokDelay {
    Ms8  = 0,
    Ms16 = 1,
    Ms32 = 2,
    Ms64 = 3,
}

impl PwrokDelay {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Ms8,
            1 => Self::Ms16,
            2 => Self::Ms32,
            _ => Self::Ms64,
        }
    }
}

/// PWROK and power-off sequence control (REG 25h).
///
/// Bit layout:
///   7:5  reserved
///   4    Check PWROK pin 128ms after all outputs valid
///   3    Power-off delay 4ms after PWROK disable
///   2    Power-off sequence: 0=simultaneous, 1=reverse of startup
///   1:0  PWROK delay after all outputs good
#[derive(Debug, Clone, Copy)]
pub struct PwrokConfig {
    /// Check PWROK pin 128ms after outputs valid (bit 4).
    pub check_pwrok_en: bool,
    /// 4ms power-off delay after PWROK disable (bit 3).
    pub pwroff_delay_en: bool,
    /// Power-off in reverse sequence of startup (bit 2).
    pub reverse_sequence: bool,
    /// PWROK delay after all outputs good (bits 1:0).
    pub delay: PwrokDelay,
}

// ---- Sleep/wakeup (REG 26h) ------------------------------------------------

/// Sleep and wakeup control (REG 26h).
///
/// Bit layout:
///   7:5  reserved
///   4    IRQ pin low to wakeup enable
///   3    PWROK low-level enable when wakeup
///   2    DCDC/LDO voltage select on wakeup:
///        0=default, 1=voltage before sleep
///   1    Wakeup enable (RWLC - read/write, latch on clear)
///   0    Sleep enable (RWLC)
#[derive(Debug, Clone, Copy)]
pub struct SleepWakeConfig {
    /// IRQ pin pull-down triggers wakeup (bit 4).
    pub irq_wakeup_en: bool,
    /// PWROK goes low during wakeup (bit 3).
    pub pwrok_low_on_wake: bool,
    /// Restore pre-sleep voltage on wakeup (bit 2).
    pub restore_voltage: bool,
    /// Wakeup enable (bit 1).
    pub wakeup_en: bool,
    /// Sleep enable (bit 0).
    pub sleep_en: bool,
}

// ---- TS pin control (REG 50h) ----------------------------------------------

/// TS pin function select (REG 50h bit 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsPinFunction {
    /// Battery temperature sensor input (affects charger).
    BatteryTemp = 0,
    /// External fixed input (does not affect charger).
    ExternalInput = 1,
}

/// TS current source mode (REG 50h bits 3:2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsCurrentMode {
    /// Current source off.
    Off = 0,
    /// On when TS ADC channel is enabled.
    OnWhenAdcEnabled = 1,
    /// On only when TS channel is working, off for other channels.
    OnDuringTsSample = 2,
    /// Always on.
    AlwaysOn = 3,
}

impl TsCurrentMode {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Off,
            1 => Self::OnWhenAdcEnabled,
            2 => Self::OnDuringTsSample,
            _ => Self::AlwaysOn,
        }
    }
}

/// TS pin current source value (REG 50h bits 1:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsCurrentValue {
    Ua20 = 0,
    Ua40 = 1,
    Ua50 = 2,
    Ua60 = 3,
}

impl TsCurrentValue {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Ua20,
            1 => Self::Ua40,
            2 => Self::Ua50,
            _ => Self::Ua60,
        }
    }
}

/// TS pin configuration (REG 50h).
#[derive(Debug, Clone, Copy)]
pub struct TsPinConfig {
    /// TS pin function: battery temp sensor or external input (bit 4).
    pub function: TsPinFunction,
    /// Current source on/off mode (bits 3:2).
    pub current_mode: TsCurrentMode,
    /// Current source value (bits 1:0).
    pub current_value: TsCurrentValue,
}

// ---- JEITA CV configuration (REG 59h) --------------------------------------

/// JEITA current reduction factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JeitaCurrentFall {
    /// 100% - no reduction.
    Full = 0,
    /// 50% of normal charge current.
    Half = 1,
}

/// JEITA voltage reduction (gear steps below CV setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JeitaVoltageFall {
    /// 0 mV - no reduction.
    None = 0,
    /// One gear lower than CV setting.
    OneGear = 1,
    /// Two gears lower than CV setting.
    TwoGears = 2,
}

impl JeitaVoltageFall {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::None,
            1 => Self::OneGear,
            2 => Self::TwoGears,
            _ => Self::None,
        }
    }
}

/// JEITA CV configuration (REG 59h).
#[derive(Debug, Clone, Copy)]
pub struct JeitaCvConfig {
    /// Current reduction in warm zone (bit 6).
    pub warm_current: JeitaCurrentFall,
    /// Current reduction in cool zone (bit 4).
    pub cool_current: JeitaCurrentFall,
    /// Voltage reduction in warm zone (bits 3:2).
    pub warm_voltage: JeitaVoltageFall,
    /// Voltage reduction in cool zone (bits 1:0).
    pub cool_voltage: JeitaVoltageFall,
}

// ---- Charger timeout (REG 67h) ---------------------------------------------

/// Charge done safety timer duration (REG 67h bits 5:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeDoneTimeout {
    Hours5  = 0,
    Hours8  = 1,
    Hours12 = 2,
    Hours20 = 3,
}

impl ChargeDoneTimeout {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Hours5,
            1 => Self::Hours8,
            2 => Self::Hours12,
            _ => Self::Hours20,
        }
    }
}

/// Pre-charge safety timer duration (REG 67h bits 1:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreChargeTimeout {
    Mins40 = 0,
    Mins50 = 1,
    Mins60 = 2,
    Mins70 = 3,
}

impl PreChargeTimeout {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Mins40,
            1 => Self::Mins50,
            2 => Self::Mins60,
            _ => Self::Mins70,
        }
    }
}

/// Charger timeout configuration (REG 67h).
///
/// Bit layout:
///   7    Safety timer slowed during DPM/thermal regulation
///   6    Charge done safety timer enable
///   5:4  Charge done safety timer duration
///   3    reserved
///   2    Pre-charge safety timer enable
///   1:0  Pre-charge safety timer duration
#[derive(Debug, Clone, Copy)]
pub struct ChargerTimeout {
    /// Slow safety timer during input DPM or thermal regulation (bit 7).
    pub slow_during_dpm: bool,
    /// Charge done safety timer enable (bit 6).
    pub done_timer_en: bool,
    /// Charge done safety timer duration (bits 5:4).
    pub done_timeout: ChargeDoneTimeout,
    /// Pre-charge safety timer enable (bit 2).
    pub precharge_timer_en: bool,
    /// Pre-charge safety timer duration (bits 1:0).
    pub precharge_timeout: PreChargeTimeout,
}

// ---- CHGLED (REG 69h) ------------------------------------------------------

/// CHGLED output mode when controlled by register (REG 69h bits 5:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChgLedOutput {
    /// High-impedance.
    HiZ = 0,
    /// Low/HiZ 25%/75% duty, 1 Hz.
    Blink1Hz = 1,
    /// Low/HiZ 25%/75% duty, 4 Hz.
    Blink4Hz = 2,
    /// Drive low.
    Low = 3,
}

impl ChgLedOutput {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::HiZ,
            1 => Self::Blink1Hz,
            2 => Self::Blink4Hz,
            _ => Self::Low,
        }
    }
}

/// CHGLED display function (REG 69h bits 2:1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChgLedFunction {
    /// Type A display function.
    TypeA = 0,
    /// Type B display function.
    TypeB = 1,
    /// Output controlled by register (chgled_out_ctrl).
    Manual = 2,
}

impl ChgLedFunction {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::TypeA,
            1 => Self::TypeB,
            _ => Self::Manual,
        }
    }
}

/// CHGLED configuration (REG 69h).
#[derive(Debug, Clone, Copy)]
pub struct ChgLedConfig {
    /// Manual output mode when function is Manual (bits 5:4).
    pub output: ChgLedOutput,
    /// Display function selection (bits 2:1).
    pub function: ChgLedFunction,
    /// CHGLED pin enable (bit 0).
    pub enabled: bool,
}

// ---- DCDC force PWM (REG 81h) ----------------------------------------------

/// DCDC UVP debounce time (REG 81h bits 1:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcdcUvpDebounce {
    Us60  = 0,
    Us120 = 1,
    Us180 = 2,
    Us240 = 3,
}

impl DcdcUvpDebounce {
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Us60,
            1 => Self::Us120,
            2 => Self::Us180,
            _ => Self::Us240,
        }
    }
}

/// DCDC PWM/PFM and frequency spread configuration (REG 81h).
///
/// Bit layout:
///   7    Frequency spread enable
///   6    Frequency spread range: 0=50kHz, 1=100kHz
///   5    DCDC4 force PWM (0=auto, 1=always PWM)
///   4    DCDC3 force PWM
///   3    DCDC2 force PWM
///   2    DCDC1 force PWM
///   1:0  UVP debounce time
#[derive(Debug, Clone, Copy)]
pub struct DcdcPwmConfig {
    /// Frequency spread spectrum enable (bit 7).
    pub freq_spread_en: bool,
    /// Frequency spread range: false=50kHz, true=100kHz (bit 6).
    pub freq_spread_100k: bool,
    /// Force DCDC4 to always-PWM mode (bit 5).
    pub dcdc4_force_pwm: bool,
    /// Force DCDC3 to always-PWM mode (bit 4).
    pub dcdc3_force_pwm: bool,
    /// Force DCDC2 to always-PWM mode (bit 3).
    pub dcdc2_force_pwm: bool,
    /// Force DCDC1 to always-PWM mode (bit 2).
    pub dcdc1_force_pwm: bool,
    /// UVP debounce time (bits 1:0).
    pub uvp_debounce: DcdcUvpDebounce,
}

// ---- Fast power-on sequence (REG 28h-2Bh) ----------------------------------

/// Fast power-on start sequence code for a single rail.
/// 00-10 = sequence step, 11 = disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastPwronSeq {
    Step0 = 0,
    Step1 = 1,
    Step2 = 2,
    Disabled = 3,
}

impl FastPwronSeq {
    #[allow(dead_code)] // reserved for future fast power-on sequence decoder
    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Step0,
            1 => Self::Step1,
            2 => Self::Step2,
            _ => Self::Disabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REG 62 per datasheet V1.4 (6.13.2.60): 25*N up to code 8
    /// (200 mA), then 200 + 100*(N-8) up to code 16 (1000 mA). The
    /// original implementation was one code off in both ranges.
    #[test]
    fn charge_current_matches_datasheet() {
        assert_eq!(ChargeCurrent::from_ma(25).0, 1);
        assert_eq!(ChargeCurrent::from_ma(100).0, 4); // was 3 = 75 mA
        assert_eq!(ChargeCurrent::from_ma(200).0, 8);
        assert_eq!(ChargeCurrent::from_ma(250).0, 8); // gap clamps down
        assert_eq!(ChargeCurrent::from_ma(300).0, 9); // was 8 = 200 mA
        assert_eq!(ChargeCurrent::from_ma(1000).0, 16);
        assert_eq!(ChargeCurrent::from_ma(2500).0, 16); // reserved above
        for code in 0..=16u8 {
            let ma = ChargeCurrent(code).as_ma();
            assert_eq!(ChargeCurrent::from_ma(ma).0, code, "roundtrip {ma} mA");
        }
    }

    /// REG 63: 25*N, codes 9-15 reserved -> clamp at 200 mA.
    #[test]
    fn termination_current_matches_datasheet() {
        assert_eq!(TerminationCurrent::from_ma(25).0, 1);
        assert_eq!(TerminationCurrent::from_ma(50).0, 2);
        assert_eq!(TerminationCurrent::from_ma(375).0, 8);
    }

    /// REG 24: 2.6 V + 0.1 V per code, 3.2 V = 0b110.
    #[test]
    fn power_off_voltage_matches_datasheet() {
        assert_eq!(PowerOffVoltage::from_mv(2600).0, 0);
        assert_eq!(PowerOffVoltage::from_mv(3200).0, 6);
        assert_eq!(PowerOffVoltage::from_mv(3300).0, 7);
        assert_eq!(PowerOffVoltage::from_mv(9999).as_mv(), 3300);
    }
}
