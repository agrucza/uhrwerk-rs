//! NFC boot canary + boot-time RF probe - ST25R3916 behind the shared
//! SPI bus.
//!
//! Boot canary: raise CS (the park holds it LOW against the
//! unpowered chip - it MUST be high before the rail comes up, or the
//! line back-feeds the dead chip), power DLDO1, run the datasheet's
//! mandatory first contact, read the IC identity, report to the boot
//! console. Screams in the log if a hardware fault or driver
//! regression ever kills the reader.
//!
//! RF probe (TEMPORARY, replaced by a command-driven session in a
//! later step): on a known-good chip, run the bring-up-proven ritual
//! - oscillator (osc_ok), Adjust Regulators, ISO14443A reader field -
//! and poll REQA for a short window after boot. Every card presented
//! is identified through the protocol reader, logged, emitted as
//! `SystemEvent::NfcProbe` (so it lands in the event log and survives
//! a USB drop), then HALTed so it stays quiet while it lies on the
//! back; lifting it and presenting it again is a new presentation.
//!
//! Ritual facts (DS12484 rev 8, section 4.1): the overheat
//! protection frame must be the first contact after every power-up
//! and Set Default; sup3V must be set because DLDO1 feeds the chip
//! 3.3 V while the reset default assumes 5 V; the oscillator is
//! proven by osc_ok, and Adjust Regulators only runs in Ready mode.
//!
//! The chip is SPI mode 1 - its device seat carries that config and
//! the shared-bus wrapper applies it per transaction, so SD (mode 0)
//! traffic can interleave freely.

use app_core::events::SystemEvent;
use app_core::nfc::{CardIdentity, CardKind};
use drivers::pmu::{Config as PmuConfig, Pmu};
use drivers::st25r3916::{regs, Error, St25r3916};
use embassy_time::{Delay, Duration, Instant, Timer};
use esp_hal::gpio::Output;
use esp_hal::spi::master::Config as SpiConfig;
use esp_hal::spi::Mode as SpiMode;
use esp_hal::time::Rate;
use nfc::reader::Reader;
use system_core::bus::{self, SharedI2c, EVENTS};
use system_core::spi_bus::{SharedSpiBus, SharedSpiDevice};

type SpiErr = esp_hal::spi::Error;

/// Boot probe window (DEV, temporary until the command-driven
/// session): how long the field stays up after the identity check,
/// detecting every card presentation, and the REQA cadence. The
/// 500 ms cadence is the one proven at bring-up. The task holds a
/// wake lock for the window, so hardware light sleep is off while the
/// scanner is active.
const CARD_POLL_SECS: u64 = 15;
const CARD_POLL_GAP_MS: u64 = 500;

