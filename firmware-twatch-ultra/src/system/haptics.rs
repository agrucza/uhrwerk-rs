//! Haptics dispatch - DRV2605 behind a command channel.
//!
//! `Board::buzz`/`buzz_stop` are sync calls with no bus access
//! (designed for GPIO motors), but this board's motor sits behind a
//! DRV2605 on the shared I2C bus. Same resolution as audio dispatch:
//! the `Board` impl only `try_send`s into this channel; the task here
//! owns the driver and the bus locking. Continuous RTP drive maps
//! 1:1 onto the on/off semantics - the model pulses the buzz pattern
//! itself, exactly as it does for the GPIO motor board.

use drivers::drv2605::{Config as DrvConfig, Drv2605, RTP_MAX};
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};
use system_core::bus::{SharedI2c, SleepState, SLEEP_WATCH};

#[derive(Clone, Copy)]
pub enum HapticCommand {
    On,
    Off,
    /// One-shot self-terminating ROM click (see [`CLICK_EFFECT`]). The
    /// DRV2605 plays it autonomously and stops itself, so unlike the
    /// On/Off pair it can never latch the motor on - the right shape
    /// for a scan-confirm buzz that fires at a wake-from-sleep
    /// boundary.
    Click,
}

/// DRV2605 ROM library A effect for the click: 1 = "Strong Click -
/// 100%". Short and crisp; the chip ends it on its own.
const CLICK_EFFECT: u8 = 1;

/// Depth 8 is burst headroom only: pattern pulses arrive every
/// ~100+ ms and the task drains a command in ~1 ms of I2C.
pub static HAPTIC_COMMAND: Channel<CriticalSectionRawMutex, HapticCommand, 8> =
    Channel::new();

/// Owns the DRV2605. Init happens here rather than in bringup - the
/// motor is not boot-critical, and a missing/broken DRV2605 must not
/// block the watch (commands are then drained and dropped).
#[embassy_executor::task]
pub async fn haptics_task(i2c_bus: &'static SharedI2c) {
    let drv = Drv2605::new(DrvConfig::default());
    // Init retries: at boot this task races the BHI260's ~103 KB
    // firmware upload on the shared I2C bus, and roughly one boot in
    // four the DRV2605's first contact failed - costing buzz for the
    // whole uptime over one bad millisecond. Three attempts, 100 ms
    // apart (the lock is NOT held across the wait), ride out the
    // storm; a genuinely absent/broken chip still fails all three.
    let mut online = false;
    for attempt in 1..=3 {
        let result = {
            let mut i2c = i2c_bus.lock().await;
            drv.init(&mut *i2c)
        };
        match result {
            Ok(id) => {
                log::info!("Haptics: DRV2605 online (device id {})", id);
                online = true;
                break;
            }
            Err(e) if attempt < 3 => {
                log::warn!(
                    "Haptics: DRV2605 init attempt {} failed: {:?} - retrying",
                    attempt, e,
                );
                Timer::after(Duration::from_millis(100)).await;
            }
            Err(e) => {
                log::error!(
                    "Haptics: DRV2605 init failed {} times (last: {:?}) - buzz disabled",
                    attempt, e,
                );
            }
        }
    }
    system_core::bus::boot_report("HAPT", if online { "OK" } else { "FAILED" });

    // Held while the motor is on: the DRV2605 drives autonomously,
    // so if hardware light sleep freezes the executor between an
    // enqueued Off and this task delivering it, the motor runs on
    // until the next heartbeat (hardware-observed: a 350 ms pulse at
    // GPS-session end - wake lock just released - buzzed 3-5 s).
    // The hold keeps the manager idling until Off has actually
    // reached the chip.
    let mut motor_hold: Option<system_core::bus::WakeHold> = None;
    // React to sleep transitions too, not just buzz commands: the
    // motor MUST be off before the system enters real light sleep, or
    // a lost/failed Off leaves it vibrating straight through sleep
    // (executor frozen, nothing to deliver the stop until the next
    // wake).
    let mut sleep_rx = SLEEP_WATCH.receiver().unwrap();
    loop {
        match select(HAPTIC_COMMAND.receive(), sleep_rx.changed()).await {
            Either::First(cmd) => {
                if !online {
                    continue;
                }
                match cmd {
                    HapticCommand::On => {
                        if motor_hold.is_none() {
                            motor_hold = Some(system_core::bus::WakeHold::new());
                        }
                        let mut i2c = i2c_bus.lock().await;
                        if drv.buzz_on(&mut *i2c, RTP_MAX).is_err() {
                            log::warn!("Haptics: buzz_on I2C write failed");
                        }
                    }
                    HapticCommand::Off => {
                        // Retry the stop: a single failed Off (plausible
                        // in the I2C storm right at a wake, when this
                        // buzz fires) would otherwise strand the motor
                        // driving continuously. The wake lock is held
                        // across the retries so the system can't sleep
                        // with the motor still on.
                        if !stop_motor(&drv, i2c_bus).await {
                            log::warn!("Haptics: buzz_off failed after retries");
                        }
                        motor_hold = None;
                    }
                    HapticCommand::Click => {
                        // Self-terminating ROM effect: fire and forget.
                        // No wake lock - the chip finishes the click on
                        // its own even if the system sleeps right after,
                        // and there is no Off to lose.
                        let mut i2c = i2c_bus.lock().await;
                        if drv.play_effect(&mut *i2c, CLICK_EFFECT).is_err() {
                            log::warn!("Haptics: click I2C write failed");
                        }
                    }
                }
            }
            Either::Second(state) => {
                // Safety net: force the motor off before real light
                // sleep, regardless of what the command stream did.
                if matches!(state, SleepState::Sleeping) {
                    if online {
                        let _ = stop_motor(&drv, i2c_bus).await;
                    }
                    motor_hold = None;
                }
            }
        }
    }
}

/// Stop the motor, retrying the I2C writes - a lost Off leaves the
/// DRV2605 in continuous RTP drive. Returns true once the stop lands.
async fn stop_motor(drv: &Drv2605, i2c_bus: &'static SharedI2c) -> bool {
    for attempt in 0..5 {
        {
            let mut i2c = i2c_bus.lock().await;
            if drv.buzz_off(&mut *i2c).is_ok() {
                return true;
            }
        }
        if attempt < 4 {
            Timer::after(Duration::from_millis(5)).await;
        }
    }
    false
}
