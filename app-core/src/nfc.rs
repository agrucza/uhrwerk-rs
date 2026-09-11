//! NFC card value types - shared by the reader (`nfc` crate) and the
//! UI.
//!
//! Pure data: no hardware, no protocol logic. They live in `app-core`
//! (not the `nfc` protocol crate) so a screen can render a card
//! without pulling in the reader driver - the same reason the other
//! snapshot types live here. The `nfc` crate depends on this module
//! and fills these in; nothing here depends on the `nfc` crate, so
//! there is no cycle.
//!
//! The identity is tagged by the RF technology that found the card.
//! Each technology's identity struct has the fields its protocol
//! defines, so the shape is fixed now even where the protocol module
//! that fills it has not landed yet.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use heapless::Vec;

// ============================================================================
// Technology - which RF protocol family found the card.
// ============================================================================

/// The RF technologies a reader polls in reader mode. One protocol
/// module per technology; the identity carried back is technology-
/// specific (see [`CardIdentity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Technology {
    /// ISO14443-A (NFC-A): MIFARE Classic / Ultralight / NTAG /
    /// DESFire / Plus, Topaz.
    Iso14443a,
    /// ISO14443-B (NFC-B).
    Iso14443b,
    /// FeliCa (NFC-F).
    Felica,
    /// ISO15693 (NFC-V).
    Iso15693,
}

impl Technology {
    /// Human label for logs and the session screen.
    pub fn label(self) -> &'static str {
        match self {
            Technology::Iso14443a => "ISO14443-A",
            Technology::Iso14443b => "ISO14443-B",
            Technology::Felica => "FeliCa",
            Technology::Iso15693 => "ISO15693 / NFC-V",
        }
    }

    /// Short label for space-tight rows ("ISO-A", "FeliCa").
    pub fn short_label(self) -> &'static str {
        match self {
            Technology::Iso14443a => "ISO-A",
            Technology::Iso14443b => "ISO-B",
            Technology::Felica => "FeliCa",
            Technology::Iso15693 => "ISO-V",
        }
    }
}

// ============================================================================
// ISO14443-A
// ============================================================================

/// A Type A UID is 4, 7, or 10 bytes (single / double / triple
/// cascade).
pub type Uid = Vec<u8, 10>;

/// Type A card family, decoded from ATQA + SAK. Only the families the
/// reader distinguishes are named; everything else Type A lands in
/// `Iso14443aOther`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum CardKind {
    /// MIFARE Classic 320-byte "Mini" (5 sectors).
    MifareClassicMini,
    /// MIFARE Classic 1K (16 sectors, 64 blocks).
    MifareClassic1K,
    /// MIFARE Classic 4K (40 sectors, 256 blocks).
    MifareClassic4K,
    /// MIFARE Ultralight / NTAG family (Type 2 tag).
    MifareUltralight,
    /// MIFARE DESFire (Type 4 tag) - identified, not dumped.
    MifareDesfire,
    /// MIFARE Plus - identified, not dumped in Classic terms.
    MifarePlus,
    /// A Type A card we can name only as ISO14443-A.
    Iso14443aOther,
}

