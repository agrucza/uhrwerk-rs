//! NFC Forum Type 2 Tag knowledge (MIFARE Ultralight / NTAG): the
//! chip table, lock-bit and configuration decoding, and the per-page
//! dump summary. Pure data, host-tested; the RF side is in the `nfc`
//! crate.
//!
//! Sources: NXP NTAG213/215/216 rev 3.2 (8.5, 10.1, 10.2) and MIFARE
//! Ultralight EV1 MF0ULx1 rev 3.3 (8.5, 10.1, 10.2). Every address
//! and bit position here is from those tables, not from memory.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use heapless::Vec;

/// Bytes per page.
pub const PAGE_SIZE: usize = 4;

/// Most pages on any supported chip (the NTAG216).
pub const PAGES_MAX: usize = 231;

/// GET_VERSION command code.
pub const GET_VERSION: u8 = 0x60;

/// The chips the watch tells apart, by GET_VERSION byte 6 (storage
/// size) or, for a chip that NAKs GET_VERSION, the plain Ultralight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Type2Chip {
    /// MF0ICU1: no GET_VERSION, 16 pages, static lock bytes only.
    Ultralight,
    /// MF0UL11: 20 pages, 48 B user.
    UltralightEv1_11,
    /// MF0UL21: 41 pages, 128 B user.
    UltralightEv1_21,
    /// 45 pages, 144 B user.
    Ntag213,
    /// 135 pages, 504 B user.
    Ntag215,
    /// 231 pages, 888 B user.
    Ntag216,
}

impl Type2Chip {
    /// From the GET_VERSION answer (NTAG Table 28, UL EV1 Table 15):
    /// byte 2 = product type (04 NTAG, 03 Ultralight), byte 6 =
    /// storage size. Unknown combinations yield `None`.
    pub fn from_version(v: &[u8; 8]) -> Option<Self> {
        match (v[2], v[6]) {
            (0x04, 0x0F) => Some(Type2Chip::Ntag213),
            (0x04, 0x11) => Some(Type2Chip::Ntag215),
            (0x04, 0x13) => Some(Type2Chip::Ntag216),
            (0x03, 0x0B) => Some(Type2Chip::UltralightEv1_11),
            (0x03, 0x0E) => Some(Type2Chip::UltralightEv1_21),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Type2Chip::Ultralight => "Ultralight",
            Type2Chip::UltralightEv1_11 => "Ultralight EV1 48B",
            Type2Chip::UltralightEv1_21 => "Ultralight EV1 128B",
            Type2Chip::Ntag213 => "NTAG213",
            Type2Chip::Ntag215 => "NTAG215",
            Type2Chip::Ntag216 => "NTAG216",
        }
    }

    /// Total pages, page 0 included.
    pub fn pages(self) -> u8 {
        match self {
            Type2Chip::Ultralight => 16,
            Type2Chip::UltralightEv1_11 => 20,
            Type2Chip::UltralightEv1_21 => 41,
            Type2Chip::Ntag213 => 45,
            Type2Chip::Ntag215 => 135,
            Type2Chip::Ntag216 => 231,
        }
    }

    /// User memory in bytes (pages 4 up to the dynamic lock / config
    /// pages).
    pub fn user_bytes(self) -> u16 {
        match self {
            Type2Chip::Ultralight => 48,
            Type2Chip::UltralightEv1_11 => 48,
            Type2Chip::UltralightEv1_21 => 128,
            Type2Chip::Ntag213 => 144,
            Type2Chip::Ntag215 => 504,
            Type2Chip::Ntag216 => 888,
        }
    }

    /// Page holding the dynamic lock bytes, where the chip has them.
    pub fn dyn_lock_page(self) -> Option<u8> {
        match self {
            Type2Chip::Ultralight | Type2Chip::UltralightEv1_11 => None,
            Type2Chip::UltralightEv1_21 => Some(0x24),
            Type2Chip::Ntag213 => Some(0x28),
            Type2Chip::Ntag215 => Some(0x82),
            Type2Chip::Ntag216 => Some(0xE2),
        }
    }

