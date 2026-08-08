//! LoRa boot canary - SX1262 behind the shared SPI bus.
//!
//! A ~1 s hardware check at boot: wake the radio from its cold-sleep
//! park, verify the SPI fingerprint and the TCXO path, re-park.
//! Screams in the log if a hardware fault or driver regression ever
//! kills the radio; emits no RF and holds the wake lock for barely
//! a second.
//!
//! This replaces the bring-up probe (60 s Meshtastic LongFast sniff
//! + TX beacons, both hardware-proven over the air 2026-08-06 and
//! retired 2026-08-08 - beaconing the public mesh on every boot had
//! no business staying). The full probe, including the verified
//! LongFast channel parameters (869.525 MHz, SF11/BW250, sync word
//! 0x24B4, header layout), lives in git history for the LoRa
//! session effort to draw on when it lands and claims DIO1.
//!
//! Hardware facts this leans on (schematic + vendor init, see the
//! board docs): the radio is an HPB16B3 module with a DIO3-powered
//! 1.6 V TCXO, so the crystal path fails at POR by design - the
//! XOSC_START_ERR latched before the TCXO is configured is expected
//! and cleared mid-sequence; the real proof the TCXO runs is the
//! chip reaching STDBY_XOSC afterwards.

use drivers::sx1262::{
    ms_to_steps, regs, ChipMode, Error, SleepConfig, StandbyClk, Sx1262, TcxoVoltage,
};
use embassy_time::{Duration, Timer};
use esp_hal::gpio::{Input, Output};
use system_core::bus;
use system_core::spi_bus::SharedSpiDevice;

/// TCXO start-up gate handed to SetDIO3AsTcxoCtrl. Generous for a
/// wearable-size TCXO; only stretches mode entries, costs nothing
/// in steady state.
const TCXO_DELAY_MS: u32 = 5;

type SpiErr = esp_hal::spi::Error;

#[embassy_executor::task]
pub async fn lora_task(
    mut spi: SharedSpiDevice,
    mut rst: Output<'static>,
    mut busy: Input<'static>,
) {
    // Let the boot storm (display init, firmware uploads) pass so
    // the log lines land readable and the shared bus is quiet.
    Timer::after(Duration::from_secs(3)).await;

    // Hardware sleep would freeze the executor mid-sequence; hold
    // the wake lock for the (short) check.
    let _wake = bus::WakeHold::new();

    let drv = Sx1262::new();

    // Hard reset out of the cold-sleep park: deterministic POR
    // state, superseding the CS-edge wake path. >=100 us low, then
    // the chip boots to STDBY_RC (~ms).
    rst.set_low();
    Timer::after(Duration::from_millis(1)).await;
    rst.set_high();
    Timer::after(Duration::from_millis(10)).await;

    match canary(&drv, &mut spi, &mut busy).await {
        Ok(()) => log::info!("LoRa: canary ok - SX1262 re-parked in cold sleep"),
        Err(e) => {
            log::error!("LoRa: canary failed: {:?}", e);
            // Best effort: never leave the radio awake and burning.
            let _ = drv.set_sleep(
                &mut spi,
                &mut busy,
                SleepConfig { warm_start: false, rtc_wake: false },
            );
        }
    }
    // Task ends here; the wake hold drops and normal sleep resumes.
}

async fn canary(
    drv: &Sx1262,
    spi: &mut SharedSpiDevice,
    busy: &mut Input<'static>,
) -> Result<(), Error<SpiErr>> {
    // 1. Alive + identity. The SX126x has no ID register (the data
    //    sheet says SX1261/2 cannot even be told apart over SPI),
    //    so the fingerprint is the LoRa sync word's reset value.
    let status = drv.status(spi, busy)?;
    let mut sync = [0u8; 2];
    drv.read_register(spi, busy, regs::reg::LORA_SYNC_WORD_MSB, &mut sync)?;
    if sync == regs::lora_sync_word::PRIVATE.to_be_bytes() {
        log::info!(
            "LoRa: SX1262 alive after reset ({:?}, sync-word fingerprint ok)",
            status.chip_mode,
        );
    } else {
        log::warn!(
            "LoRa: unexpected sync-word reset value {:02X}{:02X} (status {:?})",
            sync[0], sync[1], status,
        );
    }

    // 2. TCXO start-up. The POR crystal attempt has already latched
    //    XOSC_START_ERR - expected; hand DIO3 to the TCXO, clear,
    //    recalibrate against the real 32 MHz, and prove the
    //    oscillator by entering STDBY_XOSC.
    let errs = drv.device_errors(spi, busy)?;
    if errs & regs::op_error::XOSC_START_ERR == 0 {
        log::warn!(
            "LoRa: POR errors {:#06X} - expected XOSC_START_ERR (TCXO board)",
            errs,
        );
    }
    drv.set_dio3_tcxo(spi, busy, TcxoVoltage::V1_6, ms_to_steps(TCXO_DELAY_MS))?;
    drv.clear_device_errors(spi, busy)?;
    drv.calibrate(spi, busy, regs::calibrate::ALL)?;
    // Calibration holds BUSY ~3.5 ms; the next command's BUSY spin
    // absorbs it.
    drv.set_standby(spi, busy, StandbyClk::Xosc)?;
    let status = drv.status(spi, busy)?;
    let errs = drv.device_errors(spi, busy)?;
    if status.chip_mode == ChipMode::StandbyXosc && errs == 0 {
        log::info!("LoRa: TCXO running (STDBY_XOSC reached, no device errors)");
    } else {
        log::warn!(
            "LoRa: TCXO check odd - mode {:?}, errors {:#06X}",
            status.chip_mode, errs,
        );
    }

    // 3. Re-park through the driver: back to standby first (SetSleep
    //    is only legal from STDBY), then cold sleep - CS idles high
    //    on the shared-bus device, ~160 nA.
    drv.set_standby(spi, busy, StandbyClk::Rc)?;
    drv.set_sleep(spi, busy, SleepConfig { warm_start: false, rtc_wake: false })?;
    Ok(())
}
