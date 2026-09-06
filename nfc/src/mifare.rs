//! MIFARE Classic specifics: block/sector geometry, command bytes,
//! the well-known default-key dictionary, and the Crypto1 auth
//! handshake - including the ENCRYPTED-PARITY wire framing that the
//! chip's raw (no_tx_par / no_rx_par) mode needs. All pure and
//! host-tested; the transceive that carries these onto a card is
//! [`crate::reader`].

use crate::crypto1::{prng_successor, Crypto1};
use crate::iso14443a::oddparity8;
use app_core::nfc::CardKind;

/// Authenticate with key A / key B (first byte of the auth frame).
pub const AUTH_KEY_A: u8 = 0x60;
pub const AUTH_KEY_B: u8 = 0x61;
/// Read one 16-byte block.
pub const READ_BLOCK: u8 = 0x30;
/// Write one 16-byte block.
pub const WRITE_BLOCK: u8 = 0xA0;

/// Every 16-byte block is this wide.
pub const BLOCK_LEN: usize = 16;

/// The keys tried, in order, against each sector before giving up on
/// it. These are the published factory / vendor / transit defaults
/// that open the large majority of never-reconfigured cards. This is
/// dictionary lookup with keys we already hold - not key recovery;
/// cracking unknown keys (nested / hardnested) is a later effort.
pub const DEFAULT_KEYS: &[[u8; 6]] = &[
    [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], // factory default
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5], // MAD key A
    [0xD3, 0xF7, 0xD3, 0xF7, 0xD3, 0xF7], // NFC Forum / NDEF
    [0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5],
    [0x4D, 0x3A, 0x99, 0xC3, 0x51, 0xDD],
    [0x1A, 0x98, 0x2C, 0x7E, 0x45, 0x9A],
    [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
    [0x71, 0x4C, 0x5C, 0x88, 0x6E, 0x97],
    [0x58, 0x7E, 0xE5, 0xF9, 0x35, 0x0F],
    [0xA0, 0x47, 0x8C, 0xC3, 0x90, 0x91],
    [0x53, 0x3C, 0xB6, 0xC7, 0x23, 0xF6],
    [0x8F, 0xD0, 0xA4, 0xF2, 0x56, 0xE9],
];

/// Pack a 6-byte key (big-endian, as stored in a trailer) into the
/// 48-bit integer the cipher loads.
pub fn key_to_u64(key: &[u8; 6]) -> u64 {
    let mut k = 0u64;
    for b in key {
        k = (k << 8) | *b as u64;
    }
    k
}

/// Number of sectors on a Classic card family (0 for non-Classic).
pub fn sector_count(kind: CardKind) -> u8 {
    kind.classic_sectors()
}

/// Total 16-byte blocks on a Classic card family.
pub fn block_count(kind: CardKind) -> u16 {
    match kind {
        CardKind::MifareClassicMini => 20,   // 5 sectors x 4
        CardKind::MifareClassic1K => 64,     // 16 sectors x 4
        CardKind::MifareClassic4K => 256,    // 32x4 + 8x16
        _ => 0,
    }
}

/// Blocks in a given sector. The 4K card's last eight sectors
/// (32..=39) hold 16 blocks each; every other Classic sector holds
/// four.
pub fn blocks_in_sector(sector: u8) -> u8 {
    if sector >= 32 {
        16
    } else {
        4
    }
}

/// Absolute block index of a sector's first block.
pub fn sector_first_block(sector: u8) -> u16 {
    if sector < 32 {
        sector as u16 * 4
    } else {
        128 + (sector as u16 - 32) * 16
    }
}

/// Absolute block index of a sector's trailer (its last block).
pub fn sector_trailer_block(sector: u8) -> u16 {
    sector_first_block(sector) + blocks_in_sector(sector) as u16 - 1
}

/// The sector a given absolute block belongs to.
pub fn sector_of_block(block: u16) -> u8 {
    if block < 128 {
        (block / 4) as u8
    } else {
        32 + ((block - 128) / 16) as u8
    }
}

