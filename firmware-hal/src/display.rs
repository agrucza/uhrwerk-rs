//! Display init / HAL glue for the CO5300 QSPI AMOLED panel.
//!
//! Used by both the S3 and C6 firmware crates. The function `init_display`
//! takes esp-hal peripheral singletons and a framebuffer, builds the QSPI
//! bus over DMA, runs the CO5300 reset + init sequence, and returns a
//! ready-to-use display handle. The framebuffer can be full-panel
//! ([`FB_BYTES`]) or partial (see [`fb_bytes_for_rows`]); the CO5300 driver
//! clips drawing operations to the actual FB row count.
//!
//! ## Pipelined DMA
//!
//! `EspQspi` uses esp-hal's raw [`SpiDma`] (not the buffered `SpiDmaBus`
//! wrapper) so it can return from a pixel write *while the DMA is still
//! running*. The caller is then free to render into the framebuffer for
//! the next tile, overlapping CPU work with the SPI transfer. The next
//! call into the bus implicitly awaits the in-flight DMA before starting
//! its own. Call [`EspQspi::flush_pending`] to drain the in-flight DMA at
//! end-of-frame.
//!
//! ## DMA / source memory
//!
//! `EspQspi` owns a single [`DmaTxBuf`] sized for one tile's worth of
//! pixels. When a pixel write is requested, the caller's framebuffer slice
//! is copied into that internal DmaTxBuf and DMA pulls from there. Because
//! the copy happens up front (synchronously, blocking the caller for ~1 ms
//! per tile), the source framebuffer can live in PSRAM (S3) or internal
//! SRAM (C6) without any cross-region DMA concern - GDMA only ever reads
//! from the internal-SRAM DmaTxBuf.

use drivers::display::QspiWrite;
use embassy_time::{Duration, Timer};
use esp_hal::{
    Async,
    dma::{DmaChannelFor, DmaTxBuf},
    gpio::{Output, interconnect::{PeripheralInput, PeripheralOutput}},
    spi::master::{
        Address, AnySpi, Command, Config, DataMode, Spi, SpiDma, SpiDmaTransfer,
    },
    spi::Mode,
    time::Rate,
};

pub use drivers::display::{
    co5300::{FB_BYTES, HEIGHT, WIDTH, fb_bytes_for_rows},
    CO5300,
};

/// Tile height for partial-FB rendering.
///
/// Both boards run the same render loop: it walks the panel in
/// horizontal bands of `TILE_H` rows, rendering the screen into a small
/// FB sized for one tile, hashing the result, and only pushing tiles
/// whose content changed.
///
/// 50 rows is LVGL's recommended partial-FB size (1/10 of screen
/// height). Even, so panel address windows stay 2-row-aligned without
/// further rounding. FB cost: 50 * WIDTH * 2 = 41,000 bytes.
pub const TILE_H: u16 = 50;

/// Number of tiles needed to cover the panel: `ceil(HEIGHT / TILE_H)`.
///
/// The last tile is short whenever `HEIGHT % TILE_H != 0` (e.g. 502
/// rows / 50 = 10 full + 1 short 2-row tile). [`CO5300::flush_tile`]
/// clips the push for the short tile, so callers only need to iterate
/// `tile_y` in steps of `TILE_H` up to `HEIGHT`.
pub const NUM_TILES: usize = HEIGHT.div_ceil(TILE_H) as usize;

/// DCS commands used inside `write_pixels`.
const RAMWR:  u8 = 0x2C;
const RAMWRC: u8 = 0x3C;

/// SPI instruction opcodes.
const OPCODE_CTRL:  u8 = 0x02; // 1-wire config/command writes
const OPCODE_PIXEL: u8 = 0x32; // Quad 4-wire pixel writes

/// DMA TX buffer capacity in bytes.
///
/// 32736 = 8 * 4092, the largest size that fits both:
///   - the esp-hal `dma_buffers!` macro's natural 4092-byte descriptor
///     stride, and
///   - the ESP32-S3 SPI peripheral's 18-bit length register, which
///     caps a single `half_duplex_write` transfer at 32 KB. Sending
///     more than that in one call silently overflows the register and
///     the panel receives truncated data (manifests as a solid-color
///     panel because RAMWR latches a partial first-chunk worth of
///     pixels into GRAM and then bails).
///
/// Tiles larger than this are sent in multiple chunks by
/// [`EspQspi::write_pixels`]; the first chunk uses RAMWR and
/// subsequent chunks use RAMWRC (continuation), matching how the
/// panel expects multi-chunk burst writes.
const TX_BUF_SIZE: usize = 32736;

