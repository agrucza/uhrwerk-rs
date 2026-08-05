//! SX1261/2 opcodes, registers, and bit definitions.
//!
//! Every value in this file is taken from the Semtech SX1261/2 data
//! sheet rev 2.2 (DS.SX1261-2.W.APP, Dec 2024); table numbers are
//! cited inline so a datasheet check never starts from scratch.

/// Command opcodes (Tables 11-1 .. 11-5).
pub mod opcode {
    // Operational modes (Table 11-1)
    pub const SET_SLEEP: u8 = 0x84;
    pub const SET_STANDBY: u8 = 0x80;
    pub const SET_FS: u8 = 0xC1;
    pub const SET_TX: u8 = 0x83;
    pub const SET_RX: u8 = 0x82;
    pub const STOP_TIMER_ON_PREAMBLE: u8 = 0x9F;
    pub const SET_RX_DUTY_CYCLE: u8 = 0x94;
    pub const SET_CAD: u8 = 0xC5;
    pub const SET_TX_CONTINUOUS_WAVE: u8 = 0xD1;
    pub const SET_TX_INFINITE_PREAMBLE: u8 = 0xD2;
    pub const SET_REGULATOR_MODE: u8 = 0x96;
    pub const CALIBRATE: u8 = 0x89;
    pub const CALIBRATE_IMAGE: u8 = 0x98;
    pub const SET_PA_CONFIG: u8 = 0x95;
    pub const SET_RX_TX_FALLBACK_MODE: u8 = 0x93;
    // Register and buffer access (Table 11-2)
    pub const WRITE_REGISTER: u8 = 0x0D;
    pub const READ_REGISTER: u8 = 0x1D;
    pub const WRITE_BUFFER: u8 = 0x0E;
    pub const READ_BUFFER: u8 = 0x1E;
    // DIO and IRQ control (Table 11-3)
    pub const SET_DIO_IRQ_PARAMS: u8 = 0x08;
    pub const GET_IRQ_STATUS: u8 = 0x12;
    pub const CLEAR_IRQ_STATUS: u8 = 0x02;
    pub const SET_DIO2_AS_RF_SWITCH_CTRL: u8 = 0x9D;
    pub const SET_DIO3_AS_TCXO_CTRL: u8 = 0x97;
    // RF, modulation and packet (Table 11-4)
    pub const SET_RF_FREQUENCY: u8 = 0x86;
    pub const SET_PACKET_TYPE: u8 = 0x8A;
    pub const GET_PACKET_TYPE: u8 = 0x11;
    pub const SET_TX_PARAMS: u8 = 0x8E;
    pub const SET_MODULATION_PARAMS: u8 = 0x8B;
    pub const SET_PACKET_PARAMS: u8 = 0x8C;
    pub const SET_CAD_PARAMS: u8 = 0x88;
    pub const SET_BUFFER_BASE_ADDRESS: u8 = 0x8F;
    pub const SET_LORA_SYMB_NUM_TIMEOUT: u8 = 0xA0;
    // Status (Table 11-5)
    pub const GET_STATUS: u8 = 0xC0;
    pub const GET_RSSI_INST: u8 = 0x15;
    pub const GET_RX_BUFFER_STATUS: u8 = 0x13;
    pub const GET_PACKET_STATUS: u8 = 0x14;
    pub const GET_DEVICE_ERRORS: u8 = 0x17;
    pub const CLEAR_DEVICE_ERRORS: u8 = 0x07;
    pub const GET_STATS: u8 = 0x10;
    pub const RESET_STATS: u8 = 0x00;
}

/// Register addresses (Table 12-1, plus the section-8.6 DIO control
/// block and the section-9.6 RX-gain retention trio).
pub mod reg {
    /// FSK sync word bytes 0..=7 (Table 13-55).
    pub const FSK_SYNC_WORD_0: u16 = 0x06C0;
    /// FSK node address (Table 13-57).
    pub const FSK_NODE_ADDRESS: u16 = 0x06CD;
    /// FSK broadcast address (Table 13-58).
    pub const FSK_BROADCAST_ADDRESS: u16 = 0x06CE;
    /// FSK whitening initial value MSB/LSB (Table 13-65). Only the
    /// LSB may be user-modified - the 7 MSB of the MSB register are
    /// fixed.
    pub const FSK_WHITENING_INITIAL_MSB: u16 = 0x06B8;
    pub const FSK_WHITENING_INITIAL_LSB: u16 = 0x06B9;
    /// FSK CRC initial value / polynomial (Tables 13-62/13-63).
    pub const FSK_CRC_INITIAL_MSB: u16 = 0x06BC;
    pub const FSK_CRC_INITIAL_LSB: u16 = 0x06BD;
    pub const FSK_CRC_POLYNOMIAL_MSB: u16 = 0x06BE;
    pub const FSK_CRC_POLYNOMIAL_LSB: u16 = 0x06BF;
    /// IQ polarity setup; bit 2 is the inverted-IQ errata knob
    /// (section 15.4).
    pub const IQ_POLARITY: u16 = 0x0736;
    /// LoRa sync word MSB/LSB. 0x3444 = public network, 0x1424 =
    /// private network (Table 12-1).
    pub const LORA_SYNC_WORD_MSB: u16 = 0x0740;
    pub const LORA_SYNC_WORD_LSB: u16 = 0x0741;
    /// Coding rate extracted from the last received explicit header
    /// (bits 6:4).
    pub const LORA_CODING_RATE_RX: u16 = 0x0749;
    /// CRC-present flag extracted from the last received explicit
    /// header (bit 4).
    pub const LORA_CRC_CONFIG_RX: u16 = 0x076B;
    /// 4 bytes of hardware entropy (Table 12-1; sample while RX).
    pub const RANDOM_NUMBER_0: u16 = 0x0819;
    /// TX modulation quality; bit 2 is the BW500 errata knob
    /// (section 15.1).
    pub const TX_MODULATION: u16 = 0x0889;
    /// RX gain: 0x94 power-saving (reset default), 0x96 boosted
    /// (section 9.6).
    pub const RX_GAIN: u16 = 0x08AC;
    /// RX-gain retention setup trio - programs the retention memory
    /// so the boosted gain survives warm-start sleep (section 9.6:
    /// 0x029F=0x01, 0x02A0=0x08, 0x02A1=0xAC).
    pub const RETENTION_LIST_0: u16 = 0x029F;
    pub const RETENTION_LIST_1: u16 = 0x02A0;
    pub const RETENTION_LIST_2: u16 = 0x02A1;
    /// PA clamp threshold; bits 4:1 are the antenna-mismatch errata
    /// knob (section 15.2, SX1262 only).
    pub const TX_CLAMP_CONFIG: u16 = 0x08D8;
    /// Over-current protection in 2.5 mA steps. Reset default 0x38
    /// (140 mA) on the SX1262, 0x18 (60 mA) on the SX1261.
    pub const OCP_CONFIGURATION: u16 = 0x08E7;
    /// RTC control - the implicit-header timeout errata stops the
    /// counter here (section 15.3).
    pub const RTC_CONTROL: u16 = 0x0902;
    /// XTAL trim capacitors; only writable in STDBY_XOSC.
    pub const XTA_TRIM: u16 = 0x0911;
    pub const XTB_TRIM: u16 = 0x0912;
    /// Non-standard DIO3 control block (section 8.6).
    pub const DIO_OUTPUT_ENABLE: u16 = 0x0580;
    pub const DIO_INPUT_ENABLE: u16 = 0x0583;
    pub const DIO_PULL_UP: u16 = 0x0584;
    pub const DIO_PULL_DOWN: u16 = 0x0585;
    pub const DIO3_OUTPUT_VOLTAGE: u16 = 0x0920;
    /// Event mask - the implicit-header timeout errata clears the
    /// latched event here (section 15.3).
    pub const EVENT_MASK: u16 = 0x0944;
}

