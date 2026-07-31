//! BHI260AP FIFO stream parsing (datasheet Sections 14/15).
//!
//! Both sensor FIFOs (wake-up and non-wake-up) carry the same wire
//! format: a stream of events, each a 1-byte FIFO Event ID followed by
//! a payload whose size is fixed per ID (Table 88). The stream a host
//! transfer delivers is
//! `[small delta ts (2)] [block: meta(4) + full ts(6) + events..]..`
//! with 0xFF filler between blocks and 0x00 padding at the end - all
//! of which are themselves just events with known sizes, so one
//! sequential walk handles everything.
//!
//! The parser tracks the running 40-bit timestamp (1/64000 s units,
//! wraps after ~198 days) exactly as Section 15.2 prescribes: a full
//! timestamp replaces it, small/large deltas advance it, and it
//! applies to every following event until the next timestamp event.

/// FIFO Event IDs (Table 88). Where a sensor exists in both FIFOs the
/// non-wake-up and wake-up variants have distinct IDs; `_WU` marks the
/// wake-up one.
pub mod event_id {
    // Quaternion+ format (11 bytes)
    pub const ROTATION_VECTOR: u8 = 34;
    pub const ROTATION_VECTOR_WU: u8 = 35;
    pub const GAME_ROTATION_VECTOR: u8 = 37;
    pub const GAME_ROTATION_VECTOR_WU: u8 = 38;
    pub const GEO_ROTATION_VECTOR: u8 = 40;
    pub const GEO_ROTATION_VECTOR_WU: u8 = 41;
    // Euler format (7 bytes)
    pub const ORIENTATION: u8 = 43;
    pub const ORIENTATION_WU: u8 = 44;
    // 3D vector format (7 bytes)
    pub const ACCEL_PASSTHROUGH: u8 = 1;
    pub const ACCEL_RAW: u8 = 3;
    pub const ACCEL_CORRECTED: u8 = 4;
    pub const ACCEL_OFFSET: u8 = 5;
    pub const ACCEL_CORRECTED_WU: u8 = 6;
    pub const ACCEL_RAW_WU: u8 = 7;
    pub const GYRO_PASSTHROUGH: u8 = 10;
    pub const GYRO_RAW: u8 = 12;
    pub const GYRO_CORRECTED: u8 = 13;
    pub const GYRO_OFFSET: u8 = 14;
    pub const GYRO_CORRECTED_WU: u8 = 15;
    pub const GYRO_RAW_WU: u8 = 16;
    pub const MAG_PASSTHROUGH: u8 = 19;
    pub const MAG_RAW: u8 = 21;
    pub const MAG_CORRECTED: u8 = 22;
    pub const MAG_OFFSET: u8 = 23;
    pub const MAG_CORRECTED_WU: u8 = 24;
    pub const MAG_RAW_WU: u8 = 25;
    pub const GRAVITY: u8 = 28;
    pub const GRAVITY_WU: u8 = 29;
    pub const LINEAR_ACCEL: u8 = 31;
    pub const LINEAR_ACCEL_WU: u8 = 32;
    pub const ACCEL_OFFSET_WU: u8 = 91;
    pub const GYRO_OFFSET_WU: u8 = 92;
    pub const MAG_OFFSET_WU: u8 = 93;
    // Scalar formats
    pub const LIGHT: u8 = 146; // u16, 10000 lux / 216
    pub const LIGHT_WU: u8 = 148;
    pub const PROXIMITY: u8 = 147; // u8, 0 far / 1 near
    pub const PROXIMITY_WU: u8 = 149;
    pub const HUMIDITY: u8 = 130; // u8, 1 %RH
    pub const HUMIDITY_WU: u8 = 134;
    pub const STEP_COUNTER: u8 = 52; // u32, 1 step
    pub const STEP_COUNTER_WU: u8 = 53;
    pub const AUX_STEP_COUNTER: u8 = 136;
    pub const AUX_STEP_COUNTER_WU: u8 = 139;
    pub const TEMPERATURE: u8 = 128; // i16, degC / 100
    pub const TEMPERATURE_WU: u8 = 132;
    pub const BAROMETER: u8 = 129; // u24, 1/128 Pa
    pub const BAROMETER_WU: u8 = 133;
    pub const GAS: u8 = 131; // u32, Ohms
    pub const GAS_WU: u8 = 135;
    // Payload-less events (1 byte)
    pub const TILT_DETECTOR_WU: u8 = 48;
    pub const STEP_DETECTOR: u8 = 50;
    pub const SIGNIFICANT_MOTION_WU: u8 = 55;
    pub const WAKE_GESTURE_WU: u8 = 57;
    pub const GLANCE_GESTURE_WU: u8 = 59;
    pub const PICKUP_GESTURE_WU: u8 = 61;
    pub const WRIST_TILT_GESTURE_WU: u8 = 67;
    pub const STATIONARY_DETECT_WU: u8 = 75;
    pub const MOTION_DETECT_WU: u8 = 77;
    pub const STEP_DETECTOR_WU: u8 = 94;
    pub const AUX_STEP_DETECTOR: u8 = 137;
    pub const AUX_SIGNIFICANT_MOTION: u8 = 138;
    pub const AUX_STEP_DETECTOR_WU: u8 = 140;
    pub const AUX_SIGNIFICANT_MOTION_WU: u8 = 141;
    pub const AUX_ANY_MOTION: u8 = 142;
    pub const AUX_ANY_MOTION_WU: u8 = 143;
    // Structured formats
    pub const ACTIVITY: u8 = 63; // u16 activity-change bitmap
    pub const DEVICE_ORIENTATION: u8 = 69; // u8 portrait/landscape
    pub const DEVICE_ORIENTATION_WU: u8 = 70;
    pub const CAMERA_SHUTTER: u8 = 144;
    pub const GPS: u8 = 145;
    pub const SELF_LEARNING_AI: u8 = 112;
    pub const PDR_WU: u8 = 113;
    pub const SWIM: u8 = 114;
    // Stream structure events
    pub const DEBUG_DATA: u8 = 250;
    pub const TS_SMALL_DELTA: u8 = 251; // u8 delta
    pub const TS_SMALL_DELTA_WU: u8 = 245;
    pub const TS_LARGE_DELTA: u8 = 252; // u16 delta
    pub const TS_LARGE_DELTA_WU: u8 = 246;
    pub const TS_FULL: u8 = 253; // u40 absolute
    pub const TS_FULL_WU: u8 = 247;
    pub const META_EVENT: u8 = 254;
    pub const META_EVENT_WU: u8 = 248;
    pub const FILLER: u8 = 255;
    pub const PADDING: u8 = 0;
}

