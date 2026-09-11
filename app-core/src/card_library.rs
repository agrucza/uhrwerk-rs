//! The on-watch card library: a small, persistent collection of the
//! cards an NFC scan has identified.
//!
//! Pure data and list logic, no hardware and no storage I/O - it lives
//! in `app-core` so a screen can render the list and the model can
//! maintain it, while the manager owns the flash read/write.
//!
//! # On flash
//!
//! One small file per card, each a versioned blob whose payload is a
//! TLV record (see [`crate::tlv`]): `id, len, postcard-value` per
//! field. Reasons for a file per card rather than one index blob:
//!
//! * Each record is well under the storage layer's blob-buffer limit,
//!   so nothing in the flash layer has to change.
//! * A corrupt file costs one card, not the whole library.
//! * Records are independent, matching the flat storage direction.
//!
//! The dump payload is a separate file per card (a later step), flagged
//! here by [`CardMeta::has_dump`], so building or persisting the list
//! never touches a dump.
//!
//! # Ordering
//!
//! Files enumerate in arbitrary order, so each record carries a
//! monotonic [`CardMeta::seq`]; [`CardLibrary::sort_newest_first`]
//! rebuilds newest-first order from it after a boot load, and
//! [`CardLibrary::upsert`] assigns a fresh top `seq` so a scanned card
//! goes to the front. Position is then the display order.
//!
//! # Forward compatibility
//!
//! Each field is a TLV entry with a stable id, so the record can gain
//! a field in a future firmware version without invalidating the cards
//! already stored: an old file simply lacks the new id and defaults
//! it. Ids are assigned once in [`meta_field`] and never reused.

#[cfg(feature = "serde")]
use core::fmt::{self, Formatter};
#[cfg(feature = "serde")]
use serde::{de, ser, Deserialize, Deserializer, Serialize, Serializer};

use heapless::{String, Vec};

use crate::data::TimeData;
use crate::nfc::{CardIdentity, SectorAccess, SectorState};
#[cfg(any(feature = "serde", test))]
use crate::nfc::CardKind;

// -- Classic sweep summary ---------------------------------------------------

/// Most sectors on any Classic family (the 4K).
pub const CLASSIC_SECTORS_MAX: usize = 40;

/// Distinct keys a summary can table. The sweep's dictionary holds 13,
/// so this cannot overflow today; index 15 is reserved as "not
/// tabled" should it ever grow past this.
pub const CLASSIC_KEYS_MAX: usize = 15;

/// Sector byte value for "not swept" (the card left the field before
/// the sweep reached it, or no dump has run).
const SECTOR_NONE: u8 = 0;

/// Key index meaning "the key is not in the table".
const KEY_UNTABLED: u8 = 0xF;

/// Per-sector outcome of a card's stored dump plus the keys that
/// opened it - what the detail's sector grid and KEYS section render.
/// Classic-only; empty on every other card and until a dump runs.
///
/// NOT part of [`CardMeta`]: only one card's summary is ever on
/// screen, so `SystemData` holds a single [`CardSummary`] slot and the
/// summary persists as its own small per-card flash file next to the
/// dump. Thirty copies inside the by-value library once cost 4 KB of
/// boot-time stack and hit the guard.
///
/// One byte per sector, index = sector number:
///
/// | bits | meaning |
/// |---|---|
/// | 0-3 | index into `keys` (`Read`/`Partial` only; 0xF = not tabled) |
/// | 4 | key B opened it (else key A) |
/// | 5-6 | 0 not swept, 1 read, 2 partial, 3 locked |
/// | 7 | reserved |
///
/// Keys are stored per card, not as dictionary indices, so the record
/// stays valid if the sweep's dictionary is ever reordered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ClassicSummary {
    /// One byte per sector (see the type docs). Length is the card's
    /// sector count once a dump has begun, 0 otherwise.
    pub sectors: Vec<u8, CLASSIC_SECTORS_MAX>,
    /// Distinct keys that opened a sector, in order of first use.
    pub keys: Vec<[u8; 6], CLASSIC_KEYS_MAX>,
    /// Per sector, parallel to `sectors`: the trailer's access
    /// conditions packed by `SectorAccess::packed` plus
    /// `ACCESS_DECODED`. 0 = trailer not read; `ACCESS_INVALID` = the
    /// trailer's plain and inverted bits disagreed (format violation).
    pub access: Vec<u16, CLASSIC_SECTORS_MAX>,
}

/// `access` entry flag: decoded from a read trailer.
const ACCESS_DECODED: u16 = 1 << 15;
/// `access` entry flag: trailer read but its access bits are malformed.
const ACCESS_INVALID: u16 = 1 << 14;

/// A card's dump summary, by technology. The per-card summary file
/// holds one of these; a technology gains a variant when its read
/// exists (DESFire later).
///
/// On flash it is a flat TLV record like [`CardMeta`]: a technology
/// tag plus that technology's fields, each under a stable id, so the
/// summary can grow (a new field, a new technology) without ever
/// invalidating stored files. Never a versioned struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TechSummary {
    Classic(ClassicSummary),
    Type2(crate::type2::Type2Summary),
}

/// Stable TLV field ids for [`TechSummary`]. NEVER reuse a retired
/// id; a type change allocates a new id. NEXT_FIELD_ID: 8.
#[cfg(feature = "serde")]
mod summary_field {
    /// u8: 1 = Classic, 2 = Type 2. Mandatory.
    pub const TECH: u16 = 1;
    pub const CLASSIC_SECTORS: u16 = 2;
    pub const CLASSIC_KEYS: u16 = 3;
    pub const CLASSIC_ACCESS: u16 = 4;
    pub const TYPE2_CHIP: u16 = 5;
    pub const TYPE2_PAGES: u16 = 6;
    pub const TYPE2_CONFIG: u16 = 7;
}

