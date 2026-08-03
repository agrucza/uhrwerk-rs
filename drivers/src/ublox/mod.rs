//! u-blox M10 GNSS receiver driver (UBX protocol) - written for the
//! MIA-M10Q module (ROM SPG 5.10).
//!
//! Sources, all verified against the official documents:
//!  - u-blox M10 SPG 5.10 Interface description, UBX-21035062 R03
//!    (UBX framing section 3.2/3.4, CFG-VALSET 3.10.5, NAV-PVT
//!    3.15.11, RXM-PMREQ 3.16.6, configuration keys chapter 4.9).
//!  - MIA-M10Q Data sheet, UBX-22015849 R08 (default interface
//!    settings, power figures).
//!  - MIA-M10Q Integration manual, UBX-21028173 R05 (power
//!    management, backup modes).
//!
//! Transport-agnostic by design: the receiver speaks UBX over UART
//! (or I2C), but this module never owns an interface - it encodes
//! frames into caller buffers and decodes bytes the caller feeds in.
//! The owning task does the actual IO, mirroring how the rest of the
//! firmware splits driver/task responsibilities.
//!
//! Completeness stance: the [`frame`] layer reaches the ENTIRE UBX
//! protocol - any message can be built with [`frame::encode`] /
//! polled with [`encode_poll`] and any reply surfaces through
//! [`frame::Parser`]. Typed helpers exist for the subset this
//! hardware uses: configuration ([`cfg`]), the position/velocity/
//! time solution ([`nav`]), acknowledgements and the power-down
//! request (this module).
//!
//! Hardware facts worth keeping close (data sheet):
//!  - UART default: **38400 baud**, 8N1 (9600 only in safe boot).
//!    Factory-default output is NMEA chatter (GGA GLL GSA GSV RMC
//!    VTG TXT); input accepts NMEA + UBX. First configuration job is
//!    therefore: NMEA output off, UBX NAV-PVT on.
//!  - Supply currents: acquisition ~12.5 mA / tracking ~10.5 mA at
//!    3.0 V (default constellations) + ~2.4 mA on V_IO; hardware
//!    backup mode (main rail OFF, V_BCKP kept) 28 uA. Inrush up to
//!    100 mA at startup.
//!  - Configuration written to the BBR layer rides V_BCKP and
//!    survives main-rail power cycles - a receiver configured once
//!    streams NAV-PVT immediately on the next power-up, no
//!    reconfiguration needed (still re-send it defensively: BBR is
//!    lost if backup power ever failed).

pub mod cfg;
pub mod frame;
pub mod nav;

/// UBX message class ids used by this driver (interface description
/// section 3.8).
pub mod class {
    /// Navigation results (NAV-PVT et al).
    pub const NAV: u8 = 0x01;
    /// Receiver manager (RXM-PMREQ et al).
    pub const RXM: u8 = 0x02;
    /// Acknowledgements for CFG-class writes.
    pub const ACK: u8 = 0x05;
    /// Configuration (VALSET/VALGET/RST).
    pub const CFG: u8 = 0x06;
    /// Monitoring (MON-VER et al).
    pub const MON: u8 = 0x0a;
}

/// Message ids within their class.
pub mod msg {
    /// NAV-PVT - position/velocity/time solution (92-byte payload).
    pub const NAV_PVT: u8 = 0x07;
    /// RXM-PMREQ - power management request (16-byte payload).
    pub const RXM_PMREQ: u8 = 0x41;
    /// ACK-NAK.
    pub const ACK_NAK: u8 = 0x00;
    /// ACK-ACK.
    pub const ACK_ACK: u8 = 0x01;
    /// CFG-VALSET - set configuration items.
    pub const CFG_VALSET: u8 = 0x8a;
    /// CFG-VALGET - get configuration items.
    pub const CFG_VALGET: u8 = 0x8b;
    /// MON-VER - receiver/software version (poll).
    pub const MON_VER: u8 = 0x04;
}

/// Outcome of an ACK-ACK / ACK-NAK frame: which message the receiver
/// is answering, and whether it accepted it (section 3.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    /// Class of the acknowledged message.
    pub class: u8,
    /// Id of the acknowledged message.
    pub id: u8,
    /// `true` for ACK-ACK, `false` for ACK-NAK.
    pub ok: bool,
}

/// Interpret a parsed frame as an acknowledgement, if it is one. The
/// two-byte payload names the acknowledged message.
pub fn parse_ack(class: u8, id: u8, payload: &[u8]) -> Option<Ack> {
    if class != class::ACK || payload.len() < 2 {
        return None;
    }
    Some(Ack {
        class: payload[0],
        id: payload[1],
        ok: id == msg::ACK_ACK,
    })
}

/// Encode a poll request for any message: a frame with the target
/// class/id and an empty payload (UBX polling mechanism, section
/// 3.5.2). The receiver answers with the same message populated.
/// Needs an 8-byte buffer.
pub fn encode_poll<'a>(class: u8, id: u8, buf: &'a mut [u8]) -> Option<&'a [u8]> {
    frame::encode(class, id, &[], buf)
}

/// Encode UBX-RXM-PMREQ requesting software standby ("backup mode"):
/// indefinite duration, woken by an edge on the UART RX pin (i.e.
/// just start talking to it). The `force` flag is REQUIRED to enter
/// the state (integration manual 3.6.3.2). V_IO keeps BBR/RTC/PIOs
/// alive at ~46 uA.
///
/// Unused on boards that can cut the receiver's main rail instead -
/// hardware backup mode (28 uA, no protocol risk) beats software
/// standby there - but it is the documented alternative when the
/// rail is not switchable. Needs a 24-byte buffer.
pub fn encode_pmreq_backup(buf: &mut [u8]) -> Option<&[u8]> {
    // Payload (interface description 3.16.6): version u8 = 0,
    // reserved[3], duration u32 ms (0 = wait for a wake pin), flags
    // x4 (bit1 backup, bit2 force), wakeupSources x4 (bit3 uartrx).
    let mut payload = [0u8; 16];
    payload[8] = (1 << 1) | (1 << 2);
    payload[12] = 1 << 3;
    frame::encode(class::RXM, msg::RXM_PMREQ, &payload, buf)
}
