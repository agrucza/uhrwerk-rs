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
