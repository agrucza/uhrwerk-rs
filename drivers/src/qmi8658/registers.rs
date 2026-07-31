//! QMI8658C register addresses and bit-field constants.
//!
//! Source: QMI8658C Datasheet Rev 0.6 (January 13, 2021).

// ---- General purpose registers --------------------------------------------------

/// WHO_AM_I (0x00, RO): device identifier - always reads 0x05.
pub const WHO_AM_I:    u8 = 0x00;
/// REVISION_ID (0x01, RO): silicon revision - reads 0x79.
pub const REVISION_ID: u8 = 0x01;

/// Expected value read from WHO_AM_I to confirm device identity.
pub const CHIP_ID: u8 = 0x05;

// ---- Setup and control registers ------------------------------------------------

/// CTRL1 (0x02, RW, default 0x20): serial interface and oscillator control.
///
/// Default has bit 5 (SPI_BE) = 1 (big-endian for SPI reads, irrelevant over I2C).
/// Set bit 6 (SPI_AI) to enable address auto-increment for burst reads.
pub const CTRL1: u8 = 0x02;

/// CTRL2 (0x03, RW, default 0x00): accelerometer settings.
///
/// Bit 7 = aST (self-test), bits 6:4 = aFS[2:0] (full-scale), bits 3:0 = aODR[3:0].
pub const CTRL2: u8 = 0x03;

/// CTRL3 (0x04, RW, default 0x00): gyroscope settings.
///
/// Bit 7 = gST (self-test), bits 6:4 = gFS[2:0] (full-scale), bits 3:0 = gODR[3:0].
pub const CTRL3: u8 = 0x04;

/// CTRL4 (0x05, RW, default 0x00): magnetometer settings.
///
/// Bits 6:3 = mDEV[3:0] (device selection), bits 2:0 = mODR[2:0].
pub const CTRL4: u8 = 0x05;

/// CTRL5 (0x06, RW, default 0x00): sensor DSP / low-pass filter settings.
///
/// Bits 6:5 = gLPF_MODE, bit 4 = gLPF_EN, bits 2:1 = aLPF_MODE, bit 0 = aLPF_EN.
pub const CTRL5: u8 = 0x06;

/// CTRL6 (0x07, RW, default 0x00): AttitudeEngine ODR and Motion on Demand.
///
/// Bit 7 = sMoD (Motion on Demand enable), bits 2:0 = sODR[2:0] (AE ODR).
pub const CTRL6: u8 = 0x07;

/// CTRL7 (0x08, RW, default 0x00): sensor enable / disable.
///
/// Bit 7 = syncSmpl, bit 6 = sys_hs, bit 4 = gSN (snooze),
/// bit 3 = sEN (AttitudeEngine), bit 2 = mEN (magnetometer),
/// bit 1 = gEN (gyroscope), bit 0 = aEN (accelerometer).
pub const CTRL7: u8 = 0x08;

/// CTRL8 (0x09, RW, default 0x00): Motion Detection Control.
///
/// Per QMI8658C datasheet Rev 0.9 (Rev 0.6 mislabeled this register
/// as "reserved"):
///
///   * Bit 7: CTRL9 handshake type (0 = INT1, 1 = STATUSINT.bit7)
///   * Bit 6: INT pin for motion-detection event
///   * Bits 4..0: per-engine enables (pedometer, sig-motion, no-motion,
///     any-motion, tap) - all QMI8658A-only features, not present on
///     the C variant.
///
/// The driver sets bit 7 = 1 at init so the CTRL9 command protocol
/// uses STATUSINT.bit7 for handshake instead of INT1. Otherwise every
/// CTRL9 command drives INT1 as part of the handshake and leaves it
/// in an unpredictable state, breaking WoM signalling.
pub const CTRL8: u8 = 0x09;

/// CTRL9 (0x0A, RW, default 0x00): host command register.
///
/// Writing a command code here initiates a CTRL9 protocol operation.
/// The device signals completion via STATUS1 bit 0 (CmdDone) and INT1.
pub const CTRL9: u8 = 0x0A;

// ---- Host-controlled calibration registers (CAL1-CAL4) --------------------------

pub const CAL1_L: u8 = 0x0B;
pub const CAL1_H: u8 = 0x0C;
pub const CAL2_L: u8 = 0x0D;
pub const CAL2_H: u8 = 0x0E;
pub const CAL3_L: u8 = 0x0F;
pub const CAL3_H: u8 = 0x10;
pub const CAL4_L: u8 = 0x11;
pub const CAL4_H: u8 = 0x12;

// ---- FIFO registers -------------------------------------------------------------