impl CardKind {
    /// Human label for logs and the session screen.
    pub fn label(self) -> &'static str {
        match self {
            CardKind::MifareClassicMini => "MIFARE Classic Mini",
            CardKind::MifareClassic1K => "MIFARE Classic 1K",
            CardKind::MifareClassic4K => "MIFARE Classic 4K",
            CardKind::MifareUltralight => "MIFARE Ultralight / NTAG",
            CardKind::MifareDesfire => "MIFARE DESFire",
            CardKind::MifarePlus => "MIFARE Plus",
            CardKind::Iso14443aOther => "ISO14443-A",
        }
    }

    /// Short family label for space-tight rows, where the full
    /// [`label`](Self::label) plus a UID would not fit ("Classic 1K",
    /// "Ultralight").
    pub fn short_label(self) -> &'static str {
        match self {
            CardKind::MifareClassicMini => "Classic Mini",
            CardKind::MifareClassic1K => "Classic 1K",
            CardKind::MifareClassic4K => "Classic 4K",
            CardKind::MifareUltralight => "Ultralight",
            CardKind::MifareDesfire => "DESFire",
            CardKind::MifarePlus => "Plus",
            CardKind::Iso14443aOther => "ISO-A",
        }
    }

    /// True for the families the Crypto1 default-key sweep applies
    /// to (Classic sectors).
    pub fn is_mifare_classic(self) -> bool {
        matches!(
            self,
            CardKind::MifareClassicMini
                | CardKind::MifareClassic1K
                | CardKind::MifareClassic4K
        )
    }

    /// Sector count for the Classic families (0 for non-Classic).
    pub fn classic_sectors(self) -> u8 {
        match self {
            CardKind::MifareClassicMini => 5,
            CardKind::MifareClassic1K => 16,
            CardKind::MifareClassic4K => 40,
            _ => 0,
        }
    }

    /// Total 16-byte block count for the Classic families (0 for
    /// non-Classic). Mini/1K are 4 blocks per sector; the 4K's first 32
    /// sectors hold 4 blocks and its last 8 hold 16 (128 + 128 = 256).
    /// Used as the denominator of a dump's completeness ("N/total").
    pub fn classic_blocks(self) -> u16 {
        match self {
            CardKind::MifareClassicMini => 20,
            CardKind::MifareClassic1K => 64,
            CardKind::MifareClassic4K => 256,
            _ => 0,
        }
    }
}

/// Outcome of one MIFARE Classic sector in a default-key sweep, as
/// reported per sector (`SystemEvent::NfcDumpSector`) and kept per
/// card in the library record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum SectorState {
    /// A dictionary key opened it and every block was read.
    Read,
    /// A dictionary key opened it but at least one block read failed
    /// (the card moved mid-sector).
    Partial,
    /// No dictionary key opened it.
    Locked,
}

// ============================================================================
// MIFARE Classic access conditions (MF1S50/MF1S70 datasheets, 8.7)
// ============================================================================

/// Which key may perform an operation (Tables 7 and 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRight {
    Never,
    KeyA,
    KeyB,
    KeyAB,
}

impl KeyRight {
    /// Short form for the detail's status line.
    pub fn short(self) -> &'static str {
        match self {
            KeyRight::Never => "-",
            KeyRight::KeyA => "A",
            KeyRight::KeyB => "B",
            KeyRight::KeyAB => "AB",
        }
    }
}

/// The 3-bit access condition of one block group, `C1 C2 C3` packed
/// as `c1 << 2 | c2 << 1 | c3`. Groups 0..2 are the data blocks (on a
/// 4K's big sectors: blocks 0-4, 5-9, 10-14), group 3 is the trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SectorAccess {
    pub groups: [u8; 4],
}

/// Transport (delivery) configuration: data groups 000, trailer 001.
pub const ACCESS_TRANSPORT: SectorAccess = SectorAccess { groups: [0, 0, 0, 0b001] };

impl SectorAccess {
    /// Decode trailer bytes 6, 7, 8 (Figure 10). Each condition bit is
    /// stored plain and inverted; a mismatch is a format violation
    /// (the card blocks such a sector for good) and yields `None`.
    ///
    /// byte 6 = ~C2[3..0] | ~C1[3..0], byte 7 = C1[3..0] | ~C3[3..0],
    /// byte 8 = C3[3..0] | C2[3..0]; bit i of a nibble is group i.
    pub fn decode(b6: u8, b7: u8, b8: u8) -> Option<Self> {
        let c1 = b7 >> 4;
        let c2 = b8 & 0x0F;
        let c3 = b8 >> 4;
        let ok = (b6 >> 4) == (!c2 & 0x0F)
            && (b6 & 0x0F) == (!c1 & 0x0F)
            && (b7 & 0x0F) == (!c3 & 0x0F);
        if !ok {
            return None;
        }
        let mut groups = [0u8; 4];
        for (i, g) in groups.iter_mut().enumerate() {
            let bit = |v: u8| (v >> i) & 1;
            *g = bit(c1) << 2 | bit(c2) << 1 | bit(c3);
        }
        Some(Self { groups })
    }

    /// Pack into 12 bits (group i at bits 3i..3i+2) for the summary.
    pub fn packed(self) -> u16 {
        self.groups
            .iter()
            .enumerate()
            .fold(0u16, |acc, (i, g)| acc | ((*g as u16 & 0x7) << (3 * i)))
    }

