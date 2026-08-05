//! Shared blocking SPI bus - the SPI counterpart of `bus::SharedI2c`.
//!
//! Some boards hang several chip-selected devices off one SPI bus
//! (SD card, radios). The bin builds the `esp-hal` bus driver once,
//! parks it in the [`SHARED_SPI_BUS`] static via [`init_shared_bus`],
//! and hands each consumer a [`SharedSpiDevice`] - a CS-scoped
//! `embedded-hal 1` `SpiDevice` that locks the bus per transaction.
//!
//! ## Locking discipline
//!
//! The mutex is taken with `try_lock` inside a spin loop, never held
//! across an `await`. That mirrors the `SharedI2c` rule: a guard
//! lives for one synchronous burst of bus work and is dropped before
//! the holder yields. On a single-threaded executor the spin
//! therefore never actually iterates - tasks cannot interleave
//! inside a synchronous section - and on multi-threaded setups it
//! degrades to a short busy-wait for the other side's in-flight
//! transaction.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embedded_hal::delay::DelayNs;
use embedded_hal::spi::{ErrorType, Operation, SpiBus, SpiDevice};
use esp_hal::gpio::Output;
use esp_hal::spi::master::Spi;
use esp_hal::Blocking;
use static_cell::StaticCell;

/// The shared blocking SPI bus driver behind its mutex.
pub type SharedSpiBus = Mutex<CriticalSectionRawMutex, Spi<'static, Blocking>>;

static SHARED_SPI_BUS: StaticCell<SharedSpiBus> = StaticCell::new();

/// Park the bin-built bus driver in the shared static. Call once at
/// boot; the returned reference feeds every [`SharedSpiDevice`].
pub fn init_shared_bus(spi: Spi<'static, Blocking>) -> &'static SharedSpiBus {
    SHARED_SPI_BUS.init(Mutex::new(spi))
}

/// One chip-selected device on the shared bus. Owns its CS output;
/// every `SpiDevice` transaction locks the bus, frames the
/// operations with CS, and releases.
pub struct SharedSpiDevice {
    bus: &'static SharedSpiBus,
    cs: Output<'static>,
}

impl SharedSpiDevice {
    /// `cs` must already idle in its deselected (high) state.
    pub fn new(bus: &'static SharedSpiBus, cs: Output<'static>) -> Self {
        Self { bus, cs }
    }
}

impl ErrorType for SharedSpiDevice {
    type Error = esp_hal::spi::Error;
}

impl SpiDevice<u8> for SharedSpiDevice {
    fn transaction(
        &mut self,
        operations: &mut [Operation<'_, u8>],
    ) -> Result<(), Self::Error> {
        // See the module docs: contention is only ever another
        // thread's in-flight transaction, bounded and short.
        let mut guard = loop {
            if let Ok(g) = self.bus.try_lock() {
                break g;
            }
            core::hint::spin_loop();
        };
        let bus: &mut Spi<'static, Blocking> = &mut guard;

        self.cs.set_low();
        let mut res = Ok(());
        for op in operations.iter_mut() {
            res = match op {
                Operation::Read(buf) => SpiBus::read(bus, buf),
                Operation::Write(buf) => SpiBus::write(bus, buf),
                Operation::Transfer(rd, wr) => SpiBus::transfer(bus, rd, wr),
                Operation::TransferInPlace(buf) => {
                    SpiBus::transfer_in_place(bus, buf)
                }
                Operation::DelayNs(ns) => {
                    // The contract requires prior operations to have
                    // hit the wire before the delay starts.
                    match SpiBus::flush(bus) {
                        Ok(()) => {
                            esp_hal::delay::Delay::new().delay_ns(*ns);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                }
            };
            if res.is_err() {
                break;
            }
        }
        // CS must deassert whether the transaction succeeded or not,
        // and only after the bus has drained.
        let flush = SpiBus::flush(bus);
        self.cs.set_high();
        res.and(flush)
    }
}
