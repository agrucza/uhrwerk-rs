//! Crypto1 - the MIFARE Classic 48-bit stream cipher, in software.
//!
//! The reader front end is a plain ISO14443 chip with no Crypto1 core
//! (unlike NXP's licensed readers), so the cipher runs here on the
//! MCU and the front end is driven in a manual-parity / manual-CRC
//! mode for the encrypted frames. This module is the cipher and the
//! nonce arithmetic only - pure functions, no hardware, host-tested.
//! The [`crate::mifare`] layer turns keystream into wire frames and
//! [`crate::reader`] carries them.
//!
//! Port of the well-known `crapto1` reference (Garcia et al.), whose
//! split odd/even 24-bit state representation these routines follow
//! bit-for-bit. The cipher's *correctness against a real card* is
//! proven by a successful authentication on hardware: the tag's
//! answer only decrypts to `suc96(nt)` if the cipher is right.

/// Feedback taps applied to the odd half of the split LFSR state.
const LF_POLY_ODD: u32 = 0x0029_CE5C;
/// Feedback taps applied to the even half.
const LF_POLY_EVEN: u32 = 0x0087_0804;

/// Even parity of the low 32 bits (1 if an odd number of set bits).
#[inline]
fn evenparity32(x: u32) -> u32 {
    x.count_ones() & 1
}

/// The 20-bit non-linear filter function `f`, packed exactly as in
/// the reference: five 4-bit lookups into fixed tables, then one
/// output bit selected from `0xEC57E80A`.
#[inline]
fn filter(x: u32) -> u32 {
    let mut f = (0x000f_22c0u32 >> (x & 0xf)) & 16;
    f |= (0x0006_c9c0u32 >> ((x >> 4) & 0xf)) & 8;
    f |= (0x0003_c8b0u32 >> ((x >> 8) & 0xf)) & 4;
    f |= (0x0001_e458u32 >> ((x >> 12) & 0xf)) & 2;
    f |= (0x0000_d938u32 >> ((x >> 16) & 0xf)) & 1;
    (0xEC57_E80Au32 >> f) & 1
}

/// The running Crypto1 state: the LFSR split into its odd- and
/// even-indexed bits (24 bits each), matching the reference so the
/// filter's tap selection lines up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crypto1 {
    odd: u32,
    even: u32,
}

impl Crypto1 {
    /// Load a 48-bit key into the split state (only the low 48 bits
    /// of `key` are used, big-endian as on a MIFARE key block).
    pub fn new(key: u64) -> Self {
        let mut s = Crypto1 { odd: 0, even: 0 };
        let mut i: i32 = 47;
        while i > 0 {
            s.odd = (s.odd << 1) | bit64(key, ((i - 1) ^ 7) as u32);
            s.even = (s.even << 1) | bit64(key, (i ^ 7) as u32);
            i -= 2;
        }
        s
    }

    /// The keystream bit the NEXT [`bit`](Self::bit) call will return,
    /// without advancing the cipher. MIFARE Classic encrypts each
    /// byte's parity bit with exactly this bit - the one that then
    /// also encrypts the first data bit of the following byte - so
    /// the parity costs no keystream.
    #[inline]
    pub fn peek_bit(&self) -> u8 {
        filter(self.odd) as u8
    }

    /// Advance the cipher one bit. `input` is the bit fed into the
    /// LFSR (a data bit, or 0 to draw pure keystream); `encrypted`
    /// selects whether the filter output is folded back into the
    /// feed (set once the channel is encrypted). Returns the
    /// keystream bit.
    #[inline]
    pub fn bit(&mut self, input: u8, encrypted: bool) -> u8 {
        let out = filter(self.odd);
        let mut feed = out & (encrypted as u32);
        feed ^= (input & 1) as u32;
        feed ^= LF_POLY_ODD & self.odd;
        feed ^= LF_POLY_EVEN & self.even;
        self.even = (self.even << 1) | evenparity32(feed);
        core::mem::swap(&mut self.odd, &mut self.even);
        out as u8
    }

    /// Advance eight bits, LSB first, returning the keystream byte.
    #[inline]
    pub fn byte(&mut self, input: u8, encrypted: bool) -> u8 {
        let mut ret = 0u8;
        for i in 0..8 {
            ret |= self.bit((input >> i) & 1, encrypted) << i;
        }
        ret
    }

    /// Advance 32 bits in the reference's big-endian bit order,
    /// returning the keystream word. Used for the nonce exchange.
    #[inline]
    pub fn word(&mut self, input: u32, encrypted: bool) -> u32 {
        let mut ret = 0u32;
        for i in 0..32u32 {
            let in_bit = ((input >> (24 ^ i)) & 1) as u8;
            ret |= (self.bit(in_bit, encrypted) as u32) << (24 ^ i);
        }
        ret
    }
}

#[inline]
fn bit64(x: u64, n: u32) -> u32 {
    ((x >> n) & 1) as u32
}

/// The 16-bit LFSR successor used by MIFARE Classic tag nonces
/// (polynomial x^16 + x^14 + x^13 + x^11 + 1). Advancing a tag
/// nonce by 64 steps gives the reader's answer `aR`; by 96 the
/// tag's answer `aT`. Operates on the 32-bit value with the low 16
/// bits as the live register, as the reference does.
pub fn prng_successor(x: u32, n: u32) -> u32 {
    let mut x = x.swap_bytes();
    for _ in 0..n {
        let hi = ((x >> 16) ^ (x >> 18) ^ (x >> 19) ^ (x >> 21)) & 1;
        x = (x >> 1) | (hi << 31);
    }
    x.swap_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_load_is_stable() {
        // Two distinct keys must not collide in the split state.
        let a = Crypto1::new(0xFFFF_FFFF_FFFF);
        let b = Crypto1::new(0x0000_0000_0000);
        assert_ne!(a, b);
    }

    #[test]
    fn bit_and_byte_agree() {
        // byte() must equal eight LSB-first bit() calls on a clone.
        let mut s1 = Crypto1::new(0xA0A1_A2A3_A4A5);
        let mut s2 = s1;
        let b = s1.byte(0x00, false);
        let mut acc = 0u8;
        for i in 0..8 {
            acc |= s2.bit(0, false) << i;
        }
        assert_eq!(b, acc);
        assert_eq!(s1, s2);
    }

    #[test]
    fn peek_matches_next_bit_and_does_not_advance() {
        let mut s = Crypto1::new(0x1122_3344_5566);
        s.word(0xDEAD_BEEF, false);
        let before = s;
        let peeked = s.peek_bit();
        assert_eq!(s, before, "peek must not change state");
        assert_eq!(s.bit(0, false), peeked, "peek must equal the next keystream bit");
    }

    #[test]
    fn keystream_is_deterministic() {
        // Same key + same feed -> same keystream every run.
        let ks = |()| {
            let mut s = Crypto1::new(0x1122_3344_5566);
            s.word(0xDEAD_BEEF, false);
            [s.byte(0, true), s.byte(0, true), s.byte(0, true), s.byte(0, true)]
        };
        assert_eq!(ks(()), ks(()));
    }

    #[test]
    fn prng_successor_advances() {
        // Distinct nonces stay distinct after the same advance, and
        // suc96 = suc(suc64 advanced 32 more) is internally
        // consistent (aT is aR advanced 32 steps).
        let nt = 0x0123_4567u32;
        let ar = prng_successor(nt, 64);
        let at = prng_successor(nt, 96);
        assert_eq!(prng_successor(ar, 32), at);
        assert_ne!(prng_successor(0x0000_0001, 64), prng_successor(0x0000_0002, 64));
    }
}
