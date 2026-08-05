#![no_std]
#![no_main]

extern crate alloc;

mod board;
// The audio stack lives in `system_core::audio` - same hardware on
// every board. `spawn_audio` below hands this board's I2S / DMA /
// speaker pins to the bin-local `audio_task` dispatcher; the actual
// session logic (build I2S, run play/capture loop, drop transfer)
// lives in the shared `system_core::audio::run_session`. Full duplex
// (alarm tone + mic capture) is supported via session-scoped transfer
// rebuilds (`Peri::reborrow()` per command) - see the design comment
// on `run_session`.
mod system;

// `config`/`events`/`ui` live in `app-core` (host-testable);
// re-export at the crate root so existing `crate::config::...` paths
// keep working.
pub use app_core::{config, events, ui};

use crate::system::power::PowerControls;
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

/// S3 boot-construction seam. Holds the raw peripheral tokens (esp-hal
/// singletons can't be partial-moved through `&mut self`, so each is
/// an `Option` `.take()`-n once by its `Bringup` method). The shared
/// `system_core::manager::run` drives the canonical sequence.
struct S3Bringup {
    i2c0: Option<p::I2C0<'static>>,
    i2c_sda: Option<p::GPIO15<'static>>,
    i2c_scl: Option<p::GPIO14<'static>>,
    sys_out: Option<p::GPIO10<'static>>,
    motor: Option<p::GPIO18<'static>>,
    spi2: Option<p::SPI2<'static>>,
    lcd_sclk: Option<p::GPIO11<'static>>,
    lcd_sio0: Option<p::GPIO4<'static>>,
    lcd_sio1: Option<p::GPIO5<'static>>,
    lcd_sio2: Option<p::GPIO6<'static>>,
    lcd_sio3: Option<p::GPIO7<'static>>,
    lcd_cs: Option<p::GPIO12<'static>>,
    dma_ch0: Option<p::DMA_CH0<'static>>,
    lcd_reset: Option<p::GPIO8<'static>>,
    lcd_te: Option<p::GPIO13<'static>>,
    touch_rst: Option<p::GPIO9<'static>>,
    touch_int: Option<p::GPIO38<'static>>,
    btn_boot: Option<p::GPIO0<'static>>,
    rtc_int: Option<p::GPIO39<'static>>,
    imu_int1: Option<p::GPIO21<'static>>,
    flash: Option<p::FLASH<'static>>,
    spi3: Option<p::SPI3<'static>>,
    sd_sck: Option<p::GPIO2<'static>>,
    sd_mosi: Option<p::GPIO1<'static>>,
    sd_miso: Option<p::GPIO3<'static>>,
    sd_cs: Option<p::GPIO17<'static>>,
    lpwr: Option<p::LPWR<'static>>,
    // Audio - full-duplex (speaker TX + mic RX share one I2S).
    i2s0: Option<p::I2S0<'static>>,
    dma_ch1: Option<p::DMA_CH1<'static>>,
    spk_mclk: Option<p::GPIO16<'static>>,
    spk_bclk: Option<p::GPIO41<'static>>,
    spk_ws: Option<p::GPIO45<'static>>,
    spk_dout: Option<p::GPIO40<'static>>,
    mic_din: Option<p::GPIO42<'static>>, // ES7210 ASDOUT -> ESP RX
    spk_pa: Option<p::GPIO46<'static>>,
}

