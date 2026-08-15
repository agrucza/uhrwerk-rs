#![no_std]
#![no_main]

//! LilyGo T-Watch Ultra firmware - the full `system_core` port.
//!
//! Mirrors `firmware-s3/src/main.rs` in role: this bin owns the pins,
//! the `Bringup` construction seam and the `Board` impl; the shared
//! `system_core::manager::run` drives the canonical boot sequence and
//! event loop. Board deltas from the S3: touch is a CST92xx whose
//! reset runs through the XL9555 expander (built here, handed to the
//! shared task via `TouchTaskState::with_driver`); the IMU is a
//! BHI260AP hub driven through the AnyImu seam (its firmware upload
//! and discovery run staged inside the shared IMU task);
//! haptics are a DRV2605 dispatched through a bin-local task; and
//! audio is codec-less: a MAX98357A speaker on standard I2S TX plus
//! a T3902 PDM mic on the same I2S0's RX unit in hardware PDM-to-PCM
//! mode (see `audio_task` / `tune_pdm_rx` - no codec chips, no MCLK,
//! no audio I2C). The `smoke` bin (src/bin/smoke.rs) is the
//! standalone hardware diagnostic from bring-up.

extern crate alloc;

mod board;
mod system;
// Throwaway WiFi credentials (gitignored; template next to it).
// Dies when on-device provisioning lands.
mod wifi_secrets;

use crate::system::power::{LoraSleepPins, TwatchUltraBoard};
use drivers::touch::cst9217::Cst9217;
use drivers::touch::AnyTouch;
use drivers::xl9555::{Config as ExpanderConfig, Xl9555};
use system_core::display::{init_display, Display};
use system_core::flash_fs::FlashRegion;
use system_core::manager::{run, Bringup};
use system_core::storage::Store;
use system_core::tasks::{
    boot_button::BootButtonTaskState,
    imu::ImuTaskState,
    power::PowerTaskState,
    rtc::RtcTaskState,
    touch::TouchTaskState,
};
use embassy_time::{Delay, Duration, Timer};
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull, WakeEvent};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::peripherals as p;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode as SpiMode;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::Blocking;

esp_bootloader_esp_idf::esp_app_desc!();

/// T-Watch Ultra boot-construction seam. Holds the raw peripheral
/// tokens (esp-hal singletons can't be partial-moved through
/// `&mut self`, so each is an `Option` `.take()`-n once by its
/// `Bringup` method).
struct TwatchUltraBringup {
    i2c0: Option<p::I2C0<'static>>,
    i2c_sda: Option<p::GPIO3<'static>>,
    i2c_scl: Option<p::GPIO2<'static>>,
    pmu_irq: Option<p::GPIO7<'static>>,
    // Shared-SPI chip selects held deselected (see TwatchUltraBoard).
    lora_cs: Option<p::GPIO36<'static>>,
    nfc_cs: Option<p::GPIO4<'static>>,
    // SX1262 reset/busy - reborrowed once at boot for the cold-sleep
    // park, then consumed by the LoRa task at spawn.
    lora_rst: Option<p::GPIO47<'static>>,
    lora_busy: Option<p::GPIO48<'static>>,
    spi2: Option<p::SPI2<'static>>,
    lcd_sclk: Option<p::GPIO40<'static>>,
    lcd_sio0: Option<p::GPIO38<'static>>,
    lcd_sio1: Option<p::GPIO39<'static>>,
    lcd_sio2: Option<p::GPIO42<'static>>,
    lcd_sio3: Option<p::GPIO45<'static>>,
    lcd_cs: Option<p::GPIO41<'static>>,
    dma_ch0: Option<p::DMA_CH0<'static>>,
    psram: Option<p::PSRAM<'static>>,
    // Filled by make_display (PSRAM is mapped there), handed to the
    // orchestrator via take_fb_canvas.
    fb_canvas: Option<&'static mut [u8]>,
    lcd_reset: Option<p::GPIO37<'static>>,
    // Speaker (MAX98357A, standard I2S TX) and mic (T3902 PDM, RX in
    // hardware PDM-to-PCM mode) - both on I2S0, see `audio_task`.
    i2s0: Option<p::I2S0<'static>>,
    dma_ch1: Option<p::DMA_CH1<'static>>,
    spk_bclk: Option<p::GPIO9<'static>>,
    spk_ws: Option<p::GPIO10<'static>>,
    spk_dout: Option<p::GPIO11<'static>>,
    mic_clk: Option<p::GPIO17<'static>>,
    mic_din: Option<p::GPIO18<'static>>,
    // GPS (MIA-M10Q) - rail-gated UART sessions, see system/gps.rs.
    uart1: Option<p::UART1<'static>>,
    gps_tx: Option<p::GPIO43<'static>>,
    gps_rx: Option<p::GPIO44<'static>>,
    lcd_te: Option<p::GPIO6<'static>>,
    touch_int: Option<p::GPIO12<'static>>,
    btn_boot: Option<p::GPIO0<'static>>,
    rtc_int: Option<p::GPIO1<'static>>,
    imu_int: Option<p::GPIO8<'static>>,
    flash: Option<p::FLASH<'static>>,
    spi3: Option<p::SPI3<'static>>,
    sd_sck: Option<p::GPIO35<'static>>,
    sd_mosi: Option<p::GPIO34<'static>>,
    sd_miso: Option<p::GPIO33<'static>>,
    sd_cs: Option<p::GPIO21<'static>>,
    /// The shared SPI3 bus (SD + SX1262 + NFC behind their own chip
    /// selects), parked in the spi_bus static by make_store. Kept
    /// here so the LoRa task can mint its own device handle.
    spi_bus: Option<&'static system_core::spi_bus::SharedSpiBus>,
    /// SX1262 chip select after the cold-sleep park (held HIGH),
    /// waiting for the LoRa task's device seat.
    lora_cs_out: Option<Output<'static>>,
    /// ST25R3916 chip select (held LOW against the unpowered chip),
    /// waiting for the NFC task - which must raise it BEFORE
    /// enabling DLDO1.
    nfc_cs_out: Option<Output<'static>>,
    lpwr: Option<p::LPWR<'static>>,
    // WiFi radio - session-gated NTP sync, see system-core's wifi
    // module.
    wifi: Option<p::WIFI<'static>>,
}