    pub fn unpack(v: u16) -> Self {
        let mut groups = [0u8; 4];
        for (i, g) in groups.iter_mut().enumerate() {
            *g = ((v >> (3 * i)) & 0x7) as u8;
        }
        Self { groups }
    }

    pub fn is_transport(self) -> bool {
        self == ACCESS_TRANSPORT
    }

    /// Table 7: key B is returned in the clear (and, per the note to
    /// Table 8, cannot authenticate) for trailer conditions 000, 010
    /// and 001.
    pub fn key_b_readable(self) -> bool {
        matches!(self.groups[3], 0b000 | 0b010 | 0b001)
    }

    /// Table 8 for data group `g` (0..2): who may read, write,
    /// increment, and decrement/transfer/restore.
    pub fn data_rights(self, g: usize) -> DataRights {
        use KeyRight::*;
        let (read, write, inc, dec) = match self.groups[g.min(2)] {
            0b000 => (KeyAB, KeyAB, KeyAB, KeyAB),
            0b010 => (KeyAB, Never, Never, Never),
            0b100 => (KeyAB, KeyB, Never, Never),
            0b110 => (KeyAB, KeyB, KeyB, KeyAB),
            0b001 => (KeyAB, Never, Never, KeyAB),
            0b011 => (KeyB, KeyB, Never, Never),
            0b101 => (KeyB, Never, Never, Never),
            _ => (Never, Never, Never, Never),
        };
        DataRights { read, write, inc, dec }
    }

    /// Whether all three data groups share one condition.
    pub fn data_uniform(self) -> bool {
        self.groups[0] == self.groups[1] && self.groups[1] == self.groups[2]
    }
}

/// Per-operation rights of a data block group (Table 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRights {
    pub read: KeyRight,
    pub write: KeyRight,
    pub inc: KeyRight,
    pub dec: KeyRight,
}

/// What every Type A card yields from anticollision, before any
/// authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TypeAInfo {
    pub uid: Uid,
    /// Answer To Request, Type A (2 bytes, wire order).
    pub atqa: [u8; 2],
    /// Select Acknowledge (the last cascade level's SAK).
    pub sak: u8,
    pub kind: CardKind,
}

// ============================================================================
// ISO14443-B
// ============================================================================

/// What a Type B card yields from its ATQB (ISO14443-3 7.9).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TypeBInfo {
    /// Pseudo-Unique PICC Identifier.
    pub pupi: [u8; 4],
    /// Application data (AFI-dependent).
    pub app_data: [u8; 4],
    /// Protocol info: bit rates, max frame size, protocol type, FWI.
    pub protocol_info: [u8; 3],
}

// ============================================================================
// FeliCa
// ============================================================================

/// What a FeliCa card yields from the polling response.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct FelicaInfo {
    /// Manufacture ID - the card's unique identifier.
    pub idm: [u8; 8],
    /// Manufacture parameters (IC code, timing).
    pub pmm: [u8; 8],
}

// ============================================================================
// ISO15693 / NFC-V
// ============================================================================

/// What an NFC-V card yields from INVENTORY.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TypeVInfo {
    /// 64-bit UID (E0 + manufacturer + serial), in the order received.
    pub uid: [u8; 8],
    /// Data Storage Format Identifier.
    pub dsfid: u8,
}

// ============================================================================
// CardIdentity - the identity, tagged by the technology that found it.
// ============================================================================

/// A card's identity: which technology answered, and that
/// technology's identity fields. Read the technology-neutral view
/// through [`technology`](Self::technology), [`id_bytes`](Self::id_bytes)
/// and [`label`](Self::label); match a variant only for
/// technology-specific work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum CardIdentity {
    Iso14443a(TypeAInfo),
    Iso14443b(TypeBInfo),
    Felica(FelicaInfo),
    Iso15693(TypeVInfo),
}

impl CardIdentity {
    pub fn technology(&self) -> Technology {
        match self {
            CardIdentity::Iso14443a(_) => Technology::Iso14443a,
            CardIdentity::Iso14443b(_) => Technology::Iso14443b,
            CardIdentity::Felica(_) => Technology::Felica,
            CardIdentity::Iso15693(_) => Technology::Iso15693,
        }
    }