impl Bringup for S3Bringup {
    type Board = PowerControls<'static>;

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
        let (power, pmu) = PowerControls::init(
            Output::new(self.sys_out.take().unwrap(), Level::Low, OutputConfig::default()),
            Output::new(self.motor.take().unwrap(), Level::Low, OutputConfig::default()),
            i2c,
        )
        .expect("PMU init failed - halting");
        (power, PowerTaskState::new(pmu))
    }

    async fn make_display(&mut self) -> Display<'static> {
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
        )
        .await
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

        let touch = TouchTaskState::init(
            Output::new(self.touch_rst.take().unwrap(), Level::High, OutputConfig::default()),
            touch_int,
            i2c,
        )
        .await;
        (touch, BootButtonTaskState::new(boot_btn))
    }

    fn make_store(&mut self) -> Store<'static> {
        // The SD card is this board's only SPI3 device, but it rides
        // the shared-bus seam anyway - one code shape across boards.
        // 400 kHz mode 0, the SD-identification-safe rate the old
        // exclusive path used.
        let spi = Spi::new(
            self.spi3.take().unwrap(),
            SpiConfig::default()
                .with_frequency(Rate::from_khz(400))
                .with_mode(SpiMode::_0),
        )
        .unwrap()
        .with_sck(self.sd_sck.take().unwrap())
        .with_mosi(self.sd_mosi.take().unwrap())
        .with_miso(self.sd_miso.take().unwrap());
        let bus = system_core::spi_bus::init_shared_bus(spi);
        Store::init(
            self.flash.take().unwrap(),
            FlashRegion::new(board::FLASH_FS_START, board::FLASH_FS_SIZE),
            system_core::spi_bus::SharedSpiDevice::new(
                bus,
                Output::new(self.sd_cs.take().unwrap(), Level::High, OutputConfig::default()),
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
        // The chip choice is the board's: QMI8658 behind the AnyImu
        // seam. Bring-up (reset, bias, self-tests) runs inside the
        // shared IMU task, staged over the shared bus.
        let imu = ImuTaskState::new(
            drivers::imu::AnyImu::Qmi8658(drivers::imu::QmiImu::new()),
            Input::new(self.imu_int1.take().unwrap(), InputConfig::default().with_pull(Pull::Down)),
        );
        (rtc_state, imu)
    }

    fn make_rtc_ctrl(&mut self) -> esp_hal::rtc_cntl::Rtc<'static> {
        esp_hal::rtc_cntl::Rtc::new(self.lpwr.take().unwrap())
    }

    fn spawn_audio(
        &mut self,
        spawner: embassy_executor::Spawner,
        i2c_bus: &'static system_core::bus::SharedI2c,
    ) {
        // Circular DMA buffers. RX is 32 KB (~512 ms at 16 kHz/16-bit
        // stereo) - sized to ride main-loop stalls without overrunning
        // the consumer. A full-screen settings render can take ~100 ms
        // and bursts of renders can stack; if the DMA fills the ring
        // before the audio task pops, `rx.pop` returns
        // `DmaError::Late`, which esp-hal returns synchronously and
        // can starve the executor (see audio.rs error arm). TX stays
        // small - the speaker only streams short alert tones.
        let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) =
            esp_hal::dma_circular_buffers!(32_768, 4096);
        spawner.spawn(
            audio_task(
                i2c_bus,
                self.i2s0.take().unwrap(),
                self.dma_ch1.take().unwrap(),
                self.spk_mclk.take().unwrap(),
                self.spk_bclk.take().unwrap(),
                self.spk_ws.take().unwrap(),
                self.spk_dout.take().unwrap(),
                self.mic_din.take().unwrap(),
                Output::new(self.spk_pa.take().unwrap(), Level::Low, OutputConfig::default()),
                tx_buffer,
                tx_descriptors,
                rx_buffer,
                rx_descriptors,
            )
            .unwrap(),
        );
    }
}

/// ES7210 analog input gain. 30 dB is the esp-bsp / esp_codec_dev
/// reference default for this ADC and is usable now that the modulator
/// clock is correct (the earlier "30 dB clips on ambient" reading was
/// the misclocked modulator's own noise, not real signal). The shared
/// level-meter constants are calibrated against this value.
const MIC_GAIN: drivers::es7210::MicGain = drivers::es7210::MicGain::Db30;

