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

use app_core::nfc::{CardIdentity, CardKind, NfcScanError, TypeAInfo, Uid};
use drivers::st25r3916::{regs, St25r3916};
use embedded_hal::spi::SpiDevice;
use embedded_hal_async::delay::DelayNs;
use heapless::Vec;

use crate::crypto1::Crypto1;
use crate::{iso14443a, mifare, type2};

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

/// One 16-byte block delivered by [`Reader::sweep_classic`], with
/// where it sits and the key that unlocked its sector. Handed to the
/// sweep callback and dropped - the sweep keeps no history, so the
/// RAM footprint is one of these, whatever the card's size. `data` is
/// a copy (16 bytes), not a borrow, so the callback has no lifetime to
/// juggle.
#[derive(Debug, Clone, Copy)]
pub struct SweepBlock {
    /// Sector this block belongs to.
    pub sector: u8,
    /// Absolute block index (0-based across the whole card).
    pub block: u8,
    /// True for the sector trailer (its last block: access bits +
    /// keys; key bytes usually read back as zero).
    pub is_trailer: bool,
    /// The 6-byte key that authenticated this block's sector.
    pub key: [u8; 6],
    /// Whether `key` is key A (`true`) or key B (`false`).
    pub key_is_a: bool,
    /// The 16 plaintext bytes.
    pub data: [u8; 16],
}

