//! NFC Forum Type 2 Tag commands (MIFARE Ultralight / NTAG) - the
//! pure parts.
//!
//! The command codes and the layout of the first four pages (UID,
//! BCCs, lock bytes, capability container) live here so they are
//! host-tested without a radio. The READ transceive itself is
//! [`crate::reader::Reader::read_type2`].

/// READ: returns 16 bytes = 4 consecutive pages starting at the
/// addressed page (NTAG21x / MF0UL datasheets, "READ").
pub const READ: u8 = 0x30;
/// Bytes per page.
pub const PAGE_SIZE: usize = 4;
/// Bytes returned by one READ (4 pages).
pub const READ_LEN: usize = 16;

/// Extract and verify the 7-byte UID from a READ of page 0 (pages
/// 0..=3). Layout: page 0 = UID0..2 + BCC0, page 1 = UID3..6, page 2
/// = BCC1 + internal + lock bytes, page 3 = capability container.
/// BCC0 = 0x88 ^ UID0 ^ UID1 ^ UID2 (the cascade tag is part of the
/// check, exactly as in anticollision level 1); BCC1 = UID3 ^ UID4 ^
/// UID5 ^ UID6. Returns `None` if either BCC fails.
pub fn uid_from_pages0_3(pages: &[u8; READ_LEN]) -> Option<[u8; 7]> {
    let bcc0 = 0x88 ^ pages[0] ^ pages[1] ^ pages[2];
    if bcc0 != pages[3] {
        return None;
    }
    let bcc1 = pages[4] ^ pages[5] ^ pages[6] ^ pages[7];
    if bcc1 != pages[8] {
        return None;
    }
    Some([pages[0], pages[1], pages[2], pages[4], pages[5], pages[6], pages[7]])
}

/// How many of a READ reply's trailing pages are roll-over copies of
/// pages 0, 1, 2. READ wraps to page 00h past the end of the
/// accessible memory, and "just before the AUTH0 page" when the rest
/// is password-protected (NTAG 10.2, UL EV1 10.2). `page0_3` is the
/// reply for page 0. Checked longest first: a genuine page that
/// happens to equal the UID pages is the only false positive.
pub fn wrapped_tail(reply: &[u8; READ_LEN], page0_3: &[u8; READ_LEN]) -> usize {
    for c in (1..4).rev() {
        if reply[(4 - c) * PAGE_SIZE..] == page0_3[..c * PAGE_SIZE] {
            return c;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_tail_finds_the_rolled_over_pages() {
        let mut p0 = [0u8; READ_LEN];
        for (i, b) in p0.iter_mut().enumerate() {
            *b = 0xA0 + i as u8;
        }
        // AUTH0 = start + 2: the reply's last two pages are pages 0, 1.
        let mut reply = [0x55u8; READ_LEN];
        reply[8..16].copy_from_slice(&p0[..8]);
        assert_eq!(wrapped_tail(&reply, &p0), 2);
        // No wrap.
        let reply = [0x55u8; READ_LEN];
        assert_eq!(wrapped_tail(&reply, &p0), 0);
        // Only the last page wrapped.
        let mut reply = [0x55u8; READ_LEN];
        reply[12..16].copy_from_slice(&p0[..4]);
        assert_eq!(wrapped_tail(&reply, &p0), 1);
    }

    #[test]
    fn page0_read_yields_uid_and_checks_bccs() {
        // The NTAG identified on the watch: UID 04 8D C5 12 2B 5E 80.
        // BCC0 = 88^04^8D^C5 = C4 and BCC1 = 12^2B^5E^80 = E7 - the
        // same values the two anticollision levels returned.
        let mut p = [0u8; READ_LEN];
        p[0..4].copy_from_slice(&[0x04, 0x8D, 0xC5, 0xC4]);
        p[4..8].copy_from_slice(&[0x12, 0x2B, 0x5E, 0x80]);
        p[8] = 0xE7;
        assert_eq!(
            uid_from_pages0_3(&p),
            Some([0x04, 0x8D, 0xC5, 0x12, 0x2B, 0x5E, 0x80]),
        );
        // A corrupted BCC is rejected.
        p[3] ^= 1;
        assert_eq!(uid_from_pages0_3(&p), None);
    }
}
