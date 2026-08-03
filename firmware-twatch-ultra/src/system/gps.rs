//! GPS dispatch - MIA-M10Q behind the shared GPS command signal.
//!
//! The task owns the hardware (UART1 on GPS_TX/GPS_RX plus the
//! BLDO1 rail via the shared I2C bus); the UI reaches it through
//! `Effect::GpsCommand` -> `bus::GPS_COMMAND` (the settings GPS
//! view, gated on the gps capability this bin declares). The
//! receiver is rail-gated: BLDO1 comes up only for the duration of
//! a sync session and goes straight back down after - between
//! sessions the module sits in u-blox hardware backup mode (~28 uA
//! from the always-on VRTC via V_BCKP), keeping RTC time and
//! ephemeris for warm starts.
//!
//! A session powers the rail, configures the receiver over UBX
//! (NMEA chatter off, NAV-PVT once per epoch at 1 Hz, wrist dynamic
//! model - config written RAM+BBR so it survives the rail-off), then
//! reads NAV-PVT until it has both a trustworthy UTC time and a
//! usable position fix, or the session budget runs out. The first
//! trustworthy time is pushed into the PCF85063 through the shared
//! RTC task (`RtcCommand::SetTime`), shifted by the timezone offset
//! the command carries (a persisted user setting - the task stays
//! config-blind).
//!
//! The whole session runs under a [`bus::WakeHold`]: the manager
//! idles across heartbeats instead of entering hardware light sleep
//! (which gates the UART clock and drops bytes), so a session
//! survives the display going dark. Progress is published as
//! `SystemEvent::GpsSyncUpdated` for the settings GPS view.

use app_core::data::GpsSyncState;
use app_core::events::SystemEvent;
use drivers::pmu::{Config as PmuConfig, Pmu};
use drivers::ublox::{self, cfg, frame, nav};
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals as p;
use esp_hal::uart::{Config as UartConfig, Uart};
use esp_hal::Async;
use system_core::bus::{
    self, GpsCommand, RtcCommand, SharedI2c, EVENTS, GPS_COMMAND, RTC_COMMAND,
};

/// How long a sync session may hunt before giving up and powering
/// the receiver back down. Cold start under open sky needs ~30 s;
/// indoors it may never fix - this bounds the battery cost.
const SESSION_BUDGET_SECS: u64 = 120;

/// Cadence of the session's progress log line and status event.
const STATUS_LOG_SECS: u64 = 5;

#[embassy_executor::task]
pub async fn gps_task(
    i2c_bus: &'static SharedI2c,
    mut uart: p::UART1<'static>,
    mut tx: p::GPIO43<'static>,
    mut rx: p::GPIO44<'static>,
) {
    loop {
        let GpsCommand::SyncOnce { tz_offset_minutes } = GPS_COMMAND.wait().await;
        run_sync_session(
            i2c_bus,
            uart.reborrow(),
            tx.reborrow(),
            rx.reborrow(),
            tz_offset_minutes,
        )
        .await;
    }
}

/// Publish session progress for the settings GPS view. Best-effort:
/// a full event channel drops the update (the next cadence tick
/// re-publishes fresher state anyway).
fn publish(state: GpsSyncState) {
    let _ = EVENTS.try_send(SystemEvent::GpsSyncUpdated { state });
}

/// Switch the GPS main rail. The PMU driver handle is stateless -
/// constructing one here per call keeps the task free of init
/// ordering concerns.
async fn set_rail(i2c_bus: &'static SharedI2c, on: bool) -> bool {
    let pmu = Pmu::new(PmuConfig::default());
    let mut i2c = i2c_bus.lock().await;
    match pmu.set_bldo1_enable(&mut *i2c, on) {
        Ok(()) => true,
        Err(_) => {
            log::error!("GPS: rail switch failed ({})", if on { "on" } else { "off" });
            false
        }
    }
}