/// Framebuffer size in bytes for one full tile.
///
/// Same as [`fb_bytes_for_rows(TILE_H)`] but expressible as a `const`
/// for use in a static-array dimension.
pub const FB_BYTES_PER_TILE: usize = TILE_H as usize * WIDTH as usize * 2;

// -- Framebuffer storage -----------------------------------------------------
//
// The render-pass framebuffer is a single tile's worth of RGB565 pixels
// (~41 KB), declared as a `static mut` so it lands in internal-SRAM BSS
// regardless of which board variant we're built for.
//
// Why a static and not `alloc::vec![..].leak()` (the old pattern):
//   - On boards where the default heap lives in PSRAM (S3), a leaked
//     Vec would put the FB in PSRAM. Pulling pixels from PSRAM into the
//     bus's internal-SRAM `DmaTxBuf` on every flush would cost an extra
//     cross-bus copy (~2-3x slower than SRAM-to-SRAM). Forcing the FB
//     into internal SRAM keeps the per-tile flush copy fast.
//   - On boards without PSRAM (C6) the heap is already in internal SRAM,
//     so the placement is the same either way - but having every
//     firmware variant take the FB from this single static keeps the
//     display path free of per-board allocator divergence.
//
// Taken at most once via [`take_framebuffer`]; a double-take panics so
// the failure mode of two render paths fighting over the FB is loud
// rather than silent.
static mut FRAMEBUFFER: [u8; FB_BYTES_PER_TILE] = [0u8; FB_BYTES_PER_TILE];
static FRAMEBUFFER_TAKEN: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Take ownership of the static framebuffer. Lives in internal-SRAM BSS
/// on both S3 and C6. Call exactly once at display init; a second call
/// panics.
pub fn take_framebuffer() -> &'static mut [u8] {
    use core::sync::atomic::Ordering;
    if FRAMEBUFFER_TAKEN.swap(true, Ordering::AcqRel) {
        panic!("display framebuffer already taken - take_framebuffer() may only be called once");
    }
    // SAFETY: AtomicBool above guarantees this code path runs at most
    // once across all execution contexts, so the returned `&mut` slice
    // has unique access. The static is `'static`-lived.
    unsafe { &mut *core::ptr::addr_of_mut!(FRAMEBUFFER) }
}

/// Bus state machine: either we own the hardware and a free DMA buffer
/// (`Idle`), or we have an in-flight DMA transfer that owns both
/// (`Pending`). Held inside `EspQspi` behind an `Option` so we can
/// `.take()` during state transitions.
enum BusState<'d> {
    Idle {
        bus: SpiDma<'d, Async>,
        txbuf: DmaTxBuf,
    },
    Pending {
        transfer: SpiDmaTransfer<'d, Async, DmaTxBuf>,
    },
}

/// Pipelined QSPI bus wrapper around esp-hal's raw [`SpiDma`].
///
/// The public surface is the [`QspiWrite`] trait. The state field is
/// `Option` purely so we can take the state out of `&mut self` and pass
/// owned values into the consuming `half_duplex_write` API; it is never
/// observed as `None` between method calls.
pub struct EspQspi<'d> {
    state: Option<BusState<'d>>,
}

impl<'d> EspQspi<'d> {
    /// Drain any in-flight DMA so the bus is back to [`BusState::Idle`].
    /// Cheap when nothing's pending. Used internally by every
    /// bus-operating method as the first step.
    async fn ensure_idle(&mut self) {
        match self.state.take() {
            Some(BusState::Idle { bus, txbuf }) => {
                self.state = Some(BusState::Idle { bus, txbuf });
            }
            Some(BusState::Pending { mut transfer }) => {
                // `wait_for_done` is the async wait - yields to the
                // executor while waiting for the DMA-complete interrupt,
                // so other tasks (touch, RTC, IMU) get to run during the
                // ~1 ms tile push. `wait()` is then immediate because
                // the transfer is already done; it just hands back
                // ownership of bus + buf.
                transfer.wait_for_done().await;
                let (bus, txbuf) = transfer.wait();
                self.state = Some(BusState::Idle { bus, txbuf });
            }
            None => {
                // Bus was left in an invalid state (took() never replaced).
                // Should be impossible if every method puts state back.
                panic!("EspQspi state machine corrupted");
            }
        }
    }
}

