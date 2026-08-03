//! Configuration interface: key ids + UBX-CFG-VALSET payload builder
//! (interface description sections 3.10.5 and 4.9).
//!
//! Every configuration item is addressed by a 32-bit key id whose
//! bits 30..28 encode the value size (1: one bit stored as a byte,
//! 2: one byte, 3: two bytes, 4: four bytes, 5: eight bytes). The
//! builder asserts the width of each `add_*` call against its key in
//! debug builds.
//!
//! Layer semantics (section 4.3): RAM is the live configuration,
//! BBR survives main-rail power cycles on V_BCKP (this board: the
//! always-on VRTC), flash is not fitted on the MIA-M10Q's host - so
//! RAM+BBR is the durable choice here.

/// Apply to the live (RAM) configuration.
pub const LAYER_RAM: u8 = 1 << 0;
/// Apply to battery-backed RAM - survives main-rail off while
/// V_BCKP is supplied.
pub const LAYER_BBR: u8 = 1 << 1;

/// Configuration key ids (all verified against interface
/// description chapter 4.9).
pub mod key {
    /// CFG-UART1-BAUDRATE (U4) - the data sheet default is 38400.
    pub const UART1_BAUDRATE: u32 = 0x4052_0001;
    /// CFG-UART1-ENABLED (L).
    pub const UART1_ENABLED: u32 = 0x1052_0005;
    /// CFG-UART1INPROT-UBX (L) - UBX accepted as input (default on).
    pub const UART1INPROT_UBX: u32 = 0x1073_0001;
    /// CFG-UART1INPROT-NMEA (L) - NMEA accepted as input.
    pub const UART1INPROT_NMEA: u32 = 0x1073_0002;
    /// CFG-UART1OUTPROT-UBX (L) - UBX emitted (default on).
    pub const UART1OUTPROT_UBX: u32 = 0x1074_0001;
    /// CFG-UART1OUTPROT-NMEA (L) - NMEA emitted (default ON: the
    /// factory GGA/GLL/GSA/GSV/RMC/VTG/TXT chatter; turn it off).
    pub const UART1OUTPROT_NMEA: u32 = 0x1074_0002;
    /// CFG-MSGOUT-UBX_NAV_PVT_UART1 (U1) - NAV-PVT per how many
    /// navigation epochs on UART1 (0 = off, 1 = every epoch).
    pub const MSGOUT_NAV_PVT_UART1: u32 = 0x2091_0007;
    /// CFG-RATE-MEAS (U2, ms) - time between GNSS measurements
    /// (1000 = 1 Hz, minimum 25).
    pub const RATE_MEAS: u32 = 0x3021_0001;
    /// CFG-RATE-NAV (U2) - measurements per navigation solution.
    pub const RATE_NAV: u32 = 0x3021_0002;
    /// CFG-NAVSPG-DYNMODEL (E1) - dynamic platform model.
    pub const NAVSPG_DYNMODEL: u32 = 0x2011_0021;
    /// CFG-PM-OPERATEMODE (E1) - 0 FULL, 1 PSMOO, 2 PSMCT. Unused
    /// while the host hard-gates the main rail; documented for a
    /// future power-save-mode experiment (integration manual 3.6.2).
    pub const PM_OPERATEMODE: u32 = 0x20d0_0001;
}

/// CFG-NAVSPG-DYNMODEL constants (Table 23). Only the ones relevant
/// to this hardware; WRIST is flagged "not available in all
/// products" - treat an ACK-NAK on it as non-fatal and fall back to
/// PORTABLE.
pub mod dynmodel {
    /// Portable - the general-purpose default.
    pub const PORTABLE: u8 = 0;
    /// Pedestrian.
    pub const PEDESTRIAN: u8 = 3;
    /// Wrist-worn watch: filters wrist-swing motion.
    pub const WRIST: u8 = 9;
}

/// Width in bytes of a key's value, from key id bits 30..28.
const fn key_width(key: u32) -> usize {
    match (key >> 28) & 0x7 {
        1 | 2 => 1,
        3 => 2,
        4 => 4,
        5 => 8,
        _ => 0,
    }
}

/// Builds a transactionless UBX-CFG-VALSET payload (message version
/// 0) into a caller buffer: header `[0, layers, 0, 0]` then packed
/// key/value pairs, little-endian. Frame it with
/// [`super::frame::encode`] under class [`super::class::CFG`], id
/// [`super::msg::CFG_VALSET`]. The receiver answers ACK-ACK/NAK.
///
/// Capacity overflow is remembered and surfaced by [`Self::payload`]
/// returning `None` - one check at the end instead of one per add.
pub struct ValSet<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflow: bool,
}

impl<'a> ValSet<'a> {
    /// Start a VALSET targeting `layers` (OR of [`LAYER_RAM`] /
    /// [`LAYER_BBR`]). Needs 4 bytes + 4+width per item.
    pub fn new(buf: &'a mut [u8], layers: u8) -> Self {
        let mut s = Self { buf, len: 0, overflow: false };
        s.raw(&[0, layers, 0, 0]);
        s
    }

    fn raw(&mut self, bytes: &[u8]) {
        if self.buf.len() - self.len < bytes.len() {
            self.overflow = true;
            return;
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn item(&mut self, key: u32, value: &[u8]) -> &mut Self {
        debug_assert_eq!(key_width(key), value.len(), "value width != key width");
        self.raw(&key.to_le_bytes());
        self.raw(value);
        self
    }

    /// Add a one-bit (L) item.
    pub fn add_bool(&mut self, key: u32, v: bool) -> &mut Self {
        self.item(key, &[v as u8])
    }

    /// Add a one-byte (U1/E1/X1) item.
    pub fn add_u8(&mut self, key: u32, v: u8) -> &mut Self {
        self.item(key, &[v])
    }

    /// Add a two-byte (U2/E2/X2) item.
    pub fn add_u16(&mut self, key: u32, v: u16) -> &mut Self {
        self.item(key, &v.to_le_bytes())
    }

    /// Add a four-byte (U4/E4/X4) item.
    pub fn add_u32(&mut self, key: u32, v: u32) -> &mut Self {
        self.item(key, &v.to_le_bytes())
    }

    /// The finished payload, or `None` if the buffer overflowed or
    /// no items were added.
    pub fn payload(&self) -> Option<&[u8]> {
        if self.overflow || self.len <= 4 {
            None
        } else {
            Some(&self.buf[..self.len])
        }
    }
}
