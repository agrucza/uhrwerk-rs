//! QMI8658C 6-axis IMU driver - HAL-agnostic.
//!
//! Works with any I²C implementation that satisfies the `embedded-hal` traits.
//! The firmware passes in the concrete `esp_hal` I²C type; this driver never
//! imports `esp_hal` directly.
//!
//! The I²C bus is **not** owned by this struct - it is passed by mutable
//! reference on each call. This allows the same bus to be shared with the
//! PMU, touch controller, RTC, and any other I²C peripheral.
//!
//! # Wiring on ESP32-S3-Touch-AMOLED-2.06
//!
//! | Signal   | GPIO |
//! |----------|------|
//! | I2C_SDA  | 15   |
//! | I2C_SCL  | 14   |
//! | INT1     | 21   |
//!
//! The SDO/SA0 pin is tied to GND, giving I²C address 0x6B.
//! The CS pin is tied to VCC3V3, selecting I²C mode (not SPI).
//!
//! # Startup timing
//!
//! The QMI8658C requires **150 ms** from when VDD/VDDIO are within 1% of their
//! final values before any register may be written. The PMU enables the 3.3 V
//! rail early in `main`; ensure at least 150 ms has elapsed before calling
//! [`Qmi8658::init`]. In practice the display initialisation sequence
//! (~300 ms of delays) runs in between, so no extra wait is needed.

pub mod registers;
pub mod types;

pub use types::*;

use embedded_hal::i2c::I2c as I2cTrait;

/// Default I²C address when SDO/SA0 is tied to GND.
pub const ADDR: u8 = 0x6B;

/// QMI8658C IMU driver.
///
/// Holds the I²C address, configured scale factors, and an optional software
/// gyroscope bias. Call [`set_gyro_bias`] after [`collect_gyro_bias`] to
/// activate it; [`read`] then subtracts the stored values from every sample.
///
/// [`set_gyro_bias`]: Qmi8658::set_gyro_bias
/// [`collect_gyro_bias`]: Qmi8658::collect_gyro_bias
/// [`read`]: Qmi8658::read
pub struct Qmi8658 {
    addr:        u8,
    accel_scale: AccelScale,
    gyro_scale:  GyroScale,
    /// Subtracted from raw gyro readings in `read()`. Zero until `set_gyro_bias`.
    gyro_bias:   (i16, i16, i16),
}

/// Driver configuration.
pub struct Config {
    /// I²C device address (default: [`ADDR`]).
    pub address:     u8,
    /// Accelerometer full-scale range (default: ±8 g).
    pub accel_scale: AccelScale,
    /// Gyroscope full-scale range (default: ±256 dps).
    pub gyro_scale:  GyroScale,
    /// Accelerometer output data rate (default: 125 Hz).
    pub accel_odr:   Odr,
    /// Gyroscope output data rate (default: 125 Hz).
    pub gyro_odr:    Odr,
    /// Gyroscope low-pass filter (default: Mode1 = 3.59% of ODR).
    /// `None` disables the filter.
    pub gyro_lpf:    Option<LpfMode>,
    /// Accelerometer low-pass filter (default: disabled).
    pub accel_lpf:   Option<LpfMode>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            address:     ADDR,
            accel_scale: AccelScale::G8,
            gyro_scale:  GyroScale::Dps256,
            accel_odr:   Odr::Hz125,
            gyro_odr:    Odr::Hz125,
            gyro_lpf:    Some(LpfMode::Mode1),
            accel_lpf:   None,
        }
    }
}

impl Qmi8658 {
    /// Create a new driver instance. Call [`init`] before reading data.
    ///
    /// [`init`]: Qmi8658::init
    pub fn new(config: Config) -> Self {
        Self {
            addr:        config.address,
            accel_scale: config.accel_scale,
            gyro_scale:  config.gyro_scale,
            gyro_bias:   (0, 0, 0),
        }
    }

    /// Initialise the QMI8658C.
    ///
    /// 1. Verifies the WHO_AM_I register returns the expected chip ID (0x05).
    /// 2. Enables address auto-increment in CTRL1 for burst reads.
    /// 3. Configures the accelerometer and gyroscope ODR and full-scale range.
    /// 4. Enables both sensors via CTRL7.
    ///
    /// Returns `Err(Error::DeviceNotFound)` if the chip does not respond with
    /// the correct ID.
    pub fn init<I2C, E>(&self, i2c: &mut I2C, config: &Config) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Verify device identity.
        let chip_id = self.read_register(i2c, registers::WHO_AM_I)?;
        if chip_id != registers::CHIP_ID {
            return Err(Error::DeviceNotFound);
        }

        // CTRL1: keep default SPI_BE bit, add AUTO_INCREMENT for burst reads.
        // Default 0x20 | 0x40 = 0x60.
        self.write_register(i2c, registers::CTRL1,
            registers::ctrl1::SPI_BIG_ENDIAN | registers::ctrl1::AUTO_INCREMENT)?;

