//! NFC boot canary - ST25R3916 behind the shared SPI bus.
//!
//! A sub-second hardware check at boot: raise CS (the park holds it
//! LOW against the unpowered chip - it MUST be high before the rail
//! comes up, or the line back-feeds the dead chip), power DLDO1,
//! run the datasheet's mandatory first contact, read the IC
//! identity, tear down (chip to power-down, rail off, CS back LOW).
//! Screams in the log if a hardware fault or driver regression ever
//! kills the reader; emits no field.
//!
//! This replaces the bring-up probe (oscillator/regulator ritual +
//! 20 s of ISO14443A field polling, card detection hardware-proven
//! 2026-08-06 and retired 2026-08-08 - a 20 s field hunt on every
//! boot earned nothing). The full probe lives in git history for
//! the NFC session effort to draw on.
//!
//! Ritual facts (DS12484 rev 8, section 4.1): the overheat
//! protection frame must be the first contact after every power-up
//! and Set Default; sup3V must be set because DLDO1 feeds the chip
//! 3.3 V while the reset default assumes 5 V.
//!
//! The chip is SPI mode 1 - its device seat carries that config and
//! the shared-bus wrapper applies it per transaction, so SD (mode 0)
//! traffic can interleave freely.

use drivers::pmu::{Config as PmuConfig, Pmu};
use drivers::st25r3916::{regs, Error, St25r3916};
use embassy_time::{Duration, Timer};
use esp_hal::gpio::Output;
use esp_hal::spi::master::Config as SpiConfig;
use esp_hal::spi::Mode as SpiMode;
use esp_hal::time::Rate;
use system_core::bus::{self, SharedI2c};
use system_core::spi_bus::{SharedSpiBus, SharedSpiDevice};

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
        Ok(()) => log::info!("NFC: canary complete"),
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

    // Identity readable = SPI path, power-up ritual and silicon all
    // good - the canary's job is done. Oscillator, regulators and
    // the ISO14443A field were proven during bring-up and return
    // with the session effort.
    Ok(())
}