/// The 4-byte UID value used in MIFARE authentication: the last four
/// bytes of the (4/7/10-byte) UID, big-endian.
pub fn uid_for_auth(uid: &[u8]) -> u32 {
    let n = uid.len();
    if n >= 4 {
        u32::from_be_bytes([uid[n - 4], uid[n - 3], uid[n - 2], uid[n - 1]])
    } else {
        0
    }
}

// -- Encrypted-parity framing ------------------------------------------------

/// Encrypt `plain` byte-wise on `cipher` and produce, per byte, the
/// ENCRYPTED parity bit MIFARE Classic transmits: the odd parity of
/// the plaintext byte XOR the keystream bit that follows the byte -
/// peeked, not consumed, because the next data bit reuses it. With
/// `feed` the plaintext bits are also fed into the LFSR (the reader
/// nonce `nr` is; everything after it is pure keystream). `out` and
/// `parity` must hold at least `plain.len()` entries.
pub fn encrypt_with_parity(
    cipher: &mut Crypto1,
    plain: &[u8],
    feed: bool,
    out: &mut [u8],
    parity: &mut [u8],
) {
    for (i, &p) in plain.iter().enumerate() {
        let mut enc = 0u8;
        for j in 0..8 {
            let pb = (p >> j) & 1;
            let ks = cipher.bit(if feed { pb } else { 0 }, false);
            enc |= (pb ^ ks) << j;
        }
        out[i] = enc;
        parity[i] = oddparity8(p) ^ cipher.peek_bit();
    }
}

/// Decrypt `enc` byte-wise on `cipher` (pure keystream) and verify
/// each received ENCRYPTED parity bit against the recovered
/// plaintext. A parity mismatch means the cipher is out of sync or
/// the framing is wrong - the check that turns a bad read into an
/// error instead of silent garbage. Returns `Err(index)` naming the
/// first bad byte.
pub fn decrypt_with_parity(
    cipher: &mut Crypto1,
    enc: &[u8],
    parity: &[u8],
    out: &mut [u8],
) -> Result<(), usize> {
    for (i, &e) in enc.iter().enumerate() {
        let ks = cipher.byte(0, false);
        let plain = e ^ ks;
        let expect = oddparity8(plain) ^ cipher.peek_bit();
        if parity[i] & 1 != expect {
            return Err(i);
        }
        out[i] = plain;
    }
    Ok(())
}

/// Everything the reader transmits and expects in the Crypto1
/// three-pass authentication, in WIRE form: the 8-byte `{nr, ar}`
/// frame as ciphertext with its encrypted parity bits, the cipher
/// positioned to decrypt the tag's answer, and the `aT` that answer
/// must decrypt to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthFrame {
    pub bytes: [u8; 8],
    pub parity: [u8; 8],
    /// The cipher after `ar`; decrypting the tag's `aT` through it
    /// (32 more bits) leaves it synchronized for the encrypted
    /// commands that follow a successful auth.
    pub cipher: Crypto1,
    /// `suc96(nt)` - what the decrypted `aT` must equal.
    pub at_expected: u32,
}

/// Build the reader's pass-2 frame for a sector. `uid` is the 4-byte
/// auth UID ([`uid_for_auth`]), `nt` the tag nonce just received in
/// clear, `nr` the reader nonce we choose (any value; a fixed one is
/// fine for a dictionary read).
pub fn auth_frame(key: u64, uid: u32, nt: u32, nr: u32) -> AuthFrame {
    let mut cipher = Crypto1::new(key);
    // Absorb uid XOR nt; the returned word is discarded.
    let _ = cipher.word(uid ^ nt, false);
    let mut bytes = [0u8; 8];
    let mut parity = [0u8; 8];
    // nr: fed into the LFSR as it goes.
    encrypt_with_parity(&mut cipher, &nr.to_be_bytes(), true, &mut bytes[..4], &mut parity[..4]);
    // ar = suc64(nt): pure keystream.
    let ar = prng_successor(nt, 64);
    encrypt_with_parity(&mut cipher, &ar.to_be_bytes(), false, &mut bytes[4..], &mut parity[4..]);
    AuthFrame { bytes, parity, cipher, at_expected: prng_successor(nt, 96) }
}

/// The reader's side of the three-pass authentication as WORDS - the
/// crapto1 reference form, kept as the host-tested cross-check for
/// [`auth_frame`]'s byte-wise framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthResponse {
    pub nr_enc: u32,
    pub ar_enc: u32,
    pub at_expected: u32,
    pub cipher: Crypto1,
}

