//! ISO14443-A activation logic - the pure parts.
//!
//! Classification (ATQA/SAK -> [`CardKind`]) and the UID / BCC
//! arithmetic of the anticollision cascade live here as plain
//! functions so they are host-tested without a radio. The transceive
//! that actually runs WUPA, the cascade, and SELECT against a card is
//! [`crate::reader`].

use app_core::nfc::{CardKind, Uid};

/// Cascade Tag - prefixes a cascade level whose UID continues into
/// the next level (ISO14443-3 6.5.4).
pub const CASCADE_TAG: u8 = 0x88;

/// SELECT command codes, one per cascade level.
pub const SEL_CL1: u8 = 0x93;
pub const SEL_CL2: u8 = 0x95;
pub const SEL_CL3: u8 = 0x97;

/// HLTA: put a selected card into the HALT state (ISO14443-3 6.3.3).
/// A halted card ignores REQA and answers only WUPA - or field loss,
/// which resets it to IDLE. The PICC must not reply; silence is the
/// success case.
pub const HLTA: [u8; 2] = [0x50, 0x00];

/// The SELECT code for a cascade level (1..=3).
pub fn sel_for_level(level: u8) -> Option<u8> {
    match level {
        1 => Some(SEL_CL1),
        2 => Some(SEL_CL2),
        3 => Some(SEL_CL3),
        _ => None,
    }
}

/// Block Check Character for a 4-byte anticollision chunk: the XOR
/// of the four bytes (ISO14443-3 6.5.4).
pub fn bcc(chunk: &[u8; 4]) -> u8 {
    chunk[0] ^ chunk[1] ^ chunk[2] ^ chunk[3]
}

/// True when a level's SAK says the UID is not yet complete (bit 2
/// set) and another cascade level must run.
pub fn sak_says_cascade(sak: u8) -> bool {
    sak & 0x04 != 0
}

/// True when a level's SAK marks the card ISO14443-4 compliant
/// (bit 5) - it will answer RATS with an ATS.
pub fn sak_is_iso4(sak: u8) -> bool {
    sak & 0x20 != 0
}

/// Append the UID bytes carried by one completed cascade level's
/// 5-byte anticollision answer (`uid0..uid3` + BCC). A level whose
/// first byte is the cascade tag contributes only its last three
/// UID bytes; a terminal level contributes all four.
///
/// Returns `Err(())` if the BCC does not check.
pub fn absorb_level(uid: &mut Uid, answer: &[u8; 5]) -> Result<(), ()> {
    let chunk = [answer[0], answer[1], answer[2], answer[3]];
    if bcc(&chunk) != answer[4] {
        return Err(());
    }
    if chunk[0] == CASCADE_TAG {
        // Bytes 1..=3 are UID; byte 0 is the continuation marker.
        for b in &chunk[1..4] {
            uid.push(*b).map_err(|_| ())?;
        }
    } else {
        for b in &chunk {
            uid.push(*b).map_err(|_| ())?;
        }
    }
    Ok(())
}

/// Decode the card family from its ATQA (2 bytes) and final SAK.
///
/// SAK is the primary signal (NXP AN10833 / ISO14443-3); ATQA
/// disambiguates a couple of overlaps. Anything Type A we cannot
/// name specifically becomes [`CardKind::Iso14443aOther`].
pub fn classify(atqa: [u8; 2], sak: u8) -> CardKind {
    // Mask off the cascade/complete bookkeeping bits before matching
    // the identity nibbles.
    match sak & 0x7F {
        0x00 => CardKind::MifareUltralight,
        0x09 => CardKind::MifareClassicMini,
        0x08 => CardKind::MifareClassic1K,
        0x18 => CardKind::MifareClassic4K,
        0x10 | 0x11 => CardKind::MifarePlus,
        // ISO14443-4 cards. DESFire's ATQA is 0x0344; Plus in
        // security level 3 also lands here - call the 0x0344 case
        // DESFire, the rest a generic ISO14443-4 card.
        s if s & 0x20 != 0 => {
            if atqa == [0x44, 0x03] || atqa == [0x03, 0x44] {
                CardKind::MifareDesfire
            } else {
                CardKind::Iso14443aOther
            }
        }
        _ => CardKind::Iso14443aOther,
    }
}

