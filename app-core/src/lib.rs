//! app-core - hardware-agnostic application logic.
//!
//! This crate is `no_std` when compiled for the embedded target, but compiles
//! with `std` on the host so that `cargo test -p app-core` works without a
//! physical device or cross-compiler.
//!
//! Rules:
//!  - No `esp-hal`, no GPIO, no SPI, no I2C.
//!  - Only pure logic: state machines, event types, UI model, data transforms.
//!  - Value types from hardware-ish crates (`embassy-time` for `Duration` /
//!    `Instant`, `drivers::pmu` enums) are allowed as long as no hardware
//!    side-effect call is made from app-core.

#![cfg_attr(not(test), no_std)]

pub mod buzz;
pub mod card_library;
pub mod commands;
pub mod config;
pub mod data;
pub mod events;
pub mod log;
pub mod model;
pub mod nav;
pub mod nfc;
/// Shared TLV primitives for versioned flash records; only needed by
/// the code that persists (config, card library), hence serde-gated.
#[cfg(feature = "serde")]
pub mod tlv;
pub mod ui;