    /// First configuration page (CFG0; CFG1 follows, then PWD, PACK),
    /// where the chip has them.
    pub fn cfg_page(self) -> Option<u8> {
        match self {
            Type2Chip::Ultralight => None,
            Type2Chip::UltralightEv1_11 => Some(0x10),
            Type2Chip::UltralightEv1_21 => Some(0x25),
            Type2Chip::Ntag213 => Some(0x29),
            Type2Chip::Ntag215 => Some(0x83),
            Type2Chip::Ntag216 => Some(0xE3),
        }
    }

    /// Whether `page` is write-locked, from the static lock bytes
    /// (page 2 bytes 2 and 3) and, for pages from 16 on, the dynamic
    /// lock bytes 0 and 1 when that page was read. Pages 0-2 are
    /// always read-only; page 3 (CC/OTP) and 4-15 follow the static
    /// bits; beyond that the chip's dynamic table (NTAG Fig 10-12,
    /// UL EV1 Fig 9). `None` for a page the tables do not cover.
    pub fn page_locked(self, page: u8, lock0: u8, lock1: u8, dyn_lock: Option<[u8; 2]>) -> Option<bool> {
        if page >= self.pages() {
            return None;
        }
        match page {
            0..=2 => Some(true),
            // Lock byte 0: bit 3 = page 3, bits 4-7 = pages 4-7.
            3 => Some(lock0 & (1 << 3) != 0),
            4..=7 => Some(lock0 & (1 << (page as u32)) != 0),
            // Lock byte 1: bit 0 = page 8 ... bit 7 = page 15.
            8..=15 => Some(lock1 & (1 << (page as u32 - 8)) != 0),
            _ => {
                let [d0, d1] = dyn_lock?;
                let bit = match self {
                    Type2Chip::Ultralight | Type2Chip::UltralightEv1_11 => return None,
                    // 2-page granularity: byte 0 bit i = pages 16+2i,
                    // 17+2i; byte 1 bits 0-1 (UL21) / 0-3 (NTAG213)
                    // continue at page 32.
                    Type2Chip::UltralightEv1_21 | Type2Chip::Ntag213 => {
                        let i = (page as u32 - 16) / 2;
                        if i < 8 { d0 >> i } else { d1 >> (i - 8) }
                    }
                    // 16-page granularity: byte 0 bit i = pages
                    // 16+16i..31+16i; NTAG215 bit 7 = 128-129 only.
                    Type2Chip::Ntag215 => {
                        let i = (page as u32 - 16) / 16;
                        d0 >> i
                    }
                    // NTAG216: byte 0 bits 0-7 cover 16..143, byte 1
                    // bits 0-5 cover 144..225.
                    Type2Chip::Ntag216 => {
                        let i = (page as u32 - 16) / 16;
                        if i < 8 { d0 >> i } else { d1 >> (i - 8) }
                    }
                };
                Some(bit & 1 != 0)
            }
        }
    }
}

/// Password configuration read from CFG0 byte 3 and CFG1 byte 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Type2Config {
    /// First password-protected page; protection is off when this is
    /// beyond the last page (factory: FF).
    pub auth0: u8,
    /// PROT bit: `true` = read and write protected, `false` = write
    /// protected only.
    pub prot: bool,
    /// CFGLCK: the configuration pages are permanently write-locked.
    pub cfglck: bool,
    /// AUTHLIM: 0 = unlimited failed password attempts, else the count
    /// after which the protected area locks for good.
    pub authlim: u8,
}

impl Type2Config {
    pub fn decode(cfg0: &[u8; 4], cfg1: &[u8; 4]) -> Self {
        Self {
            auth0: cfg0[3],
            prot: cfg1[0] & 0x80 != 0,
            cfglck: cfg1[0] & 0x40 != 0,
            authlim: cfg1[0] & 0x07,
        }
    }

    /// Whether any page of a chip with `pages` pages is protected.
    pub fn protects(self, pages: u8) -> bool {
        self.auth0 < pages
    }
}

/// One page's outcome in a dump, 2 bits in the summary bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PageState {
    /// Not read: the sweep did not reach it (card lost) or the lock
    /// state is unknown.
    NotRead,
    /// Read, writable.
    Read,
    /// Read, write-locked by a lock bit.
    Locked,
    /// Unreadable behind the password (page >= AUTH0 with PROT set).
    Protected,
}