#[cfg(feature = "serde")]
const TECH_CLASSIC: u8 = 1;
#[cfg(feature = "serde")]
const TECH_TYPE2: u8 = 2;

/// Upper bound on a summary's TLV payload. Classic: 41 B sectors +
/// 91 B keys + up to 121 B access words. Type 2: 59 B pages + chip +
/// config. With entry headers and headroom; under the 512 B blob
/// buffer.
#[cfg(feature = "serde")]
const SUMMARY_TAGGED_MAX: usize = 320;

#[cfg(feature = "serde")]
impl TechSummary {
    fn encode_tagged(&self, buf: &mut [u8]) -> Result<usize, ()> {
        use summary_field::*;
        let mut at = 0usize;
        match self {
            TechSummary::Classic(c) => {
                crate::tlv::put(buf, &mut at, TECH, &TECH_CLASSIC)?;
                crate::tlv::put(buf, &mut at, CLASSIC_SECTORS, &c.sectors)?;
                crate::tlv::put(buf, &mut at, CLASSIC_KEYS, &c.keys)?;
                crate::tlv::put(buf, &mut at, CLASSIC_ACCESS, &c.access)?;
            }
            TechSummary::Type2(t) => {
                crate::tlv::put(buf, &mut at, TECH, &TECH_TYPE2)?;
                crate::tlv::put(buf, &mut at, TYPE2_CHIP, &t.chip)?;
                crate::tlv::put(buf, &mut at, TYPE2_PAGES, &t.pages)?;
                if let Some(cfg) = &t.config {
                    crate::tlv::put(buf, &mut at, TYPE2_CONFIG, cfg)?;
                }
            }
        }
        Ok(at)
    }

    /// The technology tag is mandatory, and a Type 2 record needs its
    /// chip; everything else defaults when missing.
    fn decode_tagged(bytes: &[u8]) -> Option<TechSummary> {
        use summary_field::*;
        let mut tech: Option<u8> = None;
        let mut classic = ClassicSummary::default();
        let mut chip: Option<crate::type2::Type2Chip> = None;
        let mut pages: Vec<u8, { crate::type2::PAGES_MAX.div_ceil(4) }> = Vec::new();
        let mut config: Option<crate::type2::Type2Config> = None;
        for (id, val) in crate::tlv::entries(bytes) {
            match id {
                TECH => tech = postcard::from_bytes(val).ok(),
                CLASSIC_SECTORS => crate::tlv::get(val, &mut classic.sectors),
                CLASSIC_KEYS => crate::tlv::get(val, &mut classic.keys),
                CLASSIC_ACCESS => crate::tlv::get(val, &mut classic.access),
                TYPE2_CHIP => chip = postcard::from_bytes(val).ok(),
                TYPE2_PAGES => crate::tlv::get(val, &mut pages),
                TYPE2_CONFIG => config = postcard::from_bytes(val).ok(),
                _ => {} // written by a newer firmware - skip
            }
        }
        match tech? {
            TECH_CLASSIC => Some(TechSummary::Classic(classic)),
            TECH_TYPE2 => {
                let mut t = crate::type2::Type2Summary::begin(chip?);
                if pages.len() == t.pages.len() {
                    t.pages = pages;
                }
                t.config = config;
                Some(TechSummary::Type2(t))
            }
            _ => None,
        }
    }
}

#[cfg(feature = "serde")]
impl Serialize for TechSummary {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut buf = [0u8; SUMMARY_TAGGED_MAX];
        let len = self
            .encode_tagged(&mut buf)
            .map_err(|_| ser::Error::custom("summary overflow"))?;
        s.serialize_bytes(&buf[..len])
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for TechSummary {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BytesVisitor;
        impl<'de> de::Visitor<'de> for BytesVisitor {
            type Value = TechSummary;
            fn expecting(&self, f: &mut Formatter) -> fmt::Result {
                f.write_str("summary bytes")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<TechSummary, E> {
                TechSummary::decode_tagged(v).ok_or_else(|| E::custom("summary missing tech"))
            }
            fn visit_borrowed_bytes<E: de::Error>(self, v: &'de [u8]) -> Result<TechSummary, E> {
                self.visit_bytes(v)
            }
        }
        d.deserialize_bytes(BytesVisitor)
    }
}

impl TechSummary {
    pub fn classic(&self) -> Option<&ClassicSummary> {
        match self {
            TechSummary::Classic(c) => Some(c),
            _ => None,
        }
    }

    pub fn type2(&self) -> Option<&crate::type2::Type2Summary> {
        match self {
            TechSummary::Type2(t) => Some(t),
            _ => None,
        }
    }
}

/// `access` entry flag: decoded from a read trailer.
const ACCESS_DECODED: u16 = 1 << 15;
/// `access` entry flag: trailer read but its access bits are malformed.
const ACCESS_INVALID: u16 = 1 << 14;

/// The one summary held in RAM: which card it belongs to, and the
/// summary itself. Filled live by the sweep for the card being dumped,
/// or loaded from that card's summary file when its detail opens. A
/// screen renders it only when `id` matches the card it is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSummary {
    pub id: Vec<u8, 10>,
    pub summary: TechSummary,
}

/// A decoded sector byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectorInfo {
    pub state: SectorState,
    /// The key that opened it and whether it was key A - `None` for a
    /// locked sector or a key that could not be tabled.
    pub key: Option<([u8; 6], bool)>,
    /// The trailer's access conditions: `Some(Ok)` decoded, `Some(Err)`
    /// malformed, `None` when the trailer was not read.
    pub access: Option<Result<SectorAccess, ()>>,
}

