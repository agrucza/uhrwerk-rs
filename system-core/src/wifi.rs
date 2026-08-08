//! WiFi: session-based NTP time sync (bring-up).
//!
//! The radio exists only for the seconds a session runs - the same
//! rail-gated model as the GPS sessions. A session: bring the radio
//! up, join the configured AP, get a DHCP lease, do one SNTP
//! exchange, hand the time to the shared RTC task, then tear the
//! whole stack down. Dropping the `WifiController` deinitializes the
//! WiFi driver and stops the radio (esp-radio documents this on the
//! controller's `Drop`), so between sessions the radio contributes
//! nothing to sleep current and its heap is returned.
//!
//! The session holds a [`bus::WakeHold`] for its duration: the radio
//! does not survive hardware light sleep, so the heartbeat must not
//! fire mid-session. Outside sessions the boards sleep exactly as
//! before.
//!
//! Board-agnostic like the audio session layer: esp-radio's API is
//! chip-neutral the same way esp-hal's is - the leaf bin's chip
//! feature selects the silicon. Gated behind this crate's `wifi`
//! cargo feature so boards that haven't wired it yet don't pay
//! esp-radio's build cost. The bin owns: the `WIFI` peripheral
//! token, the credentials (a gitignored `wifi_secrets.rs` until the
//! versioned config blob can carry them), and the spawn/kick.
//!
//! BRING-UP: the bin kicks one `SyncOnce` at boot so every flash
//! exercises the whole path without UI. Remove the kick when the
//! settings trigger lands (Phase B). `WifiCommand` formally joins
//! `app_core::commands` at the same time.

use embassy_futures::select::{select, Either};
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration};
use esp_hal::peripherals as p;
use esp_hal::rng::Rng;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{Config as WifiConfig, ControllerConfig};

use crate::bus::{self, RtcCommand, RTC_COMMAND};
use crate::clock_math::add_minutes;

use alloc::string::String;

/// Whole-session budget: radio init through RTC set. Covers AP
/// association, DHCP and one SNTP round-trip with slack; an absent
/// AP or dead uplink ends the session here instead of hanging it.
const SESSION_BUDGET_SECS: u64 = 30;

/// NTP pool hostname - resolved per session via the DHCP-provided
/// DNS server.
const NTP_SERVER: &str = "pool.ntp.org";

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch
/// (1970-01-01).
const NTP_UNIX_OFFSET: u32 = 2_208_988_800;

/// Commands for the WiFi task. Bring-up shape; moves to
/// `app_core::commands` when the manager/UI grow a trigger.
pub enum WifiCommand {
    /// One connect -> NTP -> RTC-set session.
    SyncOnce { tz_offset_minutes: i16 },
}

/// Bring-up command signal (see [`WifiCommand`]).
pub static WIFI_COMMAND: Signal<CriticalSectionRawMutex, WifiCommand> =
    Signal::new();

/// Owns the WIFI peripheral across sessions; each session borrows it
/// via `reborrow()` so the token is ready again for the next one -
/// the same session-scoped peripheral pattern as audio's I2S0.
/// Credentials arrive from the bin (this crate stays secrets-blind).
#[embassy_executor::task]
pub async fn wifi_task(
    mut wifi: p::WIFI<'static>,
    ssid: &'static str,
    password: &'static str,
) {
    loop {
        let WifiCommand::SyncOnce { tz_offset_minutes } =
            WIFI_COMMAND.wait().await;
        // Hardware light sleep would gate the radio's clocks
        // mid-association; hold the wake lock for the whole session
        // (released on every exit path by RAII).
        let _wake = bus::WakeHold::new();
        log::info!("WiFi: session start");
        run_sync_session(wifi.reborrow(), ssid, password, tz_offset_minutes)
            .await;
        log::info!("WiFi: session done - radio off");
    }
}

/// One full sync session. Every early return tears the radio down:
/// `controller` is declared first, so it drops last - the network
/// stack and sockets die before the WiFi driver deinitializes.
async fn run_sync_session(
    wifi: p::WIFI<'_>,
    ssid: &str,
    password: &str,
    tz_offset_minutes: i16,
) {
    let (mut controller, interfaces) =
        match esp_radio::wifi::new(wifi, ControllerConfig::default()) {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("WiFi: radio init failed: {:?}", e);
                return;
            }
        };

    let station = StationConfig::default()
        .with_ssid(ssid)
        .with_password(String::from(password));
    if let Err(e) = controller.set_config(&WifiConfig::Station(station)) {
        log::warn!("WiFi: station config rejected: {:?}", e);
        return;
    }

    // Session-scoped embassy-net stack over the station interface.
    // Sockets in play: DHCP + DNS + our UDP socket.
    let mut resources: StackResources<3> = StackResources::new();
    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;
    let (stack, mut runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        &mut resources,
        seed,
    );

    // The stack's poll loop (`runner.run()`) never returns; it runs
    // only while this select lives - ending the session ends it.
    let work = with_timeout(
        Duration::from_secs(SESSION_BUDGET_SECS),
        sync_once(&mut controller, stack, tz_offset_minutes),
    );
    match select(runner.run(), work).await {
        Either::First(never) => match never {},
        Either::Second(Ok(())) => {}
        Either::Second(Err(_)) => {
            log::warn!(
                "WiFi: session budget ({}s) exhausted",
                SESSION_BUDGET_SECS
            );
        }
    }
}

