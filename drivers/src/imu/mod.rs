//! Board-selected IMU behind one task-level interface.
//!
//! The shared IMU task is an embassy task and therefore can't be
//! generic over the driver type, so boards pick their chip by
//! constructing the matching variant; the task dispatches through
//! this enum - the same pattern as `touch::AnyTouch`.
//!
//! The interface is the task's contract, not a chip API: a motion
//! snapshot while awake, a wake-on-motion mode while the system
//! sleeps, and the two boot self-tests. Chips whose bring-up is more
//! than a few register writes (the BHI260AP needs a ~100 KB firmware
//! upload) run it incrementally through [`AnyImu::boot_step`], so
//! the task can share the I2C bus fairly and the system boots
//! without waiting on the IMU.
//!
//! All motion samples are normalized to the QMI8658 raw scale the
//! UI was calibrated against: accel +/-8 g full scale, gyro
//! +/-256 dps full scale, i16 axes (see `ImuData`).

use embedded_hal::i2c::I2c as I2cTrait;

#[cfg(feature = "qmi8658")]
use crate::qmi8658::{Config as QmiConfig, Odr, Qmi8658, WomConfig, WomInterrupt};

#[cfg(feature = "bhi260")]
pub mod bhi260_imu;

/// A single snapshot of IMU sensor data - the cross-chip contract
/// every [`AnyImu`] variant delivers.
///
/// All values are raw 16-bit signed integers, normalized to the
/// scale the system's consumers are calibrated against: accel
/// +/-8 g full scale (1 g = 4096), gyro +/-256 dps full scale.
/// `temp_raw` is degrees Celsius x 256, or 0 for chips without a
/// temperature path.
///
/// Axes are the DEVICE frame (Android convention), not the chip's:
/// +X toward the screen's right edge (3 o'clock), +Y toward its top
/// edge (12 o'clock), +Z out of the screen toward the viewer - so a
/// device lying screen-up reads accel ~[0, 0, +4096]. Adapters owe
/// this frame to their consumers (via chip configuration, an
/// orientation-matrix write, or host-side remap); app-level logic
/// like the wrist-gesture trajectory gate depends on it.
#[derive(Debug, Clone, Default)]
pub struct ImuData {
    /// X-axis acceleration (raw signed 16-bit).
    pub accel_x: i16,
    /// Y-axis acceleration.
    pub accel_y: i16,
    /// Z-axis acceleration.
    pub accel_z: i16,
    /// X-axis angular rate (raw signed 16-bit).
    pub gyro_x: i16,
    /// Y-axis angular rate.
    pub gyro_y: i16,
    /// Z-axis angular rate.
    pub gyro_z: i16,
    /// Raw temperature. Divide by 256 for degrees Celsius.
    pub temp_raw: i16,
}

// ---- QMI8658 wake-on-motion tunables (datasheet section 9.4) -----

/// Accelerometer ODR while in Wake-on-Motion sleep. A low ODR
/// reduces per-sample slopes from micro-vibrations (USB cable, desk
/// noise) so the threshold can reject noise while still catching
/// real wrist motion.
#[cfg(feature = "qmi8658")]
const QMI_WOM_ACCEL_ODR: Odr = Odr::Hz31_25;

/// Motion threshold in milli-g for the WoM engine. Slopes smaller
/// than this are ignored. 80 mg filters out table vibrations; real
/// wrist motion produces much larger slopes.
#[cfg(feature = "qmi8658")]
const QMI_WOM_THRESHOLD_MG: u8 = 80;

/// Which interrupt pin WoM drives, and its idle-state value. INT1
/// starts low and toggles high on each motion event; reading STATUS1
/// resets it.
#[cfg(feature = "qmi8658")]
const QMI_WOM_INTERRUPT: WomInterrupt = WomInterrupt::Int1Low;

/// Blanking time after WoM enable, in accelerometer samples. 63 is
/// the max of the 6-bit blanking field - at 31.25 Hz that's about
/// 2 s, enough to skip power-up transients.
#[cfg(feature = "qmi8658")]
const QMI_WOM_BLANKING_SAMPLES: u8 = 63;