/// Per-session audio hardware bring-up, passed to `run_session` as its
/// `init_hw` future - this board's codec pair (ES8311 DAC + ES7210
/// ADC) at this board's levels. Runs with the I2S clocks already up;
/// see the hook's call site in `run_session` for the timing contract
/// and why it must re-run every session.
///
/// The ALDO1 analog rail both codecs need is already enabled at boot
/// (the FT3168 touch controller shares it - see `Pmu::set_audio_rail`),
/// so no rail toggle happens here.
async fn codec_init(i2c_bus: &'static system_core::bus::SharedI2c) {
    use drivers::es8311::Es8311;
    use drivers::es7210::Es7210;
    use embassy_time::{Duration, Timer};

    let mut i2c = i2c_bus.lock().await;
    let es8311 = Es8311::new();
    match es8311.init(&mut *i2c) {
        Ok(()) => {
            // Reset hold, per the reference driver. Skipping it left
            // the chip half-reset on a coin-flip of session re-inits:
            // silent or warbling DAC, hot ADC path.
            Timer::after(Duration::from_millis(20)).await;
            match es8311.init_after_reset(&mut *i2c) {
                Ok(()) => log::info!("Audio: ES8311 ready"),
                Err(_) => log::error!("Audio: ES8311 config failed"),
            }
        }
        Err(_) => log::error!(
            "Audio: ES8311 not found at 0x{:02X}",
            drivers::es8311::ADDR,
        ),
    }
    let _ = es8311.set_volume(&mut *i2c, 0xAF);

    let es7210 = Es7210::new();
    match es7210.init(&mut *i2c) {
        Ok(()) => {
            Timer::after(Duration::from_millis(10)).await;
            let _ = es7210.init_after_delay(&mut *i2c);
            Timer::after(Duration::from_millis(10)).await;
            match es7210.finalize(&mut *i2c) {
                Ok(()) => {
                    let _ = es7210.set_gain(&mut *i2c, MIC_GAIN);
                    log::info!("Audio: ES7210 ready (gain override applied)");
                }
                Err(_) => log::error!("Audio: ES7210 finalize failed"),
            }
        }
        Err(_) => log::error!(
            "Audio: ES7210 not found at 0x{:02X}",
            drivers::es7210::ADDR,
        ),
    }
}

