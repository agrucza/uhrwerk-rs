//! drivers - HAL-agnostic peripheral drivers.
//!
//! Each driver is written against `embedded-hal` / `embedded-hal-async`
//! traits, not against `esp-hal` types directly. This means:
//!
//!  - Drivers are portable across any HAL that implements those traits.
//!  - Drivers can be unit-tested on the host with `embedded-hal-mock`.

#![no_std]

#[cfg(feature = "display")]
pub mod display;

#[cfg(feature = "imu")]
pub mod imu;

#[cfg(feature = "qmi8658")]
pub mod qmi8658;

#[cfg(feature = "pmu")]
pub mod pmu;

#[cfg(feature = "rtc")]
pub mod rtc;

#[cfg(feature = "sdcard")]
pub mod sdcard;

#[cfg(feature = "touch")]
pub mod touch;

#[cfg(feature = "es8311")]
pub mod es8311;

#[cfg(feature = "es7210")]
pub mod es7210;

#[cfg(feature = "xl9555")]
pub mod xl9555;

#[cfg(feature = "drv2605")]
pub mod drv2605;

#[cfg(feature = "bhi260")]
pub mod bhi260;

#[cfg(feature = "ublox")]
pub mod ublox;

pub use embedded_hal;

#[cfg(feature = "defmt")]
pub use defmt;

#[cfg(feature = "pmu")]
pub use pmu::*;

#[cfg(feature = "sdcard")]
pub use sdcard::*;
