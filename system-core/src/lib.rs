#![no_std]

//! Board-agnostic system brain shared by the board bin crates.
//!
//! The whole board-agnostic system layer lives here, written once and
//! board-blind: the event loop, render loop, tick state machine, the
//! bus channels/signals, the peripheral tasks, the display
//! state-transition helper, and the storage subsystem. Everything
//! genuinely per-board is injected through the `Board` seam or
//! constructed in the bin and passed to `SystemManager::new` (pin
//! maps, peripheral construction, the `Board` impl, and the flash
//! region geometry all stay in the bin).

// `manager` (mic drain buffer) uses `alloc::boxed::Box`. The bin
// crate provides the `#[global_allocator]`; a library only needs the
// `alloc` crate in scope.
extern crate alloc;

// Storage subsystem: the per-backend contract + blob helpers, the
// LittleFS flash backend (region passed in by the bin), and below
// them the SD backend + the `Store` composite + event log as they
// land.
pub mod fs;
pub mod flash_fs;
pub mod sdcard_hal;
pub mod spi_bus;
pub mod sd_fs;
pub mod storage;
pub mod event_log;

pub mod audio;
pub mod audio_hal;

pub mod board;
pub mod bus;
pub mod clock_math;
pub mod display;
pub mod tasks;
pub mod manager;

// Session-based WiFi (NTP time sync). Feature-gated so bins that
// haven't wired WiFi yet don't pay esp-radio's build cost - the same
// opt-in idea as the drivers crate's per-chip features.
#[cfg(feature = "wifi")]
pub mod wifi;
