//! Shared tag-length-value primitives for versioned flash records.
//!
//! A record's payload is a flat list of entries, each
//! `id: u16 LE, len: u8, value` where the value is `postcard`-encoded.
//! Reading walks the list: a known id decodes its field, an unknown id
//! is skipped, a missing id keeps its default, and a truncated tail
//! stops the walk with everything before it intact. That per-field
//! tolerance is the whole point - a record's field set can grow across
//! firmware versions without invalidating what is already stored.
//!
//! This is the mechanism the settings tree uses (see [`crate::config`])
//! and the card library reuses (see [`crate::card_library`]); the
//! helpers live here so both share one implementation instead of each
//! carrying its own copy.
//!
//! Only compiled under the `serde` feature - the encoders that persist
//! to flash pull it in; pure host UI tests do not.

use serde::{Deserialize, Serialize};

/// Append one entry to `buf` at `*at`: `id` (2 bytes LE), the value's
/// length (1 byte), then the `postcard`-encoded value. Advances `*at`
/// past the entry.
///
/// `Err(())` on overflow of `buf` or a value longer than 255 bytes
/// (the single-byte length field's ceiling) - both are "this record's
/// buffer budget is too small", a caller bug, not a runtime condition.
pub fn put<T: Serialize>(
    buf: &mut [u8],
    at: &mut usize,
    id: u16,
    value: &T,
) -> Result<(), ()> {
    // Value goes after a reserved 3-byte header (id + len).
    let start = *at + 3;
    if start > buf.len() {
        return Err(());
    }
    let used = postcard::to_slice(value, &mut buf[start..])
        .map_err(|_| ())?
        .len();
    if used > u8::MAX as usize {
        return Err(());
    }
    buf[*at..*at + 2].copy_from_slice(&id.to_le_bytes());
    buf[*at + 2] = used as u8;
    *at = start + used;
    Ok(())
}

/// Decode one entry's value in place. On any failure the target keeps
/// whatever it already held - per-field tolerance is the whole point,
/// so a single unreadable value never fails the surrounding record.
pub fn get<T: for<'de> Deserialize<'de>>(bytes: &[u8], into: &mut T) {
    if let Ok(v) = postcard::from_bytes(bytes) {
        *into = v;
    }
}

/// Walk a TLV payload as `(id, value)` pairs. Stops cleanly at a
/// truncated tail (a header or value that runs past the end), yielding
/// every complete entry before it.
pub fn entries(bytes: &[u8]) -> Entries<'_> {
    Entries { bytes, i: 0 }
}

/// Iterator over [`entries`].
pub struct Entries<'a> {
    bytes: &'a [u8],
    i: usize,
}

impl<'a> Iterator for Entries<'a> {
    type Item = (u16, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        // Need at least a 3-byte header.
        if self.i + 3 > self.bytes.len() {
            return None;
        }
        let id = u16::from_le_bytes([self.bytes[self.i], self.bytes[self.i + 1]]);
        let len = self.bytes[self.i + 2] as usize;
        self.i += 3;
        // Truncated value: stop, keeping everything read so far.
        if self.i + len > self.bytes.len() {
            self.i = self.bytes.len();
            return None;
        }
        let val = &self.bytes[self.i..self.i + len];
        self.i += len;
        Some((id, val))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heapless::String;

    #[test]
    fn round_trips_known_ids_and_skips_unknown() {
        let mut buf = [0u8; 128];
        let mut at = 0;
        put(&mut buf, &mut at, 1, &7u32).unwrap();
        put(&mut buf, &mut at, 2, &true).unwrap();
        let s: String<8> = String::try_from("hi").unwrap();
        put(&mut buf, &mut at, 9, &s).unwrap();

        // Decode: known ids land, an id we don't ask about is skipped.
        let mut n = 0u32;
        let mut b = false;
        let mut seen_ids: heapless::Vec<u16, 8> = heapless::Vec::new();
        for (id, val) in entries(&buf[..at]) {
            seen_ids.push(id).unwrap();
            match id {
                1 => get(val, &mut n),
                2 => get(val, &mut b),
                _ => {} // e.g. id 9 - skipped
            }
        }
        assert_eq!(n, 7);
        assert!(b);
        assert_eq!(&seen_ids[..], &[1, 2, 9]);
    }

    #[test]
    fn truncated_tail_keeps_prefix() {
        let mut buf = [0u8; 64];
        let mut at = 0;
        put(&mut buf, &mut at, 1, &7u32).unwrap();
        let full = at;
        put(&mut buf, &mut at, 2, &0xAABBu16).unwrap();
        // Chop the last entry's value mid-way.
        let truncated = &buf[..full + 4]; // header of entry 2 + 1 value byte
        let ids: heapless::Vec<u16, 4> = entries(truncated).map(|(id, _)| id).collect();
        // Only the first, complete entry survives.
        assert_eq!(&ids[..], &[1]);
    }

    #[test]
    fn missing_field_keeps_default() {
        let mut buf = [0u8; 32];
        let mut at = 0;
        put(&mut buf, &mut at, 1, &7u32).unwrap();
        let mut only_present = 0u32;
        let mut absent = 99u32;
        for (id, val) in entries(&buf[..at]) {
            if id == 1 {
                get(val, &mut only_present);
            }
        }
        assert_eq!(only_present, 7);
        assert_eq!(absent, 99); // never written, keeps its default
        let _ = &mut absent;
    }
}
