//! Runtime configuration - THE tree of everything the watch remembers.
//!
//! One root struct, organized by owner: `config.display.*`,
//! `config.alerts.*`, `config.time.*`, `config.gps.*`,
//! `config.wifi.*`, `config.alarms.*`. Every persisted setting is
//! reachable through its owning group; there is no second persistent
//! store.
//!
//! Held as a mutable struct on `SystemManager`; loaded from the flash
//! blob at boot (BEFORE hardware init - hardware initializes from
//! stored settings) and saved on change. Defaults apply per-field
//! when the blob lacks or can't decode a value.
//!
//! # Persistence: the tagged field store
//!
//! The blob's payload is a flat serialized list of `(field id,
//! length, value)` entries - TLV. Each *leaf setting* carries a
//! stable numeric id ([`field_id`]); values are individually
//! postcard-encoded. Loading walks the list: known id -> decode that
//! field, unknown id -> skip it, missing id -> keep the default.
//! The struct tree above is a pure code-side shape - no name, no
//! nesting, no order exists on flash - which is what makes
//! reorganizing this tree free: the ids never move.
//!
//! ## Rules when changing this schema
//!
//! - **Adding a setting**: give it the next free id (see
//!   `NEXT_FIELD_ID`), add one line each to `encode_tagged`,
//!   `decode_tagged`, and the exhaustive `scrambled()` test config
//!   (the compiler forces the last one - it's a full struct literal).
//! - **Renaming/moving a field in the tree**: free - the id is the
//!   identity.
//! - **Changing a field's TYPE**: allocate a NEW id and retire the
//!   old one; decoding an old value into a new type can succeed by
//!   coincidence and produce garbage. Retired ids are never reused.
//! - **Enum leaf values** (e.g. [`GpsTrackingCadence`]): variants
//!   are append-only - postcard encodes the variant index.
//! - **Alarm slots**: one id per slot (`ALARM_SLOT_BASE + i`), value
//!   = the whole entry; an `AlarmEntry` schema change costs the
//!   stored slots, never the rest of the config.

use crate::ui::types::AlarmState;
#[cfg(feature = "serde")]
use crate::ui::types::MAX_ALARMS;
use heapless::String;

/// Display power-management parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayConfig {
    /// Seconds of no user activity before the display dims.
    pub dim_timeout_s: u64,
    /// Seconds of no user activity before the display blanks entirely
    /// via `DISPOFF`. Must be greater than `dim_timeout_s`.
    pub off_timeout_s: u64,
    /// Brightness level (0..=255) when the display is fully active.
    /// Programmed into the panel's init sequence at boot, so the
    /// first lit frame already matches the stored setting.
    pub brightness_active: u8,
    /// Brightness level (0..=255) when the display is dimmed. AMOLED
    /// current scales roughly with lit pixels * brightness, so a low
    /// value here is where most of the idle-current savings come from
    /// on a wrist-worn device.
    pub brightness_dim: u8,
    /// When true, clamps the effective Active-state brightness to
    /// [`DisplayConfig::NIGHT_MODE_MAX_HW`] regardless of
    /// `brightness_active`. The user's slider-set value is preserved
    /// in `brightness_active`; only the hardware register is limited.
    pub night_mode: bool,
    /// When true, the display stays Active indefinitely - the
    /// idle-dim and idle-off timers are skipped. Tradeoff: higher
    /// average current draw on the wrist.
    pub always_on: bool,
}

impl DisplayConfig {
    /// Upper bound on `brightness_active` when `night_mode` is on.
    /// 76 ≈ 30 % of the 0..=255 panel register (spec's "caps max at
    /// 30 %").
    pub const NIGHT_MODE_MAX_HW: u8 = 76;

    /// Max allowed slider percent given the current night_mode
    /// setting. Used by the Quick Access slider to clamp the
    /// draggable range.
    pub const fn max_brightness_pct(&self) -> u8 {
        if self.night_mode { 30 } else { 100 }
    }
}