/// Meta event types (byte 1 of a meta event, Table 99).
pub mod meta {
    pub const FLUSH_COMPLETE: u8 = 1;
    pub const SAMPLE_RATE_CHANGED: u8 = 2;
    pub const POWER_MODE_CHANGED: u8 = 3;
    pub const SYSTEM_ERROR: u8 = 4; // status FIFO
    pub const ALGORITHM_EVENTS: u8 = 5;
    pub const SENSOR_STATUS: u8 = 6;
    pub const SENSOR_ERROR: u8 = 11; // status FIFO
    pub const FIFO_OVERFLOW: u8 = 12;
    pub const DYNAMIC_RANGE_CHANGED: u8 = 13;
    pub const FIFO_WATERMARK: u8 = 14;
    pub const INITIALIZED: u8 = 16; // bytes 2-3 = RAM version
    pub const TRANSFER_CAUSE: u8 = 17;
    pub const SW_FRAMEWORK: u8 = 18;
    pub const RESET: u8 = 19; // byte 3 = reset cause (Table 101)
    pub const SPACER: u8 = 20;
}

/// Total size in the FIFO (ID byte included) for an event ID, per the
/// "Bytes in FIFO" column of Table 88. `None` = unknown ID: the host
/// has lost sync and must abort/resync (Section 16.4) - guessing a
/// size would corrupt every event after it.
pub fn event_size(id: u8) -> Option<usize> {
    use event_id as e;
    Some(match id {
        e::PADDING | e::FILLER => 1,
        // Quaternion+
        e::ROTATION_VECTOR | e::ROTATION_VECTOR_WU | e::GAME_ROTATION_VECTOR
        | e::GAME_ROTATION_VECTOR_WU | e::GEO_ROTATION_VECTOR | e::GEO_ROTATION_VECTOR_WU => 11,
        // Euler
        e::ORIENTATION | e::ORIENTATION_WU => 7,
        // 3D vectors
        e::ACCEL_PASSTHROUGH | e::ACCEL_RAW | e::ACCEL_CORRECTED | e::ACCEL_OFFSET
        | e::ACCEL_CORRECTED_WU | e::ACCEL_RAW_WU | e::GYRO_PASSTHROUGH | e::GYRO_RAW
        | e::GYRO_CORRECTED | e::GYRO_OFFSET | e::GYRO_CORRECTED_WU | e::GYRO_RAW_WU
        | e::MAG_PASSTHROUGH | e::MAG_RAW | e::MAG_CORRECTED | e::MAG_OFFSET
        | e::MAG_CORRECTED_WU | e::MAG_RAW_WU | e::GRAVITY | e::GRAVITY_WU
        | e::LINEAR_ACCEL | e::LINEAR_ACCEL_WU | e::ACCEL_OFFSET_WU | e::GYRO_OFFSET_WU
        | e::MAG_OFFSET_WU => 7,
        // Scalars
        e::LIGHT | e::LIGHT_WU | e::TEMPERATURE | e::TEMPERATURE_WU => 3,
        e::PROXIMITY | e::PROXIMITY_WU | e::HUMIDITY | e::HUMIDITY_WU => 2,
        e::STEP_COUNTER | e::STEP_COUNTER_WU | e::AUX_STEP_COUNTER | e::AUX_STEP_COUNTER_WU
        | e::GAS | e::GAS_WU => 5,
        e::BAROMETER | e::BAROMETER_WU => 4,
        // Payload-less
        e::TILT_DETECTOR_WU | e::STEP_DETECTOR | e::SIGNIFICANT_MOTION_WU
        | e::WAKE_GESTURE_WU | e::GLANCE_GESTURE_WU | e::PICKUP_GESTURE_WU
        | e::WRIST_TILT_GESTURE_WU | e::STATIONARY_DETECT_WU | e::MOTION_DETECT_WU
        | e::STEP_DETECTOR_WU | e::AUX_STEP_DETECTOR | e::AUX_SIGNIFICANT_MOTION
        | e::AUX_STEP_DETECTOR_WU | e::AUX_SIGNIFICANT_MOTION_WU | e::AUX_ANY_MOTION
        | e::AUX_ANY_MOTION_WU => 1,
        // Structured
        e::ACTIVITY => 3,
        e::DEVICE_ORIENTATION | e::DEVICE_ORIENTATION_WU | e::CAMERA_SHUTTER => 2,
        e::GPS => 27,
        e::SELF_LEARNING_AI => 11,
        e::PDR_WU => 16,
        e::SWIM => 15,
        // Stream structure
        e::DEBUG_DATA => 18,
        e::TS_SMALL_DELTA | e::TS_SMALL_DELTA_WU => 2,
        e::TS_LARGE_DELTA | e::TS_LARGE_DELTA_WU => 3,
        e::TS_FULL | e::TS_FULL_WU => 6,
        e::META_EVENT | e::META_EVENT_WU => 4,
        _ => return None,
    })
}

