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
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use system_core::bus::SharedI2c;

#[derive(Clone, Copy)]
pub enum HapticCommand {
    On,
    Off,
}

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
    let online = {
        let mut i2c = i2c_bus.lock().await;
        match drv.init(&mut *i2c) {
            Ok(id) => {
                log::info!("Haptics: DRV2605 online (device id {})", id);
                true
            }
            Err(_) => {
                log::error!("Haptics: DRV2605 init failed - buzz disabled");
                false
            }
        }
    };

    // Held while the motor is on: the DRV2605 drives autonomously,
    // so if hardware light sleep freezes the executor between an
    // enqueued Off and this task delivering it, the motor runs on
    // until the next heartbeat (hardware-observed: a 350 ms pulse at
    // GPS-session end - wake lock just released - buzzed 3-5 s).
    // The hold keeps the manager idling until Off has actually
    // reached the chip.
    let mut motor_hold: Option<system_core::bus::WakeHold> = None;
    loop {
        let cmd = HAPTIC_COMMAND.receive().await;
        if !online {
            continue;
        }
        let mut i2c = i2c_bus.lock().await;
        let res = match cmd {
            HapticCommand::On => {
                if motor_hold.is_none() {
                    motor_hold = Some(system_core::bus::WakeHold::new());
                }
                drv.buzz_on(&mut *i2c, RTP_MAX)
            }
            HapticCommand::Off => {
                let res = drv.buzz_off(&mut *i2c);
                motor_hold = None;
                res
            }
        };
        if res.is_err() {
            log::warn!("Haptics: DRV2605 I2C write failed");
        }
    }
}
