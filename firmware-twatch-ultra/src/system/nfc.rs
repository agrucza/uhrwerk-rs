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
use esp_hal::gpio::{Input, Output};
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
const CARD_POLL_SECS: u64 = 2;
const CARD_POLL_GAP_MS: u64 = 500;

/// Wake-up amplitude detection delta (`am_d<3:0>`, 0..=15). Lower =
/// more sensitive: a smaller antenna-amplitude change trips the sensor.
/// A MIFARE Classic 1K couples strongly and trips at almost any value;
/// weaker couplers (an NTAG216, and a MIFARE Classic 4K) produce a
/// smaller change and were missed at 4 - the NTAG hit-and-miss, the 4K
/// not at all. 2 catches them. Raise it toward 15 if idle antenna
/// drift false-trips (harmless-but-wasteful: the field powers up, no
/// card answers, the gate keeps it silent); lower toward 1 if a card
/// still won't trip.
///
/// The post-detection false-trip loop that a tight delta used to
/// provoke is fixed at the source: `arm_wakeup` measures the empty
/// antenna and arms with that as a FIXED reference (am_ae = 0) on every
/// arm, so a card can no longer drag an auto-averaged reference off the
/// baseline. That let delta stay at 2 (needed for the weakly-coupling
/// 4K/NTAG) without the loop.
const WAKEUP_DELTA: u8 = 2;

/// Field-off settle in `arm_wakeup` before the reference measurement:
/// lets the reader field ring down so the measured amplitude is the
/// empty-antenna baseline. Also throttles the arm loop between trips.
const WAKEUP_REARM_SETTLE_MS: u64 = 200;

/// Outcome of one RF poll window. `presentations` counts every card
/// that identified. `dump` is set only when the poll was armed for a
/// dump AND a MIFARE Classic card was actually swept: the block count
/// read and whether the sweep (and its SD write, when present)
/// completed cleanly. A false trip, or a non-dumpable card tapped while
/// armed, leaves `dump` `None` so the arm stays standing.
struct ProbeOutcome {
    presentations: u32,
    /// A completed dump's `(blocks_read, sectors_unlocked, ok)`, or
    /// `None` when this probe did not dump (plain identify or false
    /// trip). `ok` is false if the dump file could not be written.
    dump: Option<(u16, u8, bool)>,
    /// A card answered REQA this window (identified or not), so one is
    /// physically resting on the coil. The caller waits for it to leave
    /// before re-arming wake-up mode.
    saw_card: bool,
}