/// One parsed FIFO event. Payload interpretation for the formats the
/// system consumes; everything else is surfaced raw so callers can
/// still handle any sensor the firmware offers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event<'a> {
    /// 3D vector sample (accel/gyro/mag family), raw i16 axes. Scale
    /// to SI: value x dynamic_range / 32768 (dynamic scaling, Table 88
    /// note 3).
    Vector3 { id: u8, x: i16, y: i16, z: i16 },
    /// Quaternion+ sample, components scaled by 2^-14; accuracy in
    /// radians scaled by 2^-14 (0 for Game Rotation).
    Quaternion { id: u8, x: i16, y: i16, z: i16, w: i16, accuracy: u16 },
    /// Euler orientation, each scaled by 360 deg / 2^15.
    Euler { id: u8, heading: i16, pitch: i16, roll: i16 },
    /// Step counter total (u32 wire value).
    StepCount { id: u8, steps: u32 },
    /// Activity-change bitmap (Table 93).
    Activity { id: u8, bitmap: u16 },
    /// A payload-less occurrence event (wake gesture, any motion,
    /// significant motion, step detector, ...).
    Occurrence { id: u8 },
    /// Meta event with its two payload bytes (Table 99: byte1 = type).
    Meta { id: u8, kind: u8, b2: u8, b3: u8 },
    /// Any other known-size event, raw payload (scalars, GPS, PDR,
    /// swim, self-learning AI, debug data, device orientation, ...).
    Raw { id: u8, payload: &'a [u8] },
}