/// Alert-behavior switches: how the watch is allowed to get your
/// attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertsConfig {
    /// Master audible-alert enable. When false, the manager drops the
    /// alarm / timer alert tone. Independent of `haptics_enabled` so
    /// the buzz and the tone toggle separately. Defaults on.
    pub sound_enabled: bool,
    /// Master haptic-feedback enable. When false, the manager skips
    /// every motor-pulse / buzz Effect. Defaults on.
    pub haptics_enabled: bool,
    /// Do-not-disturb. When true, alarms / notifications still fire
    /// in the model layer (they're still scheduled and recorded) but
    /// the manager suppresses their hardware side effects (haptics,
    /// audible buzz).
    pub dnd: bool,
}

/// Wall-clock / timezone parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeConfig {
    /// Local-time offset from UTC in minutes, applied to GPS/NTP time
    /// before it is written into the RTC. User-adjustable in 15 min
    /// steps (odd offsets like +5:45 exist); a fixed offset, so DST
    /// is a manual twice-a-year nudge. Range clamped by the model to
    /// -12 h .. +14 h (the real-world UTC offset span).
    pub tz_offset_minutes: i16,
}

/// GPS tracking preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpsConfig {
    /// Tracking on/off - the switch the settings toggle and the
    /// no-fix auto-off flip. The model schedules the sessions and
    /// auto-disables after consecutive fixless sessions.
    pub tracking_enabled: bool,
    /// Session cadence used while tracking is enabled. Kept when
    /// tracking is off (including auto-off) so re-enabling resumes
    /// the user's pick.
    pub tracking_cadence: GpsTrackingCadence,
}

/// The single stored WiFi network (user decision: one network, not
/// a list). Credentials live in the tagged store like every other
/// setting; an empty `ssid` means "not provisioned" and an empty
/// `passphrase` with a set `ssid` means an open network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiConfig {
    /// Network name, IEEE 802.11 maximum of 32 bytes.
    pub ssid: String<{ WifiConfig::SSID_MAX }>,
    /// WPA2 passphrase: 8..=63 printable ASCII characters (the
    /// capacity rounds up to 64).
    pub passphrase: String<{ WifiConfig::PASSPHRASE_CAP }>,
}

impl WifiConfig {
    /// SSID capacity in bytes (the 802.11 limit).
    pub const SSID_MAX: usize = 32;
    /// Longest WPA2 passphrase the user can type.
    pub const PASSPHRASE_MAX: usize = 63;
    /// Storage capacity of the passphrase field.
    pub const PASSPHRASE_CAP: usize = 64;

    pub const DEFAULT: Self = Self { ssid: String::new(), passphrase: String::new() };

    /// True once a network name is stored.
    pub fn is_set(&self) -> bool {
        !self.ssid.is_empty()
    }
}

/// Top-level runtime config - the settings tree. `Clone`, not
/// `Copy`: the WiFi credentials are heapless Strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub display: DisplayConfig,
    pub alerts: AlertsConfig,
    pub time: TimeConfig,
    pub gps: GpsConfig,
    pub wifi: WifiConfig,
    /// The alarm list (plus its transient runtime flags, which are
    /// never persisted - see [`AlarmState`]). Persisted per slot:
    /// each entry is its own tagged field.
    pub alarms: AlarmState,
}

/// Cadence of GPS tracking sessions. `Continuous` re-kicks a
/// full-budget session the moment the last one ends (receiver
/// effectively always on - a deliberate navigation-style mode,
/// hours of battery, not days); the interval modes run short
/// hot-start sessions with the rail off in between.
///
/// Persisted as a tagged leaf value: variants are APPEND-ONLY
/// (postcard stores the variant index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum GpsTrackingCadence {
    Continuous,
    Every15s,
    Every30s,
    #[default]
    Every60s,
}

