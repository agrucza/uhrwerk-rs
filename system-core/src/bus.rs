//! System-wide plumbing for the event-driven architecture.
//!
//! This module owns the globally-accessible statics that peripheral
//! tasks share with the main loop:
//!
//!   * [`EVENTS`]  - MPSC event channel. All tasks push events into
//!     this channel; the main loop drains it.
//!   * [`I2C_BUS`] - Shared I2C bus protected by an async mutex.
//!     Every task that needs I2C locks this before accessing it.
//!   * [`SLEEP_WATCH`] - Watch that broadcasts the current sleep
//!     state. Tasks subscribe once at startup and await `changed()`
//!     in their main loops to react to sleep/wake transitions
//!     (IMU swaps between snapshot polling and WoM, touch flips
//!     between Active and Monitor power modes, the power task
//!     stretches its PMU poll cadence).
//!
//! Everything here is initialised by the manager and then referenced
//! by tasks via `&'static` references, so lifetimes work out for
//! `#[embassy_executor::task]` definitions.

use app_core::events::SystemEvent;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::Channel,
    mutex::Mutex,
    signal::Signal,
    watch::Watch,
};
use esp_hal::{i2c::master::I2c, Blocking};
use static_cell::StaticCell;

// Command / broadcast payload enums live in `app_core::commands`
// so `Effect` can carry them. The `Signal` and `Watch` statics
// below still live here - they're hardware-coupled (task wakers,
// interrupt-safe mutexes).
pub use app_core::commands::{
    AudioCommand, GpsCommand, ImuCommand, RtcCommand, SleepState, WifiCommand,
};

/// Size of the system event channel. Should be large enough to
/// buffer a burst of events without blocking producers but small
/// enough that a backed-up main loop gets noticed.
pub const EVENT_CHANNEL_SIZE: usize = 32;

/// Global MPSC event channel.
///
/// All peripheral tasks push [`SystemEvent`]s into this channel.
/// The main loop drains it via `EVENTS.receive().await`.
pub static EVENTS: Channel<CriticalSectionRawMutex, SystemEvent, EVENT_CHANNEL_SIZE> =
    Channel::new();

/// Maximum number of tasks that can subscribe to [`SLEEP_WATCH`] at
/// once. Bump this when adding a new subscriber beyond the current
/// three (IMU, touch, power). Each subscriber consumes one slot
/// whether or not it's currently parked on `changed()`.
pub const SLEEP_WATCH_SUBSCRIBERS: usize = 4;

/// Broadcast of the current system sleep state.
///
/// Main publishes transitions via
/// `SLEEP_WATCH.sender().send(state)` when entering or exiting
/// sleep. Any task that needs to react acquires a receiver once
/// at task startup (`SLEEP_WATCH.receiver().unwrap()`) and awaits
/// `rx.changed()` inside its main loop's select.
///
/// The `Watch` primitive fans out the latest value to multiple
/// independent receivers, each tracking its own "last seen" id -
/// exactly what's needed for sleep state broadcast without the
/// single-consumer limitation of the old `Signal`-based design.
/// Current subscribers: IMU task (enters WoM mode on Sleeping,
/// restores normal config on Awake), touch task (switches
/// FT3168 to Monitor mode / back to Active), power task (slows
/// its PMU poll cadence while sleeping).
pub static SLEEP_WATCH: Watch<CriticalSectionRawMutex, SleepState, SLEEP_WATCH_SUBSCRIBERS> =
    Watch::new();

/// Main-to-IMU command signal.
///
/// The main loop publishes an [`ImuCommand`] here when a UI screen
/// returns an action that needs IMU hardware access (e.g. tapping a
/// self-test card returns `Action::RunSelfTest(id)`, the main loop
/// routes it here). The IMU task listens for it as one arm of its
/// awake-branch select.
///
/// Single-consumer: only the IMU task should call `wait()` on this.
pub static IMU_COMMAND: Signal<CriticalSectionRawMutex, ImuCommand> = Signal::new();

/// Main-to-RTC command signal.
///
/// Single-consumer: only the RTC task should call `wait()` on this.
pub static RTC_COMMAND: Signal<CriticalSectionRawMutex, RtcCommand> = Signal::new();