/// Progress of an incremental chip bring-up (see
/// [`AnyImu::boot_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootStep {
    /// Chip is operational - the task may start its normal loop.
    Ready,
    /// More bring-up work remains; call again after roughly this
    /// delay (the caller owns the pacing and the bus lock).
    Pending { delay_ms: u32 },
    /// Bring-up failed permanently this boot.
    Failed(&'static str),
}

/// Unified self-test outcome: pass/fail plus the chip's 3-axis
/// diagnostic values (mg for accel, dps for gyro; zeros where a chip
/// reports none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfTestAxes {
    pub passed: bool,
    pub values: [i32; 3],
}

/// Which self-test to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfTestKind {
    Accel,
    Gyro,
}

/// Progress of an in-flight self-test (see
/// [`AnyImu::self_test_poll`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfTestPoll {
    /// Still running - poll again after a short delay.
    Pending,
    /// Finished; pass/fail and diagnostics inside.
    Done(SelfTestAxes),
    /// Could not complete (rejected, unsupported, or bus trouble).
    Error,
}

/// Semantic wrist intents - the unified signals a wrist-worn board
/// emits through the seam, regardless of chip. The system layer
/// consumes only these; all pose math stays below the seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WristIntent {
    /// The wrist entered the viewing pose and settled there.
    Raised,
    /// The wrist left the viewing pose and settled elsewhere.
    Lowered,
}

/// Wear calibration, supplied by the bin: it encodes how the user
/// wears this specific board (wrist, crown direction, personal
/// viewing angle), which neither the chip nor any shared layer can
/// know. Thresholds are in the seam's normalized accel scale
/// (+/-8 g full scale, 4096 = 1 g), applied to the gravity
/// component along `axis`.
#[derive(Debug, Clone, Copy)]
pub struct WearCalibration {
    /// Viewing-axis selector in the device frame; components are
    /// -1/0/1 and pick (possibly negated) axes, e.g. `[0, 1, 0]`
    /// = +Y.
    pub axis: [i8; 3],
    /// In the viewing cone when the along-axis component rises to
    /// this or above.
    pub enter_lsb: i16,
    /// Out of it when the component drops to this or below; the
    /// gap between the two is the hysteresis band, which holds the
    /// current side.
    pub exit_lsb: i16,
}

/// Consecutive samples required on the far side of a threshold
/// before the pose flips - ~0.5 s at the 25 Hz awake sample rate,
/// so arm swings passing through the cone don't flicker intents.
const WEAR_DWELL_SAMPLES: u8 = 12;

/// Pose classifier over the accel stream: tracks whether the wrist
/// is inside the calibrated viewing cone and turns settled
/// transitions into [`WristIntent`]s. Chip-neutral pure math - an
/// adapter feeds it every normalized accel sample it parses and
/// hands out the produced intents via its `take_intent`.
pub struct WearClassifier {
    cal: WearCalibration,
    in_view: bool,
    streak: u8,
    pending: Option<WristIntent>,
}

impl WearClassifier {
    pub fn new(cal: WearCalibration) -> Self {
        Self {
            cal,
            in_view: false,
            streak: 0,
            pending: None,
        }
    }

    /// Feed one normalized accel sample (device frame).
    ///
    /// Streak rules: a sample past the far threshold accumulates
    /// evidence toward flipping; a sample past the near threshold
    /// (confirming the current side) resets it; a sample inside the
    /// hysteresis band is AMBIGUOUS and holds the streak as-is. The
    /// band must not reset - a held-but-human arm wobbles into it,
    /// and resetting there made real raises unrecognizable
    /// (hardware-observed 2026-07-31).
    pub fn feed(&mut self, x: i16, y: i16, z: i16) {
        let along = self.cal.axis[0] as i32 * x as i32
            + self.cal.axis[1] as i32 * y as i32
            + self.cal.axis[2] as i32 * z as i32;
        let evidence = if along >= self.cal.enter_lsb as i32 {
            Some(true)
        } else if along <= self.cal.exit_lsb as i32 {
            Some(false)
        } else {
            None
        };
        match evidence {
            None => {}
            Some(side) if side == self.in_view => self.streak = 0,
            Some(side) => {
                self.streak += 1;
                if self.streak >= WEAR_DWELL_SAMPLES {
                    self.streak = 0;
                    self.in_view = side;
                    self.pending = Some(if side {
                        WristIntent::Raised
                    } else {
                        WristIntent::Lowered
                    });
                }
            }
        }
    }

