//! BHI260AP host interface constants, named per the datasheet
//! (BST-BHI260AP-DS000-02) so the code can be checked against the
//! document table by table.

/// Host interface register map (datasheet Table 17).
pub mod reg {
    // DMA channels (Table 6/7). Not auto-incrementing - a burst on one
    // of these streams the channel's data.
    pub const CHANNEL_CMD: u8 = 0x00; // write-only: command packets in
    pub const CHANNEL_WAKE_FIFO: u8 = 0x01; // read-only: wake-up FIFO out
    pub const CHANNEL_NONWAKE_FIFO: u8 = 0x02; // read-only: non-wake-up FIFO out
    // Read-only: status/debug out. NOTE: in synchronous mode (the
    // reset default) this channel carries raw status packets with NO
    // transfer-length prefix - the FIFO framing of channels 1/2 only
    // applies here in asynchronous mode (Section 14.2).
    pub const CHANNEL_STATUS_FIFO: u8 = 0x03;

    pub const CHIP_CONTROL: u8 = 0x05; // Table 18
    pub const HOST_INTERFACE_CTRL: u8 = 0x06; // Table 19
    pub const HOST_INTERRUPT_CTRL: u8 = 0x07; // Table 20
    // 0x08-0x13: WGP1-WGP3 general purpose host-writeable (unused here)
    pub const RESET_REQUEST: u8 = 0x14; // Table 21, bit 0 self-clearing
    pub const TIMESTAMP_EVENT_REQUEST: u8 = 0x15; // Table 22
    pub const HOST_CONTROL: u8 = 0x16; // Table 24 (SPI wires / I2C watchdog)
    pub const HOST_STATUS: u8 = 0x17; // Table 25
    // 0x18-0x1B: Host Channel CRC (ISO-3309, of the last channel used)
    pub const HOST_CRC: u8 = 0x18;
    pub const PRODUCT_ID: u8 = 0x1C; // reads 0x89 (Fuser2)
    pub const REVISION_ID: u8 = 0x1D; // reads 0x02 or 0x03
    pub const ROM_VERSION: u8 = 0x1E; // u16 LSB first, reads 0x142E
    pub const KERNEL_VERSION: u8 = 0x20; // u16, 0 before firmware boot
    pub const USER_VERSION: u8 = 0x22; // u16, 0 before firmware boot
    pub const FEATURE_STATUS: u8 = 0x24; // Table 26
    pub const BOOT_STATUS: u8 = 0x25; // Table 27
    pub const HOST_INTERRUPT_TIMESTAMP: u8 = 0x26; // 40 bit, 0x26-0x2A
    pub const CHIP_ID: u8 = 0x2B; // reads 0x70 or 0xF0
    pub const INTERRUPT_STATUS: u8 = 0x2D; // Table 28
    pub const ERROR_VALUE: u8 = 0x2E; // Table 29
    pub const ERROR_AUX: u8 = 0x2F;
    pub const DEBUG_VALUE: u8 = 0x30;
    pub const DEBUG_STATE: u8 = 0x31; // Table 30
    // 0x32-0x3D: RGP5-RGP7 general purpose host-readable (unused here)
}

/// Chip Control (0x05) bits, Table 18.
pub mod chip_control {
    /// Cap the core at 20 MHz during upload/verify (lower peak current;
    /// default is full speed with up to ~3 mA draw).
    pub const CPU_TURBO_DISABLE: u8 = 1 << 0;
    /// Clear the error and debug registers (0x2E-0x31).
    pub const CLEAR_ERROR_REGS: u8 = 1 << 1;
}

/// Host Interface Control (0x06) bits, Table 19.
pub mod hif_control {
    pub const ABORT_CH0: u8 = 1 << 0;
    pub const ABORT_CH1: u8 = 1 << 1;
    pub const ABORT_CH2: u8 = 1 << 2;
    pub const ABORT_CH3: u8 = 1 << 3;
    /// Host power status: 1 = suspended (only wake-up sensors raise the
    /// host interrupt), 0 = awake (either FIFO may).
    pub const AP_SUSPENDED: u8 = 1 << 4;
    /// Route Timestamp Event Request responses to the Host Interrupt
    /// Timestamp registers instead of a status packet.
    pub const TIMESTAMP_EVENT_REQUEST: u8 = 1 << 6;
    /// Status/debug FIFO mode: 0 = synchronous (command responses
    /// only), 1 = asynchronous (FIFO-formatted, includes debug/errors).
    pub const ASYNC_STATUS_CHANNEL: u8 = 1 << 7;
}

