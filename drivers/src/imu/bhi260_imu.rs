//! BHI260AP adapter for the [`AnyImu`](super::AnyImu) task contract.
//!
//! Bridges the hub's virtual-sensor world onto the IMU task's simple
//! needs: continuous accel+gyro snapshots while awake, a wake-up
//! motion sensor while the system sleeps, an always-on step counter,
//! and the two boot self-tests. Chip specifics stay here; the task
//! sees only the seam.
//!
//! Bring-up is a state machine driven by [`Bhi260Imu::boot_step`] -
//! one short bus transaction per call - because the mandatory
//! firmware upload (~104 KB over I2C, roughly 3 s at 400 kHz) must
//! neither block boot nor monopolize the shared bus. The sequence is
//! the datasheet's Host Boot Mode procedure (Section 5.2.1): reset,
//! wait for the bootloader, upload + verify, boot, wait for the
//! firmware, drain the Initialized meta events, then discover and
//! enable sensors.
//!
//! Samples are rescaled from the hub's current dynamic range to the
//! QMI8658 raw scale the shared task normalizes on (accel +/-8 g,
//! gyro +/-256 dps full scale) - see `read`. The temperature field
//! is 0: the base firmware exposes no temperature virtual sensor
//! (that needs an aux BME280/BME680 attached to the hub).

use embedded_hal::i2c::I2c as I2cTrait;

use super::{
    BootStep, SelfTestAxes, SelfTestKind, SelfTestPoll, WearCalibration, WearClassifier,
    WristIntent,
};
use crate::bhi260::{self, fifo, flush, Bhi260, Error};
use crate::imu::ImuData;

/// The continuous streaming pair: the PASSTHROUGH outputs (direct
/// physical sensor data), matching the vendor's own watch examples
/// (LilyGoLib BHI260AP_6DoF: ACCEL_PASSTHROUGH/GYRO_PASSTHROUGH).
/// Deliberately NOT the BSX "corrected" outputs: those route
/// through the fusion library, which on this firmware delivers at
/// rate/8 from boot until the first host-suspend cycle
/// (hardware-measured 2026-07-31, layer diagnostic: virt/phys
/// bookkeeping healthy in both states while actual delivery
/// differed 8x) - the same sick layer that starves the gesture
/// engines while awake. Passthrough bypasses BSX entirely.
const STREAM_ACCEL: u8 = fifo::event_id::ACCEL_PASSTHROUGH;
const STREAM_GYRO: u8 = fifo::event_id::GYRO_PASSTHROUGH;

/// Normalization targets: the QMI8658 full-scale ranges the shared
/// task's consumers are calibrated against.
const NORM_ACCEL_RANGE_G: i32 = 8;
const NORM_GYRO_RANGE_DPS: i32 = 256;

/// Continuous sample rate requested while awake. The task polls at
/// 20 Hz; 25 Hz is the framework's nearest supported rate at or
/// above that, so each poll drains 1-2 fresh samples per sensor.
const AWAKE_RATE_HZ: f32 = 25.0;

/// Rate requested for the wake-up motion sensor while sleeping
/// (gesture/motion sensors are on-change or one-shot - the rate
/// mainly gates their underlying accel).
const WOM_RATE_HZ: f32 = 25.0;

/// Step-counter candidates in preference order: the BSX step counter
/// (52), else the auxiliary variant (136). Non-wake-up on purpose:
/// the total accumulates hub-side straight through host sleep
/// without ever asserting the wake line - the host reads the newest
/// total from the non-wake FIFO whenever it is next awake. The
/// sensor is on-change, so the rate only gates its underlying accel.
const STEP_PREFERRED: [u8; 2] = [
    fifo::event_id::STEP_COUNTER,
    fifo::event_id::AUX_STEP_COUNTER,
];
const STEP_RATE_HZ: f32 = 25.0;

/// Requested dynamic ranges (SI units). Accel matches the
/// normalization target exactly; 250 dps is the closest supported
/// gyro range to the 256 dps target (the residual 250/256 factor is
/// handled by the rescale math).
const REQ_ACCEL_RANGE_G: u16 = 8;
const REQ_GYRO_RANGE_DPS: u16 = 250;

/// Wake-up virtual sensors armed TOGETHER for wake-on-motion - every
/// one of these the loaded firmware offers gets enabled (any wake
/// FIFO event wakes the system, so they compose): Wake Gesture is
/// the wrist-raise (the watch UX), Motion Detect the sustained-
/// motion catch-all (its Android contract needs ~5 s of continuous
/// motion), Any Motion the fast slope detector (absent from the
/// stock image, armed automatically on firmwares that carry it).
const WOM_PREFERRED: [u8; 6] = [
    fifo::event_id::WAKE_GESTURE_WU,
    // The wearable lift-to-wake suite - snappier raise semantics
    // than Wake Gesture's raise-and-hold contract, where the
    // firmware offers them.
    fifo::event_id::WRIST_TILT_GESTURE_WU,
    fifo::event_id::PICKUP_GESTURE_WU,
    fifo::event_id::GLANCE_GESTURE_WU,
    fifo::event_id::MOTION_DETECT_WU,
    fifo::event_id::AUX_ANY_MOTION_WU,
];

/// Last-resort wake source if none of the preferred set exists
/// (walking-scale algorithm - strict, but better than no motion
/// wake).
const WOM_FALLBACK: u8 = fifo::event_id::SIGNIFICANT_MOTION_WU;

/// Retry budgets for the boot state machine's wait states.
const BOOTLOADER_TRIES: u8 = 50; // x1 ms; spec max is 1.3 ms
const VERIFY_TRIES: u8 = 100; // x5 ms; verify overlaps the upload
const FW_BOOT_TRIES: u8 = 50; // x10 ms; typ 81 ms
/// Discovery retries. The first parameter read lands right after the
/// Initialized meta events, while the framework is still bringing up
/// BSX - responses can lag for a while, so keep asking (~1 s total
/// on top of the per-attempt status wait) before giving up.
const DISCOVER_TRIES: u8 = 20; // x50 ms between attempts

/// Range read-back retries after the enable burst (same late-answer
/// behavior as discovery).
const RANGE_TRIES: u8 = 20; // x50 ms between attempts

