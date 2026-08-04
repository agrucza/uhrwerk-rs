//! PCF85063A RTC driver - HAL-agnostic.
//!
//! NXP PCF85063ATL on the shared I2C bus.
//! Default address: 0x51.
//! Interrupt pin: GPIO39 (active-low, driven on alarm or timer match).
//!
//! Register map (from PCF85063A datasheet Rev 7):
//!
//!   Control and status registers:
//!   0x00  Control_1     - [7]=EXT_TEST [5]=STOP [4]=SR [2]=CIE [1]=12_24 [0]=CAP_SEL
//!   0x01  Control_2     - [7]=AIE [6]=AF [5]=MI [4]=HMI [3]=TF [2:0]=COF[2:0]
//!   0x02  Offset        - [7]=MODE [6:0]=OFFSET[6:0]
//!   0x03  RAM_byte      - [7:0]=B[7:0]
//!
//!   Time and date registers:
//!   0x04  Seconds       - [7]=OS (oscillator-stop flag) [6:0]=BCD seconds
//!   0x05  Minutes       - [6:0]=BCD minutes
//!   0x06  Hours         - [5:0]=BCD hours (24h mode)
//!   0x07  Days          - [5:0]=BCD days (1-31)
//!   0x08  Weekdays      - [2:0]=weekday (0=Sunday...6=Saturday)
//!   0x09  Months        - [4:0]=BCD months (1-12)
//!   0x0A  Years         - [7:0]=BCD years (00-99, i.e. 2000-2099)
//!
//!   Alarm registers (AEN bit 7: 1 = field disabled / not compared):
//!   0x0B  Second_alarm  - [7]=AEN_S [6:0]=BCD seconds
//!   0x0C  Minute_alarm  - [7]=AEN_M [6:0]=BCD minutes
//!   0x0D  Hour_alarm    - [7]=AEN_H [5:0]=BCD hours
//!   0x0E  Day_alarm     - [7]=AEN_D [5:0]=BCD days
//!   0x0F  Weekday_alarm - [7]=AEN_W [2:0]=weekday
//!
//!   Timer registers:
//!   0x10  Timer_value   - [7:0]=T[7:0]
//!   0x11  Timer_mode    - [4:3]=TCF[1:0] [2]=TE [1]=TIE [0]=TI_TP
//!
//! Works with any I2C implementation that satisfies the `embedded-hal` traits.
//! The I2C bus is passed by mutable reference on each call so it can be shared
//! with the touch controller, PMU, and any other I2C peripheral.

pub mod types;
pub use types::*;

use embedded_hal::i2c::I2c as I2cTrait;

/// Default I2C address for the PCF85063A (fixed, not configurable).
pub const DEFAULT_ADDRESS: u8 = 0x51;

// ---- Register addresses ---------------------------------------------------------

const REG_CTRL1:        u8 = 0x00;
const REG_CTRL2:        u8 = 0x01;
const REG_OFFSET:       u8 = 0x02;
const REG_RAM:          u8 = 0x03;
const REG_SECONDS:      u8 = 0x04; // burst-read start for time: 0x04-0x0A (7 bytes)
const REG_ALARM_SECOND: u8 = 0x0B; // burst-write start for alarm: 0x0B-0x0F (5 bytes)
const REG_TIMER_VALUE:  u8 = 0x10;
const REG_TIMER_MODE:   u8 = 0x11;

// ---- Control_2 bit masks --------------------------------------------------------

const CTRL2_AIE: u8 = 1 << 7; // Alarm Interrupt Enable - drives INT# pin on alarm
const CTRL2_AF:  u8 = 1 << 6; // Alarm Flag - set by chip on match, write 0 to clear
const CTRL2_MI:  u8 = 1 << 5; // Minute Interrupt - pulses INT# at second=0 every minute
const CTRL2_HMI: u8 = 1 << 4; // Half-Minute Interrupt - pulses INT# at second=0 and second=30
const CTRL2_TF:  u8 = 1 << 3; // Timer Flag - set by chip on timer expiry, write 0 to clear
const CTRL2_COF: u8 = 0x07;   // COF[2:0] mask - CLKOUT frequency bits

// ---- Timer_mode bit masks -------------------------------------------------------

const TIMER_TE:    u8 = 1 << 2; // Timer Enable
const TIMER_TIE:   u8 = 1 << 1; // Timer Interrupt Enable - drives INT# pin on expiry
const TIMER_TI_TP: u8 = 1 << 0; // 0 = interrupt (INT# held low), 1 = pulse

