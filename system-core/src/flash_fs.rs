//! LittleFS-backed persistent storage.
//!
//! Mounts a LittleFS filesystem on a dedicated flash region and
//! exposes the two access patterns firmware needs:
//!
//! * **Versioned config blobs** - `load_blob<T>` / `save_blob<T>`
//!   for `Config` / `AlarmState` style value types, wrapped in
//!   `StoredBlob { version, inner }` and postcard-serialised.
//! * **Text log files** - `append_line` / `for_each_line` for the
//!   event log.
//!
//! ## Flash layout
//!
//! This module is board-agnostic: the bin crate passes its
//! [`FlashRegion`] (start + size) into [`FlashFs::mount_or_format`].
//! The bin owns its partition CSV and is the single source of truth
//! for its own flash geometry; firmware doesn't read the partition
//! table at runtime, so the region passed in must match the bin's
//! `storage` partition exactly or writes land in the wrong place.
//! Block size is the flash sector size (4 KB) on every supported
//! part.
//!
//! ## Layout on the filesystem
//!
//! ```text
//! /system/
//! ├── config/
//! │   └── config.bin   // postcard(StoredBlob<Config>) - the whole
//! │                    // settings tree, alarms included
//! ├── logs/
//! │   └── events.log   // one CSV line per event
//! └── sounds/          // reserved for future alarm audio
//! ```
//!
//! The `/system/` prefix matches the SD-card layout, so the SD
//! mirror built in Stage C becomes a trivial byte-for-byte file
//! copy with no path translation. Future user-synced content
//! would land under `/user/` (not yet defined).

use alloc::vec::Vec;
use core::ops::ControlFlow;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_hal::peripherals::FLASH;
use esp_storage::{FlashStorage, FlashStorageError};
use littlefs_rust::{
    Config as LfsConfig, FileType, Filesystem, OpenFlags,
    Storage as LfsStorage,
};
pub use littlefs_rust::Error as LfsError;
use serde::{Deserialize, Serialize};

use crate::fs::{unwrap_blob, wrap_blob};

// -- Region geometry --------------------------------------------------------

/// Flash erase-sector size, in bytes. Board-agnostic - this is the
/// NOR flash sector size on every supported part (4 KB).
const BLOCK_SIZE: u32 = 4096;

/// The flash slice this filesystem lives in.
///
/// Passed in by the bin crate, which owns its partition CSV and is
/// the single source of truth for its own flash geometry. This crate
/// stays board-agnostic: no `cfg`-selected region constants, no board
/// identity. `start` and `size` must be `BLOCK_SIZE`-aligned and
/// match the bin's `storage` partition exactly - drift lands writes
/// in the wrong region (firmware doesn't read the partition table at
/// runtime).
#[derive(Debug, Clone, Copy)]
pub struct FlashRegion {
    /// Byte offset of the region from the base of flash.
    pub start: u32,
    /// Region length in bytes.
    pub size: u32,
}

impl FlashRegion {
    pub const fn new(start: u32, size: u32) -> Self {
        Self { start, size }
    }

    /// Exclusive end offset.
    pub const fn end(&self) -> u32 {
        self.start + self.size
    }

    /// Number of LittleFS blocks (erase sectors) in the region.
    pub const fn block_count(&self) -> u32 {
        self.size / BLOCK_SIZE
    }
}

// -- Storage adapter --------------------------------------------------------

/// Bridges `esp_storage::FlashStorage` (byte-addressable, NorFlash
/// trait) to `littlefs_rust::Storage` (block-addressable). Owns the
/// underlying `FlashStorage`.
pub struct FlashFsStorage<'d> {
    flash: FlashStorage<'d>,
    /// Byte offset of the region base - block addresses are relative
    /// to this. Supplied by the bin via [`FlashRegion`].
    start: u32,
}

impl<'d> FlashFsStorage<'d> {
    pub fn new(flash: FLASH<'d>, start: u32) -> Self {
        Self { flash: FlashStorage::new(flash), start }
    }
}

impl<'d> LfsStorage for FlashFsStorage<'d> {
    fn read(&mut self, block: u32, offset: u32, buf: &mut [u8]) -> Result<(), LfsError> {
        let addr = self.start + block * BLOCK_SIZE + offset;
        self.flash.read(addr, buf).map_err(map_storage_err)
    }