impl<'d> QspiWrite for EspQspi<'d> {
    type Error = esp_hal::spi::Error;

    /// Drain any in-flight pixel DMA. Called at end-of-frame so the bus
    /// is idle before the manager returns and other code paths (sleep
    /// transitions, brightness writes, screen renders) can start cleanly.
    async fn flush_pending(&mut self) -> Result<(), Self::Error> {
        self.ensure_idle().await;
        Ok(())
    }

    /// Send one MIPI DCS command over 1-wire SPI (opcode 0x02). Commands
    /// are small and quick; we issue them synchronously (start transfer,
    /// wait inline) rather than leaving DMA in flight, since commands
    /// generally need their effect to take hold before the next operation.
    async fn write_cmd(&mut self, cmd: u8, params: &[u8]) -> Result<(), Self::Error> {
        self.ensure_idle().await;
        let (bus, mut txbuf) = match self.state.take() {
            Some(BusState::Idle { bus, txbuf }) => (bus, txbuf),
            _ => unreachable!("ensure_idle guarantees Idle"),
        };
        // Copy params into the DMA buffer's slice. We deliberately do
        // NOT call `txbuf.set_length(params.len())` here, mirroring how
        // esp-hal's own blocking `SpiDmaBus::half_duplex_write` operates:
        // the descriptor chain stays sized for the buffer's full
        // capacity, and the SPI hardware stops at `bytes_to_write`
        // bytes regardless. Calling `set_length(0)` for an empty-param
        // command (SLPOUT / DISPON / SLPIN / DISPOFF) tears down the
        // descriptor chain, which then doesn't re-extend cleanly on
        // the next pixel-sized transfer - that bug manifested as a
        // solid green panel after init.
        if !params.is_empty() {
            txbuf.as_mut_slice()[..params.len()].copy_from_slice(params);
        }
        match bus.half_duplex_write(
            DataMode::Single,
            Command::_8Bit(OPCODE_CTRL as u16, DataMode::Single),
            Address::_24Bit((cmd as u32) << 8, DataMode::Single),
            0,
            params.len(),
            txbuf,
        ) {
            Ok(transfer) => {
                // Synchronously wait for the command transfer to complete.
                // `wait()` is blocking but commands are <16 bytes so the
                // wait is microseconds.
                let (bus, txbuf) = transfer.wait();
                self.state = Some(BusState::Idle { bus, txbuf });
                Ok(())
            }
            Err((e, bus, txbuf)) => {
                self.state = Some(BusState::Idle { bus, txbuf });
                Err(e)
            }
        }
    }

    /// Write pixel bytes to the display.
    ///
    /// `data` is split into [`TX_BUF_SIZE`]-byte chunks (32 KB each) to
    /// stay under the SPI hardware's 18-bit length-register cap. The
    /// first chunk uses `RAMWR` (or `RAMWRC` if `first == false`); all
    /// subsequent chunks use `RAMWRC` (continuation) so the panel keeps
    /// writing into the same address window.
    ///
    /// **Pipelining behaviour:** the *last* chunk's DMA is left
    /// running on return - the caller's `data` slice was fully copied
    /// into the bus's internal DMA buffer during the synchronous chunk
    /// loop, so it's safe to mutate immediately. The next call into
    /// the bus (or [`flush_pending`]) awaits the in-flight tail
    /// transfer. For typical tile sizes (41 KB) this means: copy +
    /// kick chunk 1 → wait + copy + kick chunk 2 (last) → return.
    /// Caller renders next tile while chunk 2's DMA runs.
    async fn write_pixels(&mut self, first: bool, data: &[u8]) -> Result<(), Self::Error> {
        let mut offset = 0;
        let mut is_first = first;
        while offset < data.len() {
            self.ensure_idle().await;
            let (bus, mut txbuf) = match self.state.take() {
                Some(BusState::Idle { bus, txbuf }) => (bus, txbuf),
                _ => unreachable!("ensure_idle guarantees Idle"),
            };
            let n = (data.len() - offset).min(txbuf.capacity());
            txbuf.as_mut_slice()[..n].copy_from_slice(&data[offset..offset + n]);

            let dcs = if is_first { RAMWR } else { RAMWRC };
            match bus.half_duplex_write(
                DataMode::Quad,
                Command::_8Bit(OPCODE_PIXEL as u16, DataMode::Single),
                Address::_24Bit((dcs as u32) << 8, DataMode::Single),
                0,
                n,
                txbuf,
            ) {
                Ok(transfer) => {
                    // Don't await - DMA runs in the background; the next
                    // iteration of this loop (or the next call into the
                    // bus) blocks via ensure_idle if it's still running.
                    self.state = Some(BusState::Pending { transfer });
                }
                Err((e, bus, txbuf)) => {
                    self.state = Some(BusState::Idle { bus, txbuf });
                    return Err(e);
                }
            }
            is_first = false;
            offset += n;
        }
        Ok(())
    }
}