/// Host Interrupt Control (0x07) bits, Table 20. All mask bits: 1
/// disables that interrupt source. Reset value 0 = active high, level,
/// push-pull, everything enabled.
pub mod irq_control {
    pub const MASK_WAKE_FIFO: u8 = 1 << 0;
    pub const MASK_NONWAKE_FIFO: u8 = 1 << 1;
    pub const MASK_STATUS_AVAILABLE: u8 = 1 << 2;
    pub const MASK_DEBUG_AVAILABLE: u8 = 1 << 3;
    pub const MASK_FAULT: u8 = 1 << 4;
    pub const ACTIVE_LOW: u8 = 1 << 5;
    pub const EDGE: u8 = 1 << 6; // 1 = 10 us pulse, 0 = level until drained
    pub const OPEN_DRAIN: u8 = 1 << 7;
}

/// Boot Status (0x25) bits, Table 27.
pub mod boot_status {
    pub const FLASH_DETECTED: u8 = 1 << 0;
    pub const FLASH_VERIFY_DONE: u8 = 1 << 1;
    pub const FLASH_VERIFY_ERROR: u8 = 1 << 2;
    pub const NO_FLASH: u8 = 1 << 3;
    /// Set when the bootloader (or booted firmware) accepts host
    /// commands. Clears during upload-to-boot transitions.
    pub const HOST_INTERFACE_READY: u8 = 1 << 4;
    pub const FIRMWARE_VERIFY_DONE: u8 = 1 << 5;
    pub const FIRMWARE_VERIFY_ERROR: u8 = 1 << 6;
    /// 1 = firmware halted (bootloader still in charge).
    pub const FIRMWARE_IDLE: u8 = 1 << 7;
}

/// Interrupt Status (0x2D) bits/fields, Table 28. The two FIFO fields
/// are 2 bits wide: 0 none, 1 immediate, 2 latency, 3 watermark.
pub mod int_status {
    pub const HOST_IRQ_ASSERTED: u8 = 1 << 0;
    pub const WAKE_FIFO_SHIFT: u8 = 1;
    pub const NONWAKE_FIFO_SHIFT: u8 = 3;
    pub const FIFO_FIELD_MASK: u8 = 0b11;
    pub const STATUS: u8 = 1 << 5; // sync status packet available
    pub const DEBUG: u8 = 1 << 6; // async status/debug data available
    pub const RESET_OR_FAULT: u8 = 1 << 7;

    /// Any of the three output channels has data pending.
    pub const ANY_DATA: u8 = 0b0111_1110;
}

/// Host Status (0x17) bits, Table 25.
pub mod host_status {
    pub const POWER_STATE_SLEEPING: u8 = 1 << 0;
    pub const HOST_PROTOCOL_SPI: u8 = 1 << 1;
}

/// Host interface commands (Table 31), written to channel 0 as
/// `[id u16 le][content len u16 le][content..]`, zero-padded to a
/// multiple of 4 bytes (padding included in the length field).
pub mod cmd {
    pub const DOWNLOAD_POST_MORTEM: u16 = 0x0001;
    /// Bootloader: firmware upload. Length field = 32-bit WORD count of
    /// the whole image (unique among commands - everything else counts
    /// bytes).
    pub const UPLOAD_TO_PROGRAM_RAM: u16 = 0x0002;
    pub const BOOT_PROGRAM_RAM: u16 = 0x0003;
    pub const ERASE_FLASH: u16 = 0x0004;
    pub const WRITE_FLASH: u16 = 0x0005;
    pub const BOOT_FLASH: u16 = 0x0006;
    pub const SET_INJECTION_MODE: u16 = 0x0007;
    pub const INJECT_SENSOR_DATA: u16 = 0x0008;
    pub const FIFO_FLUSH: u16 = 0x0009;
    pub const SOFT_PASSTHROUGH: u16 = 0x000A;
    pub const REQUEST_SELF_TEST: u16 = 0x000B;
    pub const REQUEST_FOC: u16 = 0x000C;
    pub const CONFIGURE_SENSOR: u16 = 0x000D;
    pub const CHANGE_DYNAMIC_RANGE: u16 = 0x000E;
    pub const SET_CHANGE_SENSITIVITY: u16 = 0x000F;
    pub const DEBUG_TEST: u16 = 0x0010;
    pub const DUT_CONTINUE: u16 = 0x0011;
    pub const DUT_START_TEST: u16 = 0x0012;
    pub const CONTROL_FIFO_FORMAT: u16 = 0x0015;
    pub const RAISE_HOST_INTERFACE_SPEED: u16 = 0x0017;
    /// Parameter write = 0x0000 | param id, parameter read = 0x1000 |
    /// param id (Section 13.3.1).
    pub const PARAM_READ_FLAG: u16 = 0x1000;
}

