//! The RF reader, identify-only: drives the chip over a `SpiDevice`
//! to run the ISO14443-A activation - WUPA, the anticollision
//! cascade, SELECT - and returns the card's identity as a
//! [`CardInfo`].
//!
//! The caller owns the rail, the chip's power-up ritual, and the RF
//! field: this reader only transceives on a field that is already
//! lit, and leaves the field exactly as it found it. The transceive
//! is the FIFO / interrupt pattern proven at bring-up.
//!
//! HARDWARE-ONLY PATH: the anticollision exchange depends on the
//! chip's FIFO and interrupt timing and must be verified on the
//! watch; the pure decode it feeds ([`crate::iso14443a`]) is
//! host-tested.
//!
//! DIAGNOSTIC (temporary, step 2 bring-up): every exchange logs its
//! label, the raw interrupt flags, the FIFO count and the received
//! bytes - on the error path too - so a failed cascade says exactly
//! which exchange failed and what the chip actually delivered.

use app_core::nfc::{CardInfo, NfcScanError, Uid};
use drivers::st25r3916::{regs, St25r3916};
use embedded_hal::spi::SpiDevice;
use embedded_hal_async::delay::DelayNs;
use heapless::Vec;

use crate::iso14443a;

/// Per-transceive response wait: poll the RX interrupt this many
/// times, this far apart. A card answers within ~100 us of the
/// command end, so this is generous.
const RX_POLL_TRIES: u32 = 6;
const RX_POLL_GAP_MS: u32 = 2;

/// Reader over one chip seat. Borrows the SPI device and a delay for
/// the reader's lifetime; the field must already be up.
pub struct Reader<'a, S: SpiDevice, D: DelayNs> {
    drv: St25r3916,
    spi: &'a mut S,
    delay: &'a mut D,
}

impl<'a, S: SpiDevice, D: DelayNs> Reader<'a, S, D> {
    pub fn new(spi: &'a mut S, delay: &'a mut D) -> Self {
        Reader { drv: St25r3916::new(), spi, delay }
    }

    /// Identify the one card the caller's REQA just brought into the
    /// READY state: run the anticollision cascade and SELECT per
    /// level until the SAK says the UID is complete. Takes the ATQA
    /// that REQA returned. Returns the identity; the card family is
    /// classified from ATQA + SAK.
    ///
    /// No WUPA here, deliberately. A card that has answered REQA is
    /// in READY and, per the ISO14443-3 state machine, answers only
    /// the anticollision/SELECT commands: a REQA or WUPA in READY gets
    /// no reply and drops the card back to IDLE (hardware-observed:
    /// TX_END fired, RX never came). WUPA is for waking a HALTed card,
    /// which is a different flow.
    pub async fn identify(&mut self, atqa: [u8; 2]) -> Result<CardInfo, NfcScanError> {

        let mut uid: Uid = Vec::new();
        let mut sak = 0u8;
        for level in 1..=3u8 {
            let sel = iso14443a::sel_for_level(level).ok_or(NfcScanError::SelectFailed)?;

            // Anticollision: [SEL, NVB=0x20], no CRC. Reply is the
            // 4 UID bytes of this level + BCC - also CRC-less, so the
            // receiver must be told it is an anticollision frame
            // (`antcl`, Table 27 bit 0) or it CRC-checks the reply
            // and rejects it. REQA/WUPA get CRC-less receive for free
            // from their direct commands; only this exchange needs
            // the bit, and it MUST be back to 0 for SELECT and
            // everything else - so it is cleared on every path
            // before an error can propagate.
            self.drv
                .update_reg(
                    self.spi,
                    regs::reg_a::ISO14443A_NFC,
                    0,
                    regs::iso14443a_nfc::ANTCL,
                )
                .map_err(hw)?;
            let mut ac = [0u8; 5];
            let anticol = self.transceive("anticol", &[sel, 0x20], false, &mut ac).await;
            self.drv
                .update_reg(
                    self.spi,
                    regs::reg_a::ISO14443A_NFC,
                    regs::iso14443a_nfc::ANTCL,
                    0,
                )
                .map_err(hw)?;
            let n = anticol.map_err(|_| NfcScanError::SelectFailed)?;
            if n < 5 {
                log::warn!("nfc-dbg: anticol L{} short reply n={} {:02X?}", level, n, &ac[..n]);
                return Err(NfcScanError::SelectFailed);
            }
            if let Err(()) = iso14443a::absorb_level(&mut uid, &ac) {
                log::warn!("nfc-dbg: anticol L{} BCC mismatch {:02X?}", level, ac);
                return Err(NfcScanError::SelectFailed);
            }
            log::info!("nfc-dbg: anticol L{} ok {:02X?}", level, ac);

            // SELECT: [SEL, NVB=0x70, uid0..3, bcc] with CRC. Reply
            // is the 1-byte SAK (+CRC, stripped by the chip).
            let sel_frame = [sel, 0x70, ac[0], ac[1], ac[2], ac[3], ac[4]];
            let mut sak_buf = [0u8; 1];
            let n = self
                .transceive("select", &sel_frame, true, &mut sak_buf)
                .await
                .map_err(|_| NfcScanError::SelectFailed)?;
            if n < 1 {
                log::warn!("nfc-dbg: select L{} empty reply", level);
                return Err(NfcScanError::SelectFailed);
            }
            sak = sak_buf[0];
            log::info!("nfc-dbg: select L{} ok SAK {:02X}", level, sak);
            if !iso14443a::sak_says_cascade(sak) {
                break;
            }
        }

        Ok(CardInfo { uid, atqa, sak, kind: iso14443a::classify(atqa, sak) })
    }

