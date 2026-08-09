#![no_std]

//! Display hardware-init helpers shared by the board bin crates.
//!
//! This crate is the display HAL: the esp-hal-coupled QSPI bus build,
//! the CO5300 driver wrapper, `init_display`, and the framebuffer
//! statics. The bin crates carry their own `board.rs` (pin maps) and
//! call into this crate for display bring-up.
//!
//! Everything else board-agnostic (the system layer + the storage
//! subsystem, including `flash_fs`/`fs`) lives in `system-core`.

#[cfg(feature = "psram-fb")]
extern crate alloc;

pub mod display;
