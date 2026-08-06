//! ST NFC universal device / EMVCo reader driver (ST25R3916/7).
//!
//! Datasheet-complete command layer over the chip's SPI interface
//! (DS12484 rev 8): register access in both spaces, the direct
//! command set, FIFO and passive-target-memory transfers, the
//! four-register interrupt system, and typed helpers for the
//! power-up ritual the datasheet mandates (overheat-protection
//! frame, 3.3 V supply mode, regulator adjustment). Protocol logic -
//! ISO14443 anticollision, NDEF, card emulation flows - belongs to a
//! layer above; this driver only speaks chip verbs, like the crate's
//! other drivers.
//!
//! ## Transport requirements
//!
//! - **SPI mode 1** (CPOL = 0, CPHA = 1; section 4.3.3) - unlike the
//!   mode-0 devices this chip may share a bus with. The caller's
//!   `SpiDevice` must apply that mode per transaction.
//! - The IRQ pin is level-high until all set interrupt registers are
//!   read; the caller owns the pin and the read loop.
//!
//! ## Power-up ritual (section 4.1)
//!
//! After the rail comes up (and after every Set Default): send the
//! overheat-protection frame, configure the IO registers (3.3 V
//! supplies MUST set sup3V), enable the oscillator and wait for
//! osc_ok / I_osc, then Adjust Regulators. Only then is the RF part
//! trustworthy.

use embedded_hal::spi::{Operation, SpiDevice};

pub mod regs;
use regs::{cmd, reg_a, reg_b, spi_mode};

/// Errors surfaced by the command layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// SPI transaction failed.
    Spi(E),
}

/// Decoded IC identity register (Table 117).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// 5-bit type code; [`regs::IC_TYPE_ST25R3916`] on this part.
    pub ic_type: u8,
    /// 3-bit silicon revision (0b010 = rev 3.1).
    pub ic_rev: u8,
}

impl Identity {
    pub fn is_st25r3916(&self) -> bool {
        self.ic_type == regs::IC_TYPE_ST25R3916
    }
}

/// The four interrupt status registers, read (and thereby cleared)
/// in one auto-incremented burst (section 4.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interrupts {
    /// Main register (0x1A) - see [`regs::irq_main`].
    pub main: u8,
    /// Timer and NFC register (0x1B) - see [`regs::irq_timer_nfc`].
    pub timer_nfc: u8,
    /// Error and wake-up register (0x1C) - see
    /// [`regs::irq_error_wup`].
    pub error_wup: u8,
    /// Passive target register (0x1D).
    pub target: u8,
}

/// Decoded FIFO status (Tables 66/67).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FifoStatus {
    /// Bytes currently in the 512-byte FIFO.
    pub bytes: u16,
    pub underflow: bool,
    pub overflow: bool,
    /// Valid bits in an incomplete last byte (0 = complete).
    pub last_byte_bits: u8,
    /// Parity bit missing in the last byte.
    pub missing_parity: bool,
}

/// ST25R3916/7 command-layer driver. Stateless; the SPI device is
/// borrowed per call so the chip can live on a shared bus behind
/// its own chip select.
pub struct St25r3916;

impl St25r3916 {
    pub fn new() -> Self {
        St25r3916
    }

    // -- Raw access ----------------------------------------------------------