/// Status packet codes on the status/debug FIFO (channel 3).
pub mod status {
    pub const CRASH_DUMP: u16 = 0x0003;
    pub const INJECTED_SENSOR_CONFIG_REQUEST: u16 = 0x0004;
    pub const SOFT_PASSTHROUGH_RESULTS: u16 = 0x0005;
    pub const SELF_TEST_RESULTS: u16 = 0x0006;
    pub const FOC_RESULTS: u16 = 0x0007;
    pub const FLASH_ERASE_COMPLETE: u16 = 0x000A;
    pub const FLASH_WRITE_COMPLETE: u16 = 0x000B;
    pub const TIMESTAMP_EVENT: u16 = 0x000D;
    /// Table 86; contents: [cmd u16][error u8][reserved].
    pub const COMMAND_ERROR: u16 = 0x000F;
}

/// FIFO Flush (0x0009) flush values, Table 47.
pub mod flush {
    pub const SEND_ALL: u8 = 0xFF;
    pub const DISCARD_ALL: u8 = 0xFE;
    pub const SEND_WAKE: u8 = 0xFD;
    pub const SEND_NONWAKE: u8 = 0xFC;
    pub const DISCARD_WAKE: u8 = 0xFB;
    pub const DISCARD_NONWAKE: u8 = 0xFA;
    pub const DISCARD_STATUS: u8 = 0xF9;
}

/// Parameter IDs (Table 61/62 and Sections 13.3.3-13.3.9).
pub mod param {
    // System parameters
    pub const META_EVENT_CTRL_NONWAKE: u16 = 0x0101;
    pub const META_EVENT_CTRL_WAKE: u16 = 0x0102;
    pub const FIFO_CONTROL: u16 = 0x0103; // watermarks (rw) + sizes (ro)
    pub const FIRMWARE_VERSION: u16 = 0x0104;
    pub const TIMESTAMPS: u16 = 0x0105; // 3x 40-bit, 1/64000 s units
    pub const FRAMEWORK_STATUS: u16 = 0x0106;
    /// 256-bit bitmap of compiled-in virtual sensors, bit = sensor ID.
    pub const VIRT_SENSORS_PRESENT: u16 = 0x011F;
    /// 64-bit bitmap of present physical sensors (BSX input IDs).
    pub const PHYS_SENSORS_PRESENT: u16 = 0x0120;
    /// Physical Sensor Information: 0x0120 + physical sensor ID
    /// (write form = orientation matrix, Table 70). NOTE: the
    /// datasheet's prose (Section 13.3.2.7, "0x0121 refers to
    /// Physical Sensor ID 0") is WRONG on real silicon - the chip
    /// rejects that mapping with a command error. Bosch's reference
    /// host library uses 0x0120 + id (accel 1 -> 0x0121), and
    /// hardware confirms it (verified 2026-07-28).
    pub const PHYS_SENSOR_INFO_BASE: u16 = 0x0120;
    // BSX algorithm parameters
    pub const BSX_CALIB_ACCEL: u16 = 0x0201;
    pub const BSX_CALIB_GYRO: u16 = 0x0203;
    pub const BSX_CALIB_MAG: u16 = 0x0205;
    pub const BSX_SIC_MATRIX: u16 = 0x027D;
    pub const BSX_VERSION: u16 = 0x027E;
    /// Virtual Sensor Information base: 0x0300 + virtual sensor ID
    /// (Table 74, 28 bytes, read-only).
    pub const VIRT_SENSOR_INFO_BASE: u16 = 0x0300;
    /// Virtual Sensor Configuration base: 0x0500 + virtual sensor ID
    /// (Table 75, 12 bytes, read-only).
    pub const VIRT_SENSOR_CONF_BASE: u16 = 0x0500;
}

/// Physical sensor IDs (BSX input IDs, Section 13.3.2.6).
pub mod phys {
    pub const ACCEL: u8 = 1;
    pub const GYRO: u8 = 3;
    pub const MAG: u8 = 5;
    pub const TEMP_GYRO: u8 = 7;
    pub const ANY_MOTION: u8 = 9;
    pub const PRESSURE: u8 = 11;
    pub const POSITION: u8 = 13;
    pub const HUMIDITY: u8 = 15;
    pub const TEMPERATURE: u8 = 17;
    pub const GAS_RESISTOR: u8 = 19;
    pub const STEP_COUNTER: u8 = 32;
    pub const STEP_DETECTOR: u8 = 33;
    pub const SIGNIFICANT_MOTION: u8 = 34;
    pub const PHYS_ANY_MOTION: u8 = 35;
    pub const EXT_CAMERA_INPUT: u8 = 36;
    pub const GPS: u8 = 48;
    pub const LIGHT: u8 = 49;
    pub const PROXIMITY: u8 = 50;
}