impl GpsTrackingCadence {
    /// Seconds between session kicks; 0 = continuous (re-kick as
    /// soon as the previous session reports a terminal state).
    pub fn interval_secs(self) -> u32 {
        match self {
            GpsTrackingCadence::Continuous => 0,
            GpsTrackingCadence::Every15s => 15,
            GpsTrackingCadence::Every30s => 30,
            GpsTrackingCadence::Every60s => 60,
        }
    }
}

impl Config {
    /// Compile-time defaults. Tuned for a wrist-worn smartwatch on a
    /// small battery: short dim/off timeouts, dim well below active.
    pub const DEFAULT: Self = Self {
        display: DisplayConfig {
            dim_timeout_s: 20,
            off_timeout_s: 30,
            brightness_active: 80,
            brightness_dim: 16,
            night_mode: false,
            always_on: false,
        },
        alerts: AlertsConfig {
            sound_enabled: true,
            haptics_enabled: true,
            dnd: false,
        },
        time: TimeConfig { tz_offset_minutes: 120 },
        gps: GpsConfig {
            tracking_enabled: false,
            tracking_cadence: GpsTrackingCadence::Every60s,
        },
        wifi: WifiConfig::DEFAULT,
        alarms: AlarmState::DEFAULT,
    };

    /// Clamp bounds for `time.tz_offset_minutes`: UTC-12:00 (Baker
    /// Island) to UTC+14:00 (Line Islands).
    pub const TZ_OFFSET_MIN: i16 = -12 * 60;
    pub const TZ_OFFSET_MAX: i16 = 14 * 60;
}

impl Default for Config {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// -- Tagged field store ------------------------------------------------------

/// Stable field ids. NEVER reuse a retired id; a type change
/// allocates a new id (see the module docs). Scalar leaves live in
/// 1..=31; alarm slots occupy `ALARM_SLOT_BASE..ALARM_SLOT_BASE +
/// MAX_ALARMS`.
///
/// NEXT_FIELD_ID: 15 (scalars), alarm slots 32..=39 assigned.
#[cfg(feature = "serde")]
mod field_id {
    pub const DIM_TIMEOUT_S: u16 = 1;
    pub const OFF_TIMEOUT_S: u16 = 2;
    pub const BRIGHTNESS_ACTIVE: u16 = 3;
    pub const BRIGHTNESS_DIM: u16 = 4;
    pub const NIGHT_MODE: u16 = 5;
    pub const ALWAYS_ON: u16 = 6;
    pub const HAPTICS_ENABLED: u16 = 7;
    pub const SOUND_ENABLED: u16 = 8;
    pub const DND: u16 = 9;
    pub const TZ_OFFSET_MINUTES: u16 = 10;
    pub const GPS_TRACKING_ENABLED: u16 = 11;
    pub const GPS_TRACKING_CADENCE: u16 = 12;
    pub const WIFI_SSID: u16 = 13;
    pub const WIFI_PASSPHRASE: u16 = 14;
    /// One id per alarm slot: `ALARM_SLOT_BASE + slot_index`.
    pub const ALARM_SLOT_BASE: u16 = 32;
}

/// Upper bound on the tagged payload. Scalars ~90 B + 8 alarm slots
/// at ~8 B each + the WiFi credentials (SSID 32 + passphrase 63 +
/// headers) ~ 260 B worst case, with headroom for growth. The
/// storage layer's blob buffer is 512 B - keep this below it.
#[cfg(feature = "serde")]
const TAGGED_MAX: usize = 384;

#[cfg(feature = "serde")]
impl Config {
    /// Serialize as a TLV entry list into `buf`; returns the used
    /// length. Entry: `id: u16 LE, len: u8, value: postcard bytes`.
    /// `Err` only on buffer overflow (a schema far larger than
    /// [`TAGGED_MAX`] budgeted for).
    fn encode_tagged(&self, buf: &mut [u8]) -> Result<usize, ()> {
        use field_id::*;
        let mut at = 0usize;
        let d = &self.display;
        put(buf, &mut at, DIM_TIMEOUT_S, &d.dim_timeout_s)?;
        put(buf, &mut at, OFF_TIMEOUT_S, &d.off_timeout_s)?;
        put(buf, &mut at, BRIGHTNESS_ACTIVE, &d.brightness_active)?;
        put(buf, &mut at, BRIGHTNESS_DIM, &d.brightness_dim)?;
        put(buf, &mut at, NIGHT_MODE, &d.night_mode)?;
        put(buf, &mut at, ALWAYS_ON, &d.always_on)?;
        let a = &self.alerts;
        put(buf, &mut at, HAPTICS_ENABLED, &a.haptics_enabled)?;
        put(buf, &mut at, SOUND_ENABLED, &a.sound_enabled)?;
        put(buf, &mut at, DND, &a.dnd)?;
        put(buf, &mut at, TZ_OFFSET_MINUTES, &self.time.tz_offset_minutes)?;
        put(buf, &mut at, GPS_TRACKING_ENABLED, &self.gps.tracking_enabled)?;
        put(buf, &mut at, GPS_TRACKING_CADENCE, &self.gps.tracking_cadence)?;
        put(buf, &mut at, WIFI_SSID, &self.wifi.ssid)?;
        put(buf, &mut at, WIFI_PASSPHRASE, &self.wifi.passphrase)?;
        // Alarm slots: only the entries persist - the runtime flags
        // (`active_hw` / `alerting` / `snoozed`) are never encoded.
        for (i, entry) in self.alarms.entries.iter().enumerate() {
            put(buf, &mut at, ALARM_SLOT_BASE + i as u16, entry)?;
        }
        Ok(at)
    }

