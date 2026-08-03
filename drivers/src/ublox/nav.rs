//! UBX-NAV-PVT - the one-message position/velocity/time solution
//! (interface description section 3.15.11, 92-byte payload).

/// GNSSfix type (payload byte 20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FixType {
    #[default]
    NoFix,
    DeadReckoning,
    Fix2D,
    Fix3D,
    GnssDeadReckoning,
    TimeOnly,
    /// A value the interface description doesn't define.
    Unknown(u8),
}

impl From<u8> for FixType {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::NoFix,
            1 => Self::DeadReckoning,
            2 => Self::Fix2D,
            3 => Self::Fix3D,
            4 => Self::GnssDeadReckoning,
            5 => Self::TimeOnly,
            other => Self::Unknown(other),
        }
    }
}

/// Parsed NAV-PVT. Raw units are kept exactly as the wire defines
/// them (documented per field) - scaling to UI units is the
/// caller's business.
#[derive(Debug, Clone, Copy, Default)]
pub struct NavPvt {
    /// GPS time of week, ms.
    pub itow_ms: u32,
    /// UTC calendar date/time.
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
    /// Validity flags (payload byte 11): see `VALID_*`.
    pub valid: u8,
    /// Time accuracy estimate, ns.
    pub t_acc_ns: u32,
    /// Sub-second UTC fraction, -1e9..1e9 ns (can be negative:
    /// `sec` is rounded to the nearest second).
    pub nano_ns: i32,
    /// Fix type (payload byte 20).
    pub fix_type: FixType,
    /// Fix status flags (payload byte 21): see `FLAGS_GNSS_FIX_OK`.
    pub flags: u8,
    /// Additional flags (payload byte 22): see `FLAGS2_CONFIRMED_*`.
    pub flags2: u8,
    /// Satellites used in the navigation solution.
    pub num_sv: u8,
    /// Longitude / latitude, 1e-7 degrees.
    pub lon_1e7: i32,
    pub lat_1e7: i32,
    /// Height above ellipsoid / above mean sea level, mm.
    pub height_mm: i32,
    pub hmsl_mm: i32,
    /// Horizontal / vertical accuracy estimate, mm.
    pub h_acc_mm: u32,
    pub v_acc_mm: u32,
    /// NED velocities, mm/s.
    pub vel_n_mms: i32,
    pub vel_e_mms: i32,
    pub vel_d_mms: i32,
    /// Ground speed (2-D), mm/s.
    pub gspeed_mms: i32,
    /// Heading of motion (2-D), 1e-5 degrees.
    pub head_motion_1e5: i32,
    /// Speed accuracy estimate, mm/s.
    pub s_acc_mms: u32,
    /// Heading accuracy estimate, 1e-5 degrees.
    pub head_acc_1e5: u32,
    /// Position DOP, 0.01.
    pub pdop_1e2: u16,
    /// Additional flags (payload bytes 78-79): bit 0 = lon/lat/
    /// height/hMSL are invalid.
    pub flags3: u16,
}

/// `valid` bit 0: UTC date is valid.
pub const VALID_DATE: u8 = 1 << 0;
/// `valid` bit 1: UTC time of day is valid.
pub const VALID_TIME: u8 = 1 << 1;
/// `valid` bit 2: time of day fully resolved (no seconds
/// uncertainty).
pub const VALID_FULLY_RESOLVED: u8 = 1 << 2;
/// `flags` bit 0: fix is valid (within DOP and accuracy masks).
pub const FLAGS_GNSS_FIX_OK: u8 = 1 << 0;
/// `flags2` bit 6: UTC date validity could be confirmed.
pub const FLAGS2_CONFIRMED_DATE: u8 = 1 << 6;
/// `flags2` bit 7: UTC time-of-day validity could be confirmed.
pub const FLAGS2_CONFIRMED_TIME: u8 = 1 << 7;
/// `flags3` bit 0: the position fields are invalid.
pub const FLAGS3_INVALID_LLH: u16 = 1 << 0;

impl NavPvt {
    /// Wire payload length.
    pub const LEN: usize = 92;

    /// Parse a NAV-PVT payload. `None` if it is too short.
    pub fn parse(p: &[u8]) -> Option<Self> {
        if p.len() < Self::LEN {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes([p[o], p[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
        let i32_at = |o: usize| u32_at(o) as i32;
        Some(Self {
            itow_ms: u32_at(0),
            year: u16_at(4),
            month: p[6],
            day: p[7],
            hour: p[8],
            min: p[9],
            sec: p[10],
            valid: p[11],
            t_acc_ns: u32_at(12),
            nano_ns: i32_at(16),
            fix_type: FixType::from(p[20]),
            flags: p[21],
            flags2: p[22],
            num_sv: p[23],
            lon_1e7: i32_at(24),
            lat_1e7: i32_at(28),
            height_mm: i32_at(32),
            hmsl_mm: i32_at(36),
            h_acc_mm: u32_at(40),
            v_acc_mm: u32_at(44),
            vel_n_mms: i32_at(48),
            vel_e_mms: i32_at(52),
            vel_d_mms: i32_at(56),
            gspeed_mms: i32_at(60),
            head_motion_1e5: i32_at(64),
            s_acc_mms: u32_at(68),
            head_acc_1e5: u32_at(72),
            pdop_1e2: u16_at(76),
            flags3: u16_at(78),
        })
    }

    /// The UTC date+time fields are trustworthy: date and time valid
    /// AND the seconds ambiguity resolved. This is the gate for
    /// setting a real-time clock from the receiver.
    pub fn time_trustworthy(&self) -> bool {
        self.valid & (VALID_DATE | VALID_TIME | VALID_FULLY_RESOLVED)
            == (VALID_DATE | VALID_TIME | VALID_FULLY_RESOLVED)
    }

    /// A usable position: 2-D/3-D fix, flagged OK, coordinates not
    /// marked invalid.
    pub fn position_usable(&self) -> bool {
        matches!(self.fix_type, FixType::Fix2D | FixType::Fix3D)
            && self.flags & FLAGS_GNSS_FIX_OK != 0
            && self.flags3 & FLAGS3_INVALID_LLH == 0
    }
}
