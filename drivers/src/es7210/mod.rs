//! ES7210 4-channel microphone ADC driver.
//!
//! Communicates over I2C (address 0x40, A0=A1=0).
//! The ESP32-S3 is the I2S master; this chip is I2S slave.
//! MCLK is supplied externally.
//!
//! On this board only MIC1 and MIC2 are populated.
//! Configured for: 16 kHz, 16-bit, standard I2S (Philips), slave mode.
//!
//! Init sequence follows Espressif's esp-bsp `es7210` component
//! (`es7210_config_codec` + its clock coefficient table), except that
//! MIC3/4 stay powered down here since they aren't populated.

use embedded_hal::i2c::I2c;

pub const ADDR: u8 = 0x40;

// Register map
const REG_RESET:        u8 = 0x00;
const REG_MAINCLK:      u8 = 0x02;
const REG_LRCK_DIVH:    u8 = 0x04;
const REG_LRCK_DIVL:    u8 = 0x05;
const REG_DLL_PWR:      u8 = 0x06;
const REG_OSR:          u8 = 0x07;
const REG_TIME_CTRL0:   u8 = 0x09;
const REG_TIME_CTRL1:   u8 = 0x0A;
const REG_SDP_IF1:      u8 = 0x11;
const REG_SDP_IF2:      u8 = 0x12;
const REG_ADC_VOL1:     u8 = 0x1B;
const REG_ADC_VOL2:     u8 = 0x1C;
const REG_ADC_VOL3:     u8 = 0x1D;
const REG_ADC_VOL4:     u8 = 0x1E;
const REG_ADC34_HPF2:   u8 = 0x20;
const REG_ADC34_HPF1:   u8 = 0x21;
const REG_ADC12_HPF1:   u8 = 0x22;
const REG_ADC12_HPF2:   u8 = 0x23;
const REG_ANALOG:       u8 = 0x40;
const REG_MIC12_BIAS:   u8 = 0x41;
const REG_MIC34_BIAS:   u8 = 0x42;
const REG_MIC1_GAIN:    u8 = 0x43;
const REG_MIC2_GAIN:    u8 = 0x44;
const REG_MIC3_GAIN:    u8 = 0x45;
const REG_MIC4_GAIN:    u8 = 0x46;
const REG_MIC1_POWER:   u8 = 0x47;
const REG_MIC2_POWER:   u8 = 0x48;
const REG_MIC3_POWER:   u8 = 0x49;
const REG_MIC4_POWER:   u8 = 0x4A;
const REG_MIC12_POWER:  u8 = 0x4B;
const REG_MIC34_POWER:  u8 = 0x4C;

/// Microphone gain (3 dB per step).
#[derive(Clone, Copy)]
pub enum MicGain {
    Db0  = 0x00,
    Db3  = 0x01,
    Db6  = 0x02,
    Db9  = 0x03,
    Db12 = 0x04,
    Db15 = 0x05,
    Db18 = 0x06,
    Db21 = 0x07,
    Db24 = 0x08,
    Db27 = 0x09,
    Db30 = 0x0A,
    Db33 = 0x0B,
}

pub struct Es7210;

impl Es7210 {
    pub fn new() -> Self {
        Self
    }

    /// Initialise the ADC (step 1: reset).
    ///
    /// Call once after power-up with MCLK already running.
    /// Assumes MCLK = 256 * 16 000 Hz = 4.096 MHz from the ESP32 I2S peripheral.
    ///
    /// The caller must provide a ~10ms delay after this call, then call
    /// `init_after_delay()`, another ~10ms delay, then `finalize()`.
    pub fn init<I: I2c>(&self, i2c: &mut I) -> Result<(), I::Error> {
        // Software reset
        self.write(i2c, REG_RESET, 0xFF)?;
        Ok(())
    }