/// Build and configure the QSPI DMA SPI bus for the CO5300.
///
/// The pins are chip-board-specific (see each `firmware-*/src/board.rs`).
/// This function is fully generic over the peripheral types, so the same
/// implementation serves both S3 and C6.
pub fn build_spi<'d>(
    spi:  impl esp_hal::spi::master::Instance + 'd,
    sck:  impl PeripheralOutput<'d>,
    // QSPI data lines are bidirectional in esp-hal 1.1's SPI API -
    // PeripheralInput bound has to be propagated through the wrapper.
    sio0: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    sio1: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    sio2: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    sio3: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    cs:   impl PeripheralOutput<'d>,
    dma:  impl DmaChannelFor<AnySpi<'d>>,
) -> EspQspi<'d> {
    // Only TX is used; we never read pixel data back from the panel.
    // Descriptor count is auto-computed by the macro - one descriptor
    // per 4092 bytes, so 41000 / 4092 = ~11 descriptors here.
    let (_, _, tx_buffer, tx_descriptors) =
        esp_hal::dma_buffers!(0, TX_BUF_SIZE);
    let dma_tx_buf = DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();

    // 66 MHz: at the CO5300 15 ns write spec limit. If this turns out to be
    // unstable in practice we can drop to 40 MHz or push back up to 80 MHz
    // (out of spec but known to work on most panels).
    let spi_dma = Spi::new(
        spi,
        Config::default()
            .with_frequency(Rate::from_mhz(66))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(sck)
    .with_sio0(sio0)
    .with_sio1(sio1)
    .with_sio2(sio2)
    .with_sio3(sio3)
    .with_cs(cs)
    .with_dma(dma)
    .into_async();

    EspQspi {
        state: Some(BusState::Idle { bus: spi_dma, txbuf: dma_tx_buf }),
    }
}

/// Full display hardware init.
///
/// Builds the QSPI bus over DMA, performs the CO5300 reset pulse, sends the
/// init command sequence, exits sleep, turns the panel on. Returns the
/// ready-to-use display handle.
///
/// `fb` may be a full-panel buffer ([`FB_BYTES`]) or any partial buffer
/// sized via [`fb_bytes_for_rows`]; the CO5300 driver clips drawing
/// operations to the actual FB row count.
pub async fn init_display<'d, 'fb>(
    spi: impl esp_hal::spi::master::Instance + 'd,
    sclk: impl PeripheralOutput<'d>,
    sio0: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    sio1: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    sio2: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    sio3: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    cs: impl PeripheralOutput<'d>,
    dma: impl DmaChannelFor<AnySpi<'d>>,
    reset_pin: Output<'d>,
    fb: &'fb mut [u8],
    brightness: u8,
) -> CO5300<'fb, EspQspi<'d>, Output<'d>> {
    let bus = build_spi(spi, sclk, sio0, sio1, sio2, sio3, cs, dma);
    let mut display = CO5300::new(bus, reset_pin, fb);

    // Hardware reset: short low pulse, then 120 ms settle.
    display.reset_high();
    Timer::after(Duration::from_millis(10)).await;
    display.reset_low();
    Timer::after(Duration::from_millis(10)).await;
    display.reset_high();
    Timer::after(Duration::from_millis(120)).await;

    log::info!("Display: initializing CO5300...");
    display.init(brightness).await;
    display.wake().await;
    Timer::after(Duration::from_millis(120)).await; // SLPOUT settle
    display.display_on().await;
    Timer::after(Duration::from_millis(70)).await;
    log::info!("Display: ready ({} FB rows)", display.fb_rows());

    display
}
