//! Shared numeric text formatting for the UI.
//!
//! Every screen that renders a clock-style value goes through these
//! instead of hand-rolling `write!("{:02}:{:02}...")` - one place
//! decides padding and rollover behavior.

use core::fmt::Write;
use heapless::String;

/// `HH:MM:SS` from a total-seconds count. Hours are NOT capped: a
/// count past 99 hours widens the field naturally ("100:00:00"),
/// which keeps long uptime readouts honest.
pub fn hms(total_secs: u64) -> String<12> {
    hms_parts(total_secs / 3600, (total_secs / 60) % 60, total_secs % 60)
}

/// `HH:MM:SS` from separate fields (clock-of-day readouts).
pub fn hms_parts(h: u64, m: u64, s: u64) -> String<12> {
    let mut buf = String::new();
    let _ = write!(buf, "{:02}:{:02}:{:02}", h, m, s);
    buf
}

/// `HH:MM` clock time (status bars, compact readouts).
pub fn hm(h: u8, m: u8) -> String<8> {
    let mut buf = String::new();
    let _ = write!(buf, "{:02}:{:02}", h, m);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(hms(0).as_str(), "00:00:00");
        assert_eq!(hms(3661).as_str(), "01:01:01");
        // No hour cap - long uptimes stay honest.
        assert_eq!(hms(100 * 3600 + 59).as_str(), "100:00:59");
        assert_eq!(hms_parts(9, 8, 7).as_str(), "09:08:07");
        assert_eq!(hm(7, 5).as_str(), "07:05");
    }
}