#[embassy_executor::task]
pub async fn nfc_task(
    i2c_bus: &'static SharedI2c,
    spi_bus: &'static SharedSpiBus,
    mut cs: Output<'static>,
    // Shared flash+SD store, handed in at spawn (SharedStore is not
    // Sync, so it cannot be reached through a global). The Classic
    // dump streams to its SD side.
    store: &'static bus::SharedStore,
    // ST25R3916 IRQ line (GPIO5, active-high). In wake-up mode the
    // chip drives it high when a card trips the amplitude threshold.
    mut irq: Input<'static>,
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

    // Wake-up standby, only on a chip that answered its identity.
    // DLDO1 STAYS ON here - the chip needs power to run its 32 kHz
    // wake-up timer. The loop never returns.
    if identified {
        // Release the boot wake lock: the loop is idle (parked on the
        // IRQ) between taps and must let the system light-sleep. It
        // takes its own short-lived lock only while handling a trip.
        // (Phase 1 is tested while awake; arming GPIO5 as a light-
        // sleep wake source is Phase 2.)
        drop(_wake);
        wakeup_loop(&drv, &mut spi, &mut irq, store).await
    } else {
        // Unidentified chip: tear down and park, as before. Chip to
        // power-down (OP_CONTROL = 0 drops EN/TX/RX), rail off, CS
        // back to its parked LOW.
        let _ = drv.stop_all_activities(&mut spi);
        let _ = drv.write_reg(&mut spi, regs::reg_a::OP_CONTROL, &[0]);
        set_rail(i2c_bus, false).await;
        let mut cs = spi.release();
        cs.set_low();
        log::info!("NFC: ST25R3916 parked (rail off, CS low)");
        // Release the wake lock BEFORE parking forever (a held lock
        // here cost two days of fake sleep once).
        drop(_wake);
        core::future::pending::<()>().await
    }
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
    // Read the card's contents after identifying it: Type 2 pages or a
    // full Classic dump to SD. A plain identify tap passes `false` so
    // it never spins a multi-second sweep; dump mode (Phase 3) passes
    // `true`.
    do_dump: bool,
) -> Result<ProbeOutcome, Error<SpiErr>> {
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
        return Ok(ProbeOutcome { presentations: 0, dump: None, saw_card: false });
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
    // Set when a Classic sweep actually ran this window (dump mode).
    // (blocks_read, ok). Carried out in the ProbeOutcome so the caller
    // can report NfcDumpComplete and disarm.
    let mut dump: Option<(u16, u8, bool)> = None;
    // Whether any card answered REQA (ATQA received), even if it then
    // failed to identify. Gates the "unrecognized" report so a bare
    // wake-up trip with no card answering (a false trip, or the card
    // already lifted) stays silent instead of flashing "not recognized".
    let mut saw_atqa = false;
    while Instant::now() < deadline {
        drv.direct_command(spi, regs::cmd::TRANSMIT_REQA)?;
        // ATQA arrives within ~100 us of the REQA end; poll the
        // (self-clearing) interrupt registers briefly.
        for _ in 0..5 {
            Timer::after(Duration::from_millis(2)).await;
            let irqs = drv.read_interrupts(spi)?;
            if irqs.main & regs::irq_main::RX_END != 0 {
                saw_atqa = true;
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
                        // Announce the identified card NOW - before any
                        // multi-second Classic sweep - so the recognition
                        // click and the wake-to-screen land the instant
                        // the card is read, not after the dump finishes.
                        // This is also the presentation's event-log
                        // record. On a dump the dump-done click then
                        // arrives seconds later (NfcDumpComplete), so the
                        // two reads as start/finish, not a double-click.
                        // Clone so `card` stays for the dump borrow below.
                        presentations += 1;
                        EVENTS
                            .send(SystemEvent::NfcProbe { card: Some(card.clone()) })
                            .await;
                        // Step 3a: on a Type 2 tag - still ACTIVE
                        // straight after SELECT - read pages 0..=3 and
                        // check the UID they carry against the one
                        // anticollision assembled. Proves the 16-byte
                        // block read + CRC-tail path with no
                        // cryptography in the way.
                        //
                        // Gated on dump mode: a plain identify tap must
                        // not read pages or spin a multi-second sweep.
                        // Phase 3's dump-arm passes do_dump = true.
                        if do_dump {
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
                                    // Structured dump streamed to a flash file,
                                    // one record per readable block - NOTHING
                                    // accumulates in RAM. Path
                                    // /system/nfc/dumps/<full-UID-hex>.dump: same
                                    // stem as the card's meta blob, its own dir
                                    // so the card enumeration never parses it.
                                    // Flash-only - a full-UID name exceeds the SD
                                    // FAT 8.3 limit, and the library is a flash
                                    // feature. Built into a fixed ASCII buffer -
                                    // no heapless in this bin. Must match
                                    // system-core's dump_path().
                                    const HEX: &[u8; 16] = b"0123456789ABCDEF";
                                    let mut pbuf = [0u8; 64];
                                    let prefix = b"/system/nfc/dumps/";
                                    pbuf[..prefix.len()].copy_from_slice(prefix);
                                    let mut plen = prefix.len();
                                    for &byte in &a.uid[..] {
                                        pbuf[plen] = HEX[(byte >> 4) as usize];
                                        pbuf[plen + 1] = HEX[(byte & 0x0F) as usize];
                                        plen += 2;
                                    }
                                    pbuf[plen..plen + 5].copy_from_slice(b".dump");
                                    plen += 5;
                                    let path = core::str::from_utf8(&pbuf[..plen])
                                        .unwrap_or("/system/nfc/dumps/dump.dump");

                                    // Truncate/create with a small header: magic
                                    // "NFD", format version, a card-kind byte
                                    // (1=1K, 2=4K), and the 16-byte block size.
                                    // Flash-only via flash_mut().
                                    let kind_byte: u8 = match a.kind {
                                        CardKind::MifareClassic1K => 1,
                                        CardKind::MifareClassic4K => 2,
                                        _ => 0,
                                    };
                                    let header = [b'N', b'F', b'D', 1u8, kind_byte, 16u8];
                                    let created = {
                                        let mut g = store.lock().await;
                                        match g.flash_mut().write_file(path, &header) {
                                            Ok(()) => true,
                                            Err(e) => {
                                                log::warn!("NFC: dump create {} failed: {:?}", path, e);
                                                false
                                            }
                                        }
                                    };

                                    // `dump_failed` latches on the first write
                                    // error (create included) so a flaky card
                                    // yields a clearly-incomplete file, not a
                                    // silent truncation. Interior mutability: the
                                    // async callback is FnMut -> Future and can't
                                    // borrow &mut across calls.
                                    let dump_failed = core::cell::Cell::new(!created);
                                    let dump_failed_ref = &dump_failed;
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
                                        // Serial logging above always runs; skip
                                        // the file once create or a prior write
                                        // failed.
                                        if dump_failed_ref.get() {
                                            return;
                                        }
                                        // One 25-byte record per block: sector,
                                        // block, flags (bit0 key A, bit1 trailer),
                                        // the 6-byte key, the 16 data bytes. A
                                        // sector that never opens fires no
                                        // callback, so it is simply ABSENT from
                                        // the file - unambiguous, unlike a zero
                                        // fill.
                                        let flags = (b.key_is_a as u8) | ((b.is_trailer as u8) << 1);
                                        let mut rec = [0u8; 25];
                                        rec[0] = b.sector;
                                        rec[1] = b.block;
                                        rec[2] = flags;
                                        rec[3..9].copy_from_slice(&b.key);
                                        rec[9..25].copy_from_slice(&b.data);
                                        // One lock, only synchronous flash writes
                                        // inside it, then release - the store lock
                                        // must never be held across an await.
                                        let mut g = store.lock().await;
                                        if g.flash_mut().append_line(path, &rec).is_err() {
                                            log::warn!(
                                                "NFC: dump write to {} failed at S{:02} B{:03} - stopping, file incomplete",
                                                path, b.sector, b.block,
                                            );
                                            dump_failed_ref.set(true);
                                        }
                                    };
                                    match reader.sweep_classic(a.kind, uid32, on_block).await {
                                        Ok(s) => {
                                            let status = if dump_failed.get() {
                                                " (dump INCOMPLETE - write error)"
                                            } else {
                                                " (saved to flash)"
                                            };
                                            log::info!(
                                                "NFC: sweep done - {}/{} sectors unlocked, {} blocks read{}",
                                                s.sectors_unlocked,
                                                s.sectors_total,
                                                s.blocks_read,
                                                status,
                                            );
                                            // Report to the UI: blocks read, and OK
                                            // only if the file wrote cleanly.
                                            dump = Some((
                                                s.blocks_read,
                                                s.sectors_unlocked,
                                                !dump_failed.get(),
                                            ));
                                        }
                                        Err(e) => {
                                            log::warn!("NFC: sweep failed: {:?}", e);
                                            // Armed dump that could not complete -
                                            // report a failed result so the UI
                                            // disarms and the user can retry.
                                            dump = Some((0, 0, false));
                                        }
                                    }
                                    // The sweep leaves the card HALTED (encrypted
                                    // halt of the last sector, or a failed auth) -
                                    // do not send it a plain HLTA on top.
                                    crypto_halted = true;
                                }
                            }
                        } // end `if do_dump`
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
    // A card ANSWERED the reader (ATQA) but none could be identified -
    // an unsupported technology or a failed read. Report it as
    // "unrecognized" so the screen shows a clear state. A bare trip
    // where nothing answered (false trip, or the card lifted before the
    // poll) stays silent - no spurious "not recognized".
    if saw_atqa && presentations == 0 {
        EVENTS.send(SystemEvent::NfcProbe { card: None }).await;
    }
    Ok(ProbeOutcome { presentations, dump, saw_card: saw_atqa })
}

