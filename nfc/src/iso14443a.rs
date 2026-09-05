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
}