impl ClassicSummary {
    /// Start a dump: every one of `sectors_total` sectors becomes "not
    /// swept" and the key table is emptied.
    pub fn begin(&mut self, sectors_total: u8) {
        self.sectors.clear();
        self.keys.clear();
        self.access.clear();
        let n = (sectors_total as usize).min(CLASSIC_SECTORS_MAX);
        let _ = self.sectors.resize(n, SECTOR_NONE);
        let _ = self.access.resize(n, 0);
    }

    /// Record one sector's outcome. A sector outside `begin`'s range is
    /// ignored. `key`/`key_is_a` are only consulted for an opened
    /// sector. `trailer_access` is the trailer's bytes 6..=8 when it
    /// was read; it is decoded here, a malformed one is kept as such.
    pub fn record(
        &mut self,
        sector: u8,
        state: SectorState,
        key: [u8; 6],
        key_is_a: bool,
        trailer_access: Option<[u8; 3]>,
    ) {
        if let Some(a) = self.access.get_mut(sector as usize) {
            *a = match trailer_access {
                None => 0,
                Some([b6, b7, b8]) => match SectorAccess::decode(b6, b7, b8) {
                    Some(acc) => ACCESS_DECODED | acc.packed(),
                    None => ACCESS_DECODED | ACCESS_INVALID,
                },
            };
        }
        let Some(slot) = self.sectors.get_mut(sector as usize) else {
            return;
        };
        let state_bits = match state {
            SectorState::Read => 1,
            SectorState::Partial => 2,
            SectorState::Locked => 3,
        };
        let mut byte = state_bits << 5;
        if state != SectorState::Locked {
            let idx = match self.keys.iter().position(|k| *k == key) {
                Some(i) => i as u8,
                None => match self.keys.push(key) {
                    Ok(()) => (self.keys.len() - 1) as u8,
                    Err(_) => KEY_UNTABLED,
                },
            };
            byte |= idx & 0x0F;
            if !key_is_a {
                byte |= 1 << 4;
            }
        }
        *slot = byte;
    }

    /// Decode sector `i`: `None` when it is out of range or not swept.
    pub fn sector(&self, i: usize) -> Option<SectorInfo> {
        let byte = *self.sectors.get(i)?;
        let state = match (byte >> 5) & 0x3 {
            1 => SectorState::Read,
            2 => SectorState::Partial,
            3 => SectorState::Locked,
            _ => return None,
        };
        let key = if state == SectorState::Locked {
            None
        } else {
            let idx = byte & 0x0F;
            self.keys.get(idx as usize).map(|k| (*k, byte & (1 << 4) == 0))
        };
        let access = match self.access.get(i).copied().unwrap_or(0) {
            0 => None,
            a if a & ACCESS_INVALID != 0 => Some(Err(())),
            a => Some(Ok(SectorAccess::unpack(a & 0x0FFF))),
        };
        Some(SectorInfo { state, key, access })
    }

    /// How many sectors key `idx` (into `keys`) opened.
    pub fn key_sector_count(&self, idx: usize) -> u8 {
        if idx >= self.keys.len() {
            return 0;
        }
        self.sectors
            .iter()
            .filter(|&&b| (b >> 5) & 0x3 != 0 && (b >> 5) & 0x3 != 3 && (b & 0x0F) as usize == idx)
            .count() as u8
    }

    /// No dump has begun.
    pub fn is_empty(&self) -> bool {
        self.sectors.is_empty()
    }

    pub fn clear(&mut self) {
        self.sectors.clear();
        self.keys.clear();
        self.access.clear();
    }
}

/// Maximum cards kept in the library. A HARD cap: at capacity a newly
/// scanned card is still shown, but not stored until the user removes
/// one. Flash is not the constraint (the store partition is
/// megabytes); this bounds the in-RAM list and keeps it scrollable.
pub const MAX_CARDS: usize = 30;

/// Maximum characters in a card's custom label (typed on the watch).
pub const LABEL_MAX: usize = 24;

/// One stored card: its identity plus library metadata.
///
/// The dump is NOT carried here - see the module docs. `has_dump` is
/// the only link to it, and stays `false` until the dump step lands.
///
/// Persisted via a hand-written TLV `Serialize`/`Deserialize` (below)
/// rather than a derive, so its fields are individually tagged and the
/// record can evolve without wiping stored cards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardMeta {
    /// The identity an NFC scan assembled (UID/ATQA/SAK, or the other
    /// technologies' fingerprints). Rendered on the detail screen.
    pub identity: CardIdentity,
    /// Custom name typed on the watch; empty when none has been set,
    /// in which case the UI shows the identity's family label instead.
    /// Never auto-filled: a fresh scan stores it empty.
    pub label: String<LABEL_MAX>,
    /// Wall-clock time this card was first added to the library.
    pub first_seen: TimeData,
    /// Wall-clock time of the most recent scan of this card.
    pub last_seen: TimeData,
    /// Whether a structured dump file exists for this card. Written by
    /// the dump step; always `false` until then.
    pub has_dump: bool,
    /// Blocks captured by the stored dump (0 when `!has_dump`). The
    /// denominator is the card kind's total (`CardKind::classic_blocks`),
    /// so the detail can show "N/total" without loading the dump file.
    pub dump_blocks_read: u16,
    /// Sectors the stored dump opened with a default key (0 when
    /// `!has_dump`). Denominator is `CardKind::classic_sectors`.
    pub dump_sectors_read: u8,
    /// Denominator of `dump_blocks_read`: blocks on a Classic, pages
    /// on a Type 2, as the sweep determined it. 0 for a record whose
    /// dump predates this field (a Classic; the kind's block count
    /// then applies).
    pub dump_total: u16,
    /// Monotonic order key: higher is newer. Assigned by
    /// [`CardLibrary::upsert`]; used to rebuild newest-first order
    /// after loading files in arbitrary order.
    pub seq: u32,
}