    /// Collect the intent produced by the last settled pose flip,
    /// if any. Clears on read.
    pub fn take(&mut self) -> Option<WristIntent> {
        self.pending.take()
    }

    /// Forget pose history (call at sleep entry): the next raise
    /// then always produces a fresh `Raised` transition, even if
    /// the device fell asleep with the wrist still in the cone.
    pub fn reset(&mut self) {
        self.in_view = false;
        self.streak = 0;
        self.pending = None;
    }
}

/// Number of samples averaged for the initial gyro bias during
/// bring-up. At 125 Hz that's ~512 ms; the device should be held
/// still during this window (shortly after power-on).
#[cfg(feature = "qmi8658")]
const QMI_GYRO_BIAS_SAMPLES: u8 = 64;

/// Settling polls before a QMI self-test actually runs (x10 ms task
/// cadence = 250 ms). Hardware lesson carried over from the
/// pre-seam code: a self-test started too soon after a soft reset
/// or `init()` times out on STATUSINT.bit0 - 250 ms reliably clears
/// that window. The seam restores config via `init()` after every
/// test, so EVERY test gets this settle, not just the first
/// (regression 2026-07-31: the gyro test ran ~15 ms after the accel
/// test's restore and timed out on the S3).
#[cfg(feature = "qmi8658")]
const QMI_ST_SETTLE_POLLS: u8 = 25;

/// QMI8658 bring-up state machine phases (see
/// [`QmiImu::boot_step`]).
#[cfg(feature = "qmi8658")]
enum QmiPhase {
    Reset,
    Init,
    Bias,
    Ready,
    Failed(&'static str),
}

/// QMI8658 wrapped with its staged bring-up: soft reset, init +
/// identity log, a settling window, then gyro-bias collection. The
/// boot self-tests are NOT part of the machine - the shared task
/// runs them through the seam once bring-up reports `Ready`, the
/// same as for every other chip.
#[cfg(feature = "qmi8658")]
pub struct QmiImu {
    imu: Qmi8658,
    phase: QmiPhase,
    /// In-flight self-test: the requested kind plus the settle-poll
    /// count ([`QMI_ST_SETTLE_POLLS`]). `self_test_start` only
    /// records the request; the test itself runs inline (bounded
    /// millisecond-scale waits in the driver) on the poll that ends
    /// the settle window.
    st_pending: Option<(SelfTestKind, u8)>,
}

#[cfg(feature = "qmi8658")]
impl Default for QmiImu {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "qmi8658")]
impl QmiImu {
    pub fn new() -> Self {
        Self {
            imu: Qmi8658::new(QmiConfig::default()),
            phase: QmiPhase::Reset,
            st_pending: None,
        }
    }

    fn boot_step<I: I2cTrait>(&mut self, i2c: &mut I) -> BootStep {
        match self.phase {
            QmiPhase::Reset => {
                log::info!("IMU: initializing QMI8658C...");
                // Soft reset to clear any leftover state from a
                // previous boot (WoM config, pedometer, etc.).
                if self.imu.soft_reset(i2c).is_ok() {
                    log::info!("IMU: soft reset OK");
                } else {
                    log::warn!("IMU: soft reset failed (continuing anyway)");
                }
                self.phase = QmiPhase::Init;
                BootStep::Pending { delay_ms: 20 }
            }
            QmiPhase::Init => {
                if self.imu.init(i2c, &QmiConfig::default()).is_err() {
                    log::error!(
                        "IMU: device not found at I2C address 0x{:02X}",
                        crate::qmi8658::ADDR,
                    );
                    self.phase = QmiPhase::Failed("QMI8658 not found");
                    return BootStep::Failed("QMI8658 not found");
                }
                match self.imu.read_ids(i2c) {
                    Ok((chip_id, rev)) => log::info!(
                        "IMU: QMI8658C chip_id=0x{:02X} rev=0x{:02X}",
                        chip_id,
                        rev,
                    ),
                    Err(_) => log::warn!("IMU: init OK but failed to read IDs"),
                }
                self.phase = QmiPhase::Bias;
                // Settling window before the next chip interaction -
                // right after reset + init the chip needs ~250 ms
                // before self-tests/sampling behave (the old boot
                // path learned this the hard way).
                BootStep::Pending { delay_ms: 250 }
            }
            QmiPhase::Bias => {
                log::info!("IMU: collecting gyro bias (keep device still ~512ms)...");
                match self.imu.collect_gyro_bias(i2c, QMI_GYRO_BIAS_SAMPLES) {
                    Err(_) => log::error!("IMU: failed to collect gyro bias"),
                    Ok((bx, by, bz)) => {
                        log::info!("IMU: gyro bias raw [{} {} {}]", bx, by, bz);
                        self.imu.set_gyro_bias(bx, by, bz);
                        log::info!("IMU: gyro bias applied (software)");
                    }
                }
                self.phase = QmiPhase::Ready;
                BootStep::Ready
            }
            QmiPhase::Ready => BootStep::Ready,
            QmiPhase::Failed(why) => BootStep::Failed(why),
        }
    }
}