        // CTRL8 bit 7 = 1: route CTRL9 command handshake through
        // STATUSINT.bit7 instead of INT1. Without this, every CTRL9
        // command (soft reset, gyro bias, WRITE_WOM_SETTING, etc.)
        // uses INT1 as part of the handshake protocol. exec_ctrl9
        // already polls STATUSINT.bit7, so this lines up what the
        // chip does with what the driver polls.
        //
        // Note: bit 6 (route motion-detection events to INT1) was
        // tried empirically as a WoM fix and made no difference -
        // on this silicon/board, the chip sets STATUS1.WOM but
        // never drives INT1 for WoM events regardless of config.
        // WoM wake is handled by polling STATUS1 in the task.
        self.write_register(i2c, registers::CTRL8,
            registers::ctrl8::CTRL9_HANDSHAKE_STATUSINT)?;

        // CTRL2: accelerometer full-scale and ODR (bit 7 = 0 = self-test off).
        let ctrl2 = (config.accel_scale.ctrl2_bits() << registers::ctrl2::FS_SHIFT)
                  | config.accel_odr.bits();
        self.write_register(i2c, registers::CTRL2, ctrl2)?;

        // CTRL3: gyroscope full-scale and ODR (bit 7 = 0 = self-test off).
        let ctrl3 = (config.gyro_scale.ctrl3_bits() << registers::ctrl3::FS_SHIFT)
                  | config.gyro_odr.bits();
        self.write_register(i2c, registers::CTRL3, ctrl3)?;

        // CTRL5: low-pass filter configuration for gyroscope and accelerometer.
        let ctrl5 = {
            let g = match config.gyro_lpf {
                Some(m) => (m.bits() << registers::ctrl5::GLPF_MODE_SHIFT) | registers::ctrl5::GLPF_EN,
                None    => 0,
            };
            let a = match config.accel_lpf {
                Some(m) => (m.bits() << registers::ctrl5::ALPF_MODE_SHIFT) | registers::ctrl5::ALPF_EN,
                None    => 0,
            };
            g | a
        };
        self.write_register(i2c, registers::CTRL5, ctrl5)?;

        // CTRL7: enable both accelerometer and gyroscope.
        self.write_register(i2c, registers::CTRL7,
            registers::ctrl7::ACCEL_EN | registers::ctrl7::GYRO_EN)?;

