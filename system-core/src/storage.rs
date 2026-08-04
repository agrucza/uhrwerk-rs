//! Unified storage facade: flash + SD mirror behind one handle.
//!
//! `Store` owns the on-flash LittleFS ([`FlashFs`]) and the SD card
//! volume ([`SdFs`]) together with the SD-mirror online flag. It
//! exposes two layers of API:
//!
//! * **Mirrored operations** - `save_blob`, `append_line`,
//!   `reset_user_data`. Flash is authoritative; SD is a best-effort
//!   mirror that gets disabled on the first write failure (pulled
//!   card, read-only media, FS corruption). Callers don't have to
//!   think about which backends are in play.
//!
//! * **Escape hatches** - `flash_mut()` / `sd_mut()` return the
//!   concrete handles for operations that only make sense on one
//!   side (flash-only config reads, SD-only user content, FS-usage
//!   queries).
//!
//! Keeping both layers available on the same type means the
//! SystemManager holds a single `store` field, and each call site
//! picks the semantics it wants at the callsite rather than at the
//! field boundary.
//!
//! Rule of thumb:
//! * `store.save_blob(...)` / `store.append_line(...)` / `store.reset_user_data()`
//!   -> both sides, coordinated.
//! * `store.flash_mut().<op>()` / `store.sd_mut().<op>()`
//!   -> explicit one-side operation.

use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, Ordering};
use esp_hal::gpio::{Output, interconnect::{PeripheralInput, PeripheralOutput}};
use esp_hal::peripherals::FLASH;
use serde::{Deserialize, Serialize};

use crate::sdcard_hal;
use crate::flash_fs::{FlashFs, FlashRegion, FsUsage, LfsError};
use crate::fs::{unwrap_blob, wrap_blob};
use crate::sd_fs::SdFs;

/// Directory holding all versioned config blobs on both backends.
/// Backfill enumerates flash at this path; restore walks the
/// explicit path list supplied by the caller.
const CONFIG_DIR: &str = "/system/config";

/// Scratch buffer size for mirror writes of versioned blobs.
/// Matches `FlashFs`'s historic 512-byte preallocation: biggest
/// record today (`AlarmState`) serialises to well under 256 B, with
/// headroom for growth.
const BLOB_SCRATCH: usize = 512;

/// Flash plus an optional SD mirror behind one handle. See module
/// docs.
///
/// `sd` is `None` on boards with no SD slot. When present, the SD
/// side is constructed once at boot so the SPI peripheral and CS pin
/// stay alive for the whole run - runtime re-probes (Settings
/// "Initialize SD card" button) don't re-plumb any hardware tokens.
/// Actual card readability is tracked by `sd_online()`. When `sd` is
/// `None`, every SD path is a no-op and `sd_online()` is always
/// `false`; the manager never sees the difference.
pub struct Store<'d> {
    flash: FlashFs<'d>,
    sd: Option<SdFs<'d>>,
    /// Whether the SD mirror is currently usable for writes.
    ///
    /// * Flipped `true` when `probe_sd()` succeeds.
    /// * Flipped `false` automatically on the first SD write
    ///   failure so warn-spam stops if the card got yanked.
    /// * User re-arms via `probe_sd()` from the Settings screen.
    /// * Always `false` when `sd` is `None` (no slot on this board).
    ///
    /// Flash writes are unaffected by this flag; flash is
    /// authoritative either way.
    sd_online: AtomicBool,
}

impl<'d> Store<'d> {
    /// Mount flash (format on first boot / corrupted superblock) and
    /// build the SD volume manager. No SD I/O yet - call
    /// [`Self::probe_sd`] afterwards to detect card presence.
    ///
    /// `region` is the flash slice for the LittleFS, owned by the bin
    /// (single source of truth for its own partition geometry).
    pub fn init(
        flash: FLASH<'d>,
        region: FlashRegion,
        spi: impl esp_hal::spi::master::Instance + 'd,
        sck: impl PeripheralOutput<'d>,
        mosi: impl PeripheralOutput<'d>,
        miso: impl PeripheralInput<'d>,
        cs: Output<'d>,
    ) -> Self {
        let flash = FlashFs::mount_or_format(flash, region);
        log::info!("SD card: building volume manager (card access deferred)");
        let sd_card = sdcard_hal::build_sdcard(spi, sck, mosi, miso, cs);
        let sd = SdFs::new(drivers::sdcard::VolumeManager::new(
            sd_card, drivers::sdcard::RtcTimeSource,
        ));
        Self { flash, sd: Some(sd), sd_online: AtomicBool::new(false) }
    }