/// Channel-flow codes the firmware latches for benign host-side
/// conditions - reading an empty channel and friends, "Temporary"
/// class in the datasheet's error table (0x75 underflow, 0x76
/// overflow, 0x77 empty). Never failures; every error-register
/// check filters them.
fn is_transient(code: u8) -> bool {
    matches!(code, 0x75 | 0x76 | 0x77)
}

/// One-shot sensors self-DISABLE after firing (datasheet Table 88
/// reporting mode) and must be re-armed to fire again - treating
/// them as continuous silently loses every gesture after the first
/// (hardware-observed).
fn is_one_shot(id: u8) -> bool {
    matches!(
        id,
        fifo::event_id::SIGNIFICANT_MOTION_WU
            | fifo::event_id::WAKE_GESTURE_WU
            | fifo::event_id::GLANCE_GESTURE_WU
            | fifo::event_id::PICKUP_GESTURE_WU
    )
}

/// Self-test progression. The request must not be issued until the
/// PHYSICAL sensor has actually powered down - virtual-sensor
/// disables propagate asynchronously (the gyro's drive takes tens of
/// milliseconds to wind down), and a request against a still-active
/// sensor is rejected without a status packet, i.e. it looks like an
/// eternal timeout.
enum StState {
    /// Sensors disabled; waiting for the physical sensor to report
    /// Power Down / Suspend in its info parameter.
    Quiesce { kind: SelfTestKind, polls: u8 },
    /// Request issued; waiting for the results status packet that
    /// echoes this physical sensor id (the echo check guards against
    /// consuming a stale packet from an earlier aborted test - the
    /// reference host library validates the same way).
    Waiting { phys: u8 },
}

/// Quiesce budget: polls (paced by the caller, ~10 ms apart) allowed
/// before declaring the physical sensor never powered down.
const QUIESCE_POLLS: u8 = 200;

enum Phase {
    Reset,
    WaitBootloader { tries: u8 },
    StartUpload,
    Upload { offset: usize },
    WaitVerify { tries: u8 },
    Boot,
    WaitFirmware { tries: u8 },
    DrainInit,
    Discover { tries: u8 },
    Enable,
    /// Re-arbitration cycle: disable-then-re-enable the continuous
    /// sensors once after the first enable. NOTE 2026-07-31: the
    /// earlier "confirmed fix" claim here was wrong - its
    /// verification logs were post-sleep, and the rate-collapse
    /// mystery this phase was built for turned out to be the
    /// unserviced-wake-transfer throttle that [`Phase::PrimeWake`]
    /// now clears. The cycle is kept as harmless config hygiene
    /// pending a with-vs-without measurement.
    Recycle { enabled: bool },
    ReadRanges { tries: u8 },
    /// Final bring-up step: one simulated sleep cycle, because from
    /// boot the chip is in a crippled state (stream at rate/8,
    /// gesture engines fully dead) that ONLY a real sleep entry has
    /// ever healed - deterministically, every observation
    /// 2026-07-31. The essential ingredient the earlier in-`read`
    /// replica missed: the host must go SILENT during the WoM gap
    /// (a real sleep polls nothing); inside a bring-up Pending
    /// delay the task provably makes no chip contact, so this
    /// phase is enter_wom -> 3 s of true silence -> exit_wom.
    BootWom { entered: bool },
    Ready,
    Failed(&'static str),
}

pub struct Bhi260Imu {
    drv: Bhi260,
    phase: Phase,
    /// Wake-up motion sensors picked at discovery (all present
    /// members of [`WOM_PREFERRED`], else [`WOM_FALLBACK`]); empty
    /// if the firmware offers none (sleep then has no motion wake -
    /// GPIO wake sources still apply).
    wake_sensors: [u8; 8],
    wake_count: usize,
    /// Step-counter virtual sensor picked at discovery (first present
    /// member of [`STEP_PREFERRED`]); `None` if the firmware offers
    /// none - `ImuData::steps` then stays `None`.
    step_sensor: Option<u8>,
    /// Wake attribution deferred out of the USB-JTAG mangle zone:
    /// the sensor id drained at wake, announced by the first awake
    /// `read` (whose log line survives, unlike lines printed at the
    /// wake edge).
    announce_wake: Option<u8>,
    /// Current dynamic ranges (SI units) read back from the Virtual
    /// Sensor Configuration parameters - the scale reference for
    /// incoming raw samples.
    accel_range_si: i32,
    gyro_range_si: i32,
    /// Latest normalized sample, repeated when a poll finds no fresh
    /// FIFO data.
    last: ImuData,
    /// In-flight self-test state, if any (see `self_test_start`).
    st: Option<StState>,
    /// Viewing-pose classifier fed by every parsed accel sample;
    /// its settled transitions surface as [`WristIntent`]s via
    /// `take_intent`.
    wear: WearClassifier,
    /// Board-provided mounting remap (packed per Table 70; see
    /// `Bhi260::pack_orientation_matrix`), applied to the physical
    /// accel and gyro during bring-up. The gesture algorithms - wake
    /// gesture above all - evaluate motion in the device frame and
    /// are blind when this is wrong.
    orientation: [u8; 5],
    /// FIFO watchdog: consecutive empty awake polls. At 20 Hz
    /// polling, ~200 in a row (~10 s) with sensors nominally enabled
    /// means an enable was silently dropped (e.g. rejected while a
    /// self-test still ran) - re-arm.
    empty_streak: u16,
}

impl Bhi260Imu {
    /// `orientation` is the board's mounting remap, packed via
    /// [`Bhi260::pack_orientation_matrix`] (identity for a chip
    /// mounted in the datasheet's default frame). `wear` is the
    /// bin's wear calibration for the viewing-pose classifier.
    pub fn new(orientation: [u8; 5], wear: WearCalibration) -> Self {
        Self {
            wear: WearClassifier::new(wear),
            drv: Bhi260::new(),
            phase: Phase::Reset,
            orientation,
            accel_range_si: NORM_ACCEL_RANGE_G,
            gyro_range_si: 2000, // chip default until read back
            wake_sensors: [0; 8],
            wake_count: 0,
            step_sensor: None,
            announce_wake: None,
            last: ImuData::default(),
            st: None,
            empty_streak: 0,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.phase, Phase::Ready)
    }

    // ---- Bring-up ------------------------------------------------