impl Bringup for TwatchUltraBringup {
    type Board = TwatchUltraBoard;

    fn make_i2c(&mut self) -> I2c<'static, Blocking> {
        I2c::new(
            self.i2c0.take().unwrap(),
            I2cConfig::default().with_frequency(Rate::from_khz(400)),
        )
        .unwrap()
        .with_sda(self.i2c_sda.take().unwrap())
        .with_scl(self.i2c_scl.take().unwrap())
    }

    fn make_power(
        &mut self,
        i2c: &mut I2c<'static, Blocking>,
    ) -> (Self::Board, PowerTaskState) {
        // External 10K pull-up on the PMU IRQ line; armed for wake so
        // a PWR-button press wakes the watch from light sleep.
        let mut pmu_irq = Input::new(
            self.pmu_irq.take().unwrap(),
            InputConfig::default().with_pull(Pull::None),
        );
        let _ = pmu_irq.wakeup_enable(true, WakeEvent::LowLevel);

        // Constructed here for the cold-sleep park, then kept in the
        // bringup struct: the LoRa task's shared-bus device seat
        // takes it over. Held HIGH throughout - deselected AND
        // keeping the parked chip asleep.
        let mut lora_cs =
            Output::new(self.lora_cs.take().unwrap(), Level::High, OutputConfig::default());
        // NFC CS idles LOW: the ST25R3916's rail (DLDO1) is off, and
        // a driven-high line would back-feed the unpowered chip
        // through its input-protection diodes. Kept in the bringup
        // struct; the NFC task raises it BEFORE powering the rail.
        self.nfc_cs_out = Some(Output::new(
            self.nfc_cs.take().unwrap(),
            Level::Low,
            OutputConfig::default(),
        ));

        // One-shot SX1262 cold-sleep pins. SCK/MOSI are reborrowed -
        // make_store still builds the shared SPI bus from the same
        // pins later; RST/BUSY go to the LoRa task at spawn.
        let lora = LoraSleepPins {
            rst: Output::new(
                self.lora_rst.as_mut().unwrap().reborrow(),
                Level::High,
                OutputConfig::default(),
            ),
            busy: Input::new(
                self.lora_busy.as_mut().unwrap().reborrow(),
                InputConfig::default().with_pull(Pull::None),
            ),
            sck: Output::new(
                self.sd_sck.as_mut().unwrap().reborrow(),
                Level::Low,
                OutputConfig::default(),
            ),
            mosi: Output::new(
                self.sd_mosi.as_mut().unwrap().reborrow(),
                Level::Low,
                OutputConfig::default(),
            ),
        };

        let (board, pmu) = TwatchUltraBoard::init(i2c, pmu_irq, &mut lora_cs, lora)
            .expect("PMU init failed - halting");
        self.lora_cs_out = Some(lora_cs);

        (board, PowerTaskState::new(pmu))
    }

    async fn make_display(
        &mut self,
        config: &app_core::config::Config,
    ) -> Display<'static> {
        // 8 MB PSRAM, mapped here solely to back the full-panel
        // canvas - the heap stays in internal SRAM, and rendering stays
        // in the internal staging tile (`take_framebuffer`); the render
        // loop mirrors pushed tiles into the canvas. This board's part
        // is the APS6404L: QUAD-SPI SDR, not the octal chip the
        // Waveshare S3 carries (see board.rs) - quad mode is required.
        // The mirror is sequential writes, the access pattern quad
        // PSRAM handles fine; per-pixel rendering into it would not be.
        let psram = esp_hal::psram::Psram::new(
            self.psram.take().unwrap(),
            esp_hal::psram::PsramConfig {
                mode: esp_hal::psram::PsramMode::QuadSpi,
                ram_frequency: esp_hal::psram::SpiRamFreq::Freq80m,
                ..Default::default()
            },
        );
        self.fb_canvas = Some(firmware_hal::display::take_psram_canvas(&psram));

        let fb: &'static mut [u8] = firmware_hal::display::take_framebuffer();
        init_display(
            self.spi2.take().unwrap(),
            self.lcd_sclk.take().unwrap(),
            self.lcd_sio0.take().unwrap(),
            self.lcd_sio1.take().unwrap(),
            self.lcd_sio2.take().unwrap(),
            self.lcd_sio3.take().unwrap(),
            self.lcd_cs.take().unwrap(),
            self.dma_ch0.take().unwrap(),
            Output::new(self.lcd_reset.take().unwrap(), Level::High, OutputConfig::default()),
            fb,
            // Config-first boot: the panel's first lit frame is
            // already at the stored brightness.
            config.display.brightness_active,
        )
        .await
    }

    fn take_fb_canvas(&mut self) -> Option<&'static mut [u8]> {
        self.fb_canvas.take()
    }

    fn make_lcd_te(&mut self) -> Option<Input<'static>> {
        Some(Input::new(
            self.lcd_te.take().unwrap(),
            InputConfig::default().with_pull(Pull::None),
        ))
    }

    async fn make_input(
        &mut self,
        i2c: &mut I2c<'static, Blocking>,
    ) -> (TouchTaskState<'static>, BootButtonTaskState<'static>) {
        let mut touch_int = Input::new(
            self.touch_int.take().unwrap(),
            InputConfig::default().with_pull(Pull::Up),
        );
        let mut boot_btn = Input::new(
            self.btn_boot.take().unwrap(),
            InputConfig::default().with_pull(Pull::Up),
        );
        let _ = touch_int.wakeup_enable(true, WakeEvent::LowLevel);
        let _ = boot_btn.wakeup_enable(true, WakeEvent::LowLevel);

        // The CST92xx reset line runs through the XL9555, so the
        // shared `TouchTaskState::init` (host-GPIO reset) doesn't
        // apply here: pulse the expander pin (vendor timing), probe
        // the chip, and hand the ready driver to the shared task.
        let expander = Xl9555::new(ExpanderConfig::default());
        let _ = expander.write_pin(i2c, board::EXP_TOUCH_RST, false);
        Timer::after(Duration::from_millis(20)).await;
        let _ = expander.write_pin(i2c, board::EXP_TOUCH_RST, true);
        Timer::after(Duration::from_millis(60)).await;

        let mut cst = Cst9217::new();
        match cst.init(i2c, &mut Delay) {
            Ok(info) => log::info!(
                "Touch: CST{:04X}, fw 0x{:08X}, matrix {}x{}",
                info.chip_id, info.fw_version, info.res_x, info.res_y,
            ),
            Err(()) => log::error!("Touch: CST92xx init failed"),
        }

        let touch = TouchTaskState::with_driver(AnyTouch::Cst92xx(cst), touch_int);
        (touch, BootButtonTaskState::new(boot_btn))
    }

    fn make_store(&mut self) -> Store<'static> {
        // The Store always holds the SD's seat on the bus; card
        // presence is the manager's business (Board::sd_detect gates
        // every probe, so an empty slot never pays the
        // embedded-sdmmc retry stall). Unconditional ownership is
        // what makes hotplug work from a cardless boot.
        //
        // The bus itself is SHARED: SD, SX1262 and NFC hang off the
        // same SPI3 pins behind their own chip selects, so the bus
        // driver goes into the spi_bus static and each consumer
        // gets a CS-scoped SharedSpiDevice. Built here (after
        // make_power - the radio park bit-bangs these pins first).
        // 400 kHz mode 0: SD-identification-safe, and well within
        // the SX1262's SPI range.
        let spi = Spi::new(self.spi3.take().unwrap(), spi3_mode0_config())
            .unwrap()
        .with_sck(self.sd_sck.take().unwrap())
        .with_mosi(self.sd_mosi.take().unwrap())
        .with_miso(self.sd_miso.take().unwrap());
        let bus = system_core::spi_bus::init_shared_bus(spi);
        // Kept for the radio tasks: each gets its own device seat on
        // the same bus.
        self.spi_bus = Some(bus);

        // Mixed-mode bus (NFC is SPI mode 1), so every device
        // carries its explicit config - see SharedSpiDevice docs.
        let region = FlashRegion::new(board::FLASH_FS_START, board::FLASH_FS_SIZE);
        Store::init(
            self.flash.take().unwrap(),
            region,
            system_core::spi_bus::SharedSpiDevice::with_config(
                bus,
                Output::new(self.sd_cs.take().unwrap(), Level::High, OutputConfig::default()),
                spi3_mode0_config(),
            ),
        )
    }

    async fn make_sensors(
        &mut self,
        i2c: &mut I2c<'static, Blocking>,
    ) -> (RtcTaskState<'static>, ImuTaskState<'static>) {
        let mut rtc_int = Input::new(
            self.rtc_int.take().unwrap(),
            InputConfig::default().with_pull(Pull::Up),
        );
        let _ = rtc_int.wakeup_enable(true, WakeEvent::LowLevel);
        let rtc_state = RtcTaskState::init(Some(rtc_int), i2c);

        // This board's IMU is the BHI260AP hub: its firmware upload
        // and sensor discovery run inside the shared IMU task via
        // the AnyImu seam, staged chunk by chunk over the shared bus
        // (the UI boots without waiting; the IMU comes alive a few
        // seconds later). The mounting remap is the IDENTITY:
        // measured on hardware (2026-07-29, three-pose gravity
        // check) the chip sits in the datasheet-default frame
        // relative to THIS firmware's screen orientation. The
        // vendor's remap (LilyGoLib: TOP_LAYER_BOTTOM_RIGHT_CORNER,
        // 180 deg around Z) serves LilyGo's display rotation, which
        // is 180 deg from ours - do not copy it. The wake-gesture
        // algorithm evaluates this device frame and is blind when
        // it is wrong.
        let imu = ImuTaskState::new(
            drivers::imu::AnyImu::Bhi260(drivers::imu::bhi260_imu::Bhi260Imu::new(
                drivers::bhi260::Bhi260::pack_orientation_matrix([
                    1, 0, 0, //
                    0, 1, 0, //
                    0, 0, 1,
                ]),
                // Wear calibration from the guided measurement
                // session (2026-07-31, this user's wrist): the
                // viewing pose holds gravity on +Y - from ~3900
                // (formal, face near-vertical) down to ~2700 (lazy
                // glance, ~45 deg) - while every desk pose stays at
                // or below ~1100 and arm-hang sits on -X. The
                // enter/exit thresholds are centered in that gap;
                // the band between them is hysteresis.
                drivers::imu::WearCalibration {
                    axis: [0, 1, 0],
                    enter_lsb: 2400,
                    exit_lsb: 1800,
                },
            )),
            {
                // The BHI260AP interrupt line is a wake source: with
                // the AP-suspend contract, only wake-up sensor
                // events assert it during sleep (active high, level;
                // meta events are disabled for the wake FIFO), so a
                // wrist-raise wakes in milliseconds instead of
                // waiting for the next heartbeat poll.
                let mut imu_int = Input::new(
                    self.imu_int.take().unwrap(),
                    InputConfig::default().with_pull(Pull::Down),
                );
                let _ = imu_int.wakeup_enable(true, WakeEvent::HighLevel);
                imu_int
            },
        );
        (rtc_state, imu)
    }

    fn make_rtc_ctrl(&mut self) -> esp_hal::rtc_cntl::Rtc<'static> {
        esp_hal::rtc_cntl::Rtc::new(self.lpwr.take().unwrap())
    }

    /// Spawns the two bin-local dispatchers: haptics (DRV2605 -
    /// needs the shared I2C bus) and the audio task (MAX98357A
    /// speaker + T3902 PDM mic, both on I2S0 - see `audio_task`).
    /// Buffer sizes mirror the codec boards: the RX ring must match
    /// system-core's `CAPTURE_CHUNK_BYTES` pop-buffer contract, the
    /// TX ring its `TX_RING_BYTES`.
    fn spawn_audio(
        &mut self,
        spawner: embassy_executor::Spawner,
        i2c_bus: &'static system_core::bus::SharedI2c,
    ) {
        spawner.spawn(crate::system::haptics::haptics_task(i2c_bus).unwrap());
        spawner.spawn(
            crate::system::gps::gps_task(
                i2c_bus,
                self.uart1.take().unwrap(),
                self.gps_tx.take().unwrap(),
                self.gps_rx.take().unwrap(),
            )
            .unwrap(),
        );
        // LoRa bring-up: its device seat on the shared bus (built by
        // make_store, which ran before this hook) + the park-era
        // RST/BUSY pins.
        spawner.spawn(
            crate::system::lora::lora_task(
                system_core::spi_bus::SharedSpiDevice::with_config(
                    self.spi_bus.expect("spi_bus built in make_store"),
                    self.lora_cs_out.take().unwrap(),
                    spi3_mode0_config(),
                ),
                Output::new(
                    self.lora_rst.take().unwrap(),
                    Level::High,
                    OutputConfig::default(),
                ),
                Input::new(
                    self.lora_busy.take().unwrap(),
                    InputConfig::default().with_pull(Pull::None),
                ),
            )
            .unwrap(),
        );
        // NFC bring-up probe: raw CS handed over (still parked LOW);
        // the task raises it before powering DLDO1 and builds its
        // own mode-1 device seat.
        spawner.spawn(
            crate::system::nfc::nfc_task(
                i2c_bus,
                self.spi_bus.expect("spi_bus built in make_store"),
                self.nfc_cs_out.take().unwrap(),
            )
            .unwrap(),
        );
        let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) =
            esp_hal::dma_circular_buffers!(32768, 4096);
        spawner.spawn(
            audio_task(
                self.i2s0.take().unwrap(),
                self.dma_ch1.take().unwrap(),
                self.spk_bclk.take().unwrap(),
                self.spk_ws.take().unwrap(),
                self.spk_dout.take().unwrap(),
                self.mic_clk.take().unwrap(),
                self.mic_din.take().unwrap(),
                tx_buffer,
                tx_descriptors,
                rx_buffer,
                rx_descriptors,
            )
            .unwrap(),
        );
        // WiFi: session-gated NTP sync; credentials are the
        // throwaway wifi_secrets scheme until on-device provisioning
        // (scan + on-screen keyboard) lands.
        spawner.spawn(
            system_core::wifi::wifi_task(
                self.wifi.take().unwrap(),
                crate::wifi_secrets::WIFI_SSID,
                crate::wifi_secrets::WIFI_PASSWORD,
            )
            .unwrap(),
        );
        // BRING-UP: one sync per boot so every flash exercises the
        // whole radio -> NTP -> RTC path without UI. Interim fixed
        // CEST offset, same as GPS's first phase - the real trigger
        // carries config.tz_offset_minutes. REMOVE with Phase B.
        //
        // Kicked DELAYED: with the radio active during the boot
        // storm, esp-radio's preemption stretched I2C transactions
        // past their timeouts - every DRV2605/BHI260 boot-init
        // failure observed 2026-08-08 landed inside the WiFi session
        // window. Ten seconds puts the session after the IMU's
        // ~103 KB firmware upload and the haptics init. The radio-
        // vs-live-I2C interaction still needs a real look when
        // sessions start running mid-use (provisioning effort).
        spawner.spawn(wifi_bringup_kick().unwrap());
    }

    /// This board carries a GNSS receiver with a live sync task (the
    /// settings GPS view) and a BHI260AP whose step counter feeds the
    /// motion pipeline (clock-face steps + MOTION STEPS panel).
    fn capabilities(&self) -> app_core::data::Capabilities {
        app_core::data::Capabilities { gps: true, steps: true }
    }

    /// The watch case's molded lip overhangs the panel: bezel-ruler
    /// probe 2026-08-15 measured ~5-8 px swallowed on the top and
    /// both sides, ~1-4 px at the bottom (the Waveshare boards'
    /// printed-glass bezels mask nothing that matters and keep the
    /// zero default).
    fn safe_area(&self) -> app_core::data::SafeArea {
        app_core::data::SafeArea {
            top: 8, bottom: 4, left: 8, right: 8,
            // Derived from the clock-telemetry clipping (~42 px
            // intrusion at y=34): a circular arc of r=112 inside
            // the visible glass reproduces it.
            corner_r: 112,
        }
    }
}