pub fn auth_response(key: u64, uid: u32, nt: u32, nr: u32) -> AuthResponse {
    let mut cipher = Crypto1::new(key);
    let _ = cipher.word(uid ^ nt, false);
    let nr_enc = cipher.word(nr, false) ^ nr;
    let ar = prng_successor(nt, 64);
    let ar_enc = cipher.word(0, false) ^ ar;
    AuthResponse { nr_enc, ar_enc, at_expected: prng_successor(nt, 96), cipher }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_1k() {
        assert_eq!(block_count(CardKind::MifareClassic1K), 64);
        assert_eq!(sector_count(CardKind::MifareClassic1K), 16);
        assert_eq!(sector_first_block(0), 0);
        assert_eq!(sector_first_block(15), 60);
        assert_eq!(sector_trailer_block(0), 3);
        assert_eq!(sector_trailer_block(15), 63);
        assert_eq!(sector_of_block(7), 1);
    }

    #[test]
    fn geometry_4k_large_sectors() {
        assert_eq!(block_count(CardKind::MifareClassic4K), 256);
        assert_eq!(blocks_in_sector(31), 4);
        assert_eq!(blocks_in_sector(32), 16);
        assert_eq!(sector_first_block(32), 128);
        assert_eq!(sector_trailer_block(32), 143);
        assert_eq!(sector_first_block(39), 240);
        assert_eq!(sector_trailer_block(39), 255);
        assert_eq!(sector_of_block(128), 32);
        assert_eq!(sector_of_block(255), 39);
    }

    #[test]
    fn key_packs_big_endian() {
        assert_eq!(key_to_u64(&[0xFF; 6]), 0xFFFF_FFFF_FFFF);
        assert_eq!(key_to_u64(&[0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5]), 0xA0A1_A2A3_A4A5);
    }

    #[test]
    fn uid_for_auth_takes_last_four() {
        assert_eq!(uid_for_auth(&[0xA4, 0xAA, 0x64, 0x35]), 0xA4AA_6435);
        assert_eq!(uid_for_auth(&[0x04, 0x8D, 0xC5, 0x12, 0x2B, 0x5E, 0x80]), 0x122B_5E80);
    }

    #[test]
    fn byte_wise_auth_frame_matches_word_reference() {
        // The framed bytes must be exactly the reference's words - the
        // parity bits are the only thing the byte-wise path adds.
        let (key, uid, nt, nr) = (0xFFFF_FFFF_FFFF, 0xA4AA_6435, 0x0123_4567, 0x1234_5678);
        let r = auth_response(key, uid, nt, nr);
        let f = auth_frame(key, uid, nt, nr);
        assert_eq!(&f.bytes[..4], &r.nr_enc.to_be_bytes());
        assert_eq!(&f.bytes[4..], &r.ar_enc.to_be_bytes());
        assert_eq!(f.at_expected, r.at_expected);
        assert_eq!(f.cipher, r.cipher);
    }

    #[test]
    fn encrypt_decrypt_round_trip_checks_parity() {
        let mut enc_c = Crypto1::new(0xA0A1_A2A3_A4A5);
        enc_c.word(0xCAFE_BABE, false);
        let mut dec_c = enc_c;
        let plain = [0x30, 0x00, 0x02, 0xA8];
        let (mut e, mut p) = ([0u8; 4], [0u8; 4]);
        encrypt_with_parity(&mut enc_c, &plain, false, &mut e, &mut p);
        let mut out = [0u8; 4];
        assert_eq!(decrypt_with_parity(&mut dec_c, &e, &p, &mut out), Ok(()));
        assert_eq!(out, plain);
        assert_eq!(enc_c, dec_c, "both sides advance identically");
        // A flipped parity bit is caught, naming the byte.
        let mut bad = p;
        bad[2] ^= 1;
        let mut dec2 = Crypto1::new(0xA0A1_A2A3_A4A5);
        dec2.word(0xCAFE_BABE, false);
        assert_eq!(decrypt_with_parity(&mut dec2, &e, &bad, &mut out), Err(2));
    }
}