    /// Decode a TLV entry list. Starts from [`Config::DEFAULT`] and
    /// overwrites every field whose id is present and decodable -
    /// unknown ids are skipped, missing or undecodable fields keep
    /// their default, a truncated tail keeps everything read so far.
    /// Total garbage therefore degrades to plain defaults; there is
    /// no error path.
    fn decode_tagged(bytes: &[u8]) -> Config {
        use field_id::*;
        let mut c = Config::DEFAULT;
        let mut i = 0usize;
        while i + 3 <= bytes.len() {
            let id = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
            let len = bytes[i + 2] as usize;
            i += 3;
            if i + len > bytes.len() {
                break;
            }
            let val = &bytes[i..i + len];
            i += len;
            match id {
                DIM_TIMEOUT_S => get(val, &mut c.display.dim_timeout_s),
                OFF_TIMEOUT_S => get(val, &mut c.display.off_timeout_s),
                BRIGHTNESS_ACTIVE => get(val, &mut c.display.brightness_active),
                BRIGHTNESS_DIM => get(val, &mut c.display.brightness_dim),
                NIGHT_MODE => get(val, &mut c.display.night_mode),
                ALWAYS_ON => get(val, &mut c.display.always_on),
                HAPTICS_ENABLED => get(val, &mut c.alerts.haptics_enabled),
                SOUND_ENABLED => get(val, &mut c.alerts.sound_enabled),
                DND => get(val, &mut c.alerts.dnd),
                TZ_OFFSET_MINUTES => get(val, &mut c.time.tz_offset_minutes),
                GPS_TRACKING_ENABLED => get(val, &mut c.gps.tracking_enabled),
                GPS_TRACKING_CADENCE => get(val, &mut c.gps.tracking_cadence),
                WIFI_SSID => get(val, &mut c.wifi.ssid),
                WIFI_PASSPHRASE => get(val, &mut c.wifi.passphrase),
                id if (ALARM_SLOT_BASE..ALARM_SLOT_BASE + MAX_ALARMS as u16)
                    .contains(&id) =>
                {
                    let slot = (id - ALARM_SLOT_BASE) as usize;
                    get(val, &mut c.alarms.entries[slot]);
                }
                _ => {} // written by a newer firmware - skip
            }
        }
        c
    }
}

/// Append one TLV entry. The value is postcard-encoded in place
/// after a reserved 3-byte header.
#[cfg(feature = "serde")]
fn put<T: serde::Serialize>(
    buf: &mut [u8],
    at: &mut usize,
    id: u16,
    value: &T,
) -> Result<(), ()> {
    let start = *at + 3;
    if start > buf.len() {
        return Err(());
    }
    let used = postcard::to_slice(value, &mut buf[start..])
        .map_err(|_| ())?
        .len();
    if used > u8::MAX as usize {
        return Err(());
    }
    buf[*at..*at + 2].copy_from_slice(&id.to_le_bytes());
    buf[*at + 2] = used as u8;
    *at = start + used;
    Ok(())
}

/// Decode one field value; on failure the target keeps its default
/// (per-field tolerance is the whole point of the tagged store).
#[cfg(feature = "serde")]
fn get<T: for<'de> serde::Deserialize<'de>>(bytes: &[u8], into: &mut T) {
    if let Ok(v) = postcard::from_bytes(bytes) {
        *into = v;
    }
}

/// The blob-facing serde surface: `Config` serializes as one opaque
/// `bytes` value containing the TLV list, so the storage layer's
/// generic `StoredBlob` envelope (and SD mirroring) is untouched.
#[cfg(feature = "serde")]
impl serde::Serialize for Config {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut buf = [0u8; TAGGED_MAX];
        let len = self
            .encode_tagged(&mut buf)
            .map_err(|_| serde::ser::Error::custom("tagged config overflow"))?;
        s.serialize_bytes(&buf[..len])
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BytesVisitor;
        impl<'de> serde::de::Visitor<'de> for BytesVisitor {
            type Value = Config;
            fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                f.write_str("tagged config bytes")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Config, E> {
                Ok(Config::decode_tagged(v))
            }
            fn visit_borrowed_bytes<E: serde::de::Error>(
                self,
                v: &'de [u8],
            ) -> Result<Config, E> {
                Ok(Config::decode_tagged(v))
            }
        }
        d.deserialize_bytes(BytesVisitor)
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;
    use crate::ui::types::AlarmEntry;

