//! HAL glue for the SD card's seat on the shared SPI bus.
//!
//! The SD card is one chip-selected device among possibly several
//! (radios share the bus on some boards), so it rides a
//! [`SharedSpiDevice`](crate::spi_bus::SharedSpiDevice) instead of
//! owning the bus. The bin builds the bus driver (frequency, mode,
//! pins - hardware init is bin-owned), parks it via
//! [`crate::spi_bus::init_shared_bus`], and hands `Store::init` the
//! SD's device handle; this module only assembles the
//! `embedded-sdmmc` stack on top.

use drivers::sdcard::{RtcTimeSource, SdCard, VolumeManager};
use esp_hal::delay::Delay;

use crate::spi_bus::SharedSpiDevice;

/// Concrete SdCard type over the shared-bus device.
pub type EspSdCard = SdCard<SharedSpiDevice, Delay>;

/// VolumeManager backed by [`EspSdCard`] + `RtcTimeSource`.
/// `RtcTimeSource` is zero-sized and reads a shared wall clock that
/// firmware updates from the RTC (see `drivers::sdcard::update_wall_clock`).
pub type EspVolumeManager = VolumeManager<EspSdCard, RtcTimeSource>;

/// Wrap the SD card's shared-bus seat into an [`EspSdCard`]. The
/// `Delay` covers the card's protocol timing needs.
pub fn build_sdcard(dev: SharedSpiDevice) -> EspSdCard {
    SdCard::new(dev, Delay::new())
}
