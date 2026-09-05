//! NFC protocol layer over the reader front end.
//!
//! Sits between the chip's command driver (`drivers::st25r3916`,
//! which speaks only chip verbs) and the firmware task. It owns the
//! protocol logic the driver deliberately leaves out:
//!
//! - [`iso14443a`] - Type A activation: the anticollision cascade,
//!   SELECT, and card-family classification from ATQA/SAK. The pure
//!   parts (BCC, UID assembly, classification) are host-tested.
//! - [`reader`] - the [`reader::Reader`] that transceives on an
//!   already-lit field over a `SpiDevice` and runs the activation to
//!   produce [`app_core::nfc`] value types.
//!
//! The caller (the firmware task) owns the rail, the chip power-up
//! ritual, and the RF field; this crate only speaks to a card on a
//! field that is already up. The transceive path is hardware-only:
//! it depends on the chip's FIFO / interrupt timing and MUST be
//! verified on the watch, one capability at a time.

#![cfg_attr(not(test), no_std)]

pub mod iso14443a;
pub mod reader;

pub use app_core::nfc as types;
