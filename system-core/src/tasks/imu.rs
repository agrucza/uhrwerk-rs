//! Shared IMU task - the chip-neutral schedule over the
//! `drivers::imu::AnyImu` seam.
//!
//! This task owns system-facing behavior only: WHEN to sample, WHEN
//! to arm wake-on-motion, WHEN to run self-tests, and which events to
//! emit. Everything chip-specific - bring-up, WoM configuration,
//! sample scaling - lives behind the seam; the bin picks the chip by
//! constructing the matching [`AnyImu`] variant into
//! [`ImuTaskState::new`].
//!
//! Two modes, driven by the system sleep state:
//!
//!   * **Awake**: motion snapshots at 20 Hz, emitted as
//!     `MotionUpdated` for live-display screens.
//!   * **Sleeping**: the chip's wake-on-motion engine is armed and
//!     its event latch polled at 2 Hz; a latched event emits
//!     `WakeOnMotion`.
//!
//! Bring-up runs inside the task (see [`AnyImu::boot_step`]): each
//! chip advances one short bus transaction at a time, so system boot
//! never waits on the IMU and a chip that needs a firmware upload
//! shares the I2C bus fairly. The boot self-tests run through the
//! seam once bring-up completes, then their results are replayed
//! over the event bus.
//!
//! Why the sleep path polls a latch instead of awaiting the INT pin:
//! on the QMI8658C silicon this system started on, the WoM interrupt
//! output provably never fires even though the chip latches the
//! event in STATUS1 (verified against both polarities and pull
//! configurations). Polling the seam's latch works on every chip and
//! costs one short register read per poll.

use app_core::events::{NUM_SELF_TESTS, SelfTestError, SelfTestId, SelfTestResult, SystemEvent};
use crate::bus::{EVENTS, IMU_COMMAND, ImuCommand, SleepState, SharedI2c, SLEEP_WATCH};
use drivers::imu::{
    AnyImu, BootStep, ImuData, SelfTestAxes, SelfTestKind, SelfTestPoll, WristIntent,
};
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_time::{Duration, Timer};
use embedded_hal::i2c::I2c as I2cTrait;
use esp_hal::gpio::Input;

/// Periodic IMU snapshot cadence while awake. 50 ms = 20 Hz,
/// plenty for live sensor-display screens without hammering the
/// I2C bus.
const AWAKE_POLL_MS: u64 = 50;

/// Wake-on-motion latch poll cadence while sleeping. 500 ms = 2 Hz
/// is a compromise between wake latency and I2C traffic / power.
const WOM_POLL_MS: u64 = 500;

/// Self-test pacing: seam polls every 10 ms with the bus released in
/// between, up to ~5 s total. The budget covers a chip's observed
/// power-down quiesce phase plus the test itself (the reference host
/// library allows 1 s for the test alone).
const SELF_TEST_POLL_MS: u64 = 10;
const SELF_TEST_POLLS: u32 = 500;

// `MotionData` (struct + Default + From<&ImuData>) lives in
// `app_core::data`. Re-exported so `crate::tasks::imu::
// MotionData` imports in firmware keep resolving.
pub use app_core::data::MotionData;

pub struct ImuTaskState<'d> {
    pub imu: AnyImu,
    int_pin: Input<'d>,
    /// Last known result of each IMU-owned self-test, indexed by
    /// `SelfTestId as usize`. Populated by the task after bring-up
    /// completes; updated in place by [`handle_command`] when a UI
    /// re-run request comes through [`IMU_COMMAND`].
    self_tests: [SelfTestResult; NUM_SELF_TESTS],
}

impl<'d> ImuTaskState<'d> {
    /// Wrap the bin's chip choice. No bus traffic happens here -
    /// bring-up runs inside [`imu_task`] via the seam's staged
    /// `boot_step`.
    pub fn new(imu: AnyImu, int_pin: Input<'d>) -> Self {
        Self {
            imu,
            int_pin,
            self_tests: [SelfTestResult::NotRun; NUM_SELF_TESTS],
        }
    }