/// Stable TLV field ids for [`CardMeta`]. NEVER reuse a retired id; a
/// type change allocates a new id. Keep the record small: it is held
/// thirty times in `SystemData`, which moves by value at boot. Bulky
/// per-card data (a dump, its sector summary) lives in its own file.
/// NEXT_FIELD_ID: 9.
#[cfg(feature = "serde")]
mod meta_field {
    pub const IDENTITY: u16 = 1;
    pub const LABEL: u16 = 2;
    pub const FIRST_SEEN: u16 = 3;
    pub const LAST_SEEN: u16 = 4;
    pub const HAS_DUMP: u16 = 5;
    pub const SEQ: u16 = 6;
    pub const DUMP_BLOCKS: u16 = 7;
    pub const DUMP_SECTORS: u16 = 8;
    pub const DUMP_TOTAL: u16 = 9;
}

/// Upper bound on one card record's TLV payload. Identity ~16 B,
/// label ~28 B, two timestamps ~20 B, flag + seq ~12 B, with entry
/// headers and headroom. Far under the storage layer's 512 B blob
/// buffer.
#[cfg(feature = "serde")]
const META_TAGGED_MAX: usize = 128;

#[cfg(feature = "serde")]
impl CardMeta {
    /// Serialize the fields as a TLV entry list into `buf`; returns the
    /// used length. `Err` only on buffer overflow.
    fn encode_tagged(&self, buf: &mut [u8]) -> Result<usize, ()> {
        use meta_field::*;
        let mut at = 0usize;
        crate::tlv::put(buf, &mut at, IDENTITY, &self.identity)?;
        // An unset label is simply absent; decode defaults it empty.
        if !self.label.is_empty() {
            crate::tlv::put(buf, &mut at, LABEL, &self.label)?;
        }
        crate::tlv::put(buf, &mut at, FIRST_SEEN, &self.first_seen)?;
        crate::tlv::put(buf, &mut at, LAST_SEEN, &self.last_seen)?;
        crate::tlv::put(buf, &mut at, HAS_DUMP, &self.has_dump)?;
        crate::tlv::put(buf, &mut at, SEQ, &self.seq)?;
        crate::tlv::put(buf, &mut at, DUMP_BLOCKS, &self.dump_blocks_read)?;
        crate::tlv::put(buf, &mut at, DUMP_SECTORS, &self.dump_sectors_read)?;
        crate::tlv::put(buf, &mut at, DUMP_TOTAL, &self.dump_total)?;
        Ok(at)
    }

    /// Decode a TLV entry list. The identity is mandatory - a record
    /// without it is not a card, so `None` is returned and the caller
    /// drops the file. Every other field defaults if missing, so a
    /// truncated or partially unreadable record still yields a usable
    /// card.
    ///
    /// Records written before custom labels existed carry a generated
    /// "<tag> <4 hex bytes>" text in the label field; one that still
    /// matches [`legacy_auto_label`] is treated as unset so those cards
    /// read like a fresh scan rather than as custom-named.
    fn decode_tagged(bytes: &[u8]) -> Option<CardMeta> {
        use meta_field::*;
        let mut identity: Option<CardIdentity> = None;
        let mut label: String<LABEL_MAX> = String::new();
        let mut first_seen = TimeData::default();
        let mut last_seen = TimeData::default();
        let mut has_dump = false;
        let mut seq = 0u32;
        let mut dump_blocks_read = 0u16;
        let mut dump_sectors_read = 0u8;
        let mut dump_total = 0u16;
        for (id, val) in crate::tlv::entries(bytes) {
            match id {
                IDENTITY => identity = postcard::from_bytes(val).ok(),
                LABEL => crate::tlv::get(val, &mut label),
                FIRST_SEEN => crate::tlv::get(val, &mut first_seen),
                LAST_SEEN => crate::tlv::get(val, &mut last_seen),
                HAS_DUMP => crate::tlv::get(val, &mut has_dump),
                SEQ => crate::tlv::get(val, &mut seq),
                DUMP_BLOCKS => crate::tlv::get(val, &mut dump_blocks_read),
                DUMP_SECTORS => crate::tlv::get(val, &mut dump_sectors_read),
                DUMP_TOTAL => crate::tlv::get(val, &mut dump_total),
                _ => {} // written by a newer firmware - skip
            }
        }
        let identity = identity?;
        if label == legacy_auto_label(&identity) {
            label.clear();
        }
        Some(CardMeta {
            identity, label, first_seen, last_seen, has_dump, seq,
            dump_blocks_read, dump_sectors_read, dump_total,
        })
    }
}