async fn run_sync_session(
    i2c_bus: &'static SharedI2c,
    uart: p::UART1<'_>,
    tx: p::GPIO43<'_>,
    rx: p::GPIO44<'_>,
    tz_offset_minutes: i16,
) {
    // Held for the whole session (any exit path drops it): the
    // manager idles across heartbeats instead of hardware-sleeping,
    // keeping the UART clocked while the display sleeps normally.
    let _wake = bus::WakeHold::new();
    publish(GpsSyncState::Syncing { sats: 0, fix_ok: false });
    if !set_rail(i2c_bus, true).await {
        publish(GpsSyncState::NoSignal);
        return;
    }
    log::info!("GPS: rail on - sync session starts");
    // Module boot: the data sheet flags up to 100 mA inrush; give
    // the rail and the receiver's own startup a moment before
    // talking. It already streams factory NMEA while we wait.
    Timer::after(Duration::from_millis(300)).await;

    // 38400 8N1 - the receiver's data-sheet default (and, once the
    // BBR-layer config below has been stored, its configured state).
    let mut port = match Uart::new(uart, UartConfig::default().with_baudrate(38400)) {
        Ok(u) => u.with_tx(tx).with_rx(rx).into_async(),
        Err(_) => {
            log::error!("GPS: UART init failed");
            set_rail(i2c_bus, false).await;
            publish(GpsSyncState::NoSignal);
            return;
        }
    };

    let mut parser = frame::Parser::new();
    if !configure(&mut port, &mut parser).await {
        log::warn!("GPS: receiver not acknowledging configuration - aborting");
        drop(port);
        set_rail(i2c_bus, false).await;
        publish(GpsSyncState::NoSignal);
        return;
    }

    // Read NAV-PVT until time + fix are in hand or the budget runs
    // out.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(SESSION_BUDGET_SECS);
    let mut next_status = started + Duration::from_secs(STATUS_LOG_SECS);
    let mut time_synced = false;
    let mut synced_local: Option<(u8, u8)> = None;
    let mut got_fix = false;
    let mut buf = [0u8; 64];
    // Latest solution, session-persistent - the status tick reports
    // from this, never from the current read batch (the tick almost
    // always fires on a timer-won select whose batch is empty).
    let mut last_pvt: Option<nav::NavPvt> = None;
    let mut rx_errors: u32 = 0;

    while Instant::now() < deadline && !(time_synced && got_fix) {
        let n = match select(
            port.read_async(&mut buf),
            Timer::at(deadline.min(next_status)),
        )
        .await
        {
            Either::First(Ok(n)) => n,
            Either::First(Err(e)) => {
                // The parser resynchronizes on the next sync
                // sequence; keep reading - but say what happened
                // (a silently swallowed error class cost a debug
                // flash once).
                rx_errors += 1;
                if rx_errors <= 3 {
                    log::warn!("GPS: uart read error: {:?}", e);
                }
                continue;
            }
            Either::Second(()) => 0,
        };

        let mut latest: Option<nav::NavPvt> = None;
        for &b in &buf[..n] {
            // Only NAV-PVT is enabled; anything else - including
            // checksum failures and false syncs from a stream
            // chopped mid-frame by light sleep - is dropped and the
            // parser self-heals at the next real frame boundary.
            if let frame::Poll::Frame { class, id } = parser.push(b) {
                if class == ublox::class::NAV && id == ublox::msg::NAV_PVT {
                    latest = nav::NavPvt::parse(parser.payload());
                }
            }
        }

        if let Some(pvt) = latest {
            last_pvt = Some(pvt);
            if pvt.time_trustworthy() && !time_synced {
                time_synced = true;
                synced_local = Some(sync_rtc(&pvt, tz_offset_minutes));
            }
            if pvt.position_usable() && !got_fix {
                got_fix = true;
                log::info!(
                    "GPS: fix {:?} lat {} lon {} (1e-7 deg) hAcc {} m, {} sats",
                    pvt.fix_type,
                    pvt.lat_1e7,
                    pvt.lon_1e7,
                    pvt.h_acc_mm / 1000,
                    pvt.num_sv,
                );
            }
        }

        if Instant::now() >= next_status {
            next_status += Duration::from_secs(STATUS_LOG_SECS);
            match &last_pvt {
                Some(pvt) => {
                    log::info!(
                        "GPS: fix {:?}, {} sats, hAcc {} m",
                        pvt.fix_type,
                        pvt.num_sv,
                        pvt.h_acc_mm / 1000,
                    );
                    publish(GpsSyncState::Syncing {
                        sats: pvt.num_sv,
                        fix_ok: pvt.position_usable(),
                    });
                }
                // NAV-PVT flows at 1 Hz from the moment config is
                // accepted, fix or not - persistent absence means
                // the receiver went quiet, not "no fix yet".
                None => log::info!("GPS: no NAV-PVT yet"),
            }
        }
    }

    drop(port);
    set_rail(i2c_bus, false).await;
    publish(match synced_local {
        Some((hour, minute)) => GpsSyncState::Synced { hour, minute },
        None => GpsSyncState::NoSignal,
    });
    log::info!(
        "GPS: session done in {} s - time {}, fix {}",
        started.elapsed().as_secs(),
        if time_synced { "synced" } else { "not synced" },
        if got_fix { "acquired" } else { "none" },
    );
}

