//! Public data types for the QMI8658C IMU driver.

// ---- Error type -----------------------------------------------------------------

/// Error type for IMU operations.
#[derive(Debug)]
pub enum Error<E> {
    /// An I2C transaction failed.
    I2c(E),
    /// WHO_AM_I register did not return the expected chip ID (0x05).
    DeviceNotFound,
    /// A CTRL9 command did not signal completion within the retry limit.
    Timeout,
}

// ---- Accelerometer full-scale range ---------------------------------------------

/// Accelerometer full-scale range (CTRL2 bits 6:4 = aFS[2:0]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelScale {
    /// ±2 g  - sensitivity 16384 LSB/g.
    G2,
    /// ±4 g  - sensitivity 8192 LSB/g.
    G4,
    /// ±8 g  - sensitivity 4096 LSB/g.
    G8,
    /// ±16 g - sensitivity 2048 LSB/g.
    G16,
}

impl AccelScale {
    /// Sensitivity in LSB per g for this range.
    ///
    /// Divide a raw accelerometer reading by this value to get acceleration in g.
    pub const fn lsb_per_g(self) -> u16 {
        match self {
            Self::G2  => 16384,
            Self::G4  =>  8192,
            Self::G8  =>  4096,
            Self::G16 =>  2048,
        }
    }

    /// Register bit value for CTRL2 bits 6:4.
    pub(crate) const fn ctrl2_bits(self) -> u8 {
        match self {
            Self::G2  => 0b000,
            Self::G4  => 0b001,
            Self::G8  => 0b010,
            Self::G16 => 0b011,
        }
    }
}

// ---- Gyroscope full-scale range -------------------------------------------------

/// Gyroscope full-scale range (CTRL3 bits 6:4 = gFS[2:0]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GyroScale {
    /// ±16 dps   - sensitivity 2048 LSB/dps.
    Dps16,
    /// ±32 dps   - sensitivity 1024 LSB/dps.
    Dps32,
    /// ±64 dps   - sensitivity 512 LSB/dps.
    Dps64,
    /// ±128 dps  - sensitivity 256 LSB/dps.
    Dps128,
    /// ±256 dps  - sensitivity 128 LSB/dps.
    Dps256,
    /// ±512 dps  - sensitivity 64 LSB/dps.
    Dps512,
    /// ±1024 dps - sensitivity 32 LSB/dps.
    Dps1024,
    /// ±2048 dps - sensitivity 16 LSB/dps.
    Dps2048,
}

impl GyroScale {
    /// Sensitivity in LSB per dps for this range.
    ///
    /// Divide a raw gyroscope reading by this value to get angular rate in dps.
    pub const fn lsb_per_dps(self) -> u16 {
        match self {
            Self::Dps16   => 2048,
            Self::Dps32   => 1024,
            Self::Dps64   =>  512,
            Self::Dps128  =>  256,
            Self::Dps256  =>  128,
            Self::Dps512  =>   64,
            Self::Dps1024 =>   32,
            Self::Dps2048 =>   16,
        }
    }

    /// Register bit value for CTRL3 bits 6:4.
    pub(crate) const fn ctrl3_bits(self) -> u8 {
        match self {
            Self::Dps16   => 0b000,
            Self::Dps32   => 0b001,
            Self::Dps64   => 0b010,
            Self::Dps128  => 0b011,
            Self::Dps256  => 0b100,
            Self::Dps512  => 0b101,
            Self::Dps1024 => 0b110,
            Self::Dps2048 => 0b111,
        }
    }
}

// ---- Output data rate -----------------------------------------------------------

/// Output data rate for accelerometer and gyroscope (bits 3:0 of CTRL2 / CTRL3).
///
/// Both sensors use the same ODR field encoding. The gyroscope only supports
/// the Normal-mode rates (Hz8000-Hz31_25); the low-power rates are accel-only
/// and are listed in [`AccelLpOdr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Odr {
    /// 8000 Hz (Normal mode, 100% duty cycle).
    Hz8000,
    /// 4000 Hz (Normal mode, 100% duty cycle).
    Hz4000,
    /// 2000 Hz (Normal mode, 100% duty cycle).
    Hz2000,
    /// 1000 Hz (Normal mode, 100% duty cycle).
    Hz1000,
    /// 500 Hz (Normal mode, 100% duty cycle).
    Hz500,
    /// 250 Hz (Normal mode, 100% duty cycle).
    Hz250,
    /// 125 Hz (Normal mode, 100% duty cycle).
    Hz125,
    /// 62.5 Hz (Normal mode, 100% duty cycle).
    Hz62_5,
    /// 31.25 Hz (Normal mode, 100% duty cycle).
    Hz31_25,
}

impl Odr {
    /// Register bit value for bits 3:0 of CTRL2 or CTRL3.
    pub(crate) const fn bits(self) -> u8 {
        match self {
            Self::Hz8000  => 0b0000,
            Self::Hz4000  => 0b0001,
            Self::Hz2000  => 0b0010,
            Self::Hz1000  => 0b0011,
            Self::Hz500   => 0b0100,
            Self::Hz250   => 0b0101,
            Self::Hz125   => 0b0110,
            Self::Hz62_5  => 0b0111,
            Self::Hz31_25 => 0b1000,
        }
    }
}

// ---- Low-pass filter ------------------------------------------------------------

/// Low-pass filter bandwidth mode (CTRL5 bits 6:5 for gyro, bits 2:1 for accel).
///
/// Cutoff frequency is expressed as a percentage of the configured ODR.
/// At 125 Hz ODR: Mode0 = 3.3 Hz, Mode1 = 4.5 Hz, Mode2 = 6.6 Hz, Mode3 = 17.5 Hz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LpfMode {
    /// 2.62% of ODR.
    Mode0,
    /// 3.59% of ODR.
    Mode1,
    /// 5.32% of ODR.
    Mode2,
    /// 14.0% of ODR.
    Mode3,
}

