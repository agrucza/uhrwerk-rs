//! ST25R3916/7 SPI framing, direct commands, and register map.
//!
//! Every value is taken from the ST DS12484 rev 8 datasheet; table
//! numbers are cited inline. The chip has two 64-register spaces
//! (A and B); space B is reached by prefixing the register access
//! with the [`cmd::SPACE_B_ACCESS`] byte inside the same
//! chip-select frame (section 4.3.3).

/// SPI mode-bit patterns for the first byte of a transaction
/// (Table 11). Register addresses OR into the low six bits.
pub mod spi_mode {
    /// Register write: `00` + address.
    pub const REG_WRITE: u8 = 0x00;
    /// Register read: `01` + address.
    pub const REG_READ: u8 = 0x40;
    /// FIFO load: one fixed byte, then 1..=512 payload bytes.
    pub const FIFO_LOAD: u8 = 0x80;
    /// FIFO read: one fixed byte, then data clocks out.
    pub const FIFO_READ: u8 = 0x9F;
    /// Passive-target memory loads (A-config / F-config / TSN).
    pub const PT_LOAD_A_CONFIG: u8 = 0xA0;
    pub const PT_LOAD_F_CONFIG: u8 = 0xA8;
    pub const PT_LOAD_TSN: u8 = 0xAC;
    /// Passive-target memory read; a zero byte must follow before
    /// data clocks out (Table 11 note).
    pub const PT_READ: u8 = 0xBF;
}

/// Direct commands (Table 13). The `11` mode prefix is part of the
/// code. Commands marked "chaining" in the table may be followed by
/// further SPI modes within the same chip-select frame.
pub mod cmd {
    pub const SET_DEFAULT: u8 = 0xC1;
    pub const STOP_ALL_ACTIVITIES: u8 = 0xC2;
    pub const TRANSMIT_WITH_CRC: u8 = 0xC4;
    pub const TRANSMIT_WITHOUT_CRC: u8 = 0xC5;
    pub const TRANSMIT_REQA: u8 = 0xC6;
    pub const TRANSMIT_WUPA: u8 = 0xC7;
    pub const NFC_INITIAL_FIELD_ON: u8 = 0xC8;
    pub const NFC_RESPONSE_FIELD_ON: u8 = 0xC9;
    pub const GO_TO_SENSE: u8 = 0xCD;
    pub const GO_TO_SLEEP: u8 = 0xCE;
    pub const MASK_RECEIVE_DATA: u8 = 0xD0;
    pub const UNMASK_RECEIVE_DATA: u8 = 0xD1;
    pub const CHANGE_AM_MODULATION_STATE: u8 = 0xD2;
    pub const MEASURE_AMPLITUDE: u8 = 0xD3;
    pub const RESET_RX_GAIN: u8 = 0xD5;
    pub const ADJUST_REGULATORS: u8 = 0xD6;
    pub const CALIBRATE_DRIVER_TIMING: u8 = 0xD8;
    pub const MEASURE_PHASE: u8 = 0xD9;
    pub const CLEAR_RSSI: u8 = 0xDA;
    pub const CLEAR_FIFO: u8 = 0xDB;
    pub const ENTER_TRANSPARENT_MODE: u8 = 0xDC;
    pub const CALIBRATE_CAPACITIVE_SENSOR: u8 = 0xDD;
    pub const MEASURE_CAPACITANCE: u8 = 0xDE;
    pub const MEASURE_POWER_SUPPLY: u8 = 0xDF;
    pub const START_GP_TIMER: u8 = 0xE0;
    pub const START_WAKEUP_TIMER: u8 = 0xE1;
    pub const START_MASK_RECEIVE_TIMER: u8 = 0xE2;
    pub const START_NO_RESPONSE_TIMER: u8 = 0xE3;
    pub const START_PPON2_TIMER: u8 = 0xE4;
    pub const STOP_NO_RESPONSE_TIMER: u8 = 0xE8;
    /// Space-B register access prefix (chained within a frame).
    pub const SPACE_B_ACCESS: u8 = 0xFB;
    /// Test-register access prefix; first byte of the mandatory
    /// power-on overheat-protection frame (section 4.1).
    pub const TEST_ACCESS: u8 = 0xFC;
}