    /// Continue init after the post-reset delay (step 2: configure).
    pub fn init_after_delay<I: I2c>(&self, i2c: &mut I) -> Result<(), I::Error> {
        self.write(i2c, REG_RESET, 0x32)?;

        // Power-up timing
        self.write(i2c, REG_TIME_CTRL0, 0x30)?;
        self.write(i2c, REG_TIME_CTRL1, 0x30)?;

        // High-pass filter for all channels
        self.write(i2c, REG_ADC12_HPF2, 0x2A)?;
        self.write(i2c, REG_ADC12_HPF1, 0x0A)?;
        self.write(i2c, REG_ADC34_HPF1, 0x2A)?;
        self.write(i2c, REG_ADC34_HPF2, 0x0A)?;

        // I2S format: standard I2S, 16-bit
        self.write(i2c, REG_SDP_IF1, 0x60)?;
        // TDM disabled (stereo I2S)
        self.write(i2c, REG_SDP_IF2, 0x00)?;

        // Analog power on, VMID
        self.write(i2c, REG_ANALOG, 0xC3)?;

        // MIC1/2 bias = 2.87V; MIC3/4 bias disabled (not populated)
        self.write(i2c, REG_MIC12_BIAS, 0x70)?;
        self.write(i2c, REG_MIC34_BIAS, 0x00)?;

        // MIC1/2 gain = 30 dB; MIC3/4 off
        self.write(i2c, REG_MIC1_GAIN, 0x1A)?;
        self.write(i2c, REG_MIC2_GAIN, 0x1A)?;
        self.write(i2c, REG_MIC3_GAIN, 0x00)?;
        self.write(i2c, REG_MIC4_GAIN, 0x00)?;

        // Per-channel analog power: MIC1/2 on, MIC3/4 powered down
        self.write(i2c, REG_MIC1_POWER, 0x08)?;
        self.write(i2c, REG_MIC2_POWER, 0x08)?;
        self.write(i2c, REG_MIC3_POWER, 0xFF)?;
        self.write(i2c, REG_MIC4_POWER, 0xFF)?;

        // OSR = 32
        self.write(i2c, REG_OSR, 0x20)?;

        // Clock config for MCLK=4.096 MHz, Fs=16 kHz, per the esp-bsp
        // es7210 coefficient table row {4096000, 16000}:
        // adc_div=1, doubler=1, dll_bypass=1 -> 0x01 | (1<<6) | (1<<7).
        // The doubler is essential: the delta-sigma modulator needs
        // MCLK*2/1 = 8.192 MHz; dividing instead of doubling collapses
        // the oversampling ratio and with it sensitivity/SNR.
        self.write(i2c, REG_MAINCLK, 0xC1)?;

        // LRCK divider = MCLK/Fs = 256 (0x0100)
        self.write(i2c, REG_LRCK_DIVH, 0x01)?;
        self.write(i2c, REG_LRCK_DIVL, 0x00)?;

        // DLL power control
        self.write(i2c, REG_DLL_PWR, 0x04)?;

        // ADC/PGA power on for MIC1+MIC2; MIC3+MIC4 down
        self.write(i2c, REG_MIC12_POWER, 0x0F)?;
        self.write(i2c, REG_MIC34_POWER, 0xFF)?;

        // ADC digital volume: 0xBF = 0 dB
        self.write(i2c, REG_ADC_VOL1, 0xBF)?;
        self.write(i2c, REG_ADC_VOL2, 0xBF)?;
        self.write(i2c, REG_ADC_VOL3, 0xBF)?;
        self.write(i2c, REG_ADC_VOL4, 0xBF)?;

        // Enable device (first step)
        self.write(i2c, REG_RESET, 0x71)?;
        // Caller must provide ~10ms delay, then call finalize()
        Ok(())
    }

    /// Final enable step after the second delay (step 3).
    pub fn finalize<I: I2c>(&self, i2c: &mut I) -> Result<(), I::Error> {
        self.write(i2c, REG_RESET, 0x41)
    }

    /// Power the ADC down to its datasheet power-down state
    /// (datasheet "Power Down Mode": ~10 uA). REG4B/4C set every
    /// per-channel block to its documented power-down bit (MICBIAS,
    /// mic reference, PGA, ADC, state machines), bias levels zero,
    /// and REG40 sets PDN_ANA. The digital core loses MCLK when the
    /// session's I2S drops, so no clock gating is needed on top.
    ///
    /// The chip is external: SoC light sleep cannot gate it, and
    /// left configured it holds mic bias + four ADC channels live
    /// from the always-on rail indefinitely. Recovery is the full
    /// three-step init - which is how every audio session already
    /// starts.
    pub fn power_down<I: I2c>(&self, i2c: &mut I) -> Result<(), I::Error> {
        self.write(i2c, REG_MIC12_POWER, 0xFF)?; // bias/ref/PGA/ADC 1+2 down
        self.write(i2c, REG_MIC34_POWER, 0xFF)?; // 3+4 down (defensive)
        self.write(i2c, REG_MIC12_BIAS, 0x00)?;  // bias level off
        self.write(i2c, REG_MIC34_BIAS, 0x00)?;
        self.write(i2c, REG_ANALOG, 0x80)        // PDN_ANA = 1
    }

    /// Set microphone gain for MIC1 and MIC2.
    pub fn set_gain<I: I2c>(&self, i2c: &mut I, gain: MicGain) -> Result<(), I::Error> {
        let val = 0x10 | (gain as u8); // bit4 = enable
        self.write(i2c, REG_MIC1_GAIN, val)?;
        self.write(i2c, REG_MIC2_GAIN, val)
    }

    /// Read a single register.
    pub fn read_reg<I: I2c>(&self, i2c: &mut I, reg: u8) -> Result<u8, I::Error> {
        let mut buf = [0u8; 1];
        i2c.write_read(ADDR, &[reg], &mut buf)?;
        Ok(buf[0])
    }

    fn write<I: I2c>(&self, i2c: &mut I, reg: u8, val: u8) -> Result<(), I::Error> {
        i2c.write(ADDR, &[reg, val])
    }
}