    fn write(&mut self, block: u32, offset: u32, data: &[u8]) -> Result<(), LfsError> {
        let addr = self.start + block * BLOCK_SIZE + offset;
        self.flash.write(addr, data).map_err(map_storage_err)
    }

    fn erase(&mut self, block: u32) -> Result<(), LfsError> {
        let from = self.start + block * BLOCK_SIZE;
        let to   = from + BLOCK_SIZE;
        self.flash.erase(from, to).map_err(map_storage_err)
    }
}

fn map_storage_err(e: FlashStorageError) -> LfsError {
    // Most FlashStorageError variants are I/O faults from the ROM
    // flash routines; `NotAligned` / `OutOfBounds` shouldn't happen
    // given our block geometry but map to `Invalid` for completeness.
    match e {
        FlashStorageError::NotAligned | FlashStorageError::OutOfBounds => LfsError::Invalid,
        _ => LfsError::Io,
    }
}

// -- FlashFs ----------------------------------------------------------------

/// High-level access to the on-flash filesystem.
///
/// Thin wrapper around `littlefs_rust::Filesystem` that owns the
/// mount and exposes helpers the rest of the firmware uses. Created
/// once at boot by [`FlashFs::mount_or_format`].
pub struct FlashFs<'d> {
    fs: Filesystem<FlashFsStorage<'d>>,
    /// Region length in bytes - reported by [`Self::usage`].
    region_size: u32,
}

impl<'d> FlashFs<'d> {
    /// Mount the filesystem, formatting first if the mount fails.
    /// Blank flash / corrupted superblock / version mismatch all
    /// fall through the format path on first boot.
    ///
    /// `region` is the flash slice to host the filesystem in,
    /// supplied by the bin (single source of truth for its own
    /// partition geometry). Panics on format-then-mount failure; at
    /// that point the flash hardware itself is suspect.
    pub fn mount_or_format(flash: FLASH<'d>, region: FlashRegion) -> Self {
        let block_count = region.block_count();
        let storage = FlashFsStorage::new(flash, region.start);
        let fs = match Filesystem::mount(storage, LfsConfig::new(BLOCK_SIZE, block_count)) {
            Ok(fs) => {
                log::info!("flash_fs: mounted ({} blocks, {} KB)", block_count, region.size / 1024);
                fs
            }
            Err((e, mut storage)) => {
                log::warn!("flash_fs: mount failed ({:?}), formatting", e);
                Filesystem::format(&mut storage, &LfsConfig::new(BLOCK_SIZE, block_count))
                    .expect("flash_fs: format failed");
                let fs = Filesystem::mount(storage, LfsConfig::new(BLOCK_SIZE, block_count))
                    .map_err(|(e, _)| e)
                    .expect("flash_fs: mount after format failed");
                log::info!("flash_fs: formatted + mounted");
                fs
            }
        };
        // Ensure the firmware-owned directory tree exists. Each
        // `mkdir` returns `Exists` once the dir is there, so after
        // first boot these become cheap no-ops. Running them on
        // every mount (not just after format) keeps the layout
        // self-healing if an earlier build used a different layout.
        let _ = fs.mkdir("/system");
        let _ = fs.mkdir("/system/config");
        let _ = fs.mkdir("/system/logs");
        let _ = fs.mkdir("/system/sounds");
        Self { fs, region_size: region.size }
    }

    // -- Versioned blob helpers --------------------------------------------

