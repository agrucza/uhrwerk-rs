//! Runtime configuration.
//!
//! Held as a mutable struct on `SystemManager` so call sites always
//! read through `self.config.*`. Today the values come from
//! `Config::default()` (compile-time defaults). In the future we'll
//! add a persistent backing store (NVS via `esp-storage`) and change
//! the init path to `Config::load_or_default()` without touching any
//! of the call sites.
//!
//! This deliberate indirection is the cheapest future-proofing we can
//! do right now: structuring the values as mutable state instead of
//! `const`s means a settings screen or serial-command debug knob can
//! mutate them at runtime and the new values take effect immediately.
//!
//! PERSISTENCE REALITY CHECK: the flash blob is postcard - positional,
//! not self-describing - so the `serde(default)` attributes below do
//! NOT make added fields backwards-compatible. Adding any field makes
//! old blobs fail wholesale (`DeserializeUnexpectedEnd`) and the
//! loader falls back to `Config::default()`: a one-time reset of every
//! setting per schema change. Accepted for now (few, cheap settings);
//! a versioned-blob migration scheme is queued for when config grows
//! data that must survive (the WiFi effort's credentials). The
//! attributes stay because they cost nothing and become live the day
//! the format is versioned or self-describing.

/// Display power-management parameters.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DisplayConfig {
    /// Seconds of no user activity before the display dims.
    pub dim_timeout_s: u64,
    /// Seconds of no user activity before the display blanks entirely
    /// via `DISPOFF`. Must be greater than `dim_timeout_s`.
    pub off_timeout_s: u64,
    /// Brightness level (0..=255) when the display is fully active.
    /// The panel's init sequence also hardcodes this value, so if the
    /// default here changes, boot will flash the old brightness for a
    /// moment until the first tick reconciles it.
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
    #[cfg_attr(feature = "serde", serde(default))]
    pub night_mode: bool,
    /// When true, the display stays Active indefinitely - the
    /// idle-dim and idle-off timers are skipped. Tradeoff: higher
    /// average current draw on the wrist.
    #[cfg_attr(feature = "serde", serde(default))]
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

/// Top-level runtime config. Sub-structs group related settings.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Config {
    pub display: DisplayConfig,
    /// Master haptic-feedback enable. When false, the manager skips
    /// every motor-pulse / buzz Effect. Defaults on.
    #[cfg_attr(feature = "serde", serde(default))]
    pub haptics_enabled: bool,
    /// Master audible-alert enable. When false, the manager drops the
    /// alarm / timer alert tone (the `AudioCommand::PlayAlarm` effect
    /// is not forwarded to the audio task). Independent of
    /// `haptics_enabled` so the buzz and the tone toggle separately.
    /// Defaults on.
    #[cfg_attr(feature = "serde", serde(default = "default_true"))]
    pub sound_enabled: bool,
    /// Do-not-disturb. When true, alarms / notifications still fire
    /// in the model layer (they're still scheduled and recorded) but
    /// the manager suppresses their hardware side effects (haptics,
    /// audible buzz). Pure UI state today; proper alarm/notification
    /// routing lands when those screens get real backing.
    #[cfg_attr(feature = "serde", serde(default))]
    pub dnd: bool,
    /// Local-time offset from UTC in minutes, applied to GPS time
    /// before it is written into the RTC. User-adjustable in 15 min
    /// steps (odd offsets like +5:45 exist); a fixed offset, so DST
    /// is a manual twice-a-year nudge. Range clamped by the model to
    /// -12 h .. +14 h (the real-world UTC offset span).
    #[cfg_attr(feature = "serde", serde(default = "default_tz_offset"))]
    pub tz_offset_minutes: i16,
}

impl Config {
    /// Compile-time defaults. Tuned for a wrist-worn smartwatch on a
    /// small battery: short dim/off timeouts, dim well below active.
    /// Held as an associated `const` so the Default impl below can
    /// reuse it and so other `const` contexts (array init, etc.)
    /// can reach it without a function call.
    pub const DEFAULT: Self = Self {
        display: DisplayConfig {
            dim_timeout_s: 20,
            off_timeout_s: 30,
            brightness_active: 80,
            brightness_dim: 16,
            night_mode: false,
            always_on: false,
        },
        haptics_enabled: true,
        sound_enabled: true,
        dnd: false,
        tz_offset_minutes: 120,
    };

    /// Clamp bounds for `tz_offset_minutes`: UTC-12:00 (Baker
    /// Island) to UTC+14:00 (Line Islands).
    pub const TZ_OFFSET_MIN: i16 = -12 * 60;
    pub const TZ_OFFSET_MAX: i16 = 14 * 60;
}

/// Serde fallback for [`Config::tz_offset_minutes`]: keep the
/// compile-time default (CEST) instead of a silent UTC+0. Same
/// caveat as [`default_true`] - inert until the blob format can
/// actually skip missing fields.
#[cfg(feature = "serde")]
fn default_tz_offset() -> i16 {
    Config::DEFAULT.tz_offset_minutes
}

/// Serde fallback for [`Config::sound_enabled`]. NOTE: with the
/// current postcard blob this never fires on missing fields (see the
/// module docs - old blobs fail wholesale and reset to defaults);
/// it exists for a future versioned/self-describing format, where a
/// bare `serde(default)` would give `false` and silently mute
/// upgraded installs.
#[cfg(feature = "serde")]
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self::DEFAULT
    }
}