// -- ISO14443-3 framing helpers ----------------------------------------------

/// ISO14443-A CRC_A (ISO/IEC 14443-3 Annex B): CRC-16 with polynomial
/// 0x8408 (0x1021 reflected), initial value 0x6363, no final XOR,
/// appended low byte first. Needed in software for MIFARE Classic's
/// encrypted commands, where the chip's automatic CRC would be
/// computed over ciphertext.
pub fn crc_a(data: &[u8]) -> [u8; 2] {
    let mut crc: u16 = 0x6363;
    for &b in data {
        let mut ch = b ^ (crc as u8);
        ch ^= ch << 4;
        crc = (crc >> 8) ^ ((ch as u16) << 8) ^ ((ch as u16) << 3) ^ ((ch as u16) >> 4);
    }
    [crc as u8, (crc >> 8) as u8]
}

/// ISO14443-A uses ODD parity: the parity bit is 1 when the byte has
/// an even number of set bits.
pub fn oddparity8(b: u8) -> u8 {
    ((b.count_ones() & 1) ^ 1) as u8
}

#[inline]
fn put_bit(buf: &mut [u8], pos: usize, bit: u8) {
    let mask = 1u8 << (pos % 8);
    if bit & 1 != 0 {
        buf[pos / 8] |= mask;
    } else {
        buf[pos / 8] &= !mask;
    }
}

#[inline]
fn get_bit(buf: &[u8], pos: usize) -> u8 {
    (buf[pos / 8] >> (pos % 8)) & 1
}

/// Pack bytes with one parity bit after each into the raw bit stream
/// the chip transmits when parity generation is off (`no_tx_par`):
/// LSB first, 9 bits per byte, contiguous. Returns the bit count;
/// load `bits / 8` full FIFO bytes and declare `bits % 8` extra bits.
/// `out` must hold at least `ceil(9 * data.len() / 8)` bytes.
pub fn pack_parity_stream(data: &[u8], parity: &[u8], out: &mut [u8]) -> usize {
    let mut pos = 0usize;
    for (i, &d) in data.iter().enumerate() {
        for j in 0..8 {
            put_bit(out, pos, (d >> j) & 1);
            pos += 1;
        }
        put_bit(out, pos, parity[i]);
        pos += 1;
    }
    pos
}

