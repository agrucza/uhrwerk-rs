//! LoRa bring-up - SX1262 behind the shared SPI bus.
//!
//! Phase B of the LoRa effort: prove the RF path over the air by
//! receiving live Meshtastic traffic. One session at boot walks the
//! radio from its cold-sleep park through reset, identity
//! fingerprint and TCXO start-up (the Phase A sequence, hardware-
//! verified 2026-08-06), then tunes to the Meshtastic LongFast
//! EU868 channel and sniffs for a bounded window, logging every
//! received packet's size, RSSI and SNR. A nearby Meshtastic node
//! provides the transmitter - sending any message from its app puts
//! a packet on the air. Nothing is transmitted here; TX over the
//! air comes with the two-way session effort. Afterwards the radio
//! re-parks into cold sleep through the driver.
//!
//! Channel facts (verified against Meshtastic firmware + docs):
//! LongFast on EU868 has exactly one slot, centered 869.525 MHz;
//! SF11 / BW250 / CR4:5, 16-symbol preamble, explicit header, CRC
//! on. Meshtastic sets the one-byte sync word 0x2B through
//! RadioLib, which spreads it across the SX126x register pair with
//! 0x44 control nibbles - on the wire that is 0x24B4, and writing
//! anything else means hearing silence.
//!
//! Hardware facts this leans on (schematic + vendor init, see the
//! board docs): the radio is an HPB16B3 module with a DIO3-powered
//! 1.6 V TCXO, so the crystal path fails at POR by design - the
//! XOSC_START_ERR latched before the TCXO is configured is expected
//! and cleared mid-sequence; the real proof the TCXO runs is the
//! chip reaching STDBY_XOSC afterwards.

use drivers::sx1262::{
    regs, ChipMode, Error, LoRaBw, LoRaCr, LoRaModParams, LoRaPacketParams, LoRaSf,
    PacketType, SleepConfig, StandbyClk, Sx1262, TcxoVoltage, ms_to_steps,
    RX_CONTINUOUS,
};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::{Input, Output};
use system_core::bus;
use system_core::spi_bus::SharedSpiDevice;

/// TCXO start-up gate handed to SetDIO3AsTcxoCtrl. Generous for a
/// wearable-size TCXO; only stretches mode entries, costs nothing
/// in steady state.
const TCXO_DELAY_MS: u32 = 5;

/// Meshtastic LongFast on EU868 - the band's single slot.
const LONGFAST_FREQ_HZ: u32 = 869_525_000;
/// Meshtastic preamble length in symbols.
const LONGFAST_PREAMBLE: u16 = 16;
/// Meshtastic's one-byte sync word 0x2B in SX126x register form
/// (RadioLib interleaves the 0x44 control nibbles).
const LONGFAST_SYNC_WORD: u16 = 0x24B4;

/// How long the boot sniff listens before re-parking the radio.
const SNIFF_WINDOW_SECS: u64 = 180;
/// IRQ poll cadence during the sniff (no DIO1 wiring yet - the
/// two-way session effort claims the interrupt line).
const SNIFF_POLL_MS: u64 = 100;

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
        Ok(()) => log::info!("LoRa: session complete - SX1262 re-parked in cold sleep"),
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

    // 3. Post-POR errata, then tune to the Meshtastic LongFast
    //    EU868 channel (module docs carry the parameter provenance).
    drv.apply_tx_clamp_workaround(spi, busy)?;
    drv.set_standby(spi, busy, StandbyClk::Rc)?;
    drv.set_packet_type(spi, busy, PacketType::LoRa)?;
    drv.calibrate_image(spi, busy, regs::image_band::MHZ_863_870)?;
    drv.set_rf_frequency_hz(spi, busy, LONGFAST_FREQ_HZ)?;
    drv.set_lora_modulation(
        spi,
        busy,
        LoRaModParams {
            sf: LoRaSf::Sf11,
            bw: LoRaBw::Khz250,
            // SF11 at BW250 is an 8.2 ms symbol - under the 16.38 ms
            // LDRO threshold, so Meshtastic runs without it.
            cr: LoRaCr::Cr4_5,
            low_data_rate_opt: false,
        },
    )?;
    drv.set_lora_packet_params(
        spi,
        busy,
        LoRaPacketParams {
            preamble_symbols: LONGFAST_PREAMBLE,
            implicit_header: false,
            // Maximum the receiver accepts; actual length comes from
            // each packet's explicit header.
            payload_len: 255,
            crc_on: true,
            invert_iq: false,
        },
    )?;
    drv.set_lora_sync_word(spi, busy, LONGFAST_SYNC_WORD)?;
    drv.set_buffer_base(spi, busy, 0, 0)?;
    drv.set_rx_gain_boosted(spi, busy, true)?;

    // 4. Sniff: RX continuous, polling the latched IRQ flags (DIO1
    //    stays unclaimed until the two-way session effort). RxDone
    //    fires per packet; CRC/header errors are logged too - even
    //    those prove RF reception.
    drv.set_dio_irq_params(
        spi,
        busy,
        regs::irq::RX_DONE | regs::irq::CRC_ERR | regs::irq::HEADER_ERR,
        0,
        0,
        0,
    )?;
    drv.set_rx(spi, busy, RX_CONTINUOUS)?;
    log::info!(
        "LoRa: sniffing Meshtastic LongFast EU868 (869.525 MHz, SF11/BW250) for {} s - send a message from the app",
        SNIFF_WINDOW_SECS,
    );
    let deadline = Instant::now() + Duration::from_secs(SNIFF_WINDOW_SECS);
    let mut packets = 0u32;
    while Instant::now() < deadline {
        Timer::after(Duration::from_millis(SNIFF_POLL_MS)).await;
        let irq = drv.irq_status(spi, busy)?;
        if irq == 0 {
            continue;
        }
        drv.clear_irq(spi, busy, irq)?;
        if irq & regs::irq::RX_DONE != 0 {
            packets += 1;
            let (len, offset) = drv.rx_buffer_status(spi, busy)?;
            let mut payload = [0u8; 255];
            let head = &mut payload[..(len as usize).min(255)];
            drv.read_buffer(spi, busy, offset, head)?;
            let ps = drv.lora_packet_status(spi, busy)?;
            // Meshtastic radio header: dest(4) sender(4) id(4)
            // flags(1) chan(1)... - the first 16 bytes identify the
            // sender without any decoding.
            log::info!(
                "LoRa: RX {} B, RSSI {} dBm, SNR {} dB, head {:02X?}",
                len,
                ps.rssi_pkt_dbm,
                ps.snr_pkt_db_x4 as i16 / 4,
                &head[..head.len().min(16)],
            );
        }
        if irq & (regs::irq::CRC_ERR | regs::irq::HEADER_ERR) != 0 {
            log::warn!(
                "LoRa: damaged packet (irq {:#06X}) - RF reception happening, demod struggled",
                irq,
            );
        }
    }
    log::info!(
        "LoRa: sniff window over - {} packet(s) received",
        packets,
    );

    // 5. Re-park through the driver: back to standby first (SetSleep
    //    is only legal from STDBY), then cold sleep - CS idles high
    //    on the shared-bus device, ~160 nA.
    drv.set_standby(spi, busy, StandbyClk::Rc)?;
    drv.set_sleep(spi, busy, SleepConfig { warm_start: false, rtc_wake: false })?;
    Ok(())
}