/// The session's actual work: associate, lease, resolve, exchange,
/// set the RTC. Failures log and return - the caller's teardown is
/// identical either way.
async fn sync_once(
    controller: &mut esp_radio::wifi::WifiController<'_>,
    stack: embassy_net::Stack<'_>,
    tz_offset_minutes: i16,
) {
    match controller.connect_async().await {
        Ok(info) => log::info!("WiFi: connected: {:?}", info),
        Err(e) => {
            log::warn!("WiFi: connect failed: {:?}", e);
            return;
        }
    }

    stack.wait_config_up().await;
    match stack.config_v4() {
        Some(cfg) => {
            log::info!("WiFi: DHCP lease - ip {}", cfg.address);
        }
        None => {
            log::warn!("WiFi: link up but no IPv4 config");
            return;
        }
    }

    let server = match stack.dns_query(NTP_SERVER, DnsQueryType::A).await {
        Ok(addrs) => match addrs.first().copied() {
            Some(addr) => addr,
            None => {
                log::warn!("WiFi: DNS returned no address for {}", NTP_SERVER);
                return;
            }
        },
        Err(e) => {
            log::warn!("WiFi: DNS lookup of {} failed: {:?}", NTP_SERVER, e);
            return;
        }
    };
    log::info!("WiFi: NTP server {} -> {}", NTP_SERVER, server);

    let Some(unix_secs) = sntp_exchange(stack, server).await else {
        return;
    };

    let (year, month, day, hour, minute, second) = civil_from_unix(unix_secs);
    let (year, month, day, hour, minute) = add_minutes(
        year,
        month,
        day,
        hour,
        minute,
        tz_offset_minutes as i32,
    );
    log::info!(
        "WiFi: NTP UTC ok -> local {:04}-{:02}-{:02} {:02}:{:02}:{:02} - RTC set",
        year, month, day, hour, minute, second,
    );
    RTC_COMMAND.signal(RtcCommand::SetTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    });
}

/// One SNTP round-trip (RFC 4330, the 48-byte packet): returns the
/// server's transmit timestamp as Unix seconds, rounded to the
/// nearest second via the fraction field. No offset/delay math - a
/// single query is plenty for a wrist-watch RTC.
async fn sntp_exchange(
    stack: embassy_net::Stack<'_>,
    server: IpAddress,
) -> Option<u64> {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 128];
    let mut tx_buf = [0u8; 128];
    let mut socket =
        UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    if let Err(e) = socket.bind(0) {
        log::warn!("WiFi: UDP bind failed: {:?}", e);
        return None;
    }

    // LI = 0, version = 4, mode = 3 (client); the rest zero.
    let mut request = [0u8; 48];
    request[0] = 0x23;
    if let Err(e) = socket.send_to(&request, (server, 123)).await {
        log::warn!("WiFi: NTP send failed: {:?}", e);
        return None;
    }

    let mut response = [0u8; 48];
    let n = match socket.recv_from(&mut response).await {
        Ok((n, _meta)) => n,
        Err(e) => {
            log::warn!("WiFi: NTP receive failed: {:?}", e);
            return None;
        }
    };
    if n < 48 {
        log::warn!("WiFi: NTP response truncated ({} bytes)", n);
        return None;
    }
    // Sanity: mode 4 (server) or 5 (broadcast); stratum 0 is a
    // kiss-o'-death packet.
    let mode = response[0] & 0x07;
    let stratum = response[1];
    if !(mode == 4 || mode == 5) || stratum == 0 {
        log::warn!(
            "WiFi: NTP response rejected (mode {}, stratum {})",
            mode,
            stratum
        );
        return None;
    }

    let ntp_secs =
        u32::from_be_bytes([response[40], response[41], response[42], response[43]]);
    let frac =
        u32::from_be_bytes([response[44], response[45], response[46], response[47]]);
    // NTP era handling: the 32-bit seconds counter wraps in 2036.
    // Timestamps with the high bit set are era 0 (1968-2036); clear
    // means era 1 (2036-2104), offset by exactly 2^32.
    let unix = if ntp_secs & 0x8000_0000 != 0 {
        (ntp_secs - NTP_UNIX_OFFSET) as u64
    } else {
        ntp_secs as u64 + (u32::MAX as u64 + 1 - NTP_UNIX_OFFSET as u64)
    };
    Some(unix + if frac >= 0x8000_0000 { 1 } else { 0 })
}

/// Unix seconds -> UTC calendar date/time. Days-to-civil conversion
/// per Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_unix(secs: u64) -> (u16, u8, u8, u8, u8, u8) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) =
        ((rem / 3600) as u8, ((rem % 3600) / 60) as u8, (rem % 60) as u8);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as u16;
    (year, month, day, hour, minute, second)
}
