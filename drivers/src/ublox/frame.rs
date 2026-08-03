//! UBX frame codec (interface description sections 3.2-3.4).
//!
//! Frame layout: `B5 62 <class> <id> <len lo> <len hi> <payload...>
//! <ck_a> <ck_b>`, length = payload bytes only, checksum = 8-bit
//! Fletcher over class..payload inclusive.

/// First sync byte of every UBX frame.
pub const SYNC1: u8 = 0xb5;
/// Second sync byte of every UBX frame.
pub const SYNC2: u8 = 0x62;

/// Largest payload the streaming parser buffers. NAV-PVT is 92
/// bytes; VALGET replies and MON-VER run longer. Frames beyond this
/// are consumed and reported as [`Poll::Skipped`] rather than
/// corrupting the stream.
pub const MAX_PAYLOAD: usize = 256;

/// Longest frame the parser will silently drain past
/// [`MAX_PAYLOAD`]. A declared length above this is treated as a
/// false sync rather than a real frame: `B5 62` is legal *inside*
/// binary payloads, and a parser that resynchronizes mid-stream can
/// land on one and read garbage "length" bytes - draining up to
/// 64 KB would then swallow minutes of real stream (observed on
/// hardware: a whole session of 1 Hz NAV-PVT eaten in silence).
/// Nothing this receiver emits unpolled comes close to this size.
pub const DRAIN_CAP: usize = 1024;

/// 8-bit Fletcher checksum over `bytes` (class through payload).
pub fn checksum(bytes: &[u8]) -> (u8, u8) {
    let mut ck_a: u8 = 0;
    let mut ck_b: u8 = 0;
    for &b in bytes {
        ck_a = ck_a.wrapping_add(b);
        ck_b = ck_b.wrapping_add(ck_a);
    }
    (ck_a, ck_b)
}

/// Encode a complete frame into `buf`, returning the framed bytes.
/// `None` if `buf` is too small (needs `payload.len() + 8`).
pub fn encode<'a>(class: u8, id: u8, payload: &[u8], buf: &'a mut [u8]) -> Option<&'a [u8]> {
    let total = payload.len() + 8;
    if buf.len() < total {
        return None;
    }
    buf[0] = SYNC1;
    buf[1] = SYNC2;
    buf[2] = class;
    buf[3] = id;
    let len = payload.len() as u16;
    buf[4] = (len & 0xff) as u8;
    buf[5] = (len >> 8) as u8;
    buf[6..6 + payload.len()].copy_from_slice(payload);
    let (ck_a, ck_b) = checksum(&buf[2..6 + payload.len()]);
    buf[6 + payload.len()] = ck_a;
    buf[7 + payload.len()] = ck_b;
    Some(&buf[..total])
}

/// Result of feeding one byte to the [`Parser`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    /// Nothing complete yet.
    Pending,
    /// A frame with a valid checksum completed; its payload is
    /// available via [`Parser::payload`] until the next `push`.
    Frame { class: u8, id: u8 },
    /// A frame that could not be buffered was dropped. Two cases,
    /// told apart by `len`: between [`MAX_PAYLOAD`] and
    /// [`DRAIN_CAP`] the frame was real-looking and fully consumed;
    /// above [`DRAIN_CAP`] it was treated as a false sync and the
    /// parser resynchronized immediately instead of draining.
    Skipped { class: u8, id: u8, len: u16 },
    /// A frame arrived with a bad checksum and was dropped.
    BadChecksum { class: u8, id: u8 },
}

#[derive(Clone, Copy)]
enum State {
    Sync1,
    Sync2,
    Class,
    Id,
    LenLo,
    LenHi,
    Payload,
    CkA,
    CkB,
    /// Draining an oversized payload (+ its 2 checksum bytes).
    Drain,
}

/// Streaming UBX parser: feed raw interface bytes (interleaved NMEA
/// text included - anything outside a sync sequence is ignored), get
/// checksum-verified frames out. No allocation; one fixed buffer.
pub struct Parser {
    state: State,
    class: u8,
    id: u8,
    len: u16,
    got: u16,
    ck_a: u8,
    ck_b: u8,
    payload: [u8; MAX_PAYLOAD],
}

impl Parser {
    pub const fn new() -> Self {
        Self {
            state: State::Sync1,
            class: 0,
            id: 0,
            len: 0,
            got: 0,
            ck_a: 0,
            ck_b: 0,
            payload: [0; MAX_PAYLOAD],
        }
    }

    /// Payload of the frame most recently reported as
    /// [`Poll::Frame`]. Valid until the next [`Self::push`].
    pub fn payload(&self) -> &[u8] {
        &self.payload[..self.len as usize]
    }

    fn ck(&mut self, b: u8) {
        self.ck_a = self.ck_a.wrapping_add(b);
        self.ck_b = self.ck_b.wrapping_add(self.ck_a);
    }

    /// Feed one received byte.
    pub fn push(&mut self, b: u8) -> Poll {
        match self.state {
            State::Sync1 => {
                if b == SYNC1 {
                    self.state = State::Sync2;
                }
                Poll::Pending
            }
            State::Sync2 => {
                self.state = if b == SYNC2 {
                    State::Class
                } else if b == SYNC1 {
                    // `B5 B5 62 ...` - stay one byte into the sync.
                    State::Sync2
                } else {
                    State::Sync1
                };
                Poll::Pending
            }
            State::Class => {
                self.class = b;
                self.ck_a = b;
                self.ck_b = b;
                self.state = State::Id;
                Poll::Pending
            }
            State::Id => {
                self.id = b;
                self.ck(b);
                self.state = State::LenLo;
                Poll::Pending
            }
            State::LenLo => {
                self.len = b as u16;
                self.ck(b);
                self.state = State::LenHi;
                Poll::Pending
            }
            State::LenHi => {
                self.len |= (b as u16) << 8;
                self.ck(b);
                self.got = 0;
                if self.len as usize > DRAIN_CAP {
                    // False sync, in all likelihood - resync from
                    // the very next byte, do not drain (see
                    // DRAIN_CAP).
                    self.state = State::Sync1;
                    Poll::Skipped { class: self.class, id: self.id, len: self.len }
                } else if self.len as usize > MAX_PAYLOAD {
                    self.state = State::Drain;
                    Poll::Pending
                } else if self.len == 0 {
                    self.state = State::CkA;
                    Poll::Pending
                } else {
                    self.state = State::Payload;
                    Poll::Pending
                }
            }
            State::Payload => {
                self.payload[self.got as usize] = b;
                self.ck(b);
                self.got += 1;
                if self.got == self.len {
                    self.state = State::CkA;
                }
                Poll::Pending
            }
            State::CkA => {
                if b == self.ck_a {
                    self.state = State::CkB;
                    Poll::Pending
                } else {
                    // The frame's trailing ck_b byte re-enters the
                    // sync scan; a stray 0xB5 there still needs a
                    // following 0x62 to fake a frame start.
                    self.state = State::Sync1;
                    Poll::BadChecksum { class: self.class, id: self.id }
                }
            }
            State::CkB => {
                self.state = State::Sync1;
                if b == self.ck_b {
                    Poll::Frame { class: self.class, id: self.id }
                } else {
                    Poll::BadChecksum { class: self.class, id: self.id }
                }
            }
            State::Drain => {
                // Consume payload + 2 checksum bytes, then report.
                self.got += 1;
                if self.got == self.len + 2 {
                    self.state = State::Sync1;
                    Poll::Skipped {
                        class: self.class,
                        id: self.id,
                        len: self.len,
                    }
                } else {
                    Poll::Pending
                }
            }
        }
    }
}