/// FIFO_WTM_TH (0x13, RW): number of ODR samples before FIFO watermark fires.
pub const FIFO_WTM_TH:  u8 = 0x13;
/// FIFO_CTRL (0x14, RW): FIFO mode and size configuration.
///
/// Bit 7 = FIFO_RD_MODE, bits 3:2 = FIFO_SIZE[1:0], bits 1:0 = FIFO_MODE[1:0].
pub const FIFO_CTRL:    u8 = 0x14;
/// FIFO_SMPL_CNT (0x15, RO): lower 8 bits of the FIFO sample count (in bytes).
pub const FIFO_SMPL_CNT: u8 = 0x15;
/// FIFO_STATUS (0x16, RO): FIFO full / watermark / overflow / not-empty flags.
///
/// Bit 7 = FIFO_FULL, bit 6 = FIFO_WTM, bit 5 = FIFO_OVFLOW,
/// bit 4 = FIFO_NOT_EMPTY, bits 1:0 = FIFO_SMPL_CNT_MSB[1:0].
pub const FIFO_STATUS:  u8 = 0x16;
/// FIFO_DATA (0x17, RO): read FIFO data through this register.
pub const FIFO_DATA:    u8 = 0x17;

// ---- Status registers -----------------------------------------------------------

/// I2CM_STATUS (0x2C, RO): I2C master (magnetometer) status.
///
/// Bit 2 = I2CM_done, bit 1 = Data_VLD, bit 0 = I2CM_active.
pub const I2CM_STATUS: u8 = 0x2C;

/// STATUSINT (0x2D, RO): sensor data lock / availability flags and CTRL9 status.
///
/// Bit 7 = CmdDone (CTRL9 command complete - not in Rev 0.6 PDF but present on
/// actual silicon rev 0x7C+), bit 1 = Locked, bit 0 = Avail.
/// When syncSmpl = 0, bit 1 mirrors INT1 and bit 0 mirrors INT2.
pub const STATUSINT: u8 = 0x2D;

/// STATUS0 (0x2E, RO): per-sensor new-data flags.
///
/// Bit 3 = sDA (AttitudeEngine), bit 2 = mDA (magnetometer),
/// bit 1 = gDA (gyroscope), bit 0 = aDA (accelerometer).
pub const STATUS0: u8 = 0x2E;

/// STATUS1 (0x2F, RO): miscellaneous status (per Rev 0.6 PDF).
///
/// Bit 0 = CmdDone per Rev 0.6, but on actual silicon (rev 0x7C+) this register
/// stays 0x00. Use STATUSINT bit 7 to detect CTRL9 command completion instead.
pub const STATUS1: u8 = 0x2F;

// ---- Timestamp registers --------------------------------------------------------

/// TIMESTAMP_LOW (0x30, RO): lower 8 bits of the 24-bit sample timestamp.
pub const TIMESTAMP_LOW:  u8 = 0x30;
/// TIMESTAMP_MID (0x31, RO): middle 8 bits of the 24-bit sample timestamp.
pub const TIMESTAMP_MID:  u8 = 0x31;
/// TIMESTAMP_HIGH (0x32, RO): upper 8 bits of the 24-bit sample timestamp.
pub const TIMESTAMP_HIGH: u8 = 0x32;

// ---- Sensor data output registers -----------------------------------------------

/// TEMP_L (0x33, RO): temperature low byte. Read 2 bytes for the full i16 value.
pub const TEMP_L:  u8 = 0x33;
/// TEMP_H (0x34, RO): temperature high byte.
pub const TEMP_H:  u8 = 0x34;

/// AX_L (0x35, RO): accelerometer X low byte. Read 6 bytes for all three axes.
pub const AX_L:    u8 = 0x35;
pub const AX_H:    u8 = 0x36;
pub const AY_L:    u8 = 0x37;
pub const AY_H:    u8 = 0x38;
pub const AZ_L:    u8 = 0x39;
pub const AZ_H:    u8 = 0x3A;

/// GX_L (0x3B, RO): gyroscope X low byte. Read 6 bytes for all three axes.
pub const GX_L:    u8 = 0x3B;
pub const GX_H:    u8 = 0x3C;
pub const GY_L:    u8 = 0x3D;
pub const GY_H:    u8 = 0x3E;
pub const GZ_L:    u8 = 0x3F;
pub const GZ_H:    u8 = 0x40;

/// MX_L (0x41, RO): magnetometer X low byte. Read 6 bytes for all three axes.
pub const MX_L:    u8 = 0x41;
pub const MX_H:    u8 = 0x42;
pub const MY_L:    u8 = 0x43;
pub const MY_H:    u8 = 0x44;
pub const MZ_L:    u8 = 0x45;
pub const MZ_H:    u8 = 0x46;

// ---- AttitudeEngine output registers --------------------------------------------