    /// Flash-only `Store` for boards with no SD slot. Mounts flash
    /// (format on first boot) and leaves the SD side absent: every
    /// mirrored op is flash-only and `sd_online()` stays `false`.
    pub fn init_flash_only(flash: FLASH<'d>, region: FlashRegion) -> Self {
        let flash = FlashFs::mount_or_format(flash, region);
        Self { flash, sd: None, sd_online: AtomicBool::new(false) }
    }

    // -- Online state -------------------------------------------------------

    /// Current SD mirror state.
    pub fn sd_online(&self) -> bool {
        self.sd_online.load(Ordering::Relaxed)
    }

    /// Mark the SD mirror offline. Called on the first write
    /// failure so warn-spam stops; re-armed via `probe_sd()` (auto
    /// on detect-line boards, the Settings button otherwise).
    fn mark_sd_offline(&self) {
        self.sd_online.store(false, Ordering::Relaxed);
    }

    /// The card-detect line reported the card gone: flip the mirror
    /// offline and abandon the cached SD session. The abandon
    /// matters as much as the flag - the cached volume/dir handles
    /// reference a card that no longer exists, and both closing
    /// them later (leaks the volume slot on the failed info-sector
    /// flush - see `SdFs::abandon`) and reusing them (old card's
    /// cluster chains against whatever card comes next) are wrong.
    pub fn on_sd_removed(&mut self) {
        self.mark_sd_offline();
        if let Some(sd) = self.sd.as_mut() {
            sd.abandon();
        }
    }

    // -- Probe + backfill ---------------------------------------------------

    /// Best-effort re-probe used by the periodic recovery hook in
    /// `SystemManager::tick`. No-op when SD is already believed
    /// online; otherwise calls [`Self::probe_sd`] which forces a
    /// fresh card init (so a hot-replug or different-card swap
    /// recovers without the user pressing the Settings button).
    /// Returns `true` if SD is now online.
    pub fn try_recover_sd(&mut self) -> bool {
        if self.sd_online() {
            return true;
        }
        self.probe_sd()
    }

    /// Re-probe the SD slot and update the online flag. On a
    /// successful probe, back-fill both directions of the mirror:
    /// event-log entries newer than the SD's tail (seq-based) and
    /// every versioned config blob (whole-file re-copy). Returns
    /// the new online state.
    pub fn probe_sd(&mut self) -> bool {
        let online = match self.sd.as_mut() {
            Some(sd) => sd.probe(),
            None => false, // no SD slot on this board
        };
        self.sd_online.store(online, Ordering::Relaxed);
        if online {
            crate::event_log::backfill_sd(self);
            self.backfill_config();
        }
        online
    }

    // -- Backfill + restore -------------------------------------------------