// Blob-facing serde surface: a `CardMeta` is one opaque `bytes` value
// holding the TLV list, so the storage layer's generic `StoredBlob`
// envelope and SD mirroring are untouched - exactly as `Config` does.
#[cfg(feature = "serde")]
impl Serialize for CardMeta {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut buf = [0u8; META_TAGGED_MAX];
        let len = self
            .encode_tagged(&mut buf)
            .map_err(|_| ser::Error::custom("card meta overflow"))?;
        s.serialize_bytes(&buf[..len])
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for CardMeta {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BytesVisitor;
        impl<'de> de::Visitor<'de> for BytesVisitor {
            type Value = CardMeta;
            fn expecting(&self, f: &mut Formatter) -> fmt::Result {
                f.write_str("card meta bytes")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<CardMeta, E> {
                CardMeta::decode_tagged(v)
                    .ok_or_else(|| E::custom("card meta missing identity"))
            }
            fn visit_borrowed_bytes<E: de::Error>(self, v: &'de [u8]) -> Result<CardMeta, E> {
                self.visit_bytes(v)
            }
        }
        d.deserialize_bytes(BytesVisitor)
    }
}

/// The card library: an in-RAM, newest-first list, capped at
/// [`MAX_CARDS`]. Held in `SystemData`; the manager loads it from the
/// per-card files at boot and persists changes one file at a time. Not
/// serialized as a whole - each card is its own blob.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardLibrary {
    pub cards: Vec<CardMeta, MAX_CARDS>,
}

/// What an [`CardLibrary::upsert`] did, so the caller can react
/// (persist, highlight the row, or warn that the library is full).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    /// A new card was inserted at the front.
    Added,
    /// A card already in the library was refreshed and moved to the
    /// front.
    Bumped,
    /// The library is at capacity and this card is not already in it;
    /// nothing was stored.
    Full,
}

impl CardLibrary {
    /// Index of the card whose identity carries these id bytes
    /// (UID / PUPI / IDm), if any.
    fn position_of(&self, id: &[u8]) -> Option<usize> {
        self.cards.iter().position(|c| c.identity.id_bytes() == id)
    }

    /// Next order key: one past the current maximum.
    fn next_seq(&self) -> u32 {
        self.cards.iter().map(|c| c.seq).max().unwrap_or(0).wrapping_add(1)
    }

    /// Insert a scanned card, or refresh one already stored, keeping
    /// newest-first order.
    ///
    /// Matching is by the identity's id bytes. An existing card has its
    /// `last_seen`, `identity` (a re-read may refine the kind), and
    /// `seq` refreshed and moves to the front. A new card is inserted
    /// at the front with no label, unless the library is full.
    pub fn upsert(&mut self, identity: &CardIdentity, now: TimeData) -> Upsert {
        let seq = self.next_seq();
        if let Some(i) = self.position_of(identity.id_bytes()) {
            let mut meta = self.cards.remove(i);
            meta.last_seen = now;
            meta.identity = identity.clone();
            meta.seq = seq;
            // Room is guaranteed: we just removed this same entry.
            let _ = self.cards.insert(0, meta);
            Upsert::Bumped
        } else if self.cards.len() >= MAX_CARDS {
            Upsert::Full
        } else {
            let meta = CardMeta {
                identity: identity.clone(),
                label: String::new(),
                first_seen: now,
                last_seen: now,
                has_dump: false,
                seq,
                dump_blocks_read: 0,
                dump_sectors_read: 0,
                dump_total: 0,
            };
            // Room checked above.
            let _ = self.cards.insert(0, meta);
            Upsert::Added
        }
    }

    /// Remove the card carrying these id bytes. Returns `true` if one
    /// was removed.
    pub fn remove(&mut self, id: &[u8]) -> bool {
        match self.position_of(id) {
            Some(i) => {
                self.cards.remove(i);
                true
            }
            None => false,
        }
    }

    /// Borrow the card carrying these id bytes.
    pub fn get_by_id(&self, id: &[u8]) -> Option<&CardMeta> {
        self.cards.iter().find(|c| c.identity.id_bytes() == id)
    }

    /// Record a completed dump on the card carrying these id bytes:
    /// set `has_dump` and store the blocks/sectors captured. Returns
    /// `true` if a matching card was updated.
    pub fn set_dump(&mut self, id: &[u8], blocks_read: u16, sectors_read: u8, total: u16) -> bool {
        match self.cards.iter_mut().find(|c| c.identity.id_bytes() == id) {
            Some(c) => {
                c.has_dump = true;
                c.dump_blocks_read = blocks_read;
                c.dump_sectors_read = sectors_read;
                c.dump_total = total;
                true
            }
            None => false,
        }
    }

    /// Set (or, with an empty string, clear) the custom label of the
    /// card carrying these id bytes. Text past [`LABEL_MAX`] is cut.
    /// Returns `true` if a matching card was updated.
    pub fn set_label(&mut self, id: &[u8], label: &str) -> bool {
        match self.cards.iter_mut().find(|c| c.identity.id_bytes() == id) {
            Some(c) => {
                c.label.clear();
                for ch in label.chars() {
                    if c.label.push(ch).is_err() {
                        break;
                    }
                }
                true
            }
            None => false,
        }
    }

    /// Clear a card's stored dump: drop `has_dump` and zero the
    /// completeness counts. Returns `true` if a matching card was
    /// updated. The dump and summary files are deleted by the manager.
    pub fn clear_dump(&mut self, id: &[u8]) -> bool {
        match self.cards.iter_mut().find(|c| c.identity.id_bytes() == id) {
            Some(c) => {
                c.has_dump = false;
                c.dump_blocks_read = 0;
                c.dump_sectors_read = 0;
                c.dump_total = 0;
                true
            }
            None => false,
        }
    }

    /// Append a record loaded from flash at boot. Silently ignores an
    /// overflow past [`MAX_CARDS`] (the write path enforces the cap, so
    /// more files than that should not exist; if they do, the extras
    /// are simply not loaded).
    pub fn push_loaded(&mut self, meta: CardMeta) {
        let _ = self.cards.push(meta);
    }