/// Tally returned by [`Reader::sweep_classic`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SweepStats {
    /// Sectors on this card family.
    pub sectors_total: u8,
    /// Sectors opened by a default key (A or B).
    pub sectors_unlocked: u8,
    /// Blocks successfully read and delivered to the callback.
    pub blocks_read: u16,
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
    pub async fn identify(&mut self, atqa: [u8; 2]) -> Result<CardIdentity, NfcScanError> {
        let (uid, sak) = self.activate().await?;
        Ok(CardIdentity::Iso14443a(TypeAInfo {
            uid,
            atqa,
            sak,
            kind: iso14443a::classify(atqa, sak),
        }))
    }

    /// Run the anticollision cascade + SELECT on a card already in the
    /// READY state - the caller's REQA, or [`wake`](Self::wake), just
    /// answered - and return its assembled UID and final SAK. This is
    /// the body [`identify`](Self::identify) wraps with classification;
    /// the sweep's re-activation reuses it directly.
    async fn activate(&mut self) -> Result<(Uid, u8), NfcScanError> {
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
            log::debug!("nfc-dbg: anticol L{} ok {:02X?}", level, ac);

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
            log::debug!("nfc-dbg: select L{} ok SAK {:02X}", level, sak);
            if !iso14443a::sak_says_cascade(sak) {
                break;
            }
        }
        Ok((uid, sak))
    }

    /// Type 2 Tag READ (MIFARE Ultralight / NTAG): 16 bytes = 4 pages
    /// from `page`. The card must be ACTIVE - call straight after
    /// [`identify`](Self::identify), before the next REQA drops it back
    /// to IDLE. The chip checks the reply CRC and leaves the two CRC
    /// bytes in the FIFO after the data (as seen on SELECT: SAK + 2),
    /// so the FIFO holds 18 and the data is the first 16.
    pub async fn read_type2(
        &mut self,
        page: u8,
    ) -> Result<[u8; type2::READ_LEN], NfcScanError> {
        let mut rx = [0u8; type2::READ_LEN + 2];
        let n = self
            .transceive("t2read", &[type2::READ, page], true, &mut rx)
            .await?;
        if n < type2::READ_LEN {
            log::warn!("nfc-dbg: t2read page {} short reply n={}", page, n);
            return Err(NfcScanError::Hardware);
        }
        let mut out = [0u8; type2::READ_LEN];
        out.copy_from_slice(&rx[..type2::READ_LEN]);
        Ok(out)
    }

    /// HLTA: put the selected card into HALT so it stops answering
    /// REQA while it stays in the field. It only wakes to WUPA - or to
    /// field loss, which resets it to IDLE - so lifting the card and
    /// presenting it again is seen as a fresh presentation, while a
    /// card left on the back stays quiet. The PICC must NOT reply:
    /// silence is success; any reply means it did not halt.
    pub async fn halt(&mut self) -> Result<(), NfcScanError> {
        self.drv
            .direct_command(self.spi, regs::cmd::CLEAR_FIFO)
            .map_err(hw)?;
        self.drv.fifo_load(self.spi, &iso14443a::HLTA).map_err(hw)?;
        self.drv
            .set_num_tx_bytes(self.spi, iso14443a::HLTA.len() as u16, 0)
            .map_err(hw)?;
        self.drv
            .direct_command(self.spi, regs::cmd::TRANSMIT_WITH_CRC)
            .map_err(hw)?;
        // The frame is out in well under a millisecond; a card that
        // did not halt would NAK within ~100 us of its end.
        self.delay.delay_ms(3).await;
        let irqs = self.drv.read_interrupts(self.spi).map_err(hw)?;
        if irqs.main & regs::irq_main::RX_END != 0 {
            log::warn!(
                "nfc-dbg: hlta answered (main={:#04x}) - card not halted",
                irqs.main,
            );
            return Err(NfcScanError::SelectFailed);
        }
        log::debug!("nfc-dbg: hlta ok (main={:#04x})", irqs.main);
        Ok(())
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
                log::debug!(
                    "nfc-dbg: {} rx tx={:02X?} crc={} main={:#04x} err={:#04x} fifo={} rx={:02X?}",
                    what, tx, with_crc, irqs.main, irqs.error_wup, st.bytes, &rx[..n],
                );
                return Ok(n);
            }
        }
        // A missing reply is the normal answer to a wrong key or an
        // absent card, one line per attempt; per-frame detail only.
        log::debug!("nfc-dbg: {} no RX_END tx={:02X?} crc={}", what, tx, with_crc);
        Err(NfcScanError::NoCard)
    }

    // -- MIFARE Classic Crypto1 (encrypted-parity framing) ------------------
    //
    // HARDWARE-ONLY, NEVER RUN before this step: MIFARE Classic
    // encrypts each byte's parity bit, so the chip's automatic parity
    // is switched off (no_tx_par / no_rx_par) and the parity is framed
    // in software; the nonces carry no CRC and the read's CRC is
    // encrypted, so RX CRC checking is off (no_crc_rx) and the CRC is
    // verified after decryption. The pure cipher/framing this drives
    // is host-tested (crypto1/mifare/iso14443a); only the wire timing
    // is unproven.

    /// Set/clear manual-parity mode (no_tx_par + no_rx_par, Table 27).
    fn set_manual_parity(&mut self, on: bool) -> Result<(), NfcScanError> {
        let bits = regs::iso14443a_nfc::NO_TX_PAR | regs::iso14443a_nfc::NO_RX_PAR;
        let (clear, set) = if on { (0, bits) } else { (bits, 0) };
        self.drv
            .update_reg(self.spi, regs::reg_a::ISO14443A_NFC, clear, set)
            .map_err(hw)?;
        Ok(())
    }

    /// Set/clear receive-without-CRC (no_crc_rx, Table 36).
    fn set_rx_no_crc(&mut self, on: bool) -> Result<(), NfcScanError> {
        let (clear, set) = if on { (0, regs::aux::NO_CRC_RX) } else { (regs::aux::NO_CRC_RX, 0) };
        self.drv.update_reg(self.spi, regs::reg_a::AUX, clear, set).map_err(hw)?;
        Ok(())
    }

    /// Raw transceive for the encrypted frames: transmit `tx_bits`
    /// from `tx` (packed 9-bit symbols) with no chip CRC and no chip
    /// parity, and collect the raw reply bit stream into `rx`,
    /// returning the received bit count. The caller runs the Crypto1
    /// framing on both sides.
    async fn transceive_raw(
        &mut self,
        what: &'static str,
        tx: &[u8],
        tx_bits: usize,
        rx: &mut [u8],
    ) -> Result<usize, NfcScanError> {
        let tx_bytes = tx_bits / 8;
        let extra = (tx_bits % 8) as u8;
        let load = tx_bytes + if extra != 0 { 1 } else { 0 };
        self.drv.direct_command(self.spi, regs::cmd::CLEAR_FIFO).map_err(hw)?;
        self.drv.fifo_load(self.spi, &tx[..load]).map_err(hw)?;
        self.drv
            .set_num_tx_bytes(self.spi, tx_bytes as u16, extra)
            .map_err(hw)?;
        self.drv
            .direct_command(self.spi, regs::cmd::TRANSMIT_WITHOUT_CRC)
            .map_err(hw)?;
        for _ in 0..RX_POLL_TRIES {
            self.delay.delay_ms(RX_POLL_GAP_MS).await;
            let irqs = self.drv.read_interrupts(self.spi).map_err(hw)?;
            let err = irqs.error_wup
                & (regs::irq_error_wup::CRC_ERROR
                    | regs::irq_error_wup::PARITY_ERROR
                    | regs::irq_error_wup::HARD_FRAMING_ERROR
                    | regs::irq_error_wup::SOFT_FRAMING_ERROR);
            if irqs.main & regs::irq_main::RX_END != 0 || err != 0 {
                let st = self.drv.fifo_status(self.spi).map_err(hw)?;
                let nbytes = (st.bytes as usize).min(rx.len());
                self.drv.fifo_read(self.spi, &mut rx[..nbytes]).map_err(hw)?;
                let bits = if st.last_byte_bits == 0 {
                    nbytes * 8
                } else {
                    nbytes.saturating_sub(1) * 8 + st.last_byte_bits as usize
                };
                log::debug!(
                    "nfc-dbg: {} raw main={:#04x} err={:#04x} fifo={} lb={} -> {} bits {:02X?}",
                    what, irqs.main, irqs.error_wup, st.bytes, st.last_byte_bits, bits, &rx[..nbytes],
                );
                if irqs.main & regs::irq_main::RX_END == 0 {
                    return Err(NfcScanError::Hardware);
                }
                return Ok(bits);
            }
        }
        // Same as above: a locked sector answers none of the dictionary
        // keys, so this fires once per key tried. The sweep logs the
        // per-sector "locked" summary at warn instead.
        log::debug!("nfc-dbg: {} raw no RX_END", what);
        Err(NfcScanError::NoCard)
    }

    /// Authenticate one sector (Crypto1 three-pass) with a 6-byte key.
    /// On success returns the synchronized cipher for the encrypted
    /// read that follows. Normal parity + RX CRC are restored on every
    /// exit path, so a failure does not poison the next identify.
    pub async fn mifare_auth(
        &mut self,
        key6: [u8; 6],
        is_key_a: bool,
        block: u8,
        uid32: u32,
    ) -> Result<Crypto1, NfcScanError> {
        // Pass 1: AUTH command, in clear with normal parity but a
        // SOFTWARE CRC transmitted via Transmit-Without-CRC. The chip's
        // Transmit-With-CRC command re-enables the RX CRC check
        // (overriding no_crc_rx), which then rejects the CRC-less tag
        // nonce; Transmit-Without-CRC leaves no_crc_rx in effect, so
        // the 4-byte nonce is received clean.
        self.set_rx_no_crc(true)?;
        let cmd = if is_key_a { mifare::AUTH_KEY_A } else { mifare::AUTH_KEY_B };
        let mut auth = [cmd, block, 0, 0];
        let acrc = iso14443a::crc_a(&auth[..2]);
        auth[2] = acrc[0];
        auth[3] = acrc[1];
        let mut nt_buf = [0u8; 4];
        let nt = match self.transceive("auth1", &auth, false, &mut nt_buf).await {
            Ok(n) if n >= 4 => u32::from_be_bytes(nt_buf),
            other => {
                log::warn!("nfc-dbg: auth1 (nonce) failed: {:?}", other);
                let _ = self.set_rx_no_crc(false);
                return Err(NfcScanError::SelectFailed);
            }
        };
        log::debug!("nfc-dbg: auth nt={:08X}", nt);

        // Pass 2: encrypted {nr, ar} with encrypted parity, no CRC,
        // manual parity; the reply is the 4-byte encrypted aT.
        let frame = mifare::auth_frame(mifare::key_to_u64(&key6), uid32, nt, 0);
        self.set_manual_parity(true)?;
        let mut tx = [0u8; 9];
        let tx_bits = iso14443a::pack_parity_stream(&frame.bytes, &frame.parity, &mut tx);
        let mut rxs = [0u8; 8];
        let result = self.transceive_raw("auth2", &tx, tx_bits, &mut rxs).await;
        // Restore chip framing before interpreting the reply.
        let _ = self.set_manual_parity(false);
        let _ = self.set_rx_no_crc(false);
        let rx_bits = result?;

        let (mut at_b, mut at_p) = ([0u8; 4], [0u8; 4]);
        let n = iso14443a::unpack_parity_stream(&rxs, rx_bits, &mut at_b, &mut at_p);
        if n < 4 {
            log::warn!("nfc-dbg: auth2 short aT n={} ({} bits)", n, rx_bits);
            return Err(NfcScanError::SelectFailed);
        }
        let mut cipher = frame.cipher;
        let mut at_plain = [0u8; 4];
        if let Err(i) = mifare::decrypt_with_parity(&mut cipher, &at_b, &at_p, &mut at_plain) {
            log::warn!("nfc-dbg: aT parity mismatch at byte {} (enc {:02X?})", i, at_b);
            return Err(NfcScanError::SelectFailed);
        }
        let at = u32::from_be_bytes(at_plain);
        if at != frame.at_expected {
            log::warn!("nfc-dbg: aT {:08X} != expected {:08X}", at, frame.at_expected);
            return Err(NfcScanError::SelectFailed);
        }
        log::debug!("nfc-dbg: auth OK aT={:08X}", at);
        Ok(cipher)
    }

    /// Read one 16-byte block over an authenticated Crypto1 session.
    /// Command + CRC, and the 16 data bytes + their CRC, are encrypted
    /// with software parity; the recovered CRC is checked against the
    /// data. Advances `cipher` past the exchange.
    pub async fn mifare_read_block(
        &mut self,
        cipher: &mut Crypto1,
        block: u8,
    ) -> Result<[u8; 16], NfcScanError> {
        let mut cmd = [mifare::READ_BLOCK, block, 0, 0];
        let crc = iso14443a::crc_a(&cmd[..2]);
        cmd[2] = crc[0];
        cmd[3] = crc[1];
        let (mut enc, mut par) = ([0u8; 4], [0u8; 4]);
        mifare::encrypt_with_parity(cipher, &cmd, false, &mut enc, &mut par);
        let mut tx = [0u8; 5];
        let tx_bits = iso14443a::pack_parity_stream(&enc, &par, &mut tx);

        self.set_manual_parity(true)?;
        self.set_rx_no_crc(true)?;
        let mut rxs = [0u8; 24]; // 18 bytes * 9 bits = 162 bits = 21 FIFO bytes
        let result = self.transceive_raw("read", &tx, tx_bits, &mut rxs).await;
        let _ = self.set_manual_parity(false);
        let _ = self.set_rx_no_crc(false);
        let rx_bits = result?;

        let (mut edata, mut epar) = ([0u8; 18], [0u8; 18]);
        let n = iso14443a::unpack_parity_stream(&rxs, rx_bits, &mut edata, &mut epar);
        if n < 18 {
            log::warn!("nfc-dbg: read short reply n={} ({} bits)", n, rx_bits);
            return Err(NfcScanError::Hardware);
        }
        let mut plain = [0u8; 18];
        if let Err(i) = mifare::decrypt_with_parity(cipher, &edata[..18], &epar[..18], &mut plain) {
            log::warn!("nfc-dbg: read parity mismatch at byte {}", i);
            return Err(NfcScanError::Hardware);
        }
        let crc = iso14443a::crc_a(&plain[..16]);
        if crc != [plain[16], plain[17]] {
            log::warn!("nfc-dbg: read CRC {:02X?} != data CRC {:02X?}", crc, &plain[16..18]);
            return Err(NfcScanError::Hardware);
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&plain[..16]);
        Ok(out)
    }

    /// Halt an authenticated card: the HALT command (50 00 + CRC)
    /// encrypted with software parity through the live `cipher`. A
    /// card in a Crypto1 session only understands encrypted frames, so
    /// the plain [`halt`](Self::halt) gets a NAK; this one is
    /// understood and the card falls silent (success = no reply).
    pub async fn mifare_halt(&mut self, cipher: &mut Crypto1) -> Result<(), NfcScanError> {
        let mut cmd = [0x50u8, 0x00, 0, 0];
        let crc = iso14443a::crc_a(&cmd[..2]);
        cmd[2] = crc[0];
        cmd[3] = crc[1];
        let (mut enc, mut par) = ([0u8; 4], [0u8; 4]);
        mifare::encrypt_with_parity(cipher, &cmd, false, &mut enc, &mut par);
        let mut tx = [0u8; 5];
        let tx_bits = iso14443a::pack_parity_stream(&enc, &par, &mut tx);
        let tx_bytes = tx_bits / 8;
        let extra = (tx_bits % 8) as u8;
        let load = tx_bytes + if extra != 0 { 1 } else { 0 };

        self.set_manual_parity(true)?;
        self.set_rx_no_crc(true)?;
        let r = async {
            self.drv.direct_command(self.spi, regs::cmd::CLEAR_FIFO).map_err(hw)?;
            self.drv.fifo_load(self.spi, &tx[..load]).map_err(hw)?;
            self.drv
                .set_num_tx_bytes(self.spi, tx_bytes as u16, extra)
                .map_err(hw)?;
            self.drv
                .direct_command(self.spi, regs::cmd::TRANSMIT_WITHOUT_CRC)
                .map_err(hw)?;
            // A halted card is silent; a card that did not halt NAKs
            // within ~100 us of the frame end.
            self.delay.delay_ms(3).await;
            self.drv.read_interrupts(self.spi).map_err(hw)
        }
        .await;
        let _ = self.set_manual_parity(false);
        let _ = self.set_rx_no_crc(false);
        let irqs = r?;
        if irqs.main & regs::irq_main::RX_END != 0 {
            log::warn!("nfc-dbg: mifare-hlta answered (main={:#04x})", irqs.main);
            return Err(NfcScanError::SelectFailed);
        }
        log::debug!("nfc-dbg: mifare-hlta ok (main={:#04x})", irqs.main);
        Ok(())
    }

    // -- Streaming MIFARE Classic sweep (step 4a) ---------------------------

    /// Bring a card that is not currently selected back into the READY
    /// state. WUPA wakes a HALTed card (where an encrypted/plain HALT
    /// or a failed auth leaves it while the field stays lit); a REQA
    /// fallback covers a card that reset to IDLE instead. Returns the
    /// ATQA. The field must already be up; the answer is CRC-less and
    /// needs no FIFO preparation, exactly as the boot REQA poll proved.
    pub async fn wake(&mut self) -> Result<[u8; 2], NfcScanError> {
        for &cmd in &[regs::cmd::TRANSMIT_WUPA, regs::cmd::TRANSMIT_REQA] {
            self.drv.direct_command(self.spi, cmd).map_err(hw)?;
            for _ in 0..RX_POLL_TRIES {
                self.delay.delay_ms(RX_POLL_GAP_MS).await;
                let irqs = self.drv.read_interrupts(self.spi).map_err(hw)?;
                if irqs.main & regs::irq_main::RX_END != 0 {
                    let st = self.drv.fifo_status(self.spi).map_err(hw)?;
                    let n = (st.bytes as usize).min(2);
                    let mut atqa = [0u8; 2];
                    self.drv.fifo_read(self.spi, &mut atqa[..n]).map_err(hw)?;
                    return Ok(atqa);
                }
            }
        }
        Err(NfcScanError::NoCard)
    }

    /// Wake + re-run the anticollision cascade + SELECT, leaving a
    /// previously-halted card ACTIVE and selected again. Each sector of
    /// a sweep starts from this clean re-activation, so a fresh auth
    /// nonce is exchanged and a failed key attempt cannot poison the
    /// next one.
    async fn reactivate(&mut self) -> Result<(), NfcScanError> {
        self.wake().await?;
        let _ = self.activate().await?;
        Ok(())
    }

    /// Stream every readable block of a MIFARE Classic to `on_block`,
    /// one block at a time, accumulating nothing: the sweep holds only
    /// the current [`SweepBlock`] and its cipher, so the RAM footprint
    /// is flat for a 1K or a 4K. This is the fix for the old in-RAM
    /// whole-card dump that started the footprint disaster.
    ///
    /// Precondition: the card is SELECTED and ACTIVE (call straight
    /// after [`identify`](Self::identify)). The card is put into HALT
    /// on entry so every sector begins uniformly by waking it.
    ///
    /// For each sector it tries key A then key B, each against the
    /// [`mifare::DEFAULT_KEYS`] dictionary, re-activating the card
    /// before every attempt (a failed Crypto1 auth drops the card).
    /// On the first key that authenticates, it reads all blocks of the
    /// sector under that one auth - the cipher advancing across the
    /// reads - then encrypted-HALTs to end the session. A sector no
    /// default key opens is logged and skipped. On return the card is
    /// HALTED.
    pub async fn sweep_classic<F, Fut>(
        &mut self,
        kind: CardKind,
        uid32: u32,
        mut on_block: F,
    ) -> Result<SweepStats, NfcScanError>
    where
        F: FnMut(SweepBlock) -> Fut,
        Fut: core::future::Future<Output = ()>,
    {
        let sectors = mifare::sector_count(kind);
        let mut stats = SweepStats { sectors_total: sectors, ..Default::default() };

        // The card is ACTIVE on entry; drop it to HALT so the per-sector
        // loop can wake it uniformly with WUPA.
        self.halt().await?;

        for sector in 0..sectors {
            let first = mifare::sector_first_block(sector) as u8;
            let nblk = mifare::blocks_in_sector(sector);

            // Find a working key: key A dictionary, then key B. Every
            // attempt re-activates first, because a failed auth leaves
            // the card unselected.
            let mut opened: Option<(Crypto1, [u8; 6], bool)> = None;
            'keys: for &key_is_a in &[true, false] {
                for &key in mifare::DEFAULT_KEYS {
                    // A wake/re-select failure here means the card left
                    // the field (the small coil decouples easily on a
                    // long sweep). That is not a sweep failure - keep
                    // whatever was already read and return it.
                    if self.reactivate().await.is_err() {
                        log::warn!(
                            "nfc-dbg: sweep card lost at sector {} - partial dump, {} blocks read",
                            sector, stats.blocks_read,
                        );
                        return Ok(stats);
                    }
                    if let Ok(cipher) = self.mifare_auth(key, key_is_a, first, uid32).await {
                        opened = Some((cipher, key, key_is_a));
                        break 'keys;
                    }
                }
            }

            match opened {
                Some((mut cipher, key, key_is_a)) => {
                    stats.sectors_unlocked += 1;
                    for b in 0..nblk {
                        let block = first + b;
                        match self.mifare_read_block(&mut cipher, block).await {
                            Ok(data) => {
                                stats.blocks_read += 1;
                                on_block(SweepBlock {
                                    sector,
                                    block,
                                    is_trailer: b + 1 == nblk,
                                    key,
                                    key_is_a,
                                    data,
                                })
                                .await;
                            }
                            Err(e) => log::warn!(
                                "nfc-dbg: sweep sector {} block {} read failed: {:?}",
                                sector, block, e,
                            ),
                        }
                    }
                    // End the crypto session so the card is HALTED for
                    // the next sector's WUPA.
                    if let Err(e) = self.mifare_halt(&mut cipher).await {
                        log::warn!("nfc-dbg: sweep sector {} halt failed: {:?}", sector, e);
                    }
                }
                None => log::warn!("nfc-dbg: sweep sector {} locked (no default key)", sector),
            }
        }
        Ok(stats)
    }
}

/// Map any chip-layer error to the caller-facing hardware error.
fn hw<E>(_e: drivers::st25r3916::Error<E>) -> NfcScanError {
    NfcScanError::Hardware
}