    /// Every field deliberately differs from `Config::DEFAULT`, and
    /// the struct literals are exhaustive on purpose: adding a field
    /// breaks this function at compile time until the test (and thus
    /// the codec) is updated.
    fn scrambled() -> Config {
        let mut alarms = AlarmState::DEFAULT;
        alarms.entries[0] = AlarmEntry {
            hour: 6,
            minute: 45,
            days: 0b0011_1110,
            enabled: true,
        };
        alarms.entries[7] = AlarmEntry {
            hour: 22,
            minute: 15,
            days: 0b0100_0001,
            enabled: true,
        };
        Config {
            display: DisplayConfig {
                dim_timeout_s: 77,
                off_timeout_s: 99,
                brightness_active: 200,
                brightness_dim: 3,
                night_mode: true,
                always_on: true,
            },
            alerts: AlertsConfig {
                sound_enabled: false,
                haptics_enabled: false,
                dnd: true,
            },
            time: TimeConfig { tz_offset_minutes: -330 },
            gps: GpsConfig {
                tracking_enabled: true,
                tracking_cadence: GpsTrackingCadence::Every15s,
            },
            wifi: WifiConfig {
                ssid: String::try_from("Attic Mesh 5G").unwrap(),
                passphrase: String::try_from("c0rrect h0rse battery").unwrap(),
            },
            alarms,
        }
    }