/// Register space A addresses (Table 17).
pub mod reg_a {
    pub const IO_CONF1: u8 = 0x00;
    pub const IO_CONF2: u8 = 0x01;
    pub const OP_CONTROL: u8 = 0x02;
    pub const MODE: u8 = 0x03;
    pub const BIT_RATE: u8 = 0x04;
    pub const ISO14443A_NFC: u8 = 0x05;
    pub const ISO14443B_1: u8 = 0x06;
    pub const ISO14443B_FELICA: u8 = 0x07;
    pub const PASSIVE_TARGET: u8 = 0x08;
    pub const STREAM_MODE: u8 = 0x09;
    pub const AUX: u8 = 0x0A;
    pub const RX_CONF1: u8 = 0x0B;
    pub const RX_CONF2: u8 = 0x0C;
    pub const RX_CONF3: u8 = 0x0D;
    pub const RX_CONF4: u8 = 0x0E;
    pub const MASK_RX_TIMER: u8 = 0x0F;
    pub const NO_RESPONSE_TIMER_1: u8 = 0x10;
    pub const NO_RESPONSE_TIMER_2: u8 = 0x11;
    pub const TIMER_EMV_CONTROL: u8 = 0x12;
    pub const GPT_1: u8 = 0x13;
    pub const GPT_2: u8 = 0x14;
    pub const PPON2: u8 = 0x15;
    pub const IRQ_MASK_MAIN: u8 = 0x16;
    pub const IRQ_MASK_TIMER_NFC: u8 = 0x17;
    pub const IRQ_MASK_ERROR_WUP: u8 = 0x18;
    pub const IRQ_MASK_TARGET: u8 = 0x19;
    pub const IRQ_MAIN: u8 = 0x1A;
    pub const IRQ_TIMER_NFC: u8 = 0x1B;
    pub const IRQ_ERROR_WUP: u8 = 0x1C;
    pub const IRQ_TARGET: u8 = 0x1D;
    pub const FIFO_STATUS_1: u8 = 0x1E;
    pub const FIFO_STATUS_2: u8 = 0x1F;
    pub const COLLISION_STATUS: u8 = 0x20;
    pub const PASSIVE_TARGET_STATUS: u8 = 0x21;
    pub const NUM_TX_BYTES_1: u8 = 0x22;
    pub const NUM_TX_BYTES_2: u8 = 0x23;
    pub const BIT_RATE_DETECTION: u8 = 0x24;
    pub const AD_RESULT: u8 = 0x25;
    pub const ANT_TUNE_A: u8 = 0x26;
    pub const ANT_TUNE_B: u8 = 0x27;
    pub const TX_DRIVER: u8 = 0x28;
    pub const PT_MOD: u8 = 0x29;
    pub const FIELD_THRESHOLD_ACT: u8 = 0x2A;
    pub const FIELD_THRESHOLD_DEACT: u8 = 0x2B;
    pub const REGULATOR_CONTROL: u8 = 0x2C;
    pub const RSSI_RESULT: u8 = 0x2D;
    pub const GAIN_RED_STATE: u8 = 0x2E;
    pub const CAP_SENSOR_CONTROL: u8 = 0x2F;
    pub const CAP_SENSOR_RESULT: u8 = 0x30;
    pub const AUX_DISPLAY: u8 = 0x31;
    pub const WUP_TIMER_CONTROL: u8 = 0x32;
    pub const AMPLITUDE_MEASURE_CONF: u8 = 0x33;
    pub const AMPLITUDE_MEASURE_REF: u8 = 0x34;
    pub const AMPLITUDE_MEASURE_AA_RESULT: u8 = 0x35;
    pub const AMPLITUDE_MEASURE_RESULT: u8 = 0x36;
    pub const PHASE_MEASURE_CONF: u8 = 0x37;
    pub const PHASE_MEASURE_REF: u8 = 0x38;
    pub const PHASE_MEASURE_AA_RESULT: u8 = 0x39;
    pub const PHASE_MEASURE_RESULT: u8 = 0x3A;
    pub const CAPACITANCE_MEASURE_CONF: u8 = 0x3B;
    pub const CAPACITANCE_MEASURE_REF: u8 = 0x3C;
    pub const CAPACITANCE_MEASURE_AA_RESULT: u8 = 0x3D;
    pub const CAPACITANCE_MEASURE_RESULT: u8 = 0x3E;
    pub const IC_IDENTITY: u8 = 0x3F;
}

/// Register space B addresses (Table 18); access via the
/// [`cmd::SPACE_B_ACCESS`] prefix.
pub mod reg_b {
    pub const EMD_SUP_CONF: u8 = 0x05;
    pub const SUBC_START_TIME: u8 = 0x06;
    pub const P2P_RX_CONF: u8 = 0x0B;
    pub const CORR_CONF1: u8 = 0x0C;
    pub const CORR_CONF2: u8 = 0x0D;
    pub const SQUELCH_TIMER: u8 = 0x0F;
    pub const FIELD_ON_GT: u8 = 0x15;
    pub const AUX_MOD: u8 = 0x28;
    pub const TX_DRIVER_TIMING: u8 = 0x29;
    pub const RES_AM_MOD: u8 = 0x2A;
    pub const TX_DRIVER_TIMING_DISPLAY: u8 = 0x2B;
    pub const REGULATOR_RESULT: u8 = 0x2C;
    pub const OVERSHOOT_CONF1: u8 = 0x30;
    pub const OVERSHOOT_CONF2: u8 = 0x31;
    pub const UNDERSHOOT_CONF1: u8 = 0x32;
    pub const UNDERSHOOT_CONF2: u8 = 0x33;
}