#[embassy_executor::task]
pub async fn nfc_task(
    i2c_bus: &'static SharedI2c,
    spi_bus: &'static SharedSpiBus,
    mut cs: Output<'static>,
    // Shared flash+SD store, handed in at spawn (SharedStore is not
    // Sync, so it cannot be reached through a global). The Classic
    // dump streams to its SD side.
    store: &'static bus::SharedStore,
) {
    // Let the boot storm pass; the LoRa probe owns the early log.
    Timer::after(Duration::from_secs(5)).await;
    let _wake = bus::WakeHold::new();

    // ORDER MATTERS: CS high while the chip is still unpowered,
    // THEN the rail. The reverse back-feeds the dead chip through
    // its protection diodes (the boot park's whole reason for
    // holding this line low).
    cs.set_high();
    if !set_rail(i2c_bus, true).await {
        bus::boot_report("NFC", "RAIL FAIL");
        cs.set_low();
        // Release the wake lock BEFORE parking the task forever -
        // holding it here would silently disable hardware light
        // sleep for the whole uptime (the fake-sleep bug: display
        // off, executor idling awake, battery draining at
        // awake-level current with no log to show for it). The
        // `return` also tells the borrow checker this branch never
        // reaches the tail (pending()'s type alone doesn't).
        drop(_wake);
        return core::future::pending::<()>().await;
    }
    // Rail settle + chip POR.
    Timer::after(Duration::from_millis(5)).await;

    // The chip talks SPI mode 1, unlike everything else on this
    // bus - its device seat carries the config.
    let mut spi = SharedSpiDevice::with_config(
        spi_bus,
        cs,
        SpiConfig::default()
            .with_frequency(Rate::from_khz(400))
            .with_mode(SpiMode::_1),
    );
    let drv = St25r3916::new();

    // Identity FIRST: this is the line the boot console waits on, so
    // it must land inside the boot-report budget - before the RF
    // window, which would otherwise push it past the deadline.
    let identified = match probe(&drv, &mut spi).await {
        Ok(true) => {
            log::info!("NFC: canary complete");
            bus::boot_report("NFC", "OK");
            true
        }
        Ok(false) => {
            bus::boot_report("NFC", "FAIL");
            false
        }
        Err(e) => {
            log::error!("NFC: probe failed: {:?}", e);
            bus::boot_report("NFC", "FAIL");
            false
        }
    };

    // RF probe, only on a chip that answered its identity. The wake
    // lock taken above spans the whole window, so hardware light
    // sleep stays off while the scanner is active.
    if identified {
        match rf_probe(&drv, &mut spi, store).await {
            Ok(n) => log::info!("NFC: probe window closed, {} presentation(s)", n),
            Err(e) => log::error!("NFC: RF probe failed: {:?}", e),
        }
    }

    // Teardown regardless of outcome: chip to power-down mode
    // (OP_CONTROL = 0 drops EN/TX/RX, so the field and oscillator
    // are off), rail off, CS back to its parked LOW.
    let _ = drv.stop_all_activities(&mut spi);
    let _ = drv.write_reg(&mut spi, regs::reg_a::OP_CONTROL, &[0]);
    set_rail(i2c_bus, false).await;
    let mut cs = spi.release();
    cs.set_low();
    log::info!("NFC: ST25R3916 parked (rail off, CS low)");
    // Release the wake lock BEFORE parking the task forever (same
    // hazard as the early-return path above - a held lock here cost
    // two days of fake sleep before the missing 5 s heartbeat logs
    // gave it away).
    drop(_wake);
    // Keep the task (and thus the driven CS pin) alive forever.
    core::future::pending::<()>().await
}

/// Switch DLDO1, the reader's whole power domain.
async fn set_rail(i2c_bus: &'static SharedI2c, on: bool) -> bool {
    let pmu = Pmu::new(PmuConfig::default());
    let mut i2c = i2c_bus.lock().await;
    let r = if on {
        pmu.set_dldo1_voltage(&mut *i2c, 3300)
            .and_then(|_| pmu.set_dldo1_enable(&mut *i2c, true))
    } else {
        pmu.set_dldo1_enable(&mut *i2c, false)
    };
    if r.is_err() {
        log::error!("NFC: rail switch failed ({})", if on { "on" } else { "off" });
        return false;
    }
    true
}

/// The identity probe: overheat frame, 3.3 V supply mode, MCU_CLK
/// off, then read the IC identity. Returns `Ok(true)` for an
/// ST25R3916, `Ok(false)` for a chip that answered with the wrong
/// identity. No RF here - the field only comes up in [`rf_probe`].
async fn probe(
    drv: &St25r3916,
    spi: &mut SharedSpiDevice,
) -> Result<bool, Error<SpiErr>> {
    // 1. The mandatory first contact, then supply mode: DLDO1 is
    //    3.3 V and the reset default assumes 5 V - skipping sup3V
    //    is the classic silent misconfiguration. MCU_CLK output is
    //    disabled (the pin is unconnected on this board).
    drv.apply_overheat_protection_fix(spi)?;
    drv.write_reg(spi, regs::reg_a::IO_CONF2, &[regs::io_conf2::SUP3V])?;
    drv.write_reg(
        spi,
        regs::reg_a::IO_CONF1,
        &[regs::io_conf1::OUT_CL1 | regs::io_conf1::OUT_CL0 | regs::io_conf1::LF_CLK_OFF],
    )?;

    // 2. Identity - this chip HAS an ID register.
    let id = drv.identity(spi)?;
    if id.is_st25r3916() {
        log::info!("NFC: ST25R3916 identified (silicon rev {})", id.ic_rev);
        Ok(true)
    } else {
        log::warn!(
            "NFC: unexpected identity (type {:#07b}, rev {}) - aborting",
            id.ic_type, id.ic_rev,
        );
        Ok(false)
    }
}