/// Unpack a raw received bit stream (`no_rx_par`: 9 bits per byte,
/// LSB first) into data bytes and parity bits. `bits` is the stream
/// length - 8 per full FIFO byte plus the incomplete last byte's bits
/// as the FIFO status reports them. Returns the byte count; trailing
/// bits that do not complete a 9-bit symbol are ignored.
pub fn unpack_parity_stream(
    stream: &[u8],
    bits: usize,
    data: &mut [u8],
    parity: &mut [u8],
) -> usize {
    let n = (bits / 9).min(data.len()).min(parity.len());
    for i in 0..n {
        let mut b = 0u8;
        for j in 0..8 {
            b |= get_bit(stream, 9 * i + j) << j;
        }
        data[i] = b;
        parity[i] = get_bit(stream, 9 * i + 8);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use heapless::Vec;

    #[test]
    fn bcc_is_xor() {
        assert_eq!(bcc(&[0x04, 0x11, 0x22, 0x33]), 0x04 ^ 0x11 ^ 0x22 ^ 0x33);
    }

    #[test]
    fn single_size_uid() {
        // 4-byte UID: terminal level, all four bytes contribute.
        let mut uid: Uid = Vec::new();
        let u = [0xDE, 0xAD, 0xBE, 0xEF];
        let ans = [u[0], u[1], u[2], u[3], bcc(&u)];
        absorb_level(&mut uid, &ans).unwrap();
        assert_eq!(&uid[..], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn double_size_uid_drops_cascade_tag() {
        // 7-byte UID across two levels; CL1 carries CT + 3 bytes.
        let mut uid: Uid = Vec::new();
        let cl1 = {
            let c = [CASCADE_TAG, 0x01, 0x02, 0x03];
            [c[0], c[1], c[2], c[3], bcc(&c)]
        };
        let cl2 = {
            let c = [0x04, 0x05, 0x06, 0x07];
            [c[0], c[1], c[2], c[3], bcc(&c)]
        };
        absorb_level(&mut uid, &cl1).unwrap();
        absorb_level(&mut uid, &cl2).unwrap();
        assert_eq!(&uid[..], &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
    }

    #[test]
    fn bad_bcc_rejected() {
        let mut uid: Uid = Vec::new();
        let ans = [0x01, 0x02, 0x03, 0x04, 0xFF];
        assert!(absorb_level(&mut uid, &ans).is_err());
    }

    #[test]
    fn classify_known_saks() {
        assert_eq!(classify([0x04, 0x00], 0x08), CardKind::MifareClassic1K);
        assert_eq!(classify([0x02, 0x00], 0x18), CardKind::MifareClassic4K);
        assert_eq!(classify([0x04, 0x00], 0x09), CardKind::MifareClassicMini);
        assert_eq!(classify([0x44, 0x00], 0x00), CardKind::MifareUltralight);
        assert_eq!(classify([0x44, 0x03], 0x20), CardKind::MifareDesfire);
    }

    #[test]
    fn cascade_and_iso4_flags() {
        assert!(sak_says_cascade(0x04));
        assert!(!sak_says_cascade(0x08));
        assert!(sak_is_iso4(0x20));
        assert!(!sak_is_iso4(0x08));
    }

    #[test]
    fn crc_a_known_answer_hlta() {
        // The HLTA frame on the wire is 50 00 57 CD - the canonical
        // CRC_A vector.
        assert_eq!(crc_a(&HLTA), [0x57, 0xCD]);
    }

    #[test]
    fn odd_parity_values() {
        assert_eq!(oddparity8(0x00), 1);
        assert_eq!(oddparity8(0x01), 0);
        assert_eq!(oddparity8(0x03), 1);
        assert_eq!(oddparity8(0xFF), 1);
    }

    #[test]
    fn parity_stream_layout_and_round_trip() {
        // One byte 0x01 with parity 1: bits 1,0,0,0,0,0,0,0,1 packed
        // LSB first -> [0x01, 0x01], 9 bits.
        let mut out = [0u8; 2];
        assert_eq!(pack_parity_stream(&[0x01], &[1], &mut out), 9);
        assert_eq!(out, [0x01, 0x01]);
        // Round trip over an 8-byte frame (the {nr, ar} size): 72 bits
        // = exactly 9 FIFO bytes, no partial byte.
        let data = [0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34, 0x56, 0x78];
        let par = [1, 0, 1, 1, 0, 0, 1, 0];
        let mut stream = [0u8; 9];
        let bits = pack_parity_stream(&data, &par, &mut stream);
        assert_eq!(bits, 72);
        let (mut d, mut p) = ([0u8; 8], [0u8; 8]);
        assert_eq!(unpack_parity_stream(&stream, bits, &mut d, &mut p), 8);
        assert_eq!(d, data);
        assert_eq!(p, par);
        // A 4-byte answer is 36 bits: 4 full FIFO bytes + 4 bits.
        let mut s4 = [0u8; 5];
        assert_eq!(pack_parity_stream(&data[..4], &par[..4], &mut s4), 36);
        assert_eq!((36 / 8, 36 % 8), (4, 4));
    }
}