/// Firmware error codes from the Error Value register (0x2E),
/// Table 29. Only decode - the categories drive the recovery strategy
/// of Section 17 (fatal/hardware -> reset + re-upload, temporary ->
/// log and continue).
pub fn error_description(code: u8) -> &'static str {
    match code {
        0x00 => "no error",
        0x10 => "firmware expected version mismatch (fatal)",
        0x11 => "firmware upload: bad header CRC (fatal)",
        0x12 => "firmware upload: SHA hash mismatch (fatal)",
        0x13 => "firmware upload: bad image CRC (fatal)",
        0x14 => "firmware upload: ECDSA signature verification failed (fatal)",
        0x15 => "firmware upload: bad public key CRC (fatal)",
        0x16 => "firmware upload: signed firmware required (fatal)",
        0x17 => "firmware upload: FW header missing (fatal)",
        0x19 => "unexpected watchdog reset (fatal)",
        0x1A => "ROM version mismatch (fatal)",
        0x1B => "fatal firmware error",
        0x1C => "chained fw: next payload not found (fatal)",
        0x1D => "chained fw: payload not valid (fatal)",
        0x1E => "chained fw: payload entries invalid (fatal)",
        0x1F => "bootloader: OTP CRC invalid (fatal)",
        0x20 => "firmware init failed (hardware)",
        0x21 => "sensor init: unexpected device ID (hardware)",
        0x22 => "sensor init: no response from device",
        0x23 => "sensor init: unknown",
        0x24 => "sensor error: no valid data",
        0x25 => "slow sample rate (temporary)",
        0x26 => "data overflow / saturated sensor data (fatal)",
        0x27 => "stack overflow (fatal)",
        0x28 => "insufficient free RAM (fatal)",
        0x29 => "sensor init: driver parsing error (fatal)",
        0x2A => "too many RAM banks required",
        0x2B => "invalid event specified",
        0x2C => "more than 32 on-change",
        0x2D => "firmware too large (fatal)",
        0x2F => "invalid RAM banks (fatal)",
        0x30 => "math error (fatal)",
        0x40 => "memory error (fatal)",
        0x41 => "SWI3 error (fatal)",
        0x42 => "SWI4 error (fatal)",
        0x43 => "illegal instruction (fatal)",
        0x44 => "unhandled interrupt / exception / postmortem available (fatal)",
        0x45 => "invalid memory access (fatal)",
        0x50 => "algorithm error: BSX init",
        0x51 => "algorithm error: BSX do step",
        0x52 => "algorithm error: update sub",
        0x53 => "algorithm error: get sub",
        0x54 => "algorithm error: get phys",
        0x55 => "algorithm error: unsupported phys rate",
        0x56 => "algorithm error: cannot find BSX driver",
        0x60 => "sensor self-test failure (hardware)",
        0x61 => "sensor self-test X axis failure (hardware)",
        0x62 => "sensor self-test Y axis failure (hardware)",
        0x64 => "sensor self-test Z axis failure (hardware)",
        0x65 => "FOC failure (hardware)",
        0x66 => "sensor busy (hardware)",
        0x6F => "self-test or FOC unsupported",
        0x72 => "no host interrupt set (fatal)",
        0x73 => "event ID has no known size",
        0x75 => "host download channel underflow (temporary)",
        0x76 => "host upload channel overflow (temporary)",
        0x77 => "host download channel empty (temporary)",
        0x78 => "DMA error (hardware)",
        0x79 => "corrupted input block chain",
        0x7A => "corrupted output block chain",
        0x7B => "buffer block manager error",
        0x7C => "input channel not word aligned (temporary)",
        0x7D => "too many flush events (temporary)",
        0x7E => "unknown host channel error (hardware)",
        0x81 => "decimation too large",
        0x90 => "master SPI/I2C queue overflow (fatal)",
        0x91 => "SPI/I2C callback error (fatal)",
        0xA0 => "timer scheduling error (fatal)",
        0xB0 => "invalid GPIO for host IRQ (fatal)",
        0xB1 => "error sending initialized meta events (fatal)",
        0xC0 => "command error (temporary)",
        0xC1 => "command too long (temporary)",
        0xC2 => "command buffer overflow (temporary)",
        0xD0 => "user mode: sys call invalid (fatal)",
        0xD1 => "user mode: trap invalid (fatal)",
        0xE1 => "firmware upload failed: header corrupt (fatal)",
        0xE2 => "sensor data injection: invalid input stream",
        _ => "unknown error code",
    }
}
