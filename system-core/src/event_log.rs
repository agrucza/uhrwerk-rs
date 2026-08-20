//! Event log (flash + optional SD mirror).
//!
//! Appends a CSV line for each loggable [`SystemEvent`] to the
//! on-flash event log (always, via [`crate::storage::Store`])
//! and also to the SD card if present. Both sides use an identical
//! text format, so the SD mirror is a straight file-append - no
//! translation step.
//!
//! ## Format
//!
//! ```text
//! <seq>,YYYY-MM-DDTHH:MM:SS,<tag>[,<detail>]
//! ```
//!
//! * `<seq>` is a flash-persistent monotonic `u32`. Recovered at
//!   boot by scanning the flash log for the highest seen value +
//!   1. Drives the SD back-fill ("copy entries with
//!   `seq > last_mirrored`").
//! * Timestamp is local wall time from the PCF85063 (no timezone).
//! * `<tag>` is the static string from [`LoggedEvent`].
//! * `<detail>`, when present, is a single integer (e.g. battery percent).
//!
//! ## Layout on disk
//!
//! Identical on both sides: `/system/logs/events.log`. Created
//! on first write. The `/system/` prefix keeps firmware state
//! separate from any user content on the SD card, and mirrors the
//! same layout on flash so the mirror is a byte-for-byte file copy
//! with no path translation.
//!
//! ## I/O strategy
//!
//! Events are rare (alarms, timer expiries, battery-percent changes)
//! so we open-append-close the file on each side for every line.
//! That keeps both storage handles easy to share with other code
//! without lifetime gymnastics, at the cost of a few extra protocol
//! round-trips per write. Fine at this access rate.
//!
//! ## Failure handling
//!
//! Every call is best-effort. Flash or SD write failures get a
//! single `log::warn!` and continue. SD write failures additionally
//! flip the `Store`'s online flag off so later writes skip the SD
//! side until the user re-probes.

use crate::storage::Store;
use app_core::data::{BatteryHistory, BatterySample, TimeData};
use app_core::events::{LoggedEvent, SystemEvent, classify_for_log};
use app_core::log::parse_log_line;
use core::fmt::Write as _;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU32, Ordering};

/// Log file path - identical on both backends so the SD mirror is
/// a byte-for-byte copy of the flash log.
const LOG_PATH: &str = "/system/logs/events.log";

/// Next sequence number to emit. Initialised at boot by
/// [`init_seq_from_flash`] (which scans `/system/logs/events.log` and sets
/// this to `max_seq + 1`). Incremented on every append.
static NEXT_SEQ: AtomicU32 = AtomicU32::new(1);

/// Scan the on-flash event log at boot to recover the monotonic
/// sequence counter. Must be called once from `SystemManager::init`
/// after the `Store` is constructed and before any [`try_log`] /
/// [`log_boot`] call.
pub fn init_seq_from_flash(store: &mut Store) {
    let mut max_seq = 0u32;
    let _ = store.flash_mut().for_each_line(LOG_PATH, |line| {
        if let Some(entry) = parse_log_line(line) {
            if entry.seq > max_seq {
                max_seq = entry.seq;
            }
        }
        ControlFlow::Continue(())
    });

    // Probe the append path. If the file's metadata is damaged from
    // a prior fault, opening WRITE|APPEND will surface `LfsError::Corrupt`,
    // which `Store::append_line` self-heals by deleting the file and
    // retrying. After this call returns, the file is either intact
    // (probe is a no-op) or freshly reset (seq counter restarts at 1).
    store.append_line(LOG_PATH, b"");
    NEXT_SEQ.store(max_seq.wrapping_add(1), Ordering::Relaxed);
    log::info!("event_log: resumed at seq {}", max_seq + 1);
}

/// Boot-seed the battery-history ring buffer from the flash event
/// log. Walks the file once, pushing every `battery,<percent>` line
/// through `BatteryHistory::push` so the most recent
/// `BATTERY_HISTORY_CAP` samples land in `out` with older ones
/// naturally aged out.
///
/// Called from `SystemManager::init` before `Model::new`, so the
/// first render of the battery settings screen already has data.
pub fn load_battery_history(store: &mut Store, out: &mut BatteryHistory) {
    let mut seen = 0u32;
    let _ = store.flash_mut().for_each_line(LOG_PATH, |line| {
        if let Some(entry) = parse_log_line(line) {
            if entry.tag.as_str() == "battery" {
                if let Some(pct) = entry.detail {
                    // detail2 carries millivolts on lines written
                    // since the pair landed; older lines have none and
                    // restore with `voltage_mv: None`.
                    out.push(BatterySample {
                        time: entry.time,
                        percent: pct as u8,
                        voltage_mv: entry.detail2.map(|mv| mv as u16),
                    });
                    seen += 1;
                }
            }
        }
        ControlFlow::Continue(())
    });
    if seen > 0 {
        log::info!(
            "event_log: seeded battery history ({} scanned, kept {})",
            seen, out.len(),
        );
    }
}

