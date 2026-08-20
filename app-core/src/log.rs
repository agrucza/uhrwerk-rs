//! Event-log line format + parser.
//!
//! The writer side lives in `firmware::system::event_log` (needs SD
//! I/O); the line format and its parser are pure value logic and
//! live here so they're host-testable and reusable by both
//! read-path consumers (text viewer, battery settings graph).
//!
//! ## Format
//!
//! One event per line:
//!
//! ```text
//! <seq>,YYYY-MM-DDTHH:MM:SS,<tag>[,<detail>]
//! ```
//!
//! * `<seq>` is a monotonic `u32`, flash-persistent across reboots.
//!   Drives SD mirror back-fill ("copy entries with seq > last
//!   mirrored"). See `firmware::system::event_log` for how it's
//!   maintained.
//! * Timestamp uses 1-indexed month / day / 24h time (no timezone).
//! * `<tag>` is a short ASCII identifier. See
//!   [`crate::events::classify_for_log`] for the canonical set.
//! * `<detail>`, when present, is an integer (battery percent, etc.).
//!
//! Lines that don't match this shape are dropped by the parser. The
//! log is append-only, so a corrupt tail line from an interrupted
//! write won't block reading the rest of the file.

use crate::data::TimeData;

/// Upper bound on the tag field. The longest tag we currently emit
/// is `"timer_expired"` (13 bytes); 20 leaves headroom for future
/// tags without forcing a heap allocation.
pub const MAX_TAG_LEN: usize = 20;

/// One parsed event-log line.
///
/// `tag` is a fixed-capacity [`heapless::String`] so the whole entry
/// is fully owned (no borrow into a shared buffer) but not `Copy`.
/// `Default` is derived so callers can allocate a fixed-size buffer
/// via `core::array::from_fn(|_| LogEntry::default())` and hand it
/// to the readers as `&mut [LogEntry]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogEntry {
    pub seq: u32,
    pub time: TimeData,
    pub tag: heapless::String<MAX_TAG_LEN>,
    pub detail: Option<u32>,
    /// Second detail field, present only on lines written as
    /// `<tag>,<detail>,<detail2>` (today: `battery,<percent>,<mv>`).
    /// `None` on single-detail lines, which includes every line
    /// written before the field existed - old logs stay readable.
    pub detail2: Option<u32>,
}