    /// The technology's unique identifier bytes - UID, PUPI, IDm, UID -
    /// for logs and display.
    pub fn id_bytes(&self) -> &[u8] {
        match self {
            CardIdentity::Iso14443a(a) => &a.uid[..],
            CardIdentity::Iso14443b(b) => &b.pupi[..],
            CardIdentity::Felica(f) => &f.idm[..],
            CardIdentity::Iso15693(v) => &v.uid[..],
        }
    }

    /// Human label: the Type A family where one is known, otherwise
    /// the technology name.
    pub fn label(&self) -> &'static str {
        match self {
            CardIdentity::Iso14443a(a) => a.kind.label(),
            other => other.technology().label(),
        }
    }

    /// Short form of [`label`](Self::label) for rows that also carry
    /// the id bytes.
    pub fn short_label(&self) -> &'static str {
        match self {
            CardIdentity::Iso14443a(a) => a.kind.short_label(),
            other => other.technology().short_label(),
        }
    }
}

// ============================================================================
// NfcScan - the outcome of the last scan, for the UI.
// ============================================================================

/// What the last NFC scan produced, held in `SystemData` for the NFC
/// screen. Distinguishes "nothing scanned yet" from "a card was there
/// but could not be identified" (a technology the reader can't read, a
/// failed read, or the wake-up sensor tripping on an unreadable card)
/// so the screen shows a clear "not recognized" state instead of a
/// bare wake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NfcScan {
    /// No scan this boot.
    #[default]
    None,
    /// A card was identified.
    Card(CardIdentity),
    /// A card was present but not identified (or an unreadable trip).
    Unrecognized,
}

// ============================================================================
// Errors
// ============================================================================

/// Why an identify ended without a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfcScanError {
    /// No card answered before the poll window closed.
    NoCard,
    /// Activation started but never completed (card left the field
    /// mid-select, a BCC mismatch, or a collision we could not
    /// resolve).
    SelectFailed,
    /// The RF front end or SPI path faulted.
    Hardware,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_trailer_decodes_and_round_trips() {
        // FF 07 80: every card on the desk. Data groups 000, trailer
        // 001 (MF1S50 8.6.3 / Figure 10).
        let a = SectorAccess::decode(0xFF, 0x07, 0x80).unwrap();
        assert_eq!(a, ACCESS_TRANSPORT);
        assert!(a.is_transport());
        assert!(a.key_b_readable());
        assert!(a.data_uniform());
        assert_eq!(
            a.data_rights(0),
            DataRights {
                read: KeyRight::KeyAB, write: KeyRight::KeyAB,
                inc: KeyRight::KeyAB, dec: KeyRight::KeyAB,
            },
        );
        assert_eq!(SectorAccess::unpack(a.packed()), a);
    }

    #[test]
    fn inverted_mismatch_is_a_format_violation() {
        // Flip one plain bit without its inverted twin.
        assert!(SectorAccess::decode(0xFF, 0x07, 0x81).is_none());
        assert!(SectorAccess::decode(0xFE, 0x07, 0x80).is_none());
    }

    #[test]
    fn custom_conditions_decode_per_group() {
        // A common personalised layout: data 100 (read A|B, write B),
        // trailer 011 (key B hidden). Build bytes from the codes:
        // C1 = 0b1111? no - group bits: g0..g2 C1=1,C2=0,C3=0; g3
        // C1=0,C2=1,C3=1.
        let c1: u8 = 0b0111; // groups 0..2
        let c2: u8 = 0b1000; // group 3
        let c3: u8 = 0b1000; // group 3
        let b6 = ((!c2 & 0xF) << 4) | (!c1 & 0xF);
        let b7 = (c1 << 4) | (!c3 & 0xF);
        let b8 = (c3 << 4) | c2;
        let a = SectorAccess::decode(b6, b7, b8).unwrap();
        assert_eq!(a.groups, [0b100, 0b100, 0b100, 0b011]);
        assert!(!a.is_transport());
        assert!(!a.key_b_readable());
        assert_eq!(a.data_rights(1).write, KeyRight::KeyB);
        assert_eq!(a.data_rights(1).read, KeyRight::KeyAB);
        assert_eq!(a.data_rights(1).inc, KeyRight::Never);
        assert_eq!(SectorAccess::unpack(a.packed()), a);
    }
}