/// Flip the I2S0 RX unit into hardware PDM-to-PCM mode for the T3902
/// mic - 16 kHz mono source, 16-bit PCM out, both line slots enabled.
/// esp-hal has no PDM API (its config path force-clears `rx_pdm_en`),
/// so this mirrors ESP-IDF's register sequence (i2s_hal_pdm_set_rx_slot
/// + i2s_hal_set_rx_clock + i2s_ll_rx_enable_pdm, IDF v5.3) on top of
/// the HAL's standard bring-up. Runs as `run_session_pdm_mic`'s
/// `tune_i2s` hook: after the HAL configured the peripheral, before
/// the transfers start the clocks.
///
/// Clocking (IDF i2s_pdm_rx_calculate_clock, 16 kHz, DSR-8 - the IDF
/// default the vendor firmware runs on this watch): PDM clock out =
/// 16 kHz x 64 = 1.024 MHz, inside the T3902's Standard-mode range
/// (1.0-3.3 MHz; 430 uA). bclk_div = 8 -> MCLK = 8.192 MHz from the
/// 160 MHz PLL: divider 19 + 17/32, mapped to the fractional fields
/// per i2s_ll_rx_set_mclk (yn1 = (17*2 > 32) = 1, z = 32-17 = 15,
/// x = 32/15 - 1 = 1, y = 32%15 = 2). Fallback if the first flash
/// captures nothing: sinc_dsr_16_en=1 + halved fraction (9 + 49/64)
/// moves the mic to 2.048 MHz, mid-range instead of edge-of-spec.
///
/// Slots: a single mic holds DATA through the whole clock period
/// (T3902 datasheet, mono figure 9), so with both line slots enabled
/// (chan0 + chan1) each 4-byte frame carries the same audio twice -
/// the codec boards' stereo wire format, which keeps every
/// system-core mode loop unchanged. SELECT is tied to VDD on this
/// board (vendor firmware configures the left slot).
///
/// The write sequence ends with the rx_update handshake: the RX unit
/// latches APB-side config into its own clock domain only on that
/// bit, and the HAL performs it during configuration only - i.e.
/// before this hook ran. Without it every write here is silently
/// ignored and the mic reads as dead.
/// Bus configuration for the mode-0 devices on the shared SPI3 bus
/// (SD card, SX1262). 400 kHz is SD-identification-safe and well
/// within both chips' SPI range. The NFC reader runs its own
/// mode-1 config - see `system::nfc`.
fn spi3_mode0_config() -> SpiConfig {
    SpiConfig::default()
        .with_frequency(Rate::from_khz(400))
        .with_mode(SpiMode::_0)
}

