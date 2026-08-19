#![no_std]
#![no_main]

//! Hardware smoke test (bring-up stages 0-2, hardware-verified
//! 2026-07-22): boot + I2C scan + PMU rails + XL9555 expander +
//! CO5300 color bars + CST9217 touch traces + DRV2605 haptics
//! exercise (diagnostics, ROM click, sequencer, RTP ramp) +
//! heartbeat.
//!
//! Kept as a standalone diagnostic bin - run with
//! `cargo run --bin smoke` - for isolating hardware questions from
//! system behavior. The real firmware is `main.rs` (the
//! `system_core::manager` port).

#[path = "../board.rs"]
mod board;

use drivers::display::co5300::color;
use drivers::pmu::{Config as PmuConfig, Pmu};
use drivers::touch::cst9217::Cst9217;
use drivers::touch::TouchEvent;
use drivers::xl9555::{Config as ExpanderConfig, Xl9555};
use embassy_time::{Delay, Duration, Timer};
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use firmware_hal::display::{init_display_lit, take_framebuffer, HEIGHT, WIDTH};

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(_spawner: embassy_executor::Spawner) {
    let peripherals = esp_hal::init(
        esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()),
    );

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);
    esp_println::logger::init_logger(log::LevelFilter::Info);
    log::info!("--- LilyGo T-Watch Ultra booting (smoke test) ---");

    let mut i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO3)
    .with_scl(peripherals.GPIO2);

    // Scan the shared bus. Expected ACKs (board.rs): 0x1A touch,
    // 0x20 expander, 0x28 IMU, 0x34 PMU, 0x51 RTC, 0x5A haptic.
    log::info!("I2C scan:");
    let mut found = 0u32;
    for addr in 0x08u8..=0x77 {
        let mut byte = [0u8; 1];
        if i2c.read(addr, &mut byte).is_ok() {
            log::info!("  0x{:02X} ACK", addr);
            found += 1;
        }
    }
    log::info!("I2C scan: {} device(s)", found);

    // PMU: set the boot-rail voltages, then enable ALDO1-4 + BLDO1-2
    // in one write. Rail map is in board.rs; voltages mirror the
    // vendor initPMU. DLDO1 (NFC) stays off until the NFC effort.
    let pmu = Pmu::new(PmuConfig::default());
    let chip_id = pmu
        .check_device(&mut i2c)
        .expect("AXP2101 not responding - halting");
    log::info!("AXP2101 online (chip id 0x{:02X})", chip_id);
    pmu.set_aldo1_voltage(&mut i2c, 3300).unwrap(); // SD card
    pmu.set_aldo2_voltage(&mut i2c, 3300).unwrap(); // Display VCI
    pmu.set_aldo3_voltage(&mut i2c, 3300).unwrap(); // LoRa
    pmu.set_aldo4_voltage(&mut i2c, 1800).unwrap(); // BHI260AP sensor
    pmu.set_bldo1_voltage(&mut i2c, 3300).unwrap(); // GPS
    pmu.set_bldo2_voltage(&mut i2c, 3300).unwrap(); // Speaker amp
    pmu.enable_all_rails(&mut i2c).unwrap();
    pmu.enable_all_adc(&mut i2c).unwrap();
    pmu.enable_battery_monitor(&mut i2c).unwrap();
    Timer::after(Duration::from_millis(20)).await; // rail settle

    // XL9555: release the gates. Vendor order - haptic enable,
    // display power enable, touch reset - each driven high.
    let expander = Xl9555::new(ExpanderConfig::default());
    expander
        .probe(&mut i2c)
        .expect("XL9555 not responding - halting");
    log::info!("XL9555 online");
    for pin in [board::EXP_DRV_EN, board::EXP_DISP_EN, board::EXP_TOUCH_RST] {
        expander.set_output(&mut i2c, pin, true).unwrap();
        Timer::after(Duration::from_millis(1)).await;
    }
    match expander.read_pin(&mut i2c, board::EXP_SD_DET) {
        Ok(level) => log::info!(
            "SD card {}",
            if level { "not inserted" } else { "inserted" }, // LOW = card present
        ),
        Err(_) => log::warn!("SD detect read failed"),
    }
    Timer::after(Duration::from_millis(20)).await; // display VCI settle

    // Display: same CO5300 init path as the other boards; only the
    // pins differ. Panel power is already up (ALDO2 + VCI_EN above).
    let fb = take_framebuffer();
    let mut display = init_display_lit(
        peripherals.SPI2,
        peripherals.GPIO40, // SCLK
        peripherals.GPIO38, // SIO0
        peripherals.GPIO39, // SIO1
        peripherals.GPIO42, // SIO2
        peripherals.GPIO45, // SIO3
        peripherals.GPIO41, // CS
        peripherals.DMA_CH0,
        Output::new(peripherals.GPIO37, Level::High, OutputConfig::default()),
        fb,
    )
    .await;

    // First pixels: one color bar per 50-row tile.
    const BARS: [u16; 4] = [color::RED, color::GREEN, color::BLUE, color::WHITE];
    let mut y: u16 = 0;
    let mut bar = 0usize;
    while y < HEIGHT {
        display.set_tile_y(y);
        display.fill_solid(0, y, WIDTH, display.fb_rows(), BARS[bar % BARS.len()]);
        display.flush_tile().await;
        y += display.fb_rows();
        bar += 1;
    }
    display.flush_pending().await;
    log::info!("First pixels pushed - panel should show color bars");

    // Touch: vendor reset pulse through the expander (low 20 ms,
    // high 60 ms), then identify the chip. Its reported matrix
    // resolution should match the panel (410x502).
    expander
        .write_pin(&mut i2c, board::EXP_TOUCH_RST, false)
        .unwrap();
    Timer::after(Duration::from_millis(20)).await;
    expander
        .write_pin(&mut i2c, board::EXP_TOUCH_RST, true)
        .unwrap();
    Timer::after(Duration::from_millis(60)).await;

    let touch_int = Input::new(
        peripherals.GPIO12,
        InputConfig::default().with_pull(Pull::Up),
    );
    let mut touch = Cst9217::new();
    match touch.init(&mut i2c, &mut Delay) {
        Ok(info) => log::info!(
            "CST{:04X} online: project 0x{:04X}, fw 0x{:08X}, matrix {}x{}",
            info.chip_id, info.project_id, info.fw_version, info.res_x, info.res_y,
        ),
        Err(()) => log::error!("CST9217 init failed"),
    }

    // Haptics: exercise every DRV2605 driver path on the real motor.
    // You should FEEL, in order: a brief diagnostic twitch, one
    // strong click, a double click, then a ~1.6 s buzz ramping from
    // 25 % to full - each step also logged.
    {
        use drivers::drv2605::{Config as DrvConfig, Drv2605, SequenceStep, RTP_MAX};
        let drv = Drv2605::new(DrvConfig::default());
        match drv.init(&mut i2c) {
            Ok(id) => {
                log::info!("Haptics: DRV2605 online (device id {})", id);

                match drv.run_diagnostics(&mut i2c, &mut Delay) {
                    Ok(true) => log::info!("Haptics: diagnostics PASS (motor present, no short)"),
                    Ok(false) => log::warn!("Haptics: diagnostics FAIL (open/short/back-EMF out of range)"),
                    Err(_) => log::warn!("Haptics: diagnostics I2C error"),
                }
                Timer::after(Duration::from_millis(300)).await;

                log::info!("Haptics: ROM effect 1 (strong click)");
                let _ = drv.play_effect(&mut i2c, 1);
                Timer::after(Duration::from_millis(500)).await;

                log::info!("Haptics: sequencer double-click (150 ms gap)");
                let _ = drv.play_sequence(
                    &mut i2c,
                    &[
                        SequenceStep::Effect(1),
                        SequenceStep::WaitMs10(15),
                        SequenceStep::Effect(1),
                    ],
                );
                Timer::after(Duration::from_millis(700)).await;

                log::info!("Haptics: RTP ramp 25% -> 100%");
                for amp in [RTP_MAX / 4, RTP_MAX / 2, RTP_MAX * 3 / 4, RTP_MAX] {
                    let _ = drv.buzz_on(&mut i2c, amp);
                    Timer::after(Duration::from_millis(400)).await;
                }
                // VBAT reading is only valid while actively driving.
                if let Ok(mv) = drv.vbat_mv(&mut i2c) {
                    log::info!("Haptics: VDD under full drive: {} mV", mv);
                }
                let _ = drv.buzz_off(&mut i2c);
                log::info!("Haptics: done (standby)");
            }
            Err(_) => log::error!("Haptics: DRV2605 init failed"),
        }
    }

    // Poll loop: touch traces at 50 Hz (INT-gated, plus is_pressed so
    // lift-off is never missed), heartbeat every 5 s.
    let mut ticks: u32 = 0;
    loop {
        Timer::after(Duration::from_millis(20)).await;
        ticks += 1;
        if touch_int.is_low() || touch.is_pressed() {
            match touch.read(&mut i2c) {
                TouchEvent::Pressed { x, y } => log::info!("touch: ({}, {})", x, y),
                TouchEvent::Released => log::info!("touch: released"),
                TouchEvent::None => {}
            }
        }
        if ticks % 250 == 0 {
            let mv = pmu.battery_voltage_mv(&mut i2c).unwrap_or(0);
            let pct = pmu.battery_percent(&mut i2c).unwrap_or(0);
            let inputs = expander.read_all(&mut i2c).unwrap_or(0);
            log::info!(
                "heartbeat: batt {} mV ({}%), XL9555 inputs 0x{:04X}",
                mv, pct, inputs,
            );
        }
    }
}
