//! NFC card value types - shared by the reader (`nfc` crate) and the
//! UI.
//!
//! Pure data: no hardware, no protocol logic. They live in `app-core`
//! (not the `nfc` protocol crate) so a screen can render a card
//! without pulling in the reader driver - the same reason the other
//! snapshot types live here. The `nfc` crate depends on this module
//! and fills these in; nothing here depends on the `nfc` crate, so
//! there is no cycle.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use heapless::Vec;

/// A UID is 4, 7, or 10 bytes (single / double / triple cascade).
pub type Uid = Vec<u8, 10>;

/// Card family, decoded from ATQA + SAK. Only the families the reader
/// distinguishes are named; everything else Type A lands in
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
}

/// What every Type A card yields from anticollision, before any
/// authentication: the identity.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct CardInfo {
    pub uid: Uid,
    /// Answer To Request, Type A (2 bytes, wire order).
    pub atqa: [u8; 2],
    /// Select Acknowledge (the last cascade level's SAK).
    pub sak: u8,
    pub kind: CardKind,
}

impl CardInfo {
    /// UID width in bytes (4 / 7 / 10).
    pub fn uid_len(&self) -> usize {
        self.uid.len()
    }
}

/// Why an identify ended without a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfcScanError {
    /// No card answered before the poll window closed.
    NoCard,
    /// Anticollision started but never completed (card left the field
    /// mid-select, a BCC mismatch, or a collision we could not resolve).
    SelectFailed,
    /// The RF front end or SPI path faulted.
    Hardware,
}