    /// Rebuild newest-first order from `seq`. Call after loading files,
    /// which enumerate in arbitrary order.
    pub fn sort_newest_first(&mut self) {
        self.cards.sort_unstable_by(|a, b| b.seq.cmp(&a.seq));
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// At capacity: the next new card cannot be stored until one is
    /// removed.
    pub fn is_full(&self) -> bool {
        self.cards.len() >= MAX_CARDS
    }
}

/// The type tag of the pre-custom-label generated text (see
/// [`legacy_auto_label`]). Frozen: it must keep matching what those
/// records hold, independent of the UI's current short labels.
#[cfg(feature = "serde")]
fn legacy_kind_tag(identity: &CardIdentity) -> &'static str {
    match identity {
        CardIdentity::Iso14443a(a) => match a.kind {
            CardKind::MifareClassicMini => "Mini",
            CardKind::MifareClassic1K => "1K",
            CardKind::MifareClassic4K => "4K",
            CardKind::MifareUltralight => "UL",
            CardKind::MifareDesfire => "DESFire",
            CardKind::MifarePlus => "Plus",
            CardKind::Iso14443aOther => "A",
        },
        CardIdentity::Iso14443b(_) => "B",
        CardIdentity::Felica(_) => "F",
        CardIdentity::Iso15693(_) => "V",
    }
}