    /// Send `tx` and collect the reply into `rx`, returning the byte
    /// count. `with_crc` selects the automatic-CRC transmit command.
    /// Built on the same FIFO / interrupt pattern the probe proved.
    /// `what` labels the exchange in the diagnostic log.
    async fn transceive(
        &mut self,
        what: &'static str,
        tx: &[u8],
        with_crc: bool,
        rx: &mut [u8],
    ) -> Result<usize, NfcScanError> {
        self.drv.direct_command(self.spi, regs::cmd::CLEAR_FIFO).map_err(hw)?;
        self.drv.fifo_load(self.spi, tx).map_err(hw)?;
        self.drv
            .set_num_tx_bytes(self.spi, tx.len() as u16, 0)
            .map_err(hw)?;
        let cmd = if with_crc {
            regs::cmd::TRANSMIT_WITH_CRC
        } else {
            regs::cmd::TRANSMIT_WITHOUT_CRC
        };
        self.drv.direct_command(self.spi, cmd).map_err(hw)?;

        for _ in 0..RX_POLL_TRIES {
            self.delay.delay_ms(RX_POLL_GAP_MS).await;
            let irqs = self.drv.read_interrupts(self.spi).map_err(hw)?;
            let err_bits = irqs.error_wup
                & (regs::irq_error_wup::CRC_ERROR
                    | regs::irq_error_wup::PARITY_ERROR
                    | regs::irq_error_wup::HARD_FRAMING_ERROR);
            if err_bits != 0 {
                // DIAGNOSTIC: read what the chip delivered before
                // failing, so the log shows whether the data is
                // intact behind a spurious CRC flag (a CRC-less
                // anticollision reply) or genuinely mangled.
                let st = self.drv.fifo_status(self.spi).map_err(hw)?;
                let n = (st.bytes as usize).min(rx.len());
                self.drv.fifo_read(self.spi, &mut rx[..n]).map_err(hw)?;
                log::warn!(
                    "nfc-dbg: {} ERR tx={:02X?} crc={} main={:#04x} err={:#04x} fifo={} rx={:02X?}",
                    what, tx, with_crc, irqs.main, irqs.error_wup, st.bytes, &rx[..n],
                );
                return Err(NfcScanError::Hardware);
            }
            if irqs.main & regs::irq_main::RX_END != 0 {
                let st = self.drv.fifo_status(self.spi).map_err(hw)?;
                let n = (st.bytes as usize).min(rx.len());
                self.drv.fifo_read(self.spi, &mut rx[..n]).map_err(hw)?;
                log::info!(
                    "nfc-dbg: {} rx tx={:02X?} crc={} main={:#04x} err={:#04x} fifo={} rx={:02X?}",
                    what, tx, with_crc, irqs.main, irqs.error_wup, st.bytes, &rx[..n],
                );
                return Ok(n);
            }
        }
        log::warn!("nfc-dbg: {} no RX_END tx={:02X?} crc={}", what, tx, with_crc);
        Err(NfcScanError::NoCard)
    }
}

/// Map any chip-layer error to the caller-facing hardware error.
fn hw<E>(_e: drivers::st25r3916::Error<E>) -> NfcScanError {
    NfcScanError::Hardware
}