    /// Read a single IMU snapshot and return it as `MotionData`.
    /// Returns `Default` (all zeros) if the read fails or the chip
    /// never came up.
    pub fn snapshot(&mut self, i2c: &mut impl I2cTrait) -> MotionData {
        self.imu.read(i2c).ok().as_ref().map(MotionData::from).unwrap_or_default()
    }

    /// Raw seam-level read. Kept for places that want access to the
    /// un-converted `ImuData` (e.g. calibration routines).
    #[allow(dead_code)]
    pub fn read(&mut self, i2c: &mut impl I2cTrait) -> Option<ImuData> {
        self.imu.read(i2c).ok()
    }

    /// Async wait for an IMU interrupt. Not used by the WoM wake
    /// path (see module docs for why) but kept in case the INT line
    /// is ever repurposed - e.g. for data-ready pacing.
    #[allow(dead_code)]
    pub async fn wait_for_int(&mut self) {
        self.int_pin.wait_for_rising_edge().await;
    }

}

/// Run one self-test by id: start it through the seam, then pace the
/// polls with the bus free between checks (a chip may take the
/// better part of a second), and map the outcome onto the UI's
/// result type. The seam restores the chip's running configuration
/// on every exit path.
async fn run_self_test(
    bus: &'static SharedI2c,
    state: &mut ImuTaskState<'static>,
    id: SelfTestId,
) -> SelfTestResult {
    let (label, unit, kind) = match id {
        SelfTestId::ImuAccel => ("accel", "mg", SelfTestKind::Accel),
        SelfTestId::ImuGyro => ("gyro", "dps", SelfTestKind::Gyro),
    };
    let started = {
        let mut i2c = bus.lock().await;
        state.imu.self_test_start(&mut *i2c, kind)
    };
    if started {
        for _ in 0..SELF_TEST_POLLS {
            Timer::after(Duration::from_millis(SELF_TEST_POLL_MS)).await;
            let poll = {
                let mut i2c = bus.lock().await;
                state.imu.self_test_poll(&mut *i2c)
            };
            match poll {
                SelfTestPoll::Pending => continue,
                SelfTestPoll::Done(SelfTestAxes { passed, values }) => {
                    log::info!(
                        "IMU: {} self-test {} [{} {} {}] {}",
                        label,
                        if passed { "PASS" } else { "FAIL" },
                        values[0],
                        values[1],
                        values[2],
                        unit,
                    );
                    return if passed {
                        SelfTestResult::PassAxes3(values)
                    } else {
                        SelfTestResult::FailAxes3(values)
                    };
                }
                SelfTestPoll::Error => break,
            }
        }
        // Budget exhausted or errored - make sure the chip is back
        // in its running configuration (no-op after a clean end).
        let mut i2c = bus.lock().await;
        state.imu.self_test_abort(&mut *i2c);
    }
    log::warn!("IMU: {} self-test failed to complete", label);
    SelfTestResult::Error(SelfTestError::Timeout)
}

