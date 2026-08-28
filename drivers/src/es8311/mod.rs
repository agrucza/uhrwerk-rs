//! ES8311 mono codec / DAC driver.
//!
//! Communicates over I2C (address 0x18, CE pin pulled HIGH).
//! The ESP32-S3 is the I2S master; this chip is I2S slave.
//! MCLK is supplied externally on the MCLK pin.
//!
//! Configured for: 16 kHz, 16-bit, standard I2S (Philips), slave mode.

use embedded_hal::i2c::I2c;

pub const ADDR: u8 = 0x18;

// Register map (from ES8311 datasheet rev 10.0)
const REG_RESET:    u8 = 0x00;
const REG_CLK1:     u8 = 0x01; // clock manager
const REG_CLK2:     u8 = 0x02; // pre-divider / pre-multiplier
const REG_CLK3:     u8 = 0x03; // ADC fs mode / osr
const REG_CLK4:     u8 = 0x04; // DAC osr
const REG_CLK5:     u8 = 0x05; // ADC/DAC clock dividers
const REG_CLK6:     u8 = 0x06; // BCLK divider
const REG_CLK7:     u8 = 0x07; // LRCK divider high
const REG_CLK8:     u8 = 0x08; // LRCK divider low
const REG_SDPIN:    u8 = 0x09; // serial data port in (DAC)
const REG_SDPOUT:   u8 = 0x0A; // serial data port out (ADC)
const REG_SYS0D:    u8 = 0x0D; // analog power / bias / VMID
const REG_SYS0E:    u8 = 0x0E; // PGA / ADC modulator power
const REG_SYS12:    u8 = 0x12; // DAC power
const REG_SYS13:    u8 = 0x13; // HP driver select
const REG_SYS14:    u8 = 0x14; // mic input / PGA gain
const REG_ADC15:    u8 = 0x15; // ADC ramp rate / auto-mute
const REG_ADC17:    u8 = 0x17; // ADC volume
const REG_ADC1C:    u8 = 0x1C; // ADC EQ bypass / HPF
const REG_DAC_VOL:  u8 = 0x32; // DAC volume
const REG_DAC_EQ:   u8 = 0x37; // DAC EQ bypass / ramp rate
const REG_GP45:     u8 = 0x45; // GP control

pub struct Es8311;

impl Es8311 {
    pub fn new() -> Self {
        Self
    }

    /// Initialise the codec (step 1: assert reset).
    ///
    /// The caller MUST wait ~20 ms after this call - the reference
    /// driver's reset hold time - then call [`Self::init_after_reset`]
    /// to release reset and configure. Releasing reset in the next
    /// I2C transaction (no hold) leaves the chip in a random
    /// half-reset state on some re-inits: misclocked DAC ("warbling"
    /// playback), muted output, or a hot/garbage ADC path.
    pub fn init<I: I2c>(&self, i2c: &mut I) -> Result<(), I::Error> {
        self.write(i2c, REG_RESET, 0x1F) // assert all reset bits
    }

    /// Release reset and configure (step 2, after the ~20 ms hold).
    ///
    /// Assumes MCLK = 256 * 16 000 Hz = 4.096 MHz from the ESP32 I2S peripheral.
    /// Init sequence follows the Espressif reference driver (es8311.c) and datasheet.
    pub fn init_after_reset<I: I2c>(&self, i2c: &mut I) -> Result<(), I::Error> {
        self.write(i2c, REG_RESET, 0x00)?; // release resets
        // Power-on the chip state machine (CSM_ON=1), slave mode (MSC=0)
        self.write(i2c, REG_RESET, 0x80)?;

        // --- 2. Clock configuration ---
        // Enable all clocks, MCLK from MCLK pin (not BCLK)
        self.write(i2c, REG_CLK1, 0x3F)?;

        // Clock coefficients for MCLK=4.096 MHz, Fs=16 kHz.
        // From the Espressif coeff table {4096000, 16000}:
        //   pre_div=1, pre_multi=0(1x), adc_div=1, dac_div=1,
        //   fs_mode=0(single speed), lrck_h=0, lrck_l=0xFF,
        //   bclk_div=4, adc_osr=0x10, dac_osr=0x10
        //
        // REG02 = ((pre_div-1)<<5) | (pre_multi<<3) = 0x00
        // REG03 = (fs_mode<<6) | adc_osr = 0x10
        // REG04 = dac_osr = 0x10
        // REG05 = ((adc_div-1)<<4) | (dac_div-1) = 0x00
        // REG06 = bclk_div-1 = 0x03  (for bclk_div < 19)
        // REG07 = lrck_h = 0x00
        // REG08 = lrck_l = 0xFF  -> LRCK div = 256 -> 4096000/256 = 16000 Hz
        self.write(i2c, REG_CLK2, 0x00)?;
        self.write(i2c, REG_CLK3, 0x10)?;
        self.write(i2c, REG_CLK4, 0x10)?;
        self.write(i2c, REG_CLK5, 0x00)?;
        self.write(i2c, REG_CLK6, 0x03)?;
        self.write(i2c, REG_CLK7, 0x00)?;
        self.write(i2c, REG_CLK8, 0xFF)?;

        // --- 3. I2S format: slave, standard I2S (Philips), 16-bit ---
        // SDP_IN_WL[4:2]=3(16-bit), SDP_IN_FMT[1:0]=0(I2S) -> 0x0C
        self.write(i2c, REG_SDPIN,  0x0C)?;
        self.write(i2c, REG_SDPOUT, 0x0C)?;

        // --- 4. Power up analog section ---
        // REG0D: enable analog circuits, bias, ADC/DAC voltage references, VMID
        //   PDN_ANA=0, PDN_IBIASGEN=0, PDN_ADCBIASGEN=0, PDN_ADCVREFGEN=0,
        //   PDN_DACVREFGEN=0, PDN_VREF=0, VMIDSEL=1 (normal speed charge)
        self.write(i2c, REG_SYS0D, 0x01)?;
        // REG0E: enable analog PGA and ADC modulator
        //   PDN_PGA=0, PDN_MOD=0
        self.write(i2c, REG_SYS0E, 0x02)?;
        // REG12: power up DAC
        //   PDN_DAC=0 (enable)
        self.write(i2c, REG_SYS12, 0x00)?;
        // REG13: enable output to HP driver
        //   HPSW=1 (bit 4)
        self.write(i2c, REG_SYS13, 0x10)?;

        // --- 5. Microphone input selection ---
        // REG14: select Mic1p-Mic1n analog input, PGA gain = 30 dB
        //   LINSEL=1 (bit 4), PGAGAIN=0x0A (30 dB)
        self.write(i2c, REG_SYS14, 0x1A)?;

        // --- 6. Filter configuration ---
        // ADC EQ bypass, dynamic HPF, cancel DC offset in digital domain
        self.write(i2c, REG_ADC1C, 0x6A)?;
        // DAC EQ bypass
        self.write(i2c, REG_DAC_EQ, 0x08)?;

        // --- 7. Volume: 0 dB ---
        // REG32: 0xBF = 0 dB, 0x00 = -95.5 dB (muted), 0xFF = +32 dB
        self.write(i2c, REG_DAC_VOL, 0xBF)?;

        Ok(())
    }