fn tune_pdm_rx() {
    let i2s = unsafe { &*esp32s3::I2S0::ptr() };

    // Reset the RX FSM + FIFO before reconfiguring (IDF does this at
    // the top of its PDM slot config).
    i2s.rx_conf().modify(|_, w| {
        w.rx_reset().set_bit();
        w.rx_fifo_reset().set_bit()
    });
    i2s.rx_conf().modify(|_, w| {
        w.rx_reset().clear_bit();
        w.rx_fifo_reset().clear_bit()
    });

    // Slot geometry: 16-bit data in 16-bit channels, half_sample = 16
    // (i2s_hal_pdm_set_rx_slot hardcodes 16 for this silicon), and
    // the PDM bclk divider (8, from the clock math above).
    i2s.rx_conf1().modify(|_, w| unsafe {
        w.rx_bits_mod().bits(15);
        w.rx_tdm_chan_bits().bits(15);
        w.rx_half_sample_bits().bits(15);
        w.rx_bck_div_num().bits(7)
    });

    // Mode: PDM in, PDM-to-PCM conversion on, TDM off, master, not
    // mono-duplicating (both slots carry line data), DSR-8.
    i2s.rx_conf().modify(|_, w| {
        w.rx_slave_mod().clear_bit();
        w.rx_mono().clear_bit();
        w.rx_pdm_sinc_dsr_16_en().clear_bit();
        w.rx_pdm2pcm_en().set_bit();
        w.rx_tdm_en().clear_bit();
        w.rx_pdm_en().set_bit()
    });

    // Both slots of the single data line (TRM table "PDM-to-PCM Input
    // Mode": chan0/chan1 = I2S0I_Data_in left/right).
    i2s.rx_tdm_ctrl().modify(|_, w| {
        w.rx_tdm_pdm_chan0_en().set_bit();
        w.rx_tdm_pdm_chan1_en().set_bit();
        w.rx_tdm_pdm_chan2_en().clear_bit();
        w.rx_tdm_pdm_chan3_en().clear_bit();
        w.rx_tdm_pdm_chan4_en().clear_bit();
        w.rx_tdm_pdm_chan5_en().clear_bit();
        w.rx_tdm_pdm_chan6_en().clear_bit();
        w.rx_tdm_pdm_chan7_en().clear_bit()
    });

    // MCLK divider, in IDF's mandated sequence: park on a small
    // integer division with zeroed fraction first (double-division
    // hardware erratum workaround), then set the target coefficients,
    // integer part last. 160 MHz PLL source (rx_clk_sel = 2).
    i2s.rx_clkm_conf().modify(|_, w| unsafe {
        w.rx_clk_sel().bits(2);
        w.rx_clk_active().set_bit();
        w.rx_clkm_div_num().bits(2)
    });
    i2s.rx_clkm_div_conf().modify(|_, w| unsafe {
        w.rx_clkm_div_yn1().clear_bit();
        w.rx_clkm_div_y().bits(1);
        w.rx_clkm_div_z().bits(0);
        w.rx_clkm_div_x().bits(0)
    });
    i2s.rx_clkm_div_conf().modify(|_, w| unsafe {
        w.rx_clkm_div_yn1().set_bit();
        w.rx_clkm_div_z().bits(15);
        w.rx_clkm_div_y().bits(2);
        w.rx_clkm_div_x().bits(1)
    });
    i2s.rx_clkm_conf()
        .modify(|_, w| unsafe { w.rx_clkm_div_num().bits(19) });

    // Commit to the RX clock domain (i2s_ll_rx_start's handshake:
    // set rx_update, hardware clears it once synced).
    i2s.rx_conf().modify(|_, w| w.rx_update().set_bit());
    while i2s.rx_conf().read().rx_update().bit_is_set() {}
}