    /// Copy every regular file in flash `/system/config/` to the SD
    /// mirror at the same path. Called from [`Self::probe_sd`] once
    /// the card is confirmed online, so a blob saved while the card
    /// was out is mirrored the next time the card comes back.
    ///
    /// Best-effort: on the first SD write failure we log and stop.
    /// The online flag is *not* flipped here - matches event-log
    /// backfill semantics. The next real `save_blob` / `append_line`
    /// is the authority on whether the card is still present.
    fn backfill_config(&mut self) {
        // No SD slot -> nothing to mirror. (Also unreachable via
        // probe_sd, which only calls this when the card is online,
        // but keep the guard so the method is safe to call directly.)
        if self.sd.is_none() {
            return;
        }
        // Snapshot the file list from flash into a small heapless
        // buffer. The split-borrow pattern used by event_log's
        // backfill doesn't work here because we need an owned Vec
        // per file (read whole blob -> write whole blob), so we
        // collect names first and then loop with full `&mut self`.
        let mut names: heapless::Vec<heapless::String<32>, 8> = heapless::Vec::new();
        self.flash.for_each_file(CONFIG_DIR, |name| {
            let mut s: heapless::String<32> = heapless::String::new();
            if s.push_str(name).is_ok() && names.push(s).is_ok() {
                ControlFlow::Continue(())
            } else {
                // Names buffer full - stop early rather than silently
                // skipping the overflow entries.
                ControlFlow::Break(())
            }
        });

        let mut copied = 0u32;
        for name in &names {
            let mut path: heapless::String<96> = heapless::String::new();
            if core::fmt::Write::write_fmt(
                &mut path, format_args!("{}/{}", CONFIG_DIR, name),
            ).is_err() {
                continue;
            }
            let Some(bytes) = self.flash.read_file(&path) else { continue };
            let Some(sd) = self.sd.as_mut() else { return };
            if let Err(e) = sd.write_file(&path, &bytes) {
                log::warn!(
                    "store: config backfill SD write {} failed ({:?}), stopping",
                    path, e,
                );
                return;
            }
            copied += 1;
        }

        if copied > 0 {
            log::info!("store: config backfill mirrored {} blob(s) to SD", copied);
        }
    }

    /// Overwrite the flash copy of each `path` with whatever is on
    /// the SD mirror. Intended for the Settings "Restore config
    /// from SD" flow: user swaps an SD card in and wants to seed
    /// this device's config from it.
    ///
    /// Returns `(copied, skipped)`. `skipped` counts paths that
    /// weren't present on SD or whose reads failed; flash writes
    /// that fail *also* count as skipped (logged individually).
    ///
    /// Byte-copies the wrapped blob as-is - version validation is
    /// deferred to the next `load_blob` at boot. A mismatched
    /// version file will be dropped then and the caller falls back
    /// to defaults, same as any other unreadable blob.
    ///
    /// Caller (the manager) is responsible for rebooting afterwards;
    /// the live Model still holds the pre-restore Config / AlarmState
    /// in memory, and without a reset the next `save_blob` would
    /// clobber whatever we just restored.
    pub fn restore_config_from_sd(&mut self, paths: &[&str]) -> (u32, u32) {
        if !self.sd_online() {
            log::warn!("store: restore requested but SD is offline");
            return (0, paths.len() as u32);
        }

        let mut copied = 0u32;
        let mut skipped = 0u32;
        for path in paths {
            let bytes = match self.sd.as_mut().and_then(|sd| sd.read_file(path)) {
                Some(bytes) => bytes,
                None => {
                    log::info!("store: restore skip {} (not on SD)", path);
                    skipped += 1;
                    continue;
                }
            };
            if let Err(e) = self.flash.write_file(path, &bytes) {
                log::warn!("store: restore flash write {} failed ({:?})", path, e);
                skipped += 1;
                continue;
            }
            copied += 1;
        }
        log::info!(
            "store: restore from SD complete ({} copied, {} skipped)",
            copied, skipped,
        );
        (copied, skipped)
    }

    // -- Escape hatches -----------------------------------------------------