    #[test]
    fn wifi_credentials_roundtrip_at_capacity() {
        // Full-length SSID (32) and passphrase (63) - the longest
        // values the keyboard can produce - must survive the u8
        // length header and TAGGED_MAX.
        let mut cfg = Config::DEFAULT;
        cfg.wifi.ssid = String::try_from("abcdefghijklmnopqrstuvwxyz012345").unwrap();
        cfg.wifi.passphrase = String::try_from(
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!",
        )
        .unwrap();
        assert_eq!(cfg.wifi.ssid.len(), WifiConfig::SSID_MAX);
        assert_eq!(cfg.wifi.passphrase.len(), WifiConfig::PASSPHRASE_MAX);
        let mut buf = [0u8; TAGGED_MAX];
        let len = cfg.encode_tagged(&mut buf).unwrap();
        assert_eq!(Config::decode_tagged(&buf[..len]), cfg);
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let cfg = scrambled();
        let mut buf = [0u8; TAGGED_MAX];
        let len = cfg.encode_tagged(&mut buf).unwrap();
        assert_eq!(Config::decode_tagged(&buf[..len]), cfg);
    }

    #[test]
    fn runtime_alarm_flags_do_not_persist() {
        let mut cfg = scrambled();
        cfg.alarms.alerting = true;
        cfg.alarms.snoozed = true;
        cfg.alarms.active_hw = Some(3);
        let mut buf = [0u8; TAGGED_MAX];
        let len = cfg.encode_tagged(&mut buf).unwrap();
        let back = Config::decode_tagged(&buf[..len]);
        // Entries round-trip; runtime flags come back as defaults.
        assert_eq!(back.alarms.entries, cfg.alarms.entries);
        assert!(!back.alarms.alerting);
        assert!(!back.alarms.snoozed);
        assert_eq!(back.alarms.active_hw, None);
    }

    #[test]
    fn unknown_field_is_skipped() {
        let cfg = scrambled();
        let mut buf = [0u8; TAGGED_MAX];
        let mut len = cfg.encode_tagged(&mut buf).unwrap();
        // Append an entry from "a newer firmware": id 999, 4 bytes.
        buf[len..len + 2].copy_from_slice(&999u16.to_le_bytes());
        buf[len + 2] = 4;
        buf[len + 3..len + 7].copy_from_slice(&[1, 2, 3, 4]);
        len += 7;
        assert_eq!(Config::decode_tagged(&buf[..len]), cfg);
    }

    #[test]
    fn missing_field_keeps_default() {
        // Encode only one entry: everything else must default.
        let mut buf = [0u8; TAGGED_MAX];
        let mut at = 0;
        put(&mut buf, &mut at, field_id::TZ_OFFSET_MINUTES, &-330i16).unwrap();
        let decoded = Config::decode_tagged(&buf[..at]);
        assert_eq!(decoded.time.tz_offset_minutes, -330);
        assert_eq!(decoded.display, Config::DEFAULT.display);
        assert_eq!(decoded.alerts, Config::DEFAULT.alerts);
        assert_eq!(decoded.alarms.entries, Config::DEFAULT.alarms.entries);
    }

    #[test]
    fn truncated_tail_keeps_prefix() {
        let cfg = scrambled();
        let mut buf = [0u8; TAGGED_MAX];
        let len = cfg.encode_tagged(&mut buf).unwrap();
        // Cut into the last entry's value.
        let cut = Config::decode_tagged(&buf[..len - 1]);
        // First field survived; no panic anywhere.
        assert_eq!(cut.display.dim_timeout_s, 77);
    }

    #[test]
    fn garbage_degrades_to_defaults() {
        let junk = [0xDE, 0xAD, 0xBE, 0xEF, 0x42];
        assert_eq!(Config::decode_tagged(&junk), Config::DEFAULT);
        assert_eq!(Config::decode_tagged(&[]), Config::DEFAULT);
    }

    #[test]
    fn serde_surface_roundtrips_through_postcard() {
        // The integration reality: Config travels through the storage
        // layer's generic postcard envelope as an opaque bytes value.
        let cfg = scrambled();
        let mut buf = [0u8; 512];
        let bytes = postcard::to_slice(&cfg, &mut buf).unwrap();
        let back: Config = postcard::from_bytes(bytes).unwrap();
        assert_eq!(back, cfg);
    }
}
