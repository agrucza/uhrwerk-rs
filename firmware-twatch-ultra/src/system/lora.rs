//! LoRa bring-up - SX1262 behind the shared SPI bus.
//!
//! Phase A of the LoRa effort: prove the host-to-chip path on real
//! hardware. One session at boot walks the radio from its cold-sleep
//! park through reset, identity fingerprint, TCXO start-up,
//! calibration and a full EU868 LoRa configuration dry run, then
//! parks it again - this time through the driver instead of the boot
//! bit-bang. No RF is emitted (nothing enters TX). The air-link
//! effort replaces the boot auto-run with a command-driven session
//! loop.
//!
//! Hardware facts this leans on (schematic + vendor init, see the
//! board docs): the radio is an HPB16B3 module with a DIO3-powered
//! 1.6 V TCXO, so the crystal path fails at POR by design - the
//! XOSC_START_ERR latched before the TCXO is configured is expected
//! and cleared mid-sequence; the real proof the TCXO runs is the
//! chip reaching STDBY_XOSC afterwards.

use drivers::sx1262::{
    regs, ChipMode, Error, LoRaBw, LoRaCr, LoRaModParams, LoRaPacketParams, LoRaSf,
    PaPreset, PacketType, RampTime, SleepConfig, StandbyClk, Sx1262, TcxoVoltage,
    ms_to_steps,
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
    // the wake lock for the session like GPS sync does.
    let _wake = bus::WakeHold::new();

    let drv = Sx1262::new();

    // Hard reset out of the cold-sleep park: deterministic POR
    // state, superseding the CS-edge wake path. >=100 us low, then
    // the chip boots to STDBY_RC (~ms).
    rst.set_low();
    Timer::after(Duration::from_millis(1)).await;
    rst.set_high();
    Timer::after(Duration::from_millis(10)).await;

    match bringup(&drv, &mut spi, &mut busy).await {
        Ok(()) => log::info!("LoRa: bring-up complete - SX1262 re-parked in cold sleep"),
        Err(e) => {
            log::error!("LoRa: bring-up failed: {:?}", e);
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

async fn bringup(
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
    //    XOSC_START_ERR - log it as the expected fingerprint, hand
    //    DIO3 to the TCXO, clear, recalibrate everything against
    //    the real 32 MHz, and prove the oscillator by entering
    //    STDBY_XOSC.
    let errs = drv.device_errors(spi, busy)?;
    log::info!(
        "LoRa: device errors at POR: {:#06X}{}",
        errs,
        if errs & regs::op_error::XOSC_START_ERR != 0 {
            " (XOSC_START_ERR expected - TCXO not configured yet)"
        } else {
            ""
        },
    );
    drv.set_dio3_tcxo(spi, busy, TcxoVoltage::V1_6, ms_to_steps(TCXO_DELAY_MS))?;
    drv.clear_device_errors(spi, busy)?;
    drv.calibrate(spi, busy, regs::calibrate::ALL)?;
    // Calibration holds BUSY ~3.5 ms; the next command's BUSY spin
    // absorbs it. Then the acid test: STDBY_XOSC only works if the
    // TCXO actually delivers 32 MHz.
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

    // 3. Post-POR errata + full EU868 configuration dry run. No TX
    //    happens; this exercises every configuration verb the
    //    air-link effort will use, with the radio absorbing or
    //    rejecting each one.
    drv.apply_tx_clamp_workaround(spi, busy)?;
    drv.set_standby(spi, busy, StandbyClk::Rc)?;
    drv.set_packet_type(spi, busy, PacketType::LoRa)?;
    drv.calibrate_image(spi, busy, regs::image_band::MHZ_863_870)?;
    drv.set_rf_frequency_hz(spi, busy, 868_100_000)?;
    drv.set_pa_preset(spi, busy, PaPreset::Dbm22)?;
    drv.set_tx_params(spi, busy, 14, RampTime::Us200)?;
    drv.set_lora_modulation(
        spi,
        busy,
        LoRaModParams {
            sf: LoRaSf::Sf9,
            bw: LoRaBw::Khz125,
            cr: LoRaCr::Cr4_5,
            low_data_rate_opt: false,
        },
    )?;
    drv.set_lora_packet_params(
        spi,
        busy,
        LoRaPacketParams {
            preamble_symbols: 8,
            implicit_header: false,
            payload_len: 16,
            crc_on: true,
            invert_iq: false,
        },
    )?;
    drv.set_lora_sync_word(spi, busy, regs::lora_sync_word::PRIVATE)?;
    drv.set_buffer_base(spi, busy, 0, 0)?;
    let status = drv.status(spi, busy)?;
    let errs = drv.device_errors(spi, busy)?;
    log::info!(
        "LoRa: EU868 config dry run done ({:?}, errors {:#06X})",
        status.chip_mode, errs,
    );

    // 4. Re-park through the driver: cold sleep, CS idles high on
    //    the shared-bus device, ~160 nA until the air-link effort
    //    wakes it for real.
    drv.set_sleep(spi, busy, SleepConfig { warm_start: false, rtc_wake: false })?;
    Ok(())
}