    pub fn boot_step<I: I2cTrait>(&mut self, i2c: &mut I) -> BootStep {
        match self.phase {
            Phase::Reset => match self.drv.host_reset(i2c) {
                Ok(()) => {
                    self.phase = Phase::WaitBootloader { tries: 0 };
                    BootStep::Pending { delay_ms: 2 }
                }
                Err(_) => self.fail("no response on I2C (reset)"),
            },
            Phase::WaitBootloader { tries } => match self.drv.host_interface_ready(i2c) {
                Ok(true) => {
                    self.phase = Phase::StartUpload;
                    BootStep::Pending { delay_ms: 1 }
                }
                Ok(false) if tries < BOOTLOADER_TRIES => {
                    self.phase = Phase::WaitBootloader { tries: tries + 1 };
                    BootStep::Pending { delay_ms: 1 }
                }
                _ => self.fail("bootloader never became ready"),
            },
            Phase::StartUpload => {
                match self.drv.probe(i2c) {
                    Ok(info) => log::info!(
                        "IMU: BHI260AP bootloader ready (product 0x{:02X} rev 0x{:02X} ROM 0x{:04X}); uploading firmware ({} B)",
                        info.product_id,
                        info.revision,
                        info.rom_version,
                        bhi260::FIRMWARE.len(),
                    ),
                    Err(_) => return self.fail("identity read failed"),
                }
                // Interrupt pin config: reset default (active high,
                // level, push-pull, all sources) matches the board
                // wiring; written explicitly per the boot procedure.
                if self.drv.configure_host_interrupt(i2c, 0).is_err() {
                    return self.fail("interrupt config failed");
                }
                match self.drv.begin_ram_upload(i2c, bhi260::FIRMWARE.len()) {
                    Ok(()) => {
                        self.phase = Phase::Upload { offset: 0 };
                        BootStep::Pending { delay_ms: 1 }
                    }
                    Err(_) => self.fail("upload start failed"),
                }
            }
            Phase::Upload { offset } => {
                let end = (offset + bhi260::UPLOAD_CHUNK).min(bhi260::FIRMWARE.len());
                if let Err(e) = self.drv.upload_chunk(i2c, &bhi260::FIRMWARE[offset..end]) {
                    // The concrete error names the failure mechanism
                    // (timeout vs NACK vs bus error) - boot-time
                    // transients appeared 2026-08-08 and the bare
                    // "chunk failed" left the diagnosis to guesswork.
                    log::error!(
                        "IMU: upload chunk at {}..{} failed: {:?}",
                        offset, end, e,
                    );
                    return self.fail("upload chunk failed");
                }
                if end == bhi260::FIRMWARE.len() {
                    self.phase = Phase::WaitVerify { tries: 0 };
                } else {
                    self.phase = Phase::Upload { offset: end };
                }
                BootStep::Pending { delay_ms: 1 }
            }
            Phase::WaitVerify { tries } => match self.drv.firmware_verify_done(i2c) {
                Ok(true) => {
                    self.phase = Phase::Boot;
                    BootStep::Pending { delay_ms: 1 }
                }
                Ok(false) if tries < VERIFY_TRIES => {
                    self.phase = Phase::WaitVerify { tries: tries + 1 };
                    BootStep::Pending { delay_ms: 5 }
                }
                Ok(false) => self.fail("firmware verify timed out"),
                Err(e) => {
                    if let Error::Chip(code) = e {
                        log::error!(
                            "IMU: firmware verify failed: {}",
                            bhi260::error_description(code),
                        );
                    }
                    self.fail("firmware verify failed")
                }
            },
            Phase::Boot => match self.drv.boot_program_ram(i2c) {
                Ok(()) => {
                    self.phase = Phase::WaitFirmware { tries: 0 };
                    BootStep::Pending { delay_ms: 20 }
                }
                Err(_) => self.fail("boot command failed"),
            },
            Phase::WaitFirmware { tries } => match self.drv.host_interface_ready(i2c) {
                Ok(true) => {
                    self.phase = Phase::DrainInit;
                    BootStep::Pending { delay_ms: 1 }
                }
                Ok(false) if tries < FW_BOOT_TRIES => {
                    self.phase = Phase::WaitFirmware { tries: tries + 1 };
                    BootStep::Pending { delay_ms: 10 }
                }
                _ => self.fail("firmware never came up"),
            },
            Phase::DrainInit => {
                // Both FIFOs hold an Initialized meta event that must
                // be read out before any configuration (Section
                // 15.3.11); this also releases the first interrupt.
                let mut buf = [0u8; 64];
                // The event arrives once PER FIFO; log it once (the
                // duplicate line read as a double firmware upload).
                let mut announced = false;
                for ch in [bhi260::reg::CHANNEL_WAKE_FIFO, bhi260::reg::CHANNEL_NONWAKE_FIFO] {
                    if let Ok(n) = self.drv.read_fifo(i2c, ch, &mut buf) {
                        let mut p = fifo::Parser::new(&buf[..n]);
                        while let Some(ev) = p.next_event() {
                            if let fifo::Event::Meta { kind, b2, b3, .. } = ev {
                                if kind == fifo::meta::INITIALIZED && !announced {
                                    announced = true;
                                    log::info!(
                                        "IMU: BHI260AP firmware initialized (RAM ver {})",
                                        u16::from_le_bytes([b2, b3]),
                                    );
                                }
                            }
                        }
                    }
                }
                match self.drv.error_value(i2c) {
                    Ok(code) if code == 0 || is_transient(code) => {
                        if code != 0 {
                            let _ = self.drv.clear_error_regs(i2c);
                        }
                        self.phase = Phase::Discover { tries: 0 };
                        // Grace before the first parameter read - the
                        // framework is still initializing BSX at this
                        // point and answers commands late.
                        BootStep::Pending { delay_ms: 50 }
                    }
                    Ok(code) => {
                        log::error!(
                            "IMU: BHI260AP error after boot: {}",
                            bhi260::error_description(code),
                        );
                        self.fail("firmware error after boot")
                    }
                    Err(_) => self.fail("error register read failed"),
                }
            }
            Phase::Discover { tries } => {
                let map = match self.drv.virt_sensors_present(i2c) {
                    Ok(m) => m,
                    Err(e) => {
                        if tries < DISCOVER_TRIES {
                            self.phase = Phase::Discover { tries: tries + 1 };
                            return BootStep::Pending { delay_ms: 50 };
                        }
                        log_chip_error("sensor discovery", &e);
                        self.log_diagnostics(i2c);
                        return self.fail("sensor discovery failed");
                    }
                };
                if !Bhi260::sensor_present(&map, STREAM_ACCEL)
                    || !Bhi260::sensor_present(&map, STREAM_GYRO)
                {
                    return self.fail("firmware lacks accel/gyro sensors");
                }
                // Arm every preferred wake sensor the firmware
                // offers - they compose (any wake FIFO event wakes).
                self.wake_count = 0;
                for &id in WOM_PREFERRED.iter() {
                    if Bhi260::sensor_present(&map, id) {
                        self.wake_sensors[self.wake_count] = id;
                        self.wake_count += 1;
                    }
                }
                if self.wake_count == 0 && Bhi260::sensor_present(&map, WOM_FALLBACK) {
                    self.wake_sensors[0] = WOM_FALLBACK;
                    self.wake_count = 1;
                }
                if self.wake_count == 0 {
                    log::warn!(
                        "IMU: firmware offers no wake-up motion sensor - no motion wake",
                    );
                } else {
                    log::info!(
                        "IMU: wake-on-motion sources: {:?}",
                        &self.wake_sensors[..self.wake_count],
                    );
                }
                self.step_sensor = STEP_PREFERRED
                    .into_iter()
                    .find(|&id| Bhi260::sensor_present(&map, id));
                match self.step_sensor {
                    Some(id) => log::info!("IMU: step counter available (sensor {})", id),
                    None => log::warn!("IMU: firmware offers no step counter"),
                }
                self.phase = Phase::Enable;
                BootStep::Pending { delay_ms: 1 }
            }
            Phase::Enable => {
                // Board mounting remap first - everything downstream
                // (samples AND the wake-gesture algorithms) consumes
                // the device frame this establishes. Then the
                // normalization-friendly dynamic ranges, then enable
                // continuous accel+gyro.
                let e = self
                    .drv
                    .set_orientation_matrix(i2c, bhi260::phys::ACCEL, self.orientation)
                    .and_then(|_| {
                        self.drv
                            .set_orientation_matrix(i2c, bhi260::phys::GYRO, self.orientation)
                    })
                    // No meta events in the wake-up FIFO: during
                    // sleep the interrupt line doubles as the host
                    // wake source, and only REAL wake-sensor events
                    // may assert it - enable/rate-change chatter
                    // must not.
                    .and_then(|_| self.drv.set_meta_event_control(i2c, true, [0u8; 8]))
                    .and_then(|_| {
                        self.drv.change_dynamic_range(
                            i2c,
                            STREAM_ACCEL,
                            REQ_ACCEL_RANGE_G,
                        )
                    })
                    .and_then(|_| {
                        self.drv.change_dynamic_range(
                            i2c,
                            STREAM_GYRO,
                            REQ_GYRO_RANGE_DPS,
                        )
                    })
                    .and_then(|_| {
                        self.drv.configure_sensor(
                            i2c,
                            STREAM_ACCEL,
                            AWAKE_RATE_HZ,
                            0,
                        )
                    })
                    .and_then(|_| {
                        self.drv.configure_sensor(
                            i2c,
                            STREAM_GYRO,
                            AWAKE_RATE_HZ,
                            0,
                        )
                    });
                if e.is_err() {
                    return self.fail("sensor enable failed");
                }
                // The gesture/wake sensors run CONTINUOUSLY, awake
                // and asleep: asleep they are the wake source,
                // awake their events count as user activity (dim
                // recovery). Non-fatal if one fails - the FIFO
                // watchdog and sleep entry re-arm them.
                for i in 0..self.wake_count {
                    let _ = self
                        .drv
                        .configure_sensor(i2c, self.wake_sensors[i], WOM_RATE_HZ, 0);
                }
                // Step counter joins the always-on set (it survives
                // sleep entry untouched - see STEP_PREFERRED). Non-
                // fatal like the gestures; the watchdog re-arms it.
                if let Some(id) = self.step_sensor {
                    let _ = self.drv.configure_sensor(i2c, id, STEP_RATE_HZ, 0);
                }
                // The gesture enables may latch a benign rate
                // complaint (0x55 - the framework grumbles about the
                // requested rate and runs the sensor anyway).
                // Acknowledge it here so later error-register checks
                // (self-tests above all) only see their own errors.
                if let Ok(code) = self.drv.error_value(i2c) {
                    if code != 0 {
                        log::info!(
                            "IMU: post-enable note: {} (cleared)",
                            bhi260::error_description(code),
                        );
                        let _ = self.drv.clear_error_regs(i2c);
                    }
                }
                self.phase = Phase::Recycle { enabled: false };
                // Settle after the first enable burst before the
                // re-arbitration cycle below.
                BootStep::Pending { delay_ms: 100 }
            }
            Phase::Recycle { enabled } => {
                // See the phase's doc: reproduce the healing
                // disable->re-enable transition once.
                let r = if !enabled {
                    self.drv
                        .configure_sensor(i2c, STREAM_ACCEL, 0.0, 0)
                        .and_then(|_| {
                            self.drv
                                .configure_sensor(i2c, STREAM_GYRO, 0.0, 0)
                        })
                } else {
                    self.drv
                        .configure_sensor(i2c, STREAM_ACCEL, AWAKE_RATE_HZ, 0)
                        .and_then(|_| {
                            self.drv.configure_sensor(
                                i2c,
                                STREAM_GYRO,
                                AWAKE_RATE_HZ,
                                0,
                            )
                        })
                };
                if r.is_err() {
                    return self.fail("re-arbitration cycle failed");
                }
                if !enabled {
                    self.phase = Phase::Recycle { enabled: true };
                    BootStep::Pending { delay_ms: 100 }
                } else {
                    self.phase = Phase::ReadRanges { tries: 0 };
                    // Give the framework a beat to apply rate +
                    // range before reading the actual values back.
                    BootStep::Pending { delay_ms: 50 }
                }
            }
            Phase::ReadRanges { tries } => {
                // The framework answers parameter reads late while it
                // digests the enable burst - retry on this machine's
                // pacing (the discovery lesson, third application).
                let a = self.try_read_range(i2c, STREAM_ACCEL);
                let g = self.try_read_range(i2c, STREAM_GYRO);
                match (a, g) {
                    (Some((ar, arate)), Some((gr, grate))) => {
                        self.accel_range_si = ar;
                        self.gyro_range_si = gr;
                        log::info!(
                            "IMU: BHI260AP ready (accel +/-{} g @ {} Hz, gyro +/-{} dps @ {} Hz)",
                            ar,
                            arate,
                            gr,
                            grate,
                        );
                    }
                    _ if tries < RANGE_TRIES => {
                        self.phase = Phase::ReadRanges { tries: tries + 1 };
                        return BootStep::Pending { delay_ms: 50 };
                    }
                    _ => {
                        // Terminal fallback: assume the REQUESTED
                        // values - the chip has honored them on every
                        // observed run, unlike the chip defaults
                        // (assuming default 2000 dps against an
                        // applied 250 would scale the gyro 8x hot).
                        log::warn!(
                            "IMU: range read-back timed out - assuming requested values",
                        );
                        self.accel_range_si = REQ_ACCEL_RANGE_G as i32;
                        self.gyro_range_si = REQ_GYRO_RANGE_DPS as i32;
                        log::info!(
                            "IMU: BHI260AP ready (accel +/-{} g, gyro +/-{} dps)",
                            self.accel_range_si,
                            self.gyro_range_si,
                        );
                    }
                }
                self.phase = Phase::BootWom { entered: false };
                BootStep::Pending { delay_ms: 50 }
            }
            Phase::BootWom { entered } => {
                if !entered {
                    // Phase must be Ready for enter/exit_wom's
                    // guards; set it, then simulate the sleep.
                    self.phase = Phase::Ready;
                    self.enter_wom(i2c);
                    self.phase = Phase::BootWom { entered: true };
                    // The silent gap: the task sleeps on this delay
                    // and touches nothing.
                    BootStep::Pending { delay_ms: 3000 }
                } else {
                    self.phase = Phase::Ready;
                    self.exit_wom(i2c);
                    // Housekeeping drain, not a real wake.
                    self.announce_wake = None;
                    log::info!("IMU: boot WoM cycle complete");
                    BootStep::Ready
                }
            }
            Phase::Ready => BootStep::Ready,
            Phase::Failed(why) => BootStep::Failed(why),
        }
    }