/// Per-page outcome of a Type 2 dump plus what the chip told us. The
/// Type 2 counterpart of `ClassicSummary`: one RAM slot, own file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Type2Summary {
    pub chip: Type2Chip,
    /// 2 bits per page, page i at bits (2*(i%4)) of byte i/4, LSB
    /// first: 0 NotRead, 1 Read, 2 Locked, 3 Protected. Length covers
    /// `chip.pages()`.
    pub pages: Vec<u8, { PAGES_MAX.div_ceil(4) }>,
    /// The password configuration, when the config pages were read.
    pub config: Option<Type2Config>,
}

impl Type2Summary {
    /// All pages "not read" for `chip`.
    pub fn begin(chip: Type2Chip) -> Self {
        let mut pages = Vec::new();
        let _ = pages.resize((chip.pages() as usize).div_ceil(4), 0);
        Self { chip, pages, config: None }
    }

    pub fn set(&mut self, page: u8, state: PageState) {
        let Some(byte) = self.pages.get_mut(page as usize / 4) else {
            return;
        };
        let shift = 2 * (page as u32 % 4);
        let code = match state {
            PageState::NotRead => 0,
            PageState::Read => 1,
            PageState::Locked => 2,
            PageState::Protected => 3,
        };
        *byte = (*byte & !(0b11 << shift)) | (code << shift);
    }

    pub fn page(&self, page: usize) -> Option<PageState> {
        if page >= self.chip.pages() as usize {
            return None;
        }
        let byte = *self.pages.get(page / 4)?;
        Some(match (byte >> (2 * (page % 4))) & 0b11 {
            1 => PageState::Read,
            2 => PageState::Locked,
            3 => PageState::Protected,
            _ => PageState::NotRead,
        })
    }

    /// Pages that were read (`Read` or `Locked`).
    pub fn read_count(&self) -> u16 {
        (0..self.chip.pages() as usize)
            .filter(|&p| matches!(self.page(p), Some(PageState::Read | PageState::Locked)))
            .count() as u16
    }

    /// Pages that were read, write-locked.
    pub fn locked_count(&self) -> u16 {
        (0..self.chip.pages() as usize)
            .filter(|&p| self.page(p) == Some(PageState::Locked))
            .count() as u16
    }