/// Always-on wake-up standby. Arms the chip's tag-detection mode,
/// parks on the amplitude IRQ (GPIO5) OR an arm/disarm command from the
/// UI, and on a trip powers the field up for one short poll, then
/// re-arms. Never returns.
///
/// Identify vs dump: the NFC screen's DUMP button signals
/// [`bus::NFC_COMMAND`] with `ArmDump`; leaving the screen (or a
/// completed dump) signals `Disarm`. While armed, the next card trip
/// runs the full read/sweep (`do_dump = true`) instead of a plain
/// identify, then reports the block count as `NfcDumpComplete` and
/// disarms. A false trip or a non-dumpable card tapped while armed
/// leaves the arm standing so the user can present the right card.
///
/// A wake lock is held ONLY while handling a trip, so the system
/// light-sleeps between taps. GPIO5 is a high-level light-sleep wake
/// source (Phase 2), so a tap wakes the system straight onto this
/// screen.
async fn wakeup_loop(
    drv: &St25r3916,
    spi: &mut SharedSpiDevice,
    irq: &mut Input<'static>,
    store: &'static bus::SharedStore,
) {
    use embassy_futures::select::{select, Either};

    // Armed by the UI's DUMP button; cleared on Disarm or after a dump
    // actually runs. Local to the task - the single place identify and
    // dump branch on.
    let mut dump_armed = false;

    loop {
        let reference = match arm_wakeup(drv, spi, WAKEUP_DELTA).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("NFC: enter wake-up mode failed: {:?} - retry in 5 s", e);
                Timer::after(Duration::from_secs(5)).await;
                continue;
            }
        };
        log::info!(
            "NFC: wake-up armed (delta {}, ref {}, dump {})",
            WAKEUP_DELTA,
            reference,
            if dump_armed { "on" } else { "off" },
        );

        // Park until either a tap (level-held IRQ - a card already
        // resting on the antenna returns at once) or an arm/disarm
        // command. A command only flips the flag and re-arms wake-up;
        // it never powers the field.
        match select(irq.wait_for_high(), bus::NFC_COMMAND.wait()).await {
            Either::Second(cmd) => {
                dump_armed = matches!(cmd, bus::NfcCommand::ArmDump);
                log::info!("NFC: dump {}", if dump_armed { "ARMED" } else { "disarmed" });
                continue;
            }
            Either::First(()) => {}
        }

        let _wake = bus::WakeHold::new();

        // Reading the interrupt registers drops the level-held IRQ.
        let trip = match drv.read_interrupts(spi) {
            Ok(i) => i.error_wup & regs::irq_error_wup::WUP_AMPLITUDE != 0,
            Err(e) => {
                log::warn!("NFC: IRQ read failed: {:?}", e);
                false
            }
        };
        if trip {
            log::info!(
                "NFC: wake-up trip - polling for a card (dump {})",
                if dump_armed { "on" } else { "off" },
            );
            match rf_probe(drv, spi, store, dump_armed).await {
                Ok(outcome) => {
                    log::info!("NFC: poll done ({} card(s))", outcome.presentations);
                    // A dump actually ran (armed + a Classic card swept):
                    // report blocks/ok to the UI and disarm. Nothing to
                    // report for a plain identify or a false trip.
                    if let Some((blocks, sectors, ok)) = outcome.dump {
                        EVENTS
                            .send(SystemEvent::NfcDumpComplete { blocks, sectors, ok })
                            .await;
                        dump_armed = false;
                    }
                    // A card is resting on the coil. Wait for it to be
                    // removed before re-arming wake-up mode - otherwise
                    // the next trip relights the field, resets the card
                    // to IDLE, and re-identifies it endlessly. Field is
                    // still lit from rf_probe; the wake lock is still
                    // held.
                    if outcome.saw_card {
                        wait_for_removal(spi).await;
                    }
                }
                Err(e) => log::error!("NFC: poll after trip failed: {:?}", e),
            }
        }

        drop(_wake);
        // No settle here: the next iteration's `arm_wakeup` collapses
        // the field, settles, and measures a fresh reference before
        // re-arming (and its 5 s error retry throttles failures).
    }
}