        Ok(())
    }

    /// Read a snapshot of all sensor data in a single 14-byte burst.
    ///
    /// Reads TEMP_L through GZ_H (registers 0x33-0x40) in one I²C transaction.
    /// This guarantees that temperature, accelerometer, and gyroscope readings
    /// all come from the same sample.
    ///
    /// The raw i16 values can be converted to physical units using the scale
    /// factors stored in the [`Config`] passed to [`init`]:
    ///
    /// ```ignore
    /// let ax_g   = data.accel_x as f32 / AccelScale::G8.lsb_per_g() as f32;
    /// let gx_dps = data.gyro_x  as f32 / GyroScale::Dps256.lsb_per_dps() as f32;
    /// let temp_c = data.temp_celsius(); // integer degrees
    /// ```
    pub fn read<I2C, E>(&self, i2c: &mut I2C) -> Result<ImuData, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Burst-read 14 bytes starting at TEMP_L (0x33):
        // [0..1] = TEMP, [2..7] = ACCEL XYZ, [8..13] = GYRO XYZ
        let mut buf = [0u8; 14];
        i2c.write_read(self.addr, &[registers::TEMP_L], &mut buf)
            .map_err(Error::I2c)?;

        Ok(ImuData {
            temp_raw: i16::from_le_bytes([buf[0],  buf[1]]),
            accel_x:  i16::from_le_bytes([buf[2],  buf[3]]),
            accel_y:  i16::from_le_bytes([buf[4],  buf[5]]),
            accel_z:  i16::from_le_bytes([buf[6],  buf[7]]),
            gyro_x:   i16::from_le_bytes([buf[8],  buf[9]]).saturating_sub(self.gyro_bias.0),
            gyro_y:   i16::from_le_bytes([buf[10], buf[11]]).saturating_sub(self.gyro_bias.1),
            gyro_z:   i16::from_le_bytes([buf[12], buf[13]]).saturating_sub(self.gyro_bias.2),
        })
    }

    /// Check whether new accelerometer and/or gyroscope data is available.
    ///
    /// Reads STATUS0 (0x2E) and returns the raw byte. Test individual flags
    /// with the masks in [`registers::status0`]:
    ///
    /// ```ignore
    /// use drivers::imu::registers::status0;
    /// let s = imu.status(&mut i2c)?;
    /// if s & status0::ACCEL_READY != 0 { /* new accel sample */ }
    /// if s & status0::GYRO_READY  != 0 { /* new gyro sample */  }
    /// ```
    pub fn status<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_register(i2c, registers::STATUS0)
    }

    /// Read the WHO_AM_I and REVISION_ID registers.
    ///
    /// Returns `(chip_id, revision_id)`. Expected: `(0x05, 0x79)`.
    pub fn read_ids<I2C, E>(&self, i2c: &mut I2C) -> Result<(u8, u8), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let chip_id  = self.read_register(i2c, registers::WHO_AM_I)?;
        let revision = self.read_register(i2c, registers::REVISION_ID)?;
        Ok((chip_id, revision))
    }

    /// Collect a gyroscope bias estimate by averaging `n` distinct hardware samples.
    ///
    /// The board must be **completely still** during this call. Before reading
    /// each sample the method polls STATUS0 until the gyroscope data-ready flag
    /// (bit 1) is set, ensuring every one of the `n` samples is a genuinely new
    /// measurement from the hardware. At 125 Hz ODR this takes `n × 8 ms`
    /// (~512 ms for n=64).
    ///
    /// Returns `Err(Error::Timeout)` if a fresh sample does not arrive within
    /// 1000 I²C poll attempts (~100 ms) for any given sample slot.
    ///
    /// The returned `(bias_x, bias_y, bias_z)` values can be passed directly
    /// to [`set_gyro_bias`].
    ///
    /// [`set_gyro_bias`]: Qmi8658::set_gyro_bias
    pub fn collect_gyro_bias<I2C, E>(&self, i2c: &mut I2C, n: u8) -> Result<(i16, i16, i16), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mut sum_x: i32 = 0;
        let mut sum_y: i32 = 0;
        let mut sum_z: i32 = 0;

        for _ in 0..n {
            // Wait for a fresh gyroscope sample (STATUS0 bit 1 = gDA).
            let mut ready = false;
            for _ in 0..1000u16 {
                let s = self.read_register(i2c, registers::STATUS0)?;
                if s & registers::status0::GYRO_READY != 0 {
                    ready = true;
                    break;
                }
            }
            if !ready {
                return Err(Error::Timeout);
            }

            // Burst-read 6 bytes starting at GX_L (0x3B).
            let mut buf = [0u8; 6];
            i2c.write_read(self.addr, &[registers::GX_L], &mut buf)
                .map_err(Error::I2c)?;
            sum_x += i16::from_le_bytes([buf[0], buf[1]]) as i32;
            sum_y += i16::from_le_bytes([buf[2], buf[3]]) as i32;
            sum_z += i16::from_le_bytes([buf[4], buf[5]]) as i32;
        }

        let n = n as i32;
        Ok(((sum_x / n) as i16, (sum_y / n) as i16, (sum_z / n) as i16))
    }

    /// Store gyroscope bias values for software subtraction in [`read`].
    ///
    /// Pass the values returned by [`collect_gyro_bias`]. Every subsequent
    /// call to `read()` subtracts these raw i16 values from the gyro output
    /// before returning, so the readings sit near zero when the device is still.
    ///
    /// [`read`]: Qmi8658::read
    /// [`collect_gyro_bias`]: Qmi8658::collect_gyro_bias
    pub fn set_gyro_bias(&mut self, bias_x: i16, bias_y: i16, bias_z: i16) {
        self.gyro_bias = (bias_x, bias_y, bias_z);
    }

    /// Apply gyroscope bias offsets to the raw output registers using the
    /// `CTRL_CMD_GYRO_HOST_DELTA_OFFSET` CTRL9 command (code 0x0A).
    ///
    /// Pass the raw i16 bias values returned by [`collect_gyro_bias`]. This
    /// method converts them to the 11.5 fixed-point format the command requires
    /// (using the gyro scale stored at construction time), writes them into
    /// CAL1-CAL3, issues the command, and polls STATUSINT bit 7 for completion.
    ///
    /// After this call the chip subtracts the offsets internally before
    /// placing data in the GX/GY/GZ output registers - no software correction
    /// is needed in [`read`].
    ///
    /// The calibration is **volatile**: it is lost on power-off or reset.
    /// Call once at every boot (re-collect with [`collect_gyro_bias`], or load
    /// saved values from flash once persistent storage is implemented).
    ///
    /// [`collect_gyro_bias`]: Qmi8658::collect_gyro_bias
    /// [`read`]: Qmi8658::read
    pub fn calibrate_gyro<I2C, E>(&self, i2c: &mut I2C, bias_x: i16, bias_y: i16, bias_z: i16) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Convert raw sensor units to 11.5 fixed-point (5 fractional bits).
        // Formula: delta = bias_raw * 32 / lsb_per_dps
        // Example at ±256 dps (128 LSB/dps): bias 369 → 369*32/128 = 92
        let scale = self.gyro_scale.lsb_per_dps() as i32;
        let dx = ((bias_x as i32 * 32) / scale) as i16;
        let dy = ((bias_y as i32 * 32) / scale) as i16;
        let dz = ((bias_z as i32 * 32) / scale) as i16;

        let [dx_l, dx_h] = dx.to_le_bytes();
        let [dy_l, dy_h] = dy.to_le_bytes();
        let [dz_l, dz_h] = dz.to_le_bytes();

        self.write_register(i2c, registers::CAL1_L, dx_l)?;
        self.write_register(i2c, registers::CAL1_H, dx_h)?;
        self.write_register(i2c, registers::CAL2_L, dy_l)?;
        self.write_register(i2c, registers::CAL2_H, dy_h)?;
        self.write_register(i2c, registers::CAL3_L, dz_l)?;
        self.write_register(i2c, registers::CAL3_H, dz_h)?;

        // Issue CTRL_CMD_GYRO_HOST_DELTA_OFFSET - applies offsets to the raw
        // GX/GY/GZ output registers (unlike GYRO_BIAS 0x01 which only affects
        // the AttitudeEngine output).
        self.exec_ctrl9(i2c, registers::cmd::GYRO_HOST_DELTA_OFFSET)
    }

    /// Returns the accelerometer scale this driver was configured with.
    pub fn accel_scale(&self) -> AccelScale { self.accel_scale }

    /// Returns the gyroscope scale this driver was configured with.
    pub fn gyro_scale(&self)  -> GyroScale  { self.gyro_scale  }

    // ---- Soft reset ---------------------------------------------------------------

    /// Perform a soft reset by writing 0xB0 to REG 0x60.
    ///
    /// All registers return to their default values. The caller
    /// should wait at least 15 ms after this call for the reset
    /// to complete, then call `init()` again to reconfigure the
    /// device.
    pub fn soft_reset<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::RESET, registers::RESET_VALUE)
    }

    // ---- Self-test ----------------------------------------------------------------

    /// Enable or disable accelerometer self-test (CTRL2 bit 7).
    ///
    /// When enabled, the accelerometer applies an internal test force
    /// that produces a known output change. Compare the self-test
    /// output to normal output to verify sensor functionality.
    pub fn set_accel_self_test<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::CTRL2)?;
        let val = if enable { reg | registers::ctrl2::SELF_TEST } else { reg & !registers::ctrl2::SELF_TEST };
        self.write_register(i2c, registers::CTRL2, val)
    }

    /// Enable or disable gyroscope self-test (CTRL3 bit 7).
    pub fn set_gyro_self_test<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::CTRL3)?;
        let val = if enable { reg | registers::ctrl3::SELF_TEST } else { reg & !registers::ctrl3::SELF_TEST };
        self.write_register(i2c, registers::CTRL3, val)
    }

    /// Run the accelerometer self-test per datasheet section 11.1.
    ///
    /// Procedure:
    /// 1. Disable all sensors (CTRL7 = 0x00).
    /// 2. Enable accel self-test (CTRL2.bit7 = 1). The chip forces
    ///    ±16 g full-scale internally regardless of the aFS field.
    /// 3. Wait for the chip to drive INT2 high. INT2 is not wired on
    ///    this board, but `STATUSINT.bit0` (AVAIL) mirrors INT2 when
    ///    `syncSmpl` is 0 (our default), so we poll it over I2C.
    /// 4. Clear the self-test bit.
    /// 5. Wait for `STATUSINT.bit0` to drop back to 0.
    /// 6. Burst-read `dVX..dVZ` (0x51..0x56). Result format is
    ///    signed Q5.11 in g (1 LSB = 1/2048 g ≈ 0.488 mg).
    ///
    /// On return, the caller must re-run [`init`] to restore the
    /// normal accel+gyro configuration - self-test leaves CTRL7 = 0
    /// and CTRL2 in a partially-modified state.
    ///
    /// The poll loops are busy waits bounded by `max_poll_iters`
    /// iterations. At 400 kHz I2C each read takes ~100 μs, so 5000
    /// iterations ≈ 500 ms total budget, comfortably above the
    /// worst-case self-test duration.
    ///
    /// [`init`]: Qmi8658::init
    pub fn run_accel_self_test<I2C, E>(&self, i2c: &mut I2C) -> Result<AccelSelfTestResult, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        const MAX_POLL_ITERS: u16 = 5000;

        // 1. Disable sensors.
        self.write_register(i2c, registers::CTRL7, 0x00)?;

        // 2. Set aST, keep current ODR nibble. (Self-test forces 16g
        //    full-scale internally; the aFS bits are ignored by the
        //    test, so we don't need to touch them here.)
        let ctrl2 = self.read_register(i2c, registers::CTRL2)?;
        self.write_register(i2c, registers::CTRL2, ctrl2 | registers::ctrl2::SELF_TEST)?;

        // 3. Wait for STATUSINT.bit0 = 1 (INT2 mirror, self-test done).
        self.wait_statusint_avail(i2c, true, MAX_POLL_ITERS)?;

        // 4. Clear aST.
        self.write_register(i2c, registers::CTRL2, ctrl2 & !registers::ctrl2::SELF_TEST)?;

        // 5. Wait for STATUSINT.bit0 = 0 (ack).
        self.wait_statusint_avail(i2c, false, MAX_POLL_ITERS)?;

        // 6. Read dVX..dVZ (0x51..0x56) in one burst.
        let mut buf = [0u8; 6];
        i2c.write_read(self.addr, &[registers::DVX_L], &mut buf)
            .map_err(Error::I2c)?;
        let x_raw = i16::from_le_bytes([buf[0], buf[1]]);
        let y_raw = i16::from_le_bytes([buf[2], buf[3]]);
        let z_raw = i16::from_le_bytes([buf[4], buf[5]]);

        // Q5.11 fixed-point in g. 1 LSB = 1/2048 g. Convert to mg.
        let to_mg = |raw: i16| -> i32 { (raw as i32 * 1000) / 2048 };
        let x_mg = to_mg(x_raw);
        let y_mg = to_mg(y_raw);
        let z_mg = to_mg(z_raw);

        let passed = x_mg.abs() > 200 && y_mg.abs() > 200 && z_mg.abs() > 200;

        Ok(AccelSelfTestResult { x_mg, y_mg, z_mg, passed })
    }

    /// Run the gyroscope self-test per datasheet section 11.2.
    ///
    /// Same procedure as [`run_accel_self_test`], but flips CTRL3.bit7
    /// instead of CTRL2.bit7. The chip forces ±2048 dps full-scale
    /// and 1 kHz ODR internally regardless of CTRL3 settings. The
    /// result format is signed Q12.4 in dps (1 LSB = 1/16 dps).
    ///
    /// [`run_accel_self_test`]: Qmi8658::run_accel_self_test
    pub fn run_gyro_self_test<I2C, E>(&self, i2c: &mut I2C) -> Result<GyroSelfTestResult, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        const MAX_POLL_ITERS: u16 = 5000;

        self.write_register(i2c, registers::CTRL7, 0x00)?;

        let ctrl3 = self.read_register(i2c, registers::CTRL3)?;
        self.write_register(i2c, registers::CTRL3, ctrl3 | registers::ctrl3::SELF_TEST)?;

        self.wait_statusint_avail(i2c, true, MAX_POLL_ITERS)?;

        self.write_register(i2c, registers::CTRL3, ctrl3 & !registers::ctrl3::SELF_TEST)?;

        self.wait_statusint_avail(i2c, false, MAX_POLL_ITERS)?;

        let mut buf = [0u8; 6];
        i2c.write_read(self.addr, &[registers::DVX_L], &mut buf)
            .map_err(Error::I2c)?;
        let x_raw = i16::from_le_bytes([buf[0], buf[1]]);
        let y_raw = i16::from_le_bytes([buf[2], buf[3]]);
        let z_raw = i16::from_le_bytes([buf[4], buf[5]]);

        // Q12.4 fixed-point in dps. 1 LSB = 1/16 dps. Integer dps.
        let to_dps = |raw: i16| -> i32 { raw as i32 / 16 };
        let x_dps = to_dps(x_raw);
        let y_dps = to_dps(y_raw);
        let z_dps = to_dps(z_raw);

        let passed = x_dps.abs() > 300 && y_dps.abs() > 300 && z_dps.abs() > 300;

        Ok(GyroSelfTestResult { x_dps, y_dps, z_dps, passed })
    }

    /// Poll `STATUSINT.bit0` until it matches `target`, or the retry
    /// budget is exhausted. Used by the self-test procedures in place
    /// of waiting on the physical INT2 pin, which isn't wired.
    fn wait_statusint_avail<I2C, E>(
        &self,
        i2c: &mut I2C,
        target: bool,
        max_iters: u16,
    ) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        for _ in 0..max_iters {
            let s = self.read_register(i2c, registers::STATUSINT)?;
            let avail = (s & registers::statusint::AVAIL) != 0;
            if avail == target {
                return Ok(());
            }
        }
        Err(Error::Timeout)
    }

    // ---- Sensor enable/disable ----------------------------------------------------

    /// Enable or disable individual sensors and features via CTRL7.
    ///
    /// Use the constants in `registers::ctrl7` to build the mask:
    /// `ACCEL_EN`, `GYRO_EN`, `MAG_EN`, `AE_EN`, `GYRO_SNZ`,
    /// `SYS_HS`, `SYNC_SMPL`.
    pub fn set_ctrl7<I2C, E>(&self, i2c: &mut I2C, value: u8) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::CTRL7, value)
    }

    /// Read the current CTRL7 register value.
    pub fn ctrl7<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_register(i2c, registers::CTRL7)
    }

    /// Enable or disable the accelerometer (CTRL7 bit 0).
    pub fn set_accel_enable<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::CTRL7)?;
        let val = if enable { reg | registers::ctrl7::ACCEL_EN } else { reg & !registers::ctrl7::ACCEL_EN };
        self.write_register(i2c, registers::CTRL7, val)
    }

    /// Enable or disable the gyroscope (CTRL7 bit 1).
    pub fn set_gyro_enable<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::CTRL7)?;
        let val = if enable { reg | registers::ctrl7::GYRO_EN } else { reg & !registers::ctrl7::GYRO_EN };
        self.write_register(i2c, registers::CTRL7, val)
    }

    /// Disable all sensors by writing 0x00 to CTRL7.
    pub fn disable_all<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::CTRL7, 0x00)
    }

    // ---- FIFO ---------------------------------------------------------------------

    /// Configure the FIFO (FIFO_CTRL register 0x14).
    ///
    /// When `mode` is `Bypass`, the FIFO is disabled. For `Fifo` or
    /// `Streaming`, all enabled sensors must share the same ODR.
    pub fn configure_fifo<I2C, E>(
        &self,
        i2c: &mut I2C,
        mode: FifoMode,
        size: FifoSize,
    ) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let val = ((size as u8) << registers::fifo_ctrl::SIZE_SHIFT)
                | (mode as u8);
        self.write_register(i2c, registers::FIFO_CTRL, val)
    }

    /// Set the FIFO watermark threshold in ODR samples (0-255).
    pub fn set_fifo_watermark<I2C, E>(&self, i2c: &mut I2C, samples: u8) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::FIFO_WTM_TH, samples)
    }

    /// Read FIFO status flags and sample count.
    pub fn fifo_status<I2C, E>(&self, i2c: &mut I2C) -> Result<FifoStatus, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let cnt_lsb = self.read_register(i2c, registers::FIFO_SMPL_CNT)? as u16;
        let status = self.read_register(i2c, registers::FIFO_STATUS)?;
        let cnt_msb = (status & 0x03) as u16;
        Ok(FifoStatus {
            full:         (status & registers::fifo_status::FULL) != 0,
            watermark:    (status & registers::fifo_status::WATERMARK) != 0,
            overflow:     (status & registers::fifo_status::OVERFLOW) != 0,
            not_empty:    (status & registers::fifo_status::NOT_EMPTY) != 0,
            sample_count: (cnt_msb << 8) | cnt_lsb,
        })
    }

    /// Reset the FIFO via CTRL9 command.
    pub fn reset_fifo<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.exec_ctrl9(i2c, registers::cmd::RST_FIFO)
    }

    /// Request FIFO data via CTRL9 command, then read `buf.len()` bytes.
    ///
    /// After this call, the caller should read FIFO_DATA (0x17) in
    /// bursts of 6 bytes per enabled sensor until the FIFO is empty,
    /// then call `fifo_end_read()` to clear the read mode.
    pub fn fifo_begin_read<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.exec_ctrl9(i2c, registers::cmd::REQ_FIFO)
    }

    /// Read one byte from FIFO_DATA register.
    pub fn fifo_read_byte<I2C, E>(&self, i2c: &mut I2C) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_register(i2c, registers::FIFO_DATA)
    }

    /// End FIFO read mode by clearing FIFO_rd_mode (FIFO_CTRL bit 7).
    pub fn fifo_end_read<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let reg = self.read_register(i2c, registers::FIFO_CTRL)?;
        self.write_register(i2c, registers::FIFO_CTRL, reg & !registers::fifo_ctrl::RD_MODE)
    }

    // ---- AttitudeEngine -----------------------------------------------------------

    /// Configure and enable the AttitudeEngine.
    ///
    /// Sets the AE output data rate in CTRL6 and enables sEN in CTRL7.
    /// The accelerometer and gyroscope must already be enabled.
    pub fn enable_attitude_engine<I2C, E>(&self, i2c: &mut I2C, odr: AeOdr) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Set AE ODR in CTRL6 (preserve sMoD bit 7)
        let ctrl6 = self.read_register(i2c, registers::CTRL6)?;
        self.write_register(i2c, registers::CTRL6, (ctrl6 & !registers::ctrl6::SODR_MASK) | (odr as u8))?;

        // Enable sEN in CTRL7
        let ctrl7 = self.read_register(i2c, registers::CTRL7)?;
        self.write_register(i2c, registers::CTRL7, ctrl7 | registers::ctrl7::AE_EN)
    }

    /// Disable the AttitudeEngine (clear sEN in CTRL7).
    pub fn disable_attitude_engine<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl7 = self.read_register(i2c, registers::CTRL7)?;
        self.write_register(i2c, registers::CTRL7, ctrl7 & !registers::ctrl7::AE_EN)
    }

    /// Enable or disable Motion on Demand (CTRL6 bit 7).
    ///
    /// Requires sEN=1 in CTRL7 (AttitudeEngine enabled).
    pub fn set_motion_on_demand<I2C, E>(&self, i2c: &mut I2C, enable: bool) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl6 = self.read_register(i2c, registers::CTRL6)?;
        let val = if enable { ctrl6 | registers::ctrl6::SMOD } else { ctrl6 & !registers::ctrl6::SMOD };
        self.write_register(i2c, registers::CTRL6, val)
    }

    /// Request Motion on Demand data via CTRL9 command.
    ///
    /// After completion, quaternion and velocity data is available
    /// in the output registers (read with `read_quaternion()` and
    /// `read_velocity()`).
    pub fn request_motion_data<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.exec_ctrl9(i2c, registers::cmd::REQ_SDI)
    }

    /// Read the AttitudeEngine quaternion increment (dQW, dQX, dQY, dQZ).
    ///
    /// Burst-reads 8 bytes from registers 0x49-0x50.
    pub fn read_quaternion<I2C, E>(&self, i2c: &mut I2C) -> Result<Quaternion, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mut buf = [0u8; 8];
        i2c.write_read(self.addr, &[registers::DQW_L], &mut buf)
            .map_err(Error::I2c)?;
        Ok(Quaternion {
            dqw: i16::from_le_bytes([buf[0], buf[1]]),
            dqx: i16::from_le_bytes([buf[2], buf[3]]),
            dqy: i16::from_le_bytes([buf[4], buf[5]]),
            dqz: i16::from_le_bytes([buf[6], buf[7]]),
        })
    }

    /// Read the AttitudeEngine velocity increment (dVX, dVY, dVZ).
    ///
    /// Burst-reads 6 bytes from registers 0x51-0x56.
    pub fn read_velocity<I2C, E>(&self, i2c: &mut I2C) -> Result<VelocityIncrement, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mut buf = [0u8; 6];
        i2c.write_read(self.addr, &[registers::DVX_L], &mut buf)
            .map_err(Error::I2c)?;
        Ok(VelocityIncrement {
            dvx: i16::from_le_bytes([buf[0], buf[1]]),
            dvy: i16::from_le_bytes([buf[2], buf[3]]),
            dvz: i16::from_le_bytes([buf[4], buf[5]]),
        })
    }

    /// Read AttitudeEngine status registers (AE_REG1 and AE_REG2).
    ///
    /// AE_REG1 (0x57) contains clipping status flags.
    /// AE_REG2 (0x58) contains velocity overflow flags.
    pub fn read_ae_status<I2C, E>(&self, i2c: &mut I2C) -> Result<(u8, u8), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let r1 = self.read_register(i2c, registers::AE_REG1)?;
        let r2 = self.read_register(i2c, registers::AE_REG2)?;
        Ok((r1, r2))
    }

    // ---- Wake-on-Motion -----------------------------------------------------------

    /// Configure and enable Wake-on-Motion.
    ///
    /// Before calling this, disable all sensors (write 0x00 to CTRL7),
    /// then configure the accelerometer ODR and scale via CTRL2.
    /// After this call, enable the accelerometer in CTRL7.
    ///
    /// The full sequence is:
    /// 1. `disable_all()`
    /// 2. Write desired accel ODR/scale to CTRL2 (via `init()` or directly)
    /// 3. `configure_wom(&config)`
    /// 4. `set_accel_enable(true)`
    ///
    /// To exit WoM, call `disable_wom()`.
    pub fn configure_wom<I2C, E>(&self, i2c: &mut I2C, cfg: &WomConfig) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // CAL1_L: WoM threshold in mg (1 mg/LSB)
        self.write_register(i2c, registers::CAL1_L, cfg.threshold_mg)?;

        // CAL1_H: bits 7:6 = interrupt select, bits 5:0 = blanking time
        let cal1_h = ((cfg.interrupt as u8) << 6) | (cfg.blanking_samples & 0x3F);
        self.write_register(i2c, registers::CAL1_H, cal1_h)?;

        // Issue CTRL9 command to configure WoM
        self.exec_ctrl9(i2c, registers::cmd::WRITE_WOM_SETTING)
    }

    /// Disable Wake-on-Motion by writing threshold 0 and executing
    /// the WoM CTRL9 command. Restores interrupt pins to normal.
    ///
    /// Call `disable_all()` before this, then reconfigure sensors
    /// as desired afterward.
    pub fn disable_wom<I2C, E>(&self, i2c: &mut I2C) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::CAL1_L, 0x00)?;
        self.write_register(i2c, registers::CAL1_H, 0x00)?;
        self.exec_ctrl9(i2c, registers::cmd::WRITE_WOM_SETTING)
    }

    /// Check if a Wake-on-Motion event occurred (STATUS1 bit 2).
    ///
    /// Reading STATUS1 clears the WoM bit and resets the interrupt line.
    pub fn wom_event<I2C, E>(&self, i2c: &mut I2C) -> Result<bool, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let s = self.read_register(i2c, registers::STATUS1)?;
        Ok((s & registers::status1::WOM) != 0)
    }

    // ---- Accelerometer calibration ------------------------------------------------

    /// Apply accelerometer delta-offset via CTRL9 command 0x09.
    ///
    /// Each offset is a signed 4.12 fixed-point value (12 fractional
    /// bits). To convert from raw sensor units:
    ///   `delta = bias_raw * 4096 / lsb_per_g`
    ///
    /// This offset is volatile - lost on power cycle or reset.
    pub fn calibrate_accel<I2C, E>(
        &self,
        i2c: &mut I2C,
        bias_x: i16,
        bias_y: i16,
        bias_z: i16,
    ) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        // Convert raw sensor units to signed 4.12 fixed-point.
        let scale = self.accel_scale.lsb_per_g() as i32;
        let dx = ((bias_x as i32 * 4096) / scale) as i16;
        let dy = ((bias_y as i32 * 4096) / scale) as i16;
        let dz = ((bias_z as i32 * 4096) / scale) as i16;

        let [dx_l, dx_h] = dx.to_le_bytes();
        let [dy_l, dy_h] = dy.to_le_bytes();
        let [dz_l, dz_h] = dz.to_le_bytes();

        self.write_register(i2c, registers::CAL1_L, dx_l)?;
        self.write_register(i2c, registers::CAL1_H, dx_h)?;
        self.write_register(i2c, registers::CAL2_L, dy_l)?;
        self.write_register(i2c, registers::CAL2_H, dy_h)?;
        self.write_register(i2c, registers::CAL3_L, dz_l)?;
        self.write_register(i2c, registers::CAL3_H, dz_h)?;

        self.exec_ctrl9(i2c, registers::cmd::ACCEL_HOST_DELTA_OFFSET)
    }

    // ---- Timestamp ----------------------------------------------------------------

    /// Read the 24-bit sample timestamp.
    ///
    /// The counter increments by one for each sample from the sensor
    /// with the highest ODR. It wraps from 0xFFFFFF to 0x000000.
    pub fn read_timestamp<I2C, E>(&self, i2c: &mut I2C) -> Result<u32, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let mut buf = [0u8; 3];
        i2c.write_read(self.addr, &[registers::TIMESTAMP_LOW], &mut buf)
            .map_err(Error::I2c)?;
        Ok((buf[2] as u32) << 16 | (buf[1] as u32) << 8 | (buf[0] as u32))
    }

    // ---- USID and firmware version ------------------------------------------------

    /// Copy USID and firmware version to output registers, then read them.
    ///
    /// Returns `(fw_version, usid)` where `fw_version` is 3 bytes and
    /// `usid` is 6 bytes.
    pub fn read_usid<I2C, E>(&self, i2c: &mut I2C) -> Result<([u8; 3], [u8; 6]), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.exec_ctrl9(i2c, registers::cmd::COPY_USID)?;

        // FW version is in dQW_L, dQW_H, dQX_L (3 bytes)
        let mut fw = [0u8; 3];
        fw[0] = self.read_register(i2c, registers::DQW_L)?;
        fw[1] = self.read_register(i2c, registers::DQW_H)?;
        fw[2] = self.read_register(i2c, registers::DQX_L)?;

        // USID is in dVX_L through dVZ_H (6 bytes)
        let mut usid = [0u8; 6];
        let mut buf = [0u8; 6];
        i2c.write_read(self.addr, &[registers::DVX_L], &mut buf)
            .map_err(Error::I2c)?;
        usid.copy_from_slice(&buf);

        Ok((fw, usid))
    }

    // ---- Pull-up resistor configuration -------------------------------------------

    /// Configure IO pull-up resistors via CTRL9 command 0x11.
    ///
    /// Each bit in `disable_mask` disables one pull-up:
    ///   bit 0: aux_rpu_dis
    ///   bit 1: icm_rpu_dis
    ///   bit 2: cs_rpu_dis
    ///   bit 3: ics_rpu_dis
    pub fn set_pullup_config<I2C, E>(&self, i2c: &mut I2C, disable_mask: u8) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::CAL1_L, disable_mask & 0x0F)?;
        self.exec_ctrl9(i2c, registers::cmd::SET_RPU)
    }

    // ---- Accelerometer ODR / scale runtime change --------------------------------

    /// Change the accelerometer output data rate without touching the
    /// full-scale range. Useful for switching to a low ODR when
    /// entering Wake-on-Motion mode.
    pub fn set_accel_odr<I2C, E>(&self, i2c: &mut I2C, odr: Odr) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl2 = self.read_register(i2c, registers::CTRL2)?;
        // Clear low nibble (aODR), keep bits 7:4 (aST + aFS).
        self.write_register(i2c, registers::CTRL2, (ctrl2 & 0xF0) | odr.bits())
    }

    /// Change the gyroscope output data rate without touching the
    /// full-scale range.
    pub fn set_gyro_odr<I2C, E>(&self, i2c: &mut I2C, odr: Odr) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        let ctrl3 = self.read_register(i2c, registers::CTRL3)?;
        self.write_register(i2c, registers::CTRL3, (ctrl3 & 0xF0) | odr.bits())
    }

    // ---- CTRL9 protocol helper ----------------------------------------------------

    /// Execute a CTRL9 command and wait for completion.
    ///
    /// Protocol:
    /// 1. Write command to CTRL9 (0x0A)
    /// 2. Wait for STATUSINT.bit7 (CmdDone) = 1
    /// 3. Write NOP (0x00) to CTRL9 to acknowledge completion
    ///
    /// Rev 0.6 of the datasheet mislabelled step 2 as STATUS1.bit0 -
    /// that's only true when CTRL8.bit7 = 0, which also routes the
    /// handshake through INT1. `init()` sets CTRL8.bit7 = 1 so the
    /// handshake goes through STATUSINT instead and INT1 stays free
    /// for WoM. Rev 0.9 documents both options in the CTRL8 table.
    fn exec_ctrl9<I2C, E>(&self, i2c: &mut I2C, cmd: u8) -> Result<(), Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.write_register(i2c, registers::CTRL9, cmd)?;

        // Wait for CmdDone (STATUSINT bit 7).
        let mut done = false;
        for _ in 0..1000u16 {
            let s = self.read_register(i2c, registers::STATUSINT)?;
            if s & registers::statusint::CMD_DONE != 0 {
                done = true;
                break;
            }
        }
        if !done {
            return Err(Error::Timeout);
        }

        // Acknowledge: write NOP so the device clears CmdDone.
        self.write_register(i2c, registers::CTRL9, registers::cmd::NOP)
    }

    // ---- diagnostic helpers -----------------------------------------------------

    /// Read any single register by address. Useful for debugging CTRL9 failures.
    pub fn read_raw_register<I2C, E>(&self, i2c: &mut I2C, reg: u8) -> Result<u8, Error<E>>
    where
        I2C: I2cTrait<Error = E>,
    {
        self.read_register(i2c, reg)
    }

    // ---- private helpers --------------------------------------------------------

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
}