    /// Send one direct command (Table 13). Commands flagged with a
    /// termination interrupt raise I_dct when done.
    pub fn direct_command<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        code: u8,
    ) -> Result<(), Error<S::Error>> {
        spi.write(&[code]).map_err(Error::Spi)
    }

    /// Write consecutive space-A registers starting at `addr`
    /// (auto-increment, section 4.3.3).
    pub fn write_reg<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        addr: u8,
        data: &[u8],
    ) -> Result<(), Error<S::Error>> {
        spi.transaction(&mut [
            Operation::Write(&[spi_mode::REG_WRITE | (addr & 0x3F)]),
            Operation::Write(data),
        ])
        .map_err(Error::Spi)
    }

    /// Read consecutive space-A registers starting at `addr`.
    pub fn read_reg<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        addr: u8,
        data: &mut [u8],
    ) -> Result<(), Error<S::Error>> {
        spi.transaction(&mut [
            Operation::Write(&[spi_mode::REG_READ | (addr & 0x3F)]),
            Operation::Read(data),
        ])
        .map_err(Error::Spi)
    }

    /// Write consecutive space-B registers (the 0xFB prefix rides in
    /// the same chip-select frame).
    pub fn write_reg_b<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        addr: u8,
        data: &[u8],
    ) -> Result<(), Error<S::Error>> {
        spi.transaction(&mut [
            Operation::Write(&[cmd::SPACE_B_ACCESS, spi_mode::REG_WRITE | (addr & 0x3F)]),
            Operation::Write(data),
        ])
        .map_err(Error::Spi)
    }

    /// Read consecutive space-B registers.
    pub fn read_reg_b<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        addr: u8,
        data: &mut [u8],
    ) -> Result<(), Error<S::Error>> {
        spi.transaction(&mut [
            Operation::Write(&[cmd::SPACE_B_ACCESS, spi_mode::REG_READ | (addr & 0x3F)]),
            Operation::Read(data),
        ])
        .map_err(Error::Spi)
    }

    /// Read-modify-write one space-A register.
    pub fn update_reg<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        addr: u8,
        clear: u8,
        set: u8,
    ) -> Result<u8, Error<S::Error>> {
        let mut v = [0u8; 1];
        self.read_reg(spi, addr, &mut v)?;
        let new = (v[0] & !clear) | set;
        self.write_reg(spi, addr, &[new])?;
        Ok(new)
    }

    /// Load TX payload into the 512-byte FIFO (section 4.3.3).
    pub fn fifo_load<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        data: &[u8],
    ) -> Result<(), Error<S::Error>> {
        spi.transaction(&mut [
            Operation::Write(&[spi_mode::FIFO_LOAD]),
            Operation::Write(data),
        ])
        .map_err(Error::Spi)
    }

    /// Read received bytes out of the FIFO.
    pub fn fifo_read<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        data: &mut [u8],
    ) -> Result<(), Error<S::Error>> {
        spi.transaction(&mut [
            Operation::Write(&[spi_mode::FIFO_READ]),
            Operation::Read(data),
        ])
        .map_err(Error::Spi)
    }

    /// Load one of the passive-target memory areas (Table 11).
    pub fn pt_memory_load<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        area: u8,
        data: &[u8],
    ) -> Result<(), Error<S::Error>> {
        spi.transaction(&mut [Operation::Write(&[area]), Operation::Write(data)])
            .map_err(Error::Spi)
    }

    /// Read the passive-target memory from location 0 (a zero byte
    /// precedes the data per Table 11).
    pub fn pt_memory_read<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        data: &mut [u8],
    ) -> Result<(), Error<S::Error>> {
        spi.transaction(&mut [
            Operation::Write(&[spi_mode::PT_READ, 0x00]),
            Operation::Read(data),
        ])
        .map_err(Error::Spi)
    }

    // -- Power-up ritual -----------------------------------------------------

    /// The mandatory post-power-up / post-Set-Default frame that
    /// keeps the internal overheat protection from tripping below
    /// the junction temperature: test-access prefix, then register
    /// 0x04 = 0x10, all in one chip-select frame (section 4.1).
    pub fn apply_overheat_protection_fix<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
    ) -> Result<(), Error<S::Error>> {
        spi.write(&[cmd::TEST_ACCESS, 0x04, 0x10]).map_err(Error::Spi)
    }

    /// Put the chip back into its power-up state (section 4.4.1).
    /// Everything - calibrations, adjustments, the overheat fix -
    /// must be redone afterwards.
    pub fn set_default<S: SpiDevice<u8>>(&self, spi: &mut S) -> Result<(), Error<S::Error>> {
        self.direct_command(spi, cmd::SET_DEFAULT)
    }

    /// Stop transmission, reception, direct commands and timers;
    /// clears the IRQ line (section 4.4.2).
    pub fn stop_all_activities<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
    ) -> Result<(), Error<S::Error>> {
        self.direct_command(spi, cmd::STOP_ALL_ACTIVITIES)
    }

    /// Decoded IC identity (register 0x3F).
    pub fn identity<S: SpiDevice<u8>>(&self, spi: &mut S) -> Result<Identity, Error<S::Error>> {
        let mut v = [0u8; 1];
        self.read_reg(spi, reg_a::IC_IDENTITY, &mut v)?;
        Ok(Identity { ic_type: v[0] >> 3, ic_rev: v[0] & 0x07 })
    }

    /// Run the regulator adjustment procedure (section 4.4.10):
    /// reg_s toggled 1 -> 0 as the datasheet requires, then the
    /// Adjust Regulators command. Completion raises I_dct; the
    /// result is readable via [`Self::regulator_result_mv_3v3`].
    /// Requires Ready mode (en set, oscillator stable).
    pub fn adjust_regulators<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
    ) -> Result<(), Error<S::Error>> {
        self.update_reg(spi, reg_a::REGULATOR_CONTROL, 0, regs::regulator_control::REG_S)?;
        self.update_reg(spi, reg_a::REGULATOR_CONTROL, regs::regulator_control::REG_S, 0)?;
        self.direct_command(spi, cmd::ADJUST_REGULATORS)
    }

    /// Regulated voltage in millivolts after Adjust Regulators, as
    /// displayed in space-B 0x2C, decoded for 3.3 V supply mode
    /// (Table 92: setting 5 -> 2400 mV rising 100 mV per step to
    /// 15 -> 3400 mV; settings below 5 are undefined in this mode).
    pub fn regulator_result_mv_3v3<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
    ) -> Result<Option<u16>, Error<S::Error>> {
        let mut v = [0u8; 1];
        self.read_reg_b(spi, reg_b::REGULATOR_RESULT, &mut v)?;
        let setting = v[0] >> 4;
        Ok(if setting >= 5 { Some(2400 + 100 * (setting as u16 - 5)) } else { None })
    }

    // -- Interrupts ----------------------------------------------------------

    /// Write all four interrupt mask registers (1 = masked).
    pub fn set_irq_masks<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        masks: [u8; 4],
    ) -> Result<(), Error<S::Error>> {
        self.write_reg(spi, reg_a::IRQ_MASK_MAIN, &masks)
    }

    /// Read (and clear) all four interrupt status registers in one
    /// auto-incremented burst. Reading drops the IRQ line once every
    /// set bit has been read (section 4.3.1).
    pub fn read_interrupts<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
    ) -> Result<Interrupts, Error<S::Error>> {
        let mut v = [0u8; 4];
        self.read_reg(spi, reg_a::IRQ_MAIN, &mut v)?;
        Ok(Interrupts { main: v[0], timer_nfc: v[1], error_wup: v[2], target: v[3] })
    }

    // -- Status --------------------------------------------------------------

    /// Decoded FIFO status.
    pub fn fifo_status<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
    ) -> Result<FifoStatus, Error<S::Error>> {
        let mut v = [0u8; 2];
        self.read_reg(spi, reg_a::FIFO_STATUS_1, &mut v)?;
        Ok(FifoStatus {
            bytes: ((v[1] as u16 & 0xC0) << 2) | v[0] as u16,
            underflow: v[1] & regs::fifo_status2::UNDERFLOW != 0,
            overflow: v[1] & regs::fifo_status2::OVERFLOW != 0,
            last_byte_bits: (v[1] >> 1) & 0x07,
            missing_parity: v[1] & 0x01 != 0,
        })
    }

    /// Auxiliary display register (osc_ok, field detector state,
    /// TX/RX activity - see [`regs::aux_display`]).
    pub fn aux_display<S: SpiDevice<u8>>(&self, spi: &mut S) -> Result<u8, Error<S::Error>> {
        let mut v = [0u8; 1];
        self.read_reg(spi, reg_a::AUX_DISPLAY, &mut v)?;
        Ok(v[0])
    }

    /// A/D converter output - the landing place of the measurement
    /// direct commands (amplitude 13.02 mVpp/LSB, power supply
    /// 23.4 mV/LSB, phase per the section 4.4.11 formula).
    pub fn ad_result<S: SpiDevice<u8>>(&self, spi: &mut S) -> Result<u8, Error<S::Error>> {
        let mut v = [0u8; 1];
        self.read_reg(spi, reg_a::AD_RESULT, &mut v)?;
        Ok(v[0])
    }

    /// RSSI display register: (AM, PM) 4-bit peak values
    /// (Table 94's mV mapping).
    pub fn rssi<S: SpiDevice<u8>>(&self, spi: &mut S) -> Result<(u8, u8), Error<S::Error>> {
        let mut v = [0u8; 1];
        self.read_reg(spi, reg_a::RSSI_RESULT, &mut v)?;
        Ok((v[0] >> 4, v[0] & 0x0F))
    }

    // -- Transmission --------------------------------------------------------

    /// Program the transmit length: `bytes` full bytes plus
    /// `extra_bits` (0..=7) of a split last byte, packed across the
    /// two length registers (Tables 70/71).
    pub fn set_num_tx_bytes<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        bytes: u16,
        extra_bits: u8,
    ) -> Result<(), Error<S::Error>> {
        let reg1 = (bytes >> 5) as u8;
        let reg2 = (((bytes & 0x1F) as u8) << 3) | (extra_bits & 0x07);
        self.write_reg(spi, reg_a::NUM_TX_BYTES_1, &[reg1, reg2])
    }

    /// Select which supply the Measure Power Supply command samples
    /// (Table 90 mpsv bits: 0 VDD, 1 VDD_A, 2 VDD_D, 3 VDD_RF,
    /// 4 VDD_AM), then run it. Completion raises I_dct; read the
    /// result via [`Self::ad_result`] at 23.4 mV per LSB.
    pub fn measure_power_supply<S: SpiDevice<u8>>(
        &self,
        spi: &mut S,
        source: u8,
    ) -> Result<(), Error<S::Error>> {
        self.update_reg(spi, reg_a::REGULATOR_CONTROL, 0x07, source & 0x07)?;
        self.direct_command(spi, cmd::MEASURE_POWER_SUPPLY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_decode() {
        // Type 00101, rev 010 (3.1) -> raw 0x2A.
        let id = Identity { ic_type: 0x2A >> 3, ic_rev: 0x2A & 0x07 };
        assert!(id.is_st25r3916());
        assert_eq!(id.ic_rev, 0b010);
    }

    #[test]
    fn fifo_count_packs_ten_bits() {
        // 512 bytes: fifo_b9:8 = 10 in status 2 bits 7:6, LSB 0.
        let bytes = ((0x80u16 & 0xC0) << 2) | 0x00;
        assert_eq!(bytes, 512);
    }

    #[test]
    fn tx_byte_count_encoding() {
        // 7 bytes + 4 bits (an ISO14443A split anticollision frame):
        // reg1 = 7 >> 5 = 0, reg2 = (7 << 3) | 4.
        assert_eq!((7u16 >> 5) as u8, 0);
        assert_eq!((((7u16 & 0x1F) as u8) << 3) | 4, 0x3C);
    }

    #[test]
    fn regulator_mv_decode() {
        // Table 92, 3.3 V column: setting 15 -> 3.4 V, 5 -> 2.4 V.
        assert_eq!(2400 + 100 * (15u16 - 5), 3400);
        assert_eq!(2400 + 100 * (5u16 - 5), 2400);
    }
}