// ---- Alarm register bit ---------------------------------------------------------

/// Set in an alarm register to disable (not compare) that field.
const AEN: u8 = 1 << 7;

// ---- Driver configuration -------------------------------------------------------

/// PCF85063A RTC driver configuration.
pub struct Config {
    /// I2C device address (default: [`DEFAULT_ADDRESS`]).
    pub address: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self { address: DEFAULT_ADDRESS }
    }
}

// ---- Driver ---------------------------------------------------------------------

/// PCF85063A RTC driver.
///
/// Holds only the I2C address. The I2C bus is passed by mutable reference
/// on every call so it can be freely shared with other peripherals.
pub struct Rtc {
    addr: u8,
}

impl Rtc {
    /// Create a new RTC driver instance.
    pub fn new(config: Config) -> Self {
        Self { addr: config.address }
    }

    // ---- Initialisation ---------------------------------------------------------

    /// Initialise the PCF85063A.
    ///
    /// Reads the oscillator-stop (OS) flag FIRST and only performs
    /// the software reset when it is set - i.e. when the chip
    /// genuinely lost power and its registers are untrustworthy. A
    /// chip found running on backup power keeps its time: the
    /// software reset wipes the time registers and sets OS itself,
    /// so resetting unconditionally destroyed battery-backed time
    /// on every boot (hardware-observed as the oscillator-stop
    /// warning appearing even on reflash boots where RTC power
    /// never dropped).
    ///
    /// Either way the volatile machinery is left in a known state:
    /// Control_1 gets 24h mode, clock running, 7 pF quartz
    /// capacitor; alarm/timer interrupt enables and latched flags
    /// are cleared (the blanket reset used to provide that hygiene
    /// - without it a stale alarm armed by a previous run could
    /// fire into a fresh boot; the owning task re-arms what should
    /// be armed from persisted state).
    ///
    /// Returns `Ok(true)` if the stored time was invalid (power was
    /// lost). Call `set()` with a known time then - the seconds
    /// write also clears OS.
    pub fn init<I2C, E>(&self, i2c: &mut I2C) -> Result<bool, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // OS flag is bit 7 of the Seconds register. Read it before
        // touching anything - the reset below would set it. This
        // read doubles as the device-responding probe.
        let os_was_set = (self.read_register(i2c, REG_SECONDS)? & 0x80) != 0;

        if os_was_set {
            // Power was lost: register contents are undefined per
            // the datasheet, and the documented recovery is the
            // software reset (magic 0x58 to Control_1), followed by
            // the caller writing a known time.
            self.write_register(i2c, REG_CTRL1, 0x58)?;
        }

        // Ensure 24h mode (12_24 bit 1 = 0), clock running (STOP bit 5 = 0),
        // and 7 pF capacitor selected (CAP_SEL bit 0 = 1). Preserve other bits.
        let ctrl1 = self.read_register(i2c, REG_CTRL1)?;
        let ctrl1_new = (ctrl1 & !(1 << 5) & !(1 << 1)) | (1 << 0);
        if ctrl1_new != ctrl1 {
            self.write_register(i2c, REG_CTRL1, ctrl1_new)?;
        }

        // Boot hygiene on the volatile side, non-destructive to the
        // time: drop alarm/timer interrupt enables + latched flags,
        // and stop a still-running countdown timer.
        self.write_register(i2c, REG_CTRL2, 0x00)?;
        let timer_mode = self.read_register(i2c, REG_TIMER_MODE)?;
        if timer_mode & TIMER_TE != 0 {
            self.write_register(i2c, REG_TIMER_MODE, timer_mode & !TIMER_TE)?;
        }