    /// Power the codec down to its suspend state: DAC/ADC muted,
    /// PGA + ADC modulator down (REG0E all PDN bits), DAC down
    /// (REG12), mic input off, and REG0D = 0xFA - per the datasheet
    /// bit map that is PDN_ANA, PDN_IBIASGEN, PDN_ADCBIASGEN,
    /// PDN_ADCVREFGEN, PDN_DACVREFGEN set and the internal reference
    /// disabled. Sequence is the Espressif reference driver's
    /// `es8311_suspend`.
    ///
    /// The chip is external: SoC light sleep cannot gate it, and
    /// left configured it holds its analog blocks biased from the
    /// always-on rail indefinitely. Recovery is a full [`Self::init`]
    /// cycle - which is how every audio session already starts.
    pub fn power_down<I: I2c>(&self, i2c: &mut I) -> Result<(), I::Error> {
        self.write(i2c, REG_DAC_VOL, 0x00)?; // mute DAC (-95.5 dB)
        self.write(i2c, REG_ADC17, 0x00)?;   // mute ADC
        self.write(i2c, REG_SYS0E, 0xFF)?;   // PGA + ADC modulator down
        self.write(i2c, REG_SYS12, 0x02)?;   // DAC down
        self.write(i2c, REG_SYS14, 0x00)?;   // mic input off
        self.write(i2c, REG_SYS0D, 0xFA)?;   // analog + bias + refs down
        self.write(i2c, REG_ADC15, 0x00)?;   // ADC ramp reset
        self.write(i2c, REG_DAC_EQ, 0x08)?;  // DAC ramp default
        self.write(i2c, REG_GP45, 0x01)      // GP low-power state
    }

    /// Set DAC output volume.
    ///
    /// `volume`: 0xBF = 0 dB, 0x00 = -95.5 dB (muted), 0xFF = +32 dB.
    /// Each step is 0.5 dB.
    pub fn set_volume<I: I2c>(&self, i2c: &mut I, volume: u8) -> Result<(), I::Error> {
        self.write(i2c, REG_DAC_VOL, volume)
    }

    /// Read chip ID registers (0xFD, 0xFE, 0xFF) for verification.
    /// Expected: [0x83, 0x11, 0x00].
    pub fn read_ids<I: I2c>(&self, i2c: &mut I) -> Result<[u8; 3], I::Error> {
        let mut buf = [0u8; 3];
        for (i, reg) in [0xFDu8, 0xFE, 0xFF].iter().enumerate() {
            i2c.write_read(ADDR, &[*reg], &mut buf[i..i+1])?;
        }
        Ok(buf)
    }

    /// Read a single register.
    pub fn read_reg<I: I2c>(&self, i2c: &mut I, reg: u8) -> Result<u8, I::Error> {
        let mut buf = [0u8; 1];
        i2c.write_read(ADDR, &[reg], &mut buf)?;
        Ok(buf[0])
    }

    /// Dump key registers for debugging. Returns an array of (reg_addr, value) pairs.
    pub fn dump_regs<I: I2c>(&self, i2c: &mut I) -> Result<[(u8, u8); 16], I::Error> {
        let regs = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0A, 0x0D, 0x0E, 0x12, 0x13, 0x32,
        ];
        let mut result = [(0u8, 0u8); 16];
        for (i, &reg) in regs.iter().enumerate() {
            result[i] = (reg, self.read_reg(i2c, reg)?);
        }
        Ok(result)
    }

    fn write<I: I2c>(&self, i2c: &mut I, reg: u8, val: u8) -> Result<(), I::Error> {
        i2c.write(ADDR, &[reg, val])
    }
}