/// Parse one `<seq>,YYYY-MM-DDTHH:MM:SS,tag[,detail]` line.
///
/// Returns `None` on any shape mismatch: non-numeric seq, missing
/// separator, wrong timestamp shape, non-numeric detail, tag longer
/// than [`MAX_TAG_LEN`], or invalid UTF-8 in the tag. Trailing
/// `\r` / `\n` is tolerated.
pub fn parse_log_line(line: &str) -> Option<LogEntry> {
    // Strip any trailing CR/LF so Windows-style line endings parse.
    let line = line.trim_end_matches(|c: char| c == '\n' || c == '\r');

    // Split off the seq prefix. `<seq>,<rest>` where rest is still
    // the "YYYY-...,tag[,detail]" tail.
    let (seq_str, rest) = line.split_once(',')?;
    let seq = seq_str.parse::<u32>().ok()?;

    // Remainder must hold at least "YYYY-MM-DDTHH:MM:SS,x" = 21 bytes.
    if rest.len() < 21 {
        return None;
    }
    let ts  = rest.get(0..19)?;
    let sep = rest.as_bytes().get(19)?;
    if *sep != b',' {
        return None;
    }
    // Validate the internal timestamp separators.
    let b = ts.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }

    let year   = ts.get(0..4)?.parse::<u16>().ok()?;
    let month  = ts.get(5..7)?.parse::<u8>().ok()?;
    let day    = ts.get(8..10)?.parse::<u8>().ok()?;
    let hour   = ts.get(11..13)?.parse::<u8>().ok()?;
    let minute = ts.get(14..16)?.parse::<u8>().ok()?;
    let second = ts.get(17..19)?.parse::<u8>().ok()?;

    // Remainder is tag[,detail[,detail2]]. Split at the first comma,
    // then optionally again: a second detail is what the battery
    // lines use to carry millivolts next to the percent. Parsing it
    // is not optional politeness - before this existed, the single
    // `parse::<u32>()` below rejected the WHOLE line on a second
    // field, which would have silently dropped every battery entry
    // from both the history and the seq scan.
    let body = &rest[20..];
    let (tag_str, detail, detail2) = match body.find(',') {
        Some(c) => {
            let rest_detail = &body[c + 1..];
            match rest_detail.find(',') {
                Some(d) => {
                    let first = rest_detail[..d].parse::<u32>().ok()?;
                    let second = rest_detail[d + 1..].parse::<u32>().ok()?;
                    (&body[..c], Some(first), Some(second))
                }
                None => (&body[..c], Some(rest_detail.parse::<u32>().ok()?), None),
            }
        }
        None => (body, None, None),
    };
    if tag_str.is_empty() {
        return None;
    }

    let mut tag: heapless::String<MAX_TAG_LEN> = heapless::String::new();
    tag.push_str(tag_str).ok()?;

    Some(LogEntry {
        seq,
        time: TimeData { hour, minute, second, year, month, day },
        tag,
        detail,
        detail2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(s: &str) -> heapless::String<MAX_TAG_LEN> {
        let mut t = heapless::String::new();
        t.push_str(s).unwrap();
        t
    }

    #[test]
    fn parses_tagless_detail_line() {
        let e = parse_log_line("1,2026-04-22T19:03:14,alarm").unwrap();
        assert_eq!(e.seq, 1);
        assert_eq!(e.time.year, 2026);
        assert_eq!(e.time.month, 4);
        assert_eq!(e.time.day, 22);
        assert_eq!(e.time.hour, 19);
        assert_eq!(e.time.minute, 3);
        assert_eq!(e.time.second, 14);
        assert_eq!(e.tag, tag("alarm"));
        assert_eq!(e.detail, None);
    }

    #[test]
    fn parses_battery_line_with_detail() {
        let e = parse_log_line("42,2026-04-22T19:10:01,battery,85").unwrap();
        assert_eq!(e.seq, 42);
        assert_eq!(e.tag, tag("battery"));
        assert_eq!(e.detail, Some(85));
        // Single-detail lines predate the voltage field and must keep
        // parsing - the flash log on a device that has been running
        // is full of them.
        assert_eq!(e.detail2, None);
    }

    #[test]
    fn parses_battery_line_with_percent_and_millivolts() {
        let e = parse_log_line("43,2026-04-22T19:12:33,battery,84,3912").unwrap();
        assert_eq!(e.seq, 43);
        assert_eq!(e.tag, tag("battery"));
        assert_eq!(e.detail, Some(84));
        assert_eq!(e.detail2, Some(3912));
    }

    #[test]
    fn rejects_line_whose_second_detail_is_not_a_number() {
        // The second field is parsed, not skipped: garbage there must
        // fail the line rather than silently yield a percent-only
        // sample that looks trustworthy.
        assert!(parse_log_line("44,2026-04-22T19:13:00,battery,84,abc").is_none());
    }

    #[test]
    fn parses_large_seq() {
        let e = parse_log_line("4000000000,2026-04-22T19:03:14,boot").unwrap();
        assert_eq!(e.seq, 4_000_000_000);
    }

    #[test]
    fn tolerates_trailing_newline() {
        let e = parse_log_line("7,2026-04-22T19:03:14,boot\n").unwrap();
        assert_eq!(e.tag, tag("boot"));
    }

    #[test]
    fn tolerates_crlf() {
        let e = parse_log_line("7,2026-04-22T19:03:14,boot\r\n").unwrap();
        assert_eq!(e.tag, tag("boot"));
    }

    #[test]
    fn rejects_short_line() {
        assert!(parse_log_line("").is_none());
        assert!(parse_log_line("1,2026-04-22T19:03:14,").is_none());
    }

    #[test]
    fn rejects_missing_seq() {
        // No leading seq column
        assert!(parse_log_line("2026-04-22T19:03:14,boot").is_none());
    }

    #[test]
    fn rejects_non_numeric_seq() {
        assert!(parse_log_line("abc,2026-04-22T19:03:14,boot").is_none());
    }

    #[test]
    fn rejects_malformed_timestamp() {
        assert!(parse_log_line("1,2026 04 22T19:03:14,boot").is_none());
        assert!(parse_log_line("1,2026-04-22 19:03:14,boot").is_none());
        assert!(parse_log_line("1,YYYY-04-22T19:03:14,boot").is_none());
    }

    #[test]
    fn rejects_non_numeric_detail() {
        assert!(parse_log_line("1,2026-04-22T19:10:01,battery,full").is_none());
    }

    #[test]
    fn rejects_oversized_tag() {
        // 21 chars - one more than MAX_TAG_LEN.
        let line = "1,2026-04-22T19:03:14,aaaaaaaaaaaaaaaaaaaaa";
        assert!(parse_log_line(line).is_none());
    }
}