/// Arm wake-up mode with a freshly-measured amplitude reference, and
/// return that reference (for logging).
///
/// The chip's auto-averaged reference is seeded only once after
/// power-up and is never re-seeded on a re-arm, so a card detection
/// permanently drags it off the empty-antenna baseline and the sensor
/// free-runs (datasheet 4.2.5). Instead: collapse the field, let the
/// antenna go quiet, take a one-shot amplitude measurement (the Measure
/// Amplitude command self-enables the oscillator and raises DCT on
/// completion), and hand that value to `enter_wakeup_mode` as a FIXED
/// reference (am_ae = 0). Re-measured every arm, it tracks slow drift
/// yet a card can never contaminate it.
async fn arm_wakeup(
    drv: &St25r3916,
    spi: &mut SharedSpiDevice,
    delta: u8,
) -> Result<u8, Error<SpiErr>> {
    // Collapse the reader field and let the antenna go quiet so the
    // measurement reflects the empty baseline, not residual field.
    drv.field_off(spi)?;
    Timer::after(Duration::from_millis(WAKEUP_REARM_SETTLE_MS)).await;
    // One-shot empty-antenna amplitude measurement -> AD_RESULT.
    drv.direct_command(spi, regs::cmd::MEASURE_AMPLITUDE)?;
    wait_irq_timer_nfc(drv, spi, regs::irq_timer_nfc::DCT, 10).await?;
    let reference = drv.ad_result(spi)?;
    drv.enter_wakeup_mode(spi, delta, reference)?;
    Ok(reference)
}