    /// Pages hidden behind the password.
    pub fn protected_count(&self) -> u16 {
        (0..self.chip.pages() as usize)
            .filter(|&p| self.page(p) == Some(PageState::Protected))
            .count() as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_bytes_name_the_chip() {
        let v = |ptype: u8, size: u8| [0x00, 0x04, ptype, 0x02, 0x01, 0x00, size, 0x03];
        assert_eq!(Type2Chip::from_version(&v(0x04, 0x0F)), Some(Type2Chip::Ntag213));
        assert_eq!(Type2Chip::from_version(&v(0x04, 0x11)), Some(Type2Chip::Ntag215));
        assert_eq!(Type2Chip::from_version(&v(0x04, 0x13)), Some(Type2Chip::Ntag216));
        assert_eq!(Type2Chip::from_version(&v(0x03, 0x0B)), Some(Type2Chip::UltralightEv1_11));
        assert_eq!(Type2Chip::from_version(&v(0x03, 0x0E)), Some(Type2Chip::UltralightEv1_21));
        assert_eq!(Type2Chip::from_version(&v(0x04, 0x00)), None);
    }

    #[test]
    fn static_lock_bits_follow_figure_9() {
        let c = Type2Chip::Ntag213;
        // Lock byte 0 bit 3 = page 3, bit 4 = page 4; lock byte 1 bit
        // 0 = page 8, bit 7 = page 15.
        assert_eq!(c.page_locked(3, 1 << 3, 0, None), Some(true));
        assert_eq!(c.page_locked(4, 1 << 4, 0, None), Some(true));
        assert_eq!(c.page_locked(5, 1 << 4, 0, None), Some(false));
        assert_eq!(c.page_locked(8, 0, 1 << 0, None), Some(true));
        assert_eq!(c.page_locked(15, 0, 1 << 7, None), Some(true));
        assert_eq!(c.page_locked(14, 0, 1 << 7, None), Some(false));
        // Pages 0-2 are read-only regardless.
        assert_eq!(c.page_locked(0, 0, 0, None), Some(true));
        // Dynamic range without the dynamic bytes: unknown.
        assert_eq!(c.page_locked(16, 0, 0, None), None);
        // Beyond the chip: none.
        assert_eq!(c.page_locked(45, 0, 0, Some([0xFF, 0xFF])), None);
    }

    #[test]
    fn dynamic_lock_bits_follow_figures_10_to_12_and_ul_fig_9() {
        // NTAG213: byte 0 bit 0 = 16-17, bit 7 = 30-31; byte 1 bit 3
        // = 38-39.
        let c = Type2Chip::Ntag213;
        assert_eq!(c.page_locked(16, 0, 0, Some([1 << 0, 0])), Some(true));
        assert_eq!(c.page_locked(17, 0, 0, Some([1 << 0, 0])), Some(true));
        assert_eq!(c.page_locked(18, 0, 0, Some([1 << 0, 0])), Some(false));
        assert_eq!(c.page_locked(31, 0, 0, Some([1 << 7, 0])), Some(true));
        assert_eq!(c.page_locked(39, 0, 0, Some([0, 1 << 3])), Some(true));
        assert_eq!(c.page_locked(38, 0, 0, Some([0, 1 << 2])), Some(false));
        // NTAG216: byte 0 bit 7 = 128-143, byte 1 bit 0 = 144-159,
        // byte 1 bit 5 = 224-225.
        let c = Type2Chip::Ntag216;
        assert_eq!(c.page_locked(143, 0, 0, Some([1 << 7, 0])), Some(true));
        assert_eq!(c.page_locked(144, 0, 0, Some([1 << 7, 0])), Some(false));
        assert_eq!(c.page_locked(144, 0, 0, Some([0, 1 << 0])), Some(true));
        assert_eq!(c.page_locked(225, 0, 0, Some([0, 1 << 5])), Some(true));
        // NTAG215: byte 0 bit 7 = 128-129.
        let c = Type2Chip::Ntag215;
        assert_eq!(c.page_locked(129, 0, 0, Some([1 << 7, 0])), Some(true));
        assert_eq!(c.page_locked(31, 0, 0, Some([1 << 0, 0])), Some(true));
        // UL21: byte 1 bit 1 = 34-35.
        let c = Type2Chip::UltralightEv1_21;
        assert_eq!(c.page_locked(35, 0, 0, Some([0, 1 << 1])), Some(true));
        // UL11 has no dynamic pages.
        assert_eq!(Type2Chip::UltralightEv1_11.page_locked(16, 0, 0, None), None);
    }

    #[test]
    fn config_decodes_auth0_and_access() {
        let cfg0 = [0x04, 0x00, 0x00, 0xFF];
        let cfg1 = [0x00, 0x00, 0x00, 0x00];
        let c = Type2Config::decode(&cfg0, &cfg1);
        assert_eq!(c.auth0, 0xFF);
        assert!(!c.prot && !c.cfglck);
        assert_eq!(c.authlim, 0);
        assert!(!c.protects(45));
        let c = Type2Config::decode(&[0, 0, 0, 0x2B], &[0x80 | 0x40 | 0x03, 0, 0, 0]);
        assert_eq!(c.auth0, 0x2B);
        assert!(c.prot && c.cfglck);
        assert_eq!(c.authlim, 3);
        assert!(c.protects(45));
    }

    #[test]
    fn summary_packs_two_bits_per_page() {
        let mut s = Type2Summary::begin(Type2Chip::Ntag216);
        assert_eq!(s.pages.len(), 58);
        assert_eq!(s.page(0), Some(PageState::NotRead));
        s.set(0, PageState::Locked);
        s.set(1, PageState::Read);
        s.set(230, PageState::Protected);
        s.set(231, PageState::Read); // ignored, beyond the chip
        assert_eq!(s.page(0), Some(PageState::Locked));
        assert_eq!(s.page(1), Some(PageState::Read));
        assert_eq!(s.page(2), Some(PageState::NotRead));
        assert_eq!(s.page(230), Some(PageState::Protected));
        assert_eq!(s.page(231), None);
        assert_eq!(s.read_count(), 2);
        assert_eq!(s.locked_count(), 1);
        assert_eq!(s.protected_count(), 1);
    }
}
