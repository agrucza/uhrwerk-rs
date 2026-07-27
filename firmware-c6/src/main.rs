#![no_std]
#![no_main]

extern crate alloc;

mod board;
mod system;

use crate::system::power::C6Board;
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
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull, WakeEvent};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::peripherals as p;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::Blocking;

esp_bootloader_esp_idf::esp_app_desc!();

/// C6 boot-construction seam. Mirrors `firmware-s3`'s `S3Bringup` in
/// role; differs only where the hardware does: no SD slot
/// (`init_flash_only`), no TE GPIO (`lcd_te: None`), no RTC_INT GPIO
/// (`RtcTaskState::init(None, ...)`), and an FT3168 wake-from-MONITOR
/// poll in `wait_for_peripherals`.
struct C6Bringup {
    i2c0: Option<p::I2C0<'static>>,
    i2c_sda: Option<p::GPIO8<'static>>,
    i2c_scl: Option<p::GPIO7<'static>>,
    spi2: Option<p::SPI2<'static>>,
    lcd_sclk: Option<p::GPIO0<'static>>,
    lcd_sio0: Option<p::GPIO1<'static>>,
    lcd_sio1: Option<p::GPIO2<'static>>,
    lcd_sio2: Option<p::GPIO3<'static>>,
    lcd_sio3: Option<p::GPIO4<'static>>,
    lcd_cs: Option<p::GPIO5<'static>>,
    dma_ch0: Option<p::DMA_CH0<'static>>,
    lcd_reset: Option<p::GPIO11<'static>>,
    touch_rst: Option<p::GPIO10<'static>>,
    touch_int: Option<p::GPIO15<'static>>,
    btn_boot: Option<p::GPIO9<'static>>,
    imu_int1: Option<p::GPIO16<'static>>,
    flash: Option<p::FLASH<'static>>,
    lpwr: Option<p::LPWR<'static>>,
    // Audio - full-duplex (speaker TX + mic RX share one I2S).
    i2s0: Option<p::I2S0<'static>>,
    dma_ch1: Option<p::DMA_CH1<'static>>,
    spk_mclk: Option<p::GPIO19<'static>>,
    spk_bclk: Option<p::GPIO20<'static>>,
    spk_ws: Option<p::GPIO22<'static>>,
    spk_dout: Option<p::GPIO23<'static>>,
    mic_din: Option<p::GPIO21<'static>>, // ES7210 ASDOUT -> ESP RX
    spk_pa: Option<p::GPIO6<'static>>,
}