/// Push the receiver configuration: one VALSET with the essentials
/// (ACK required), one with the wrist dynamic model (NAK tolerated -
/// the interface description flags WRIST as "not available in all
/// products"; the portable default then stands). Both are written
/// RAM+BBR, so a receiver with intact backup power arrives here
/// already configured and the writes are idempotent.
async fn configure(port: &mut Uart<'_, Async>, parser: &mut frame::Parser) -> bool {
    let mut payload_buf = [0u8; 64];
    let mut frame_buf = [0u8; 72];

    let mut essentials = cfg::ValSet::new(&mut payload_buf, cfg::LAYER_RAM | cfg::LAYER_BBR);
    essentials
        .add_bool(cfg::key::UART1OUTPROT_NMEA, false)
        .add_u8(cfg::key::MSGOUT_NAV_PVT_UART1, 1)
        .add_u16(cfg::key::RATE_MEAS, 1000)
        .add_u16(cfg::key::RATE_NAV, 1);
    let Some(p) = essentials.payload() else { return false };
    let Some(f) = frame::encode(ublox::class::CFG, ublox::msg::CFG_VALSET, p, &mut frame_buf)
    else {
        return false;
    };

    // Two attempts: the first can land while the receiver is still
    // booting and go unanswered.
    let mut acked = false;
    for _ in 0..2 {
        if port.write_async(f).await.is_err() {
            return false;
        }
        if wait_valset_ack(port, parser).await == Some(true) {
            acked = true;
            break;
        }
    }
    if !acked {
        return false;
    }

    let mut tuning = cfg::ValSet::new(&mut payload_buf, cfg::LAYER_RAM | cfg::LAYER_BBR);
    tuning.add_u8(cfg::key::NAVSPG_DYNMODEL, cfg::dynmodel::WRIST);
    let Some(p) = tuning.payload() else { return true };
    let Some(f) = frame::encode(ublox::class::CFG, ublox::msg::CFG_VALSET, p, &mut frame_buf)
    else {
        return true;
    };
    if port.write_async(f).await.is_ok() {
        match wait_valset_ack(port, parser).await {
            Some(true) => log::info!("GPS: wrist dynamic model active"),
            Some(false) => {
                log::warn!("GPS: wrist dynamic model rejected - portable default stands")
            }
            None => log::warn!("GPS: no answer to dynamic model config"),
        }
    }
    true
}

/// Read until an ACK/NAK for CFG-VALSET arrives (Some(ok)) or a
/// 1 s window closes (None). Other frames passing by are ignored.
///
/// Every read buffer is pushed through the parser IN FULL, even
/// when the ACK shows up mid-buffer: those bytes are already
/// consumed from the UART, and dropping the tail breaks the shared
/// parser's stream continuity. Hardware-found failure mode: the
/// discarded tail held the header of the first NAV-PVT already in
/// flight, the parser resumed inside its binary payload,
/// false-synced on payload bytes, and sat draining a garbage
/// 64 KB "frame" for the rest of the session.
async fn wait_valset_ack(port: &mut Uart<'_, Async>, parser: &mut frame::Parser) -> Option<bool> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut buf = [0u8; 64];
    while Instant::now() < deadline {
        let n = match select(port.read_async(&mut buf), Timer::at(deadline)).await {
            Either::First(Ok(n)) => n,
            Either::First(Err(_)) => continue,
            Either::Second(()) => return None,
        };
        let mut outcome = None;
        for &b in &buf[..n] {
            if let frame::Poll::Frame { class, id } = parser.push(b) {
                if let Some(ack) = ublox::parse_ack(class, id, parser.payload()) {
                    if ack.class == ublox::class::CFG
                        && ack.id == ublox::msg::CFG_VALSET
                        && outcome.is_none()
                    {
                        outcome = Some(ack.ok);
                    }
                }
            }
        }
        if outcome.is_some() {
            return outcome;
        }
    }
    None
}

/// Convert the receiver's UTC to watch-local time and hand it to the
/// shared RTC task. Returns the local (hour, minute) that was set,
/// for the session's terminal status event.
fn sync_rtc(pvt: &nav::NavPvt, tz_offset_minutes: i16) -> (u8, u8) {
    let (year, month, day, hour, minute) = add_minutes(
        pvt.year,
        pvt.month,
        pvt.day,
        pvt.hour,
        pvt.min,
        tz_offset_minutes as i32,
    );
    log::info!(
        "GPS: UTC {:04}-{:02}-{:02} {:02}:{:02}:{:02} (tAcc {} ns) -> local {:04}-{:02}-{:02} {:02}:{:02}",
        pvt.year, pvt.month, pvt.day, pvt.hour, pvt.min, pvt.sec,
        pvt.t_acc_ns,
        year, month, day, hour, minute,
    );
    RTC_COMMAND.signal(RtcCommand::SetTime {
        year,
        month,
        day,
        hour,
        minute,
        second: pvt.sec,
    });
    (hour, minute)
}

/// Calendar-correct minute-offset shift of a UTC date/time,
/// including day/month/year rollover in both directions.
fn add_minutes(year: u16, month: u8, day: u8, hour: u8, min: u8, offset: i32) -> (u16, u8, u8, u8, u8) {
    let mut y = year as i32;
    let mut mo = month as i32;
    let mut d = day as i32;
    let mut total = hour as i32 * 60 + min as i32 + offset;
    while total < 0 {
        total += 24 * 60;
        d -= 1;
        if d < 1 {
            mo -= 1;
            if mo < 1 {
                mo = 12;
                y -= 1;
            }
            d = days_in_month(y, mo);
        }
    }
    while total >= 24 * 60 {
        total -= 24 * 60;
        d += 1;
        if d > days_in_month(y, mo) {
            d = 1;
            mo += 1;
            if mo > 12 {
                mo = 1;
                y += 1;
            }
        }
    }
    (y as u16, mo as u8, d as u8, (total / 60) as u8, (total % 60) as u8)
}

fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
    }
}