/// Copy any flash log entry whose `seq` is newer than the highest
/// seq already present on the SD mirror. Called internally by
/// `Store::probe_sd` after a successful probe.
///
/// Uses the SD file itself as the "last mirrored" marker: scan it,
/// track the max seq we see, copy flash entries with `seq > sd_max`.
/// No separate state file - if the SD log is trimmed, truncated, or
/// restored from a backup, the next back-fill self-corrects against
/// whatever is currently on the card.
///
/// If the SD scan fails we bail and leave the online flag alone;
/// the next real append through `Store::append_line` will flip it
/// off if the card is genuinely gone.
pub fn backfill_sd(store: &mut Store) {
    if !store.sd_online() {
        return;
    }

    // Split borrow: flash and SD are disjoint fields on the Store,
    // so we can hold both `&mut`s at once and stream flash->SD in
    // a single pass without intermediate buffering. `sd` is `None`
    // on boards with no SD slot - but `sd_online()` is always false
    // there, so we already returned above; the `else` is for safety.
    let (flash, Some(sd)) = store.parts_mut() else { return };

    // Phase 1: learn the SD's highest seq.
    let mut sd_max: u32 = 0;
    let scan = sd.for_each_line(LOG_PATH, |line| {
        if let Some(entry) = parse_log_line(line) {
            if entry.seq > sd_max {
                sd_max = entry.seq;
            }
        }
        ControlFlow::Continue(())
    });
    if scan.is_err() {
        log::warn!("event_log: backfill SD scan failed");
        return;
    }

    // Phase 2: walk the flash log, append-to-SD anything past sd_max.
    // On SD write failure we stop early so we don't spam warnings.
    // Entries that already landed are committed - the next probe
    // resumes from whatever SD now holds.
    let mut copied = 0u32;
    let mut aborted = false;
    let _ = flash.for_each_line(LOG_PATH, |line| {
        if aborted { return ControlFlow::Break(()); }
        let Some(entry) = parse_log_line(line) else {
            return ControlFlow::Continue(());
        };
        if entry.seq <= sd_max {
            return ControlFlow::Continue(());
        }

        // Re-assemble the on-disk line: parser strips the trailing
        // newline, we want the mirror to be byte-identical.
        let mut buf: heapless::String<96> = heapless::String::new();
        if core::fmt::Write::write_fmt(&mut buf, format_args!("{}\n", line)).is_err() {
            return ControlFlow::Continue(());
        }
        if let Err(e) = sd.append_line(LOG_PATH, buf.as_bytes()) {
            log::warn!(
                "event_log: backfill SD append failed at seq {} ({:?}), stopping",
                entry.seq, e,
            );
            aborted = true;
            return ControlFlow::Break(());
        }
        copied += 1;
        ControlFlow::Continue(())
    });

    if copied > 0 {
        log::info!(
            "event_log: backfilled {} entries to SD (from seq {} -> {})",
            copied, sd_max, sd_max + copied,
        );
    }
}

/// Classify and append `event` to the flash log and, if the SD
/// mirror is online, to the SD log as well. No-op if the event
/// isn't loggable.
pub fn try_log(store: &mut Store, time: &TimeData, event: &SystemEvent) {
    let Some(logged) = classify_for_log(event) else { return };
    write_line(store, time, logged);
}

/// Record a "boot" line at startup. Separate entry point because
/// there's no `SystemEvent::Boot` - boot is just "we started
/// running", emitted directly from the manager after the RTC +
/// Store are up.
pub fn log_boot(store: &mut Store, time: &TimeData) {
    write_line(store, time, LoggedEvent { tag: "boot", detail: None, detail2: None });
}

fn write_line(store: &mut Store, time: &TimeData, event: LoggedEvent) {
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);

    // 64 bytes holds "<u32>,YYYY-MM-DDTHH:MM:SS,<tag>,<u32>\n"
    // with plenty of slack (u32 is 10 digits max, tag ≤ 20).
    let mut line: heapless::String<64> = heapless::String::new();
    let fmt_result = match (event.detail, event.detail2) {
        // Two details: `<tag>,<detail>,<detail2>` (battery percent +
        // millivolts). detail2 without detail cannot be expressed in
        // the format and is treated as detail-only.
        (Some(n), Some(m)) => write!(
            &mut line,
            "{},{:04}-{:02}-{:02}T{:02}:{:02}:{:02},{},{},{}\n",
            seq,
            time.year, time.month, time.day,
            time.hour, time.minute, time.second,
            event.tag, n, m,
        ),
        (Some(n), None) => write!(
            &mut line,
            "{},{:04}-{:02}-{:02}T{:02}:{:02}:{:02},{},{}\n",
            seq,
            time.year, time.month, time.day,
            time.hour, time.minute, time.second,
            event.tag, n,
        ),
        (None, _) => write!(
            &mut line,
            "{},{:04}-{:02}-{:02}T{:02}:{:02}:{:02},{}\n",
            seq,
            time.year, time.month, time.day,
            time.hour, time.minute, time.second,
            event.tag,
        ),
    };
    if fmt_result.is_err() {
        log::warn!("event_log: line buffer overflow, dropping seq {}", seq);
        return;
    }

    store.append_line(LOG_PATH, line.as_bytes());
}