/// Audio dispatch loop - MAX98357A speaker (standard I2S TX) and
/// T3902 PDM mic (RX in hardware PDM-to-PCM mode), both on I2S0 with
/// independent unit clocks. Owns the I2S peripheral tokens and the
/// cross-session tone-phase counter, and reborrows them into a fresh
/// session per session-starting command (embassy tasks can't be
/// generic, so the concrete types stay bin-side).
///
/// Two session kinds, routed per command: pure playback (PlayAlarm /
/// PlayTones) runs `run_session_tx` - no RX DMA, the mic clock never
/// starts, the mic stays in its 12 uA sleep. The capture modes
/// (StartCapture / StartLoopback) run `run_session_pdm_mic` with
/// `tune_pdm_rx` flipping the RX unit into PDM mode each session.
/// Neither device needs an enable line or codec init: the amp wakes
/// on BCLK and the mic on its PDM clock, and both self-idle when the
/// session's transfer drop stops the clocks.
#[embassy_executor::task]
#[allow(clippy::too_many_arguments)]
async fn audio_task(
    mut i2s: p::I2S0<'static>,
    mut dma: p::DMA_CH1<'static>,
    mut bclk: p::GPIO9<'static>,
    mut ws: p::GPIO10<'static>,
    mut dout: p::GPIO11<'static>,
    mut mic_clk: p::GPIO17<'static>,
    mut mic_din: p::GPIO18<'static>,
    tx_buffer: &'static mut [u8],
    tx_descriptors: &'static mut [esp_hal::dma::DmaDescriptor],
    rx_buffer: &'static mut [u8],
    rx_descriptors: &'static mut [esp_hal::dma::DmaDescriptor],
) {
    use system_core::audio::{run_session_pdm_mic, run_session_tx, SessionMode};
    use system_core::audio_hal::SpeakerAmp;
    use system_core::bus::{AudioCommand, AUDIO_COMMAND};

    let mut amp = SpeakerAmp::fixed();
    let mut phase: u32 = 0;
    // A command consumed by a session's inner loop that the
    // dispatcher must still act on - same hand-off as the other
    // bins, so transitions between sessions never drop a command.
    let mut pending: Option<AudioCommand> = None;

    loop {
        let cmd = match pending.take() {
            Some(c) => c,
            None => AUDIO_COMMAND.receive().await,
        };
        let mode = match cmd {
            AudioCommand::StopAlarm
            | AudioCommand::StopCapture
            | AudioCommand::StopTones
            | AudioCommand::StopLoopback => continue,
            AudioCommand::PlayAlarm => SessionMode::Play,
            AudioCommand::PlayTones => SessionMode::Tones,
            AudioCommand::StartCapture => SessionMode::Capture,
            AudioCommand::StartLoopback => SessionMode::Loopback,
        };
        pending = match mode {
            SessionMode::Play | SessionMode::Tones => {
                run_session_tx(
                    mode,
                    i2s.reborrow(),
                    dma.reborrow(),
                    bclk.reborrow(),
                    ws.reborrow(),
                    dout.reborrow(),
                    &mut amp,
                    &mut tx_buffer[..],
                    &mut tx_descriptors[..],
                    &mut phase,
                    // S3 silicon: no fixups needed on the plain TX
                    // path (same as the other S3 bin).
                    || {},
                )
                .await
            }
            SessionMode::Capture | SessionMode::Loopback => {
                run_session_pdm_mic(
                    mode,
                    i2s.reborrow(),
                    dma.reborrow(),
                    bclk.reborrow(),
                    ws.reborrow(),
                    dout.reborrow(),
                    mic_clk.reborrow(),
                    mic_din.reborrow(),
                    &mut amp,
                    &mut tx_buffer[..],
                    &mut tx_descriptors[..],
                    &mut rx_buffer[..],
                    &mut rx_descriptors[..],
                    &mut phase,
                    tune_pdm_rx,
                )
                .await
            }
        };
    }
}