    /// Read a postcard-serialised, version-tagged blob at `path`.
    ///
    /// Returns `None` if the file is missing, the record's version
    /// doesn't match `expected_version`, or deserialisation fails.
    /// Callers fall back to `T::default()` on `None`.
    ///
    /// Normally reached via `Store::load_blob`; kept on `FlashFs`
    /// itself so the `store.flash_mut()` escape hatch remains useful.
    pub fn load_blob<T>(&self, path: &str, expected_version: u8) -> Option<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let bytes = self.read_file_inner(path)?;
        unwrap_blob(&bytes, expected_version)
    }

    /// Write a postcard-serialised, version-tagged blob to `path`.
    /// Creates parent directories on demand.
    ///
    /// Normally reached via `Store::save_blob` (which also mirrors
    /// the same bytes to SD when the card is online). Kept on
    /// `FlashFs` so flash-only saves are still available via
    /// `store.flash_mut()`.
    pub fn save_blob<T>(&mut self, path: &str, version: u8, value: &T)
    where
        T: Serialize,
    {
        let mut buf = [0u8; 512];
        let Some(bytes) = wrap_blob(&mut buf, version, value) else { return };
        if let Err(e) = self.fs.write_file(path, bytes) {
            log::warn!("flash_fs: write {} failed: {:?}", path, e);
        }
    }

    /// Internal whole-file read: `None` on missing or I/O error.
    /// Shared by `load_blob` and the public `read_file`.
    fn read_file_inner(&self, path: &str) -> Option<Vec<u8>> {
        match self.fs.read_to_vec(path) {
            Ok(v) => Some(v),
            Err(LfsError::NoEntry) => None,
            Err(e) => {
                log::warn!("flash_fs: read {} failed: {:?}", path, e);
                None
            }
        }
    }

    // -- Log file helpers --------------------------------------------------

    /// Append `bytes` to the file at `path`, creating it if missing.
    /// The caller is responsible for terminating lines with `\n`.
    pub fn append_line(&mut self, path: &str, bytes: &[u8]) -> Result<(), LfsError> {
        let file = self.fs.open(
            path,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::APPEND,
        )?;
        let mut off = 0;
        while off < bytes.len() {
            let n = file.write(&bytes[off..])? as usize;
            if n == 0 {
                return Err(LfsError::Io);
            }
            off += n;
        }
        file.close()
    }

    /// Stream every line of the file at `path` through `callback`.
    ///
    /// The callback gets the line without its trailing newline. It
    /// may return `ControlFlow::Break(())` to stop the scan early.
    /// If the file doesn't exist the scan returns `Ok(0)` - that's
    /// a legitimate state (no events logged yet), not an error.
    pub fn for_each_line<F>(&mut self, path: &str, mut callback: F) -> Result<usize, LfsError>
    where
        F: FnMut(&str) -> ControlFlow<()>,
    {
        let file = match self.fs.open(path, OpenFlags::READ) {
            Ok(f) => f,
            Err(LfsError::NoEntry) => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut io_buf = [0u8; 256];
        let mut line: heapless::Vec<u8, 96> = heapless::Vec::new();
        let mut visited = 0usize;
        let mut truncated = false;

        loop {
            let n = file.read(&mut io_buf)? as usize;
            if n == 0 {
                break;
            }
            for &b in &io_buf[..n] {
                if b == b'\n' {
                    if !truncated {
                        if let Ok(s) = core::str::from_utf8(&line) {
                            let s = s.strip_suffix('\r').unwrap_or(s);
                            if callback(s).is_break() {
                                let _ = file.close();
                                return Ok(visited + 1);
                            }
                        }
                    }
                    visited += 1;
                    line.clear();
                    truncated = false;
                } else if !truncated && line.push(b).is_err() {
                    truncated = true;
                    line.clear();
                }
            }
        }
        // Trailing partial line without newline.
        if !line.is_empty() && !truncated {
            if let Ok(s) = core::str::from_utf8(&line) {
                let _ = callback(s);
                visited += 1;
            }
        }
        file.close()?;
        Ok(visited)
    }

    /// Read the entire file at `path` into a `Vec`. Returns `None`
    /// on missing file; logs a warning and returns `None` on other
    /// I/O errors so callers can treat "read failed" uniformly.
    pub fn read_file(&mut self, path: &str) -> Option<Vec<u8>> {
        self.read_file_inner(path)
    }

    /// Enumerate regular files in `dir`, invoking `callback` with
    /// each filename (no path prefix). Non-file entries and any
    /// names that don't fit the internal scratch buffer are skipped.
    /// Missing directory is a silent no-op - that's a legitimate
    /// "nothing persisted yet" state, not an error.
    ///
    /// Used by `Store::backfill_config` to learn which blobs need
    /// mirroring to SD on probe.
    pub fn for_each_file<F>(&self, dir: &str, mut callback: F)
    where
        F: FnMut(&str) -> ControlFlow<()>,
    {
        let Ok(entries) = self.fs.list_dir(dir) else { return };
        for entry in entries {
            if entry.file_type != FileType::File {
                continue;
            }
            let mut name: heapless::String<64> = heapless::String::new();
            if core::fmt::Write::write_fmt(&mut name, format_args!("{}", entry.name)).is_err() {
                continue;
            }
            if callback(&name).is_break() {
                return;
            }
        }
    }

    /// Write `bytes` to `path`, creating or truncating the file.
    /// Parent directories must already exist (mount_or_format
    /// creates the `/system/` tree up front).
    pub fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), LfsError> {
        self.fs.write_file(path, bytes)
    }

    /// Delete the file at `path`. Treats "already gone" as success
    /// so callers in recovery paths don't have to special-case the
    /// missing-file race. Used by [`Store::append_line`] to clear a
    /// `LfsError::Corrupt` file before retrying the append.
    pub fn reset_file(&mut self, path: &str) -> Result<(), LfsError> {
        match self.fs.remove(path) {
            Ok(()) | Err(LfsError::NoEntry) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Directories whose contents get deleted by [`Self::reset_user_data`].
    ///
    /// The policy: firmware-written data that should revert to its
    /// default state on factory reset goes here. User-authored
    /// content that a user would expect to survive (e.g. uploaded
    /// alarm sounds) deliberately does **not** go here.
    ///
    /// When you add a new persistence category, decide: does factory
    /// reset restore it to default (list it here) or preserve it
    /// (don't)?
    const FLASH_RESET_DIRS: &'static [&'static str] = &[
        "/system/config",
        "/system/logs",
    ];

    /// Delete every regular file inside every directory named in
    /// [`Self::FLASH_RESET_DIRS`]. The filesystem itself stays
    /// mounted; only listed content is wiped. Wired to the Storage
    /// settings "Factory reset" button.
    ///
    /// This is not a full reformat. If you need a true nuke, add a
    /// `reformat()` method that drives the filesystem through
    /// format + remount instead.
    pub fn reset_user_data(&mut self) {
        for dir in Self::FLASH_RESET_DIRS {
            let Ok(entries) = self.fs.list_dir(dir) else { continue };
            for entry in entries {
                let mut path: heapless::String<96> = heapless::String::new();
                if core::fmt::Write::write_fmt(&mut path, format_args!("{}/{}", dir, entry.name)).is_err() {
                    continue;
                }
                let _ = self.fs.remove(&path);
            }
        }
        log::info!("flash_fs: reset_user_data complete");
    }

    /// Approximate filesystem usage, expressed for the settings
    /// screen. `files` is the total number of regular files across
    /// the known directories, `total_bytes` is the region size.
    pub fn usage(&self) -> FsUsage {
        let mut files = 0u32;
        for dir in ["/system/config", "/system/logs", "/system/sounds"] {
            if let Ok(entries) = self.fs.list_dir(dir) {
                files += entries.iter().filter(|e| e.file_type == FileType::File).count() as u32;
            }
        }
        FsUsage { files, total_bytes: self.region_size }
    }
}

/// Summary of filesystem usage, returned by [`FlashFs::usage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsUsage {
    pub files: u32,
    pub total_bytes: u32,
}

// -- Storage trait impl -----------------------------------------------------

impl<'d> crate::fs::Storage for FlashFs<'d> {
    type Error = LfsError;

    fn append_line(&mut self, path: &str, bytes: &[u8]) -> Result<(), Self::Error> {
        self.append_line(path, bytes)
    }

    fn for_each_line<F>(&mut self, path: &str, callback: F) -> Result<usize, Self::Error>
    where
        F: FnMut(&str) -> core::ops::ControlFlow<()>,
    {
        self.for_each_line(path, callback)
    }

    fn read_file(&mut self, path: &str) -> Option<Vec<u8>> {
        self.read_file(path)
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), Self::Error> {
        self.write_file(path, bytes)
    }

    fn reset_user_data(&mut self) {
        self.reset_user_data();
    }
}