/// Sequential FIFO stream parser. Feed it the bytes of one host
/// transfer (after the 2-byte transfer length); iterate events.
pub struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Running 40-bit timestamp in 1/64000 s ticks, valid for every
    /// event returned until the next timestamp event updates it.
    pub timestamp_ticks: u64,
    /// Set when an unknown event ID was hit - stream is out of sync
    /// beyond this point and parsing stopped (Section 16.4).
    pub lost_sync: Option<u8>,
}

impl<'a> Parser<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0, timestamp_ticks: 0, lost_sync: None }
    }

    /// Next parsed event, or `None` at end of stream / padding / loss
    /// of sync. Timestamp and filler/padding events are consumed
    /// internally (timestamps update [`Self::timestamp_ticks`]).
    pub fn next_event(&mut self) -> Option<Event<'a>> {
        use event_id as e;
        loop {
            let id = *self.buf.get(self.pos)?;
            // Padding only occurs at the end of the packed data - stop
            // parsing entirely (Table 88 note 5).
            if id == e::PADDING {
                return None;
            }
            let size = match event_size(id) {
                Some(s) => s,
                None => {
                    self.lost_sync = Some(id);
                    return None;
                }
            };
            if self.pos + size > self.buf.len() {
                // Truncated tail - the remainder arrives in the next
                // transfer; nothing more to parse here.
                return None;
            }
            let p = &self.buf[self.pos + 1..self.pos + size];
            self.pos += size;
            match id {
                e::FILLER => continue,
                e::TS_SMALL_DELTA | e::TS_SMALL_DELTA_WU => {
                    self.timestamp_ticks = self.timestamp_ticks.wrapping_add(p[0] as u64);
                    continue;
                }
                e::TS_LARGE_DELTA | e::TS_LARGE_DELTA_WU => {
                    let d = u16::from_le_bytes([p[0], p[1]]) as u64;
                    self.timestamp_ticks = self.timestamp_ticks.wrapping_add(d);
                    continue;
                }
                e::TS_FULL | e::TS_FULL_WU => {
                    self.timestamp_ticks = u64::from_le_bytes([
                        p[0], p[1], p[2], p[3], p[4], 0, 0, 0,
                    ]);
                    continue;
                }
                _ => {}
            }
            return Some(match (id, size) {
                (e::META_EVENT | e::META_EVENT_WU, _) => {
                    Event::Meta { id, kind: p[0], b2: p[1], b3: p[2] }
                }
                (_, 7) if id != e::ORIENTATION && id != e::ORIENTATION_WU => Event::Vector3 {
                    id,
                    x: i16::from_le_bytes([p[0], p[1]]),
                    y: i16::from_le_bytes([p[2], p[3]]),
                    z: i16::from_le_bytes([p[4], p[5]]),
                },
                (e::ORIENTATION | e::ORIENTATION_WU, _) => Event::Euler {
                    id,
                    heading: i16::from_le_bytes([p[0], p[1]]),
                    pitch: i16::from_le_bytes([p[2], p[3]]),
                    roll: i16::from_le_bytes([p[4], p[5]]),
                },
                (_, 11) if id != e::SELF_LEARNING_AI => Event::Quaternion {
                    id,
                    x: i16::from_le_bytes([p[0], p[1]]),
                    y: i16::from_le_bytes([p[2], p[3]]),
                    z: i16::from_le_bytes([p[4], p[5]]),
                    w: i16::from_le_bytes([p[6], p[7]]),
                    accuracy: u16::from_le_bytes([p[8], p[9]]),
                },
                (
                    e::STEP_COUNTER | e::STEP_COUNTER_WU | e::AUX_STEP_COUNTER
                    | e::AUX_STEP_COUNTER_WU,
                    _,
                ) => Event::StepCount {
                    id,
                    steps: u32::from_le_bytes([p[0], p[1], p[2], p[3]]),
                },
                (e::ACTIVITY, _) => Event::Activity {
                    id,
                    bitmap: u16::from_le_bytes([p[0], p[1]]),
                },
                (_, 1) => Event::Occurrence { id },
                _ => Event::Raw { id, payload: p },
            });
        }
    }
}