/// BRING-UP: the delayed WiFi sync kick (see its spawn site in
/// `spawn_audio`). Ten seconds clears the boot storm - the IMU's
/// ~103 KB firmware upload, haptics init - before the radio session
/// starts; the boot-time I2C init transients of 2026-08-08 all
/// happened with the session overlapping that storm. Dies together
/// with the boot auto-kick when the provisioning UI lands.
#[embassy_executor::task]
async fn wifi_bringup_kick() {
    Timer::after(Duration::from_secs(10)).await;
    system_core::wifi::WIFI_COMMAND.signal(
        system_core::wifi::WifiCommand::SyncOnce { tz_offset_minutes: 120 },
    );
}

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) {
    let peripherals = esp_hal::init(
        esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()),
    );

    // Internal-SRAM heap, 128 KB, unified with the other boards. The
    // main stack is the RAM left over after all statics - with
    // esp-radio's ~56 KB of static buffers AND the 41 KB tile staging
    // FB moved from BSS into this heap (`psram-fb`) that leaves ~55 KB
    // of stack; the deepest save path (tagged config serialize + SD
    // mirror through embedded-sdmmc's FAT walker) blew through 13.8 KB
    // once (stack-guard panic, 2026-08-08), so re-check the readelf
    // number (see NOTE below) whenever statics grow. Heap load: ~60 KB
    // observed radio-active peak + the 41 KB staging FB = ~101 KB
    // worst case. The 8 MB QSPI PSRAM holds only the full-panel canvas
    // (see board.rs memory note).
    esp_alloc::heap_allocator!(size: 128 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);
    esp_println::logger::init_logger(log::LevelFilter::Info);
    log::info!("--- LilyGo T-Watch Ultra booting ---");
    // NOTE the main stack on this board is only what RAM remains
    // after statics (see the heap_allocator comment below). After
    // adding any static-heavy dependency, re-check it against the
    // built ELF - anything under ~24 KB is a stack-overflow risk:
    //   readelf -s target/xtensa-esp32s3-none-elf/release/firmware-twatch-ultra \
    //     | grep -E "_stack_(start|end)_cpu0"
    // (stack size = start - end; 2026-08-08 it had shrunk to 13.8 KB
    // and the settings-save path overflowed it.)

    let bringup = TwatchUltraBringup {
        i2c0: Some(peripherals.I2C0),
        i2c_sda: Some(peripherals.GPIO3),
        i2c_scl: Some(peripherals.GPIO2),
        pmu_irq: Some(peripherals.GPIO7),
        lora_cs: Some(peripherals.GPIO36),
        nfc_cs: Some(peripherals.GPIO4),
        lora_rst: Some(peripherals.GPIO47),
        lora_busy: Some(peripherals.GPIO48),
        spi2: Some(peripherals.SPI2),
        lcd_sclk: Some(peripherals.GPIO40),
        lcd_sio0: Some(peripherals.GPIO38),
        lcd_sio1: Some(peripherals.GPIO39),
        lcd_sio2: Some(peripherals.GPIO42),
        lcd_sio3: Some(peripherals.GPIO45),
        lcd_cs: Some(peripherals.GPIO41),
        dma_ch0: Some(peripherals.DMA_CH0),
        psram: Some(peripherals.PSRAM),
        fb_canvas: None,
        lcd_reset: Some(peripherals.GPIO37),
        i2s0: Some(peripherals.I2S0),
        dma_ch1: Some(peripherals.DMA_CH1),
        spk_bclk: Some(peripherals.GPIO9),
        spk_ws: Some(peripherals.GPIO10),
        spk_dout: Some(peripherals.GPIO11),
        mic_clk: Some(peripherals.GPIO17),
        mic_din: Some(peripherals.GPIO18),
        uart1: Some(peripherals.UART1),
        gps_tx: Some(peripherals.GPIO43),
        gps_rx: Some(peripherals.GPIO44),
        lcd_te: Some(peripherals.GPIO6),
        touch_int: Some(peripherals.GPIO12),
        btn_boot: Some(peripherals.GPIO0),
        rtc_int: Some(peripherals.GPIO1),
        imu_int: Some(peripherals.GPIO8),
        flash: Some(peripherals.FLASH),
        spi3: Some(peripherals.SPI3),
        sd_sck: Some(peripherals.GPIO35),
        sd_mosi: Some(peripherals.GPIO34),
        sd_miso: Some(peripherals.GPIO33),
        sd_cs: Some(peripherals.GPIO21),
        spi_bus: None,
        lora_cs_out: None,
        nfc_cs_out: None,
        lpwr: Some(peripherals.LPWR),
        wifi: Some(peripherals.WIFI),
    };

    run(bringup, spawner).await
}
