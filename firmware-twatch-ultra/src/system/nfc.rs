//! NFC bring-up - ST25R3916 behind the shared SPI bus.
//!
//! One probe session at boot proves the host-to-chip path and the RF
//! front end: raise CS (the park holds it LOW against the unpowered
//! chip - it MUST be high before the rail comes up, or the line
//! back-feeds the dead chip), power DLDO1, run the datasheet's
//! power-up ritual, read the IC identity, adjust and measure the
//! regulators, then the RF proof: 13.56 MHz field on and ISO14443A
//! REQA polling for a few seconds - any Type A card (bank card,
//! transit card) answers with its 2-byte ATQA. Afterwards
//! everything is torn down: field off, chip to power-down, rail
//! off, CS back LOW. The session effort replaces this boot auto-run
//! with something command-driven.
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

use drivers::pmu::{Config as PmuConfig, Pmu};
use drivers::st25r3916::{regs, Error, St25r3916};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Output;
use esp_hal::spi::master::Config as SpiConfig;
use esp_hal::spi::Mode as SpiMode;
use esp_hal::time::Rate;
use system_core::bus::{self, SharedI2c};
use system_core::spi_bus::{SharedSpiBus, SharedSpiDevice};

/// How long the card-detect poll keeps the field up, and its retry
/// cadence. Generous enough to fish a card out of a wallet.
const CARD_POLL_SECS: u64 = 20;
const CARD_POLL_GAP_MS: u64 = 500;

type SpiErr = esp_hal::spi::Error;

#[embassy_executor::task]
pub async fn nfc_task(
    i2c_bus: &'static SharedI2c,
    spi_bus: &'static SharedSpiBus,
    mut cs: Output<'static>,
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

    match probe(&drv, &mut spi).await {
        Ok(()) => log::info!("NFC: probe complete"),
        Err(e) => log::error!("NFC: probe failed: {:?}", e),
    }

    // Teardown regardless of outcome: chip to power-down mode,
    // rail off, CS back to its parked LOW.
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

async fn probe(
    drv: &St25r3916,
    spi: &mut SharedSpiDevice,
) -> Result<(), Error<SpiErr>> {
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
    } else {
        log::warn!(
            "NFC: unexpected identity (type {:#07b}, rev {}) - aborting",
            id.ic_type, id.ic_rev,
        );
        return Ok(());
    }

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
        log::warn!("NFC: 27.12 MHz oscillator never stabilized - aborting");
        return Ok(());
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
    // Supply sanity: measure VDD through the chip's own A/D
    // (23.4 mV per LSB).
    drv.measure_power_supply(spi, 0)?;
    wait_irq_timer_nfc(drv, spi, regs::irq_timer_nfc::DCT, 5).await?;
    let vdd_mv = drv.ad_result(spi)? as u32 * 234 / 10;
    log::info!("NFC: VDD measures ~{} mV", vdd_mv);

    // 5. RF proof: ISO14443A reader field + REQA poll. Mode om=0001
    //    initiator ISO14443A, OOK modulation; 106 kbit/s both ways.
    //    REQA/ATQA needs no CRC handling (automatic per 4.4.4) and
    //    no FIFO preparation.
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
        "NFC: field on - present an ISO14443A card within {} s",
        CARD_POLL_SECS,
    );
    let deadline = Instant::now() + Duration::from_secs(CARD_POLL_SECS);
    let mut found = false;
    'poll: while Instant::now() < deadline {
        drv.direct_command(spi, regs::cmd::TRANSMIT_REQA)?;
        // ATQA arrives within ~100 us of the REQA end; poll the
        // (self-clearing) interrupt registers briefly.
        for _ in 0..5 {
            Timer::after(Duration::from_millis(2)).await;
            let irqs = drv.read_interrupts(spi)?;
            if irqs.main & regs::irq_main::RX_END != 0 {
                let st = drv.fifo_status(spi)?;
                let n = (st.bytes as usize).min(4);
                let mut atqa = [0u8; 4];
                drv.fifo_read(spi, &mut atqa[..n])?;
                log::info!(
                    "NFC: card detected! ATQA {:02X?} (RSSI am/pm {:?})",
                    &atqa[..n],
                    drv.rssi(spi)?,
                );
                found = true;
                break 'poll;
            }
        }
        Timer::after(Duration::from_millis(CARD_POLL_GAP_MS)).await;
    }
    if !found {
        log::info!("NFC: no card seen in the poll window");
    }
    Ok(())
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