/// Consecutive silent WUPA polls that mean the card is physically gone.
/// Must be >= 2: a card resting on the coil answers, is woken to READY,
/// and the next WUPA (no reply from READY) drops it back to IDLE - so a
/// resting card yields at most ONE silent poll before answering again,
/// never two in a row. A removed card is silent on every poll. 3 adds
/// margin against a stray missed reply; a rare false "removed" only
/// costs one extra re-identify (the loop re-arms, trips, and re-enters
/// this wait), never a spin.
const REMOVAL_SILENT_POLLS: u8 = 3;

/// WUPA presence-poll cadence during the removal wait.
const REMOVAL_POLL_GAP_MS: u64 = 120;

/// Wait for a just-handled card to leave the coil before the caller
/// re-arms wake-up mode.
///
/// Each wake-up re-trip relights the reader field, which power-cycles a
/// resting card back to IDLE so it answers REQA and re-identifies -
/// endlessly, for as long as it rests. So after handling a card, don't
/// return to wake-up standby yet: keep the field lit (rf_probe leaves it
/// up) and poll WUPA until the card is gone.
///
/// WUPA reaches a HALTed card, waking it to READY; a second WUPA from
/// READY gets no reply and drops it back to IDLE (ISO14443-3,
/// hardware-observed - see `Reader::identify`). So a resting card
/// alternates answer/silence and never reads silent twice running,
/// while a removed card reads silent every poll: hence
/// [`REMOVAL_SILENT_POLLS`] consecutive silences = removed.
///
/// The wake lock is held by the caller across this, so the system stays
/// awake with the field on while a card rests on the reader - correct,
/// that is active use. Re-arming happens only once the antenna is
/// empty.
async fn wait_for_removal(spi: &mut SharedSpiDevice) {
    let mut delay = Delay;
    let mut reader = Reader::new(&mut *spi, &mut delay);
    let mut silent: u8 = 0;
    loop {
        match reader.wake().await {
            Ok(_) => silent = 0,
            Err(_) => silent += 1,
        }
        if silent >= REMOVAL_SILENT_POLLS {
            log::info!("NFC: card removed - re-arming wake-up");
            return;
        }
        Timer::after(Duration::from_millis(REMOVAL_POLL_GAP_MS)).await;
    }
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