        Ok(os_was_set)
    }

    // ---- Time read / write ------------------------------------------------------

    /// Read the current date and time.
    pub fn get<I2C, E>(&self, i2c: &mut I2C) -> Result<DateTime, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Burst-read 7 bytes: Seconds, Minutes, Hours, Days, Weekdays, Months, Years.
        let mut buf = [0u8; 7];
        i2c.write_read(self.addr, &[REG_SECONDS], &mut buf)
            .map_err(Error::I2c)?;

        Ok(DateTime {
            second:  bcd2bin(buf[0] & 0x7F), // mask out OS flag
            minute:  bcd2bin(buf[1] & 0x7F),
            hour:    bcd2bin(buf[2] & 0x3F),
            day:     bcd2bin(buf[3] & 0x3F),
            weekday: bcd2bin(buf[4] & 0x07),
            month:   bcd2bin(buf[5] & 0x1F),
            year:    2000 + bcd2bin(buf[6]) as u16,
        })
    }

    /// Write a new date and time. Also clears the oscillator-stop flag.
    pub fn set<I2C, E>(&self, i2c: &mut I2C, dt: &DateTime) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Stop the clock before writing (STOP bit = 1), then restart after.
        let ctrl1 = self.read_register(i2c, REG_CTRL1)?;
        self.write_register(i2c, REG_CTRL1, ctrl1 | (1 << 5))?;

        // Burst-write 7 time registers. Seconds bit 7 = 0 clears the OS flag.
        let buf = [
            REG_SECONDS,
            bin2bcd(dt.second),               // OS flag = 0 (cleared)
            bin2bcd(dt.minute),
            bin2bcd(dt.hour),
            bin2bcd(dt.day),
            dt.weekday & 0x07,
            bin2bcd(dt.month),
            bin2bcd((dt.year - 2000) as u8),
        ];
        i2c.write(self.addr, &buf).map_err(Error::I2c)?;

        // Restart the clock (clear STOP bit).
        self.write_register(i2c, REG_CTRL1, ctrl1 & !(1 << 5))
    }

    // ---- Status -----------------------------------------------------------------

    /// Check whether the oscillator-stop (OS) flag is set.
    ///
    /// The flag is set when the chip loses power or the oscillator stops
    /// for any reason. A set flag means the time registers are unreliable
    /// and should be re-written with a known time via [`set`].
    ///
    /// [`set`]: Rtc::set
    pub fn oscillator_stopped<I2C, E>(&self, i2c: &mut I2C) -> Result<bool, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let seconds_raw = self.read_register(i2c, REG_SECONDS)?;
        Ok((seconds_raw & 0x80) != 0)
    }

    /// Read Control_2 and return a snapshot of all interrupt/flag bits.
    ///
    /// This is a single I2C read that tells you everything the INT# pin
    /// could be driven by:
    ///
    /// | Field         | Meaning                                     |
    /// |---------------|---------------------------------------------|
    /// | `alarm_flag`  | AF - alarm matched since last clear         |
    /// | `alarm_ie`    | AIE - alarm interrupt enabled               |
    /// | `timer_flag`  | TF - timer expired since last clear         |
    /// | `minute_ie`   | MI - minute interrupt enabled                |
    /// | `half_min_ie` | HMI - half-minute interrupt enabled         |
    pub fn read_status<I2C, E>(&self, i2c: &mut I2C) -> Result<RtcStatus, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        Ok(RtcStatus {
            alarm_flag:  (ctrl2 & CTRL2_AF)  != 0,
            alarm_ie:    (ctrl2 & CTRL2_AIE) != 0,
            timer_flag:  (ctrl2 & CTRL2_TF)  != 0,
            minute_ie:   (ctrl2 & CTRL2_MI)  != 0,
            half_min_ie: (ctrl2 & CTRL2_HMI) != 0,
        })
    }

    // ---- Alarm ------------------------------------------------------------------

    /// Program the alarm and enable the INT# interrupt pin.
    ///
    /// Only fields set to `Some(value)` are compared; `None` fields are
    /// ignored by the chip (AEN bit set). The alarm fires - and the INT#
    /// pin is driven low - when all enabled fields match simultaneously.
    ///
    /// Any previously pending alarm flag is cleared before the new alarm
    /// is armed, so the caller does not get a spurious interrupt.
    ///
    /// Call [`clear_alarm_flag`] inside the GPIO39 interrupt handler after
    /// reading the flag to re-arm the INT# pin for the next match.
    ///
    /// [`clear_alarm_flag`]: Rtc::clear_alarm_flag
    pub fn set_alarm<I2C, E>(&self, i2c: &mut I2C, alarm: &Alarm) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Burst-write alarm registers 0x0B-0x0F.
        // AEN=1 (bit 7) means that field is not compared.
        let buf = [
            REG_ALARM_SECOND,
            alarm.second .map_or(AEN, bin2bcd),
            alarm.minute .map_or(AEN, bin2bcd),
            alarm.hour   .map_or(AEN, bin2bcd),
            alarm.day    .map_or(AEN, bin2bcd),
            alarm.weekday.map_or(AEN, |v| v & 0x07),
        ];
        i2c.write(self.addr, &buf).map_err(Error::I2c)?;

        // Clear any stale AF flag, then enable AIE. Preserve TF and COF bits.
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, (ctrl2 & !CTRL2_AF) | CTRL2_AIE)
    }

    /// Disable the alarm: set AEN=1 on all fields, clear AIE and AF.
    ///
    /// After this call the INT# pin will not be driven by the alarm, and
    /// any pending alarm interrupt is acknowledged.
    pub fn disable_alarm<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let buf = [REG_ALARM_SECOND, AEN, AEN, AEN, AEN, AEN];
        i2c.write(self.addr, &buf).map_err(Error::I2c)?;

        // Clear AIE and AF, preserve TF and COF.
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 & !(CTRL2_AIE | CTRL2_AF))
    }

    /// Returns `true` if the alarm flag (AF) is set.
    ///
    /// Use this for polling. In interrupt-driven code, call this inside the
    /// GPIO39 handler to confirm the interrupt source is the alarm (not the timer).
    pub fn alarm_triggered<I2C, E>(&self, i2c: &mut I2C) -> Result<bool, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        Ok((ctrl2 & CTRL2_AF) != 0)
    }

    /// Clear the alarm flag (AF) to re-arm the INT# pin.
    ///
    /// Call this after handling an alarm interrupt. Does not disable the
    /// alarm - it will fire again on the next match.
    pub fn clear_alarm_flag<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 & !CTRL2_AF)
    }

    /// Read back the currently programmed alarm.
    ///
    /// Fields whose AEN bit is set (disabled / not compared) are returned
    /// as `None`, matching the representation used by [`set_alarm`].
    ///
    /// [`set_alarm`]: Rtc::set_alarm
    pub fn get_alarm<I2C, E>(&self, i2c: &mut I2C) -> Result<Alarm, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mut buf = [0u8; 5];
        i2c.write_read(self.addr, &[REG_ALARM_SECOND], &mut buf)
            .map_err(Error::I2c)?;

        let decode = |raw: u8, mask: u8| -> Option<u8> {
            if (raw & AEN) != 0 { None } else { Some(bcd2bin(raw & mask)) }
        };

        Ok(Alarm {
            second:  decode(buf[0], 0x7F),
            minute:  decode(buf[1], 0x7F),
            hour:    decode(buf[2], 0x3F),
            day:     decode(buf[3], 0x3F),
            weekday: if (buf[4] & AEN) != 0 { None } else { Some(buf[4] & 0x07) },
        })
    }

    // ---- Periodic minute / half-minute interrupts --------------------------------
    //
    // MI and HMI are pre-defined timers that generate interrupt pulses on
    // INT#, running in sync with the seconds counter. They are independent
    // of the alarm and countdown timer - neither resource is consumed.
    //
    // **Constraint**: MI and HMI must only be used when the frequency
    // offset is set to normal mode (MODE bit = 0 in the Offset register).
    // The enable methods check this and return `Err` if MODE = 1 (coarse).
    // After a software reset (or fresh `init()`) MODE defaults to 0, so
    // the check passes without extra setup.
    //
    // The two timers can be enabled independently. However, enabling MI
    // on top of HMI is not distinguishable since HMI already fires at
    // second=0.
    //
    // Timing: the first MI pulse arrives 1-59 s after enabling; the first
    // HMI pulse arrives 1-29 s after enabling. Subsequent periods are
    // exact (60 s for MI, 30 s for HMI). Pulses are 1/64 s wide.

    /// Enable the minute interrupt.
    ///
    /// The PCF85063A pulses INT# (GPIO39) low once per minute when the
    /// seconds counter rolls over to 0. The pulse is 1/64 s wide.
    ///
    /// The first pulse after enabling arrives within 1-59 seconds;
    /// subsequent pulses are exactly 60 seconds apart.
    ///
    /// Returns `Err` if the offset register is in coarse mode (MODE=1),
    /// which is incompatible with MI/HMI per the datasheet.
    pub fn enable_minute_interrupt<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.check_offset_normal_mode(i2c)?;
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 | CTRL2_MI)
    }

    /// Enable the half-minute interrupt.
    ///
    /// The PCF85063A pulses INT# (GPIO39) low twice per minute - at
    /// second=0 and second=30. The pulse is 1/64 s wide.
    ///
    /// The first pulse after enabling arrives within 1-29 seconds;
    /// subsequent pulses are exactly 30 seconds apart.
    ///
    /// Note: enabling MI on top of HMI is allowed but not
    /// distinguishable, since HMI already fires at second=0.
    ///
    /// Returns `Err` if the offset register is in coarse mode (MODE=1),
    /// which is incompatible with MI/HMI per the datasheet.
    pub fn enable_half_minute_interrupt<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.check_offset_normal_mode(i2c)?;
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 | CTRL2_HMI)
    }

    /// Disable the minute interrupt (MI). Does not touch HMI.
    pub fn disable_minute_interrupt<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 & !CTRL2_MI)
    }

    /// Disable the half-minute interrupt (HMI). Does not touch MI.
    pub fn disable_half_minute_interrupt<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 & !CTRL2_HMI)
    }

    /// Disable both the minute and half-minute interrupts.
    ///
    /// After this call the INT# pin will no longer pulse on the
    /// minute or half-minute boundary (alarm and timer sources are
    /// unaffected).
    pub fn disable_minute_interrupts<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 & !(CTRL2_MI | CTRL2_HMI))
    }

    // ---- Countdown timer --------------------------------------------------------

    /// Start the countdown timer.
    ///
    /// The timer counts down from `value` (1-255) at the rate set by
    /// `clock`. When the counter decrements from 1, the TF flag in
    /// Control_2 is set and the counter auto-reloads for the next
    /// period.
    ///
    /// If `output` is [`TimerOutput::Interrupt`], INT# (GPIO39) is
    /// held low as long as TF is set - call [`clear_timer_flag`] to
    /// release it. With [`TimerOutput::Pulse`], INT# pulses briefly
    /// on each expiry instead.
    ///
    /// The timer repeats indefinitely until [`disable_timer`] is called.
    ///
    /// `value` must be 1-255. Passing 0 returns `Err(InvalidValue)`
    /// because loading 0 into the counter stops the hardware timer.
    ///
    /// Timer durations (value * period):
    ///
    /// | Clock     | Min (value=1)  | Max (value=255)   |
    /// |-----------|----------------|-------------------|
    /// | 4096 Hz   | 244 us         | 62.256 ms         |
    /// | 64 Hz     | 15.625 ms      | 3.984 s           |
    /// | 1 Hz      | 1 s            | 255 s             |
    /// | 1/60 Hz   | 60 s           | 4 h 15 min        |
    ///
    /// For periods longer than 4 h 15 min, use the alarm function
    /// instead.
    ///
    /// Note: time periods derived from the 32.768 kHz oscillator
    /// assume 0 ppm deviation and can be affected by correction
    /// pulses when offset calibration is active.
    ///
    /// [`clear_timer_flag`]: Rtc::clear_timer_flag
    /// [`disable_timer`]: Rtc::disable_timer
    pub fn set_timer<I2C, E>(
        &self,
        i2c: &mut I2C,
        value: u8,
        clock: TimerClock,
        output: TimerOutput,
    ) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Value 0 stops the hardware timer - reject it.
        if value == 0 {
            return Err(Error::InvalidValue);
        }

        // Write the countdown value first.
        self.write_register(i2c, REG_TIMER_VALUE, value)?;

        // Clear any stale TF flag before enabling.
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 & !CTRL2_TF)?;

        // Build Timer_mode: TCF[1:0] | TE | TIE | TI_TP
        let ti_tp = match output {
            TimerOutput::Interrupt => 0,
            TimerOutput::Pulse     => TIMER_TI_TP,
        };
        let mode = ((clock as u8) << 3) | TIMER_TE | TIMER_TIE | ti_tp;
        self.write_register(i2c, REG_TIMER_MODE, mode)
    }

    /// Stop the countdown timer and clear the timer flag.
    ///
    /// Writes 0 to Timer_value (the hardware stop mechanism), clears
    /// TE and TIE, and sets TCF to 1/60 Hz as recommended by the
    /// datasheet for power saving when the timer is not in use.
    pub fn disable_timer<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Load 0 into the countdown register - stops the timer in hardware.
        self.write_register(i2c, REG_TIMER_VALUE, 0)?;

        // Set TCF to 1/60 Hz for power saving, clear TE and TIE.
        // TI_TP is irrelevant when disabled, so we write a clean byte.
        let mode = (TimerClock::Per60 as u8) << 3;
        self.write_register(i2c, REG_TIMER_MODE, mode)?;

        // Clear TF in Control_2.
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 & !CTRL2_TF)
    }

    /// Returns `true` if the timer flag (TF) is set.
    ///
    /// Use this for polling. In interrupt-driven code, call this inside the
    /// GPIO39 handler to confirm the interrupt source is the timer (not the alarm).
    pub fn timer_expired<I2C, E>(&self, i2c: &mut I2C) -> Result<bool, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        Ok((ctrl2 & CTRL2_TF) != 0)
    }

    /// Clear the timer flag (TF) to re-arm the INT# pin.
    ///
    /// Call this after handling a timer interrupt. The timer continues
    /// running and will fire again after the next full countdown.
    pub fn clear_timer_flag<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, ctrl2 & !CTRL2_TF)
    }

    /// Read the current countdown value of the timer.
    ///
    /// While the timer is running this value decrements at the rate
    /// selected in [`set_timer`]. When it reaches zero the timer
    /// reloads from the original value and TF is set.
    ///
    /// If the timer is stopped (TE = 0), this returns the last loaded
    /// value.
    ///
    /// [`set_timer`]: Rtc::set_timer
    pub fn read_timer_value<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_register(i2c, REG_TIMER_VALUE)
    }

    // ---- CLKOUT -----------------------------------------------------------------

    /// Set the CLKOUT pin frequency, or disable it.
    ///
    /// The CLKOUT pin outputs a square wave at the selected frequency.
    /// Use [`ClkoutFreq::Off`] to put the pin into high-impedance state.
    pub fn set_clkout<I2C, E>(&self, i2c: &mut I2C, freq: ClkoutFreq) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // COF[2:0] are the low 3 bits of Control_2. Preserve all other bits.
        let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
        self.write_register(i2c, REG_CTRL2, (ctrl2 & !CTRL2_COF) | (freq as u8))
    }

    // ---- Offset / calibration ---------------------------------------------------

    /// Write a clock calibration offset to trim long-term drift.
    ///
    /// `offset` is a 7-bit two's complement value (`-64` to `+63`).
    /// Positive values speed the clock up; negative values slow it down.
    ///
    /// | Mode   | Correction interval | Step size    |
    /// |--------|---------------------|--------------|
    /// | Normal | Every two hours     | +-4.340 ppm  |
    /// | Coarse | Every minute        | +-4.069 ppm  |
    ///
    /// Measure drift over several days before computing an offset - one step
    /// is small, so repeated adjustment is rarely needed.
    ///
    /// **Note**: coarse mode (MODE=1) is incompatible with the minute
    /// and half-minute interrupts (MI/HMI). If either is currently
    /// enabled and coarse mode is requested, this returns
    /// `Err(InvalidMode)`. Disable MI/HMI first, or use normal mode.
    pub fn set_offset<I2C, E>(
        &self,
        i2c: &mut I2C,
        mode: OffsetMode,
        offset: i8,
    ) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Coarse mode is incompatible with MI/HMI.
        if matches!(mode, OffsetMode::Coarse) {
            let ctrl2 = self.read_register(i2c, REG_CTRL2)?;
            if (ctrl2 & (CTRL2_MI | CTRL2_HMI)) != 0 {
                return Err(Error::InvalidMode);
            }
        }

        let mode_bit: u8 = match mode {
            OffsetMode::Normal => 0,
            OffsetMode::Coarse => 1 << 7,
        };
        // Clamp to 7-bit range and mask to 7 bits.
        let offset_bits = (offset.clamp(-64, 63) as u8) & 0x7F;
        self.write_register(i2c, REG_OFFSET, mode_bit | offset_bits)
    }

    // ---- RAM byte ---------------------------------------------------------------

    /// Read the single non-volatile RAM byte.
    ///
    /// This byte survives power cycles (as long as the RTC backup supply holds).
    /// Use it for a dirty-flag, a small config value, or a boot counter.
    pub fn read_ram<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_register(i2c, REG_RAM)
    }

    /// Write the single non-volatile RAM byte.
    pub fn write_ram<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, REG_RAM, value)
    }

    // ---- Private helpers --------------------------------------------------------

    /// Verify that the offset register is in normal mode (MODE=0).
    ///
    /// MI and HMI require normal mode. Returns `Err(InvalidMode)` if
    /// MODE=1 (coarse) is currently active.
    fn check_offset_normal_mode<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let offset = self.read_register(i2c, REG_OFFSET)?;
        if (offset & (1 << 7)) != 0 {
            return Err(Error::InvalidMode);
        }
        Ok(())
    }

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
        i2c.write(self.addr, &[reg, val]).map_err(Error::I2c)
    }
}

// ---- BCD helpers ----------------------------------------------------------------

fn bcd2bin(bcd: u8) -> u8 {
    (bcd >> 4) * 10 + (bcd & 0x0F)
}

fn bin2bcd(bin: u8) -> u8 {
    ((bin / 10) << 4) | (bin % 10)
}