/// RF probe: the bring-up-proven sequence (commit 517abfa) - Ready
/// mode (oscillator, proven by osc_ok), Adjust Regulators, then the
/// ISO14443A reader field and a REQA poll - and, for every card that
/// answers within the window, the anticollision cascade + SELECT
/// through the protocol reader. Every presentation is logged and
/// emitted as `SystemEvent::NfcProbe`, then the card is HALTed so it
/// stays quiet while it lies on the back; lifting it and presenting
/// it again is a new presentation. An empty window emits one
/// `nfc_nocard`. Returns the number of presentations. The caller
/// tears the field down afterwards.
async fn rf_probe(
    drv: &St25r3916,
    spi: &mut SharedSpiDevice,
    store: &'static bus::SharedStore,
) -> Result<u32, Error<SpiErr>> {
    // 3. Ready mode: oscillator + regulators on, proven by osc_ok.
    drv.update_reg(spi, regs::reg_a::OP_CONTROL, 0, regs::op_control::EN)?;
    let mut osc = false;
    for _ in 0..100 {
        Timer::after(Duration::from_millis(1)).await;
        if drv.aux_display(spi)? & regs::aux_display::OSC_OK != 0 {
            osc = true;
            break;
        }
    }
    if !osc {
        log::warn!("NFC: 27.12 MHz oscillator never stabilized - RF probe skipped");
        return Ok(0);
    }
    log::info!("NFC: oscillator running");

    // 4. Regulator adjustment (improves PSRR per the ritual), then
    //    read back what the regulators settled at.
    drv.adjust_regulators(spi)?;
    wait_irq_timer_nfc(drv, spi, regs::irq_timer_nfc::DCT, 10).await?;
    match drv.regulator_result_mv_3v3(spi)? {
        Some(mv) => log::info!("NFC: regulators adjusted (VDD_RF {} mV)", mv),
        None => log::warn!("NFC: regulator display below 3.3 V-mode range"),
    }

    // 5. ISO14443A reader field + REQA poll. Mode om=0001 initiator
    //    ISO14443A, OOK modulation; 106 kbit/s both ways. REQA/ATQA
    //    needs no CRC handling (automatic per 4.4.4) and no FIFO
    //    preparation.
    drv.write_reg(spi, regs::reg_a::MODE, &[0x08])?;
    drv.write_reg(spi, regs::reg_a::BIT_RATE, &[0x00])?;
    drv.update_reg(
        spi,
        regs::reg_a::OP_CONTROL,
        0,
        regs::op_control::TX_EN | regs::op_control::RX_EN,
    )?;
    // ISO14443-3 guard time before the first command.
    Timer::after(Duration::from_millis(6)).await;
    log::info!(
        "NFC: field on - present cards for {} s",
        CARD_POLL_SECS,
    );
    let deadline = Instant::now() + Duration::from_secs(CARD_POLL_SECS);
    let mut presentations: u32 = 0;
    while Instant::now() < deadline {
        drv.direct_command(spi, regs::cmd::TRANSMIT_REQA)?;
        // ATQA arrives within ~100 us of the REQA end; poll the
        // (self-clearing) interrupt registers briefly.
        for _ in 0..5 {
            Timer::after(Duration::from_millis(2)).await;
            let irqs = drv.read_interrupts(spi)?;
            if irqs.main & regs::irq_main::RX_END != 0 {
                let st = drv.fifo_status(spi)?;
                let n = (st.bytes as usize).min(2);
                let mut atqa = [0u8; 2];
                drv.fifo_read(spi, &mut atqa[..n])?;
                // On the lit field, run the anticollision cascade +
                // SELECT through the protocol reader. It borrows the
                // device seat for the exchange and hands it back.
                let mut delay = Delay;
                let mut reader = Reader::new(&mut *spi, &mut delay);
                match reader.identify(atqa).await {
                    Ok(card) => {
                        // Set once a card ends in a Crypto1 session and
                        // is halted with the ENCRYPTED HALT below; then
                        // the plain HLTA at the end is skipped (an authed
                        // card only understands encrypted frames).
                        let mut crypto_halted = false;
                        match &card {
                            CardIdentity::Iso14443a(a) => log::info!(
                                "NFC: identified {} UID {:02X?} ATQA {:02X} {:02X} SAK {:02X}",
                                a.kind.label(), &a.uid[..], a.atqa[0], a.atqa[1], a.sak,
                            ),
                            other => log::info!(
                                "NFC: identified {} id {:02X?}",
                                other.label(), other.id_bytes(),
                            ),
                        }
                        // Step 3a: on a Type 2 tag - still ACTIVE
                        // straight after SELECT - read pages 0..=3 and
                        // check the UID they carry against the one
                        // anticollision assembled. Proves the 16-byte
                        // block read + CRC-tail path with no
                        // cryptography in the way.
                        if let CardIdentity::Iso14443a(a) = &card {
                            if a.kind == CardKind::MifareUltralight {
                                match reader.read_type2(0).await {
                                    Ok(pages) => {
                                        let check = match nfc::type2::uid_from_pages0_3(&pages) {
                                            Some(u) if u.as_slice() == &a.uid[..] => "UID matches",
                                            Some(_) => "UID MISMATCH",
                                            None => "BCC FAIL",
                                        };
                                        log::info!(
                                            "NFC: type2 pages 0-3 {:02X?} - {}",
                                            pages, check,
                                        );
                                    }
                                    Err(e) => log::warn!("NFC: type2 read failed: {:?}", e),
                                }
                            }
                            // Step 4a/4b: MIFARE Classic - STREAMING sweep of
                            // the whole card. Every sector is opened with a
                            // default key (A then B), all its blocks read
                            // under that one auth, and each 16-byte block
                            // handed to the callback the instant it is read:
                            // logged to serial and, when an SD card is
                            // present, appended to a raw dump image. Nothing
                            // is accumulated in RAM. On return the card is
                            // HALTED, so the plain HLTA at the end is skipped.
                            if a.kind.is_mifare_classic() {
                                let uid32 = nfc::mifare::uid_for_auth(&a.uid);
                                // Dump path /nfc/<first-4-UID-bytes>.NFC, FAT
                                // 8.3-safe (8 hex + 3-char ext). Built into a
                                // fixed ASCII buffer - no heapless in this bin.
                                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                                let mut pbuf = *b"/nfc/00000000.NFC";
                                for k in 0..4 {
                                    let byte = a.uid.get(k).copied().unwrap_or(0);
                                    pbuf[5 + k * 2] = HEX[(byte >> 4) as usize];
                                    pbuf[6 + k * 2] = HEX[(byte & 0x0F) as usize];
                                }
                                let path = core::str::from_utf8(&pbuf).unwrap_or("/nfc/dump.NFC");

                                // Truncate/create the dump up front (also makes
                                // the /nfc dir). SD-only, never mirrored to
                                // flash. If SD is offline the sweep still runs,
                                // serial-only - identify already succeeded.
                                let sd_dump = {
                                    let mut g = store.lock().await;
                                    if g.sd_online() {
                                        match g.sd_mut().map(|sd| sd.write_file(path, &[])) {
                                            Some(Ok(())) => true,
                                            Some(Err(e)) => {
                                                log::warn!("NFC: dump create {} failed: {:?}", path, e);
                                                false
                                            }
                                            None => false,
                                        }
                                    } else {
                                        log::info!("NFC: no SD card - dumping to serial only");
                                        false
                                    }
                                };

                                // Byte offset of the next block in the image;
                                // locked sectors are zero-filled so block N
                                // always lands at byte N*16. Interior mutability
                                // (the async callback is FnMut -> Future and
                                // can't borrow &mut across calls).
                                let next = core::cell::Cell::new(0u16);
                                let next_ref = &next;
                                let on_block = move |b: nfc::reader::SweepBlock| async move {
                                    log::info!(
                                        "NFC: sweep S{:02} B{:03}{} key{} {:02X?} | {:02X?}",
                                        b.sector,
                                        b.block,
                                        if b.is_trailer { "*" } else { " " },
                                        if b.key_is_a { "A" } else { "B" },
                                        b.key,
                                        b.data,
                                    );
                                    if !sd_dump {
                                        return;
                                    }
                                    // One lock, only synchronous SD writes
                                    // inside it, then release - the store lock
                                    // must never be held across an await.
                                    let mut g = store.lock().await;
                                    if let Some(sd) = g.sd_mut() {
                                        let target = b.block as u16;
                                        let mut n = next_ref.get();
                                        while n < target {
                                            let _ = sd.append_line(path, &[0u8; 16]);
                                            n += 1;
                                        }
                                        let _ = sd.append_line(path, &b.data);
                                        next_ref.set(target + 1);
                                    }
                                };
                                match reader.sweep_classic(a.kind, uid32, on_block).await {
                                    Ok(s) => log::info!(
                                        "NFC: sweep done - {}/{} sectors unlocked, {} blocks read{}",
                                        s.sectors_unlocked,
                                        s.sectors_total,
                                        s.blocks_read,
                                        if sd_dump { " (saved to SD)" } else { "" },
                                    ),
                                    Err(e) => log::warn!("NFC: sweep failed: {:?}", e),
                                }
                                // The sweep leaves the card HALTED (encrypted
                                // halt of the last sector, or a failed auth) -
                                // do not send it a plain HLTA on top.
                                crypto_halted = true;
                            }
                        }
                        presentations += 1;
                        // Every presentation goes to the event log -
                        // the evidence must survive a USB drop.
                        EVENTS.send(SystemEvent::NfcProbe { card: Some(card) }).await;
                        // HALT the card so it stays quiet while it lies
                        // on the back; lifting it resets it to IDLE and
                        // the next presentation is detected afresh. No
                        // dedup needed - and none wanted. A card halted
                        // inside a Crypto1 session (above) is skipped:
                        // it would NAK a plain HLTA.
                        if !crypto_halted {
                            if let Err(e) = reader.halt().await {
                                log::warn!("NFC: halt failed: {:?}", e);
                            }
                        }
                    }
                    Err(e) => log::warn!(
                        "NFC: card answered REQA (ATQA {:02X} {:02X}) but identify failed: {:?}",
                        atqa[0], atqa[1], e,
                    ),
                }
                break;
            }
        }
        Timer::after(Duration::from_millis(CARD_POLL_GAP_MS)).await;
    }
    if presentations == 0 {
        log::info!("NFC: no card identified in the {} s window", CARD_POLL_SECS);
        EVENTS.send(SystemEvent::NfcProbe { card: None }).await;
    }
    Ok(presentations)
}

/// Poll the (read-clears) interrupt registers until a timer/NFC bit
/// shows up or the budget in milliseconds runs out.
async fn wait_irq_timer_nfc(
    drv: &St25r3916,
    spi: &mut SharedSpiDevice,
    bit: u8,
    budget_ms: u32,
) -> Result<(), Error<SpiErr>> {
    for _ in 0..budget_ms {
        Timer::after(Duration::from_millis(1)).await;
        if drv.read_interrupts(spi)?.timer_nfc & bit != 0 {
            return Ok(());
        }
    }
    log::warn!("NFC: command-termination IRQ not seen within {} ms", budget_ms);
    Ok(())
}