/// The label text records stored before custom labels existed: the
/// short kind tag plus the first four id bytes in hex, e.g.
/// `"1K 98D4DB3D"`. Only used by decode to recognise such a record
/// and treat its label as unset.
#[cfg(feature = "serde")]
fn legacy_auto_label(identity: &CardIdentity) -> String<LABEL_MAX> {
    use core::fmt::Write;
    let mut s: String<LABEL_MAX> = String::new();
    let _ = write!(s, "{} ", legacy_kind_tag(identity));
    for b in identity.id_bytes().iter().take(4) {
        let _ = write!(s, "{:02X}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfc::{TypeAInfo, Uid};

    fn now(day: u8) -> TimeData {
        TimeData { hour: 12, minute: 0, second: 0, year: 2026, month: 9, day }
    }

    /// A Type A card with the given UID and kind.
    fn card(uid: &[u8], kind: CardKind) -> CardIdentity {
        let mut u: Uid = Uid::new();
        for &b in uid {
            u.push(b).unwrap();
        }
        CardIdentity::Iso14443a(TypeAInfo { uid: u, atqa: [0x04, 0x00], sak: 0x08, kind })
    }

    fn ids(lib: &CardLibrary) -> heapless::Vec<u8, 32> {
        lib.cards.iter().map(|c| c.identity.id_bytes()[0]).collect()
    }

    #[test]
    fn add_puts_newest_at_front() {
        let mut lib = CardLibrary::default();
        assert_eq!(lib.upsert(&card(&[0xA1], CardKind::MifareClassic1K), now(1)), Upsert::Added);
        assert_eq!(lib.upsert(&card(&[0xB2], CardKind::MifareClassic1K), now(2)), Upsert::Added);
        assert_eq!(lib.upsert(&card(&[0xC3], CardKind::MifareClassic1K), now(3)), Upsert::Added);
        assert_eq!(lib.len(), 3);
        assert_eq!(&ids(&lib)[..], &[0xC3, 0xB2, 0xA1]);
    }

    #[test]
    fn rescan_bumps_to_front_and_refreshes() {
        let mut lib = CardLibrary::default();
        lib.upsert(&card(&[0xA1], CardKind::MifareClassic1K), now(1));
        lib.upsert(&card(&[0xB2], CardKind::MifareClassic1K), now(2));
        assert_eq!(lib.upsert(&card(&[0xA1], CardKind::MifareClassic1K), now(5)), Upsert::Bumped);
        assert_eq!(lib.len(), 2);
        assert_eq!(&ids(&lib)[..], &[0xA1, 0xB2]);
        assert_eq!(lib.cards[0].last_seen, now(5));
        assert_eq!(lib.cards[0].first_seen, now(1));
        // The bumped card must now hold the top seq.
        assert!(lib.cards[0].seq > lib.cards[1].seq);
    }

    #[test]
    fn rescan_can_refine_kind() {
        let mut lib = CardLibrary::default();
        lib.upsert(&card(&[0xA1], CardKind::Iso14443aOther), now(1));
        lib.upsert(&card(&[0xA1], CardKind::MifareClassic1K), now(2));
        assert_eq!(lib.len(), 1);
        if let CardIdentity::Iso14443a(a) = &lib.cards[0].identity {
            assert_eq!(a.kind, CardKind::MifareClassic1K);
        } else {
            panic!("expected Type A");
        }
    }

    #[test]
    fn full_refuses_new_but_still_bumps_existing() {
        let mut lib = CardLibrary::default();
        for i in 0..MAX_CARDS as u8 {
            assert_eq!(lib.upsert(&card(&[i], CardKind::MifareClassic1K), now(1)), Upsert::Added);
        }
        assert!(lib.is_full());
        assert_eq!(lib.upsert(&card(&[0xFF], CardKind::MifareClassic1K), now(2)), Upsert::Full);
        assert_eq!(lib.len(), MAX_CARDS);
        assert_eq!(lib.upsert(&card(&[0], CardKind::MifareClassic1K), now(3)), Upsert::Bumped);
        assert_eq!(lib.cards[0].identity.id_bytes()[0], 0);
    }

    #[test]
    fn remove_by_id() {
        let mut lib = CardLibrary::default();
        lib.upsert(&card(&[0xA1], CardKind::MifareClassic1K), now(1));
        lib.upsert(&card(&[0xB2], CardKind::MifareClassic1K), now(2));
        assert!(lib.remove(&[0xA1]));
        assert!(!lib.remove(&[0xA1]));
        assert_eq!(&ids(&lib)[..], &[0xB2]);
    }

    #[test]
    fn sort_rebuilds_newest_first_from_seq() {
        // Simulate a boot load: push in arbitrary (seq) order.
        let mut lib = CardLibrary::default();
        for (uid, seq) in [(0xA1u8, 3u32), (0xB2, 1), (0xC3, 2)] {
            lib.push_loaded(CardMeta {
                identity: card(&[uid], CardKind::MifareClassic1K),
                label: String::new(),
                first_seen: now(1),
                last_seen: now(1),
                has_dump: false,
                seq,
                dump_blocks_read: 0,
                dump_sectors_read: 0,
                dump_total: 0,
            });
        }
        lib.sort_newest_first();
        // Highest seq (A1=3) first, then C3=2, then B2=1.
        assert_eq!(&ids(&lib)[..], &[0xA1, 0xC3, 0xB2]);
    }

    #[test]
    fn get_by_id_finds_the_card() {
        let mut lib = CardLibrary::default();
        lib.upsert(&card(&[0xA1, 0xA2], CardKind::MifareClassic1K), now(1));
        assert!(lib.get_by_id(&[0xA1, 0xA2]).is_some());
        assert!(lib.get_by_id(&[0x00]).is_none());
    }

    #[test]
    fn new_card_has_no_label_until_set() {
        let mut lib = CardLibrary::default();
        lib.upsert(&card(&[0xA1], CardKind::MifareClassic1K), now(1));
        assert!(lib.cards[0].label.is_empty());
        assert!(lib.set_label(&[0xA1], "Office door"));
        assert_eq!(lib.cards[0].label.as_str(), "Office door");
        // Over-long text is cut at the cap, never rejected.
        assert!(lib.set_label(&[0xA1], "0123456789012345678901234567"));
        assert_eq!(lib.cards[0].label.len(), LABEL_MAX);
        // Empty clears.
        assert!(lib.set_label(&[0xA1], ""));
        assert!(lib.cards[0].label.is_empty());
        assert!(!lib.set_label(&[0xFF], "x"));
    }

    const KEY_FF: [u8; 6] = [0xFF; 6];
    const KEY_MAD: [u8; 6] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5];

    #[test]
    fn summary_packs_state_key_and_side_per_sector() {
        let mut s = ClassicSummary::default();
        assert!(s.is_empty());
        s.begin(16);
        assert_eq!(s.sectors.len(), 16);
        // Untouched sectors read as "not swept".
        assert_eq!(s.sector(3), None);
        s.record(0, SectorState::Read, KEY_MAD, true, Some([0xFF, 0x07, 0x80]));
        s.record(1, SectorState::Read, KEY_FF, true, Some([0xFF, 0x07, 0x81]));
        s.record(2, SectorState::Partial, KEY_FF, false, None);
        s.record(3, SectorState::Locked, [0; 6], true, None);
        // Out-of-range sector is ignored, not a panic.
        s.record(40, SectorState::Read, KEY_FF, true, None);
        assert_eq!(&s.keys[..], &[KEY_MAD, KEY_FF]);
        // Sector 0: transport trailer decoded. Sector 1: malformed
        // trailer kept as such. Sector 2: trailer not read.
        assert_eq!(
            s.sector(0),
            Some(SectorInfo {
                state: SectorState::Read,
                key: Some((KEY_MAD, true)),
                access: Some(Ok(crate::nfc::ACCESS_TRANSPORT)),
            }),
        );
        assert_eq!(s.sector(1).unwrap().access, Some(Err(())));
        assert_eq!(
            s.sector(2),
            Some(SectorInfo {
                state: SectorState::Partial, key: Some((KEY_FF, false)), access: None,
            }),
        );
        assert_eq!(
            s.sector(3),
            Some(SectorInfo { state: SectorState::Locked, key: None, access: None }),
        );
        assert_eq!(s.sector(16), None);
        assert_eq!(s.key_sector_count(0), 1);
        assert_eq!(s.key_sector_count(1), 2);
        assert_eq!(s.key_sector_count(5), 0);
        s.clear();
        assert!(s.is_empty());
    }

    #[test]
    fn summary_key_table_overflow_marks_untabled() {
        let mut s = ClassicSummary::default();
        s.begin(40);
        for i in 0..(CLASSIC_KEYS_MAX as u8 + 1) {
            s.record(i, SectorState::Read, [i; 6], true, None);
        }
        assert_eq!(s.keys.len(), CLASSIC_KEYS_MAX);
        // The 16th key has no table slot: still Read, key unknown.
        let last = s.sector(CLASSIC_KEYS_MAX).unwrap();
        assert_eq!(last.state, SectorState::Read);
        assert_eq!(last.key, None);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn classic_summary_tlv_round_trips_and_stays_small() {
        // Worst case: 40 sectors, every key slot used, every trailer
        // decoded. 40 sector bytes + 15 keys + 40 access words (3-byte
        // varints once bit 15 is set) must stay inside the storage
        // layer's 512 B blob buffer.
        let mut s = ClassicSummary::default();
        s.begin(40);
        for i in 0..40u8 {
            s.record(
                i, SectorState::Read, [i % CLASSIC_KEYS_MAX as u8; 6], i % 2 == 0,
                Some([0xFF, 0x07, 0x80]),
            );
        }
        let summary = TechSummary::Classic(s);
        let mut buf = [0u8; 512];
        let encoded = postcard::to_slice(&summary, &mut buf).expect("encode");
        assert!(encoded.len() < 320, "summary {} B", encoded.len());
        let decoded: TechSummary = postcard::from_bytes(encoded).expect("decode");
        assert_eq!(decoded, summary);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn type2_summary_tlv_round_trips_with_and_without_config() {
        use crate::type2::{PageState, Type2Chip, Type2Config, Type2Summary};
        let mut t = Type2Summary::begin(Type2Chip::Ntag216);
        t.set(0, PageState::Locked);
        t.set(4, PageState::Read);
        t.set(230, PageState::Protected);
        t.config = Some(Type2Config { auth0: 0xE1, prot: true, cfglck: false, authlim: 2 });
        let summary = TechSummary::Type2(t.clone());
        let mut buf = [0u8; 512];
        let encoded = postcard::to_slice(&summary, &mut buf).expect("encode");
        assert!(encoded.len() < 120, "summary {} B", encoded.len());
        let decoded: TechSummary = postcard::from_bytes(encoded).expect("decode");
        assert_eq!(decoded, summary);
        // Config pages unreadable: the field is simply absent.
        t.config = None;
        let summary = TechSummary::Type2(t);
        let encoded = postcard::to_slice(&summary, &mut buf).expect("encode");
        let decoded: TechSummary = postcard::from_bytes(encoded).expect("decode");
        assert_eq!(decoded, summary);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn summary_tlv_skips_unknown_fields_and_needs_the_tech_tag() {
        let mut inner = [0u8; 64];
        // A future field id, then the Classic tag: the field is skipped,
        // the record still decodes as an empty Classic summary.
        let mut at = 0;
        crate::tlv::put(&mut inner, &mut at, 99u16, &0xABCDu16).unwrap();
        crate::tlv::put(&mut inner, &mut at, summary_field::TECH, &TECH_CLASSIC).unwrap();
        assert_eq!(
            TechSummary::decode_tagged(&inner[..at]),
            Some(TechSummary::Classic(ClassicSummary::default())),
        );
        // No tech tag: not a summary.
        let mut at = 0;
        crate::tlv::put(&mut inner, &mut at, summary_field::CLASSIC_KEYS, &[KEY_FF]).unwrap();
        assert_eq!(TechSummary::decode_tagged(&inner[..at]), None);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn record_blob_round_trips() {
        let meta = CardMeta {
            identity: card(&[0x98, 0xD4, 0xDB, 0x3D], CardKind::MifareClassic1K),
            label: String::try_from("Office door").unwrap(),
            first_seen: now(1),
            last_seen: now(9),
            has_dump: true,
            seq: 42,
            dump_blocks_read: 48,
            dump_sectors_read: 12,
            dump_total: 64,
        };
        let mut buf = [0u8; 256];
        let encoded = postcard::to_slice(&meta, &mut buf).expect("encode");
        let decoded: CardMeta = postcard::from_bytes(encoded).expect("decode");
        assert_eq!(decoded, meta);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn unset_label_is_absent_on_wire_and_empty_on_decode() {
        let mut meta = CardMeta {
            identity: card(&[0x98, 0xD4, 0xDB, 0x3D], CardKind::MifareClassic1K),
            label: String::new(),
            first_seen: now(1),
            last_seen: now(1),
            has_dump: false,
            seq: 1,
            dump_blocks_read: 0,
            dump_sectors_read: 0,
            dump_total: 0,
        };
        let mut buf = [0u8; 256];
        let empty_len = meta.encode_tagged(&mut buf).unwrap();
        assert!(CardMeta::decode_tagged(&buf[..empty_len]).unwrap().label.is_empty());
        meta.label = String::try_from("x").unwrap();
        let set_len = meta.encode_tagged(&mut buf).unwrap();
        // The label entry only exists when set.
        assert!(set_len > empty_len);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn legacy_generated_label_decodes_as_unset() {
        // A record written before custom labels carries "<tag> <hex>"
        // in the label field; it must not surface as a custom name.
        let identity = card(&[0x98, 0xD4, 0xDB, 0x3D], CardKind::MifareClassic1K);
        assert_eq!(legacy_auto_label(&identity).as_str(), "1K 98D4DB3D");
        let mut inner = [0u8; 128];
        let mut at = 0;
        crate::tlv::put(&mut inner, &mut at, meta_field::IDENTITY, &identity).unwrap();
        let legacy: String<LABEL_MAX> = String::try_from("1K 98D4DB3D").unwrap();
        crate::tlv::put(&mut inner, &mut at, meta_field::LABEL, &legacy).unwrap();
        assert!(CardMeta::decode_tagged(&inner[..at]).unwrap().label.is_empty());
        // Anything else in the field is a real custom label.
        let mut at = 0;
        crate::tlv::put(&mut inner, &mut at, meta_field::IDENTITY, &identity).unwrap();
        let custom: String<LABEL_MAX> = String::try_from("Office door").unwrap();
        crate::tlv::put(&mut inner, &mut at, meta_field::LABEL, &custom).unwrap();
        assert_eq!(
            CardMeta::decode_tagged(&inner[..at]).unwrap().label.as_str(),
            "Office door",
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn record_missing_identity_fails_to_decode() {
        // A TLV payload with only a seq entry (no identity) is not a
        // card and must be rejected, not silently defaulted.
        let mut inner = [0u8; 32];
        let mut at = 0;
        crate::tlv::put(&mut inner, &mut at, meta_field::SEQ, &5u32).unwrap();
        assert!(CardMeta::decode_tagged(&inner[..at]).is_none());
    }
}