/// Main-to-audio command queue.
///
/// The main loop publishes [`AudioCommand`]s here
/// (`Effect::AudioCommand`). The audio task, spawned bin-side with the
/// board's I2S / DMA / speaker pins, receives them and drives the
/// speaker / microphone sessions.
///
/// A queue rather than a `Signal`: the alarm tone and mic capture are
/// two independent command streams multiplexed over this one channel,
/// and the model can emit a pair in one batch - e.g. an alarm firing
/// while the mic test is open pushes `PlayAlarm`, then the
/// leave-screen safety net pushes `StopCapture`. A single-slot
/// `Signal` collapses that pair into whichever was written last and
/// the alarm is silently lost; a queue delivers both, in order. The
/// audio task drains commands promptly (every inner-loop iteration
/// selects on this), so a small capacity is plenty.
///
/// Single-consumer: only the audio task should call `receive()` on
/// this.
pub static AUDIO_COMMAND: Channel<CriticalSectionRawMutex, AudioCommand, 4> = Channel::new();

/// Main-to-GPS command signal.
///
/// The main loop publishes a [`GpsCommand`] here
/// (`Effect::GpsCommand`). On boards with a GNSS receiver the
/// bin-spawned GPS task waits on it; on boards without one nothing
/// listens and a signal (unreachable anyway - the UI entry point is
/// capability-gated) is simply overwritten.
///
/// Single-consumer: only the GPS task should call `wait()` on this.
pub static GPS_COMMAND: Signal<CriticalSectionRawMutex, GpsCommand> = Signal::new();

/// Main-to-WiFi command signal.
///
/// The main loop publishes a [`WifiCommand`] here
/// (`Effect::WifiCommand`). In builds with the `wifi` feature the
/// shared WiFi session task waits on it; without the feature nothing
/// listens and a signal (unreachable anyway - the UI entry point is
/// capability-gated) is simply overwritten. A single slot is right:
/// the UI refuses a second kick while a session runs, and a queued
/// stale command would start a radio session nobody asked for.
///
/// Single-consumer: only the WiFi task should call `wait()` on this.
pub static WIFI_COMMAND: Signal<CriticalSectionRawMutex, WifiCommand> = Signal::new();

/// Count of live wake holds - sessions that need the executor and
/// peripheral clocks continuously up (a GPS sync session's UART
/// today; audio playback/capture is the planned second holder).
/// While nonzero, the manager idles across the heartbeat instead of
/// entering hardware light sleep, because light sleep gates the
/// UART/I2S clocks and silently drops their data. UI sleep (display
/// off, relaxed polling) is unaffected.
///
/// Hold through [`WakeHold`], not by touching this directly.
pub static WAKE_HOLDS: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(0);

/// RAII wake hold: constructing registers the hold, dropping (any
/// path, including early returns) releases it.
pub struct WakeHold(());

impl WakeHold {
    pub fn new() -> Self {
        WAKE_HOLDS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        Self(())
    }
}

impl Drop for WakeHold {
    fn drop(&mut self) {
        WAKE_HOLDS.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
    }
}

/// True while any [`WakeHold`] is alive.
pub fn wake_held() -> bool {
    WAKE_HOLDS.load(core::sync::atomic::Ordering::SeqCst) > 0
}

/// Type alias for the shared I2C bus, protected by an async mutex.
///
/// Four devices sit on this bus (PMU, touch, RTC, IMU) and each
/// lives in its own task. Tasks lock the mutex before reading or
/// writing, which serializes access without requiring any
/// per-device coordination.
pub type SharedI2c = Mutex<CriticalSectionRawMutex, I2c<'static, Blocking>>;

/// One-time storage for the shared I2C bus. Initialised by the
/// manager and handed to tasks as `&'static SharedI2c`.
pub static I2C_BUS: StaticCell<SharedI2c> = StaticCell::new();

/// Type alias for the shared persistent store, protected by an async
/// mutex - the same arrangement as [`SharedI2c`], for the same
/// reason: storage is a device more than one subsystem needs.
///
/// The manager was its sole owner while it was the only reader and
/// writer. A task that serves files over the network needs the same
/// handle, and two independent handles onto one flash region would
/// mean two caches over the same blocks.
///
/// **Lock discipline.** Every `Store` method is SYNCHRONOUS - littlefs
/// over SPI flash and embedded-sdmmc are both blocking - so whoever
/// holds this lock is also holding the executor. Lock for one
/// operation, release, and yield before the next: never read a whole
/// file, or await anything, while holding it.
pub type SharedStore = Mutex<CriticalSectionRawMutex, crate::storage::Store<'static>>;

/// One-time storage for the shared store. Initialised by the manager
/// during bring-up and handed out as `&'static SharedStore`.
pub static STORE: StaticCell<SharedStore> = StaticCell::new();