/// IRQ register bits (Table 13-29).
pub mod irq {
    pub const TX_DONE: u16 = 1 << 0;
    pub const RX_DONE: u16 = 1 << 1;
    pub const PREAMBLE_DETECTED: u16 = 1 << 2;
    /// FSK only.
    pub const SYNC_WORD_VALID: u16 = 1 << 3;
    /// LoRa only.
    pub const HEADER_VALID: u16 = 1 << 4;
    /// LoRa only.
    pub const HEADER_ERR: u16 = 1 << 5;
    pub const CRC_ERR: u16 = 1 << 6;
    /// LoRa only.
    pub const CAD_DONE: u16 = 1 << 7;
    /// LoRa only.
    pub const CAD_DETECTED: u16 = 1 << 8;
    pub const TIMEOUT: u16 = 1 << 9;
    /// LR-FHSS only.
    pub const LR_FHSS_HOP: u16 = 1 << 14;
    pub const ALL: u16 = 0x43FF;
}

/// GetDeviceErrors bits (Table 13-86).
pub mod op_error {
    pub const RC64K_CALIB_ERR: u16 = 1 << 0;
    pub const RC13M_CALIB_ERR: u16 = 1 << 1;
    pub const PLL_CALIB_ERR: u16 = 1 << 2;
    pub const ADC_CALIB_ERR: u16 = 1 << 3;
    pub const IMG_CALIB_ERR: u16 = 1 << 4;
    /// Expected (and harmless) at POR / cold-start wake on TCXO
    /// designs - the chip tried its crystal path before DIO3 was
    /// configured. Clear with ClearDeviceErrors (section 13.3.6).
    pub const XOSC_START_ERR: u16 = 1 << 5;
    pub const PLL_LOCK_ERR: u16 = 1 << 6;
    pub const PA_RAMP_ERR: u16 = 1 << 8;
}

/// Calibrate() block-selection bits (Table 13-18). OR together;
/// `ALL` is every block (~3.5 ms, STDBY_RC only).
pub mod calibrate {
    pub const RC64K: u8 = 1 << 0;
    pub const RC13M: u8 = 1 << 1;
    pub const PLL: u8 = 1 << 2;
    pub const ADC_PULSE: u8 = 1 << 3;
    pub const ADC_BULK_N: u8 = 1 << 4;
    pub const ADC_BULK_P: u8 = 1 << 5;
    pub const IMAGE: u8 = 1 << 6;
    pub const ALL: u8 = 0x7F;
}

/// CalibrateImage() ISM band presets (Table 9-2) as (freq1, freq2).
pub mod image_band {
    pub const MHZ_430_440: (u8, u8) = (0x6B, 0x6F);
    pub const MHZ_470_510: (u8, u8) = (0x75, 0x81);
    pub const MHZ_779_787: (u8, u8) = (0xC1, 0xC5);
    pub const MHZ_863_870: (u8, u8) = (0xD7, 0xDB);
    pub const MHZ_902_928: (u8, u8) = (0xE1, 0xE9);
}

/// LoRa sync word values for the 0x0740/0x0741 register pair.
pub mod lora_sync_word {
    pub const PUBLIC: u16 = 0x3444;
    pub const PRIVATE: u16 = 0x1424;
}

/// Crystal/TCXO reference frequency the frequency and timing
/// formulas are built on.
pub const F_XTAL_HZ: u32 = 32_000_000;

/// One step of every 24-bit chip timer (TX/RX timeout, TCXO delay,
/// duty-cycle periods): 15.625 us, i.e. 64 steps per millisecond.
pub const TIMER_STEPS_PER_MS: u32 = 64;