/// Operation control register bits (Table 21).
pub mod op_control {
    /// Ready mode: oscillator + regulators on.
    pub const EN: u8 = 1 << 7;
    pub const RX_EN: u8 = 1 << 6;
    pub const RX_CHN: u8 = 1 << 5;
    pub const RX_MAN: u8 = 1 << 4;
    pub const TX_EN: u8 = 1 << 3;
    pub const WU: u8 = 1 << 2;
    /// External field detector bits: 01 collision-avoidance
    /// threshold, 10 peer-detection, 11 automatic.
    pub const EN_FD_C1: u8 = 1 << 1;
    pub const EN_FD_C0: u8 = 1 << 0;
}

/// IO configuration register 2 bits (Table 20).
pub mod io_conf2 {
    /// MUST be set on 2.4-3.6 V supplies (ours) after every
    /// power-up; the reset default assumes 5 V.
    pub const SUP3V: u8 = 1 << 7;
    pub const VSPD_OFF: u8 = 1 << 6;
    pub const AAT_EN: u8 = 1 << 5;
    pub const MISO_PD2: u8 = 1 << 4;
    pub const MISO_PD1: u8 = 1 << 3;
    pub const IO_DRV_LVL: u8 = 1 << 2;
    pub const AM_REF_RF: u8 = 1 << 1;
    pub const SLOW_UP: u8 = 1 << 0;
}

/// IO configuration register 1 bits (Table 19).
pub mod io_conf1 {
    pub const SINGLE: u8 = 1 << 7;
    pub const RFO2: u8 = 1 << 6;
    /// out_cl<1:0> = 11 disables the MCU_CLK output entirely.
    pub const OUT_CL1: u8 = 1 << 2;
    pub const OUT_CL0: u8 = 1 << 1;
    pub const LF_CLK_OFF: u8 = 1 << 0;
}

/// Main interrupt register bits (Table 62); the same layout is used
/// for its mask register.
pub mod irq_main {
    pub const OSC: u8 = 1 << 7;
    pub const WATER_LEVEL: u8 = 1 << 6;
    pub const RX_START: u8 = 1 << 5;
    pub const RX_END: u8 = 1 << 4;
    pub const TX_END: u8 = 1 << 3;
    pub const COLLISION: u8 = 1 << 2;
    pub const RX_RESTART: u8 = 1 << 1;
}

/// Timer and NFC interrupt register bits (Table 63).
pub mod irq_timer_nfc {
    pub const DCT: u8 = 1 << 7;
    pub const NO_RESPONSE: u8 = 1 << 6;
    pub const GP_TIMER: u8 = 1 << 5;
    pub const FIELD_ON: u8 = 1 << 4;
    pub const FIELD_OFF: u8 = 1 << 3;
    pub const COLLISION_DURING_CA: u8 = 1 << 2;
    pub const MIN_GUARD_TIME: u8 = 1 << 1;
    pub const BIT_RATE_RECOGNIZED: u8 = 1 << 0;
}

/// Error and wake-up interrupt register bits (Table 64).
pub mod irq_error_wup {
    pub const CRC_ERROR: u8 = 1 << 7;
    pub const PARITY_ERROR: u8 = 1 << 6;
    pub const SOFT_FRAMING_ERROR: u8 = 1 << 5;
    pub const HARD_FRAMING_ERROR: u8 = 1 << 4;
    pub const WUP_TIMER: u8 = 1 << 3;
    pub const WUP_AMPLITUDE: u8 = 1 << 2;
    pub const WUP_PHASE: u8 = 1 << 1;
    pub const WUP_CAPACITANCE: u8 = 1 << 0;
}

/// FIFO status register 2 bits (Table 67; bits 7:6 are the byte
/// count MSBs).
pub mod fifo_status2 {
    pub const UNDERFLOW: u8 = 1 << 5;
    pub const OVERFLOW: u8 = 1 << 4;
}

/// Auxiliary display register bits (Table 98).
pub mod aux_display {
    pub const A_CHA: u8 = 1 << 7;
    pub const EFD_O: u8 = 1 << 6;
    pub const TX_ON: u8 = 1 << 5;
    /// Crystal oscillator running and stable.
    pub const OSC_OK: u8 = 1 << 4;
    pub const RX_ON: u8 = 1 << 3;
    pub const RX_ACT: u8 = 1 << 2;
    pub const EN_PEER: u8 = 1 << 1;
    pub const EN_AC: u8 = 1 << 0;
}

/// Regulator control register bits (Table 90).
pub mod regulator_control {
    /// 0: voltages from Adjust Regulators; 1: from rege_<3:0>.
    pub const REG_S: u8 = 1 << 7;
}

/// IC type code in the identity register's upper five bits
/// (Table 117).
pub const IC_TYPE_ST25R3916: u8 = 0b00101;

/// FIFO depth in bytes.
pub const FIFO_SIZE: usize = 512;