impl Bringup for C6Bringup {
    type Board = C6Board;

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
        // No rail config: the AXP retains rail state across resets on
        // this board. C6Board::init only sanity-checks the chip ID.
        let (board, pmu) = C6Board::init(i2c);
        (board, PowerTaskState::new(pmu))
    }

    async fn wait_for_peripherals(&mut self, i2c: &mut I2c<'static, Blocking>) {
        // The FT3168 is left in TOUCH_POWER_MONITOR by prior firmware
        // and doesn't ACK its I2C address for ~2-3 s after boot.
        const TIMEOUT_MS: u32 = 5000;
        const POLL_MS: u32 = 500;
        log::info!(
            "Waiting for FT3168 (0x{:02X}) to wake (timeout {} ms)...",
            board::TOUCH_I2C_ADDR, TIMEOUT_MS,
        );
        let mut elapsed = 0u32;
        let mut awake = false;
        while elapsed < TIMEOUT_MS {
            Timer::after(Duration::from_millis(POLL_MS as u64)).await;
            elapsed += POLL_MS;
            let mut byte = [0u8; 1];
            if i2c.read(board::TOUCH_I2C_ADDR, &mut byte).is_ok() {
                log::info!("  FT3168 awake after {} ms", elapsed);
                awake = true;
                break;
            }
        }
        if !awake {
            log::warn!("  FT3168 did not ACK within {} ms", TIMEOUT_MS);
        }
    }

    async fn make_display(&mut self) -> Display<'static> {
        let fb: &'static mut [u8] = firmware_hal::display::take_framebuffer();
        let display = init_display(
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
        .await;
        // Raise the display DMA channel's GDMA arbitration priority
        // above the audio channel's (CH1 stays at the default 0).
        // Display QSPI (CH0) and I2S audio share this chip's single
        // GDMA engine, and the top-of-screen corruption observed
        // during audio-session bring-up is consistent with the
        // display transfer losing arbitration at that moment. The
        // display DMA is configured once and never rebuilt, so a
        // one-time poke here sticks. Audio needs only ~64 KB/s and
        // doesn't care about latency.
        {
            let ch0 = unsafe { &*esp32c6::DMA::ptr() }.ch(0);
            ch0.in_pri().modify(|_, w| unsafe { w.rx_pri().bits(1) });
            ch0.out_pri().modify(|_, w| unsafe { w.tx_pri().bits(1) });
        }
        display
    }

    /// No TE GPIO on this board - the manager skips the vblank wait
    /// entirely (zero delay, no timeout).
    fn make_lcd_te(&mut self) -> Option<Input<'static>> {
        None
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

    /// Flash only - no SD slot on this board.
    fn make_store(&mut self) -> Store<'static> {
        Store::init_flash_only(
            self.flash.take().unwrap(),
            FlashRegion::new(board::FLASH_FS_START, board::FLASH_FS_SIZE),
        )
    }

    async fn make_sensors(
        &mut self,
        i2c: &mut I2c<'static, Blocking>,
    ) -> (RtcTaskState<'static>, ImuTaskState<'static>) {
        // No RTC_INT GPIO routed on this board -> poll-only RTC task.
        let rtc_state = RtcTaskState::init(None, i2c);
        let imu = ImuTaskState::init(
            Input::new(self.imu_int1.take().unwrap(), InputConfig::default().with_pull(Pull::Down)),
            i2c,
        )
        .await;
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

/// C6 duplex clock fixups, passed to `run_session` as its `tune_i2s`
/// hook (re-applied every session - esp-hal rewrites these fields in
/// its configure path).
///
/// The C6's I2S clocks its TX and RX units from two independent
/// fractional PCR dividers (160 MHz / 39.0625) and esp-hal leaves
/// them free-running, which breaks full duplex twice over: the RX
/// unit samples DIN with its own divider's phase (a per-session
/// lottery against the wire - bit-run garbage from the mic on bad
/// rolls), and the MCLK pin gets bound to the RX divider while
/// BCLK/WS come from the TX divider (incoherent clocks at the codecs
/// - chopped/stuttering ES8311 playback). ESP-IDF's duplex driver
/// fixes both with two register writes, replicated here from its C6
/// `i2s_ll`: share the TX unit's BCK/WS with the RX unit
/// (`i2s_ll_share_bck_ws` - despite the field's name, sig_loopback
/// shares clocks, not data), and bind the MCLK pin to the TX divider
/// (`i2s_ll_mclk_bind_to_tx_clk`).
fn tune_i2s() {
    unsafe { &*esp32c6::I2S0::ptr() }
        .tx_conf()
        .modify(|_, w| w.sig_loopback().set_bit());
    unsafe { &*esp32c6::PCR::ptr() }
        .i2s_rx_clkm_conf()
        .modify(|_, w| w.i2s_mclk_sel().clear_bit());
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
/// The AXP's analog rail both codecs need is already enabled at boot
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
    mut mclk: p::GPIO19<'static>,
    mut bclk: p::GPIO20<'static>,
    mut ws: p::GPIO22<'static>,
    mut dout: p::GPIO23<'static>,
    mut din: p::GPIO21<'static>,
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
            tune_i2s,
            codec_init(i2c_bus),
        )
        .await;
    }
}

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // No PSRAM on this board: internal-SRAM heap (the framebuffer is
    // in the shared display HAL's static BSS, not the heap). 128 KB
    // to match the S3: the audio test suite holds 48 KB of live
    // buffers (32 KB mic pop + 16 KB parrot recording), which OOM'd
    // the previous 64 KB heap and starved renders during capture.
    // The C6 has 512 KB of HP SRAM; the linker will complain if this
    // ever collides with the static budget.
    esp_alloc::heap_allocator!(size: 128 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);
    esp_println::logger::init_logger(log::LevelFilter::Info);
    log::info!("--- ESP32-C6-Touch-AMOLED-2.06 booting ---");

    let bringup = C6Bringup {
        i2c0: Some(peripherals.I2C0),
        i2c_sda: Some(peripherals.GPIO8),
        i2c_scl: Some(peripherals.GPIO7),
        spi2: Some(peripherals.SPI2),
        lcd_sclk: Some(peripherals.GPIO0),
        lcd_sio0: Some(peripherals.GPIO1),
        lcd_sio1: Some(peripherals.GPIO2),
        lcd_sio2: Some(peripherals.GPIO3),
        lcd_sio3: Some(peripherals.GPIO4),
        lcd_cs: Some(peripherals.GPIO5),
        dma_ch0: Some(peripherals.DMA_CH0),
        lcd_reset: Some(peripherals.GPIO11),
        touch_rst: Some(peripherals.GPIO10),
        touch_int: Some(peripherals.GPIO15),
        btn_boot: Some(peripherals.GPIO9),
        imu_int1: Some(peripherals.GPIO16),
        flash: Some(peripherals.FLASH),
        lpwr: Some(peripherals.LPWR),
        i2s0: Some(peripherals.I2S0),
        dma_ch1: Some(peripherals.DMA_CH1),
        spk_mclk: Some(peripherals.GPIO19),
        spk_bclk: Some(peripherals.GPIO20),
        spk_ws: Some(peripherals.GPIO22),
        spk_dout: Some(peripherals.GPIO23),
        mic_din: Some(peripherals.GPIO21),
        spk_pa: Some(peripherals.GPIO6),
    };

    run(bringup, spawner).await
}