/// IMU task: staged bring-up, boot self-tests, then two modes driven
/// by [`SLEEP_WATCH`]. Awake it emits `MotionUpdated` events at
/// 20 Hz; sleeping it arms WoM and polls the seam's latch at 2 Hz,
/// emitting `WakeOnMotion` on set.
#[embassy_executor::task]
pub async fn imu_task(bus: &'static SharedI2c, mut state: ImuTaskState<'static>) {
    // Staged chip bring-up: one short bus transaction per iteration,
    // paced by the seam's delay hints. On failure the task keeps
    // running with an inert chip - snapshots read all-zero and sleep
    // has no motion wake (GPIO wake sources are unaffected).
    let ready = loop {
        let step = {
            let mut i2c = bus.lock().await;
            state.imu.boot_step(&mut *i2c)
        };
        match step {
            BootStep::Ready => break true,
            BootStep::Pending { delay_ms } => {
                Timer::after(Duration::from_millis(delay_ms as u64)).await;
            }
            BootStep::Failed(why) => {
                log::error!("IMU: bring-up failed: {}", why);
                break false;
            }
        }
    };

    // Boot self-tests, through the seam's staged path for every
    // chip.
    if ready {
        for id in [SelfTestId::ImuAccel, SelfTestId::ImuGyro] {
            let result = run_self_test(bus, &mut state, id).await;
            state.self_tests[id as usize] = result;
        }
    }

    // Announce which chip this board carries (the UI renders it on
    // the settings MOTION row instead of hardcoding a name).
    EVENTS
        .send(SystemEvent::ImuIdentified { name: state.imu.name() })
        .await;

    // Replay the self-test results once, so whichever screen is
    // interested can pick them up from `cached_data` without having
    // to re-run the tests on first open.
    for id in [SelfTestId::ImuAccel, SelfTestId::ImuGyro] {
        let result = state.self_tests[id as usize];
        EVENTS.send(SystemEvent::SelfTestUpdated { id, result }).await;
    }

    // Subscribe once; reused for both the awake and sleeping
    // branches of the task's main loop.
    let mut sleep_rx = SLEEP_WATCH
        .receiver()
        .expect("IMU: no SLEEP_WATCH receiver slot available");

    let mut sleep_state = SleepState::Awake;
    loop {
        match sleep_state {
            SleepState::Awake => {
                match select3(
                    Timer::after(Duration::from_millis(AWAKE_POLL_MS)),
                    sleep_rx.changed(),
                    IMU_COMMAND.wait(),
                ).await {
                    Either3::First(_) => {
                        let data = {
                            let mut i2c = bus.lock().await;
                            state.snapshot(&mut *i2c)
                        };
                        EVENTS.send(SystemEvent::MotionUpdated { data }).await;
                        // Forward any semantic wrist intent the
                        // seam's classifier settled on during that
                        // read. No bus traffic.
                        if let Some(intent) = state.imu.take_intent() {
                            log::info!("IMU: wrist {:?}", intent);
                            EVENTS
                                .send(match intent {
                                    WristIntent::Raised => SystemEvent::WristRaised,
                                    WristIntent::Lowered => SystemEvent::WristLowered,
                                })
                                .await;
                        }
                    }
                    Either3::Second(new) => {
                        sleep_state = new;
                        if new == SleepState::Sleeping {
                            let mut i2c = bus.lock().await;
                            state.imu.enter_wom(&mut *i2c);
                        }
                    }
                    Either3::Third(cmd) => {
                        handle_command(bus, &mut state, cmd).await;
                    }
                }
            }
            SleepState::Sleeping => {
                // Poll the seam's WoM latch at WOM_POLL_MS cadence.
                // See the module-level docs for why this polls
                // instead of waiting on the INT pin.
                let wom_fired = async {
                    loop {
                        Timer::after(Duration::from_millis(WOM_POLL_MS)).await;
                        let mut i2c = bus.lock().await;
                        if state.imu.wom_event(&mut *i2c) {
                            break;
                        }
                    }
                };
                match select(wom_fired, sleep_rx.changed()).await {
                    Either::First(_) => {
                        EVENTS.send(SystemEvent::WakeOnMotion).await;
                    }
                    Either::Second(new) => {
                        sleep_state = new;
                        if new == SleepState::Awake {
                            let mut i2c = bus.lock().await;
                            state.imu.exit_wom(&mut *i2c);
                        }
                    }
                }
            }
        }
    }
}

/// Handle one [`ImuCommand`] received on the [`IMU_COMMAND`] signal.
///
/// Lives outside the `impl` block because it needs access to
/// `bus`/`EVENTS` to drive the Running → Pass/Fail event sequence,
/// which the task state struct doesn't own.
async fn handle_command(
    bus: &'static SharedI2c,
    state: &mut ImuTaskState<'static>,
    cmd: ImuCommand,
) {
    match cmd {
        ImuCommand::RunSelfTest(id) => {
            // Emit Running first so the screen can dim the card
            // before the bus lock holds up any redraws.
            EVENTS.send(SystemEvent::SelfTestUpdated {
                id,
                result: SelfTestResult::Running,
            }).await;

            // The runner paces itself and only holds the bus for
            // short start/poll transactions, so redraws keep flowing
            // while the test runs.
            let result = run_self_test(bus, state, id).await;

            // Cache locally so the next post-wake replay shows the
            // latest result rather than the stale boot-time one.
            state.self_tests[id as usize] = result;

            EVENTS.send(SystemEvent::SelfTestUpdated { id, result }).await;
        }
    }
}