/// dQW_L (0x49): quaternion increment W low byte. Read 8 bytes for dQW/dQX/dQY/dQZ.
pub const DQW_L:   u8 = 0x49;
pub const DQW_H:   u8 = 0x4A;
pub const DQX_L:   u8 = 0x4B;
pub const DQX_H:   u8 = 0x4C;
pub const DQY_L:   u8 = 0x4D;
pub const DQY_H:   u8 = 0x4E;
pub const DQZ_L:   u8 = 0x4F;
pub const DQZ_H:   u8 = 0x50;

/// dVX_L (0x51): velocity increment X low byte. Read 6 bytes for dVX/dVY/dVZ.
pub const DVX_L:   u8 = 0x51;
pub const DVX_H:   u8 = 0x52;
pub const DVY_L:   u8 = 0x53;
pub const DVY_H:   u8 = 0x54;
pub const DVZ_L:   u8 = 0x55;
pub const DVZ_H:   u8 = 0x56;

/// AE_REG1 (0x57, RO): AttitudeEngine clipping status.
pub const AE_REG1: u8 = 0x57;
/// AE_REG2 (0x58, RO): AttitudeEngine velocity overflow status.
pub const AE_REG2: u8 = 0x58;

// ---- Reset register ------------------------------------------------------------

/// RESET (0x60, WO): Soft Reset Register. Writing 0xB0 triggers a
/// sensor reset immediately.
pub const RESET: u8 = 0x60;

/// Value to write to the RESET register to trigger a soft reset.
pub const RESET_VALUE: u8 = 0xB0;

// ---- CTRL1 bit masks ------------------------------------------------------------

pub mod ctrl1 {
    /// Address auto-increment for sequential register reads (SPI and I2C).
    pub const AUTO_INCREMENT: u8 = 1 << 6;
    /// SPI read data big-endian (default 1 - set in reset value 0x20).
    pub const SPI_BIG_ENDIAN: u8 = 1 << 5;
    /// Disable internal 2 MHz oscillator (keep 0 for normal operation).
    pub const SENSOR_DISABLE: u8 = 1 << 0;
}

// ---- CTRL2 bit masks and field values -------------------------------------------

pub mod ctrl2 {
    /// Enable accelerometer self-test.
    pub const SELF_TEST: u8 = 1 << 7;
    /// Shift for aFS[2:0] (full-scale range) field in bits 6:4.
    pub const FS_SHIFT: u8 = 4;
    /// Mask for aFS[2:0] field (3 bits, pre-shift).
    pub const FS_MASK:  u8 = 0b111;
}

// ---- CTRL3 bit masks and field values -------------------------------------------

pub mod ctrl3 {
    /// Enable gyroscope self-test.
    pub const SELF_TEST: u8 = 1 << 7;
    /// Shift for gFS[2:0] (full-scale range) field in bits 6:4.
    pub const FS_SHIFT: u8 = 4;
    /// Mask for gFS[2:0] field (3 bits, pre-shift).
    pub const FS_MASK:  u8 = 0b111;
}

// ---- CTRL5 bit masks ------------------------------------------------------------

pub mod ctrl5 {
    /// Shift for gLPF_MODE[1:0] field in bits 6:5.
    pub const GLPF_MODE_SHIFT: u8 = 5;
    /// Enable gyroscope low-pass filter.
    pub const GLPF_EN:         u8 = 1 << 4;
    /// Shift for aLPF_MODE[1:0] field in bits 2:1.
    pub const ALPF_MODE_SHIFT: u8 = 1;
    /// Enable accelerometer low-pass filter.
    pub const ALPF_EN:         u8 = 1 << 0;
}

// ---- CTRL8 bit masks ------------------------------------------------------------

pub mod ctrl8 {
    /// Route CTRL9 command handshake through STATUSINT.bit7 instead
    /// of INT1. Required when INT1 is used for WoM (or any other
    /// purpose) so the handshake protocol doesn't clobber the line.
    pub const CTRL9_HANDSHAKE_STATUSINT: u8 = 1 << 7;

    /// Route motion-detection event interrupts to INT1 instead of
    /// INT2. The v0.9 datasheet has a typo on this bit (both values
    /// listed as "INT2") and its note officially only covers
    /// any/no/sig-motion, pedometer, and tap - not WoM. We set it
    /// anyway because our board doesn't wire INT2 to an ESP32 pin,
    /// and because WoM-on-INT1 isn't firing with this bit at its
    /// default, which suggests the "affects these" list may be
    /// incomplete on this silicon rev.
    pub const MOTION_INT_ON_INT1: u8 = 1 << 6;
}

// ---- CTRL7 bit masks ------------------------------------------------------------