    fn fail(&mut self, why: &'static str) -> BootStep {
        self.phase = Phase::Failed(why);
        BootStep::Failed(why)
    }


    /// One-line register dump for terminal bring-up failures, so a
    /// failed flash cycle carries its own diagnosis.
    fn log_diagnostics<I: I2cTrait>(&mut self, i2c: &mut I) {
        let boot = self.drv.boot_status(i2c).unwrap_or(0xFF);
        let irq = self.drv.interrupt_status(i2c).unwrap_or(0xFF);
        let err = self.drv.error_value(i2c).unwrap_or(0xFF);
        let aux = self.drv.read_reg(i2c, bhi260::reg::ERROR_AUX).unwrap_or(0xFF);
        let dbg = self.drv.read_reg(i2c, bhi260::reg::DEBUG_STATE).unwrap_or(0xFF);
        log::error!(
            "IMU: BHI260AP state: boot=0x{:02X} irq=0x{:02X} err=0x{:02X} ({}) aux=0x{:02X} dbg_state=0x{:02X}",
            boot,
            irq,
            err,
            bhi260::error_description(err),
            aux,
            dbg,
        );
    }

    /// Current dynamic range and ACTUAL sample rate of a virtual
    /// sensor from its configuration parameter (Table 75: rate f32
    /// at payload offset 0, range u16 at offset 10). `None` when the
    /// read fails or the range still reads 0 (not applied yet) - the
    /// caller retries. The rate is reported in the ready line: a
    /// value well under the requested rate means the framework
    /// re-arbitrated the sensor down (the rate-collapse failure the
    /// Recycle phase exists to prevent).
    fn try_read_range<I: I2cTrait>(
        &mut self,
        i2c: &mut I,
        sensor_id: u8,
    ) -> Option<(i32, i32)> {
        let mut buf = [0u8; 12];
        match self
            .drv
            .param_read(i2c, bhi260::param::VIRT_SENSOR_CONF_BASE + sensor_id as u16, &mut buf)
        {
            Ok(n) if n >= 12 => {
                let rate = f32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let r = u16::from_le_bytes([buf[10], buf[11]]) as i32;
                if r > 0 {
                    Some((r, rate as i32))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // ---- Task contract -------------------------------------------

    /// Drain the non-wake-up FIFO, keep the newest accel/gyro sample,
    /// and return it rescaled to the normalized raw ranges. With no
    /// fresh data the previous sample is returned (the chip samples
    /// at 25 Hz against the task's 20 Hz poll).
    pub fn read<I: I2cTrait>(&mut self, i2c: &mut I) -> Result<ImuData, ()> {
        if !self.is_ready() {
            return Err(());
        }
        // Deferred wake attribution - see `announce_wake`.
        if let Some(id) = self.announce_wake.take() {
            log::info!("IMU: woke by sensor {}", id);
        }
        // Service the wake-up FIFO whenever the chip offers it - an
        // unserviced wake transfer keeps the interrupt asserted and
        // wedges the non-wake pipeline (hardware-observed as
        // int=0x03 with a mute non-wake channel until the watchdog;
        // the datasheet's rule is that every channel with a pending
        // transfer must be drained).
        self.drain_wake_if_pending(i2c);
        // Read the non-wake channel unconditionally - gating on the
        // transfer-offer bit races the poll cadence and starves the
        // stream (hardware-observed: ~3 Hz effective at 25 Hz
        // sampling). Reading an empty channel latches a benign
        // "download channel empty" code; every error-register check
        // filters that class via `is_transient` instead.
        let mut buf = [0u8; 512];
        let n = self
            .drv
            .read_fifo(i2c, bhi260::reg::CHANNEL_NONWAKE_FIFO, &mut buf)
            .map_err(|_| ())?;
        // FIFO watchdog: an enabled 25 Hz sensor pair that delivers
        // nothing for ~10 s of polling means an enable was silently
        // dropped - re-arm the sensors (no-op cost when healthy).
        if n == 0 && self.st.is_none() {
            self.empty_streak = self.empty_streak.saturating_add(1);
            if self.empty_streak >= 200 {
                // Log the Host Interface Control register with the
                // warning: a stuck AP-suspend bit (bit 4) blocks
                // non-wake transfers entirely - sensors then look
                // enabled-but-mute and re-arming alone cannot help,
                // so clear it defensively before re-arming.
                let hif = self
                    .drv
                    .read_reg(i2c, bhi260::reg::HOST_INTERFACE_CTRL)
                    .unwrap_or(0xFF);
                let ist = self.drv.interrupt_status(i2c).unwrap_or(0xFF);
                log::warn!(
                    "IMU: no FIFO data for ~10 s (hif_ctrl=0x{:02X} int=0x{:02X}) - re-arming sensors",
                    hif,
                    ist,
                );
                // Detect-and-remedy only: the logged registers make
                // a trigger diagnosable, and re-enabling the sensors
                // is the one generic remedy for "silently off". Past
                // wedge causes (unserviced wake transfers, stuck
                // suspend theories) are fixed or disproven at their
                // roots - this must not re-grow into a pile of stale
                // fixes that masks the next new cause.
                self.restore_running(i2c);
                self.empty_streak = 0;
            }
        } else {
            self.empty_streak = 0;
        }
        let mut p = fifo::Parser::new(&buf[..n]);
        while let Some(ev) = p.next_event() {
            match ev {
                fifo::Event::Vector3 { id, x, y, z } => match id {
                    STREAM_ACCEL => {
                        self.last.accel_x = rescale(x, self.accel_range_si, NORM_ACCEL_RANGE_G);
                        self.last.accel_y = rescale(y, self.accel_range_si, NORM_ACCEL_RANGE_G);
                        self.last.accel_z = rescale(z, self.accel_range_si, NORM_ACCEL_RANGE_G);
                        self.wear.feed(self.last.accel_x, self.last.accel_y, self.last.accel_z);
                    }
                    STREAM_GYRO => {
                        self.last.gyro_x = rescale(x, self.gyro_range_si, NORM_GYRO_RANGE_DPS);
                        self.last.gyro_y = rescale(y, self.gyro_range_si, NORM_GYRO_RANGE_DPS);
                        self.last.gyro_z = rescale(z, self.gyro_range_si, NORM_GYRO_RANGE_DPS);
                    }
                    _ => {}
                },
                // On-change total from the step counter; the newest
                // event wins within a drain.
                fifo::Event::StepCount { steps, .. } => {
                    self.last.steps = Some(steps);
                }
                _ => {}
            }
        }
        if let Some(id) = p.lost_sync {
            log::warn!("IMU: FIFO lost sync at event id {} - aborting transfer", id);
            // Loss-of-sync recovery per Section 16.4: abort the
            // channel transfer; the next transfer restarts cleanly at
            // a block boundary.
            let _ = self.drv.write_reg(
                i2c,
                bhi260::reg::HOST_INTERFACE_CTRL,
                bhi260::hif_control::ABORT_CH2,
            );
            let _ = self.drv.write_reg(i2c, bhi260::reg::HOST_INTERFACE_CTRL, 0);
        }
        Ok(self.last.clone())
    }

    /// Sleep mode: continuous sensors off (the step counter stays
    /// running - it is non-wake-up and counts through host sleep),
    /// wake-up motion sensor on, hub told the host is suspending
    /// (only wake-up sensors then assert the interrupt -
    /// Section 16.3).
    pub fn enter_wom<I: I2cTrait>(&mut self, i2c: &mut I) {
        if !self.is_ready() {
            return;
        }
        // Pose history dies with the sleep: the first raise after
        // wake must always produce a fresh transition.
        self.wear.reset();
        let r = self
            .drv
            .configure_sensor(i2c, STREAM_ACCEL, 0.0, 0)
            .and_then(|_| self.drv.configure_sensor(i2c, STREAM_GYRO, 0.0, 0))
            .and_then(|_| self.drv.flush_fifo(i2c, flush::DISCARD_WAKE))
            .and_then(|_| self.drv.flush_fifo(i2c, flush::DISCARD_NONWAKE));
        if r.is_err() {
            log::error!("IMU: failed to reconfigure for WoM");
            return;
        }
        for i in 0..self.wake_count {
            let id = self.wake_sensors[i];
            if self.drv.configure_sensor(i2c, id, WOM_RATE_HZ, 0).is_err() {
                log::error!("IMU: failed to enable wake sensor {}", id);
                return;
            }
            // Configure has no response packet; rejections land in
            // the error register only - check, or a dead wake path
            // looks like healthy silence.
            if let Ok(code) = self.drv.error_value(i2c) {
                if is_transient(code) {
                    let _ = self.drv.clear_error_regs(i2c);
                } else if code != 0 {
                    log::warn!(
                        "IMU: wake sensor {} enable rejected: {}",
                        id,
                        bhi260::error_description(code),
                    );
                    let _ = self.drv.clear_error_regs(i2c);
                }
            }
        }
        // Final drain before suspending: catch anything the enables
        // pushed into the wake FIFO so the armed interrupt line
        // starts from a clean, deasserted state.
        let _ = self.drain_wake_if_pending(i2c);
        if self.drv.set_ap_suspended(i2c, true).is_err() {
            log::warn!("IMU: failed to signal AP suspend");
        }
        log::info!(
            "IMU: WoM enabled (BHI260AP wake sensors {:?})",
            &self.wake_sensors[..self.wake_count],
        );
    }

    /// Check for a latched wake-up event: any virtual sensor event in
    /// the wake-up FIFO counts (only the wake sensor feeds it during
    /// sleep). Reading the FIFO clears the condition.
    pub fn wom_event<I: I2cTrait>(&mut self, i2c: &mut I) -> bool {
        if !self.is_ready() {
            return false;
        }
        let st = match self.drv.interrupt_status(i2c) {
            Ok(s) => s,
            Err(_) => return false,
        };
        use bhi260::int_status as is;
        if (st >> is::WAKE_FIFO_SHIFT) & is::FIFO_FIELD_MASK == 0 {
            return false;
        }
        let mut buf = [0u8; 128];
        let n = match self.drv.read_fifo(i2c, bhi260::reg::CHANNEL_WAKE_FIFO, &mut buf) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let mut p = fifo::Parser::new(&buf[..n]);
        while let Some(ev) = p.next_event() {
            match ev {
                fifo::Event::Meta { .. } => continue,
                // Any sensor event in the wake-up FIFO means motion.
                fifo::Event::Occurrence { id } => {
                    log::info!("IMU: wake event from sensor {}", id);
                    self.announce_wake = Some(id);
                    return true;
                }
                _ => return true,
            }
        }
        false
    }

    /// Wake mode: hub told the host is awake again, wake sensor off,
    /// continuous accel+gyro back on.
    pub fn exit_wom<I: I2cTrait>(&mut self, i2c: &mut I) {
        if !self.is_ready() {
            return;
        }
        if self.drv.set_ap_suspended(i2c, false).is_err() {
            log::warn!("IMU: failed to clear AP suspend");
        }
        // Re-arm the full wake/gesture set: they stay enabled while
        // awake (dim recovery), and whichever one-shot fired to
        // cause this wake has disarmed itself.
        for i in 0..self.wake_count {
            let _ = self.drv.configure_sensor(i2c, self.wake_sensors[i], WOM_RATE_HZ, 0);
        }
        // Drain whatever the wake-up FIFO still holds. A wake that
        // came through the interrupt line (instead of the latch
        // poll) leaves the wake transfer unserviced, and an
        // unserviced transfer keeps the interrupt asserted and
        // blocks new transfers - the non-wake FIFO then reads empty
        // with sensors enabled and no error anywhere (observed on
        // hardware). Draining also tells us what fired.
        if let Some(id) = self.drain_wake_if_pending(i2c) {
            self.announce_wake = Some(id);
        }
        let r = self
            .drv
            .configure_sensor(i2c, STREAM_ACCEL, AWAKE_RATE_HZ, 0)
            .and_then(|_| {
                self.drv
                    .configure_sensor(i2c, STREAM_GYRO, AWAKE_RATE_HZ, 0)
            });
        if r.is_err() {
            log::error!("IMU: re-enable after WoM failed");
        } else {
            log::info!("IMU: WoM mode exited");
        }
    }

    /// Begin a physical self-test (Section 13.2.5): disable the
    /// continuous sensors and enter the quiesce phase - the actual
    /// request is issued by [`Self::self_test_poll`] once the
    /// PHYSICAL sensor reports it has powered down. Returns `false`
    /// if it could not start.
    pub fn self_test_start<I: I2cTrait>(&mut self, i2c: &mut I, kind: SelfTestKind) -> bool {
        if !self.is_ready() || self.st.is_some() {
            return false;
        }
        // EVERY client of the physical sensors must quiesce - the
        // continuous accel/gyro AND the gesture/wake sensors (they
        // hold the physical accel Active; hardware-observed: with
        // them running, the accel never leaves power mode 7 and the
        // self-test can never start).
        let mut off = self
            .drv
            .configure_sensor(i2c, STREAM_ACCEL, 0.0, 0)
            .and_then(|_| self.drv.configure_sensor(i2c, STREAM_GYRO, 0.0, 0));
        for i in 0..self.wake_count {
            off = off
                .and_then(|_| self.drv.configure_sensor(i2c, self.wake_sensors[i], 0.0, 0));
        }
        // The step counter holds the physical accel Active too - it
        // must quiesce with the rest or the test never starts.
        if let Some(id) = self.step_sensor {
            off = off.and_then(|_| self.drv.configure_sensor(i2c, id, 0.0, 0));
        }
        if off.is_err() {
            self.restore_running(i2c);
            return false;
        }
        self.st = Some(StState::Quiesce { kind, polls: 0 });
        true
    }

    /// One short progress check of an in-flight self-test - the
    /// caller paces the polls and keeps the bus free in between.
    /// Accel reports its offsets in mg; the gyro reports none
    /// (values stay 0 - pass/fail is the result).
    pub fn self_test_poll<I: I2cTrait>(&mut self, i2c: &mut I) -> SelfTestPoll {
        match self.st {
            None => SelfTestPoll::Error,
            Some(StState::Quiesce { kind, polls }) => {
                let phys = match kind {
                    SelfTestKind::Accel => bhi260::phys::ACCEL,
                    SelfTestKind::Gyro => bhi260::phys::GYRO,
                };
                // Physical Sensor Information flags (payload byte 6),
                // bits 5-7 = power mode: 1 Power Down, 2 Suspend are
                // quiesced; anything active means keep waiting.
                let mut info = [0u8; 20];
                let mode = match self.drv.phys_sensor_info(i2c, phys, &mut info) {
                    Ok(()) => info[6] >> 5,
                    Err(e) => {
                        // The framework answers parameter reads late
                        // while it is mid-transition (we just fired
                        // two sensor disables) - retry on the task's
                        // pacing within the quiesce budget rather
                        // than trusting the driver's tight internal
                        // spin window. Log only on terminal failure -
                        // a silent exit here cost a flash cycle once.
                        if polls >= QUIESCE_POLLS {
                            log_chip_error("self-test quiesce", &e);
                            return self.st_conclude_error(i2c);
                        }
                        self.st = Some(StState::Quiesce { kind, polls: polls + 1 });
                        return SelfTestPoll::Pending;
                    }
                };
                match mode {
                    1 | 2 => {
                        log::info!(
                            "IMU: phys sensor {} quiesced (mode {}) after {} polls - requesting self-test",
                            phys,
                            mode,
                            polls,
                        );
                        // Start from a clean error register so the
                        // wait loop only ever reacts to errors this
                        // test produces (the quiesce disables can
                        // latch benign rate complaints).
                        let _ = self.drv.clear_error_regs(i2c);
                        if self.drv.request_self_test(i2c, phys).is_err() {
                            return self.st_conclude_error(i2c);
                        }
                        self.st = Some(StState::Waiting { phys });
                        SelfTestPoll::Pending
                    }
                    _ if polls >= QUIESCE_POLLS => {
                        log::warn!(
                            "IMU: phys sensor {} never quiesced (power mode {})",
                            phys,
                            mode,
                        );
                        self.st_conclude_error(i2c)
                    }
                    _ => {
                        self.st = Some(StState::Quiesce { kind, polls: polls + 1 });
                        SelfTestPoll::Pending
                    }
                }
            }
            Some(StState::Waiting { phys }) => {
                let mut payload = [0u8; 12];
                match self.drv.try_read_status_packet(i2c, &mut payload) {
                    Ok(Some((code, _))) if code == bhi260::status::SELF_TEST_RESULTS => {
                        match Bhi260::decode_test_result(&payload) {
                            // Stale result from a different sensor's
                            // aborted run - discard, keep waiting.
                            Some(r) if r.sensor_id != phys => {
                                log::warn!(
                                    "IMU: discarding stale self-test result for sensor {}",
                                    r.sensor_id,
                                );
                                SelfTestPoll::Pending
                            }
                            // Status 8/9 mean unsupported / no device -
                            // not a pass/fail verdict about the sensor.
                            Some(r) if r.status < 8 => {
                                self.st = None;
                                self.restore_running(i2c);
                                SelfTestPoll::Done(SelfTestAxes {
                                    passed: r.status == 0,
                                    values: [
                                        r.offsets[0] as i32,
                                        r.offsets[1] as i32,
                                        r.offsets[2] as i32,
                                    ],
                                })
                            }
                            _ => self.st_conclude_error(i2c),
                        }
                    }
                    Ok(_) => {
                        // A rejected request answers through the
                        // error register instead of a status packet.
                        if let Ok(code) = self.drv.error_value(i2c) {
                            if is_transient(code) {
                                let _ = self.drv.clear_error_regs(i2c);
                            } else if code != 0 {
                                log::warn!(
                                    "IMU: self-test error: {}",
                                    bhi260::error_description(code),
                                );
                                let _ = self.drv.clear_error_regs(i2c);
                                return self.st_conclude_error(i2c);
                            }
                        }
                        SelfTestPoll::Pending
                    }
                    Err(_) => self.st_conclude_error(i2c),
                }
            }
        }
    }

    /// Abort an in-flight self-test (the caller's poll budget ran
    /// out) and restore the running configuration. No-op when
    /// nothing is in flight.
    pub fn self_test_abort<I: I2cTrait>(&mut self, i2c: &mut I) {
        if self.st.take().is_some() {
            self.restore_running(i2c);
        }
    }

    /// Collect the classifier's pending wrist intent, if any. No
    /// bus traffic; the samples were consumed during `read`.
    pub fn take_intent(&mut self) -> Option<WristIntent> {
        self.wear.take()
    }

    /// Common self-test failure exit: clear the state, restore the
    /// running sensors, report `Error`.
    fn st_conclude_error<I: I2cTrait>(&mut self, i2c: &mut I) -> SelfTestPoll {
        self.st = None;
        self.restore_running(i2c);
        SelfTestPoll::Error
    }

    /// Drain the wake-up FIFO only when Interrupt Status says it has
    /// data. Reading an EMPTY channel latches a harmless "download
    /// channel empty" code into the error register, which then
    /// pollutes later error-register checks - so never drain blind.
    /// Returns the sensor id of the last wake event drained, for
    /// wake attribution.
    fn drain_wake_if_pending<I: I2cTrait>(&mut self, i2c: &mut I) -> Option<u8> {
        let st = self.drv.interrupt_status(i2c).ok()?;
        use bhi260::int_status as is;
        if (st >> is::WAKE_FIFO_SHIFT) & is::FIFO_FIELD_MASK == 0 {
            return None;
        }
        let mut buf = [0u8; 128];
        let n = self.drv.read_fifo(i2c, bhi260::reg::CHANNEL_WAKE_FIFO, &mut buf).ok()?;
        let mut hit = None;
        let mut p = fifo::Parser::new(&buf[..n]);
        while let Some(ev) = p.next_event() {
            if let fifo::Event::Occurrence { id } = ev {
                hit = Some(id);
                // One-shot sensors disarmed themselves by firing -
                // re-arm immediately or they never fire again.
                if is_one_shot(id) {
                    let _ = self.drv.configure_sensor(i2c, id, WOM_RATE_HZ, 0);
                }
            }
        }
        hit
    }

    /// Re-enable the continuous sensors after a self-test concluded
    /// (any outcome). Configure Sensor has no response packet; a
    /// Bring the running sensor set back after a self-test (or the
    /// watchdog): wake/gesture sensors first, then the continuous
    /// pair - the same order as `exit_wom`. A rejection (e.g. the
    /// physical sensor is still busy with a self-test the host gave
    /// up waiting on) only lands in the error register - check it so
    /// a failed restore is visible. The FIFO watchdog in
    /// [`Self::read`] re-arms if this did not stick.
    fn restore_running<I: I2cTrait>(&mut self, i2c: &mut I) {
        let mut r = Ok(());
        for i in 0..self.wake_count {
            r = r.and_then(|_| {
                self.drv.configure_sensor(i2c, self.wake_sensors[i], WOM_RATE_HZ, 0)
            });
        }
        if let Some(id) = self.step_sensor {
            r = r.and_then(|_| self.drv.configure_sensor(i2c, id, STEP_RATE_HZ, 0));
        }
        r = r
            .and_then(|_| {
                self.drv
                    .configure_sensor(i2c, STREAM_ACCEL, AWAKE_RATE_HZ, 0)
            })
            .and_then(|_| {
                self.drv
                    .configure_sensor(i2c, STREAM_GYRO, AWAKE_RATE_HZ, 0)
            });
        if r.is_err() {
            log::warn!("IMU: sensor re-enable failed on the bus");
            return;
        }
        if let Ok(code) = self.drv.error_value(i2c) {
            if is_transient(code) {
                let _ = self.drv.clear_error_regs(i2c);
            } else if code != 0 {
                log::warn!(
                    "IMU: sensor re-enable rejected: {}",
                    bhi260::error_description(code),
                );
                let _ = self.drv.clear_error_regs(i2c);
            }
        }
    }
}

/// Rescale a raw i16 sample from the chip's dynamic range to the
/// normalized full-scale range, saturating at the i16 rails.
fn rescale(raw: i16, from_range: i32, to_range: i32) -> i16 {
    ((raw as i32 * from_range) / to_range).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Log the concrete failure reason of a driver call - the variants
/// carry the diagnosis (chip error code, rejected command, protocol
/// shape, or plain bus failure) and swallowing them costs a flash
/// cycle per repro.
fn log_chip_error<E>(what: &str, e: &Error<E>) {
    match e {
        Error::Chip(code) => {
            log::error!("IMU: {}: chip error: {}", what, bhi260::error_description(*code));
        }
        Error::Command { command, error } => {
            log::error!(
                "IMU: {}: command 0x{:04X} rejected (error 0x{:02X})",
                what,
                command,
                error,
            );
        }
        Error::Protocol(why) => log::error!("IMU: {}: {}", what, why),
        Error::Bus(_) => log::error!("IMU: {}: I2C bus error", what),
    }
}