    /// Direct handle on the flash-side backend. Use for flash-only
    /// operations (config reads during boot, FS usage queries, any
    /// operation the SD mirror shouldn't see).
    pub fn flash_mut(&mut self) -> &mut FlashFs<'d> {
        &mut self.flash
    }

    /// Direct handle on the SD-side backend, if this board has one.
    /// Use for SD-only operations (user content on `/user/...`,
    /// back-fill scans, anything that shouldn't touch flash).
    #[allow(dead_code)] // escape hatch for future SD-only callers (user data, sounds, backups)
    pub fn sd_mut(&mut self) -> Option<&mut SdFs<'d>> {
        self.sd.as_mut()
    }

    /// Split borrow: both backends at once. Required by
    /// `event_log::backfill_sd`, which has to stream from flash
    /// into SD in one pass. No invariant couples the two sides,
    /// so handing out both mutable refs is safe - the Store is
    /// just a bundle plus an atomic flag.
    pub fn parts_mut(&mut self) -> (&mut FlashFs<'d>, Option<&mut SdFs<'d>>) {
        (&mut self.flash, self.sd.as_mut())
    }

    /// Flash-side filesystem usage. SD-side usage isn't tracked
    /// here - the UI shows flash as the authoritative store plus a
    /// binary "SD online" indicator.
    pub fn usage(&self) -> FsUsage {
        self.flash.usage()
    }

    // -- Mirrored ops -------------------------------------------------------

    /// Load a versioned blob. Flash is authoritative for config
    /// data, so we don't fall through to SD on a flash miss -
    /// caller's `T::default()` path handles the missing case.
    pub fn load_blob<T>(&self, path: &str, expected_version: u8) -> Option<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.flash.load_blob(path, expected_version)
    }

    /// Save a versioned blob. Writes the flash primary, then
    /// (if the SD mirror is online) the identical bytes to the SD
    /// side so the mirror stays byte-for-byte consistent.
    pub fn save_blob<T>(&mut self, path: &str, version: u8, value: &T)
    where
        T: Serialize,
    {
        // Flash-side write via the existing helper so the
        // `store.flash_mut().save_blob(...)` path stays consistent
        // with this one.
        self.flash.save_blob(path, version, value);

        if !self.sd_online() {
            return;
        }
        let mut buf = [0u8; BLOB_SCRATCH];
        let Some(bytes) = wrap_blob(&mut buf, version, value) else { return };
        // `sd_online()` is only ever true when `sd` is `Some`, so the
        // `else` is unreachable in practice - kept for total safety.
        let Some(sd) = self.sd.as_mut() else { return };
        if let Err(e) = sd.write_file(path, bytes) {
            log::warn!(
                "store: SD mirror save {} failed ({:?}), marking SD offline",
                path, e,
            );
            self.mark_sd_offline();
        }
    }

    /// Append `bytes` to `path` on flash (always) and on SD (if
    /// the mirror is online). First SD failure flips the mirror
    /// offline.
    ///
    /// Flash-side `LfsError::Corrupt` is self-healing: the file is
    /// deleted and the append retried once. On a successful retry
    /// the prior contents are lost (nothing else we can do - the
    /// metadata chain was unrecoverable), but the file is back to a
    /// writable state for every subsequent call.
    pub fn append_line(&mut self, path: &str, bytes: &[u8]) {
        match self.flash.append_line(path, bytes) {
            Ok(()) => {}
            Err(LfsError::Corrupt) => {
                log::warn!(
                    "store: flash {} corrupt, resetting file and retrying", path,
                );
                if let Err(e) = self.flash.reset_file(path) {
                    log::warn!("store: reset {} failed: {:?}", path, e);
                } else if let Err(e) = self.flash.append_line(path, bytes) {
                    log::warn!(
                        "store: retry append {} after reset failed: {:?}", path, e,
                    );
                } else {
                    log::info!(
                        "store: flash {} reset after corruption (prior entries lost)",
                        path,
                    );
                }
            }
            Err(e) => {
                log::warn!("store: flash append {} failed: {:?}", path, e);
            }
        }
        if !self.sd_online() {
            return;
        }
        let Some(sd) = self.sd.as_mut() else { return };
        if let Err(e) = sd.append_line(path, bytes) {
            log::warn!(
                "store: SD mirror append {} failed ({:?}), marking SD offline",
                path, e,
            );
            self.mark_sd_offline();
        }
    }

    /// Wipe firmware-written content on both backends (factory
    /// reset). Each side honours its own reset-dirs list - flash
    /// clears `config/` + `logs/`, SD clears `logs/` only (user
    /// content elsewhere on the card is preserved).
    pub fn reset_user_data(&mut self) {
        self.flash.reset_user_data();
        if self.sd_online() {
            if let Some(sd) = self.sd.as_mut() {
                sd.reset_user_data();
            }
        }
    }

    /// Try to re-interpret a blob from whichever SD file exists
    /// under `path`. Used during gap-fill / recovery paths; no
    /// current caller, but kept so the "SD-side config mirror"
    /// design stays reachable without a second round of plumbing.
    #[allow(dead_code)] // no live caller; kept so the SD-fallback read path stays one line away
    pub fn load_blob_sd<T>(&mut self, path: &str, expected_version: u8) -> Option<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let bytes = self.sd.as_mut()?.read_file(path)?;
        unwrap_blob(&bytes, expected_version)
    }
}