pub mod ctrl7 {
    /// Enable synchronised sample mode.
    pub const SYNC_SMPL: u8 = 1 << 7;
    /// High-speed internal clock.
    pub const SYS_HS:    u8 = 1 << 6;
    /// Gyroscope snooze mode (drive only, no sensing).
    pub const GYRO_SNZ:  u8 = 1 << 4;
    /// Enable AttitudeEngine.
    pub const AE_EN:     u8 = 1 << 3;
    /// Enable magnetometer.
    pub const MAG_EN:    u8 = 1 << 2;
    /// Enable gyroscope.
    pub const GYRO_EN:   u8 = 1 << 1;
    /// Enable accelerometer.
    pub const ACCEL_EN:  u8 = 1 << 0;
}

// ---- STATUSINT bit masks --------------------------------------------------------

pub mod statusint {
    /// CTRL9 command execution complete (bit 7 - present on silicon rev 0x7C+,
    /// not documented in Rev 0.6 PDF).
    pub const CMD_DONE: u8 = 1 << 7;
    /// Sensor data locked for reading (syncSmpl mode) / mirrors INT1 otherwise.
    pub const LOCKED:   u8 = 1 << 1;
    /// Sensor data available (syncSmpl mode) / mirrors INT2 otherwise.
    pub const AVAIL:    u8 = 1 << 0;
}

// ---- STATUS0 bit masks ----------------------------------------------------------

pub mod status0 {
    /// New AttitudeEngine data available.
    pub const AE_READY:    u8 = 1 << 3;
    /// New magnetometer data available.
    pub const MAG_READY:   u8 = 1 << 2;
    /// New gyroscope data available.
    pub const GYRO_READY:  u8 = 1 << 1;
    /// New accelerometer data available.
    pub const ACCEL_READY: u8 = 1 << 0;
}

// ---- STATUS1 bit masks ----------------------------------------------------------

pub mod status1 {
    /// CTRL9 command execution complete (bit 0 per Rev 0.6 C datasheet;
    /// actual silicon uses STATUSINT.bit7 instead).
    pub const CMD_DONE: u8 = 1 << 0;
    /// Wake-on-Motion event detected (bit 2). From WoM section 9 of
    /// the C datasheet. Reading STATUS1 clears this bit.
    pub const WOM: u8 = 1 << 2;
}

// ---- CTRL6 bit masks ------------------------------------------------------------

pub mod ctrl6 {
    /// Enable Motion on Demand (requires sEN=1 in CTRL7).
    pub const SMOD: u8 = 1 << 7;
    /// Mask for sODR[2:0] (AttitudeEngine ODR) in bits 2:0.
    pub const SODR_MASK: u8 = 0x07;
}

// ---- FIFO_CTRL bit masks --------------------------------------------------------

pub mod fifo_ctrl {
    /// Put FIFO into read mode (set before reading, clear after).
    /// Automatically set by CTRL9 command REQ_FIFO.
    pub const RD_MODE: u8 = 1 << 7;
    /// Shift for FIFO size field (bits 3:2).
    pub const SIZE_SHIFT: u8 = 2;
    /// Mask for FIFO size field (bits 3:2, pre-shift).
    pub const SIZE_MASK: u8 = 0x03;
    /// Mask for FIFO mode field (bits 1:0).
    pub const MODE_MASK: u8 = 0x03;
}

// ---- FIFO_STATUS bit masks ------------------------------------------------------

pub mod fifo_status {
    pub const FULL:      u8 = 1 << 7;
    pub const WATERMARK: u8 = 1 << 6;
    pub const OVERFLOW:  u8 = 1 << 5;
    pub const NOT_EMPTY: u8 = 1 << 4;
}

// ---- CTRL9 command codes --------------------------------------------------------

pub mod cmd {
    /// No operation / acknowledgement (end of CTRL9 protocol).
    pub const NOP:                     u8 = 0x00;
    /// Apply gyroscope bias from CAL1-CAL3 registers.
    pub const GYRO_BIAS:               u8 = 0x01;
    /// Request Motion on Demand SDI data (requires AE + sMoD enabled).
    pub const REQ_SDI:                 u8 = 0x03;
    /// Reset FIFO.
    pub const RST_FIFO:                u8 = 0x04;
    /// Request FIFO data read.
    pub const REQ_FIFO:                u8 = 0x05;
    /// I2C master write/read (for external magnetometer).
    pub const I2CM_WRITE:              u8 = 0x06;
    /// Write Wake-on-Motion threshold and interrupt configuration.
    pub const WRITE_WOM_SETTING:       u8 = 0x08;
    /// Apply accelerometer delta-offset from CAL1-CAL3 registers.
    pub const ACCEL_HOST_DELTA_OFFSET: u8 = 0x09;
    /// Apply gyroscope delta-offset from CAL1-CAL3 registers.
    pub const GYRO_HOST_DELTA_OFFSET:  u8 = 0x0A;
    /// Copy USID and firmware version to output registers.
    pub const COPY_USID:               u8 = 0x10;
    /// Configure IO pull-up resistors.
    pub const SET_RPU:                 u8 = 0x11;
}