/// Audio dispatch loop. Owns the board-specific peripheral tokens and
/// the cross-session state (amp, tone-phase counter),
/// and reborrows the tokens into a fresh `run_session` for each
/// PlayAlarm / StartCapture command. embassy tasks can't be generic
/// so the concrete types stay bin-side; the actual session logic
/// (build I2S, run inner loop, drop transfer) lives in system-core's
/// `run_session` and is shared.
#[embassy_executor::task]
async fn audio_task(
    i2c_bus: &'static system_core::bus::SharedI2c,
    mut i2s: p::I2S0<'static>,
    mut dma: p::DMA_CH1<'static>,
    mut mclk: p::GPIO16<'static>,
    mut bclk: p::GPIO41<'static>,
    mut ws: p::GPIO45<'static>,
    mut dout: p::GPIO40<'static>,
    mut din: p::GPIO42<'static>,
    pa: Output<'static>,
    tx_buffer: &'static mut [u8],
    tx_descriptors: &'static mut [esp_hal::dma::DmaDescriptor],
    rx_buffer: &'static mut [u8],
    rx_descriptors: &'static mut [esp_hal::dma::DmaDescriptor],
) {
    use system_core::audio::{run_session, SessionMode};
    use system_core::audio_hal::SpeakerAmp;
    use system_core::bus::{AudioCommand, AUDIO_COMMAND};

    let mut amp = SpeakerAmp::new(pa);
    amp.disable();
    let mut phase: u32 = 0;
    // A command consumed by a session's inner loop that the dispatcher
    // must still act on (e.g. a StartCapture that interrupted a
    // playing alarm) - so transitions between sessions never drop a
    // command.
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
            | AudioCommand::StopLoopback => {
                // No active session here - the inner loops own their
                // own stop response. Defensive amp mute.
                amp.disable();
                continue;
            }
            AudioCommand::PlayAlarm => SessionMode::Play,
            AudioCommand::StartCapture => SessionMode::Capture,
            AudioCommand::PlayTones => SessionMode::Tones,
            AudioCommand::StartLoopback => SessionMode::Loopback,
        };
        pending = run_session(
            mode,
            i2s.reborrow(), dma.reborrow(),
            mclk.reborrow(), bclk.reborrow(), ws.reborrow(),
            dout.reborrow(), din.reborrow(),
            &mut amp,
            &mut tx_buffer[..],
            &mut tx_descriptors[..],
            &mut rx_buffer[..],
            &mut rx_descriptors[..],
            &mut phase,
            // No I2S register fixups needed: the S3's I2S keeps its
            // TX/RX units clock-coherent on its own (unlike the C6 -
            // see that bin's `tune_i2s`).
            || {},
            codec_init(i2c_bus),
        )
        .await;
    }
}

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) {
    let peripherals = esp_hal::init(
        esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()),
    );

    // Internal-SRAM heap, unified with the C6 (no PSRAM). PSRAM was
    // only ever the heap backing on this board - the framebuffer and
    // the DMA TxBuf are internal-SRAM statics - and live heap use is
    // ~12 KB, so the 8 MB OPI PSRAM was pure headroom. Dropping it
    // removes the dominant idle/sleep power draw here (the OPI PSRAM
    // runs at 80 MHz and is never gated during light sleep). 128 KB is
    // generous next to the C6's 64 KB - the S3 has the internal SRAM
    // to spare - and covers the ~16 KB audio drain + file-read buffers
    // with margin. `peripherals.PSRAM` is now simply left unused.
    esp_alloc::heap_allocator!(size: 128 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);
    esp_println::logger::init_logger(log::LevelFilter::Info);
    log::info!("--- ESP32-S3-Touch-AMOLED-2.06 booting ---");

    // All board-specific construction lives in the `Bringup` impl;
    // the shared orchestrator drives the canonical boot sequence.
    let bringup = S3Bringup {
        i2c0: Some(peripherals.I2C0),
        i2c_sda: Some(peripherals.GPIO15),
        i2c_scl: Some(peripherals.GPIO14),
        sys_out: Some(peripherals.GPIO10),
        motor: Some(peripherals.GPIO18),
        spi2: Some(peripherals.SPI2),
        lcd_sclk: Some(peripherals.GPIO11),
        lcd_sio0: Some(peripherals.GPIO4),
        lcd_sio1: Some(peripherals.GPIO5),
        lcd_sio2: Some(peripherals.GPIO6),
        lcd_sio3: Some(peripherals.GPIO7),
        lcd_cs: Some(peripherals.GPIO12),
        dma_ch0: Some(peripherals.DMA_CH0),
        lcd_reset: Some(peripherals.GPIO8),
        lcd_te: Some(peripherals.GPIO13),
        touch_rst: Some(peripherals.GPIO9),
        touch_int: Some(peripherals.GPIO38),
        btn_boot: Some(peripherals.GPIO0),
        rtc_int: Some(peripherals.GPIO39),
        imu_int1: Some(peripherals.GPIO21),
        flash: Some(peripherals.FLASH),
        spi3: Some(peripherals.SPI3),
        sd_sck: Some(peripherals.GPIO2),
        sd_mosi: Some(peripherals.GPIO1),
        sd_miso: Some(peripherals.GPIO3),
        sd_cs: Some(peripherals.GPIO17),
        lpwr: Some(peripherals.LPWR),
        i2s0: Some(peripherals.I2S0),
        dma_ch1: Some(peripherals.DMA_CH1),
        spk_mclk: Some(peripherals.GPIO16),
        spk_bclk: Some(peripherals.GPIO41),
        spk_ws: Some(peripherals.GPIO45),
        spk_dout: Some(peripherals.GPIO40),
        mic_din: Some(peripherals.GPIO42),
        spk_pa: Some(peripherals.GPIO46),
    };

    run(bringup, spawner).await
}