impl LpfMode {
    pub(crate) const fn bits(self) -> u8 {
        match self {
            Self::Mode0 => 0b00,
            Self::Mode1 => 0b01,
            Self::Mode2 => 0b10,
            Self::Mode3 => 0b11,
        }
    }
}

// ---- AccelLpOdr (low-power accelerometer ODR) -----------------------------------

/// Accelerometer low-power output data rates (CTRL2 bits 3:0, codes 0x0C-0x0F).
///
/// These are only valid for the accelerometer; the gyroscope does not
/// support low-power rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelLpOdr {
    /// 128 Hz, 100% duty cycle.
    Hz128 = 0x0C,
    /// 21 Hz, 58% duty cycle.
    Hz21 = 0x0D,
    /// 11 Hz, 31% duty cycle.
    Hz11 = 0x0E,
    /// 3 Hz, 8.5% duty cycle.
    Hz3 = 0x0F,
}

// ---- FIFO types -----------------------------------------------------------------

/// FIFO operating mode (FIFO_CTRL bits 1:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FifoMode {
    /// FIFO disabled. Sensor data goes directly to output registers.
    Bypass = 0,
    /// FIFO mode. Stops collecting when full.
    Fifo = 1,
    /// Streaming mode. Circular buffer, oldest data discarded when full.
    Streaming = 2,
}

/// FIFO size in samples per enabled sensor (FIFO_CTRL bits 3:2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FifoSize {
    /// 16 samples per enabled sensor.
    Samples16 = 0,
    /// 32 samples per enabled sensor.
    Samples32 = 1,
    /// 64 samples per enabled sensor.
    Samples64 = 2,
    /// 128 samples per enabled sensor (max 2 sensors).
    Samples128 = 3,
}

/// FIFO status flags from FIFO_STATUS register (0x16).
#[derive(Debug, Clone, Copy, Default)]
pub struct FifoStatus {
    /// FIFO is full.
    pub full: bool,
    /// Watermark level reached.
    pub watermark: bool,
    /// Overflow occurred (write attempted while full).
    pub overflow: bool,
    /// FIFO contains data.
    pub not_empty: bool,
    /// Total FIFO sample count in bytes (10-bit value).
    pub sample_count: u16,
}

// ---- AttitudeEngine types -------------------------------------------------------

/// AttitudeEngine output data rate (CTRL6 bits 2:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AeOdr {
    Hz1  = 0,
    Hz2  = 1,
    Hz4  = 2,
    Hz8  = 3,
    Hz16 = 4,
    Hz32 = 5,
    Hz64 = 6,
}

/// AttitudeEngine quaternion increment output.
///
/// The quaternion represents the incremental rotation since the last
/// sample. Divide each component by 2^14 to get the actual value.
#[derive(Debug, Clone, Copy, Default)]
pub struct Quaternion {
    pub dqw: i16,
    pub dqx: i16,
    pub dqy: i16,
    pub dqz: i16,
}

/// AttitudeEngine velocity increment output.
///
/// Represents the incremental velocity change since the last sample.
#[derive(Debug, Clone, Copy, Default)]
pub struct VelocityIncrement {
    pub dvx: i16,
    pub dvy: i16,
    pub dvz: i16,
}

// ---- Wake-on-Motion types -------------------------------------------------------

/// Wake-on-Motion interrupt pin selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WomInterrupt {
    /// INT1, initial value low.
    Int1Low = 0b00,
    /// INT1, initial value high.
    Int1High = 0b10,
    /// INT2, initial value low.
    Int2Low = 0b01,
    /// INT2, initial value high.
    Int2High = 0b11,
}

/// Wake-on-Motion configuration.
///
/// Written to CAL1_L and CAL1_H, then CTRL9 command 0x08 is issued.
#[derive(Debug, Clone, Copy)]
pub struct WomConfig {
    /// Threshold in mg (1 mg/LSB). 0 disables WoM.
    pub threshold_mg: u8,
    /// Which interrupt pin to use and its initial value.
    pub interrupt: WomInterrupt,
    /// Blanking time in number of accelerometer samples (0-63).
    /// Ignores this many samples after WoM is enabled to avoid
    /// spurious wakeups from startup transients.
    pub blanking_samples: u8,
}

// ---- Sensor data snapshot -------------------------------------------------------

// The snapshot struct is the cross-chip data contract and lives with
// the seam; re-exported here so `qmi8658::ImuData` keeps resolving.
pub use crate::imu::ImuData;

impl ImuData {
    /// Temperature rounded to the nearest whole degree Celsius.
    ///
    /// The QMI8658C encodes temperature as a signed 16-bit value with 1/256 °C
    /// resolution, so `temp_raw / 256` gives the integer part.
    pub fn temp_celsius(&self) -> i8 {
        (self.temp_raw >> 8) as i8
    }
}

// ---- Self-test results ----------------------------------------------------------

/// Accelerometer self-test result per datasheet section 11.1.
///
/// Values are in milli-g. A functional sensor produces an
/// absolute response greater than 200 mg on every axis.
#[derive(Debug, Clone, Copy)]
pub struct AccelSelfTestResult {
    pub x_mg: i32,
    pub y_mg: i32,
    pub z_mg: i32,
    pub passed: bool,
}

/// Gyroscope self-test result per datasheet section 11.2.
///
/// Values are in degrees per second. A functional sensor
/// produces an absolute response greater than 300 dps on
/// every axis.
#[derive(Debug, Clone, Copy)]
pub struct GyroSelfTestResult {
    pub x_dps: i32,
    pub y_dps: i32,
    pub z_dps: i32,
    pub passed: bool,
}