/// Board-selected IMU.
pub enum AnyImu {
    #[cfg(feature = "qmi8658")]
    Qmi8658(QmiImu),
    #[cfg(feature = "bhi260")]
    Bhi260(bhi260_imu::Bhi260Imu),
}

impl AnyImu {
    /// The chip's marketing name, for UI display. Known statically
    /// from the variant - valid before bring-up completes.
    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(_) => "QMI8658",
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(_) => "BHI260AP",
        }
    }

    /// Advance the incremental bring-up by one short bus
    /// transaction; the caller owns the pacing and the bus lock.
    pub fn boot_step<I: I2cTrait>(&mut self, i2c: &mut I) -> BootStep {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(q) => q.boot_step(i2c),
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(b) => b.boot_step(i2c),
        }
    }

    /// One motion snapshot in the normalized raw scale (accel
    /// +/-8 g, gyro +/-256 dps full scale).
    pub fn read<I: I2cTrait>(&mut self, i2c: &mut I) -> Result<ImuData, ()> {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(q) => q.imu.read(i2c).map_err(|_| ()),
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(b) => b.read(i2c),
        }
    }

    /// Collect a pending semantic wrist intent, if any (produced by
    /// the adapter's [`WearClassifier`] from the samples consumed by
    /// [`Self::read`]). No bus traffic. Boards that are not
    /// wrist-worn never produce one.
    pub fn take_intent(&mut self) -> Option<WristIntent> {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(_) => None,
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(b) => b.take_intent(),
        }
    }

    /// Reconfigure for the system's sleep phase: continuous sampling
    /// off, hardware motion detection armed. Poll
    /// [`Self::wom_event`] afterwards.
    pub fn enter_wom<I: I2cTrait>(&mut self, i2c: &mut I) {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(q) => {
                // QMI8658C datasheet section 9.4: disable sensors,
                // low-power accel ODR, WoM threshold/pin/blanking via
                // CTRL9, re-enable accel.
                if q.imu.disable_all(i2c).is_err() {
                    log::error!("IMU: failed to disable sensors for WoM");
                    return;
                }
                if q.imu.set_accel_odr(i2c, QMI_WOM_ACCEL_ODR).is_err() {
                    log::warn!("IMU: failed to set accel ODR for WoM");
                }
                let wom_cfg = WomConfig {
                    threshold_mg: QMI_WOM_THRESHOLD_MG,
                    interrupt: QMI_WOM_INTERRUPT,
                    blanking_samples: QMI_WOM_BLANKING_SAMPLES,
                };
                if q.imu.configure_wom(i2c, &wom_cfg).is_err() {
                    log::error!("IMU: WoM configuration failed");
                    return;
                }
                if q.imu.set_accel_enable(i2c, true).is_err() {
                    log::error!("IMU: failed to enable accel for WoM");
                    return;
                }
                // Clear any stale STATUS1.WOM bit left over from a
                // previous sleep cycle so the poll loop doesn't fire
                // immediately.
                let _ = q.imu.wom_event(i2c);
                log::info!(
                    "IMU: WoM enabled ({} mg threshold, 31.25 Hz accel ODR)",
                    wom_cfg.threshold_mg,
                );
            }
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(b) => b.enter_wom(i2c),
        }
    }

    /// `true` if a motion event was latched since the last check
    /// (checking clears it).
    pub fn wom_event<I: I2cTrait>(&mut self, i2c: &mut I) -> bool {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(q) => q.imu.wom_event(i2c).unwrap_or(false),
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(b) => b.wom_event(i2c),
        }
    }

    /// Undo [`Self::enter_wom`]: motion detection off, continuous
    /// sampling restored.
    pub fn exit_wom<I: I2cTrait>(&mut self, i2c: &mut I) {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(q) => {
                // Clear any pending WoM flag and reset INT1, disable
                // WoM (datasheet section 9.6), re-init the normal
                // 125 Hz accel+gyro config.
                let _ = q.imu.wom_event(i2c);
                if q.imu.disable_wom(i2c).is_err() {
                    log::warn!("IMU: failed to disable WoM");
                }
                if q.imu.init(i2c, &QmiConfig::default()).is_err() {
                    log::error!("IMU: re-init after WoM failed");
                } else {
                    log::info!("IMU: WoM mode exited");
                }
            }
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(b) => b.exit_wom(i2c),
        }
    }

    /// Begin a self-test. Returns `false` if it could not start.
    /// The caller then polls [`Self::self_test_poll`] with the bus
    /// released between polls - some chips take the better part of a
    /// second (a gyro drive has to spin up), and holding the shared
    /// bus that long would freeze every other peripheral.
    pub fn self_test_start<I: I2cTrait>(&mut self, i2c: &mut I, kind: SelfTestKind) -> bool {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(q) => {
                // Record only - the test runs on the poll that ends
                // the settle window (see QMI_ST_SETTLE_POLLS: a test
                // fired too soon after init() times out on
                // STATUSINT.bit0).
                let _ = i2c;
                q.st_pending = Some((kind, 0));
                true
            }
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(b) => b.self_test_start(i2c, kind),
        }
    }

    /// One short progress check of an in-flight self-test.
    pub fn self_test_poll<I: I2cTrait>(&mut self, i2c: &mut I) -> SelfTestPoll {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(q) => {
                let Some((kind, polls)) = q.st_pending else {
                    return SelfTestPoll::Error;
                };
                if polls < QMI_ST_SETTLE_POLLS {
                    q.st_pending = Some((kind, polls + 1));
                    return SelfTestPoll::Pending;
                }
                q.st_pending = None;
                // Settle over - run the test inline (the driver's
                // own waits are bounded, millisecond scale).
                let r = match kind {
                    SelfTestKind::Accel => q
                        .imu
                        .run_accel_self_test(i2c)
                        .map(|r| SelfTestAxes {
                            passed: r.passed,
                            values: [r.x_mg, r.y_mg, r.z_mg],
                        })
                        .map_err(|_| ()),
                    SelfTestKind::Gyro => q
                        .imu
                        .run_gyro_self_test(i2c)
                        .map(|r| SelfTestAxes {
                            passed: r.passed,
                            values: [r.x_dps, r.y_dps, r.z_dps],
                        })
                        .map_err(|_| ()),
                };
                // The self-test leaves sensors disabled and CTRL2/3
                // partially modified - restore the normal config
                // before handing the bus back.
                if q.imu.init(i2c, &QmiConfig::default()).is_err() {
                    log::error!("IMU: re-init after self-test failed");
                }
                match r {
                    Ok(axes) => SelfTestPoll::Done(axes),
                    Err(()) => SelfTestPoll::Error,
                }
            }
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(b) => b.self_test_poll(i2c),
        }
    }

    /// Abort an in-flight self-test after the caller's poll budget
    /// ran out, restoring the chip's running configuration. No-op if
    /// nothing is in flight.
    pub fn self_test_abort<I: I2cTrait>(&mut self, i2c: &mut I) {
        match self {
            #[cfg(feature = "qmi8658")]
            AnyImu::Qmi8658(q) => {
                // Mid-settle the chip is untouched, so there is
                // nothing to restore - just drop the request.
                let _ = i2c;
                q.st_pending = None;
            }
            #[cfg(feature = "bhi260")]
            AnyImu::Bhi260(b) => b.self_test_abort(i2c),
        }
    }

}
